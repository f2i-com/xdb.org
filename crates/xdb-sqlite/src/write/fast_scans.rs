//! Direct-on-[`WritePager`] high-performance query kernels for cached `SELECT` fast paths:
//! - Rowid range scans (`WHERE id BETWEEN ? AND ?` / `>= ? AND <= ?`)
//! - Single-column `GROUP BY` with streaming aggregates (`COUNT`, `SUM`, `TOTAL`, `AVG`)
//! - Cumulative IPK-ordered window `SUM(col) OVER (ORDER BY id)` scans
//! - Direct 2-table hash equi-joins (`FROM big JOIN small ON big.k = small.k`)
//!
//! All kernels enforce exact SQLite 3.54.1 semantics (affinity, numeric equality, overflow
//! chains, KBN floating-point summation, integer overflow detection, cycle protection, and
//! sandbox resource metering) while bypassing `SqliteDatabase` / `Pager` / `Relation` overhead.

use std::collections::{HashMap, HashSet};

use xdb_core::value::{apply_numeric_affinity, real_value};
use xdb_core::{Affinity, Meter, SqlValue, TextEncoding};

use crate::errors::{SqliteError, SqliteResult};
use crate::format::page::read_u16_be;
use crate::format::{
    decode_serial_value, get_payload_size_varint, get_varint, get_varint32, local_payload_size,
    lock_byte_page, read_u32_be, serial_type_length, BtreePageType, BTREE_MAX_DEPTH, HEADER_SIZE,
};
use crate::write::btree::DirectTableSlot;
use crate::write::pager::WritePager;

// ───────────────────────────── Shared primitives ─────────────────────────────

struct VisitedPages {
    bits: [u64; 128],
    overflow: Option<HashSet<u32>>,
}

impl VisitedPages {
    #[inline]
    fn new() -> Self {
        Self {
            bits: [0u64; 128],
            overflow: None,
        }
    }

    #[inline]
    fn insert(&mut self, pgno: u32) -> bool {
        if pgno < 8192 {
            let w = (pgno as usize) >> 6;
            let bit = 1u64 << (pgno & 63);
            let was = (self.bits[w] & bit) != 0;
            self.bits[w] |= bit;
            !was
        } else {
            self.overflow.get_or_insert_with(HashSet::new).insert(pgno)
        }
    }
}

struct ScanMemGuard<'a> {
    meter: Option<&'a dyn Meter>,
    reserved: u64,
}

impl<'a> ScanMemGuard<'a> {
    #[inline]
    fn new(meter: Option<&'a dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    #[inline]
    fn reserve(&mut self, bytes: u64) -> SqliteResult<()> {
        if let Some(m) = self.meter {
            if bytes > 0 {
                m.reserve_memory(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
            }
        }
        Ok(())
    }
}

impl Drop for ScanMemGuard<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.meter {
            if self.reserved > 0 {
                m.release_memory(self.reserved);
            }
        }
    }
}

#[inline]
fn val_heap_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Text(s) | SqlValue::Blob(s) => s.len() as u64,
        _ => 0,
    }
}

#[inline]
fn estimate_row_bytes(row: &[SqlValue]) -> u64 {
    let mut b = 24u64.saturating_add((row.len() as u64).saturating_mul(32));
    for v in row {
        b = b.saturating_add(val_heap_bytes(v));
    }
    b
}

fn read_full_payload_fast(
    pager: &WritePager,
    local: &[u8],
    total_len: usize,
    first_ovfl: u32,
) -> SqliteResult<Vec<u8>> {
    let usable = pager.usable_size() as usize;
    let chunk_cap = usable.saturating_sub(4);
    if chunk_cap == 0 {
        return Err(SqliteError::corrupt("usable size too small for overflow"));
    }
    let max_db_bytes = u64::from(pager.page_size()).saturating_mul(u64::from(pager.page_count()));
    if total_len as u64 > max_db_bytes {
        return Err(SqliteError::corrupt("payload size exceeds database size"));
    }
    let metered = !pager.is_noop_meter();
    let mut mem = ScanMemGuard::new(metered.then_some(pager.meter().as_ref()));
    mem.reserve(total_len as u64)?;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(local);
    let page_count = pager.page_count();
    let lbp = lock_byte_page(pager.page_size());
    let mut visited = VisitedPages::new();
    let mut cur = first_ovfl;
    while out.len() < total_len {
        if cur < 2 || cur > page_count || cur == lbp || !visited.insert(cur) {
            return Err(SqliteError::corrupt(format!(
                "corrupt overflow chain page {cur}"
            )));
        }
        let page = pager.read_page(cur)?;
        let bound = usable.min(page.len());
        if bound < 4 {
            return Err(SqliteError::corrupt(format!(
                "truncated overflow page {cur}"
            )));
        }
        let next = read_u32_be(page, 0);
        let need = total_len - out.len();
        let avail = bound - 4;
        let take = need.min(avail);
        out.extend_from_slice(&page[4..4 + take]);
        cur = next;
        if cur == 0 && out.len() < total_len {
            return Err(SqliteError::corrupt("overflow chain ended prematurely"));
        }
    }
    Ok(out)
}


#[inline]
fn decode_int_serial(st: u32, data: &[u8], off: usize) -> Option<i64> {
    match st {
        8 => Some(0),
        9 => Some(1),
        1 => data.get(off).map(|&b| i64::from(b as i8)),
        2 => data
            .get(off..off + 2)
            .map(|b| i64::from(i16::from_be_bytes([b[0], b[1]]))),
        3 => data.get(off..off + 3).map(|b| {
            let v = ((b[0] as i32) << 24) >> 8 | (i32::from(b[1]) << 8) | i32::from(b[2]);
            i64::from(v)
        }),
        4 => data
            .get(off..off + 4)
            .map(|b| i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))),
        5 => data.get(off..off + 6).map(|b| {
            let sign = if (b[0] & 0x80) != 0 { 0xffu8 } else { 0u8 };
            i64::from_be_bytes([sign, sign, b[0], b[1], b[2], b[3], b[4], b[5]])
        }),
        6 => data
            .get(off..off + 8)
            .map(|b| i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])),
        _ => None,
    }
}

/// Fast inline extraction of a single stored column from a local SQLite record payload.
/// Returns `Ok(Some(val))` when decoded, or `Ok(None)` if `storage_idx >= n_fields` (caller uses default).
#[inline]
fn decode_single_field_from_payload(
    payload: &[u8],
    storage_idx: usize,
    encoding: TextEncoding,
    real_back: bool,
    default_val: &SqlValue,
) -> SqliteResult<SqlValue> {
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty record payload"));
    }
    let (hdr_len_u32, mut hp) = if payload[0] < 0x80 {
        (u32::from(payload[0]), 1usize)
    } else {
        get_varint32(payload, 0)
    };
    let hdr_len = hdr_len_u32 as usize;
    if hdr_len < hp || hdr_len > payload.len() {
        return Err(SqliteError::corrupt("corrupt record header length"));
    }
    let mut bp = hdr_len;
    let mut fi = 0usize;
    while hp < hdr_len {
        let (st, n) = if payload[hp] < 0x80 {
            (u32::from(payload[hp]), 1usize)
        } else {
            get_varint32(payload, hp)
        };
        hp += n;
        if hp > hdr_len {
            return Err(SqliteError::corrupt("corrupt record header"));
        }
        let len = serial_type_length(st);
        if bp + len > payload.len() {
            return Err(SqliteError::corrupt("record body truncated"));
        }
        if fi == storage_idx {
            let raw = if let Some(iv) = decode_int_serial(st, payload, bp) {
                if real_back {
                    SqlValue::Real(iv as f64)
                } else {
                    SqlValue::Int(iv)
                }
            } else {
                let v = decode_serial_value(&payload[bp..bp + len], st, encoding);
                if real_back {
                    crate::schema::real_read_back(v)
                } else {
                    v
                }
            };
            return Ok(raw);
        }
        bp += len;
        fi += 1;
    }
    Ok(if real_back {
        crate::schema::real_read_back(default_val.clone())
    } else {
        default_val.clone()
    })
}

