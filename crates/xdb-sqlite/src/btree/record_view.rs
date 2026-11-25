//! Lazy view of one SQLite record (cell payload + on-demand overflow chain).
//!
//! Mirrors `vdbe.c OP_Column:3047`, `btree.c fetchPayload:5433`, `accessPayload:5155`,
//! and `vdbemem.c sqlite3VdbeMemFromBtree:1462`.

use std::sync::Arc;

use xdb_core::value::utf::decode_stored_text_to_utf8;
use xdb_core::{SqlValue, TextEncoding};

use super::pager::Pager;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{decode_fixed, get_varint32, read_u32_be, serial_type_length};

/// `vdbe.c OP_Column`: largest valid record header (`32768 * 3 + 3`).
pub const MAX_RECORD_HEADER: usize = 98_307;

/// Lazy, incremental record decoder over a B-tree cell payload.
#[derive(Debug)]
pub struct RecordView {
    pager: Pager,
    noop_meter: bool,
    enc: TextEncoding,
    ovfl_size: usize,
    usable: usize,
    max_record: u64,

    // Payload location.
    page: Arc<[u8]>,
    pgno: u32,
    local0: usize,
    n_local: usize,
    /// `fetchPayload`'s `szRow`: local bytes readable in place (clamped to page end).
    sz_row: usize,
    n_payload: usize,
    ovfl: Vec<u32>,
    n_ovfl: u32,

    // Incremental header state.
    hdr_read: bool,
    hdr_size: usize,
    hdr_buf: Vec<u8>,
    hdr_in_page: bool,
    hdr_base: usize,
    i_hdr: usize,
    n_parsed: usize,
    complete: bool,
    types: Vec<u32>,
    offsets: Vec<usize>,
}

impl RecordView {
    pub fn new(pager: Pager, encoding: TextEncoding) -> Self {
        let noop_meter = pager.is_noop_meter();
        let usable = pager.db().usable_size as usize;
        let max_record = u64::from(pager.page_size()) * u64::from(pager.page_count());
        Self {
            pager,
            noop_meter,
            enc: encoding,
            ovfl_size: usable - 4,
            usable,
            max_record,
            page: Arc::from([]),
            pgno: 0,
            local0: 0,
            n_local: 0,
            sz_row: 0,
            n_payload: 0,
            ovfl: Vec::new(),
            n_ovfl: 0,
            hdr_read: false,
            hdr_size: 0,
            hdr_buf: Vec::new(),
            hdr_in_page: true,
            hdr_base: 0,
            i_hdr: 0,
            n_parsed: 0,
            complete: false,
            types: Vec::new(),
            offsets: vec![0],
        }
    }

    /// Re-point this view at a cell payload without reading overflow pages yet.
    pub fn reset(
        &mut self,
        pgno: u32,
        page: Arc<[u8]>,
        local_offset: usize,
        local_size: usize,
        payload_size: u32,
    ) {
        let room = page.len().saturating_sub(local_offset);
        self.pgno = pgno;
        self.page = page;
        self.local0 = local_offset;
        self.n_local = local_size;
        self.sz_row = local_size.min(room);
        self.n_payload = payload_size as usize;
        self.ovfl.clear();
        self.n_ovfl = if self.n_payload > local_size {
            ((self.n_payload - local_size).div_ceil(self.ovfl_size)) as u32
        } else {
            0
        };
        self.hdr_read = false;
        self.hdr_size = 0;
        self.hdr_buf.clear();
        self.hdr_in_page = true;
        self.hdr_base = 0;
        self.i_hdr = 0;
        self.n_parsed = 0;
        self.complete = false;
        self.types.clear();
        self.offsets.clear();
        self.offsets.push(0);
    }

    #[inline]
    pub fn pgno(&self) -> u32 {
        self.pgno
    }

    /// Re-point this view at another cell on the SAME already-held page (avoids `Arc::clone`).
    #[inline]
    pub fn reset_same_page(&mut self, local_offset: usize, local_size: usize, payload_size: u32) {
        let room = self.page.len().saturating_sub(local_offset);
        self.local0 = local_offset;
        self.n_local = local_size;
        self.sz_row = local_size.min(room);
        self.n_payload = payload_size as usize;
        self.ovfl.clear();
        self.n_ovfl = if self.n_payload > local_size {
            ((self.n_payload - local_size).div_ceil(self.ovfl_size)) as u32
        } else {
            0
        };
        self.hdr_read = false;
        self.hdr_size = 0;
        self.hdr_buf.clear();
        self.hdr_in_page = true;
        self.hdr_base = 0;
        self.i_hdr = 0;
        self.n_parsed = 0;
        self.complete = false;
        self.types.clear();
        self.offsets.clear();
        self.offsets.push(0);
    }

