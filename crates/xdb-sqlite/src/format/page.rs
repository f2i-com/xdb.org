//! B-tree page decoding (`fileformat2 §1.6`; `btree.c:1193-2248`).

use std::sync::Arc;

use super::db_header::DatabaseInfo;
use super::varint::{get_payload_size_varint, get_varint};
use crate::errors::{SqliteError, SqliteResult};

/// The four legal B-tree page flag bytes (`btreeInt.h PTF_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BtreePageType {
    IndexInterior = 0x02,
    TableInterior = 0x05,
    IndexLeaf = 0x0a,
    TableLeaf = 0x0d,
}

impl BtreePageType {
    #[inline]
    pub fn from_byte(flag: u8) -> Option<Self> {
        match flag {
            0x02 => Some(BtreePageType::IndexInterior),
            0x05 => Some(BtreePageType::TableInterior),
            0x0a => Some(BtreePageType::IndexLeaf),
            0x0d => Some(BtreePageType::TableLeaf),
            _ => None,
        }
    }

    #[inline]
    pub const fn is_leaf(self) -> bool {
        (self as u8) >= 0x0a
    }

    #[inline]
    pub const fn is_table(self) -> bool {
        ((self as u8) & 0x01) != 0
    }
}

/// Parsed cell header (`btreeInt.h CellInfo`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellInfo {
    /// Byte offset of the cell within the page buffer.
    pub pc: usize,
    /// Left child page number (interior cells), else `0`.
    pub child: u32,
    /// Table cells: signed 64-bit rowid. Index cells: payload size.
    pub key: i64,
    /// Total payload bytes (local + overflow); `0` for table interior cells.
    pub payload_size: u32,
    /// Byte offset of the first payload byte in the page buffer.
    pub payload_offset: usize,
    /// Local payload bytes stored on this page.
    pub local_size: usize,
    /// First overflow page (`0` when local or pointer out of buffer).
    pub overflow_page: u32,
    /// Bytes the cell occupies in the cell content area (min 4).
    pub cell_size: usize,
}

/// Bytes of a `payload_size`-byte payload stored locally (`btreePayloadToLocal:1238`).
#[inline]
pub const fn local_payload_size(
    payload_size: u32,
    max_local: u32,
    min_local: u32,
    usable_size: u32,
) -> usize {
    if payload_size <= max_local {
        return payload_size as usize;
    }
    let surplus = min_local + ((payload_size - min_local) % (usable_size - 4));
    if surplus <= max_local {
        surplus as usize
    } else {
        min_local as usize
    }
}

/// Number of overflow pages required by a payload.
#[inline]
pub const fn overflow_page_count(payload_size: u32, local_size: usize, usable_size: u32) -> u32 {
    let local = local_size as u32;
    if payload_size <= local {
        return 0;
    }
    let ovfl_data = usable_size - 4;
    (payload_size - local).div_ceil(ovfl_data)
}