/// Fast inline extraction of 1..=32 projected `DirectTableSlot` columns in ONE header pass.
#[inline]
fn decode_direct_slots(
    payload: &[u8],
    rid: i64,
    slots: &[DirectTableSlot],
    max_storage_idx: Option<usize>,
    encoding: TextEncoding,
) -> SqliteResult<Vec<SqlValue>> {
    let Some(max_sidx) = max_storage_idx else {
        return Ok(vec![SqlValue::Int(rid); slots.len()]);
    };
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty record payload"));
    }
    let (hdr_len_u32, mut hp) = if payload[0] < 0x80 {
        (u32::from(payload[0]), 1usize)
    } else {
        get_varint32(payload, 0)
    };
    let hdr_len = hdr_len_u32 as usize;
    if hdr_len < hp || hdr_len > payload.len() {
        return Err(SqliteError::corrupt("corrupt record header length"));
    }
    let mut bp = hdr_len;
    let mut fi = 0usize;
    let mut row = Vec::with_capacity(slots.len());
    for sl in slots {
        match sl {
            DirectTableSlot::Rowid => row.push(SqlValue::Int(rid)),
            DirectTableSlot::Field { default_val, real_back, .. } => {
                row.push(if *real_back {
                    crate::schema::real_read_back(default_val.clone())
                } else {
                    default_val.clone()
                });
            }
        }
    }
    while hp < hdr_len && fi <= max_sidx {
        let (st, n) = if payload[hp] < 0x80 {
            (u32::from(payload[hp]), 1usize)
        } else {
            get_varint32(payload, hp)
        };
        hp += n;
        if hp > hdr_len {
            return Err(SqliteError::corrupt("corrupt record header"));
        }
        let len = serial_type_length(st);
        if bp + len > payload.len() {
            return Err(SqliteError::corrupt("record body truncated"));
        }
        // Check if any output slot references `fi`.
        for (out_i, sl) in slots.iter().enumerate() {
            if let DirectTableSlot::Field {
                storage_idx,
                real_back,
                ..
            } = sl
            {
                if *storage_idx == fi {
                    let val = if let Some(iv) = decode_int_serial(st, payload, bp) {
                        if *real_back {
                            SqlValue::Real(iv as f64)
                        } else {
                            SqlValue::Int(iv)
                        }
                    } else {
                        let v = decode_serial_value(&payload[bp..bp + len], st, encoding);
                        if *real_back {
                            crate::schema::real_read_back(v)
                        } else {
                            v
                        }
                    };
                    row[out_i] = val;
                }
            }
        }
        bp += len;
        fi += 1;
    }
    Ok(row)
}

/// Exact SQLite `sum()` / `total()` / `avg()` accumulator (matches `xdb_sql::SumAcc` bit-for-bit).
#[derive(Debug, Clone, Default)]
struct DirectSumState {
    r_sum: f64,
    r_err: f64,
    i_sum_64: i64,
    i_sum_hi: i64,
    cnt: i64,
    ovrfl: bool,
    approx: bool,
}

impl DirectSumState {
    #[inline]
    fn step_int(&mut self, iv: i64) {
        self.cnt += 1;
        let (sum, ov) = self.i_sum_64.overflowing_add(iv);
        self.i_sum_64 = sum;
        if ov {
            if !self.approx {
                self.ovrfl = true;
            }
            if iv >= 0 {
                self.i_sum_hi = self.i_sum_hi.wrapping_add(1);
            } else {
                self.i_sum_hi = self.i_sum_hi.wrapping_sub(1);
            }
        }
    }

    #[inline]
    fn kbn_add(s: &mut f64, err: &mut f64, r: f64) {
        let old = *s;
        let t = old + r;
        if old.abs() > r.abs() {
            *err += (old - t) + r;
        } else {
            *err += (r - t) + old;
        }
        *s = t;
    }

    #[inline]
    fn step_value(&mut self, v: &SqlValue, encoding: TextEncoding) {
        match v {
            SqlValue::Null => {}
            SqlValue::Int(iv) => self.step_int(*iv),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    return;
                }
                self.cnt += 1;
                self.approx = true;
                Self::kbn_add(&mut self.r_sum, &mut self.r_err, *r);
            }
            SqlValue::Text(b) => {
                if let Some(SqlValue::Int(iv)) = apply_numeric_affinity(b, false) {
                    self.step_int(iv);
                } else {
                    self.cnt += 1;
                    self.approx = true;
                    Self::kbn_add(&mut self.r_sum, &mut self.r_err, real_value(v));
                }
            }
            SqlValue::Blob(_) => {
                self.cnt += 1;
                self.approx = true;
                Self::kbn_add(
                    &mut self.r_sum,
                    &mut self.r_err,
                    xdb_core::value::real_value_enc(v, encoding),
                );
            }
        }
    }

    #[inline]
    fn kbn_total(&self) -> f64 {
        let mut s = self.r_sum;
        let mut err = self.r_err;
        if self.i_sum_hi == 0 {
            if self.i_sum_64 != 0 {
                let hi = (self.i_sum_64 / 16384) * 16384;
                let lo = self.i_sum_64 - hi;
                if hi != 0 {
                    Self::kbn_add(&mut s, &mut err, hi as f64);
                }
                if lo != 0 {
                    Self::kbn_add(&mut s, &mut err, lo as f64);
                }
            }
        } else {
            const C1: i128 = 1i128 << 44;
            const C2: i128 = 1i128 << 88;
            let full = (i128::from(self.i_sum_hi) << 64) + i128::from(self.i_sum_64);
            let top = full / C2;
            let rem1 = full - top * C2;
            let mid = rem1 / C1;
            let lo = rem1 - mid * C1;
            if top != 0 {
                Self::kbn_add(&mut s, &mut err, (top as f64) * (C2 as f64));
            }
            if mid != 0 {
                Self::kbn_add(&mut s, &mut err, (mid as f64) * (C1 as f64));
            }
            if lo != 0 {
                Self::kbn_add(&mut s, &mut err, lo as f64);
            }
        }
        if err.is_finite() {
            s + err
        } else {
            s
        }
    }

    #[inline]
    fn finalize_sum(&self) -> SqliteResult<SqlValue> {
        if self.cnt == 0 {
            return Ok(SqlValue::Null);
        }
        if self.ovrfl && !self.approx {
            return Err(SqliteError::Sql("integer overflow".to_string()));
        }
        if self.approx {
            let r = self.kbn_total();
            if r.is_nan() {
                Ok(SqlValue::Null)
            } else {
                Ok(SqlValue::Real(r))
            }
        } else {
            Ok(SqlValue::Int(self.i_sum_64))
        }
    }

    #[inline]
    fn finalize_total(&self) -> SqlValue {
        if self.cnt == 0 {
            return SqlValue::Real(0.0);
        }
        SqlValue::real(self.kbn_total())
    }

    #[inline]
    fn finalize_avg(&self) -> SqlValue {
        if self.cnt == 0 {
            return SqlValue::Null;
        }
        let r = self.kbn_total() / (self.cnt as f64);
        if r.is_nan() {
            SqlValue::Null
        } else {
            SqlValue::Real(r)
        }
    }
}