    #[inline]
    pub fn payload_size(&self) -> usize {
        self.n_payload
    }

    #[inline]
    pub fn local_size(&self) -> usize {
        self.n_local
    }

    #[inline]
    pub fn overflow_page_count(&self) -> u32 {
        self.n_ovfl
    }

    /// Number of columns physically present in this record (parses the full header).
    pub fn column_count(&mut self) -> SqliteResult<usize> {
        self.parse_through(usize::MAX)?;
        Ok(self.n_parsed)
    }

    /// Serial type of column `i`, or `None` if absent.
    pub fn serial_type(&mut self, i: usize) -> SqliteResult<Option<u32>> {
        self.parse_through(i)?;
        Ok(self.types.get(i).copied())
    }

    /// Byte offset of column `i`'s body within the payload, or `None` if absent.
    pub fn column_offset(&mut self, i: usize) -> SqliteResult<Option<usize>> {
        self.parse_through(i)?;
        if i < self.n_parsed {
            Ok(Some(self.offsets[i]))
        } else {
            Ok(None)
        }
    }

    /// Decode column `i`'s value (`SqlValue::Null` when absent from a short record).
    pub fn value(&mut self, i: usize) -> SqliteResult<SqlValue> {
        if !self.noop_meter {
            self.pager.meter().charge_steps(1)?;
        }
        self.parse_through(i)?;
        if i >= self.n_parsed {
            return Ok(SqlValue::Null);
        }
        let t = self.types[i];
        let start = self.offsets[i];
        if t < 12 {
            let len = serial_type_length(t);
            if len == 0 {
                return Ok(decode_fixed(&[], 0, t));
            }
            if start.checked_add(len).is_some_and(|e| e <= self.sz_row) {
                return Ok(decode_fixed(&self.page, self.local0 + start, t));
            }
            let buf = self.read_range(start, len, 0, true)?;
            return Ok(decode_fixed(&buf, 0, t));
        }
        let len = ((t - 12) >> 1) as usize;
        if (t & 1) == 1 {
            if len == 0 {
                return Ok(SqlValue::Text(Box::new([])));
            }
            if start.checked_add(len).is_some_and(|e| e <= self.sz_row) {
                let a = self.local0 + start;
                if self.enc == TextEncoding::Utf8 {
                    return Ok(SqlValue::Text(Box::from(&self.page[a..a + len])));
                }
                let decoded = decode_stored_text_to_utf8(&self.page[a..a + len], self.enc);
                return Ok(SqlValue::Text(decoded.into_boxed_slice()));
            }
            let buf = self.read_range(start, len, 0, true)?;
            if self.enc == TextEncoding::Utf8 {
                return Ok(SqlValue::Text(buf.into_boxed_slice()));
            }
            let decoded = decode_stored_text_to_utf8(&buf, self.enc);
            return Ok(SqlValue::Text(decoded.into_boxed_slice()));
        }
        if len == 0 {
            return Ok(SqlValue::Blob(Box::new([])));
        }
        if start.checked_add(len).is_some_and(|e| e <= self.sz_row) {
            let a = self.local0 + start;
            return Ok(SqlValue::Blob(Box::from(&self.page[a..a + len])));
        }
        let buf = self.read_range(start, len, 0, true)?;
        Ok(SqlValue::Blob(buf.into_boxed_slice()))
    }

    /// Raw body bytes of column `i` in the database's storage encoding.
    pub fn raw_column_bytes(&mut self, i: usize) -> SqliteResult<Vec<u8>> {
        self.parse_through(i)?;
        if i >= self.n_parsed {
            return Ok(Vec::new());
        }
        let start = self.offsets[i];
        let len = serial_type_length(self.types[i]);
        self.payload_bytes(start, len)
    }