#[inline]
pub fn read_u16_be(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

#[inline]
pub fn read_u32_be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Immutable parsed view over one SQLite B-tree page.
#[derive(Debug, Clone)]
pub struct BtreePage {
    pub pgno: u32,
    pub data: Arc<[u8]>,
    pub page_size: usize,
    pub mask_page: usize,
    pub hdr_offset: usize,
    pub page_type: BtreePageType,
    pub is_leaf: bool,
    pub is_table: bool,
    pub child_ptr_size: usize,
    pub n_cell: usize,
    pub cell_ptr_offset: usize,
    pub content_start_raw: u16,
    pub first_freeblock: u16,
    pub fragmented_bytes: u8,
    pub right_child: u32,
    pub max_local: u32,
    pub min_local: u32,
    pub usable_size: usize,
    pub cell_first: usize,
    pub cell_last: usize,
}

impl BtreePage {
    pub fn parse(pgno: u32, data: Arc<[u8]>, db: &DatabaseInfo) -> SqliteResult<Self> {
        let page_size = data.len();
        let hdr = if pgno == 1 { 100usize } else { 0usize };
        if hdr + 8 > page_size {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: page buffer too short ({page_size} bytes)"
            )));
        }
        let mask_page = page_size - 1;
        let flag = data[hdr];
        let Some(page_type) = BtreePageType::from_byte(flag) else {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: invalid b-tree page type 0x{flag:02x}"
            )));
        };
        let is_leaf = page_type.is_leaf();
        let is_table = page_type.is_table();
        let child_ptr_size = if is_leaf { 0 } else { 4 };
        if hdr + 8 + child_ptr_size > page_size {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: interior header runs past page end"
            )));
        }
        let (max_local, min_local) = if is_table && is_leaf {
            (db.limits.max_leaf, db.limits.min_leaf)
        } else {
            (db.limits.max_local, db.limits.min_local)
        };
        let usable_size = db.usable_size as usize;
        let n_cell = usize::from(read_u16_be(&data, hdr + 3));
        if n_cell > db.max_cells_per_page as usize {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: {n_cell} cells exceed the maximum {}",
                db.max_cells_per_page
            )));
        }
        let first_freeblock = read_u16_be(&data, hdr + 1);
        let content_start_raw = read_u16_be(&data, hdr + 5);
        let fragmented_bytes = data[hdr + 7];
        let right_child = if is_leaf {
            0
        } else {
            read_u32_be(&data, hdr + 8)
        };
        let cell_ptr_offset = hdr + 8 + child_ptr_size;
        let cell_first = cell_ptr_offset + 2 * n_cell;
        if cell_first > usable_size {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: cell pointer array runs past usable page end"
            )));
        }
        let cell_last = usable_size - 4 - usize::from(!is_leaf);
        Ok(Self {
            pgno,
            data,
            page_size,
            mask_page,
            hdr_offset: hdr,
            page_type,
            is_leaf,
            is_table,
            child_ptr_size,
            n_cell,
            cell_ptr_offset,
            content_start_raw,
            first_freeblock,
            fragmented_bytes,
            right_child,
            max_local,
            min_local,
            usable_size,
            cell_first,
            cell_last,
        })
    }

    #[inline]
    pub fn content_start(&self) -> usize {
        if self.content_start_raw == 0 {
            65536
        } else {
            usize::from(self.content_start_raw)
        }
    }

    /// `findCell` (`btree.c:1193`): offset of cell `i`, masked with `page_size - 1`.
    #[inline]
    pub fn cell_offset(&self, i: usize) -> usize {
        usize::from(read_u16_be(&self.data, self.cell_ptr_offset + 2 * i)) & self.mask_page
    }

    /// Unmasked cell offset strictly checked against `cell_first..=cell_last` (`btreeCellSizeCheck`).
    pub fn checked_cell_offset(&self, i: usize) -> SqliteResult<usize> {
        let pc = usize::from(read_u16_be(&self.data, self.cell_ptr_offset + 2 * i));
        if pc < self.cell_first || pc > self.cell_last {
            return Err(SqliteError::corrupt(format!(
                "page {}: cell {i} offset {pc} outside {}..={}",
                self.pgno, self.cell_first, self.cell_last
            )));
        }
        Ok(pc)
    }

    /// Child page number at position `i` of an interior page (`right_child` when `i >= n_cell`).
    pub fn child_at(&self, i: usize) -> SqliteResult<u32> {
        if i >= self.n_cell {
            return Ok(self.right_child);
        }
        let pc = self.cell_offset(i);
        if pc + 4 > self.usable_size {
            return Err(self.past_buffer(i));
        }
        Ok(read_u32_be(&self.data, pc))
    }

    /// Parse cell `i` exactly as SQLite's `xParseCell` does (using masked `cell_offset`).
    pub fn parse_cell(&self, i: usize) -> SqliteResult<CellInfo> {
        self.parse_at(self.cell_offset(i), i)
    }

    /// Parse cell `i` with strict `btreeCellSizeCheck` bounds (for `dbstat` / `page_stats`).
    pub fn check_cell(&self, i: usize) -> SqliteResult<CellInfo> {
        let pc = self.checked_cell_offset(i)?;
        let cell = self.parse_at(pc, i)?;
        if pc + cell.cell_size > self.usable_size {
            return Err(SqliteError::corrupt(format!(
                "page {}: cell {i} extends past the end of the usable area",
                self.pgno
            )));
        }
        Ok(cell)
    }

    fn parse_at(&self, pc: usize, i: usize) -> SqliteResult<CellInfo> {
        let data = &self.data;
        let end = self.usable_size;
        let mut out = CellInfo {
            pc,
            ..CellInfo::default()
        };
        if self.is_table {
            if !self.is_leaf {
                if pc + 4 > end {
                    return Err(self.past_buffer(i));
                }
                out.child = read_u32_be(data, pc);
                let (key, len) = get_varint(data, pc + 4);
                if pc + 4 + len > end {
                    return Err(self.past_buffer(i));
                }
                out.key = key;
                out.payload_offset = pc + 4 + len;
                out.cell_size = 4 + len;
                return Ok(out);
            }
            let (n, n_len) = if pc < end && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let p0 = pc + n_len;
            let (key, k_len) = if p0 < end && data[p0] < 0x80 {
                (i64::from(data[p0]), 1usize)
            } else {
                get_varint(data, p0)
            };
            let p = p0 + k_len;
            if p > end {
                return Err(self.past_buffer(i));
            }
            out.key = key;
            out.payload_size = n;
            out.payload_offset = p;
        } else {
            let p0 = pc + self.child_ptr_size;
            if self.child_ptr_size != 0 {
                if p0 > end {
                    return Err(self.past_buffer(i));
                }
                out.child = read_u32_be(data, pc);
            }
            let (n, n_len) = get_payload_size_varint(data, p0);
            if p0 + n_len > end {
                return Err(self.past_buffer(i));
            }
            out.key = i64::from(n);
            out.payload_size = n;
            out.payload_offset = p0 + n_len;
        }

        let n = out.payload_size;
        if n <= self.max_local {
            out.local_size = n as usize;
            out.overflow_page = 0;
            let size = (out.payload_offset - pc) + (n as usize);
            out.cell_size = size.max(4);
        } else {
            let local = local_payload_size(n, self.max_local, self.min_local, self.usable_size as u32);
            out.local_size = local;
            out.cell_size = (out.payload_offset - pc) + local + 4;
            let ptr = out.payload_offset + local;
            out.overflow_page = if ptr + 4 <= end {
                read_u32_be(data, ptr)
            } else {
                0
            };
        }
        Ok(out)
    }

    /// Rowid of cell `i` of a table page (`sqlite3BtreeTableMoveto:5922`).
    pub fn rowid_at(&self, i: usize) -> SqliteResult<i64> {
        let data = &self.data;
        let end = self.usable_size;
        let mut p = if self.is_leaf {
            let mut pos = self.cell_offset(i);
            let mut steps = 0usize;
            loop {
                if pos >= end {
                    return Err(SqliteError::corrupt(format!(
                        "page {}: cell {i} payload size runs to the end of the page",
                        self.pgno
                    )));
                }
                let b = data[pos];
                pos += 1;
                steps += 1;
                if b < 0x80 || steps >= 9 {
                    break;
                }
            }
            pos
        } else {
            self.cell_offset(i) + 4
        };
        let (val, len) = get_varint(data, p);
        p += len;
        if p > end {
            return Err(self.past_buffer(i));
        }
        Ok(val)
    }

    fn past_buffer(&self, i: usize) -> SqliteError {
        SqliteError::corrupt(format!(
            "page {}: cell {i} runs past the end of the page",
            self.pgno
        ))
    }

    /// Free bytes on the page (`btreeComputeFreeSpace:2118`).
    pub fn free_space(&self) -> SqliteResult<usize> {
        let data = &self.data;
        let usable = self.usable_size;
        let top = self.content_start();
        let i_cell_first = self.cell_first;
        let i_cell_last = usable - 4;
        let mut pc = usize::from(self.first_freeblock);
        let mut n_free = usize::from(self.fragmented_bytes) + top;
        if pc > 0 {
            if pc < top {
                return Err(SqliteError::corrupt(format!(
                    "page {}: freeblock {pc} before the content area",
                    self.pgno
                )));
            }
            let mut next;
            let mut size;
            loop {
                if pc > i_cell_last {
                    return Err(SqliteError::corrupt(format!(
                        "page {}: freeblock {pc} past the end of the page",
                        self.pgno
                    )));
                }
                next = usize::from(read_u16_be(data, pc));
                size = usize::from(read_u16_be(data, pc + 2));
                if size < 4 {
                    return Err(SqliteError::corrupt(format!(
                        "page {}: freeblock {pc} smaller than 4 bytes",
                        self.pgno
                    )));
                }
                n_free += size;
                if next < pc + size + 4 {
                    break;
                }
                pc = next;
            }
            if next > 0 {
                return Err(SqliteError::corrupt(format!(
                    "page {}: freeblocks not in ascending order",
                    self.pgno
                )));
            }
            if pc + size > usable {
                return Err(SqliteError::corrupt(format!(
                    "page {}: last freeblock extends past the page end",
                    self.pgno
                )));
            }
        }
        if n_free > usable || n_free < i_cell_first {
            return Err(SqliteError::corrupt(format!(
                "page {}: free space accounting is inconsistent",
                self.pgno
            )));
        }
        Ok(n_free - i_cell_first)
    }
}