/// Walk all table leaf pages in ascending rowid order using a zero-heap-allocation stack cursor.
/// Calls `visit_leaf(pgno, data, bound, hdr, n_cell)` for each leaf; stops early if it returns `Ok(false)`.
fn walk_table_leaves_in_order(
    pager: &WritePager,
    root_pgno: u32,
    mut visit_leaf: impl FnMut(u32, &[u8], usize, usize, usize) -> SqliteResult<bool>,
) -> SqliteResult<()> {
    let page_count = pager.page_count();
    if root_pgno == 1 && page_count == 0 {
        return Ok(());
    }
    let usable = pager.usable_size();
    let metered = !pager.is_noop_meter();
    let mut visited_count = 1u32;
    let mut visited = VisitedPages::new();
    let mut stack = [(0u32, 0usize); BTREE_MAX_DEPTH];
    let mut sp: usize;

    visited.insert(root_pgno);
    let root_data = pager.read_page(root_pgno)?;
    let root_bound = (usable as usize).min(root_data.len());
    let root_hdr = if root_pgno == 1 { HEADER_SIZE } else { 0 };
    if root_hdr + 8 > root_bound {
        return Err(SqliteError::corrupt(format!("page {root_pgno}: truncated header")));
    }
    match BtreePageType::from_byte(root_data[root_hdr]) {
        Some(BtreePageType::TableLeaf) => {
            let n_cell = usize::from(read_u16_be(root_data, root_hdr + 3));
            let _ = visit_leaf(root_pgno, root_data, root_bound, root_hdr, n_cell)?;
            return Ok(());
        }
        Some(BtreePageType::TableInterior) => {
            stack[0] = (root_pgno, 0);
            sp = 1;
        }
        _ => {
            return Err(SqliteError::corrupt(format!(
                "invalid table B-tree page type on page {root_pgno}"
            )));
        }
    }

    while sp > 0 {
        let (pgno, next_ci) = stack[sp - 1];
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        if hdr + 12 + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: truncated interior header"
            )));
        }
        let ptr_off = hdr + 12;
        let mut ci = next_ci;
        let mut descended = false;
        while ci <= n_cell {
            let child = if ci < n_cell {
                let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                if pc + 4 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: interior cell {ci} out of bounds"
                    )));
                }
                read_u32_be(data, pc)
            } else {
                read_u32_be(data, hdr + 8)
            };
            ci += 1;
            visited_count = visited_count.saturating_add(1);
            if visited_count > page_count || !visited.insert(child) {
                return Err(SqliteError::corrupt(format!(
                    "table B-tree {root_pgno} contains a cycle"
                )));
            }
            if metered {
                pager.meter().charge_steps(1)?;
            }
            let cdata = pager.read_page(child)?;
            let cbound = (usable as usize).min(cdata.len());
            let chdr = if child == 1 { HEADER_SIZE } else { 0 };
            if chdr + 8 > cbound {
                return Err(SqliteError::corrupt(format!("page {child}: truncated header")));
            }
            match BtreePageType::from_byte(cdata[chdr]) {
                Some(BtreePageType::TableLeaf) => {
                    let cn = usize::from(read_u16_be(cdata, chdr + 3));
                    if !visit_leaf(child, cdata, cbound, chdr, cn)? {
                        return Ok(());
                    }
                }
                Some(BtreePageType::TableInterior) => {
                    if sp >= BTREE_MAX_DEPTH {
                        return Err(SqliteError::corrupt(format!(
                            "table B-tree {root_pgno} exceeds max depth"
                        )));
                    }
                    stack[sp - 1].1 = ci;
                    stack[sp] = (child, 0);
                    sp += 1;
                    descended = true;
                    break;
                }
                _ => {
                    return Err(SqliteError::corrupt(format!(
                        "invalid table B-tree page type on page {child}"
                    )));
                }
            }
        }
        if !descended {
            sp -= 1;
        }
    }
    Ok(())
}

// ───────────────────── 1. Direct Rowid Range Scan (#6) ─────────────────────

/// Direct-on-`WritePager` range scan for `SELECT <cols> FROM tbl WHERE id BETWEEN lo AND hi`
/// (or `>=`/`>`/`<=`/`<` bounds). Descends directly to `lo`, walks leaves in ascending rowid
/// order until `rid > hi`, and decodes only requested columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_rowid_range_scan_direct(
    pager: &WritePager,
    table_root: u32,
    lo: Option<i64>,
    hi: Option<i64>,
    limit: Option<usize>,
    offset: usize,
    slots: &[DirectTableSlot],
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    if limit == Some(0) {
        return Ok(Vec::new());
    }
    if let (Some(l), Some(h)) = (lo, hi) {
        if l > h {
            return Ok(Vec::new());
        }
    }
    let page_count = pager.page_count();
    if table_root == 1 && page_count == 0 {
        return Ok(Vec::new());
    }
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();
    let mut mem = ScanMemGuard::new(metered.then_some(meter.as_ref()));

    let max_sidx = slots
        .iter()
        .filter_map(|s| match s {
            DirectTableSlot::Rowid => None,
            DirectTableSlot::Field { storage_idx, .. } => Some(*storage_idx),
        })
        .max();

    let est_cap = match (lo, hi, limit) {
        (_, _, Some(l)) => l.min(1024),
        (Some(l), Some(h), None) => (h.saturating_sub(l).saturating_add(1) as usize).min(1024),
        _ => 64,
    };
    let mut out = Vec::with_capacity(est_cap);
    let mut skipped = 0usize;

    // Descend directly to the starting leaf for `lo` (or leftmost leaf if `lo.is_none()`),
    // recording `(interior_pgno, next_child_idx)` on a stack cursor for forward leaf iteration.
    let mut stack = [(0u32, 0usize); BTREE_MAX_DEPTH];
    let mut sp = 0usize;
    let mut visited_count = 0u32;
    let mut visited = VisitedPages::new();
    let mut cur_pg = table_root;

    let first_leaf = loop {
        visited_count = visited_count.saturating_add(1);
        if visited_count > page_count || !visited.insert(cur_pg) {
            return Err(SqliteError::corrupt(format!(
                "table B-tree {table_root} contains a cycle"
            )));
        }
        let data = pager.read_page(cur_pg)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if cur_pg == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {cur_pg}: truncated header")));
        }
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => break cur_pg,
            Some(BtreePageType::TableInterior) => {
                if sp >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "table B-tree {table_root} exceeds max depth"
                    )));
                }
                if hdr + 12 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {cur_pg}: truncated interior header"
                    )));
                }
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let ptr_off = hdr + 12;
                if ptr_off + 2 * n_cell > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {cur_pg}: interior cell pointers out of bounds"
                    )));
                }
                let child_idx = if let Some(target_lo) = lo {
                    let mut b_lo = 0usize;
                    let mut b_hi = n_cell;
                    while b_lo < b_hi {
                        let mid = b_lo + (b_hi - b_lo) / 2;
                        let pc = usize::from(read_u16_be(data, ptr_off + 2 * mid));
                        if pc + 4 >= bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {cur_pg}: interior cell {mid} out of bounds"
                            )));
                        }
                        let (k, klen) = get_varint(data, pc + 4);
                        if pc + 4 + klen > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {cur_pg}: interior cell {mid} key out of bounds"
                            )));
                        }
                        if k < target_lo {
                            b_lo = mid + 1;
                        } else {
                            b_hi = mid;
                        }
                    }
                    b_lo
                } else {
                    0
                };
                let next_child = if child_idx < n_cell {
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * child_idx));
                    if pc + 4 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {cur_pg}: interior cell {child_idx} out of bounds"
                        )));
                    }
                    read_u32_be(data, pc)
                } else {
                    read_u32_be(data, hdr + 8)
                };
                stack[sp] = (cur_pg, child_idx + 1);
                sp += 1;
                cur_pg = next_child;
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_rowid_range_scan_direct: invalid page type on page {cur_pg}"
                )));
            }
        }
    };

    let mut scan_leaf = |pgno: u32, is_first: bool| -> SqliteResult<bool> {
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        let start_ci = if is_first {
            if let Some(target_lo) = lo {
                let mut b_lo = 0usize;
                let mut b_hi = n_cell;
                while b_lo < b_hi {
                    let mid = b_lo + (b_hi - b_lo) / 2;
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * mid));
                    if pc >= bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: leaf cell {mid} pointer out of bounds"
                        )));
                    }
                    let (_, n1) = if data[pc] < 0x80 {
                        (u32::from(data[pc]), 1usize)
                    } else {
                        get_payload_size_varint(data, pc)
                    };
                    if pc + n1 >= bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: leaf cell {mid} truncated header"
                        )));
                    }
                    let (rid, n2) = get_varint(data, pc + n1);
                    if pc + n1 + n2 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: leaf cell {mid} rowid out of bounds"
                        )));
                    }
                    if rid < target_lo {
                        b_lo = mid + 1;
                    } else {
                        b_hi = mid;
                    }
                }
                b_lo
            } else {
                0
            }
        } else {
            0
        };

        for ci in start_ci..n_cell {
            if metered {
                meter.charge_steps(1)?;
            }
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            if pc >= bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} pointer out of bounds"
                )));
            }
            let (psize_u32, n1) = if data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            if rid_off >= bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} truncated header"
                )));
            }
            let (rid, n2) = if data[rid_off] < 0x80 {
                (i64::from(data[rid_off]), 1usize)
            } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                (
                    (i64::from(data[rid_off] & 0x7f) << 7) | i64::from(data[rid_off + 1]),
                    2usize,
                )
            } else {
                get_varint(data, rid_off)
            };
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }
            if let Some(h) = hi {
                if rid > h {
                    return Ok(false);
                }
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            let row = if !has_ovfl {
                decode_direct_slots(
                    &data[local_start..local_start + local],
                    rid,
                    slots,
                    max_sidx,
                    encoding,
                )?
            } else {
                let ovfl = read_u32_be(data, local_start + local);
                let full = read_full_payload_fast(
                    pager,
                    &data[local_start..local_start + local],
                    psize_u32 as usize,
                    ovfl,
                )?;
                decode_direct_slots(&full, rid, slots, max_sidx, encoding)?
            };
            if metered {
                mem.reserve(estimate_row_bytes(&row))?;
            }
            out.push(row);
            if let Some(lim) = limit {
                if out.len() >= lim {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };

    if !scan_leaf(first_leaf, true)? {
        return Ok(out);
    }

    // Continue advancing forward across sibling leaves via the interior stack.
    while sp > 0 {
        let (pgno, next_ci) = stack[sp - 1];
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 12 > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: truncated interior header"
            )));
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let ptr_off = hdr + 12;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: interior cell pointers out of bounds"
            )));
        }
        if next_ci > n_cell {
            sp -= 1;
            continue;
        }
        let child = if next_ci < n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * next_ci));
            if pc + 4 > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: interior cell {next_ci} out of bounds"
                )));
            }
            read_u32_be(data, pc)
        } else {
            read_u32_be(data, hdr + 8)
        };
        stack[sp - 1].1 = next_ci + 1;

        visited_count = visited_count.saturating_add(1);
        if visited_count > page_count || !visited.insert(child) {
            return Err(SqliteError::corrupt(format!(
                "table B-tree {table_root} contains a cycle"
            )));
        }
        let cdata = pager.read_page(child)?;
        let cbound = (usable as usize).min(cdata.len());
        let chdr = if child == 1 { HEADER_SIZE } else { 0 };
        if chdr + 8 > cbound {
            return Err(SqliteError::corrupt(format!("page {child}: truncated header")));
        }
        match BtreePageType::from_byte(cdata[chdr]) {
            Some(BtreePageType::TableLeaf) => {
                if !scan_leaf(child, false)? {
                    return Ok(out);
                }
            }
            Some(BtreePageType::TableInterior) => {
                if sp >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "table B-tree {table_root} exceeds max depth"
                    )));
                }
                if chdr + 12 > cbound {
                    return Err(SqliteError::corrupt(format!(
                        "page {child}: truncated interior header"
                    )));
                }
                let cn = usize::from(read_u16_be(cdata, chdr + 3));
                if chdr + 12 + 2 * cn > cbound {
                    return Err(SqliteError::corrupt(format!(
                        "page {child}: interior cell pointers out of bounds"
                    )));
                }
                stack[sp] = (child, 0);
                sp += 1;
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_rowid_range_scan_direct: invalid page type on page {child}"
                )));
            }
        }
    }

    Ok(out)
}