    /// Decode all columns present in the record in order.
    pub fn values(&mut self) -> SqliteResult<Vec<SqlValue>> {
        let n = self.column_count()?;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.value(i)?);
        }
        Ok(out)
    }

    /// Read `length` payload bytes starting at `offset`.
    pub fn payload_bytes(&mut self, offset: usize, length: usize) -> SqliteResult<Vec<u8>> {
        if offset.checked_add(length).is_none_or(|end| end > self.n_payload) {
            return Err(SqliteError::misuse(format!(
                "payload range {offset}+{length} outside 0..{}",
                self.n_payload
            )));
        }
        if length == 0 {
            return Ok(Vec::new());
        }
        if offset + length <= self.sz_row {
            let a = self.local0 + offset;
            return Ok(self.page[a..a + length].to_vec());
        }
        self.read_range(offset, length, 0, true)
    }

    /// Read the full payload for a key comparison (padded with `pad` trailing zero bytes if copied).
    pub fn payload_for_compare(
        &mut self,
        pad: usize,
        check_size: bool,
        always_copy: bool,
    ) -> SqliteResult<(Arc<[u8]>, usize)> {
        let n = self.n_payload;
        if !always_copy && n <= self.sz_row {
            return Ok((self.page.clone(), self.local0));
        }
        let buf = self.read_range(0, n, pad, check_size)?;
        Ok((buf.into(), 0))
    }

    fn read_range(
        &mut self,
        offset: usize,
        len: usize,
        pad: usize,
        check_size: bool,
    ) -> SqliteResult<Vec<u8>> {
        let end = (offset as u64).saturating_add(len as u64);
        if check_size && end > self.max_record {
            return Err(SqliteError::corrupt(format!(
                "page {}: payload range {offset}+{len} exceeds the database size",
                self.pgno
            )));
        }
        let n_local = self.n_local;
        let local0 = self.local0;
        if local0 > self.usable.saturating_sub(n_local) {
            return Err(SqliteError::corrupt(format!(
                "page {}: cell payload extends past the end of the usable area",
                self.pgno
            )));
        }
        let alloc_len = len.saturating_add(pad);
        self.pager.meter().reserve_memory(alloc_len as u64)?;
        let mut out = vec![0u8; alloc_len];
        let mut o = 0usize;
        let mut off = offset;
        if off < n_local {
            let a = (n_local - off).min(len);
            out[..a].copy_from_slice(&self.page[local0 + off..local0 + off + a]);
            o = a;
            off = n_local;
        }
        if o == len {
            self.pager.meter().release_memory(alloc_len as u64);
            return Ok(out);
        }
        let ovfl_size = self.ovfl_size;
        let mut rel = off - n_local;
        let mut k = rel / ovfl_size;
        rel -= k * ovfl_size;
        while o < len {
            let data = match self.overflow_page(k) {
                Ok(d) => d,
                Err(e) => {
                    self.pager.meter().release_memory(alloc_len as u64);
                    return Err(e);
                }
            };
            let take = (ovfl_size - rel).min(len - o);
            out[o..o + take].copy_from_slice(&data[4 + rel..4 + rel + take]);
            o += take;
            rel = 0;
            k += 1;
        }
        self.pager.meter().release_memory(alloc_len as u64);
        Ok(out)
    }

    fn overflow_page(&mut self, k: usize) -> SqliteResult<Arc<[u8]>> {
        if self.ovfl.is_empty() {
            let p = self.local0 + self.n_local;
            if p + 4 > self.page.len() {
                return Err(SqliteError::corrupt(format!(
                    "page {}: overflow pointer lies past the end of the page",
                    self.pgno
                )));
            }
            self.ovfl.push(read_u32_be(&self.page, p));
        }
        while self.ovfl.len() <= k {
            if self.ovfl.len() as u32 > self.pager.page_count() {
                return Err(SqliteError::corrupt(format!(
                    "page {}: overflow chain cycle detected",
                    self.pgno
                )));
            }
            let j = self.ovfl.len() - 1;
            let prev = self.check_overflow_pgno(self.ovfl[j], j)?;
            let data = self
                .pager
                .get_page(prev, &format!("overflow chain of a cell on page {}", self.pgno))?;
            self.ovfl.push(read_u32_be(&data, 0));
        }
        let pgno = self.check_overflow_pgno(self.ovfl[k], k)?;
        self.pager
            .get_page(pgno, &format!("overflow chain of a cell on page {}", self.pgno))
    }

    fn check_overflow_pgno(&self, pgno: u32, k: usize) -> SqliteResult<u32> {
        if pgno == 0 {
            return Err(SqliteError::corrupt(format!(
                "page {}: overflow chain ends after {k} of {} pages",
                self.pgno, self.n_ovfl
            )));
        }
        if pgno > self.pager.page_count() {
            return Err(SqliteError::corrupt(format!(
                "page {}: overflow page {pgno} is beyond the database",
                self.pgno
            )));
        }
        Ok(pgno)
    }

    fn parse_through(&mut self, p2: usize) -> SqliteResult<()> {
        if p2 < self.n_parsed || self.complete {
            return Ok(());
        }
        if !self.hdr_read {
            self.read_header()?;
        }
        if self.complete {
            return Ok(());
        }

        let end = self.hdr_size;
        let payload = self.n_payload;
        let base = self.hdr_base;
        let mut i = self.n_parsed;
        let mut z = self.i_hdr;
        let mut off64 = self.offsets[i];

        let mut parsed_this_call = 0usize;
        loop {
            parsed_this_call += 1;
            if !self.noop_meter && (parsed_this_call & 31) == 0 {
                self.pager.meter().charge_steps(1)?;
            }
            let buf: &[u8] = if self.hdr_in_page {
                &self.page
            } else {
                &self.hdr_buf
            };
            let b = buf.get(base + z).copied().unwrap_or(0);
            let t = if b < 0x80 {
                z += 1;
                u32::from(b)
            } else {
                let (v, len) = get_varint32(buf, base + z);
                z += len;
                v
            };
            if z > end {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record header overruns its declared size {end}",
                    self.pgno
                )));
            }
            let Some(next_off) = off64.checked_add(serial_type_length(t)) else {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record body offset overflows payload size {payload}",
                    self.pgno
                )));
            };
            if next_off > payload || (z == end && next_off != payload) {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record bodies end at {next_off}, payload is {payload} bytes",
                    self.pgno
                )));
            }
            self.types.push(t);
            off64 = next_off;
            i += 1;
            self.offsets.push(off64);
            if i > p2 || z >= end {
                break;
            }
        }

        self.n_parsed = i;
        self.i_hdr = z;
        if z >= end {
            self.complete = true;
        }
        Ok(())
    }

    fn read_header(&mut self) -> SqliteResult<()> {
        if self.n_payload == 0 || self.sz_row == 0 {
            return Err(SqliteError::corrupt(format!(
                "page {}: empty record payload has no header",
                self.pgno
            )));
        }
        let p = self.local0;
        let Some(&b0) = self.page.get(p) else {
            return Err(SqliteError::corrupt(format!(
                "page {}: record header lies past the end of the page",
                self.pgno
            )));
        };
        let (hdr_size_u32, len) = if b0 < 0x80 {
            (u32::from(b0), 1usize)
        } else {
            let (v, l) = get_varint32(&self.page, p);
            if p + l > self.page.len() {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record header lies past the end of the page",
                    self.pgno
                )));
            }
            (v, l)
        };
        let hdr_size = hdr_size_u32 as usize;
        if hdr_size == 0 {
            self.hdr_size = 0;
            self.complete = true;
            self.hdr_read = true;
            return Ok(());
        }
        if hdr_size <= self.sz_row {
            if hdr_size <= len {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record header size {hdr_size} leaves no serial types",
                    self.pgno
                )));
            }
            self.hdr_in_page = true;
            self.hdr_base = p;
        } else {
            if hdr_size > MAX_RECORD_HEADER || hdr_size > self.n_payload {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record header size {hdr_size} is invalid for a {}-byte payload",
                    self.pgno, self.n_payload
                )));
            }
            self.hdr_buf = self.read_range(0, hdr_size, 0, true)?;
            self.hdr_in_page = false;
            self.hdr_base = 0;
            if hdr_size <= len {
                return Err(SqliteError::corrupt(format!(
                    "page {}: record header size {hdr_size} leaves no serial types",
                    self.pgno
                )));
            }
        }
        self.hdr_size = hdr_size;
        self.i_hdr = len;
        self.offsets[0] = hdr_size;
        self.hdr_read = true;
        Ok(())
    }
}