// ───────────────────── 2. Direct Single-Col GROUP BY (#14) ─────────────────────

/// Output expression specification for [`table_group_by_single_col_direct`].
#[derive(Debug, Clone)]
pub(crate) enum DirectGroupOut {
    GroupKey,
    CountStar,
    CountCol { sum_slot: usize },
    Sum { sum_slot: usize },
    Total { sum_slot: usize },
    Avg { sum_slot: usize },
}

/// Unique stored column fed into one or more `COUNT`/`SUM`/`TOTAL`/`AVG` aggregates.
#[derive(Debug, Clone)]
pub(crate) struct DirectAggInput {
    pub storage_idx: usize,
    pub default_val: SqlValue,
    pub real_back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GroupHashKey {
    Null,
    Int(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl GroupHashKey {
    #[inline]
    fn from_sql(v: &SqlValue) -> Self {
        match v {
            SqlValue::Null => Self::Null,
            SqlValue::Int(i) => Self::Int(*i),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    Self::Null
                } else {
                    if r.fract() == 0.0 && *r >= -9223372036854775808.0 && *r < 9223372036854775808.0 {
                        let i = *r as i64;
                        if (i as f64) == *r {
                            return Self::Int(i);
                        }
                    }
                    let norm = if *r == 0.0 { 0.0f64 } else { *r };
                    Self::Real(norm.to_bits())
                }
            }
            SqlValue::Text(b) => Self::Text(b.to_vec()),
            SqlValue::Blob(b) => Self::Blob(b.to_vec()),
        }
    }
}

struct GroupBucket {
    key: SqlValue,
    count_star: i64,
    sums: Vec<DirectSumState>,
}

/// Direct-on-`WritePager` single-column `GROUP BY` scanner with inline small-int direct
/// indexing (`0..4096`), single-pass record field extraction, and exact SQLite KBN summation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_group_by_single_col_direct(
    pager: &WritePager,
    table_root: u32,
    grp_storage_idx: usize,
    grp_default: &SqlValue,
    grp_real_back: bool,
    agg_inputs: &[DirectAggInput],
    outputs: &[DirectGroupOut],
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();
    let mut mem = ScanMemGuard::new(metered.then_some(meter.as_ref()));

    let mut small_idx = [0u32; 4096]; // 1-based index into `buckets`
    let mut other_map: HashMap<GroupHashKey, usize> = HashMap::new();
    let mut buckets: Vec<GroupBucket> = Vec::with_capacity(64);
    let n_sums = agg_inputs.len();

    // Specialized ultra-fast 2-int case (`grp` + 1 single agg column, e.g. `GROUP BY grp` with
    // `count(*), sum(val), avg(val)`): reads BOTH integers in one pass straight from the page!
    let single_input = if n_sums == 1 {
        Some((agg_inputs[0].storage_idx, agg_inputs[0].real_back))
    } else {
        None
    };

    let mut max_needed = grp_storage_idx;
    for inp in agg_inputs {
        if inp.storage_idx > max_needed {
            max_needed = inp.storage_idx;
        }
    }

    walk_table_leaves_in_order(pager, table_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        if metered {
            meter.charge_steps((n_cell as u64 / 4).max(1))?;
        }

        for ci in 0..n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            // Skip assembling rowid value — we only need its varint byte length!
            let rid_off = pc + n1;
            let n2 = if rid_off < bound && data[rid_off] < 0x80 {
                1usize
            } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                2usize
            } else {
                get_varint(data, rid_off).1
            };
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }

            // Fast inline register-only path for local payload + single agg input + integer grp & val!
            if !has_ovfl && !grp_real_back {
                if let Some((val_sidx, val_real_back)) = single_input {
                    let payload = &data[local_start..local_start + local];
                    if !payload.is_empty() && payload[0] < 0x80 {
                        let hdr_len = usize::from(payload[0]);
                        if hdr_len >= 1 && hdr_len <= payload.len() {
                            let mut hp = 1usize;
                            let mut bp = hdr_len;
                            let mut fi = 0usize;
                            let mut g_int: Option<i64> = None;
                            let mut v_int: Option<i64> = None;
                            let mut v_is_null = false;
                            let mut fast_ok = true;
                            while hp < hdr_len && fi <= max_needed {
                                let st_b = payload[hp];
                                let (st, n) = if st_b < 0x80 {
                                    (u32::from(st_b), 1usize)
                                } else {
                                    get_varint32(payload, hp)
                                };
                                hp += n;
                                if hp > hdr_len {
                                    fast_ok = false;
                                    break;
                                }
                                let len = serial_type_length(st);
                                if bp + len > payload.len() {
                                    fast_ok = false;
                                    break;
                                }
                                if fi == grp_storage_idx {
                                    if let Some(iv) = decode_int_serial(st, payload, bp) {
                                        g_int = Some(iv);
                                    } else {
                                        fast_ok = false;
                                        break;
                                    }
                                }
                                if fi == val_sidx {
                                    if st == 0 {
                                        v_is_null = true;
                                    } else if !val_real_back {
                                        if let Some(iv) = decode_int_serial(st, payload, bp) {
                                            v_int = Some(iv);
                                        } else {
                                            fast_ok = false;
                                            break;
                                        }
                                    } else {
                                        fast_ok = false;
                                        break;
                                    }
                                }
                                bp += len;
                                fi += 1;
                            }
                            if fast_ok {
                                if let Some(gk) = g_int {
                                    if (0..4096).contains(&gk) {
                                        let slot = small_idx[gk as usize];
                                        let bidx = if slot != 0 {
                                            (slot - 1) as usize
                                        } else {
                                            let idx = buckets.len();
                                            if metered {
                                                mem.reserve(96)?;
                                            }
                                            buckets.push(GroupBucket {
                                                key: SqlValue::Int(gk),
                                                count_star: 0,
                                                sums: vec![DirectSumState::default(); n_sums],
                                            });
                                            small_idx[gk as usize] = (idx + 1) as u32;
                                            idx
                                        };
                                        let b = &mut buckets[bidx];
                                        b.count_star += 1;
                                        if let Some(iv) = v_int {
                                            b.sums[0].step_int(iv);
                                        } else if !v_is_null && val_sidx >= fi {
                                            if val_real_back {
                                                let dv = crate::schema::real_read_back(
                                                    agg_inputs[0].default_val.clone(),
                                                );
                                                b.sums[0].step_value(&dv, encoding);
                                            } else {
                                                b.sums[0].step_value(&agg_inputs[0].default_val, encoding);
                                            }
                                        }
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // General exact fallback for overflow payloads, non-int keys, floats, or multiple agg inputs.
            let owned_full: Vec<u8>;
            let payload: &[u8] = if !has_ovfl {
                &data[local_start..local_start + local]
            } else {
                let ovfl = read_u32_be(data, local_start + local);
                owned_full = read_full_payload_fast(
                    pager,
                    &data[local_start..local_start + local],
                    psize_u32 as usize,
                    ovfl,
                )?;
                &owned_full
            };
            let gval = decode_single_field_from_payload(
                payload,
                grp_storage_idx,
                encoding,
                grp_real_back,
                grp_default,
            )?;
            let ghash = GroupHashKey::from_sql(&gval);
            let bidx = if let GroupHashKey::Int(gk) = ghash {
                if (0..4096).contains(&gk) {
                    let slot = small_idx[gk as usize];
                    if slot != 0 {
                        (slot - 1) as usize
                    } else {
                        let idx = buckets.len();
                        if metered {
                            mem.reserve(96)?;
                        }
                        buckets.push(GroupBucket {
                            key: gval,
                            count_star: 0,
                            sums: vec![DirectSumState::default(); n_sums],
                        });
                        small_idx[gk as usize] = (idx + 1) as u32;
                        idx
                    }
                } else if let Some(&idx) = other_map.get(&ghash) {
                    idx
                } else {
                    let idx = buckets.len();
                    if metered {
                        let cost = 128u64
                            .saturating_add((n_sums as u64).saturating_mul(48))
                            .saturating_add(val_heap_bytes(&gval).saturating_mul(2));
                        mem.reserve(cost)?;
                    }
                    buckets.push(GroupBucket {
                        key: gval,
                        count_star: 0,
                        sums: vec![DirectSumState::default(); n_sums],
                    });
                    other_map.insert(ghash, idx);
                    idx
                }
            } else if let Some(&idx) = other_map.get(&ghash) {
                idx
            } else {
                let idx = buckets.len();
                if metered {
                    let cost = 128u64
                        .saturating_add((n_sums as u64).saturating_mul(48))
                        .saturating_add(val_heap_bytes(&gval).saturating_mul(2));
                    mem.reserve(cost)?;
                }
                buckets.push(GroupBucket {
                    key: gval,
                    count_star: 0,
                    sums: vec![DirectSumState::default(); n_sums],
                });
                other_map.insert(ghash, idx);
                idx
            };

            buckets[bidx].count_star += 1;
            for (si, inp) in agg_inputs.iter().enumerate() {
                let v = decode_single_field_from_payload(
                    payload,
                    inp.storage_idx,
                    encoding,
                    inp.real_back,
                    &inp.default_val,
                )?;
                buckets[bidx].sums[si].step_value(&v, encoding);
            }
        }
        Ok(true)
    })?;

    // Emit final rows in ascending key order (or group discovery order — SQLite does not
    // require a specific GROUP BY output order without ORDER BY, matching streaming hash GROUP BY).
    let mut out = Vec::with_capacity(buckets.len());
    for b in buckets {
        let mut row = Vec::with_capacity(outputs.len());
        for o in outputs {
            match o {
                DirectGroupOut::GroupKey => row.push(b.key.clone()),
                DirectGroupOut::CountStar => row.push(SqlValue::Int(b.count_star)),
                DirectGroupOut::CountCol { sum_slot } => row.push(SqlValue::Int(b.sums[*sum_slot].cnt)),
                DirectGroupOut::Sum { sum_slot } => row.push(b.sums[*sum_slot].finalize_sum()?),
                DirectGroupOut::Total { sum_slot } => row.push(b.sums[*sum_slot].finalize_total()),
                DirectGroupOut::Avg { sum_slot } => row.push(b.sums[*sum_slot].finalize_avg()),
            }
        }
        if metered {
            mem.reserve(estimate_row_bytes(&row))?;
        }
        out.push(row);
    }
    Ok(out)
}


// ───────────────────── 3. Direct Cumulative Window SUM OVER (ORDER BY id) (#9) ─────────────────────

#[derive(Debug, Clone)]
pub(crate) enum DirectCumulOut {
    TableSlot(DirectTableSlot),
    RunningSum,
    RunningTotal,
}

/// Direct-on-`WritePager` cumulative window `SUM(val) OVER (ORDER BY id)` scanner.
/// Because `id` (rowid / IPK) is strictly unique and table leaves are already stored in ascending
/// rowid order, both `RANGE` and `ROWS` unbounded-preceding frames emit in a single leaf pass!
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_cumul_sum_window_ipk_direct(
    pager: &WritePager,
    table_root: u32,
    hi_rowid: Option<i64>,
    val_storage_idx: usize,
    val_default: &SqlValue,
    val_real_back: bool,
    outputs: &[DirectCumulOut],
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();
    let mut mem = ScanMemGuard::new(metered.then_some(meter.as_ref()));

    let est_cap = hi_rowid.map_or(256, |h| (h.max(0) as usize).min(4096));
    let mut out = Vec::with_capacity(est_cap);
    let mut running = DirectSumState::default();

    // Check ultra-fast 2-column specialization: `[Rowid, RunningSum]` (exact #9 query!).
    let fast_rid_and_sum = outputs.len() == 2
        && matches!(outputs[0], DirectCumulOut::TableSlot(DirectTableSlot::Rowid))
        && matches!(outputs[1], DirectCumulOut::RunningSum)
        && !val_real_back;

    let extra_slots: Vec<DirectTableSlot> = if fast_rid_and_sum {
        Vec::new()
    } else {
        outputs
            .iter()
            .filter_map(|o| match o {
                DirectCumulOut::TableSlot(ts) => Some(ts.clone()),
                _ => None,
            })
            .collect()
    };
    let max_extra_sidx = extra_slots
        .iter()
        .filter_map(|s| match s {
            DirectTableSlot::Rowid => None,
            DirectTableSlot::Field { storage_idx, .. } => Some(*storage_idx),
        })
        .max();

    walk_table_leaves_in_order(pager, table_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        if metered {
            meter.charge_steps((n_cell as u64 / 4).max(1))?;
        }
        for ci in 0..n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            let (rid, n2) = if rid_off < bound && data[rid_off] < 0x80 {
                (i64::from(data[rid_off]), 1usize)
            } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                (
                    (i64::from(data[rid_off] & 0x7f) << 7) | i64::from(data[rid_off + 1]),
                    2usize,
                )
            } else {
                get_varint(data, rid_off)
            };
            if let Some(hi) = hi_rowid {
                if rid > hi {
                    return Ok(false);
                }
            }
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }

            if fast_rid_and_sum && !has_ovfl {
                let payload = &data[local_start..local_start + local];
                if !payload.is_empty() && payload[0] < 0x80 {
                    let hdr_len = usize::from(payload[0]);
                    if hdr_len >= 1 && hdr_len <= payload.len() {
                        let mut hp = 1usize;
                        let mut bp = hdr_len;
                        let mut fi = 0usize;
                        let mut handled = false;
                        while hp < hdr_len {
                            let st_b = payload[hp];
                            let (st, n) = if st_b < 0x80 {
                                (u32::from(st_b), 1usize)
                            } else {
                                get_varint32(payload, hp)
                            };
                            hp += n;
                            if hp > hdr_len {
                                break;
                            }
                            let len = serial_type_length(st);
                            if bp + len > payload.len() {
                                break;
                            }
                            if fi == val_storage_idx {
                                if st == 0 {
                                    handled = true;
                                } else if let Some(iv) = decode_int_serial(st, payload, bp) {
                                    running.step_int(iv);
                                    handled = true;
                                }
                                break;
                            }
                            bp += len;
                            fi += 1;
                        }
                        if handled {
                            let sum_val = running.finalize_sum()?;
                            if metered {
                                mem.reserve(88)?;
                            }
                            out.push(vec![SqlValue::Int(rid), sum_val]);
                            continue;
                        }
                    }
                }
            }

            let owned_full: Vec<u8>;
            let payload: &[u8] = if !has_ovfl {
                &data[local_start..local_start + local]
            } else {
                let ovfl = read_u32_be(data, local_start + local);
                owned_full = read_full_payload_fast(
                    pager,
                    &data[local_start..local_start + local],
                    psize_u32 as usize,
                    ovfl,
                )?;
                &owned_full
            };
            let v = decode_single_field_from_payload(
                payload,
                val_storage_idx,
                encoding,
                val_real_back,
                val_default,
            )?;
            running.step_value(&v, encoding);

            let row = if fast_rid_and_sum {
                vec![SqlValue::Int(rid), running.finalize_sum()?]
            } else {
                let base = decode_direct_slots(payload, rid, &extra_slots, max_extra_sidx, encoding)?;
                let mut bi = 0usize;
                let mut r = Vec::with_capacity(outputs.len());
                for o in outputs {
                    match o {
                        DirectCumulOut::TableSlot(_) => {
                            r.push(base[bi].clone());
                            bi += 1;
                        }
                        DirectCumulOut::RunningSum => r.push(running.finalize_sum()?),
                        DirectCumulOut::RunningTotal => r.push(running.finalize_total()),
                    }
                }
                r
            };
            if metered {
                mem.reserve(estimate_row_bytes(&row))?;
            }
            out.push(row);
        }
        Ok(true)
    })?;

    Ok(out)
}


// ───────────────────── 4. Direct 2-Table Hash Equi-Join (#16) ─────────────────────

#[derive(Debug, Clone)]
pub(crate) enum DirectJoinProj {
    Left(DirectTableSlot),
    Right(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JoinHashKey {
    Int(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl JoinHashKey {
    #[inline]
    fn from_sql(v: &SqlValue, cmp_aff: Affinity) -> Option<Self> {
        if v.is_null() {
            return None;
        }
        let coerced = if cmp_aff >= Affinity::Numeric {
            if let SqlValue::Text(b) = v {
                apply_numeric_affinity(b, false)
            } else {
                None
            }
        } else {
            None
        };
        let eff = coerced.as_ref().unwrap_or(v);
        match eff {
            SqlValue::Null => None,
            SqlValue::Int(i) => Some(Self::Int(*i)),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    return None;
                }
                if r.fract() == 0.0 && *r >= -9223372036854775808.0 && *r < 9223372036854775808.0 {
                    let i = *r as i64;
                    if (i as f64) == *r {
                        return Some(Self::Int(i));
                    }
                }
                let norm = if *r == 0.0 { 0.0f64 } else { *r };
                Some(Self::Real(norm.to_bits()))
            }
            SqlValue::Text(b) => Some(Self::Text(b.to_vec())),
            SqlValue::Blob(b) => Some(Self::Blob(b.to_vec())),
        }
    }

    #[inline]
    fn heap_bytes(&self) -> u64 {
        match self {
            Self::Text(b) | Self::Blob(b) => b.len() as u64,
            _ => 0,
        }
    }
}

/// Direct-on-`WritePager` hash equi-join between two rowid tables (`INNER` or `LEFT` JOIN).
/// Builds a compact small-int `[0..4096)` lookup array (plus `HashMap` overflow for arbitrary
/// keys/duplicates) directly from `right_root`, then streams `left_root` leaves inline.
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_hash_equijoin_direct(
    pager: &WritePager,
    left_root: u32,
    left_key: &DirectTableSlot,
    right_root: u32,
    right_key: &DirectTableSlot,
    right_proj: &[DirectTableSlot],
    out_proj: &[DirectJoinProj],
    cmp_aff: Affinity,
    is_left: bool,
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();
    let mut mem = ScanMemGuard::new(metered.then_some(meter.as_ref()));

    // Phase 1: Build compact hash table from `right_root`.
    let r_max_sidx = right_proj
        .iter()
        .filter_map(|s| match s {
            DirectTableSlot::Rowid => None,
            DirectTableSlot::Field { storage_idx, .. } => Some(*storage_idx),
        })
        .max();

    let mut small_slots = [0u32; 4096]; // 0 = empty, >0 = 1-based unique index, u32::MAX = moved to multi_map
    let mut r_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(64);
    let mut multi_map: HashMap<JoinHashKey, Vec<usize>> = HashMap::new();

    walk_table_leaves_in_order(pager, right_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        for ci in 0..n_cell {
            if metered {
                meter.charge_steps(1)?;
            }
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            let (rid, n2) = get_varint(data, rid_off);
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }
            let owned_full: Vec<u8>;
            let payload: &[u8] = if !has_ovfl {
                &data[local_start..local_start + local]
            } else {
                let ovfl = read_u32_be(data, local_start + local);
                owned_full = read_full_payload_fast(
                    pager,
                    &data[local_start..local_start + local],
                    psize_u32 as usize,
                    ovfl,
                )?;
                &owned_full
            };
            let rkey_val = match right_key {
                DirectTableSlot::Rowid => SqlValue::Int(rid),
                DirectTableSlot::Field {
                    storage_idx,
                    default_val,
                    real_back,
                } => decode_single_field_from_payload(
                    payload,
                    *storage_idx,
                    encoding,
                    *real_back,
                    default_val,
                )?,
            };
            let Some(jk) = JoinHashKey::from_sql(&rkey_val, cmp_aff) else {
                continue;
            };
            let rrow = decode_direct_slots(payload, rid, right_proj, r_max_sidx, encoding)?;
            if metered {
                mem.reserve(estimate_row_bytes(&rrow))?;
            }
            let ridx = r_rows.len();
            r_rows.push(rrow);

            if let JoinHashKey::Int(iv) = jk {
                if (0..4096).contains(&iv) {
                    let slot = small_slots[iv as usize];
                    if slot == 0 {
                        small_slots[iv as usize] = (ridx + 1) as u32;
                    } else if slot == u32::MAX {
                        if metered {
                            mem.reserve(8)?;
                        }
                        multi_map.entry(JoinHashKey::Int(iv)).or_default().push(ridx);
                    } else {
                        let prev = (slot - 1) as usize;
                        small_slots[iv as usize] = u32::MAX;
                        if metered {
                            mem.reserve(64)?;
                        }
                        multi_map.insert(JoinHashKey::Int(iv), vec![prev, ridx]);
                    }
                    continue;
                }
            }
            if metered {
                if multi_map.contains_key(&jk) {
                    mem.reserve(8)?;
                } else {
                    mem.reserve(56u64.saturating_add(jk.heap_bytes()))?;
                }
            }
            multi_map.entry(jk).or_default().push(ridx);
        }
        Ok(true)
    })?;

    if !is_left && r_rows.is_empty() {
        return Ok(Vec::new());
    }

    // Phase 2: Stream `left_root` and probe hash table inline!
    let est_out = if metered {
        64
    } else {
        (crate::write::btree::table_count_rows(pager, left_root).unwrap_or(1024) as usize)
            .min(1_000_000)
    };
    let mut out = Vec::with_capacity(est_out);

    // Ultra-fast specialized path for the common FK join:
    // `INNER JOIN`, unique small-int right table, `SELECT l.id, l.col, r.col`!
    let fast_l_rid_and_1col_r1 = if !is_left
        && multi_map.is_empty()
        && out_proj.len() == 3
        && right_proj.len() == 1
    {
        match (left_key, &out_proj[0], &out_proj[1], &out_proj[2]) {
            (
                DirectTableSlot::Field {
                    storage_idx: lk_sidx,
                    real_back: false,
                    ..
                },
                DirectJoinProj::Left(DirectTableSlot::Rowid),
                DirectJoinProj::Left(DirectTableSlot::Field {
                    storage_idx: lval_sidx,
                    default_val: lval_default,
                    real_back: false,
                }),
                DirectJoinProj::Right(0),
            ) => Some((*lk_sidx, *lval_sidx, lval_default)),
            _ => None,
        }
    } else {
        None
    };
    let r_single_col: Vec<SqlValue> = if fast_l_rid_and_1col_r1.is_some() {
        if metered {
            let extra: u64 = r_rows.iter().fold(0u64, |acc, r| {
                acc.saturating_add(32u64.saturating_add(val_heap_bytes(&r[0])))
            });
            mem.reserve(extra)?;
        }
        r_rows.iter().map(|r| r[0].clone()).collect()
    } else {
        Vec::new()
    };

    let left_slots: Vec<DirectTableSlot> = out_proj
        .iter()
        .filter_map(|p| match p {
            DirectJoinProj::Left(s) => Some(s.clone()),
            DirectJoinProj::Right(_) => None,
        })
        .collect();
    let l_max_sidx = left_slots
        .iter()
        .filter_map(|s| match s {
            DirectTableSlot::Rowid => None,
            DirectTableSlot::Field { storage_idx, .. } => Some(*storage_idx),
        })
        .max();

    walk_table_leaves_in_order(pager, left_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        if metered {
            meter.charge_steps((n_cell as u64 / 4).max(1))?;
        }
        for ci in 0..n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            let (rid, n2) = if rid_off < bound && data[rid_off] < 0x80 {
                (i64::from(data[rid_off]), 1usize)
            } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                (
                    (i64::from(data[rid_off] & 0x7f) << 7) | i64::from(data[rid_off + 1]),
                    2usize,
                )
            } else {
                get_varint(data, rid_off)
            };
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }

            // Ultra-fast single-pass inline register extraction for `(id, val, r.col)` FK join!
            if !has_ovfl {
                if let Some((lk_sidx, lval_sidx, lval_default)) = fast_l_rid_and_1col_r1 {
                    let payload = &data[local_start..local_start + local];
                    if !payload.is_empty() && payload[0] < 0x80 {
                        let hdr_len = usize::from(payload[0]);
                        if hdr_len >= 1 && hdr_len <= payload.len() {
                            let max_f = lk_sidx.max(lval_sidx);
                            let mut hp = 1usize;
                            let mut bp = hdr_len;
                            let mut fi = 0usize;
                            let mut lk_int: Option<i64> = None;
                            let mut lval: Option<SqlValue> = None;
                            let mut fast_ok = true;
                            while hp < hdr_len && fi <= max_f {
                                let st_b = payload[hp];
                                let (st, n) = if st_b < 0x80 {
                                    (u32::from(st_b), 1usize)
                                } else {
                                    get_varint32(payload, hp)
                                };
                                hp += n;
                                if hp > hdr_len {
                                    fast_ok = false;
                                    break;
                                }
                                let len = serial_type_length(st);
                                if bp + len > payload.len() {
                                    fast_ok = false;
                                    break;
                                }
                                if fi == lk_sidx {
                                    if let Some(iv) = decode_int_serial(st, payload, bp) {
                                        lk_int = Some(iv);
                                    } else {
                                        fast_ok = false;
                                        break;
                                    }
                                }
                                if fi == lval_sidx {
                                    if let Some(iv) = decode_int_serial(st, payload, bp) {
                                        lval = Some(SqlValue::Int(iv));
                                    } else {
                                        lval = Some(decode_serial_value(
                                            &payload[bp..bp + len],
                                            st,
                                            encoding,
                                        ));
                                    }
                                }
                                bp += len;
                                fi += 1;
                            }
                            if fast_ok {
                                if let Some(k) = lk_int {
                                    if (0..4096).contains(&k) {
                                        let slot = small_slots[k as usize];
                                        if slot != 0 && slot != u32::MAX {
                                            let lv = lval.unwrap_or_else(|| lval_default.clone());
                                            let rv = r_single_col[(slot - 1) as usize].clone();
                                            let row = vec![SqlValue::Int(rid), lv, rv];
                                            if metered {
                                                mem.reserve(estimate_row_bytes(&row))?;
                                            }
                                            out.push(row);
                                        }
                                        continue;
                                    } else {
                                        // Not in small_slots and multi_map is empty -> guaranteed no match!
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let owned_full: Vec<u8>;
            let payload: &[u8] = if !has_ovfl {
                &data[local_start..local_start + local]
            } else {
                let ovfl = read_u32_be(data, local_start + local);
                owned_full = read_full_payload_fast(
                    pager,
                    &data[local_start..local_start + local],
                    psize_u32 as usize,
                    ovfl,
                )?;
                &owned_full
            };
            let lkey_val = match left_key {
                DirectTableSlot::Rowid => SqlValue::Int(rid),
                DirectTableSlot::Field {
                    storage_idx,
                    default_val,
                    real_back,
                } => decode_single_field_from_payload(
                    payload,
                    *storage_idx,
                    encoding,
                    *real_back,
                    default_val,
                )?,
            };

            let mut matched_one: Option<usize> = None;
            let mut matched_many: Option<&[usize]> = None;
            if let Some(jk) = JoinHashKey::from_sql(&lkey_val, cmp_aff) {
                match jk {
                    JoinHashKey::Int(k) if (0..4096).contains(&k) => {
                        let slot = small_slots[k as usize];
                        if slot == u32::MAX {
                            if let Some(list) = multi_map.get(&JoinHashKey::Int(k)) {
                                matched_many = Some(list.as_slice());
                            }
                        } else if slot != 0 {
                            matched_one = Some((slot - 1) as usize);
                        }
                    }
                    other => {
                        if let Some(list) = multi_map.get(&other) {
                            matched_many = Some(list.as_slice());
                        }
                    }
                }
            }

            if matched_one.is_none() && matched_many.is_none() && !is_left {
                continue;
            }
            let lproj = decode_direct_slots(payload, rid, &left_slots, l_max_sidx, encoding)?;
            if let Some(ridx) = matched_one {
                let rrow = &r_rows[ridx];
                let mut li = 0usize;
                let mut row = Vec::with_capacity(out_proj.len());
                for p in out_proj {
                    match p {
                        DirectJoinProj::Left(_) => {
                            row.push(lproj[li].clone());
                            li += 1;
                        }
                        DirectJoinProj::Right(ri) => row.push(rrow[*ri].clone()),
                    }
                }
                if metered {
                    mem.reserve(estimate_row_bytes(&row))?;
                }
                out.push(row);
            } else if let Some(list) = matched_many {
                for &ridx in list {
                    if metered {
                        meter.charge_steps(1)?;
                    }
                    let rrow = &r_rows[ridx];
                    let mut li = 0usize;
                    let mut row = Vec::with_capacity(out_proj.len());
                    for p in out_proj {
                        match p {
                            DirectJoinProj::Left(_) => {
                                row.push(lproj[li].clone());
                                li += 1;
                            }
                            DirectJoinProj::Right(ri) => row.push(rrow[*ri].clone()),
                        }
                    }
                    if metered {
                        mem.reserve(estimate_row_bytes(&row))?;
                    }
                    out.push(row);
                }
            } else {
                // LEFT JOIN with no match -> NULL for right columns.
                let mut li = 0usize;
                let mut row = Vec::with_capacity(out_proj.len());
                for p in out_proj {
                    match p {
                        DirectJoinProj::Left(_) => {
                            row.push(lproj[li].clone());
                            li += 1;
                        }
                        DirectJoinProj::Right(_) => row.push(SqlValue::Null),
                    }
                }
                if metered {
                    mem.reserve(estimate_row_bytes(&row))?;
                }
                out.push(row);
            }
        }
        Ok(true)
    })?;

    Ok(out)
}

// ───────────────────── 5. Direct COUNT(*) WHERE ... col IN (SELECT ...) (#10) ─────────────────────

/// Direct-on-`WritePager` `SELECT count(*) FROM outer WHERE [rowid <= hi AND] col IN (SELECT sub_col FROM sub)`.
/// Builds a stack-resident `[u64; 256]` bitset (`0..16384`, zero heap alloc!) plus overflow
/// `HashSet` directly from `sub_root`, then streams `outer_root` leaves inline counting hits!
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_count_in_subquery_direct(
    pager: &WritePager,
    outer_root: u32,
    outer_lo: Option<i64>,
    outer_hi: Option<i64>,
    outer_col: &DirectTableSlot,
    sub_root: u32,
    sub_col: &DirectTableSlot,
    cmp_aff: Affinity,
    encoding: TextEncoding,
) -> SqliteResult<i64> {
    if let (Some(l), Some(h)) = (outer_lo, outer_hi) {
        if l > h {
            return Ok(0);
        }
    }
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();
    let mut mem = ScanMemGuard::new(metered.then_some(meter.as_ref()));

    // Phase 1: Populate compact bitset (0..16384) + optional HashSet overflow from `sub_root`.
    const BITSET_MAX: i64 = 16_384;
    let mut bits = [0u64; (BITSET_MAX as usize) / 64];
    let mut any_in_bits = false;
    let mut overflow_set: Option<HashSet<JoinHashKey>> = None;

    walk_table_leaves_in_order(pager, sub_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        if metered {
            meter.charge_steps((n_cell as u64 / 4).max(1))?;
        }
        for ci in 0..n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            let (rid, n2) = if matches!(sub_col, DirectTableSlot::Rowid) {
                get_varint(data, rid_off)
            } else {
                let len = if rid_off < bound && data[rid_off] < 0x80 {
                    1usize
                } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                    2usize
                } else {
                    get_varint(data, rid_off).1
                };
                (0i64, len)
            };
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }
            let val = match sub_col {
                DirectTableSlot::Rowid => SqlValue::Int(rid),
                DirectTableSlot::Field {
                    storage_idx,
                    default_val,
                    real_back,
                } => {
                    if !has_ovfl {
                        decode_single_field_from_payload(
                            &data[local_start..local_start + local],
                            *storage_idx,
                            encoding,
                            *real_back,
                            default_val,
                        )?
                    } else {
                        let ovfl = read_u32_be(data, local_start + local);
                        let full = read_full_payload_fast(
                            pager,
                            &data[local_start..local_start + local],
                            psize_u32 as usize,
                            ovfl,
                        )?;
                        decode_single_field_from_payload(
                            &full,
                            *storage_idx,
                            encoding,
                            *real_back,
                            default_val,
                        )?
                    }
                }
            };
            if let Some(jk) = JoinHashKey::from_sql(&val, cmp_aff) {
                if let JoinHashKey::Int(iv) = jk {
                    if (0..BITSET_MAX).contains(&iv) {
                        bits[(iv as usize) >> 6] |= 1u64 << (iv & 63);
                        any_in_bits = true;
                        continue;
                    }
                }
                let set = overflow_set.get_or_insert_with(HashSet::new);
                if !set.contains(&jk) {
                    if metered {
                        mem.reserve(48u64.saturating_add(jk.heap_bytes()))?;
                    }
                    set.insert(jk);
                }
            }
        }
        Ok(true)
    })?;

    if !any_in_bits && overflow_set.is_none() {
        return Ok(0);
    }

    // Phase 2: Stream `outer_root` leaves in rowid order, counting matching rows inline!
    let mut count = 0i64;
    let fast_outer_sidx = match outer_col {
        DirectTableSlot::Field {
            storage_idx,
            real_back: false,
            ..
        } => Some(*storage_idx),
        _ => None,
    };

    walk_table_leaves_in_order(pager, outer_root, |pgno, data, bound, hdr, n_cell| {
        let ptr_off = hdr + 8;
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: leaf cell pointers out of bounds"
            )));
        }
        if metered {
            meter.charge_steps((n_cell as u64 / 4).max(1))?;
        }
        for ci in 0..n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
            let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                (u32::from(data[pc]), 1usize)
            } else {
                get_payload_size_varint(data, pc)
            };
            let rid_off = pc + n1;
            let need_rid = outer_lo.is_some()
                || outer_hi.is_some()
                || matches!(outer_col, DirectTableSlot::Rowid);
            let (rid, n2) = if !need_rid {
                let len = if rid_off < bound && data[rid_off] < 0x80 {
                    1usize
                } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                    2usize
                } else {
                    get_varint(data, rid_off).1
                };
                (0i64, len)
            } else if rid_off < bound && data[rid_off] < 0x80 {
                (i64::from(data[rid_off]), 1usize)
            } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                (
                    (i64::from(data[rid_off] & 0x7f) << 7) | i64::from(data[rid_off + 1]),
                    2usize,
                )
            } else {
                get_varint(data, rid_off)
            };
            if let Some(lo) = outer_lo {
                if rid < lo {
                    continue;
                }
            }
            if let Some(hi) = outer_hi {
                if rid > hi {
                    return Ok(false);
                }
            }
            let local_start = rid_off + n2;
            let local = if psize_u32 <= limits.max_leaf {
                psize_u32 as usize
            } else {
                local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
            };
            let has_ovfl = psize_u32 > local as u32;
            let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
            if cell_end > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: leaf cell {ci} runs past usable page end"
                )));
            }

            // Ultra-fast register-only integer probe into stack bitset!
            if !has_ovfl && overflow_set.is_none() {
                if let Some(sidx) = fast_outer_sidx {
                    let payload = &data[local_start..local_start + local];
                    if !payload.is_empty() && payload[0] < 0x80 {
                        let hdr_len = usize::from(payload[0]);
                        if hdr_len >= 1 && hdr_len <= payload.len() {
                            let mut hp = 1usize;
                            let mut bp = hdr_len;
                            let mut fi = 0usize;
                            let mut handled = false;
                            while hp < hdr_len {
                                let st_b = payload[hp];
                                let (st, n) = if st_b < 0x80 {
                                    (u32::from(st_b), 1usize)
                                } else {
                                    get_varint32(payload, hp)
                                };
                                hp += n;
                                if hp > hdr_len {
                                    break;
                                }
                                let len = serial_type_length(st);
                                if bp + len > payload.len() {
                                    break;
                                }
                                if fi == sidx {
                                    if st == 0 {
                                        handled = true;
                                    } else if let Some(iv) = decode_int_serial(st, payload, bp) {
                                        if (0..BITSET_MAX).contains(&iv)
                                            && (bits[(iv as usize) >> 6] & (1u64 << (iv & 63))) != 0
                                        {
                                            count += 1;
                                        }
                                        handled = true;
                                    }
                                    break;
                                }
                                bp += len;
                                fi += 1;
                            }
                            if handled {
                                continue;
                            }
                        }
                    }
                }
            }

            let val = match outer_col {
                DirectTableSlot::Rowid => SqlValue::Int(rid),
                DirectTableSlot::Field {
                    storage_idx,
                    default_val,
                    real_back,
                } => {
                    if !has_ovfl {
                        decode_single_field_from_payload(
                            &data[local_start..local_start + local],
                            *storage_idx,
                            encoding,
                            *real_back,
                            default_val,
                        )?
                    } else {
                        let ovfl = read_u32_be(data, local_start + local);
                        let full = read_full_payload_fast(
                            pager,
                            &data[local_start..local_start + local],
                            psize_u32 as usize,
                            ovfl,
                        )?;
                        decode_single_field_from_payload(
                            &full,
                            *storage_idx,
                            encoding,
                            *real_back,
                            default_val,
                        )?
                    }
                }
            };
            if let Some(jk) = JoinHashKey::from_sql(&val, cmp_aff) {
                let hit = match &jk {
                    JoinHashKey::Int(iv) if (0..BITSET_MAX).contains(iv) => {
                        (bits[(*iv as usize) >> 6] & (1u64 << (*iv & 63))) != 0
                    }
                    _ => overflow_set.as_ref().is_some_and(|s| s.contains(&jk)),
                };
                if hit {
                    count += 1;
                }
            }
        }
        Ok(true)
    })?;

    Ok(count)
}
