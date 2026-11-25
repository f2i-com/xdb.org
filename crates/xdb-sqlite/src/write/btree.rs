//! Pure safe Rust SQLite 3 binary B-tree writer (`TableLeaf`/`TableInterior` and
//! `IndexLeaf`/`IndexInterior`) with exact overflow page chains, multi-level splitting,
//! interior deletion, freelist reclamation, and `PRAGMA integrity_check`.

use std::cmp::Ordering;
use std::collections::HashSet;

use xdb_core::value::{mem_compare, BuiltinCollation, Collation};
use xdb_core::{SqlValue, TextEncoding};

use crate::access::KeyInfo;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    decode_serial_value, get_payload_size_varint, get_varint, get_varint32, local_payload_size,
    lock_byte_page, overflow_page_count, payload_limits, read_u32_be, serial_type_length,
    BtreePageType, PayloadLimits, BTREE_MAX_DEPTH, HEADER_SIZE,
};
use crate::format::page::read_u16_be;
use crate::write::pager::{init_empty_btree_page, WritePager};
use crate::write::record::{
    encode_two_int_record_stack, put_varint, put_varint_slice, splice_single_utf8_record_field,
};

/// Create a brand-new empty table or index root page and return its 1-based page number.
pub fn create_btree(pager: &mut WritePager, kind: BtreePageType) -> SqliteResult<u32> {
    let pgno = pager.allocate_page()?;
    let usable = pager.usable_size();
    let buf = pager.write_page_mut(pgno)?;
    init_empty_btree_page(buf, pgno, kind, usable);
    Ok(pgno)
}

/// Free all pages and overflow chains belonging to a B-tree rooted at `root_pgno` (> 1).
pub fn drop_btree(pager: &mut WritePager, root_pgno: u32) -> SqliteResult<()> {
    if root_pgno <= 1 {
        return Ok(());
    }
    let usable = pager.usable_size();
    let usable_len = usable as usize;
    let limits = payload_limits(usable);
    let mut stack = vec![root_pgno];
    let mut seen = HashSet::new();
    let mut to_free = Vec::new();

    while let Some(pgno) = stack.pop() {
        if !seen.insert(pgno) {
            return Err(SqliteError::corrupt(format!(
                "drop_btree: cycle or shared page {pgno}"
            )));
        }
        let data = pager.read_page(pgno)?.to_vec();
        let bound = usable_len.min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!(
                "drop_btree: page {pgno} truncated"
            )));
        }
        let Some(ptype) = BtreePageType::from_byte(data[hdr]) else {
            return Err(SqliteError::corrupt(format!(
                "drop_btree: invalid page type 0x{:02x} on page {pgno}",
                data[hdr]
            )));
        };
        let n_cell = usize::from(read_u16_be(&data, hdr + 3));
        let is_leaf = ptype.is_leaf();
        let is_table = ptype.is_table();
        let ptr_off = hdr + if is_leaf { 8 } else { 12 };
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "drop_btree: cell pointer array overflows page {pgno}"
            )));
        }

        if !is_leaf {
            let right_child = read_u32_be(&data, hdr + 8);
            if right_child != 0 {
                stack.push(right_child);
            }
        }
        let (max_l, min_l) = if is_table && is_leaf {
            (limits.max_leaf, limits.min_leaf)
        } else {
            (limits.max_local, limits.min_local)
        };

        for i in 0..n_cell {
            let pc = usize::from(read_u16_be(&data, ptr_off + 2 * i));
            let mut pos = pc;
            if !is_leaf {
                if pos + 4 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "drop_btree: cell {i} on page {pgno} truncated"
                    )));
                }
                let left_child = read_u32_be(&data, pos);
                if left_child != 0 {
                    stack.push(left_child);
                }
                pos += 4;
            }
            if is_table && !is_leaf {
                continue;
            }
            let (psize_u32, n1) = get_payload_size_varint(&data, pos);
            pos += n1;
            if is_table {
                let (_, n2) = get_varint(&data, pos);
                pos += n2;
            }
            let local = local_payload_size(psize_u32, max_l, min_l, usable);
            if psize_u32 > local as u32 {
                if pos + local + 4 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "drop_btree: cell {i} overflow pointer out of bounds on page {pgno}"
                    )));
                }
                let ovfl = read_u32_be(&data, pos + local);
                free_overflow_chain_tracked(pager, ovfl, &mut seen)?;
            }
        }
        to_free.push(pgno);
    }

    for pgno in to_free {
        pager.free_page(pgno)?;
    }
    Ok(())
}

// ───────────────────────────── Overflow helpers ─────────────────────────────

fn write_overflow_chain(pager: &mut WritePager, overflow_data: &[u8]) -> SqliteResult<u32> {
    if overflow_data.is_empty() {
        return Ok(0);
    }
    let chunk_cap = (pager.usable_size() as usize) - 4;
    let n_pages = overflow_data.len().div_ceil(chunk_cap);
    let mut pgnos = Vec::with_capacity(n_pages);
    for _ in 0..n_pages {
        pgnos.push(pager.allocate_page()?);
    }
    for (idx, &pgno) in pgnos.iter().enumerate() {
        let next_pgno = pgnos.get(idx + 1).copied().unwrap_or(0);
        let start = idx * chunk_cap;
        let end = (start + chunk_cap).min(overflow_data.len());
        let buf = pager.write_page_mut(pgno)?;
        buf.fill(0);
        buf[0..4].copy_from_slice(&next_pgno.to_be_bytes());
        buf[4..4 + (end - start)].copy_from_slice(&overflow_data[start..end]);
    }
    Ok(pgnos[0])
}

fn free_overflow_chain(pager: &mut WritePager, pgno: u32) -> SqliteResult<()> {
    let mut seen = HashSet::new();
    free_overflow_chain_tracked(pager, pgno, &mut seen)
}

fn free_overflow_chain_tracked(
    pager: &mut WritePager,
    mut pgno: u32,
    seen: &mut HashSet<u32>,
) -> SqliteResult<()> {
    let mut to_free = Vec::new();
    while pgno != 0 {
        if !seen.insert(pgno) {
            return Err(SqliteError::corrupt(format!(
                "overflow page cycle or shared overflow page {pgno}"
            )));
        }
        let next = {
            let p = pager.read_page(pgno)?;
            read_u32_be(p, 0)
        };
        to_free.push(pgno);
        pgno = next;
    }
    for p in to_free {
        pager.free_page(p)?;
    }
    Ok(())
}

fn read_full_payload(
    pager: &WritePager,
    local: &[u8],
    total_size: usize,
    mut ovfl_pgno: u32,
) -> SqliteResult<Vec<u8>> {
    if total_size <= local.len() {
        return Ok(local[..total_size].to_vec());
    }
    let chunk_cap = (pager.usable_size() as usize).saturating_sub(4);
    let max_record = (pager.page_size() as u64).saturating_mul(pager.page_count() as u64);
    if total_size as u64 > max_record {
        return Err(SqliteError::corrupt("payload size exceeds database size"));
    }
    let meter = pager.meter().as_ref();
    let metered = !meter.is_noop();
    if metered {
        meter.reserve_memory(total_size as u64)?;
    }
    struct PayloadRelease<'a>(&'a dyn xdb_core::Meter, u64);
    impl Drop for PayloadRelease<'_> {
        fn drop(&mut self) {
            if self.1 > 0 {
                self.0.release_memory(self.1);
            }
        }
    }
    let _guard = PayloadRelease(meter, if metered { total_size as u64 } else { 0 });
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(local);
    let max_steps = pager.page_count() as usize + 1;
    let mut steps = 0usize;
    while out.len() < total_size {
        if ovfl_pgno == 0 {
            return Err(SqliteError::corrupt("truncated overflow chain"));
        }
        steps += 1;
        if steps > max_steps {
            return Err(SqliteError::corrupt("overflow page cycle"));
        }
        let page = pager.read_page(ovfl_pgno)?;
        let next = read_u32_be(page, 0);
        let need = (total_size - out.len()).min(chunk_cap);
        out.extend_from_slice(&page[4..4 + need]);
        ovfl_pgno = next;
    }
    Ok(out)
}

// ───────────────────────────── Raw page packing ─────────────────────────────

#[inline]
fn cell_storage_len(cell_bytes: &[u8]) -> usize {
    cell_bytes.len().max(4)
}

fn cells_fit(pgno: u32, is_leaf: bool, usable_size: usize, cells: &[Vec<u8>]) -> bool {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let hdr_bytes = hdr + if is_leaf { 8 } else { 12 };
    let ptr_bytes = cells.len() * 2;
    let body_bytes: usize = cells.iter().map(|c| cell_storage_len(c)).sum();
    hdr_bytes + ptr_bytes + body_bytes <= usable_size
}

/// Write a cleanly defragmented B-tree page with `cells` in exact order.
fn pack_btree_page(
    pager: &mut WritePager,
    pgno: u32,
    kind: BtreePageType,
    right_child: u32,
    cells: &[Vec<u8>],
) -> SqliteResult<()> {
    let usable = pager.usable_size() as usize;
    let is_leaf = kind.is_leaf();
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + if is_leaf { 8 } else { 12 };

    let ptr_end = ptr_off + cells.len() * 2;
    let total_body: usize = cells.iter().map(|c| cell_storage_len(c)).sum();
    if ptr_end.saturating_add(total_body) > usable {
        return Err(SqliteError::corrupt(format!(
            "pack_btree_page overflow on page {pgno}"
        )));
    }

    let buf = pager.write_page_mut(pgno)?;
    // Preserve bytes 0..100 on page 1; clear the B-tree area cleanly.
    buf[hdr..usable].fill(0);
    buf[hdr] = kind as u8;
    // first_freeblock = 0
    buf[hdr + 1..hdr + 3].copy_from_slice(&0u16.to_be_bytes());
    buf[hdr + 3..hdr + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    buf[hdr + 7] = 0; // fragmented_bytes = 0
    if !is_leaf {
        buf[hdr + 8..hdr + 12].copy_from_slice(&right_child.to_be_bytes());
    }

    let mut content_pos = usable;
    for (i, cell) in cells.iter().enumerate() {
        let alloc = cell_storage_len(cell);
        content_pos -= alloc;
        buf[content_pos..content_pos + cell.len()].copy_from_slice(cell);
        let ptr_u16 = content_pos as u16;
        buf[ptr_off + 2 * i..ptr_off + 2 * i + 2].copy_from_slice(&ptr_u16.to_be_bytes());
    }
    let content_raw: u16 = if content_pos == 65536 {
        0
    } else {
        content_pos as u16
    };
    buf[hdr + 5..hdr + 7].copy_from_slice(&content_raw.to_be_bytes());
    Ok(())
}

/// Fast-path in-place cell insertion into an existing B-tree leaf page when the contiguous
/// unallocated gap between the cell pointer array and the cell content area fits the new cell.
/// Returns `Ok(true)` if inserted in place, `Ok(false)` if defragmentation or splitting is needed.
#[inline]
fn insert_leaf_cell_in_place_unchecked(
    pager: &mut WritePager,
    pgno: u32,
    ins_idx: usize,
    new_raw: &[u8],
) -> SqliteResult<()> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + 8;
    let alloc = cell_storage_len(new_raw);
    let buf = pager.write_page_mut(pgno)?;
    let n_cell = usize::from(read_u16_be(buf, hdr + 3));
    let ptr_end = ptr_off + 2 * n_cell;
    let content_raw = read_u16_be(buf, hdr + 5);
    let content_start = if content_raw == 0 {
        65536usize
    } else {
        usize::from(content_raw)
    };
    let new_pos = content_start - alloc;
    buf[new_pos..new_pos + new_raw.len()].copy_from_slice(new_raw);
    if new_raw.len() < alloc {
        buf[new_pos + new_raw.len()..new_pos + alloc].fill(0);
    }
    if ins_idx < n_cell {
        let shift_start = ptr_off + 2 * ins_idx;
        buf.copy_within(shift_start..ptr_end, shift_start + 2);
    }
    let ptr_u16 = new_pos as u16;
    buf[ptr_off + 2 * ins_idx..ptr_off + 2 * ins_idx + 2].copy_from_slice(&ptr_u16.to_be_bytes());
    let next_n_cell = (n_cell + 1) as u16;
    buf[hdr + 3..hdr + 5].copy_from_slice(&next_n_cell.to_be_bytes());
    let next_content_raw: u16 = if new_pos == 65536 {
        0
    } else {
        new_pos as u16
    };
    buf[hdr + 5..hdr + 7].copy_from_slice(&next_content_raw.to_be_bytes());
    Ok(())
}

fn try_insert_leaf_cell_in_place(
    pager: &mut WritePager,
    pgno: u32,
    ins_idx: usize,
    new_raw: &[u8],
) -> SqliteResult<bool> {
    let usable = pager.usable_size() as usize;
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + 8;
    let alloc = cell_storage_len(new_raw);

    // Check fit on read-only view first so we don't mark page dirty unless it fits (or avoid extra work).
    {
        let data = pager.read_page(pgno)?;
        if hdr + 8 > data.len() {
            return Ok(false);
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        if ins_idx > n_cell {
            return Ok(false);
        }
        let ptr_end = ptr_off + 2 * n_cell;
        let content_raw = read_u16_be(data, hdr + 5);
        let content_start = if content_raw == 0 {
            65536usize
        } else {
            usize::from(content_raw)
        };
        if content_start > usable || ptr_end + 2 + alloc > content_start {
            return Ok(false);
        }
    }

    insert_leaf_cell_in_place_unchecked(pager, pgno, ins_idx, new_raw)?;
    Ok(true)
}

/// Read-only check verifying that freeing `[region_start .. region_start + region_len)`
/// into a leaf page's sorted freeblock list can be done strictly in-place without
/// violating SQLite's 4-byte minimum separation or freeblock invariants.
/// On success returns `Some((prev_fb, prev_sz, next_fb))` so the writer doesn't re-walk the list.
fn find_freeblock_insert_pos(
    data: &[u8],
    pgno: u32,
    usable: usize,
    region_start: usize,
    region_len: usize,
) -> Option<(usize, usize, usize)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    if region_len < 4 || hdr + 8 > usable || usable > data.len() {
        return None;
    }
    let content_raw = read_u16_be(data, hdr + 5);
    let content_start = if content_raw == 0 {
        65536usize
    } else {
        usize::from(content_raw)
    };
    if content_start > usable
        || region_start < content_start
        || region_start.saturating_add(region_len) > usable
    {
        return None;
    }
    let mut fb = usize::from(read_u16_be(data, hdr + 1));
    let mut prev_fb = 0usize;
    let mut prev_sz = 0usize;
    let mut hops = 0usize;
    while fb != 0 && fb < region_start {
        hops += 1;
        if hops > 64 || fb < content_start || fb + 4 > usable {
            return None;
        }
        let next = usize::from(read_u16_be(data, fb));
        let sz = usize::from(read_u16_be(data, fb + 2));
        if sz < 4 || fb.saturating_add(sz) > usable {
            return None;
        }
        if next != 0 && next < fb.saturating_add(sz).saturating_add(4) {
            return None;
        }
        prev_fb = fb;
        prev_sz = sz;
        fb = next;
    }
    if prev_fb != 0 {
        let prev_end = prev_fb + prev_sz;
        if prev_end > region_start {
            return None;
        }
        let gap = region_start - prev_end;
        if gap > 0 && gap < 4 {
            return None;
        }
    }
    if fb != 0 {
        if fb < content_start || fb + 4 > usable {
            return None;
        }
        let next_sz = usize::from(read_u16_be(data, fb + 2));
        if next_sz < 4 || fb.saturating_add(next_sz) > usable {
            return None;
        }
        let region_end = region_start + region_len;
        if region_end > fb {
            return None;
        }
        let gap = fb - region_end;
        if gap > 0 && gap < 4 {
            return None;
        }
    }
    Some((prev_fb, prev_sz, fb))
}

#[inline]
fn can_free_leaf_region_in_place(
    data: &[u8],
    pgno: u32,
    usable: usize,
    region_start: usize,
    region_len: usize,
) -> bool {
    find_freeblock_insert_pos(data, pgno, usable, region_start, region_len).is_some()
}

/// Insert `[region_start .. region_start + region_len)` at precomputed `(prev_fb, prev_sz, fb)`.
#[allow(clippy::too_many_arguments)]
fn free_leaf_region_in_place_at(
    buf: &mut [u8],
    pgno: u32,
    usable: usize,
    region_start: usize,
    region_len: usize,
    prev_fb: usize,
    prev_sz: usize,
    fb: usize,
) {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let content_raw = read_u16_be(buf, hdr + 5);
    let mut content_start = if content_raw == 0 {
        65536usize
    } else {
        usize::from(content_raw)
    };
    let mut first_fb = usize::from(read_u16_be(buf, hdr + 1));

    // Zero freed region to avoid stale cell payload leakage.
    buf[region_start..region_start + region_len].fill(0);

    let (after_next, merged_sz) = if fb != 0 && region_start + region_len == fb {
        let an = usize::from(read_u16_be(buf, fb));
        let nsz = usize::from(read_u16_be(buf, fb + 2));
        buf[fb..fb + 4].fill(0);
        (an, region_len + nsz)
    } else {
        (fb, region_len)
    };

    if prev_fb != 0 && prev_fb + prev_sz == region_start {
        let total_sz = prev_sz + merged_sz;
        buf[prev_fb..prev_fb + 2].copy_from_slice(&(after_next as u16).to_be_bytes());
        buf[prev_fb + 2..prev_fb + 4].copy_from_slice(&(total_sz as u16).to_be_bytes());
    } else {
        buf[region_start..region_start + 2].copy_from_slice(&(after_next as u16).to_be_bytes());
        buf[region_start + 2..region_start + 4].copy_from_slice(&(merged_sz as u16).to_be_bytes());
        if prev_fb != 0 {
            buf[prev_fb..prev_fb + 2].copy_from_slice(&(region_start as u16).to_be_bytes());
        } else {
            first_fb = region_start;
            buf[hdr + 1..hdr + 3].copy_from_slice(&(first_fb as u16).to_be_bytes());
        }
    }

    // Reclaim any leading freeblock touching `content_start` directly back into the unallocated gap.
    while first_fb != 0 && first_fb == content_start && first_fb + 4 <= usable {
        let next = usize::from(read_u16_be(buf, first_fb));
        let sz = usize::from(read_u16_be(buf, first_fb + 2));
        if sz < 4 || content_start.saturating_add(sz) > usable {
            break;
        }
        buf[first_fb..first_fb + 4].fill(0);
        content_start += sz;
        first_fb = next;
    }
    buf[hdr + 1..hdr + 3].copy_from_slice(&(first_fb as u16).to_be_bytes());
    let new_content_raw: u16 = if content_start == 65536 {
        0
    } else {
        content_start as u16
    };
    buf[hdr + 5..hdr + 7].copy_from_slice(&new_content_raw.to_be_bytes());
}

/// Insert `[region_start .. region_start + region_len)` into a leaf page's sorted freeblock
/// chain, coalescing adjacent freeblocks and reclaiming leading freeblocks at `content_start`.
/// Caller MUST have verified `can_free_leaf_region_in_place` first.
fn free_leaf_region_in_place(
    buf: &mut [u8],
    pgno: u32,
    usable: usize,
    region_start: usize,
    region_len: usize,
) {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let first_fb = usize::from(read_u16_be(buf, hdr + 1));
    let mut prev_fb = 0usize;
    let mut prev_sz = 0usize;
    let mut fb = first_fb;
    while fb != 0 && fb < region_start {
        let next = usize::from(read_u16_be(buf, fb));
        let sz = usize::from(read_u16_be(buf, fb + 2));
        prev_fb = fb;
        prev_sz = sz;
        fb = next;
    }
    free_leaf_region_in_place_at(buf, pgno, usable, region_start, region_len, prev_fb, prev_sz, fb);
}

/// Inspect a single `TableLeaf` cell at `cell_idx` with zero allocations.
/// Returns `(cell_offset, cell_len, overflow_pgno)`.
fn inspect_table_leaf_cell(
    data: &[u8],
    pgno: u32,
    usable: u32,
    limits: PayloadLimits,
    cell_idx: usize,
) -> SqliteResult<(usize, usize, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    let ptr_off = hdr + 8;
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * cell_idx));
    if pc >= bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: cell {cell_idx} offset out of bounds"
        )));
    }
    let b0 = data[pc];
    let (psize_u32, n1) = if b0 < 0x80 {
        (u32::from(b0), 1usize)
    } else {
        get_payload_size_varint(data, pc)
    };
    let (_, n2) = get_varint(data, pc + n1);
    let (local, has_ovfl) = if psize_u32 <= limits.max_leaf {
        (psize_u32 as usize, false)
    } else {
        (
            local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable),
            true,
        )
    };
    let cell_len = n1 + n2 + local + if has_ovfl { 4 } else { 0 };
    if pc + cell_len > bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: table leaf cell {cell_idx} runs past usable page end"
        )));
    }
    let ovfl = if has_ovfl {
        read_u32_be(data, pc + n1 + n2 + local)
    } else {
        0
    };
    Ok((pc, cell_len, ovfl))
}

/// Inspect a single `IndexLeaf` cell at `cell_idx` with zero allocations.
/// Returns `(cell_offset, cell_len, overflow_pgno)`.
fn inspect_index_leaf_cell(
    data: &[u8],
    pgno: u32,
    usable: u32,
    limits: PayloadLimits,
    cell_idx: usize,
) -> SqliteResult<(usize, usize, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    let ptr_off = hdr + 8;
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * cell_idx));
    if pc >= bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: index cell {cell_idx} offset out of bounds"
        )));
    }
    let b0 = data[pc];
    let (psize_u32, n1) = if b0 < 0x80 {
        (u32::from(b0), 1usize)
    } else {
        get_payload_size_varint(data, pc)
    };
    let (local, has_ovfl) = if psize_u32 <= limits.max_local {
        (psize_u32 as usize, false)
    } else {
        (
            local_payload_size(psize_u32, limits.max_local, limits.min_local, usable),
            true,
        )
    };
    let cell_len = n1 + local + if has_ovfl { 4 } else { 0 };
    if pc + cell_len > bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: index leaf cell {cell_idx} runs past usable page end"
        )));
    }
    let ovfl = if has_ovfl {
        read_u32_be(data, pc + n1 + local)
    } else {
        0
    };
    Ok((pc, cell_len, ovfl))
}

/// Fast-path in-place deletion of cell `hit_idx` from a non-empty `TableLeaf` or `IndexLeaf`
/// page with `n_cell >= 2`. Returns `Ok(true)` if deleted in place without repacking the page.
#[inline]
#[allow(clippy::too_many_arguments)]
fn delete_leaf_cell_in_place_at(
    pager: &mut WritePager,
    pgno: u32,
    hit_idx: usize,
    pc: usize,
    cell_len: usize,
    prev_fb: usize,
    prev_sz: usize,
    next_fb: usize,
) -> SqliteResult<()> {
    let usable = pager.usable_size() as usize;
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + 8;
    let alloc = cell_len.max(4);
    let buf = pager.write_page_mut(pgno)?;
    let n_cell = usize::from(read_u16_be(buf, hdr + 3));
    let ptr_end = ptr_off + 2 * n_cell;
    if hit_idx + 1 < n_cell {
        buf.copy_within(ptr_off + 2 * (hit_idx + 1)..ptr_end, ptr_off + 2 * hit_idx);
    }
    buf[ptr_end - 2..ptr_end].fill(0);
    buf[hdr + 3..hdr + 5].copy_from_slice(&((n_cell - 1) as u16).to_be_bytes());
    free_leaf_region_in_place_at(buf, pgno, usable, pc, alloc, prev_fb, prev_sz, next_fb);
    Ok(())
}

fn try_delete_leaf_cell_in_place(
    pager: &mut WritePager,
    pgno: u32,
    hit_idx: usize,
    pc: usize,
    cell_len: usize,
) -> SqliteResult<bool> {
    let usable = pager.usable_size() as usize;
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let alloc = cell_len.max(4);
    let (prev_fb, prev_sz, next_fb) = {
        let data = pager.read_page(pgno)?;
        if hdr + 8 > data.len() {
            return Ok(false);
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        if n_cell < 2 || hit_idx >= n_cell {
            return Ok(false);
        }
        let Some(pos) = find_freeblock_insert_pos(data, pgno, usable, pc, alloc) else {
            return Ok(false);
        };
        pos
    };
    delete_leaf_cell_in_place_at(pager, pgno, hit_idx, pc, cell_len, prev_fb, prev_sz, next_fb)?;
    Ok(true)
}

/// Fast-path in-place overwrite of `TableLeaf` cell `hit_idx` with `new_raw` (when same size,
/// smaller, or larger and fitting in the contiguous unallocated gap).
fn try_replace_table_leaf_cell_in_place(
    pager: &mut WritePager,
    pgno: u32,
    hit_idx: usize,
    old_pc: usize,
    old_len: usize,
    new_raw: &[u8],
) -> SqliteResult<bool> {
    let usable = pager.usable_size() as usize;
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + 8;
    let old_alloc = old_len.max(4);
    let new_alloc = cell_storage_len(new_raw);

    enum Plan {
        SameSize,
        ShrinkAtContentStart(usize),
        ShrinkFrag(usize),
        ShrinkFreeblock(usize),
        RelocateToGap(usize),
    }

    let plan = {
        let data = pager.read_page(pgno)?;
        if hdr + 8 > data.len() {
            return Ok(false);
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        if hit_idx >= n_cell {
            return Ok(false);
        }
        let content_raw = read_u16_be(data, hdr + 5);
        let content_start = if content_raw == 0 {
            65536usize
        } else {
            usize::from(content_raw)
        };
        if content_start > usable || old_pc < content_start || old_pc + old_alloc > usable {
            return Ok(false);
        }
        if new_alloc == old_alloc {
            Plan::SameSize
        } else if new_alloc < old_alloc {
            let rem = old_alloc - new_alloc;
            if rem <= 3 {
                if old_pc == content_start {
                    Plan::ShrinkAtContentStart(rem)
                } else if usize::from(data[hdr + 7]) + rem <= 60 {
                    Plan::ShrinkFrag(rem)
                } else {
                    return Ok(false);
                }
            } else if can_free_leaf_region_in_place(data, pgno, usable, old_pc + new_alloc, rem) {
                Plan::ShrinkFreeblock(rem)
            } else {
                return Ok(false);
            }
        } else {
            let ptr_end = ptr_off + 2 * n_cell;
            if ptr_end + new_alloc <= content_start
                && can_free_leaf_region_in_place(data, pgno, usable, old_pc, old_alloc)
            {
                Plan::RelocateToGap(content_start - new_alloc)
            } else {
                return Ok(false);
            }
        }
    };

    let buf = pager.write_page_mut(pgno)?;
    match plan {
        Plan::SameSize => {
            buf[old_pc..old_pc + new_raw.len()].copy_from_slice(new_raw);
            if new_raw.len() < old_alloc {
                buf[old_pc + new_raw.len()..old_pc + old_alloc].fill(0);
            }
        }
        Plan::ShrinkAtContentStart(rem) => {
            let new_pc = old_pc + rem;
            buf[old_pc..new_pc].fill(0);
            buf[new_pc..new_pc + new_raw.len()].copy_from_slice(new_raw);
            if new_raw.len() < new_alloc {
                buf[new_pc + new_raw.len()..new_pc + new_alloc].fill(0);
            }
            buf[ptr_off + 2 * hit_idx..ptr_off + 2 * hit_idx + 2]
                .copy_from_slice(&(new_pc as u16).to_be_bytes());
            let new_cs: u16 = if new_pc == 65536 { 0 } else { new_pc as u16 };
            buf[hdr + 5..hdr + 7].copy_from_slice(&new_cs.to_be_bytes());
        }
        Plan::ShrinkFrag(rem) => {
            buf[old_pc..old_pc + new_raw.len()].copy_from_slice(new_raw);
            buf[old_pc + new_raw.len()..old_pc + old_alloc].fill(0);
            buf[hdr + 7] = buf[hdr + 7].saturating_add(rem as u8);
        }
        Plan::ShrinkFreeblock(rem) => {
            buf[old_pc..old_pc + new_raw.len()].copy_from_slice(new_raw);
            if new_raw.len() < new_alloc {
                buf[old_pc + new_raw.len()..old_pc + new_alloc].fill(0);
            }
            free_leaf_region_in_place(buf, pgno, usable, old_pc + new_alloc, rem);
        }
        Plan::RelocateToGap(new_pc) => {
            buf[new_pc..new_pc + new_raw.len()].copy_from_slice(new_raw);
            if new_raw.len() < new_alloc {
                buf[new_pc + new_raw.len()..new_pc + new_alloc].fill(0);
            }
            let new_cs: u16 = if new_pc == 65536 { 0 } else { new_pc as u16 };
            buf[hdr + 5..hdr + 7].copy_from_slice(&new_cs.to_be_bytes());
            buf[ptr_off + 2 * hit_idx..ptr_off + 2 * hit_idx + 2]
                .copy_from_slice(&(new_pc as u16).to_be_bytes());
            free_leaf_region_in_place(buf, pgno, usable, old_pc, old_alloc);
        }
    }
    Ok(true)
}

// ───────────────────────────── Table B-Tree (rowid -> payload) ─────────────────────────────

#[derive(Debug, Clone)]
struct TableLeafCell {
    rowid: i64,
    raw: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TableInteriorCell {
    left_child: u32,
    key: i64,
}

impl TableInteriorCell {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(13);
        out.extend_from_slice(&self.left_child.to_be_bytes());
        put_varint(self.key as u64, &mut out);
        out
    }
}

fn parse_table_leaf_cells(
    data: &[u8],
    pgno: u32,
    usable: u32,
) -> SqliteResult<Vec<TableLeafCell>> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    if hdr + 8 > data.len() {
        return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
    }
    if data[hdr] != BtreePageType::TableLeaf as u8 {
        return Err(SqliteError::corrupt(format!(
            "expected TableLeaf on page {pgno}, got 0x{:02x}",
            data[hdr]
        )));
    }
    let limits = payload_limits(usable);
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let ptr_off = hdr + 8;
    if ptr_off + 2 * n_cell > data.len() {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: cell pointer array runs past page end"
        )));
    }
    let mut out = Vec::with_capacity(n_cell);
    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        let (psize_u32, n1) = get_payload_size_varint(data, pc);
        let (rowid_u64, n2) = get_varint(data, pc + n1);
        let local = local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable);
        let has_ovfl = psize_u32 > (local as u32);
        let cell_end = pc + n1 + n2 + local + if has_ovfl { 4 } else { 0 };
        if cell_end > (usable as usize).min(data.len()) {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: table leaf cell {i} runs past usable page end"
            )));
        }
        out.push(TableLeafCell {
            rowid: rowid_u64,
            raw: data[pc..cell_end].to_vec(),
        });
    }
    Ok(out)
}

fn parse_table_interior_cells(
    data: &[u8],
    pgno: u32,
) -> SqliteResult<(Vec<TableInteriorCell>, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    if hdr + 12 > data.len() {
        return Err(SqliteError::corrupt(format!("page {pgno}: truncated interior header")));
    }
    if data[hdr] != BtreePageType::TableInterior as u8 {
        return Err(SqliteError::corrupt(format!(
            "expected TableInterior on page {pgno}, got 0x{:02x}",
            data[hdr]
        )));
    }
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let right_child = read_u32_be(data, hdr + 8);
    let ptr_off = hdr + 12;
    if ptr_off + 2 * n_cell > data.len() {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: interior cell pointer array runs past page end"
        )));
    }
    let mut out = Vec::with_capacity(n_cell);
    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        if pc + 4 > data.len() {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: table interior cell {i} runs past page end"
            )));
        }
        let left_child = read_u32_be(data, pc);
        let (key_u64, klen) = get_varint(data, pc + 4);
        if pc + 4 + klen > data.len() {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: table interior cell {i} key runs past page end"
            )));
        }
        out.push(TableInteriorCell {
            left_child,
            key: key_u64,
        });
    }
    Ok((out, right_child))
}

fn build_table_leaf_cell(
    pager: &mut WritePager,
    rowid: i64,
    payload: &[u8],
) -> SqliteResult<TableLeafCell> {
    if payload.len() > i32::MAX as usize {
        return Err(SqliteError::Sql("string or blob too big".to_string()));
    }
    let usable = pager.usable_size();
    let limits = payload_limits(usable);
    let psize = payload.len() as u32;
    let local = local_payload_size(psize, limits.max_leaf, limits.min_leaf, usable);
    let ovfl = if payload.len() > local {
        write_overflow_chain(pager, &payload[local..])?
    } else {
        0
    };
    let mut raw = Vec::with_capacity(18 + local + 4);
    put_varint(u64::from(psize), &mut raw);
    put_varint(rowid as u64, &mut raw);
    raw.extend_from_slice(&payload[..local]);
    if ovfl != 0 {
        raw.extend_from_slice(&ovfl.to_be_bytes());
    }
    Ok(TableLeafCell { rowid, raw })
}

/// Zero-copy binary search for `rowid` on a `TableLeaf` page's 2-byte cell pointer array.
fn table_leaf_binary_search(
    data: &[u8],
    pgno: u32,
    usable: u32,
    rowid: i64,
) -> SqliteResult<Result<usize, usize>> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    if hdr + 8 > bound {
        return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
    }
    if data[hdr] != BtreePageType::TableLeaf as u8 {
        return Err(SqliteError::corrupt(format!(
            "expected TableLeaf on page {pgno}, got 0x{:02x}",
            data[hdr]
        )));
    }
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let ptr_off = hdr + 8;
    if ptr_off + 2 * n_cell > bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: cell pointer array runs past page end"
        )));
    }
    // O(1) rightmost-append fast check for monotonic sequential INSERTs.
    if n_cell > 0 {
        let last = n_cell - 1;
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * last));
        if pc >= bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: cell {last} offset out of bounds"
            )));
        }
        let (_, n1) = get_payload_size_varint(data, pc);
        let (last_rowid, _) = get_varint(data, pc + n1);
        match last_rowid.cmp(&rowid) {
            Ordering::Less => return Ok(Err(n_cell)),
            Ordering::Equal => return Ok(Ok(last)),
            Ordering::Greater => {}
        }
    }
    let mut lo = 0usize;
    let mut hi = n_cell.saturating_sub(1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * mid));
        if pc >= bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: cell {mid} offset out of bounds"
            )));
        }
        let (_, n1) = get_payload_size_varint(data, pc);
        let (cell_rowid, _) = get_varint(data, pc + n1);
        match cell_rowid.cmp(&rowid) {
            Ordering::Equal => return Ok(Ok(mid)),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    Ok(Err(lo))
}

/// Find the maximum existing rowid in a table B-tree (`None` if empty) with zero allocations.
pub fn table_max_rowid(pager: &WritePager, root_pgno: u32) -> SqliteResult<Option<i64>> {
    let usable = pager.usable_size() as usize;
    let mut pgno = root_pgno;
    let mut depth = 0usize;
    loop {
        if depth >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "table_max_rowid: b-tree {root_pgno} exceeds max depth"
            )));
        }
        let data = pager.read_page(pgno)?;
        let bound = usable.min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => {
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                if n_cell == 0 {
                    return Ok(None);
                }
                let ptr_off = hdr + 8;
                if ptr_off + 2 * n_cell > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: cell pointer array runs past page end"
                    )));
                }
                let pc = usize::from(read_u16_be(data, ptr_off + 2 * (n_cell - 1)));
                if pc >= bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: last cell offset out of bounds"
                    )));
                }
                let (_, n1) = get_payload_size_varint(data, pc);
                let (rowid, _) = get_varint(data, pc + n1);
                return Ok(Some(rowid));
            }
            Some(BtreePageType::TableInterior) => {
                if hdr + 12 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: truncated interior header"
                    )));
                }
                let right_child = read_u32_be(data, hdr + 8);
                pgno = right_child;
                depth += 1;
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_max_rowid: invalid page type on page {pgno}"
                )));
            }
        }
    }
}

/// Stack-allocated B-tree descent path `(parent_pgno, child_slot)` — zero heap allocation!
#[derive(Clone, Copy)]
#[derive(Debug)]
struct BtreePath {
    items: [(u32, usize); BTREE_MAX_DEPTH],
    len: usize,
}

impl BtreePath {
    #[inline]
    fn new() -> Self {
        Self {
            items: [(0, 0); BTREE_MAX_DEPTH],
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, item: (u32, usize)) {
        self.items[self.len] = item;
        self.len += 1;
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }
}

impl std::ops::Deref for BtreePath {
    type Target = [(u32, usize)];

    #[inline]
    fn deref(&self) -> &[(u32, usize)] {
        &self.items[..self.len]
    }
}

/// Descend a table B-tree from `root_pgno` to the leaf responsible for `rowid` (zero-copy).
/// Returns `(leaf_pgno, path_of_(parent_pgno, child_slot))` where `child_slot == n_cell` means `right_child`.
fn table_descend(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
) -> SqliteResult<(u32, BtreePath)> {
    let mut pgno = root_pgno;
    let mut path = BtreePath::new();
    loop {
        if path.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "table_descend: b-tree {root_pgno} exceeds max depth"
            )));
        }
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > data.len() {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => return Ok((pgno, path)),
            Some(BtreePageType::TableInterior) => {
                if hdr + 12 > data.len() {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: truncated interior header"
                    )));
                }
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let right_child = read_u32_be(data, hdr + 8);
                let ptr_off = hdr + 12;
                if ptr_off + 2 * n_cell > data.len() {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: interior cell pointer array runs past page end"
                    )));
                }
                // O(1) rightmost-child fast check for monotonic sequential INSERTs.
                let mut lo = 0usize;
                let mut hi = n_cell;
                if n_cell > 0 {
                    let last = n_cell - 1;
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * last));
                    if pc + 4 > data.len() {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: table interior cell {last} runs past page end"
                        )));
                    }
                    let (last_key, _) = get_varint(data, pc + 4);
                    if last_key < rowid {
                        lo = n_cell;
                        hi = n_cell;
                    }
                }
                // Binary search for first slot with `key >= rowid`.
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * mid));
                    if pc + 4 > data.len() {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: table interior cell {mid} runs past page end"
                        )));
                    }
                    let (key, _) = get_varint(data, pc + 4);
                    if key < rowid {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let next = if lo < n_cell {
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * lo));
                    read_u32_be(data, pc)
                } else {
                    right_child
                };
                path.push((pgno, lo));
                pgno = next;
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_descend: invalid page type 0x{:02x} on page {pgno}",
                    data[hdr]
                )));
            }
        }
    }
}

/// Look up whether `rowid` exists in a table B-tree (zero-copy).
pub fn table_contains_rowid(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
) -> SqliteResult<bool> {
    let (leaf_pgno, _) = table_descend(pager, root_pgno, rowid)?;
    let data = pager.read_page(leaf_pgno)?;
    Ok(table_leaf_binary_search(data, leaf_pgno, pager.usable_size(), rowid)?.is_ok())
}

/// Directly decode all stored record columns for `rowid` from a table B-tree without
/// constructing a read-only `SqliteDatabase` or `Pager` (zero payload copy when local!).
pub fn table_get_record_fields(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let (leaf_pgno, _) = table_descend(pager, root_pgno, rowid)?;
    let data = pager.read_page(leaf_pgno)?;
    let Ok(idx) = table_leaf_binary_search(data, leaf_pgno, usable, rowid)? else {
        return Ok(None);
    };
    let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    let pc = usize::from(read_u16_be(data, hdr + 8 + 2 * idx));
    let limits = payload_limits(usable);
    let (psize_u32, n1) = get_payload_size_varint(data, pc);
    let (_, n2) = get_varint(data, pc + n1);
    let local = local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable);
    let local_start = pc + n1 + n2;
    let has_ovfl = psize_u32 > local as u32;
    let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
    if cell_end > bound {
        return Err(SqliteError::corrupt(format!(
            "page {leaf_pgno}: table leaf cell {idx} runs past usable page end"
        )));
    }
    if !has_ovfl {
        Ok(Some(decode_record_fields_raw_metered(
            &data[local_start..local_start + local],
            encoding,
            pager.meter().as_ref(),
        )?))
    } else {
        let ovfl = read_u32_be(data, local_start + local);
        let full = read_full_payload(
            pager,
            &data[local_start..local_start + local],
            psize_u32 as usize,
            ovfl,
        )?;
        Ok(Some(decode_record_fields_raw_metered(
            &full,
            encoding,
            pager.meter().as_ref(),
        )?))
    }
}

/// Count all rows in a rowid table B-tree in O(pages) time directly on `WritePager`
/// (sums leaf `n_cell` without building a `SqliteDatabase` or `TableCursor`).
pub fn table_count_rows(pager: &WritePager, root_pgno: u32) -> SqliteResult<u64> {
    let page_count = pager.page_count();
    if root_pgno == 1 && page_count == 0 {
        return Ok(0);
    }
    let metered = !pager.is_noop_meter();
    let mut total = 0u64;
    let usable = pager.usable_size();
    let mut visited = 0u32;
    let mut visited_pages = std::collections::HashSet::<u32>::new();
    let mut stack: Vec<(u32, usize)> = Vec::with_capacity(BTREE_MAX_DEPTH * 2);
    stack.push((root_pgno, 0));
    while let Some((pgno, depth)) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > page_count || !visited_pages.insert(pgno) {
            return Err(SqliteError::corrupt(format!(
                "table B-tree {root_pgno} contains a cycle"
            )));
        }
        if metered {
            pager.meter().charge_steps(1)?;
        }
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => {
                total = total.saturating_add(n_cell as u64);
            }
            Some(BtreePageType::TableInterior) => {
                if depth + 1 >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "table B-tree {root_pgno} exceeds max depth"
                    )));
                }
                if hdr + 12 + 2 * n_cell > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: truncated interior header"
                    )));
                }
                let rc = read_u32_be(data, hdr + 8);
                stack.push((rc, depth + 1));
                let ptr_off = hdr + 12;
                for i in 0..n_cell {
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
                    if pc + 4 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: interior cell {i} out of bounds"
                        )));
                    }
                    stack.push((read_u32_be(data, pc), depth + 1));
                }
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_count_rows: invalid page type on page {pgno}"
                )));
            }
        }
    }
    Ok(total)
}

#[inline]
fn table_leaf_cell_rowid(
    data: &[u8],
    ptr_off: usize,
    idx: usize,
    bound: usize,
) -> SqliteResult<i64> {
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * idx));
    if pc >= bound {
        return Err(SqliteError::corrupt(format!(
            "table leaf cell {idx} offset out of bounds"
        )));
    }
    let (_, n1) = if data[pc] < 0x80 {
        (u32::from(data[pc]), 1usize)
    } else {
        get_payload_size_varint(data, pc)
    };
    let (rid, _) = get_varint(data, pc + n1);
    Ok(rid)
}

#[inline]
fn table_leaf_bounds(data: &[u8], pgno: u32, usable: u32) -> SqliteResult<Option<(i64, i64)>> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    if hdr + 8 > bound || data[hdr] != BtreePageType::TableLeaf as u8 {
        return Ok(None);
    }
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    if n_cell == 0 {
        return Ok(None);
    }
    let ptr_off = hdr + 8;
    if ptr_off + 2 * n_cell > bound {
        return Ok(None);
    }
    let min_r = table_leaf_cell_rowid(data, ptr_off, 0, bound)?;
    let max_r = if n_cell == 1 {
        min_r
    } else {
        table_leaf_cell_rowid(data, ptr_off, n_cell - 1, bound)?
    };
    if min_r <= max_r {
        Ok(Some((min_r, max_r)))
    } else {
        Ok(None)
    }
}

/// Direct point-lookup decoding ONLY the requested record field indices (`want_fields`)
/// straight into `out_buf` (must have `out_buf.len() >= want_fields.len()`), avoiding
/// an intermediate `Vec<SqlValue>` allocation.
pub fn table_get_record_sparse_fields_into(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
    out_buf: &mut [SqlValue],
) -> SqliteResult<Option<usize>> {
    let mut no_hint = None;
    table_get_record_sparse_fields_into_hinted(
        pager,
        root_pgno,
        rowid,
        encoding,
        want_fields,
        out_buf,
        &mut no_hint,
    )
}

/// Same as [`table_get_record_sparse_fields_into`], reusing a 1-entry cached leaf hint
/// `(leaf_pgno, min_rid, max_rid)` to bypass `table_descend` when consecutive point lookups
/// fall on the same table leaf page.
pub fn table_get_record_sparse_fields_into_hinted(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
    out_buf: &mut [SqlValue],
    leaf_hint: &mut Option<(u32, i64, i64)>,
) -> SqliteResult<Option<usize>> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let (leaf_pgno, used_hint) = match *leaf_hint {
        Some((hleaf, min_r, max_r)) if rowid >= min_r && rowid <= max_r => (hleaf, true),
        _ => (table_descend(pager, root_pgno, rowid)?.0, false),
    };
    let data = pager.read_page(leaf_pgno)?;
    if !used_hint {
        *leaf_hint = table_leaf_bounds(data, leaf_pgno, usable)?
            .map(|(lo, hi)| (leaf_pgno, lo, hi));
    }
    let Ok(idx) = table_leaf_binary_search(data, leaf_pgno, usable, rowid)? else {
        return Ok(None);
    };
    let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    let pc = usize::from(read_u16_be(data, hdr + 8 + 2 * idx));
    let (psize_u32, n1) = get_payload_size_varint(data, pc);
    let (_, n2) = get_varint(data, pc + n1);
    let local = local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable);
    let local_start = pc + n1 + n2;
    let has_ovfl = psize_u32 > local as u32;
    let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
    if cell_end > bound {
        return Err(SqliteError::corrupt(format!(
            "page {leaf_pgno}: table leaf cell {idx} runs past usable page end"
        )));
    }
    if !has_ovfl {
        Ok(Some(decode_record_sparse_fields_into_metered(
            &data[local_start..local_start + local],
            encoding,
            want_fields,
            out_buf,
            pager.meter().as_ref(),
        )?))
    } else {
        let ovfl = read_u32_be(data, local_start + local);
        let full = read_full_payload(
            pager,
            &data[local_start..local_start + local],
            psize_u32 as usize,
            ovfl,
        )?;
        Ok(Some(decode_record_sparse_fields_into_metered(
            &full,
            encoding,
            want_fields,
            out_buf,
            pager.meter().as_ref(),
        )?))
    }
}

/// Direct point-lookup decoding ONLY the requested record field indices (`want_fields`).
/// Returns `Ok(Some((values_aligned_to_want_fields, record_field_count)))` when found,
/// skipping decoding & heap allocation of all unrequested `TEXT`/`BLOB` columns.
pub fn table_get_record_sparse_fields(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
) -> SqliteResult<Option<(Vec<SqlValue>, usize)>> {
    let mut out = vec![SqlValue::Null; want_fields.len()];
    match table_get_record_sparse_fields_into(pager, root_pgno, rowid, encoding, want_fields, &mut out)? {
        Some(n_fields) => Ok(Some((out, n_fields))),
        None => Ok(None),
    }
}

fn decode_record_sparse_fields_into_metered(
    payload: &[u8],
    encoding: TextEncoding,
    want_fields: &[usize],
    out: &mut [SqlValue],
    meter: &dyn xdb_core::Meter,
) -> SqliteResult<usize> {
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty record payload"));
    }
    let max_want = want_fields.iter().copied().max();
    let metered = !meter.is_noop();
    let (hdr_len_u32, mut hdr_pos) = if payload[0] < 0x80 {
        (u32::from(payload[0]), 1usize)
    } else {
        get_varint32(payload, 0)
    };
    let hdr_len = hdr_len_u32 as usize;
    if hdr_len == 0 {
        if payload.len() > 1 {
            return Err(SqliteError::corrupt("record payload has trailing bytes"));
        }
        return Ok(0);
    }
    if hdr_len > payload.len() || hdr_pos >= hdr_len {
        return Err(SqliteError::corrupt("record header out of bounds"));
    }
    let mut body_pos = hdr_len;
    let mut field_idx = 0usize;
    while hdr_pos < hdr_len {
        if metered && (field_idx & 31) == 31 {
            meter.charge_steps(1)?;
        }
        let b = payload[hdr_pos];
        let (st, n) = if b < 0x80 {
            (u32::from(b), 1)
        } else {
            get_varint32(payload, hdr_pos)
        };
        hdr_pos += n;
        if hdr_pos > hdr_len {
            return Err(SqliteError::corrupt("record header varint past header end"));
        }
        let len = serial_type_length(st);
        if body_pos.saturating_add(len) > payload.len() {
            return Err(SqliteError::corrupt("record body out of bounds"));
        }
        if max_want.is_some_and(|mw| field_idx <= mw) {
            for (slot, &wf) in want_fields.iter().enumerate() {
                if wf == field_idx {
                    out[slot] =
                        decode_serial_value(&payload[body_pos..body_pos + len], st, encoding);
                }
            }
        }
        body_pos += len;
        field_idx += 1;
    }
    if body_pos != payload.len() {
        return Err(SqliteError::corrupt("record payload has trailing bytes"));
    }
    Ok(field_idx)
}

fn decode_record_sparse_fields_metered(
    payload: &[u8],
    encoding: TextEncoding,
    want_fields: &[usize],
    meter: &dyn xdb_core::Meter,
) -> SqliteResult<(Vec<SqlValue>, usize)> {
    let mut out = vec![SqlValue::Null; want_fields.len()];
    let n = decode_record_sparse_fields_into_metered(payload, encoding, want_fields, &mut out, meter)?;
    Ok((out, n))
}

/// Decode all columns of a raw SQLite record payload into `Vec<SqlValue>`.
pub fn decode_record_fields_raw(payload: &[u8], encoding: TextEncoding) -> SqliteResult<Vec<SqlValue>> {
    decode_record_fields_raw_metered(payload, encoding, &xdb_core::NoopMeter)
}

fn decode_record_fields_raw_metered(
    payload: &[u8],
    encoding: TextEncoding,
    meter: &dyn xdb_core::Meter,
) -> SqliteResult<Vec<SqlValue>> {
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty record payload"));
    }
    let metered = !meter.is_noop();
    let (hdr_len_u32, mut hdr_pos) = get_varint32(payload, 0);
    let hdr_len = hdr_len_u32 as usize;
    if hdr_len == 0 {
        if payload.len() > 1 {
            return Err(SqliteError::corrupt("record payload has trailing bytes"));
        }
        return Ok(Vec::new());
    }
    if hdr_len > payload.len() || hdr_pos >= hdr_len {
        return Err(SqliteError::corrupt("record header out of bounds"));
    }
    let mut types = Vec::with_capacity(8);
    let mut col_cnt = 0usize;
    while hdr_pos < hdr_len {
        if metered && (col_cnt & 31) == 31 {
            meter.charge_steps(1)?;
        }
        col_cnt += 1;
        let b = payload[hdr_pos];
        let (st, n) = if b < 0x80 {
            (u32::from(b), 1)
        } else {
            get_varint32(payload, hdr_pos)
        };
        hdr_pos += n;
        if hdr_pos > hdr_len {
            return Err(SqliteError::corrupt("record header varint past header end"));
        }
        types.push(st);
    }
    let mut vals = Vec::with_capacity(types.len());
    let mut body_pos = hdr_len;
    for (ci, st) in types.into_iter().enumerate() {
        if metered && (ci & 31) == 31 {
            meter.charge_steps(1)?;
        }
        let len = serial_type_length(st);
        if body_pos.saturating_add(len) > payload.len() {
            return Err(SqliteError::corrupt("record body out of bounds"));
        }
        vals.push(decode_serial_value(
            &payload[body_pos..body_pos + len],
            st,
            encoding,
        ));
        body_pos += len;
    }
    Ok(vals)
}

/// Fast-path monotonic append of `(rowid, payload)` to a table B-tree when the caller has
/// already proven `rowid` is strictly greater than every existing rowid in `root_pgno`.
/// Reuses `hint_leaf` (the cached rightmost leaf page) to skip `table_descend` and binary search
/// entirely on ~99% of sequential inserts, refreshing `hint_leaf` on every leaf split.
pub fn table_append_monotonic(
    pager: &mut WritePager,
    root_pgno: u32,
    hint_leaf: &mut Option<u32>,
    rowid: i64,
    payload: &[u8],
) -> SqliteResult<()> {
    let limits = pager.payload_limits();
    if payload.len() <= 480 && payload.len() <= limits.max_leaf as usize {
        if let Some(leaf_pgno) = *hint_leaf {
            let usable = pager.usable_size() as usize;
            let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
            let ptr_off = hdr + 8;
            let mut small_buf = [0u8; 128];
            let mut large_buf = [0u8; 512];
            let cell: &[u8] = if payload.len() <= 108 {
                let n1 = put_varint_slice(payload.len() as u64, &mut small_buf);
                let n2 = put_varint_slice(rowid as u64, &mut small_buf[n1..]);
                let clen = n1 + n2 + payload.len();
                small_buf[n1 + n2..clen].copy_from_slice(payload);
                &small_buf[..clen]
            } else {
                let n1 = put_varint_slice(payload.len() as u64, &mut large_buf);
                let n2 = put_varint_slice(rowid as u64, &mut large_buf[n1..]);
                let clen = n1 + n2 + payload.len();
                large_buf[n1 + n2..clen].copy_from_slice(payload);
                &large_buf[..clen]
            };
            let alloc = cell_storage_len(cell);
            let can_append = {
                let data = pager.read_page(leaf_pgno)?;
                let bound = usable.min(data.len());
                if hdr + 8 <= bound && data[hdr] == BtreePageType::TableLeaf as u8 {
                    let n_cell = usize::from(read_u16_be(data, hdr + 3));
                    let ptr_end = ptr_off + 2 * n_cell;
                    let content_raw = read_u16_be(data, hdr + 5);
                    let content_start = if content_raw == 0 {
                        65536usize
                    } else {
                        usize::from(content_raw)
                    };
                    if content_start <= bound && ptr_end + 2 + alloc <= content_start {
                        if n_cell == 0 {
                            leaf_pgno == root_pgno
                        } else {
                            let pc = usize::from(read_u16_be(data, ptr_off + 2 * (n_cell - 1)));
                            if pc < bound {
                                let (_, n1) = if data[pc] < 0x80 {
                                    (u32::from(data[pc]), 1usize)
                                } else {
                                    get_payload_size_varint(data, pc)
                                };
                                let (last_rid, _) = get_varint(data, pc + n1);
                                last_rid < rowid
                            } else {
                                false
                            }
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if can_append {
                let buf = pager.write_page_mut(leaf_pgno)?;
                let n_cell = usize::from(read_u16_be(buf, hdr + 3));
                let content_raw = read_u16_be(buf, hdr + 5);
                let content_start = if content_raw == 0 {
                    65536usize
                } else {
                    usize::from(content_raw)
                };
                let new_pos = content_start - alloc;
                buf[new_pos..new_pos + cell.len()].copy_from_slice(cell);
                if cell.len() < alloc {
                    buf[new_pos + cell.len()..new_pos + alloc].fill(0);
                }
                buf[ptr_off + 2 * n_cell..ptr_off + 2 * n_cell + 2]
                    .copy_from_slice(&(new_pos as u16).to_be_bytes());
                buf[hdr + 3..hdr + 5].copy_from_slice(&((n_cell + 1) as u16).to_be_bytes());
                let next_content_raw: u16 = if new_pos == 65536 { 0 } else { new_pos as u16 };
                buf[hdr + 5..hdr + 7].copy_from_slice(&next_content_raw.to_be_bytes());
                return Ok(());
            }
        }
    }

    // Slow path (first insert, leaf full/split, or overflow payload): full descent + refresh hint.
    let usable = pager.usable_size();
    let (leaf_pgno, path) = table_descend(pager, root_pgno, rowid)?;
    let (search_res, leaf_n_cell) = {
        let data = pager.read_page(leaf_pgno)?;
        let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
        let res = table_leaf_binary_search(data, leaf_pgno, usable, rowid)?;
        let nc = usize::from(read_u16_be(data, hdr + 3));
        (res, nc)
    };
    let ins_idx = match search_res {
        Ok(_) => {
            *hint_leaf = None;
            return Err(SqliteError::Sql(format!(
                "UNIQUE constraint failed: rowid {rowid}"
            )));
        }
        Err(idx) => idx,
    };
    let is_rightmost_append = if ins_idx == leaf_n_cell {
        let mut all_right = true;
        for &(parent_pg, child_slot) in &*path {
            let pdata = pager.read_page(parent_pg)?;
            let phdr = if parent_pg == 1 { HEADER_SIZE } else { 0 };
            let pn = usize::from(read_u16_be(pdata, phdr + 3));
            if child_slot != pn {
                all_right = false;
                break;
            }
        }
        all_right
    } else {
        false
    };

    if payload.len() <= 480 && payload.len() <= limits.max_leaf as usize {
        let mut small_buf = [0u8; 128];
        let mut large_buf = [0u8; 512];
        let cell: &[u8] = if payload.len() <= 108 {
            let n1 = put_varint_slice(payload.len() as u64, &mut small_buf);
            let n2 = put_varint_slice(rowid as u64, &mut small_buf[n1..]);
            let clen = n1 + n2 + payload.len();
            small_buf[n1 + n2..clen].copy_from_slice(payload);
            &small_buf[..clen]
        } else {
            let n1 = put_varint_slice(payload.len() as u64, &mut large_buf);
            let n2 = put_varint_slice(rowid as u64, &mut large_buf[n1..]);
            let clen = n1 + n2 + payload.len();
            large_buf[n1 + n2..clen].copy_from_slice(payload);
            &large_buf[..clen]
        };
        if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, cell)? {
            *hint_leaf = is_rightmost_append.then_some(leaf_pgno);
            return Ok(());
        }
        if let Some(new_right) =
            try_table_balance_quick(pager, root_pgno, leaf_pgno, &path, ins_idx, cell)?
        {
            *hint_leaf = Some(new_right);
            return Ok(());
        }
        *hint_leaf = None;
        let mut cells = {
            let data = pager.read_page(leaf_pgno)?;
            parse_table_leaf_cells(data, leaf_pgno, usable)?
        };
        cells.insert(
            ins_idx,
            TableLeafCell {
                rowid,
                raw: cell.to_vec(),
            },
        );
        let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
        let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
        if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
            pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
            if is_rightmost_append {
                *hint_leaf = Some(leaf_pgno);
            }
            return Ok(());
        }
        table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, false)?;
        return Ok(());
    }

    *hint_leaf = None;
    let new_cell = build_table_leaf_cell(pager, rowid, payload)?;
    if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, &new_cell.raw)? {
        if is_rightmost_append {
            *hint_leaf = Some(leaf_pgno);
        }
        return Ok(());
    }
    if let Some(new_right) =
        try_table_balance_quick(pager, root_pgno, leaf_pgno, &path, ins_idx, &new_cell.raw)?
    {
        *hint_leaf = Some(new_right);
        return Ok(());
    }
    let mut cells = {
        let data = pager.read_page(leaf_pgno)?;
        parse_table_leaf_cells(data, leaf_pgno, usable)?
    };
    cells.insert(ins_idx, new_cell);
    let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
    let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
    if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
        pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
        if is_rightmost_append {
            *hint_leaf = Some(leaf_pgno);
        }
        return Ok(());
    }
    table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, false)?;
    Ok(())
}

/// Insert (or replace if `replace == true`) `(rowid, payload)` into a table B-tree.
/// Returns `Ok(true)` if an existing row with `rowid` was replaced, `Ok(false)` on fresh insert.
pub fn table_insert(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    payload: &[u8],
    replace: bool,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let (leaf_pgno, path) = table_descend(pager, root_pgno, rowid)?;

    // Zero-copy probe on leaf page: fast-path in-place insert when rowid is absent and fits!
    let search_res = {
        let data = pager.read_page(leaf_pgno)?;
        table_leaf_binary_search(data, leaf_pgno, usable, rowid)?
    };
    match search_res {
        Ok(_) if !replace => Err(SqliteError::Sql(format!(
            "UNIQUE constraint failed: rowid {rowid}"
        ))),
        Err(ins_idx) => {
            // Stack fast path for normal inline cells (<= 480B, no overflow): zero Vec<u8> alloc!
            let limits = pager.payload_limits();
            if payload.len() <= 480 && payload.len() <= limits.max_leaf as usize {
                let mut small_buf = [0u8; 128];
                let mut large_buf = [0u8; 512];
                let cell: &[u8] = if payload.len() <= 108 {
                    let n1 = put_varint_slice(payload.len() as u64, &mut small_buf);
                    let n2 = put_varint_slice(rowid as u64, &mut small_buf[n1..]);
                    let clen = n1 + n2 + payload.len();
                    small_buf[n1 + n2..clen].copy_from_slice(payload);
                    &small_buf[..clen]
                } else {
                    let n1 = put_varint_slice(payload.len() as u64, &mut large_buf);
                    let n2 = put_varint_slice(rowid as u64, &mut large_buf[n1..]);
                    let clen = n1 + n2 + payload.len();
                    large_buf[n1 + n2..clen].copy_from_slice(payload);
                    &large_buf[..clen]
                };
                if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, cell)? {
                    return Ok(false);
                }
                if try_table_balance_quick(
                    pager, root_pgno, leaf_pgno, &path, ins_idx, cell,
                )?
                .is_some()
                {
                    return Ok(false);
                }
                let mut cells = {
                    let data = pager.read_page(leaf_pgno)?;
                    parse_table_leaf_cells(data, leaf_pgno, usable)?
                };
                cells.insert(
                    ins_idx,
                    TableLeafCell {
                        rowid,
                        raw: cell.to_vec(),
                    },
                );
                let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
                let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
                if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                    pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
                    return Ok(false);
                }
                return table_split_leaf(
                    pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, false,
                );
            }
            let new_cell = build_table_leaf_cell(pager, rowid, payload)?;
            if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, &new_cell.raw)? {
                return Ok(false);
            }
            if try_table_balance_quick(
                pager,
                root_pgno,
                leaf_pgno,
                &path,
                ins_idx,
                &new_cell.raw,
            )?
            .is_some()
            {
                return Ok(false);
            }
            // Fall through to full parse -> defragment or split (reusing `new_cell` without rebuilding!).
            let mut cells = {
                let data = pager.read_page(leaf_pgno)?;
                parse_table_leaf_cells(data, leaf_pgno, usable)?
            };
            cells.insert(ins_idx, new_cell);
            let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
            let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
                return Ok(false);
            }
            table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, false)
        }
        Ok(hit_idx) => {
            // Fast zero-copy inspection + in-place overwrite when cell size matches, shrinks,
            // or fits in the contiguous unallocated gap!
            let limits = pager.payload_limits();
            let (old_pc, old_len, old_ovfl) = {
                let data = pager.read_page(leaf_pgno)?;
                inspect_table_leaf_cell(data, leaf_pgno, usable, limits, hit_idx)?
            };
            if payload.len() <= 480 && payload.len() <= limits.max_leaf as usize {
                let mut stack_cell = [0u8; 512];
                let n1 = put_varint_slice(payload.len() as u64, &mut stack_cell);
                let n2 = put_varint_slice(rowid as u64, &mut stack_cell[n1..]);
                let clen = n1 + n2 + payload.len();
                stack_cell[n1 + n2..clen].copy_from_slice(payload);
                if old_ovfl != 0 {
                    free_overflow_chain(pager, old_ovfl)?;
                }
                if try_replace_table_leaf_cell_in_place(
                    pager,
                    leaf_pgno,
                    hit_idx,
                    old_pc,
                    old_len,
                    &stack_cell[..clen],
                )? {
                    return Ok(true);
                }
                let mut cells = {
                    let data = pager.read_page(leaf_pgno)?;
                    parse_table_leaf_cells(data, leaf_pgno, usable)?
                };
                cells[hit_idx] = TableLeafCell {
                    rowid,
                    raw: stack_cell[..clen].to_vec(),
                };
                let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
                let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
                if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                    pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
                    return Ok(true);
                }
                return table_split_leaf(
                    pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, true,
                );
            }
            // Build replacement cell first so an error (e.g. too-big payload or budget)
            // never leaves the existing row's overflow chain prematurely freed.
            let new_cell = build_table_leaf_cell(pager, rowid, payload)?;
            if old_ovfl != 0 {
                free_overflow_chain(pager, old_ovfl)?;
            }
            if try_replace_table_leaf_cell_in_place(
                pager,
                leaf_pgno,
                hit_idx,
                old_pc,
                old_len,
                &new_cell.raw,
            )? {
                return Ok(true);
            }
            // Fallback: full parse -> defragment or split (old overflow already freed above).
            let mut cells = {
                let data = pager.read_page(leaf_pgno)?;
                parse_table_leaf_cells(data, leaf_pgno, usable)?
            };
            cells[hit_idx] = new_cell;
            let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
            let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
                return Ok(true);
            }
            table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, true)
        }
    }
}

/// SQLite `balance_quick` rightmost-append table leaf split: when appending a strictly
/// larger rowid at the rightmost leaf of a table B-tree (no freeblocks/fragmentation),
/// leave the full left leaf completely untouched (0 bytes moved, ~100% fill factor) and
/// place only `new_cell` onto a fresh right-sibling leaf. Returns `Some(new_right_pgno)` if done.
fn try_table_balance_quick(
    pager: &mut WritePager,
    root_pgno: u32,
    leaf_pgno: u32,
    path: &[(u32, usize)],
    ins_idx: usize,
    new_cell: &[u8],
) -> SqliteResult<Option<u32>> {
    if leaf_pgno == 1 {
        return Ok(None);
    }
    let usable = pager.usable_size() as usize;
    let alloc = cell_storage_len(new_cell);
    if alloc + 2 > usable.saturating_sub(8) {
        return Ok(None);
    }
    let max_left_rowid = {
        let data = pager.read_page(leaf_pgno)?;
        let bound = usable.min(data.len());
        if bound < 8 {
            return Ok(None);
        }
        let first_fb = read_u16_be(data, 1);
        let n_cell = usize::from(read_u16_be(data, 3));
        let frag = data[7];
        if first_fb != 0 || frag != 0 || n_cell == 0 || ins_idx != n_cell {
            return Ok(None);
        }
        let ptr_off = 8 + 2 * (n_cell - 1);
        if ptr_off + 2 > bound {
            return Ok(None);
        }
        let pc = usize::from(read_u16_be(data, ptr_off));
        if pc >= bound {
            return Ok(None);
        }
        let (_, n1) = get_payload_size_varint(data, pc);
        let (rid, _) = get_varint(data, pc + n1);
        rid
    };

    for &(parent_pg, child_slot) in path {
        let pdata = pager.read_page(parent_pg)?;
        let phdr = if parent_pg == 1 { HEADER_SIZE } else { 0 };
        let pn = usize::from(read_u16_be(pdata, phdr + 3));
        if child_slot != pn {
            return Ok(None);
        }
    }

    if path.is_empty() {
        let left_bytes: Vec<u8> = pager.read_page(leaf_pgno)?.to_vec();
        let left_pg = pager.allocate_page()?;
        {
            let lbuf = pager.write_page_mut(left_pg)?;
            lbuf.copy_from_slice(&left_bytes);
        }
        let right_pg = pager.allocate_page()?;
        pack_single_table_leaf_cell(pager, right_pg, new_cell)?;
        let div = TableInteriorCell {
            left_child: left_pg,
            key: max_left_rowid,
        }
        .encode();
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::TableInterior,
            right_pg,
            &[div],
        )?;
        return Ok(Some(right_pg));
    }

    let sib_pg = pager.allocate_page()?;
    pack_single_table_leaf_cell(pager, sib_pg, new_cell)?;
    table_insert_siblings_into_parent(
        pager,
        root_pgno,
        path,
        leaf_pgno,
        &[(max_left_rowid, sib_pg)],
    )?;
    Ok(Some(sib_pg))
}

#[inline]
fn pack_single_table_leaf_cell(
    pager: &mut WritePager,
    pgno: u32,
    cell: &[u8],
) -> SqliteResult<()> {
    debug_assert!(pgno > 1);
    let usable = pager.usable_size() as usize;
    let alloc = cell_storage_len(cell);
    let cell_off = usable - alloc;
    let buf = pager.write_page_mut(pgno)?;
    buf[0] = BtreePageType::TableLeaf as u8;
    buf[1..3].copy_from_slice(&0u16.to_be_bytes());
    buf[3..5].copy_from_slice(&1u16.to_be_bytes());
    let cs_u16 = if cell_off == 65536 {
        0u16
    } else {
        cell_off as u16
    };
    buf[5..7].copy_from_slice(&cs_u16.to_be_bytes());
    buf[7] = 0;
    buf[8..10].copy_from_slice(&(cell_off as u16).to_be_bytes());
    buf[cell_off..cell_off + cell.len()].copy_from_slice(cell);
    if cell.len() < alloc {
        buf[cell_off + cell.len()..cell_off + alloc].fill(0);
    }
    Ok(())
}

fn table_split_leaf(
    pager: &mut WritePager,
    root_pgno: u32,
    leaf_pgno: u32,
    path: &[(u32, usize)],
    rowids: &[i64],
    raw_cells: &[Vec<u8>],
    replaced: bool,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    // Leaf overflowed: partition into 1..=k guaranteed-fitting non-root leaf chunks!
    // Special case: single cell on page 1 that exceeds `usable - 108` but fits on a normal page
    // (`usable - 8`) — SQLite's `balance_deeper` turns page 1 into a 0-cell TableInterior!
    if raw_cells.len() == 1 {
        if !path.is_empty() || leaf_pgno != 1 || !cells_fit(2, true, usable as usize, raw_cells) {
            return Err(SqliteError::corrupt(format!(
                "single table leaf cell ({} bytes) cannot fit on usable size {usable}",
                raw_cells[0].len()
            )));
        }
        let child_pg = pager.allocate_page()?;
        pack_btree_page(pager, child_pg, BtreePageType::TableLeaf, 0, raw_cells)?;
        pack_btree_page(pager, root_pgno, BtreePageType::TableInterior, child_pg, &[])?;
        return Ok(replaced);
    }

    let chunks = partition_table_leaf_cells(raw_cells, usable as usize)?;
    debug_assert!(chunks.len() >= 2);

    if path.is_empty() {
        // Splitting the root leaf into `k >= 2` child leaves and turning root into TableInterior.
        let mut child_pgs = Vec::with_capacity(chunks.len());
        for &(start, end) in &chunks {
            let pg = pager.allocate_page()?;
            pack_btree_page(pager, pg, BtreePageType::TableLeaf, 0, &raw_cells[start..end])?;
            child_pgs.push(pg);
        }
        let mut dividers = Vec::with_capacity(chunks.len() - 1);
        for j in 0..chunks.len() - 1 {
            let div = TableInteriorCell {
                left_child: child_pgs[j],
                key: rowids[chunks[j].1 - 1],
            };
            dividers.push(div.encode());
        }
        let right_pg = *child_pgs.last().expect("chunks >= 2");
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::TableInterior,
            right_pg,
            &dividers,
        )?;
        return Ok(replaced);
    }

    // Non-root leaf split: keep chunk 0 on `leaf_pgno`, allocate fresh pages for chunks `1..`.
    let (s0, e0) = chunks[0];
    pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells[s0..e0])?;
    let mut new_siblings: Vec<(i64, u32)> = Vec::with_capacity(chunks.len() - 1);
    for (j, &(start, end)) in chunks.iter().enumerate().skip(1) {
        let sib_pg = pager.allocate_page()?;
        pack_btree_page(pager, sib_pg, BtreePageType::TableLeaf, 0, &raw_cells[start..end])?;
        let divider_key = rowids[chunks[j - 1].1 - 1];
        new_siblings.push((divider_key, sib_pg));
    }

    table_insert_siblings_into_parent(pager, root_pgno, path, leaf_pgno, &new_siblings)?;
    Ok(replaced)
}

fn table_insert_siblings_into_parent(
    pager: &mut WritePager,
    root_pgno: u32,
    path: &[(u32, usize)],
    left_pg: u32,
    siblings: &[(i64, u32)],
) -> SqliteResult<()> {
    let (parent_pg, child_slot) = *path.last().expect("path non-empty");
    let usable = pager.usable_size() as usize;
    let (mut cells, mut right_child) = {
        let data = pager.read_page(parent_pg)?;
        parse_table_interior_cells(data, parent_pg)?
    };

    // Previously child at `child_slot` was `left_pg`. Now `left_pg` holds chunk 0 and each
    // `(divider_key, sib_pg)` in `siblings` splits off the next chunk to the right.
    let mut cur_left = left_pg;
    let mut insert_at = child_slot;
    for &(div_key, sib_pg) in siblings {
        if insert_at < cells.len() {
            cells[insert_at].left_child = sib_pg;
            cells.insert(
                insert_at,
                TableInteriorCell {
                    left_child: cur_left,
                    key: div_key,
                },
            );
        } else {
            cells.push(TableInteriorCell {
                left_child: cur_left,
                key: div_key,
            });
            right_child = sib_pg;
        }
        cur_left = sib_pg;
        insert_at += 1;
    }

    let raw_cells: Vec<Vec<u8>> = cells.iter().map(TableInteriorCell::encode).collect();
    if cells_fit(parent_pg, false, usable, &raw_cells) {
        pack_btree_page(
            pager,
            parent_pg,
            BtreePageType::TableInterior,
            right_child,
            &raw_cells,
        )?;
        return Ok(());
    }

    // Interior page overflowed: split cells into left, promoted separator `mid`, right!
    // Left gets cells[0..mid] with right_child = cells[mid].left_child;
    // promoted key is cells[mid].key;
    // Right gets cells[mid+1..] with right_child = old right_child!
    let mid = cells.len() / 2;
    let promoted_key = cells[mid].key;
    let left_right_child = cells[mid].left_child;
    let left_raw = &raw_cells[..mid];
    let right_raw = &raw_cells[mid + 1..];

    if path.len() == 1 {
        // Parent is root: balance_deeper!
        let new_left = pager.allocate_page()?;
        let new_right = pager.allocate_page()?;
        pack_btree_page(
            pager,
            new_left,
            BtreePageType::TableInterior,
            left_right_child,
            left_raw,
        )?;
        pack_btree_page(
            pager,
            new_right,
            BtreePageType::TableInterior,
            right_child,
            right_raw,
        )?;
        let root_cell = TableInteriorCell {
            left_child: new_left,
            key: promoted_key,
        };
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::TableInterior,
            new_right,
            &[root_cell.encode()],
        )?;
        return Ok(());
    }

    let new_right = pager.allocate_page()?;
    pack_btree_page(
        pager,
        parent_pg,
        BtreePageType::TableInterior,
        left_right_child,
        left_raw,
    )?;
    pack_btree_page(
        pager,
        new_right,
        BtreePageType::TableInterior,
        right_child,
        right_raw,
    )?;
    table_insert_siblings_into_parent(
        pager,
        root_pgno,
        &path[..path.len() - 1],
        parent_pg,
        &[(promoted_key, new_right)],
    )
}

/// Delete `rowid` from a table B-tree. Returns `Ok(true)` if deleted, `Ok(false)` if absent.
pub fn table_delete(pager: &mut WritePager, root_pgno: u32, rowid: i64) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let (leaf_pgno, path) = table_descend(pager, root_pgno, rowid)?;

    // Zero-copy binary search + in-place leaf deletion without parsing/repacking all cells!
    let (hit_idx, n_cell, pc, cell_len, ovfl) = {
        let data = pager.read_page(leaf_pgno)?;
        let Ok(idx) = table_leaf_binary_search(data, leaf_pgno, usable, rowid)? else {
            return Ok(false);
        };
        let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let (pc, cell_len, ovfl) =
            inspect_table_leaf_cell(data, leaf_pgno, usable, pager.payload_limits(), idx)?;
        (idx, n_cell, pc, cell_len, ovfl)
    };
    if ovfl != 0 {
        free_overflow_chain(pager, ovfl)?;
    }

    if n_cell > 1 {
        if try_delete_leaf_cell_in_place(pager, leaf_pgno, hit_idx, pc, cell_len)? {
            return Ok(true);
        }
        let mut cells = {
            let data = pager.read_page(leaf_pgno)?;
            parse_table_leaf_cells(data, leaf_pgno, usable)?
        };
        cells.remove(hit_idx);
        let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
        pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
        return Ok(true);
    }

    if path.is_empty() {
        // Last cell on root leaf: reset root to empty TableLeaf cleanly.
        let buf = pager.write_page_mut(leaf_pgno)?;
        init_empty_btree_page(buf, leaf_pgno, BtreePageType::TableLeaf, usable);
        return Ok(true);
    }

    // Non-root leaf became empty: free it and detach from parent!
    pager.free_page(leaf_pgno)?;
    table_remove_empty_child(pager, root_pgno, &path)?;
    Ok(true)
}

/// Result of a single-descent raw-record single-field update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastFieldUpdateOutcome {
    NotFound,
    Updated,
    Fallback,
}

/// Single-descent point `UPDATE`: locate `rowid` on its leaf page, splice ONLY storage
/// column `target_sidx` into `record_scratch` without decoding untouched `TEXT`/`BLOB`
/// columns, and overwrite the leaf cell in-place (falling back cleanly for overflow rows,
/// non-UTF-8 DBs, or rows missing `target_sidx`).
pub fn table_update_single_field(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    target_sidx: usize,
    new_val: &SqlValue,
    record_scratch: &mut Vec<u8>,
) -> SqliteResult<FastFieldUpdateOutcome> {
    let mut no_hint = None;
    table_update_single_field_hinted(
        pager,
        root_pgno,
        rowid,
        target_sidx,
        new_val,
        record_scratch,
        &mut no_hint,
    )
}

/// Same as [`table_update_single_field`], reusing a 1-entry cached leaf hint
/// `(leaf_pgno, min_rid, max_rid)` to bypass `table_descend` when consecutive point updates
/// hit the same table leaf page.
pub fn table_update_single_field_hinted(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    target_sidx: usize,
    new_val: &SqlValue,
    record_scratch: &mut Vec<u8>,
    leaf_hint: &mut Option<(u32, i64, i64)>,
) -> SqliteResult<FastFieldUpdateOutcome> {
    if pager.encoding() != TextEncoding::Utf8 {
        return Ok(FastFieldUpdateOutcome::Fallback);
    }
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let schema_format = pager.schema_format();
    let (leaf_pgno, path_opt, used_hint) = match *leaf_hint {
        Some((hleaf, min_r, max_r)) if rowid >= min_r && rowid <= max_r => (hleaf, None, true),
        _ => {
            let (lpg, p) = table_descend(pager, root_pgno, rowid)?;
            (lpg, Some(p), false)
        }
    };

    enum DirectIntWrite {
        None,
        Bytes { abs_off: usize, width: usize, val: i64 },
        ConstSt { abs_hdr_off: usize, st: u8 },
    }

    let (hit_idx, old_pc, old_len, direct_int) = {
        let data = pager.read_page(leaf_pgno)?;
        if !used_hint {
            *leaf_hint = table_leaf_bounds(data, leaf_pgno, usable)?
                .map(|(lo, hi)| (leaf_pgno, lo, hi));
        }
        let Ok(idx) = table_leaf_binary_search(data, leaf_pgno, usable, rowid)? else {
            return Ok(FastFieldUpdateOutcome::NotFound);
        };
        let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
        let bound = (usable as usize).min(data.len());
        let pc = usize::from(read_u16_be(data, hdr + 8 + 2 * idx));
        let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
            (u32::from(data[pc]), 1usize)
        } else {
            get_payload_size_varint(data, pc)
        };
        let (_, n2) = get_varint(data, pc + n1);
        let local = if psize_u32 <= limits.max_leaf {
            psize_u32 as usize
        } else {
            local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
        };
        if psize_u32 > local as u32 {
            return Ok(FastFieldUpdateOutcome::Fallback);
        }
        let local_start = pc + n1 + n2;
        let cell_end = local_start + local;
        if cell_end > bound {
            return Err(SqliteError::corrupt(format!(
                "page {leaf_pgno}: table leaf cell {idx} runs past usable page end"
            )));
        }
        let payload = &data[local_start..cell_end];

        // Ultra-fast same-width in-place integer overwrite: skips record splicing, stack cell
        // copy, and leaf cell replacement completely whenever the integer serial type matches!
        let mut fast_write = DirectIntWrite::None;
        if let SqlValue::Int(iv) = *new_val {
            if !payload.is_empty() && payload[0] < 0x80 {
                let hdr_len = usize::from(payload[0]);
                if hdr_len > 1 && hdr_len <= payload.len() {
                    let want_st: u32 = if schema_format >= 4 && iv == 0 {
                        8
                    } else if schema_format >= 4 && iv == 1 {
                        9
                    } else if (-128..=127).contains(&iv) {
                        1
                    } else if (-32768..=32767).contains(&iv) {
                        2
                    } else if (-8_388_608..=8_388_607).contains(&iv) {
                        3
                    } else if (-2_147_483_648..=2_147_483_647).contains(&iv) {
                        4
                    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&iv) {
                        5
                    } else {
                        6
                    };
                    let mut hpos = 1usize;
                    let mut boff = hdr_len;
                    let mut col = 0usize;
                    while hpos < hdr_len {
                        let st_off = hpos;
                        let hb = payload[hpos];
                        let (st, n) = if hb < 0x80 {
                            (u32::from(hb), 1usize)
                        } else {
                            get_varint32(payload, hpos)
                        };
                        hpos += n;
                        if hpos > hdr_len {
                            break;
                        }
                        let blen = serial_type_length(st);
                        if boff.saturating_add(blen) > payload.len() {
                            break;
                        }
                        if col == target_sidx {
                            if st == want_st && (1..=6).contains(&want_st) {
                                fast_write = DirectIntWrite::Bytes {
                                    abs_off: local_start + boff,
                                    width: blen,
                                    val: iv,
                                };
                            } else if n == 1 && matches!(st, 8 | 9) && matches!(want_st, 8 | 9) {
                                fast_write = DirectIntWrite::ConstSt {
                                    abs_hdr_off: local_start + st_off,
                                    st: want_st as u8,
                                };
                            }
                            break;
                        }
                        boff += blen;
                        col += 1;
                    }
                }
            }
        }

        if matches!(fast_write, DirectIntWrite::None)
            && !splice_single_utf8_record_field(
                payload,
                target_sidx,
                new_val,
                schema_format,
                record_scratch,
            )
        {
            return Ok(FastFieldUpdateOutcome::Fallback);
        }
        (idx, pc, cell_end - pc, fast_write)
    };

    match direct_int {
        DirectIntWrite::Bytes {
            abs_off,
            width,
            val,
        } => {
            let buf = pager.write_page_mut(leaf_pgno)?;
            let be = val.to_be_bytes();
            buf[abs_off..abs_off + width].copy_from_slice(&be[8 - width..8]);
            return Ok(FastFieldUpdateOutcome::Updated);
        }
        DirectIntWrite::ConstSt { abs_hdr_off, st } => {
            let buf = pager.write_page_mut(leaf_pgno)?;
            buf[abs_hdr_off] = st;
            return Ok(FastFieldUpdateOutcome::Updated);
        }
        DirectIntWrite::None => {}
    }

    let payload = record_scratch.as_slice();
    if payload.len() <= 480 && payload.len() <= limits.max_leaf as usize {
        let mut small_buf = [0u8; 128];
        let mut large_buf = [0u8; 512];
        let cell: &[u8] = if payload.len() <= 108 {
            let n1 = put_varint_slice(payload.len() as u64, &mut small_buf);
            let n2 = put_varint_slice(rowid as u64, &mut small_buf[n1..]);
            let clen = n1 + n2 + payload.len();
            small_buf[n1 + n2..clen].copy_from_slice(payload);
            &small_buf[..clen]
        } else {
            let n1 = put_varint_slice(payload.len() as u64, &mut large_buf);
            let n2 = put_varint_slice(rowid as u64, &mut large_buf[n1..]);
            let clen = n1 + n2 + payload.len();
            large_buf[n1 + n2..clen].copy_from_slice(payload);
            &large_buf[..clen]
        };
        if try_replace_table_leaf_cell_in_place(pager, leaf_pgno, hit_idx, old_pc, old_len, cell)? {
            return Ok(FastFieldUpdateOutcome::Updated);
        }
        let mut cells = {
            let data = pager.read_page(leaf_pgno)?;
            parse_table_leaf_cells(data, leaf_pgno, usable)?
        };
        cells[hit_idx] = TableLeafCell {
            rowid,
            raw: cell.to_vec(),
        };
        let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
        let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
        if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
            pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
            return Ok(FastFieldUpdateOutcome::Updated);
        }
        *leaf_hint = None;
        let path = match path_opt {
            Some(p) => p,
            None => table_descend(pager, root_pgno, rowid)?.1,
        };
        table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, true)?;
        return Ok(FastFieldUpdateOutcome::Updated);
    }

    let new_cell = build_table_leaf_cell(pager, rowid, payload)?;
    if try_replace_table_leaf_cell_in_place(
        pager,
        leaf_pgno,
        hit_idx,
        old_pc,
        old_len,
        &new_cell.raw,
    )? {
        return Ok(FastFieldUpdateOutcome::Updated);
    }
    let mut cells = {
        let data = pager.read_page(leaf_pgno)?;
        parse_table_leaf_cells(data, leaf_pgno, usable)?
    };
    cells[hit_idx] = new_cell;
    let rowids: Vec<i64> = cells.iter().map(|c| c.rowid).collect();
    let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
    if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
        pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
        return Ok(FastFieldUpdateOutcome::Updated);
    }
    *leaf_hint = None;
    let path = match path_opt {
        Some(p) => p,
        None => table_descend(pager, root_pgno, rowid)?.1,
    };
    table_split_leaf(pager, root_pgno, leaf_pgno, &path, &rowids, &raw_cells, true)?;
    Ok(FastFieldUpdateOutcome::Updated)
}

/// Update an integer column on a single-page leaf (such as `sqlite_sequence`) using a cached
/// `(page_byte_offset, serial_type)` slot when the integer byte width is unchanged, falling back
/// to [`table_update_single_field`] and refreshing the cached slot whenever the width changes.
#[allow(clippy::too_many_arguments)]
pub fn table_update_int_field_cached(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    target_sidx: usize,
    new_val: i64,
    cached_loc: &mut Option<(usize, u32)>,
    record_scratch: &mut Vec<u8>,
) -> SqliteResult<(FastFieldUpdateOutcome, bool)> {
    let fmt = pager.schema_format();
    let want_st: u32 = if fmt >= 4 && new_val == 0 {
        8
    } else if fmt >= 4 && new_val == 1 {
        9
    } else if (-128..=127).contains(&new_val) {
        1
    } else if (-32768..=32767).contains(&new_val) {
        2
    } else if (-8_388_608..=8_388_607).contains(&new_val) {
        3
    } else if (-2_147_483_648..=2_147_483_647).contains(&new_val) {
        4
    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&new_val) {
        5
    } else {
        6
    };
    if let Some((val_off, cached_st)) = *cached_loc {
        if cached_st == want_st && (1..=6).contains(&want_st) {
            let width = serial_type_length(want_st);
            let hdr = if root_pgno == 1 { HEADER_SIZE } else { 0 };
            let buf = pager.write_page_mut(root_pgno)?;
            if buf.len() >= val_off + width && buf[hdr] == BtreePageType::TableLeaf as u8 {
                let be = new_val.to_be_bytes();
                buf[val_off..val_off + width].copy_from_slice(&be[8 - width..8]);
                return Ok((FastFieldUpdateOutcome::Updated, false));
            }
        }
        *cached_loc = None;
    }

    let val = SqlValue::Int(new_val);
    let outcome = table_update_single_field(
        pager,
        root_pgno,
        rowid,
        target_sidx,
        &val,
        record_scratch,
    )?;
    if matches!(outcome, FastFieldUpdateOutcome::Updated) && (1..=6).contains(&want_st) {
        let usable = pager.usable_size();
        let limits = pager.payload_limits();
        if let Ok(data) = pager.read_page(root_pgno) {
            let hdr = if root_pgno == 1 { HEADER_SIZE } else { 0 };
            if hdr + 8 <= data.len() && data[hdr] == BtreePageType::TableLeaf as u8 {
                if let Ok(Ok(idx)) = table_leaf_binary_search(data, root_pgno, usable, rowid) {
                    let pc = usize::from(read_u16_be(data, hdr + 8 + 2 * idx));
                    let (psize_u32, n1) = get_payload_size_varint(data, pc);
                    let (_, n2) = get_varint(data, pc + n1);
                    let local =
                        local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable);
                    if psize_u32 == local as u32 {
                        let local_start = pc + n1 + n2;
                        let payload = &data[local_start..local_start + local];
                        if !payload.is_empty() && payload[0] < 0x80 {
                            let hdr_len = usize::from(payload[0]);
                            if hdr_len > 1 && hdr_len <= payload.len() {
                                let mut hpos = 1usize;
                                let mut boff = hdr_len;
                                let mut col = 0usize;
                                while hpos < hdr_len {
                                    let (st, n) = if payload[hpos] < 0x80 {
                                        (u32::from(payload[hpos]), 1)
                                    } else {
                                        get_varint32(payload, hpos)
                                    };
                                    hpos += n;
                                    let blen = serial_type_length(st);
                                    if col == target_sidx {
                                        if st == want_st && boff + blen <= payload.len() {
                                            *cached_loc = Some((local_start + boff, st));
                                        }
                                        break;
                                    }
                                    boff += blen;
                                    col += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // `table_update_single_field` may have relocated/repacked cells on `root_pgno`, so signal
    // callers to invalidate any OTHER cached byte offsets on this page!
    Ok((outcome, true))
}

/// Pre-located table leaf cell ready for atomic deferred deletion after secondary index cleanup.
#[derive(Debug)]
pub struct LocatedTableCell {
    pub root_pgno: u32,
    pub leaf_pgno: u32,
    path: BtreePath,
    hit_idx: usize,
    n_cell: usize,
    pc: usize,
    cell_len: usize,
    ovfl: u32,
    pub n_fields: usize,
    fb_pos: Option<(usize, usize, usize)>,
    /// Post-delete leaf hint `(leaf_pgno, min_rid, max_rid)` valid once this cell is removed.
    pub post_delete_hint: Option<(u32, i64, i64)>,
}

/// Read-only locate and sparse decode of a table leaf cell (fusing freeblock insert position
/// lookup into the single `read_page` so callers can delete secondary indexes BEFORE mutating
/// the table page, guaranteeing full statement atomicity with zero redundant page reads).
pub fn table_locate_and_read_sparse_fields_into(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
    out_buf: &mut [SqlValue],
) -> SqliteResult<Option<LocatedTableCell>> {
    let mut no_hint = None;
    table_locate_and_read_sparse_fields_into_hinted(
        pager,
        root_pgno,
        rowid,
        encoding,
        want_fields,
        out_buf,
        &mut no_hint,
    )
}

/// Same as [`table_locate_and_read_sparse_fields_into`], reusing a 1-entry cached leaf hint
/// `(leaf_pgno, min_rid, max_rid)` to bypass `table_descend` when consecutive point deletes
/// fall on the same table leaf page (only when `n_cell > 1`, so `path` is never needed).
pub fn table_locate_and_read_sparse_fields_into_hinted(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
    out_buf: &mut [SqlValue],
    leaf_hint: &mut Option<(u32, i64, i64)>,
) -> SqliteResult<Option<LocatedTableCell>> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let (mut leaf_pgno, mut path, mut used_hint_bounds): (u32, BtreePath, Option<(i64, i64)>) =
        match *leaf_hint {
            Some((hleaf, min_r, max_r)) if rowid >= min_r && rowid <= max_r => {
                (hleaf, BtreePath::new(), Some((min_r, max_r)))
            }
            _ => {
                let (lpg, p) = table_descend(pager, root_pgno, rowid)?;
                (lpg, p, None)
            }
        };
    let mut data = pager.read_page(leaf_pgno)?;
    if used_hint_bounds.is_some() {
        let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
        let bound = (usable as usize).min(data.len());
        if hdr + 8 > bound
            || data[hdr] != BtreePageType::TableLeaf as u8
            || usize::from(read_u16_be(data, hdr + 3)) <= 1
        {
            let (lpg, p) = table_descend(pager, root_pgno, rowid)?;
            leaf_pgno = lpg;
            path = p;
            data = pager.read_page(leaf_pgno)?;
            used_hint_bounds = None;
        }
    }
    let Ok(idx) = table_leaf_binary_search(data, leaf_pgno, usable, rowid)? else {
        if used_hint_bounds.is_none() {
            let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
            let bound = (usable as usize).min(data.len());
            if hdr + 8 <= bound && usize::from(read_u16_be(data, hdr + 3)) > 1 {
                *leaf_hint = table_leaf_bounds(data, leaf_pgno, usable)?
                    .map(|(lo, hi)| (leaf_pgno, lo, hi));
            } else {
                *leaf_hint = None;
            }
        }
        return Ok(None);
    };
    let hdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
    let bound = (usable as usize).min(data.len());
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let ptr_off = hdr + 8;
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * idx));
    let (psize_u32, n1) = get_payload_size_varint(data, pc);
    let (_, n2) = get_varint(data, pc + n1);
    let local = local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable);
    let local_start = pc + n1 + n2;
    let has_ovfl = psize_u32 > local as u32;
    let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
    if cell_end > bound {
        return Err(SqliteError::corrupt(format!(
            "page {leaf_pgno}: table leaf cell {idx} runs past usable page end"
        )));
    }
    let ovfl = if has_ovfl {
        read_u32_be(data, local_start + local)
    } else {
        0
    };
    let n_fields = if want_fields.is_empty() {
        0
    } else if !has_ovfl {
        decode_record_sparse_fields_into_metered(
            &data[local_start..local_start + local],
            encoding,
            want_fields,
            out_buf,
            pager.meter().as_ref(),
        )?
    } else {
        let full = read_full_payload(
            pager,
            &data[local_start..local_start + local],
            psize_u32 as usize,
            ovfl,
        )?;
        decode_record_sparse_fields_into_metered(
            &full,
            encoding,
            want_fields,
            out_buf,
            pager.meter().as_ref(),
        )?
    };
    let cell_len = cell_end - pc;
    let fb_pos = if n_cell > 1 {
        find_freeblock_insert_pos(data, leaf_pgno, usable as usize, pc, cell_len.max(4))
    } else {
        None
    };
    let post_delete_hint = if n_cell <= 2 {
        None
    } else if idx == 0 {
        let lo = table_leaf_cell_rowid(data, ptr_off, 1, bound)?;
        let hi = match used_hint_bounds {
            Some((_, r)) => r,
            None => table_leaf_cell_rowid(data, ptr_off, n_cell - 1, bound)?,
        };
        (lo <= hi).then_some((leaf_pgno, lo, hi))
    } else if idx == n_cell - 1 {
        let lo = match used_hint_bounds {
            Some((l, _)) => l,
            None => table_leaf_cell_rowid(data, ptr_off, 0, bound)?,
        };
        let hi = table_leaf_cell_rowid(data, ptr_off, n_cell - 2, bound)?;
        (lo <= hi).then_some((leaf_pgno, lo, hi))
    } else {
        let (lo, hi) = match used_hint_bounds {
            Some(b) => b,
            None => (
                table_leaf_cell_rowid(data, ptr_off, 0, bound)?,
                table_leaf_cell_rowid(data, ptr_off, n_cell - 1, bound)?,
            ),
        };
        (lo <= hi).then_some((leaf_pgno, lo, hi))
    };
    Ok(Some(LocatedTableCell {
        root_pgno,
        leaf_pgno,
        path,
        hit_idx: idx,
        n_cell,
        pc,
        cell_len,
        ovfl,
        n_fields,
        fb_pos,
        post_delete_hint,
    }))
}

/// Complete deletion of a cell previously located by [`table_locate_and_read_sparse_fields_into`].
pub fn table_finish_delete_located(
    pager: &mut WritePager,
    loc: LocatedTableCell,
) -> SqliteResult<()> {
    let usable = pager.usable_size();
    if loc.ovfl != 0 {
        free_overflow_chain(pager, loc.ovfl)?;
    }
    if loc.n_cell > 1 {
        if let Some((prev_fb, prev_sz, next_fb)) = loc.fb_pos {
            delete_leaf_cell_in_place_at(
                pager,
                loc.leaf_pgno,
                loc.hit_idx,
                loc.pc,
                loc.cell_len,
                prev_fb,
                prev_sz,
                next_fb,
            )?;
        } else {
            let mut cells = {
                let data = pager.read_page(loc.leaf_pgno)?;
                parse_table_leaf_cells(data, loc.leaf_pgno, usable)?
            };
            cells.remove(loc.hit_idx);
            let raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            pack_btree_page(
                pager,
                loc.leaf_pgno,
                BtreePageType::TableLeaf,
                0,
                &raw_cells,
            )?;
        }
        return Ok(());
    }
    if loc.path.is_empty() {
        let buf = pager.write_page_mut(loc.leaf_pgno)?;
        init_empty_btree_page(buf, loc.leaf_pgno, BtreePageType::TableLeaf, usable);
        return Ok(());
    }
    pager.free_page(loc.leaf_pgno)?;
    table_remove_empty_child(pager, loc.root_pgno, &loc.path)
}

/// Single-descent point `DELETE` writing ONLY the sparse stored columns listed in `want_fields`
/// directly into `out_buf` (must have `out_buf.len() >= want_fields.len()`), avoiding heap
/// allocation in point-DELETE hot paths.
pub fn table_delete_and_read_sparse_fields_into(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
    out_buf: &mut [SqlValue],
) -> SqliteResult<Option<usize>> {
    let Some(loc) = table_locate_and_read_sparse_fields_into(
        pager,
        root_pgno,
        rowid,
        encoding,
        want_fields,
        out_buf,
    )?
    else {
        return Ok(None);
    };
    let n_fields = loc.n_fields;
    table_finish_delete_located(pager, loc)?;
    Ok(Some(n_fields))
}

/// Single-descent point `DELETE` returning ONLY the sparse stored columns listed in
/// `want_fields` (e.g. indexed columns) before removing the row from the table B-tree.
pub fn table_delete_and_read_sparse_fields(
    pager: &mut WritePager,
    root_pgno: u32,
    rowid: i64,
    encoding: TextEncoding,
    want_fields: &[usize],
) -> SqliteResult<Option<(Vec<SqlValue>, usize)>> {
    let mut out = vec![SqlValue::Null; want_fields.len()];
    match table_delete_and_read_sparse_fields_into(
        pager,
        root_pgno,
        rowid,
        encoding,
        want_fields,
        &mut out,
    )? {
        Some(n_fields) => Ok(Some((out, n_fields))),
        None => Ok(None),
    }
}

/// Output of direct single-column table B-tree scan for `CREATE INDEX`.
#[derive(Debug)]
pub enum FastIndexScan {
    AllInts(Vec<(i64, i64)>),
    Mixed(Vec<(SqlValue, i64)>),
}

/// Walk a table B-tree rooted at `root_pgno` directly on `pager` (bypassing `SqliteDatabase`,
/// `Pager` Mutex, and `RecordView`), extracting `(storage_idx_value, rowid)` into a compact
/// `Vec<(i64, i64)>` while all values are integers (automatically promoting to `Mixed` if any
/// non-integer, NULL, or overflow row is encountered).
pub fn table_scan_single_stored_col_direct(
    pager: &WritePager,
    root_pgno: u32,
    storage_idx: usize,
    default_val: &SqlValue,
    real_back: bool,
    encoding: TextEncoding,
    reserve_mem: &mut impl FnMut(u64) -> SqliteResult<()>,
) -> SqliteResult<FastIndexScan> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let page_count = pager.page_count();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();

    let mut ints: Vec<(i64, i64)> = Vec::new();
    let mut mixed: Option<Vec<(SqlValue, i64)>> = None;
    let mut visited_count = 0u32;
    let mut visited = std::collections::HashSet::<u32>::new();
    let mut stack: Vec<(u32, usize)> = Vec::with_capacity(BTREE_MAX_DEPTH * 2);
    stack.push((root_pgno, 0));

    let promote = |ints: &mut Vec<(i64, i64)>,
                   mixed: &mut Option<Vec<(SqlValue, i64)>>,
                   reserve_mem: &mut dyn FnMut(u64) -> SqliteResult<()>|
     -> SqliteResult<()> {
        if mixed.is_none() {
            if metered && !ints.is_empty() {
                reserve_mem((ints.len() as u64).saturating_mul(24))?;
            }
            let mut m = Vec::with_capacity(ints.len().saturating_add(16));
            for &(v, rid) in ints.iter() {
                m.push((SqlValue::Int(v), rid));
            }
            ints.clear();
            ints.shrink_to_fit();
            *mixed = Some(m);
        }
        Ok(())
    };

    while let Some((pgno, depth)) = stack.pop() {
        visited_count = visited_count.saturating_add(1);
        if visited_count > page_count || !visited.insert(pgno) {
            return Err(SqliteError::corrupt(format!(
                "table B-tree {root_pgno} contains a cycle"
            )));
        }
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        let bound = (usable as usize).min(data.len());
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableInterior) => {
                if depth + 1 >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "table B-tree {root_pgno} exceeds max depth"
                    )));
                }
                if metered {
                    meter.charge_steps(1)?;
                }
                if hdr + 12 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: truncated interior header"
                    )));
                }
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let right_child = read_u32_be(data, hdr + 8);
                let ptr_off = hdr + 12;
                if ptr_off + 2 * n_cell > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: interior cell pointers out of bounds"
                    )));
                }
                // Push rightmost first so DFS pops leftmost leaf first (natural rowid order).
                stack.push((right_child, depth + 1));
                for ci in (0..n_cell).rev() {
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                    if pc + 4 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: interior cell {ci} out of bounds"
                        )));
                    }
                    stack.push((read_u32_be(data, pc), depth + 1));
                }
            }
            Some(BtreePageType::TableLeaf) => {
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
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
                    let local = if psize_u32 <= limits.max_leaf {
                        psize_u32 as usize
                    } else {
                        local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
                    };
                    let local_start = rid_off + n2;
                    let has_ovfl = psize_u32 > local as u32;
                    let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
                    if cell_end > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: leaf cell {ci} out of bounds"
                        )));
                    }
                    let mut fast_int: Option<i64> = None;
                    let mut decoded_val: Option<SqlValue> = None;
                    if !has_ovfl && !real_back {
                        let payload = &data[local_start..local_start + local];
                        if !payload.is_empty() {
                            let b0 = payload[0];
                            let (hdr_len_u32, mut hpos) = if b0 < 0x80 {
                                (u32::from(b0), 1usize)
                            } else {
                                get_varint32(payload, 0)
                            };
                            let hdr_len = hdr_len_u32 as usize;
                            if hdr_len > hpos && hdr_len <= payload.len() {
                                let mut boff = hdr_len;
                                let mut col = 0usize;
                                let mut target_st: Option<(u32, usize, usize)> = None;
                                while hpos < hdr_len {
                                    if metered && (col & 31) == 31 {
                                        meter.charge_steps(1)?;
                                    }
                                    let hb = payload[hpos];
                                    let (st, n) = if hb < 0x80 {
                                        (u32::from(hb), 1usize)
                                    } else {
                                        get_varint32(payload, hpos)
                                    };
                                    hpos += n;
                                    if hpos > hdr_len {
                                        return Err(SqliteError::corrupt(
                                            "record header varint overrun",
                                        ));
                                    }
                                    let blen = serial_type_length(st);
                                    let end = boff.checked_add(blen).ok_or_else(|| {
                                        SqliteError::corrupt("record body length overflow")
                                    })?;
                                    if end > payload.len() {
                                        return Err(SqliteError::corrupt(
                                            "record body truncated",
                                        ));
                                    }
                                    if col == storage_idx {
                                        if hpos == hdr_len && end != payload.len() {
                                            return Err(SqliteError::corrupt(
                                                "record body has trailing bytes",
                                            ));
                                        }
                                        target_st = Some((st, boff, end));
                                        break;
                                    }
                                    boff = end;
                                    col += 1;
                                }
                                if target_st.is_none() && boff != payload.len() {
                                    return Err(SqliteError::corrupt(
                                        "record body has trailing bytes",
                                    ));
                                }
                                if let Some((st, s_off, e_off)) = target_st {
                                    match st {
                                        8 => fast_int = Some(0),
                                        9 => fast_int = Some(1),
                                        1 => fast_int = Some(i64::from(payload[s_off] as i8)),
                                        2 => {
                                            fast_int = Some(i64::from(i16::from_be_bytes([
                                                payload[s_off],
                                                payload[s_off + 1],
                                            ])))
                                        }
                                        3 => {
                                            let s = &payload[s_off..e_off];
                                            let v = ((i32::from(s[0] as i8)) << 16)
                                                | (i32::from(s[1]) << 8)
                                                | i32::from(s[2]);
                                            fast_int = Some(i64::from(v));
                                        }
                                        4 => {
                                            fast_int = Some(i64::from(i32::from_be_bytes([
                                                payload[s_off],
                                                payload[s_off + 1],
                                                payload[s_off + 2],
                                                payload[s_off + 3],
                                            ])))
                                        }
                                        5 => {
                                            let s = &payload[s_off..e_off];
                                            let v = ((i64::from(s[0] as i8)) << 40)
                                                | (i64::from(s[1]) << 32)
                                                | (i64::from(s[2]) << 24)
                                                | (i64::from(s[3]) << 16)
                                                | (i64::from(s[4]) << 8)
                                                | i64::from(s[5]);
                                            fast_int = Some(v);
                                        }
                                        6 => {
                                            fast_int = Some(i64::from_be_bytes([
                                                payload[s_off],
                                                payload[s_off + 1],
                                                payload[s_off + 2],
                                                payload[s_off + 3],
                                                payload[s_off + 4],
                                                payload[s_off + 5],
                                                payload[s_off + 6],
                                                payload[s_off + 7],
                                            ]))
                                        }
                                        _ => {
                                            decoded_val = Some(decode_serial_value(
                                                &payload[s_off..e_off],
                                                st,
                                                encoding,
                                            ));
                                        }
                                    }
                                } else {
                                    decoded_val = Some(default_val.clone());
                                }
                            }
                        }
                    }

                    if let Some(iv) = fast_int {
                        if let Some(ref mut m) = mixed {
                            if metered {
                                reserve_mem(40)?;
                            }
                            m.push((SqlValue::Int(iv), rid));
                        } else {
                            if metered {
                                reserve_mem(16)?;
                            }
                            ints.push((iv, rid));
                        }
                    } else {
                        let mut val = match decoded_val {
                            Some(v) => v,
                            None => {
                                let (mut dec, n_fields) = if !has_ovfl {
                                    decode_record_sparse_fields_metered(
                                        &data[local_start..local_start + local],
                                        encoding,
                                        &[storage_idx],
                                        meter.as_ref(),
                                    )?
                                } else {
                                    let ovfl = read_u32_be(data, local_start + local);
                                    let full = read_full_payload(
                                        pager,
                                        &data[local_start..local_start + local],
                                        psize_u32 as usize,
                                        ovfl,
                                    )?;
                                    decode_record_sparse_fields_metered(
                                        &full,
                                        encoding,
                                        &[storage_idx],
                                        meter.as_ref(),
                                    )?
                                };
                                if storage_idx < n_fields {
                                    std::mem::replace(&mut dec[0], SqlValue::Null)
                                } else {
                                    default_val.clone()
                                }
                            }
                        };
                        if real_back {
                            val = crate::schema::real_read_back(val);
                        }
                        if mixed.is_none() {
                            if let SqlValue::Int(iv) = val {
                                if metered {
                                    reserve_mem(16)?;
                                }
                                ints.push((iv, rid));
                                continue;
                            }
                            promote(&mut ints, &mut mixed, reserve_mem)?;
                        }
                        if metered {
                            let extra = match &val {
                                SqlValue::Text(b) | SqlValue::Blob(b) => b.len() as u64,
                                _ => 0,
                            };
                            reserve_mem(extra.saturating_add(40))?;
                        }
                        mixed.as_mut().expect("promoted").push((val, rid));
                    }
                }
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_scan_single_stored_col_direct: invalid page type 0x{:02x} on page {pgno}",
                    data[hdr]
                )));
            }
        }
    }

    Ok(match mixed {
        Some(m) => FastIndexScan::Mixed(m),
        None => FastIndexScan::AllInts(ints),
    })
}

fn table_remove_empty_child(
    pager: &mut WritePager,
    root_pgno: u32,
    path: &[(u32, usize)],
) -> SqliteResult<()> {
    let (parent_pg, child_slot) = *path.last().expect("path non-empty");
    let (mut cells, mut right_child) = {
        let data = pager.read_page(parent_pg)?;
        parse_table_interior_cells(data, parent_pg)?
    };

    if child_slot < cells.len() {
        cells.remove(child_slot);
    } else if let Some(last) = cells.pop() {
        right_child = last.left_child;
    } else {
        // Parent had 0 cells and only right_child (now deleted).
        if path.len() == 1 {
            // Root is now completely empty -> reset root to empty TableLeaf!
            let usable = pager.usable_size();
            let buf = pager.write_page_mut(root_pgno)?;
            init_empty_btree_page(buf, root_pgno, BtreePageType::TableLeaf, usable);
            return Ok(());
        }
        pager.free_page(parent_pg)?;
        return table_remove_empty_child(pager, root_pgno, &path[..path.len() - 1]);
    }

    // If parent now has 0 cells (meaning only `right_child` remains):
    if cells.is_empty() {
        if path.len() == 1 {
            // Root interior page has 0 cells and 1 child (`right_child`): try collapsing child up into root!
            if try_collapse_child_into_root(pager, root_pgno, right_child)? {
                return Ok(());
            }
            // If child couldn't fit on page 1 (because of the 100-byte DB header), keep interior with 0 cells?
            // Actually SQLite forbids 0-cell interior pages! Split child if needed or re-pack:
            // Note: page 1 is only `sqlite_schema` (small rows), and non-page-1 roots have identical
            // header offsets so child ALWAYS fits on non-page-1 roots!
            // For page 1 overflow edge case, keep valid interior by splitting child!
            return split_child_into_page1_table(pager, root_pgno, right_child);
        }
        // Non-root parent has 0 cells and 1 remaining child (`right_child`):
        // rebalance with an adjacent sibling interior page so ALL leaves stay at uniform depth!
        return table_rebalance_zero_cell_interior(
            pager,
            root_pgno,
            parent_pg,
            right_child,
            &path[..path.len() - 1],
        );
    }

    let raw: Vec<Vec<u8>> = cells.iter().map(TableInteriorCell::encode).collect();
    pack_btree_page(
        pager,
        parent_pg,
        BtreePageType::TableInterior,
        right_child,
        &raw,
    )
}

/// Rebalance a non-root `TableInterior` page `empty_pg` that dropped to 0 cells (with single
/// remaining child `sole_child`) by merging into or borrowing from an adjacent sibling interior
/// page under its parent (`gp_path`), preserving uniform leaf depth across the entire B-tree.
fn table_rebalance_zero_cell_interior(
    pager: &mut WritePager,
    root_pgno: u32,
    empty_pg: u32,
    sole_child: u32,
    gp_path: &[(u32, usize)],
) -> SqliteResult<()> {
    let usable = pager.usable_size() as usize;
    if gp_path.is_empty() {
        // `empty_pg` IS `root_pgno` (collapsed all the way up to root).
        if try_collapse_child_into_root(pager, root_pgno, sole_child)? {
            return Ok(());
        }
        return split_child_into_page1_table(pager, root_pgno, sole_child);
    }

    let (gp_pg, gp_slot) = *gp_path.last().unwrap();
    let (mut gp_cells, mut gp_rc) = {
        let data = pager.read_page(gp_pg)?;
        parse_table_interior_cells(data, gp_pg)?
    };
    if gp_cells.is_empty() {
        // Grandparent also had 0 cells (only `empty_pg` as `gp_rc`) -> shrinking height uniformly!
        pager.free_page(empty_pg)?;
        return table_rebalance_zero_cell_interior(
            pager,
            root_pgno,
            gp_pg,
            sole_child,
            &gp_path[..gp_path.len() - 1],
        );
    }

    if gp_slot < gp_cells.len() {
        // Merge/redistribute with RIGHT sibling of `empty_pg`.
        let sep_key = gp_cells[gp_slot].key;
        let sib_pg = if gp_slot + 1 < gp_cells.len() {
            gp_cells[gp_slot + 1].left_child
        } else {
            gp_rc
        };
        let (sib_cells, sib_rc) = {
            let data = pager.read_page(sib_pg)?;
            parse_table_interior_cells(data, sib_pg)?
        };
        let mut merged = Vec::with_capacity(1 + sib_cells.len());
        merged.push(TableInteriorCell {
            left_child: sole_child,
            key: sep_key,
        });
        merged.extend(sib_cells);
        let merged_raw: Vec<Vec<u8>> = merged.iter().map(TableInteriorCell::encode).collect();

        if cells_fit(sib_pg, false, usable, &merged_raw) {
            pack_btree_page(pager, sib_pg, BtreePageType::TableInterior, sib_rc, &merged_raw)?;
            pager.free_page(empty_pg)?;
            gp_cells.remove(gp_slot);
            if gp_cells.is_empty() {
                return table_rebalance_zero_cell_interior(
                    pager,
                    root_pgno,
                    gp_pg,
                    sib_pg,
                    &gp_path[..gp_path.len() - 1],
                );
            }
            let gp_raw: Vec<Vec<u8>> = gp_cells.iter().map(TableInteriorCell::encode).collect();
            return pack_btree_page(pager, gp_pg, BtreePageType::TableInterior, gp_rc, &gp_raw);
        }

        // Redistribute between `empty_pg` (left) and `sib_pg` (right).
        let mid = merged.len() / 2;
        let left_rc = merged[mid].left_child;
        let promoted_key = merged[mid].key;
        pack_btree_page(
            pager,
            empty_pg,
            BtreePageType::TableInterior,
            left_rc,
            &merged_raw[..mid],
        )?;
        pack_btree_page(
            pager,
            sib_pg,
            BtreePageType::TableInterior,
            sib_rc,
            &merged_raw[mid + 1..],
        )?;
        gp_cells[gp_slot].key = promoted_key;
        table_pack_or_split_interior(pager, root_pgno, gp_pg, &gp_path[..gp_path.len() - 1], gp_cells, gp_rc)
    } else {
        // `empty_pg` is `gp_rc`: merge/redistribute with LEFT sibling (`gp_cells.last()`).
        let sep_key = gp_cells.last().unwrap().key;
        let sib_pg = gp_cells.last().unwrap().left_child;
        let (sib_cells, sib_rc) = {
            let data = pager.read_page(sib_pg)?;
            parse_table_interior_cells(data, sib_pg)?
        };
        let mut merged = Vec::with_capacity(sib_cells.len() + 1);
        merged.extend(sib_cells);
        merged.push(TableInteriorCell {
            left_child: sib_rc,
            key: sep_key,
        });
        let merged_raw: Vec<Vec<u8>> = merged.iter().map(TableInteriorCell::encode).collect();

        if cells_fit(sib_pg, false, usable, &merged_raw) {
            pack_btree_page(
                pager,
                sib_pg,
                BtreePageType::TableInterior,
                sole_child,
                &merged_raw,
            )?;
            pager.free_page(empty_pg)?;
            gp_cells.pop();
            gp_rc = sib_pg;
            if gp_cells.is_empty() {
                return table_rebalance_zero_cell_interior(
                    pager,
                    root_pgno,
                    gp_pg,
                    sib_pg,
                    &gp_path[..gp_path.len() - 1],
                );
            }
            let gp_raw: Vec<Vec<u8>> = gp_cells.iter().map(TableInteriorCell::encode).collect();
            return pack_btree_page(pager, gp_pg, BtreePageType::TableInterior, gp_rc, &gp_raw);
        }

        // Redistribute between `sib_pg` (left) and `empty_pg` (right).
        let mid = merged.len() / 2;
        let left_rc = merged[mid].left_child;
        let promoted_key = merged[mid].key;
        pack_btree_page(
            pager,
            sib_pg,
            BtreePageType::TableInterior,
            left_rc,
            &merged_raw[..mid],
        )?;
        pack_btree_page(
            pager,
            empty_pg,
            BtreePageType::TableInterior,
            sole_child,
            &merged_raw[mid + 1..],
        )?;
        gp_cells.last_mut().unwrap().key = promoted_key;
        table_pack_or_split_interior(pager, root_pgno, gp_pg, &gp_path[..gp_path.len() - 1], gp_cells, gp_rc)
    }
}

fn table_pack_or_split_interior(
    pager: &mut WritePager,
    root_pgno: u32,
    pgno: u32,
    parent_path: &[(u32, usize)],
    cells: Vec<TableInteriorCell>,
    right_child: u32,
) -> SqliteResult<()> {
    let usable = pager.usable_size() as usize;
    let raw_cells: Vec<Vec<u8>> = cells.iter().map(TableInteriorCell::encode).collect();
    if cells_fit(pgno, false, usable, &raw_cells) {
        return pack_btree_page(pager, pgno, BtreePageType::TableInterior, right_child, &raw_cells);
    }
    let mid = cells.len() / 2;
    let promoted_key = cells[mid].key;
    let left_rc = cells[mid].left_child;
    let left_raw = &raw_cells[..mid];
    let right_raw = &raw_cells[mid + 1..];
    if parent_path.is_empty() {
        let new_left = pager.allocate_page()?;
        let new_right = pager.allocate_page()?;
        pack_btree_page(pager, new_left, BtreePageType::TableInterior, left_rc, left_raw)?;
        pack_btree_page(pager, new_right, BtreePageType::TableInterior, right_child, right_raw)?;
        let root_cell = TableInteriorCell {
            left_child: new_left,
            key: promoted_key,
        };
        return pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::TableInterior,
            new_right,
            &[root_cell.encode()],
        );
    }
    let new_right = pager.allocate_page()?;
    pack_btree_page(pager, pgno, BtreePageType::TableInterior, left_rc, left_raw)?;
    pack_btree_page(pager, new_right, BtreePageType::TableInterior, right_child, right_raw)?;
    table_insert_siblings_into_parent(
        pager,
        root_pgno,
        parent_path,
        pgno,
        &[(promoted_key, new_right)],
    )
}

fn split_child_into_page1_table(
    pager: &mut WritePager,
    root_pgno: u32,
    child_pg: u32,
) -> SqliteResult<()> {
    let usable = pager.usable_size();
    let data = pager.read_page(child_pg)?.to_vec();
    let ptype = BtreePageType::from_byte(data[0])
        .ok_or_else(|| SqliteError::corrupt("invalid child page type"))?;
    match ptype {
        BtreePageType::TableLeaf => {
            let cells = parse_table_leaf_cells(&data, child_pg, usable)?;
            let raw: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
            if cells.len() <= 1 {
                if cells_fit(root_pgno, true, usable as usize, &raw) {
                    pack_btree_page(pager, root_pgno, BtreePageType::TableLeaf, 0, &raw)?;
                    pager.free_page(child_pg)?;
                } else {
                    // Single large cell cannot fit after page 1's 100-byte DB header;
                    // keep valid 0-cell TableInterior on page 1 pointing to `child_pg`.
                    pack_btree_page(pager, root_pgno, BtreePageType::TableInterior, child_pg, &[])?;
                }
                return Ok(());
            }
            let chunks = partition_table_leaf_cells(&raw, usable as usize)?;
            let mut pgs = Vec::with_capacity(chunks.len());
            pgs.push(child_pg);
            pack_btree_page(
                pager,
                child_pg,
                BtreePageType::TableLeaf,
                0,
                &raw[chunks[0].0..chunks[0].1],
            )?;
            for &(start, end) in &chunks[1..] {
                let npg = pager.allocate_page()?;
                pack_btree_page(pager, npg, BtreePageType::TableLeaf, 0, &raw[start..end])?;
                pgs.push(npg);
            }
            let mut divs = Vec::with_capacity(chunks.len() - 1);
            for j in 0..chunks.len() - 1 {
                divs.push(
                    TableInteriorCell {
                        left_child: pgs[j],
                        key: cells[chunks[j].1 - 1].rowid,
                    }
                    .encode(),
                );
            }
            let right_pg = *pgs.last().expect("chunks >= 2");
            pack_btree_page(pager, root_pgno, BtreePageType::TableInterior, right_pg, &divs)
        }
        BtreePageType::TableInterior => {
            let (cells, rc) = parse_table_interior_cells(&data, child_pg)?;
            if cells.is_empty() {
                return Err(SqliteError::corrupt(format!(
                    "page {child_pg}: empty table interior page during split"
                )));
            }
            let raw: Vec<Vec<u8>> = cells.iter().map(TableInteriorCell::encode).collect();
            let mid = cells.len() / 2;
            let right_pg = pager.allocate_page()?;
            pack_btree_page(
                pager,
                child_pg,
                BtreePageType::TableInterior,
                cells[mid].left_child,
                &raw[..mid],
            )?;
            pack_btree_page(
                pager,
                right_pg,
                BtreePageType::TableInterior,
                rc,
                &raw[mid + 1..],
            )?;
            let div = TableInteriorCell {
                left_child: child_pg,
                key: cells[mid].key,
            };
            pack_btree_page(
                pager,
                root_pgno,
                BtreePageType::TableInterior,
                right_pg,
                &[div.encode()],
            )
        }
        _ => Err(SqliteError::corrupt("expected table page")),
    }
}

/// Try to copy `child_pg`'s B-tree cells into `root_pgno` and free `child_pg`.
/// Always succeeds for `root_pgno != 1`; on page 1 succeeds whenever cells fit after the 100-byte header.
fn try_collapse_child_into_root(
    pager: &mut WritePager,
    root_pgno: u32,
    child_pg: u32,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let child_data = pager.read_page(child_pg)?.to_vec();
    let Some(ptype) = BtreePageType::from_byte(child_data[0]) else {
        return Err(SqliteError::corrupt("invalid child page type"));
    };
    let is_leaf = ptype.is_leaf();
    let right_child = if is_leaf {
        0
    } else {
        read_u32_be(&child_data, 8)
    };
    let raw_cells = extract_raw_cells(&child_data, child_pg, ptype, usable)?;
    if !cells_fit(root_pgno, is_leaf, usable as usize, &raw_cells) {
        return Ok(false);
    }
    pack_btree_page(pager, root_pgno, ptype, right_child, &raw_cells)?;
    pager.free_page(child_pg)?;
    Ok(true)
}

fn extract_raw_cells(
    data: &[u8],
    pgno: u32,
    ptype: BtreePageType,
    usable: u32,
) -> SqliteResult<Vec<Vec<u8>>> {
    match ptype {
        BtreePageType::TableLeaf => Ok(parse_table_leaf_cells(data, pgno, usable)?
            .into_iter()
            .map(|c| c.raw)
            .collect()),
        BtreePageType::TableInterior => Ok(parse_table_interior_cells(data, pgno)?
            .0
            .into_iter()
            .map(|c| c.encode())
            .collect()),
        BtreePageType::IndexLeaf => Ok(parse_index_cells(data, pgno, usable)?
            .0
            .into_iter()
            .map(|c| c.raw)
            .collect()),
        BtreePageType::IndexInterior => Ok(parse_index_cells(data, pgno, usable)?
            .0
            .into_iter()
            .map(|c| c.raw)
            .collect()),
    }
}

// ───────────────────────────── Index B-Tree (record key order via KeyInfo) ─────────────────────────────

#[derive(Debug, Clone)]
struct IndexCell {
    left_child: u32,
    /// Raw cell bytes (WITHOUT 4-byte `left_child` prefix for `IndexLeaf`, WITH 4-byte `left_child` for `IndexInterior`).
    raw: Vec<u8>,
    overflow_pgno: u32,
}

impl IndexCell {
    /// Body bytes after the optional 4-byte `left_child` (`varint(size) + local_payload + [ovfl]`).
    fn body(&self, is_leaf: bool) -> &[u8] {
        if is_leaf {
            &self.raw
        } else {
            &self.raw[4..]
        }
    }

    fn as_leaf_raw(&self, was_leaf: bool) -> Vec<u8> {
        self.body(was_leaf).to_vec()
    }
}

fn parse_index_cells(data: &[u8], pgno: u32, usable: u32) -> SqliteResult<(Vec<IndexCell>, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    if hdr + 8 > data.len() {
        return Err(SqliteError::corrupt(format!("page {pgno}: truncated index header")));
    }
    let ptype = BtreePageType::from_byte(data[hdr])
        .ok_or_else(|| SqliteError::corrupt(format!("invalid index page type on page {pgno}")))?;
    let is_leaf = match ptype {
        BtreePageType::IndexLeaf => true,
        BtreePageType::IndexInterior => false,
        _ => {
            return Err(SqliteError::corrupt(format!(
                "expected index page on {pgno}, got {:?}",
                ptype
            )));
        }
    };
    let limits = payload_limits(usable);
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let ptr_off = hdr + if is_leaf { 8 } else { 12 };
    if ptr_off + 2 * n_cell > data.len() {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: index cell pointer array runs past page end"
        )));
    }
    let right_child = if is_leaf {
        0
    } else {
        read_u32_be(data, hdr + 8)
    };
    let mut out = Vec::with_capacity(n_cell);

    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        let mut pos = pc;
        let left_child = if is_leaf {
            0
        } else {
            if pos + 4 > data.len() {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: index interior cell {i} runs past page end"
                )));
            }
            let lc = read_u32_be(data, pos);
            pos += 4;
            lc
        };
        let (psize_u32, n1) = get_payload_size_varint(data, pos);
        pos += n1;
        let local = local_payload_size(psize_u32, limits.max_local, limits.min_local, usable);
        let has_ovfl = psize_u32 > local as u32;
        let cell_end = pos + local + if has_ovfl { 4 } else { 0 };
        if cell_end > (usable as usize).min(data.len()) {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {i} runs past usable page end"
            )));
        }
        let ovfl = if has_ovfl {
            read_u32_be(data, pos + local)
        } else {
            0
        };
        out.push(IndexCell {
            left_child,
            raw: data[pc..cell_end].to_vec(),
            overflow_pgno: ovfl,
        });
    }
    Ok((out, right_child))
}

fn build_index_cell_body(pager: &mut WritePager, payload: &[u8]) -> SqliteResult<Vec<u8>> {
    if payload.len() > i32::MAX as usize {
        return Err(SqliteError::Sql("string or blob too big".to_string()));
    }
    let usable = pager.usable_size();
    let limits = payload_limits(usable);
    let psize = payload.len() as u32;
    let local = local_payload_size(psize, limits.max_local, limits.min_local, usable);
    let ovfl = if payload.len() > local {
        write_overflow_chain(pager, &payload[local..])?
    } else {
        0
    };
    let mut raw = Vec::with_capacity(9 + local + 4);
    put_varint(u64::from(psize), &mut raw);
    raw.extend_from_slice(&payload[..local]);
    if ovfl != 0 {
        raw.extend_from_slice(&ovfl.to_be_bytes());
    }
    Ok(raw)
}

/// Decode all fields of a binary SQLite record payload (used for exact `KeyInfo` comparison).
pub fn decode_record_fields(payload: &[u8], key_info: &KeyInfo) -> SqliteResult<Vec<SqlValue>> {
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty index record payload"));
    }
    let (hdr_len, mut hpos) = get_varint32(payload, 0);
    let hdr_end = hdr_len as usize;
    if hdr_end == 0 {
        return Err(SqliteError::corrupt("zero-length index record header"));
    }
    if hdr_end < hpos || hdr_end > payload.len() {
        return Err(SqliteError::corrupt("index record header exceeds payload"));
    }
    let mut serials = Vec::new();
    while hpos < hdr_end {
        let (st, n) = get_varint32(payload, hpos);
        serials.push(st);
        hpos += n;
    }
    if hpos != hdr_end {
        return Err(SqliteError::corrupt("index record header varint overrun"));
    }
    let mut boff = hdr_end;
    let mut vals = Vec::with_capacity(serials.len());
    for st in serials {
        let blen = serial_type_length(st);
        let Some(end) = boff.checked_add(blen) else {
            return Err(SqliteError::corrupt("index record body length overflow"));
        };
        if end > payload.len() {
            return Err(SqliteError::corrupt("index record body truncated"));
        }
        vals.push(decode_serial_value(
            &payload[boff..end],
            st,
            key_info.encoding,
        ));
        boff = end;
    }
    Ok(vals)
}

/// Compare two decoded index records using SQLite `KeyInfo` (collations & ASC/DESC),
/// honoring UTF-16 `BINARY` code-unit ordering on UTF-16 databases.
pub fn compare_index_values(a: &[SqlValue], b: &[SqlValue], key_info: &KeyInfo) -> Ordering {
    let a_len = a.len().min(key_info.n_all_field);
    let b_len = b.len().min(key_info.n_all_field);
    let n = a_len.min(b_len);
    let utf16_bin = if key_info.encoding != TextEncoding::Utf8 {
        Some(Collation::builtin(BuiltinCollation::Binary, key_info.encoding))
    } else {
        None
    };
    for i in 0..n {
        let coll = key_info
            .collations
            .get(i)
            .and_then(|c| c.as_ref())
            .or(utf16_bin.as_ref());
        let mut ord = mem_compare(&a[i], &b[i], coll);
        if ord != Ordering::Equal {
            if key_info.desc.get(i).copied().unwrap_or(false) {
                ord = ord.reverse();
            }
            return ord;
        }
    }
    a_len.cmp(&b_len)
}

/// Zero-copy streaming comparison of `key_vals` against an encoded SQLite record `payload`,
/// stopping at the first non-equal field without allocating a `Vec<SqlValue>`.
fn compare_key_to_record_bytes(
    key_vals: &[SqlValue],
    payload: &[u8],
    key_info: &KeyInfo,
    prefix_only: bool,
) -> SqliteResult<Ordering> {
    if payload.is_empty() {
        if prefix_only {
            return Err(SqliteError::corrupt("empty index record"));
        }
        return Ok(key_vals.len().min(key_info.n_all_field).cmp(&0));
    }
    let b0 = payload[0];
    let (hdr_len, mut hpos) = if b0 < 0x80 {
        (u32::from(b0), 1usize)
    } else {
        get_varint32(payload, 0)
    };
    let hdr_end = hdr_len as usize;
    if hdr_end == 0 {
        if prefix_only {
            return Err(SqliteError::corrupt("empty index record header"));
        }
        return Ok(key_vals.len().min(key_info.n_all_field).cmp(&0));
    }
    if hdr_end <= hpos || hdr_end > payload.len() {
        return Err(SqliteError::corrupt("index record header exceeds payload"));
    }
    let utf16_bin = if key_info.encoding != TextEncoding::Utf8 {
        Some(Collation::builtin(BuiltinCollation::Binary, key_info.encoding))
    } else {
        None
    };
    let a_len = if prefix_only {
        key_vals.len()
    } else {
        key_vals.len().min(key_info.n_all_field)
    };
    let mut boff = hdr_end;
    let mut i = 0usize;
    while i < a_len {
        if hpos >= hdr_end {
            if prefix_only {
                return Err(SqliteError::corrupt("truncated index record"));
            }
            return Ok(a_len.cmp(&i));
        }
        let hb = payload[hpos];
        let (st, n) = if hb < 0x80 {
            (u32::from(hb), 1usize)
        } else {
            get_varint32(payload, hpos)
        };
        hpos += n;
        if hpos > hdr_end {
            return Err(SqliteError::corrupt("index record header varint overrun"));
        }
        let blen = serial_type_length(st);
        let Some(end) = boff.checked_add(blen) else {
            return Err(SqliteError::corrupt("index record body length overflow"));
        };
        if end > payload.len() {
            return Err(SqliteError::corrupt("index record body truncated"));
        }
        let mut ord = match (&key_vals[i], st) {
            (SqlValue::Int(a), 8) => a.cmp(&0),
            (SqlValue::Int(a), 9) => a.cmp(&1),
            (SqlValue::Int(a), 1) => a.cmp(&i64::from(payload[boff] as i8)),
            (SqlValue::Int(a), 2) => {
                a.cmp(&i64::from(i16::from_be_bytes([payload[boff], payload[boff + 1]])))
            }
            (SqlValue::Int(a), 3) => {
                let s = &payload[boff..end];
                let v = ((i32::from(s[0] as i8)) << 16)
                    | (i32::from(s[1]) << 8)
                    | i32::from(s[2]);
                a.cmp(&i64::from(v))
            }
            (SqlValue::Int(a), 4) => a.cmp(&i64::from(i32::from_be_bytes([
                payload[boff],
                payload[boff + 1],
                payload[boff + 2],
                payload[boff + 3],
            ]))),
            (SqlValue::Int(a), 5) => {
                let s = &payload[boff..end];
                let v = ((i64::from(s[0] as i8)) << 40)
                    | (i64::from(s[1]) << 32)
                    | (i64::from(s[2]) << 24)
                    | (i64::from(s[3]) << 16)
                    | (i64::from(s[4]) << 8)
                    | i64::from(s[5]);
                a.cmp(&v)
            }
            (SqlValue::Int(a), 6) => a.cmp(&i64::from_be_bytes([
                payload[boff],
                payload[boff + 1],
                payload[boff + 2],
                payload[boff + 3],
                payload[boff + 4],
                payload[boff + 5],
                payload[boff + 6],
                payload[boff + 7],
            ])),
            (SqlValue::Text(a_bytes), s)
                if s >= 13 && (s & 1) == 1 && key_info.encoding == TextEncoding::Utf8 =>
            {
                let coll = key_info
                    .collations
                    .get(i)
                    .and_then(|c| c.as_ref());
                let b_bytes = &payload[boff..end];
                match coll {
                    Some(c) => c.compare_utf8(a_bytes, b_bytes),
                    None => a_bytes.as_ref().cmp(b_bytes),
                }
            }
            (SqlValue::Blob(a_bytes), s) if s >= 12 && (s & 1) == 0 => {
                a_bytes.as_ref().cmp(&payload[boff..end])
            }
            _ => {
                let coll = key_info
                    .collations
                    .get(i)
                    .and_then(|c| c.as_ref())
                    .or(utf16_bin.as_ref());
                let b_val = decode_serial_value(&payload[boff..end], st, key_info.encoding);
                mem_compare(&key_vals[i], &b_val, coll)
            }
        };
        if ord != Ordering::Equal {
            if key_info.desc.get(i).copied().unwrap_or(false) {
                ord = ord.reverse();
            }
            return Ok(ord);
        }
        boff = end;
        i += 1;
    }
    if prefix_only {
        return Ok(Ordering::Equal);
    }
    if a_len < key_info.n_all_field && hpos < hdr_end {
        Ok(Ordering::Less)
    } else {
        Ok(Ordering::Equal)
    }
}

/// Inspect a single index cell at `cell_idx` directly on `data` and compare `key_vals` against it.
#[allow(clippy::too_many_arguments)]
fn compare_key_to_page_index_cell(
    pager: &WritePager,
    data: &[u8],
    pgno: u32,
    is_leaf: bool,
    ptr_off: usize,
    cell_idx: usize,
    usable: u32,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
    prefix_only: bool,
) -> SqliteResult<Ordering> {
    let bound = (usable as usize).min(data.len());
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * cell_idx));
    let mut pos = pc;
    if !is_leaf {
        if pos + 4 > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index interior cell {cell_idx} runs past page end"
            )));
        }
        pos += 4;
    }
    let limits = pager.payload_limits();
    let (psize_u32, n1) = if pos < bound && data[pos] < 0x80 {
        (u32::from(data[pos]), 1usize)
    } else {
        get_payload_size_varint(data, pos)
    };
    pos += n1;
    let (local, has_ovfl) = if psize_u32 <= limits.max_local {
        (psize_u32 as usize, false)
    } else {
        (
            local_payload_size(psize_u32, limits.max_local, limits.min_local, usable),
            true,
        )
    };
    let cell_end = pos + local + if has_ovfl { 4 } else { 0 };
    if cell_end > bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: index cell {cell_idx} runs past usable page end"
        )));
    }
    if !has_ovfl {
        compare_key_to_record_bytes(key_vals, &data[pos..pos + local], key_info, prefix_only)
    } else {
        let ovfl = read_u32_be(data, pos + local);
        let full = read_full_payload(pager, &data[pos..pos + local], psize_u32 as usize, ovfl)?;
        compare_key_to_record_bytes(key_vals, &full, key_info, prefix_only)
    }
}

/// Decode a single index cell's full record fields from `data` at `cell_idx`.
#[allow(clippy::too_many_arguments)]
fn decode_page_index_cell_fields(
    pager: &WritePager,
    data: &[u8],
    pgno: u32,
    is_leaf: bool,
    ptr_off: usize,
    cell_idx: usize,
    usable: u32,
    key_info: &KeyInfo,
) -> SqliteResult<Vec<SqlValue>> {
    let bound = (usable as usize).min(data.len());
    let pc = usize::from(read_u16_be(data, ptr_off + 2 * cell_idx));
    let mut pos = pc + if is_leaf { 0 } else { 4 };
    let limits = pager.payload_limits();
    let (psize_u32, n1) = get_payload_size_varint(data, pos);
    pos += n1;
    let local = local_payload_size(psize_u32, limits.max_local, limits.min_local, usable);
    let has_ovfl = psize_u32 > local as u32;
    let cell_end = pos + local + if has_ovfl { 4 } else { 0 };
    if cell_end > bound {
        return Err(SqliteError::corrupt(format!(
            "page {pgno}: index cell {cell_idx} runs past usable page end"
        )));
    }
    if !has_ovfl {
        decode_record_fields(&data[pos..pos + local], key_info)
    } else {
        let ovfl = read_u32_be(data, pos + local);
        let full = read_full_payload(pager, &data[pos..pos + local], psize_u32 as usize, ovfl)?;
        decode_record_fields(&full, key_info)
    }
}

#[inline]
fn decode_int_st_fast(st: u8, payload: &[u8], off: usize) -> Option<(i64, usize)> {
    match st {
        8 => Some((0, 0)),
        9 => Some((1, 0)),
        1 if off < payload.len() => Some((i64::from(payload[off] as i8), 1)),
        2 if off + 2 <= payload.len() => Some((
            i64::from(i16::from_be_bytes([payload[off], payload[off + 1]])),
            2,
        )),
        3 if off + 3 <= payload.len() => {
            let v = ((i32::from(payload[off] as i8)) << 16)
                | (i32::from(payload[off + 1]) << 8)
                | i32::from(payload[off + 2]);
            Some((i64::from(v), 3))
        }
        4 if off + 4 <= payload.len() => Some((
            i64::from(i32::from_be_bytes([
                payload[off],
                payload[off + 1],
                payload[off + 2],
                payload[off + 3],
            ])),
            4,
        )),
        5 if off + 6 <= payload.len() => {
            let v = ((i64::from(payload[off] as i8)) << 40)
                | (i64::from(payload[off + 1]) << 32)
                | (i64::from(payload[off + 2]) << 24)
                | (i64::from(payload[off + 3]) << 16)
                | (i64::from(payload[off + 4]) << 8)
                | i64::from(payload[off + 5]);
            Some((v, 6))
        }
        6 if off + 8 <= payload.len() => Some((
            i64::from_be_bytes([
                payload[off],
                payload[off + 1],
                payload[off + 2],
                payload[off + 3],
                payload[off + 4],
                payload[off + 5],
                payload[off + 6],
                payload[off + 7],
            ]),
            8,
        )),
        _ => None,
    }
}

/// Specialized zero-allocation descent for 2-integer `ASC` indexes (e.g. `(col, rowid)`):
/// compares directly in registers when the on-page record has two integer serial types,
/// falling back seamlessly to `compare_key_to_page_index_cell` on mixed types or overflows.
#[allow(clippy::type_complexity)]
/// Fused 2-integer index lookup returning inline leaf fit / freeblock metadata from the single
/// leaf `read_page`, eliminating redundant page lookups in both `index_insert_two_int_asc` and
/// `index_delete_two_int_asc`.
enum TwoIntFindResult {
    FoundLeafInPlace {
        pgno: u32,
        hit_idx: usize,
        pc: usize,
        cell_len: usize,
        ovfl: u32,
        fb_pos: (usize, usize, usize),
    },
    FoundGeneral(u32, usize, BtreePath),
    NotFoundLeaf {
        pgno: u32,
        ins_idx: usize,
        fits_in_place: bool,
        path: BtreePath,
    },
}

fn index_find_two_int_ex(
    pager: &WritePager,
    root_pgno: u32,
    k0: i64,
    k1: i64,
    key_info: &KeyInfo,
    insert_alloc: usize,
    want_delete_info: bool,
) -> SqliteResult<TwoIntFindResult> {
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let mut pgno = root_pgno;
    let mut path = BtreePath::new();

    loop {
        if path.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "index_find: b-tree {root_pgno} exceeds max depth"
            )));
        }
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: truncated index header"
            )));
        }
        let is_leaf = match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::IndexLeaf) => true,
            Some(BtreePageType::IndexInterior) => false,
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "index_find: invalid page type 0x{:02x} on page {pgno}",
                    data[hdr]
                )));
            }
        };
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let ptr_off = hdr + if is_leaf { 8 } else { 12 };
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell pointer array runs past page end"
            )));
        }
        let cell_hdr_off = if is_leaf { 0usize } else { 4usize };

        let mut lo = 0usize;
        let mut hi = n_cell;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * mid));
            let pos = pc + cell_hdr_off;
            let mut fast_leaf_len = 0usize;
            let ord = if pos < bound && data[pos] < 0x80 {
                let psize = usize::from(data[pos]);
                let pstart = pos + 1;
                if psize >= 3
                    && psize <= limits.max_local as usize
                    && pstart + psize <= bound
                    && data[pstart] == 3
                {
                    let payload = &data[pstart..pstart + psize];
                    if let Some((r0, l0)) = decode_int_st_fast(payload[1], payload, 3) {
                        if k0 != r0 {
                            k0.cmp(&r0)
                        } else if let Some((r1, _)) =
                            decode_int_st_fast(payload[2], payload, 3 + l0)
                        {
                            fast_leaf_len = 1 + psize;
                            k1.cmp(&r1)
                        } else {
                            let kv = [SqlValue::Int(k0), SqlValue::Int(k1)];
                            compare_key_to_page_index_cell(
                                pager, data, pgno, is_leaf, ptr_off, mid, usable, &kv, key_info,
                                false,
                            )?
                        }
                    } else {
                        let kv = [SqlValue::Int(k0), SqlValue::Int(k1)];
                        compare_key_to_page_index_cell(
                            pager, data, pgno, is_leaf, ptr_off, mid, usable, &kv, key_info, false,
                        )?
                    }
                } else {
                    let kv = [SqlValue::Int(k0), SqlValue::Int(k1)];
                    compare_key_to_page_index_cell(
                        pager, data, pgno, is_leaf, ptr_off, mid, usable, &kv, key_info, false,
                    )?
                }
            } else {
                let kv = [SqlValue::Int(k0), SqlValue::Int(k1)];
                compare_key_to_page_index_cell(
                    pager, data, pgno, is_leaf, ptr_off, mid, usable, &kv, key_info, false,
                )?
            };
            match ord {
                Ordering::Equal => {
                    if want_delete_info && is_leaf && n_cell > 1 {
                        let (cell_len, ovfl) = if fast_leaf_len > 0 {
                            (fast_leaf_len, 0u32)
                        } else {
                            let (_, cl, ov) =
                                inspect_index_leaf_cell(data, pgno, usable, limits, mid)?;
                            (cl, ov)
                        };
                        if let Some(fb_pos) = find_freeblock_insert_pos(
                            data,
                            pgno,
                            usable as usize,
                            pc,
                            cell_len.max(4),
                        ) {
                            return Ok(TwoIntFindResult::FoundLeafInPlace {
                                pgno,
                                hit_idx: mid,
                                pc,
                                cell_len,
                                ovfl,
                                fb_pos,
                            });
                        }
                    }
                    return Ok(TwoIntFindResult::FoundGeneral(pgno, mid, path));
                }
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
        if is_leaf {
            let fits_in_place = if insert_alloc > 0 {
                let content_raw = read_u16_be(data, hdr + 5);
                let content_start = if content_raw == 0 {
                    65536usize
                } else {
                    usize::from(content_raw)
                };
                content_start <= usable as usize
                    && ptr_off + 2 * n_cell + 2 + insert_alloc <= content_start
            } else {
                false
            };
            return Ok(TwoIntFindResult::NotFoundLeaf {
                pgno,
                ins_idx: lo,
                fits_in_place,
                path,
            });
        }
        let child = if lo < n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * lo));
            if pc + 4 > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: index interior cell {lo} truncated"
                )));
            }
            read_u32_be(data, pc)
        } else {
            read_u32_be(data, hdr + 8)
        };
        path.push((pgno, lo));
        pgno = child;
    }
}

#[allow(clippy::type_complexity)]
fn index_find_two_int(
    pager: &WritePager,
    root_pgno: u32,
    k0: i64,
    k1: i64,
    key_info: &KeyInfo,
) -> SqliteResult<Result<(u32, usize, BtreePath), (u32, usize, BtreePath)>> {
    match index_find_two_int_ex(pager, root_pgno, k0, k1, key_info, 0, false)? {
        TwoIntFindResult::FoundGeneral(pgno, idx, path) => Ok(Ok((pgno, idx, path))),
        TwoIntFindResult::FoundLeafInPlace { pgno, hit_idx, .. } => {
            Ok(Ok((pgno, hit_idx, BtreePath::new())))
        }
        TwoIntFindResult::NotFoundLeaf {
            pgno,
            ins_idx,
            path,
            ..
        } => Ok(Err((pgno, ins_idx, path))),
    }
}

/// Search an index B-tree for `key_vals` (zero-copy binary search over page pointer arrays).
/// Returns:
/// - `Ok(Ok((pgno, cell_idx, path)))` if an exact match was found (may be leaf OR interior!)
/// - `Ok(Err((leaf_pgno, insert_idx, path)))` if absent (gives exact leaf insertion slot!)
#[allow(clippy::type_complexity)]
fn index_find(
    pager: &WritePager,
    root_pgno: u32,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<Result<(u32, usize, BtreePath), (u32, usize, BtreePath)>> {
    if key_vals.len() == 2 && key_info.is_two_int_asc() {
        if let (SqlValue::Int(k0), SqlValue::Int(k1)) = (&key_vals[0], &key_vals[1]) {
            return index_find_two_int(pager, root_pgno, *k0, *k1, key_info);
        }
    }
    let usable = pager.usable_size();
    let mut pgno = root_pgno;
    let mut path = BtreePath::new();

    loop {
        if path.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "index_find: b-tree {root_pgno} exceeds max depth"
            )));
        }
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: truncated index header"
            )));
        }
        let is_leaf = match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::IndexLeaf) => true,
            Some(BtreePageType::IndexInterior) => false,
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "index_find: invalid page type 0x{:02x} on page {pgno}",
                    data[hdr]
                )));
            }
        };
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let ptr_off = hdr + if is_leaf { 8 } else { 12 };
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell pointer array runs past page end"
            )));
        }

        // Zero-copy binary search over the 2-byte cell pointer array.
        let mut lo = 0usize;
        let mut hi = n_cell;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match compare_key_to_page_index_cell(
                pager, data, pgno, is_leaf, ptr_off, mid, usable, key_vals, key_info, false,
            )? {
                Ordering::Equal => return Ok(Ok((pgno, mid, path))),
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
        if is_leaf {
            return Ok(Err((pgno, lo, path)));
        }
        let child = if lo < n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * lo));
            if pc + 4 > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: index interior cell {lo} truncated"
                )));
            }
            read_u32_be(data, pc)
        } else {
            read_u32_be(data, hdr + 8)
        };
        path.push((pgno, lo));
        pgno = child;
    }
}

/// Check if an index B-tree already contains an entry matching the leading `prefix_vals`
/// (zero-copy binary search; decodes record only on match).
pub fn index_contains_prefix(
    pager: &WritePager,
    root_pgno: u32,
    prefix_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let mut pgno = root_pgno;
    let mut depth = 0usize;
    loop {
        if depth >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "index_contains_prefix: b-tree {root_pgno} exceeds max depth"
            )));
        }
        depth += 1;
        let data = pager.read_page(pgno)?;
        let bound = (usable as usize).min(data.len());
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: truncated index header"
            )));
        }
        let is_leaf = match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::IndexLeaf) => true,
            Some(BtreePageType::IndexInterior) => false,
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "index_contains_prefix: invalid page type on page {pgno}"
                )));
            }
        };
        let n_cell = usize::from(read_u16_be(data, hdr + 3));
        let ptr_off = hdr + if is_leaf { 8 } else { 12 };
        if ptr_off + 2 * n_cell > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell pointer array runs past page end"
            )));
        }
        let mut lo = 0usize;
        let mut hi = n_cell;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match compare_key_to_page_index_cell(
                pager, data, pgno, is_leaf, ptr_off, mid, usable, prefix_vals, key_info, true,
            )? {
                Ordering::Equal => {
                    let vals = decode_page_index_cell_fields(
                        pager, data, pgno, is_leaf, ptr_off, mid, usable, key_info,
                    )?;
                    return Ok(Some(vals));
                }
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
        if is_leaf {
            return Ok(None);
        }
        pgno = if lo < n_cell {
            let pc = usize::from(read_u16_be(data, ptr_off + 2 * lo));
            if pc + 4 > bound {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: index interior cell {lo} truncated"
                )));
            }
            read_u32_be(data, pc)
        } else {
            read_u32_be(data, hdr + 8)
        };
    }
}

/// Insert (or replace if `replace == true`) a full index record `payload` into an index B-tree,
/// decoding `key_vals` from `payload`.
pub fn index_insert(
    pager: &mut WritePager,
    root_pgno: u32,
    payload: &[u8],
    key_info: &KeyInfo,
    replace: bool,
) -> SqliteResult<bool> {
    let key_vals = decode_record_fields(payload, key_info)?;
    index_insert_with_values(pager, root_pgno, &key_vals, payload, key_info, replace)
}

/// Fused insert for a 2-integer `ASC` index entry `(k0, k1)` (such as `(col, rowid)` on a
/// single-column secondary index): encodes directly into a 24-byte stack cell, runs
/// register-only `index_find_two_int`, and inserts in-place without zeroing a 512-byte stack
/// buffer or allocating a `[SqlValue; 2]` array.
pub fn index_insert_two_int_asc(
    pager: &mut WritePager,
    root_pgno: u32,
    k0: i64,
    k1: i64,
    schema_format: u32,
    key_info: &KeyInfo,
    replace: bool,
) -> SqliteResult<bool> {
    debug_assert!(key_info.is_two_int_asc());
    let mut stack_cell = [0u8; 24];
    let rlen = encode_two_int_record_stack(
        k0,
        k1,
        schema_format,
        (&mut stack_cell[1..21]).try_into().unwrap(),
    );
    stack_cell[0] = rlen as u8;
    let clen = 1 + rlen;
    let usable = pager.usable_size();
    let alloc = clen.max(4);

    match index_find_two_int_ex(pager, root_pgno, k0, k1, key_info, alloc, false)? {
        TwoIntFindResult::FoundGeneral(hit_pgno, hit_idx, path) => {
            if !replace {
                return Err(SqliteError::Sql("UNIQUE constraint failed".to_string()));
            }
            let data = pager.read_page(hit_pgno)?.to_vec();
            let hdr = if hit_pgno == 1 { HEADER_SIZE } else { 0 };
            let is_leaf = data[hdr] == BtreePageType::IndexLeaf as u8;
            let (cells, right_child) = parse_index_cells(&data, hit_pgno, usable)?;
            if cells[hit_idx].overflow_pgno != 0 {
                free_overflow_chain(pager, cells[hit_idx].overflow_pgno)?;
            }
            let left_child = cells[hit_idx].left_child;
            let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            raw_cells[hit_idx] = if is_leaf {
                stack_cell[..clen].to_vec()
            } else {
                let mut v = Vec::with_capacity(4 + clen);
                v.extend_from_slice(&left_child.to_be_bytes());
                v.extend_from_slice(&stack_cell[..clen]);
                v
            };
            if cells_fit(hit_pgno, is_leaf, usable as usize, &raw_cells) {
                let ptype = if is_leaf {
                    BtreePageType::IndexLeaf
                } else {
                    BtreePageType::IndexInterior
                };
                pack_btree_page(pager, hit_pgno, ptype, right_child, &raw_cells)?;
            } else if is_leaf {
                index_split_leaf(pager, root_pgno, hit_pgno, &path, raw_cells)?;
            } else {
                index_split_interior(pager, root_pgno, hit_pgno, &path, raw_cells, right_child)?;
            }
            Ok(true)
        }
        TwoIntFindResult::FoundLeafInPlace { .. } => unreachable!(),
        TwoIntFindResult::NotFoundLeaf {
            pgno: leaf_pgno,
            ins_idx,
            fits_in_place,
            path,
        } => {
            if fits_in_place {
                insert_leaf_cell_in_place_unchecked(pager, leaf_pgno, ins_idx, &stack_cell[..clen])?;
                return Ok(false);
            }
            let (cells, _) = {
                let data = pager.read_page(leaf_pgno)?;
                parse_index_cells(data, leaf_pgno, usable)?
            };
            let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            raw_cells.insert(ins_idx, stack_cell[..clen].to_vec());
            if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                pack_btree_page(pager, leaf_pgno, BtreePageType::IndexLeaf, 0, &raw_cells)?;
                return Ok(false);
            }
            index_split_leaf(pager, root_pgno, leaf_pgno, &path, raw_cells)?;
            Ok(false)
        }
    }
}

/// Insert (or replace if `replace == true`) an index record with pre-known `key_vals` into an
/// index B-tree, using an in-place leaf fast path when space permits.
pub fn index_insert_with_values(
    pager: &mut WritePager,
    root_pgno: u32,
    key_vals: &[SqlValue],
    payload: &[u8],
    key_info: &KeyInfo,
    replace: bool,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();

    match index_find(pager, root_pgno, key_vals, key_info)? {
        Ok((hit_pgno, hit_idx, path)) => {
            if !replace {
                return Err(SqliteError::Sql("UNIQUE constraint failed".to_string()));
            }
            let data = pager.read_page(hit_pgno)?.to_vec();
            let hdr = if hit_pgno == 1 { HEADER_SIZE } else { 0 };
            let is_leaf = data[hdr] == BtreePageType::IndexLeaf as u8;
            let (cells, right_child) = parse_index_cells(&data, hit_pgno, usable)?;
            let new_body = build_index_cell_body(pager, payload)?;
            if cells[hit_idx].overflow_pgno != 0 {
                free_overflow_chain(pager, cells[hit_idx].overflow_pgno)?;
            }
            let left_child = cells[hit_idx].left_child;
            let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            raw_cells[hit_idx] = if is_leaf {
                new_body
            } else {
                let mut v = Vec::with_capacity(4 + new_body.len());
                v.extend_from_slice(&left_child.to_be_bytes());
                v.extend_from_slice(&new_body);
                v
            };
            if cells_fit(hit_pgno, is_leaf, usable as usize, &raw_cells) {
                let ptype = if is_leaf {
                    BtreePageType::IndexLeaf
                } else {
                    BtreePageType::IndexInterior
                };
                pack_btree_page(pager, hit_pgno, ptype, right_child, &raw_cells)?;
            } else if is_leaf {
                index_split_leaf(pager, root_pgno, hit_pgno, &path, raw_cells)?;
            } else {
                index_split_interior(pager, root_pgno, hit_pgno, &path, raw_cells, right_child)?;
            }
            Ok(true)
        }
        Err((leaf_pgno, ins_idx, path)) => {
            let limits = pager.payload_limits();
            if payload.len() <= 480 && payload.len() <= limits.max_local as usize {
                let mut stack_cell = [0u8; 512];
                let n1 = put_varint_slice(payload.len() as u64, &mut stack_cell);
                let clen = n1 + payload.len();
                stack_cell[n1..clen].copy_from_slice(payload);
                if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, &stack_cell[..clen])? {
                    return Ok(false);
                }
                let (cells, _) = {
                    let data = pager.read_page(leaf_pgno)?;
                    parse_index_cells(data, leaf_pgno, usable)?
                };
                let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
                raw_cells.insert(ins_idx, stack_cell[..clen].to_vec());

                if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                    pack_btree_page(pager, leaf_pgno, BtreePageType::IndexLeaf, 0, &raw_cells)?;
                    return Ok(false);
                }
                index_split_leaf(pager, root_pgno, leaf_pgno, &path, raw_cells)?;
                return Ok(false);
            }
            let new_body = build_index_cell_body(pager, payload)?;
            if try_insert_leaf_cell_in_place(pager, leaf_pgno, ins_idx, &new_body)? {
                return Ok(false);
            }
            let (cells, _) = {
                let data = pager.read_page(leaf_pgno)?;
                parse_index_cells(data, leaf_pgno, usable)?
            };
            let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            raw_cells.insert(ins_idx, new_body);

            if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
                pack_btree_page(pager, leaf_pgno, BtreePageType::IndexLeaf, 0, &raw_cells)?;
                return Ok(false);
            }
            index_split_leaf(pager, root_pgno, leaf_pgno, &path, raw_cells)?;
            Ok(false)
        }
    }
}

fn index_split_leaf(
    pager: &mut WritePager,
    root_pgno: u32,
    leaf_pgno: u32,
    path: &[(u32, usize)],
    raw_cells: Vec<Vec<u8>>,
) -> SqliteResult<()> {
    // Index leaf split promotes middle cell `mid` into parent interior page!
    // Must have >= 3 cells to have non-empty left & right + promoted mid; with 2 cells,
    // left gets 1, mid gets 1, right gets 0 —Wait: SQLite forbids 0-cell non-root leaf!
    // With SQLite's max_local <= ~25% of usable_size, a page overflowing ALWAYS has >= 3 cells!
    let mid = choose_index_split_mid(&raw_cells);
    let left_raw = &raw_cells[..mid];
    let mid_body = raw_cells[mid].clone();
    let right_raw = &raw_cells[mid + 1..];

    if path.is_empty() {
        let left_pg = pager.allocate_page()?;
        let right_pg = pager.allocate_page()?;
        pack_btree_page(pager, left_pg, BtreePageType::IndexLeaf, 0, left_raw)?;
        pack_btree_page(pager, right_pg, BtreePageType::IndexLeaf, 0, right_raw)?;
        let mut div = Vec::with_capacity(4 + mid_body.len());
        div.extend_from_slice(&left_pg.to_be_bytes());
        div.extend_from_slice(&mid_body);
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::IndexInterior,
            right_pg,
            &[div],
        )
    } else {
        let right_pg = pager.allocate_page()?;
        pack_btree_page(pager, leaf_pgno, BtreePageType::IndexLeaf, 0, left_raw)?;
        pack_btree_page(pager, right_pg, BtreePageType::IndexLeaf, 0, right_raw)?;
        index_insert_into_parent(pager, root_pgno, path, leaf_pgno, mid_body, right_pg)
    }
}

fn index_insert_into_parent(
    pager: &mut WritePager,
    root_pgno: u32,
    path: &[(u32, usize)],
    left_pg: u32,
    sep_body: Vec<u8>,
    right_pg: u32,
) -> SqliteResult<()> {
    let (parent_pg, child_slot) = *path.last().expect("path non-empty");
    let usable = pager.usable_size();
    let (cells, mut right_child) = {
        let data = pager.read_page(parent_pg)?;
        parse_index_cells(data, parent_pg, usable)?
    };
    let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();

    let mut new_int_cell = Vec::with_capacity(4 + sep_body.len());
    new_int_cell.extend_from_slice(&left_pg.to_be_bytes());
    new_int_cell.extend_from_slice(&sep_body);

    if child_slot < raw_cells.len() {
        raw_cells[child_slot][0..4].copy_from_slice(&right_pg.to_be_bytes());
        raw_cells.insert(child_slot, new_int_cell);
    } else {
        raw_cells.push(new_int_cell);
        right_child = right_pg;
    }

    if cells_fit(parent_pg, false, usable as usize, &raw_cells) {
        pack_btree_page(
            pager,
            parent_pg,
            BtreePageType::IndexInterior,
            right_child,
            &raw_cells,
        )?;
        return Ok(());
    }

    index_split_interior(
        pager,
        root_pgno,
        parent_pg,
        &path[..path.len() - 1],
        raw_cells,
        right_child,
    )
}

fn index_split_interior(
    pager: &mut WritePager,
    root_pgno: u32,
    pgno: u32,
    parent_path: &[(u32, usize)],
    raw_cells: Vec<Vec<u8>>,
    right_child: u32,
) -> SqliteResult<()> {
    let mid = choose_index_split_mid(&raw_cells);
    let left_raw = &raw_cells[..mid];
    let left_right_child = read_u32_be(&raw_cells[mid], 0);
    let promoted_body = raw_cells[mid][4..].to_vec();
    let right_raw = &raw_cells[mid + 1..];

    if parent_path.is_empty() {
        let new_left = pager.allocate_page()?;
        let new_right = pager.allocate_page()?;
        pack_btree_page(
            pager,
            new_left,
            BtreePageType::IndexInterior,
            left_right_child,
            left_raw,
        )?;
        pack_btree_page(
            pager,
            new_right,
            BtreePageType::IndexInterior,
            right_child,
            right_raw,
        )?;
        let mut root_cell = Vec::with_capacity(4 + promoted_body.len());
        root_cell.extend_from_slice(&new_left.to_be_bytes());
        root_cell.extend_from_slice(&promoted_body);
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::IndexInterior,
            new_right,
            &[root_cell],
        )
    } else {
        let new_right = pager.allocate_page()?;
        pack_btree_page(
            pager,
            pgno,
            BtreePageType::IndexInterior,
            left_right_child,
            left_raw,
        )?;
        pack_btree_page(
            pager,
            new_right,
            BtreePageType::IndexInterior,
            right_child,
            right_raw,
        )?;
        index_insert_into_parent(
            pager,
            root_pgno,
            parent_path,
            pgno,
            promoted_body,
            new_right,
        )
    }
}

/// Append an index leaf cell body (payload_size varint + local payload + optional 4-byte overflow
/// pgno) straight into a flat `out` buffer, returning its `(start, end)` byte range.
pub fn append_index_cell_body(
    pager: &mut WritePager,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> SqliteResult<(usize, usize)> {
    if payload.len() > i32::MAX as usize {
        return Err(SqliteError::Sql("string or blob too big".to_string()));
    }
    let usable = pager.usable_size();
    let limits = payload_limits(usable);
    let psize = payload.len() as u32;
    let local = local_payload_size(psize, limits.max_local, limits.min_local, usable);
    let ovfl = if payload.len() > local {
        write_overflow_chain(pager, &payload[local..])?
    } else {
        0
    };
    let start = out.len();
    put_varint(u64::from(psize), out);
    out.extend_from_slice(&payload[..local]);
    if ovfl != 0 {
        out.extend_from_slice(&ovfl.to_be_bytes());
    }
    Ok((start, out.len()))
}

fn pack_btree_page_flat(
    pager: &mut WritePager,
    pgno: u32,
    ptype: BtreePageType,
    right_child: u32,
    buf: &[u8],
    ranges: &[(usize, usize)],
) -> SqliteResult<()> {
    let usable = pager.usable_size() as usize;
    let page = pager.write_page_mut(pgno)?;
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let is_leaf = ptype.is_leaf();
    let hdr_len = if is_leaf { 8 } else { 12 };
    let n = ranges.len();

    page[hdr..usable].fill(0);
    page[hdr] = ptype as u8;
    page[hdr + 1..hdr + 3].copy_from_slice(&0u16.to_be_bytes());
    page[hdr + 3..hdr + 5].copy_from_slice(&(n as u16).to_be_bytes());
    page[hdr + 7] = 0;
    if !is_leaf {
        page[hdr + 8..hdr + 12].copy_from_slice(&right_child.to_be_bytes());
    }

    let ptr_off = hdr + hdr_len;
    let mut content = usable;
    for (i, &(s, e)) in ranges.iter().enumerate() {
        let cell = &buf[s..e];
        let alloc_len = cell.len().max(4);
        if content < ptr_off + 2 * n + alloc_len {
            return Err(SqliteError::corrupt(format!(
                "pack_btree_page_flat overflow on page {pgno}"
            )));
        }
        content -= alloc_len;
        page[content..content + cell.len()].copy_from_slice(cell);
        page[ptr_off + 2 * i..ptr_off + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    let content_field = if content == 65536 { 0 } else { content as u16 };
    page[hdr + 5..hdr + 7].copy_from_slice(&content_field.to_be_bytes());
    Ok(())
}

/// Bulk-load an empty index B-tree at `root_pgno` from payloads ALREADY sorted in exact
/// ascending index key order (bottom-up O(N) build with zero binary searches or splits!).
pub fn index_bulk_load_sorted(
    pager: &mut WritePager,
    root_pgno: u32,
    sorted_payloads: &[Vec<u8>],
) -> SqliteResult<()> {
    if sorted_payloads.is_empty() {
        return Ok(());
    }
    let mut cell_buf = Vec::with_capacity(sorted_payloads.len().saturating_mul(16));
    let mut cell_ranges = Vec::with_capacity(sorted_payloads.len());
    for payload in sorted_payloads {
        let range = append_index_cell_body(pager, payload, &mut cell_buf)?;
        cell_ranges.push(range);
    }
    index_bulk_load_sorted_flat(pager, root_pgno, &cell_buf, &cell_ranges)
}

/// Bulk-load already-sorted, pre-encoded index leaf cell bodies stored in a single flat buffer
/// `cell_buf` with `(start, end)` slices in `cell_ranges` (zero per-cell `Vec<u8>` allocations!).
pub fn index_bulk_load_sorted_flat(
    pager: &mut WritePager,
    root_pgno: u32,
    cell_buf: &[u8],
    cell_ranges: &[(usize, usize)],
) -> SqliteResult<()> {
    if cell_ranges.is_empty() {
        return Ok(());
    }
    let usable = pager.usable_size() as usize;
    let hdr_root = if root_pgno == 1 { HEADER_SIZE } else { 0 } + 8;
    let total_root_bytes: usize = cell_ranges
        .iter()
        .map(|&(s, e)| 2 + (e - s).max(4))
        .sum();
    if hdr_root + total_root_bytes <= usable {
        return pack_btree_page_flat(
            pager,
            root_pgno,
            BtreePageType::IndexLeaf,
            0,
            cell_buf,
            cell_ranges,
        );
    }

    // Partition sorted leaf cells into greedy leaf chunks + promoted separator cells.
    let hdr_leaf = 8usize;
    let mut leaf_ranges: Vec<(usize, usize)> = Vec::new();
    let mut div_indices: Vec<usize> = Vec::new();
    let mut cur_start = 0usize;
    let mut cur_bytes = hdr_leaf;
    let total = cell_ranges.len();

    let mut i = 0usize;
    while i < total {
        let (cs, ce) = cell_ranges[i];
        let cell_cost = 2 + (ce - cs).max(4);
        if i > cur_start && cur_bytes + cell_cost > usable {
            if i + 1 < total {
                leaf_ranges.push((cur_start, i));
                div_indices.push(i);
                cur_start = i + 1;
                cur_bytes = hdr_leaf;
                i += 1;
                continue;
            } else {
                // Choose a byte-balanced divider `mid` in `(cur_start + 1)..=(total - 2)` so
                // both `(cur_start, mid)` and `(mid + 1, total)` fit strictly within `usable`
                // even when small keys precede large keys.
                let total_chunk_bytes = cur_bytes - hdr_leaf + cell_cost;
                let half = total_chunk_bytes / 2;
                let mut left_body = 0usize;
                let mut mid = total - 2;
                for (j, &(s, e)) in cell_ranges.iter().enumerate().take(total - 1).skip(cur_start) {
                    let cj = 2 + (e - s).max(4);
                    if j > cur_start {
                        let right_body = total_chunk_bytes - left_body - cj;
                        if hdr_leaf + left_body <= usable
                            && hdr_leaf + right_body <= usable
                            && left_body >= half
                        {
                            mid = j;
                            break;
                        }
                    }
                    left_body += cj;
                }
                leaf_ranges.push((cur_start, mid));
                div_indices.push(mid);
                leaf_ranges.push((mid + 1, total));
                cur_start = total;
                break;
            }
        }
        cur_bytes += cell_cost;
        i += 1;
    }
    if cur_start < total {
        leaf_ranges.push((cur_start, total));
    }
    debug_assert_eq!(leaf_ranges.len(), div_indices.len() + 1);

    let mut child_pgs: Vec<u32> = Vec::with_capacity(leaf_ranges.len());
    for (s, e) in leaf_ranges {
        let pg = pager.allocate_page()?;
        pack_btree_page_flat(
            pager,
            pg,
            BtreePageType::IndexLeaf,
            0,
            cell_buf,
            &cell_ranges[s..e],
        )?;
        child_pgs.push(pg);
    }
    let mut dividers: Vec<Vec<u8>> = div_indices
        .into_iter()
        .map(|idx| {
            let (s, e) = cell_ranges[idx];
            cell_buf[s..e].to_vec()
        })
        .collect();

    // Build interior levels bottom-up until all remaining cells fit on `root_pgno`.
    loop {
        let m = dividers.len();
        debug_assert_eq!(child_pgs.len(), m + 1);
        let mut int_cells: Vec<Vec<u8>> = Vec::with_capacity(m);
        for k in 0..m {
            let mut c = Vec::with_capacity(4 + dividers[k].len());
            c.extend_from_slice(&child_pgs[k].to_be_bytes());
            c.extend_from_slice(&dividers[k]);
            int_cells.push(c);
        }
        let last_child = child_pgs[m];
        if cells_fit(root_pgno, false, usable, &int_cells) {
            return pack_btree_page(
                pager,
                root_pgno,
                BtreePageType::IndexInterior,
                last_child,
                &int_cells,
            );
        }

        // Partition interior cells into non-empty `IndexInterior` pages + next-level dividers.
        let hdr_int = 12usize;
        let mut page_ranges: Vec<(usize, usize)> = Vec::new();
        let mut next_div_indices: Vec<usize> = Vec::new();
        let mut start = 0usize;
        let mut bytes = hdr_int;
        let mut k = 0usize;
        while k < m {
            let cost = 2 + cell_storage_len(&int_cells[k]);
            if k > start && bytes + cost > usable {
                if k + 1 < m {
                    page_ranges.push((start, k));
                    next_div_indices.push(k);
                    start = k + 1;
                    bytes = hdr_int;
                    k += 1;
                    continue;
                } else {
                    let total_chunk_bytes = bytes - hdr_int + cost;
                    let half = total_chunk_bytes / 2;
                    let mut left_body = 0usize;
                    let mut mid = m - 2;
                    for (j, cell) in int_cells.iter().enumerate().take(m - 1).skip(start) {
                        let cj = 2 + cell_storage_len(cell);
                        if j > start {
                            let right_body = total_chunk_bytes - left_body - cj;
                            if hdr_int + left_body <= usable
                                && hdr_int + right_body <= usable
                                && left_body >= half
                            {
                                mid = j;
                                break;
                            }
                        }
                        left_body += cj;
                    }
                    page_ranges.push((start, mid));
                    next_div_indices.push(mid);
                    page_ranges.push((mid + 1, m));
                    start = m;
                    break;
                }
            }
            bytes += cost;
            k += 1;
        }
        if start < m {
            page_ranges.push((start, m));
        }

        let mut next_children: Vec<u32> = Vec::with_capacity(page_ranges.len());
        for (s, e) in page_ranges {
            let pg = pager.allocate_page()?;
            let rc = child_pgs[e];
            pack_btree_page(
                pager,
                pg,
                BtreePageType::IndexInterior,
                rc,
                &int_cells[s..e],
            )?;
            next_children.push(pg);
        }
        let next_dividers: Vec<Vec<u8>> = next_div_indices
            .into_iter()
            .map(|idx| std::mem::take(&mut dividers[idx]))
            .collect();
        child_pgs = next_children;
        dividers = next_dividers;
    }
}

#[inline]
fn int_serial_len(v: i64, schema_format: u32) -> usize {
    if schema_format >= 4 && (v == 0 || v == 1) {
        0
    } else if (-128..=127).contains(&v) {
        1
    } else if (-32768..=32767).contains(&v) {
        2
    } else if (-8_388_608..=8_388_607).contains(&v) {
        3
    } else if (-2_147_483_648..=2_147_483_647).contains(&v) {
        4
    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&v) {
        6
    } else {
        8
    }
}

#[inline]
fn two_int_cell_storage_len(k0: i64, k1: i64, schema_format: u32) -> usize {
    // 1-byte payload-size varint + 3-byte record header + int bodies (min 4 bytes -> always >= 4).
    4 + int_serial_len(k0, schema_format) + int_serial_len(k1, schema_format)
}

fn pack_two_int_index_leaf(
    pager: &mut WritePager,
    pgno: u32,
    entries: &[(i64, i64)],
    schema_format: u32,
) -> SqliteResult<()> {
    let usable = pager.usable_size();
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    let ptr_off = hdr + 8;
    let page = pager.write_page_mut(pgno)?;
    if pgno == 1 {
        page[HEADER_SIZE..].fill(0);
    } else {
        page.fill(0);
    }
    page[hdr] = BtreePageType::IndexLeaf as u8;
    page[hdr + 1..hdr + 3].copy_from_slice(&0u16.to_be_bytes());
    page[hdr + 3..hdr + 5].copy_from_slice(&(entries.len() as u16).to_be_bytes());
    page[hdr + 7] = 0;

    let mut content = usable as usize;
    let mut rec_buf = [0u8; 20];
    for (i, &(k0, k1)) in entries.iter().enumerate() {
        let rlen =
            crate::write::record::encode_two_int_record_stack(k0, k1, schema_format, &mut rec_buf);
        let clen = 1 + rlen; // always >= 4
        if content < ptr_off + 2 * entries.len() + clen {
            return Err(SqliteError::corrupt(format!(
                "pack_two_int_index_leaf: cells overflow page {pgno}"
            )));
        }
        content -= clen;
        page[content] = rlen as u8;
        page[content + 1..content + clen].copy_from_slice(&rec_buf[..rlen]);
        page[ptr_off + 2 * i..ptr_off + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    let content_field = if content == 65536 { 0 } else { content as u16 };
    page[hdr + 5..hdr + 7].copy_from_slice(&content_field.to_be_bytes());
    Ok(())
}

/// Zero-intermediate bottom-up bulk loader for sorted 2-integer `(key, rowid)` index entries:
/// streams directly into packed `IndexLeaf` pages without allocating flat `cell_buf`/`cell_ranges`.
pub fn index_bulk_load_sorted_two_int(
    pager: &mut WritePager,
    root_pgno: u32,
    sorted_entries: &[(i64, i64)],
    schema_format: u32,
) -> SqliteResult<()> {
    if sorted_entries.is_empty() {
        return Ok(());
    }
    let usable = pager.usable_size() as usize;
    let hdr_root = if root_pgno == 1 { HEADER_SIZE } else { 0 } + 8;
    let total_root_bytes: usize = sorted_entries
        .iter()
        .map(|&(k0, k1)| 2 + two_int_cell_storage_len(k0, k1, schema_format))
        .sum();
    if hdr_root + total_root_bytes <= usable {
        return pack_two_int_index_leaf(pager, root_pgno, sorted_entries, schema_format);
    }

    let hdr_leaf = 8usize;
    let mut leaf_ranges: Vec<(usize, usize)> = Vec::new();
    let mut div_indices: Vec<usize> = Vec::new();
    let mut cur_start = 0usize;
    let mut cur_bytes = hdr_leaf;
    let total = sorted_entries.len();

    let mut i = 0usize;
    while i < total {
        let (k0, k1) = sorted_entries[i];
        let cell_cost = 2 + two_int_cell_storage_len(k0, k1, schema_format);
        if i > cur_start && cur_bytes + cell_cost > usable {
            if i + 1 < total {
                leaf_ranges.push((cur_start, i));
                div_indices.push(i);
                cur_start = i + 1;
                cur_bytes = hdr_leaf;
                i += 1;
                continue;
            } else {
                let total_chunk_bytes = cur_bytes - hdr_leaf + cell_cost;
                let half = total_chunk_bytes / 2;
                let mut left_body = 0usize;
                let mut mid = total - 2;
                for (j, &(ek0, ek1)) in sorted_entries
                    .iter()
                    .enumerate()
                    .take(total - 1)
                    .skip(cur_start)
                {
                    let cj = 2 + two_int_cell_storage_len(ek0, ek1, schema_format);
                    if j > cur_start {
                        let right_body = total_chunk_bytes - left_body - cj;
                        if hdr_leaf + left_body <= usable
                            && hdr_leaf + right_body <= usable
                            && left_body >= half
                        {
                            mid = j;
                            break;
                        }
                    }
                    left_body += cj;
                }
                leaf_ranges.push((cur_start, mid));
                div_indices.push(mid);
                leaf_ranges.push((mid + 1, total));
                cur_start = total;
                break;
            }
        }
        cur_bytes += cell_cost;
        i += 1;
    }
    if cur_start < total {
        leaf_ranges.push((cur_start, total));
    }

    let mut child_pgs: Vec<u32> = Vec::with_capacity(leaf_ranges.len());
    for (s, e) in leaf_ranges {
        let pg = pager.allocate_page()?;
        pack_two_int_index_leaf(pager, pg, &sorted_entries[s..e], schema_format)?;
        child_pgs.push(pg);
    }
    let mut rec_buf = [0u8; 20];
    let mut dividers: Vec<Vec<u8>> = div_indices
        .into_iter()
        .map(|idx| {
            let (k0, k1) = sorted_entries[idx];
            let rlen = crate::write::record::encode_two_int_record_stack(
                k0,
                k1,
                schema_format,
                &mut rec_buf,
            );
            let mut v = Vec::with_capacity(1 + rlen);
            v.push(rlen as u8);
            v.extend_from_slice(&rec_buf[..rlen]);
            v
        })
        .collect();

    loop {
        let m = dividers.len();
        debug_assert_eq!(child_pgs.len(), m + 1);
        let mut int_cells: Vec<Vec<u8>> = Vec::with_capacity(m);
        for k in 0..m {
            let mut c = Vec::with_capacity(4 + dividers[k].len());
            c.extend_from_slice(&child_pgs[k].to_be_bytes());
            c.extend_from_slice(&dividers[k]);
            int_cells.push(c);
        }
        let last_child = child_pgs[m];
        if cells_fit(root_pgno, false, usable, &int_cells) {
            return pack_btree_page(
                pager,
                root_pgno,
                BtreePageType::IndexInterior,
                last_child,
                &int_cells,
            );
        }

        let hdr_int = 12usize;
        let mut page_ranges: Vec<(usize, usize)> = Vec::new();
        let mut next_div_indices: Vec<usize> = Vec::new();
        let mut start = 0usize;
        let mut bytes = hdr_int;
        let mut k = 0usize;
        while k < m {
            let cost = 2 + cell_storage_len(&int_cells[k]);
            if k > start && bytes + cost > usable {
                if k + 1 < m {
                    page_ranges.push((start, k));
                    next_div_indices.push(k);
                    start = k + 1;
                    bytes = hdr_int;
                    k += 1;
                    continue;
                } else {
                    let total_chunk_bytes = bytes - hdr_int + cost;
                    let half = total_chunk_bytes / 2;
                    let mut left_body = 0usize;
                    let mut mid = m - 2;
                    for (j, cell) in int_cells.iter().enumerate().take(m - 1).skip(start) {
                        let cj = 2 + cell_storage_len(cell);
                        if j > start {
                            let right_body = total_chunk_bytes - left_body - cj;
                            if hdr_int + left_body <= usable
                                && hdr_int + right_body <= usable
                                && left_body >= half
                            {
                                mid = j;
                                break;
                            }
                        }
                        left_body += cj;
                    }
                    page_ranges.push((start, mid));
                    next_div_indices.push(mid);
                    page_ranges.push((mid + 1, m));
                    start = m;
                    break;
                }
            }
            bytes += cost;
            k += 1;
        }
        if start < m {
            page_ranges.push((start, m));
        }

        let mut next_children: Vec<u32> = Vec::with_capacity(page_ranges.len());
        for (s, e) in page_ranges {
            let pg = pager.allocate_page()?;
            let rc = child_pgs[e];
            pack_btree_page(
                pager,
                pg,
                BtreePageType::IndexInterior,
                rc,
                &int_cells[s..e],
            )?;
            next_children.push(pg);
        }
        let next_dividers: Vec<Vec<u8>> = next_div_indices
            .into_iter()
            .map(|idx| std::mem::take(&mut dividers[idx]))
            .collect();
        child_pgs = next_children;
        dividers = next_dividers;
    }
}

/// Fused delete for a 2-integer `ASC` index entry `(k0, k1)` (such as `(col, rowid)`):
/// uses register-only `index_find_two_int_ex` to fuse search, leaf inspection, and freeblock
/// lookup into a single `read_page` before mutating in place (0 extra page lookups, 0 `SqlValue`).
pub fn index_delete_two_int_asc(
    pager: &mut WritePager,
    root_pgno: u32,
    k0: i64,
    k1: i64,
    key_info: &KeyInfo,
) -> SqliteResult<bool> {
    debug_assert!(key_info.is_two_int_asc());
    match index_find_two_int_ex(pager, root_pgno, k0, k1, key_info, 0, true)? {
        TwoIntFindResult::FoundLeafInPlace {
            pgno,
            hit_idx,
            pc,
            cell_len,
            ovfl,
            fb_pos: (prev_fb, prev_sz, next_fb),
        } => {
            if ovfl != 0 {
                free_overflow_chain(pager, ovfl)?;
            }
            delete_leaf_cell_in_place_at(
                pager, pgno, hit_idx, pc, cell_len, prev_fb, prev_sz, next_fb,
            )?;
            Ok(true)
        }
        TwoIntFindResult::FoundGeneral(hit_pgno, hit_idx, path) => {
            let kvals = [SqlValue::Int(k0), SqlValue::Int(k1)];
            index_delete_at(pager, root_pgno, hit_pgno, hit_idx, path, &kvals, key_info)
        }
        TwoIntFindResult::NotFoundLeaf { .. } => Ok(false),
    }
}

/// Delete an exact entry `key_vals` from an index B-tree. Returns `Ok(true)` if deleted.
pub fn index_delete(
    pager: &mut WritePager,
    root_pgno: u32,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<bool> {
    let Ok((hit_pgno, hit_idx, path)) = index_find(pager, root_pgno, key_vals, key_info)? else {
        return Ok(false);
    };
    index_delete_at(pager, root_pgno, hit_pgno, hit_idx, path, key_vals, key_info)
}

fn index_delete_at(
    pager: &mut WritePager,
    root_pgno: u32,
    hit_pgno: u32,
    hit_idx: usize,
    path: BtreePath,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let hdr = if hit_pgno == 1 { HEADER_SIZE } else { 0 };
    let leaf_info = {
        let data = pager.read_page(hit_pgno)?;
        if data[hdr] == BtreePageType::IndexLeaf as u8 {
            let n_cell = usize::from(read_u16_be(data, hdr + 3));
            let (pc, cell_len, ovfl) =
                inspect_index_leaf_cell(data, hit_pgno, usable, pager.payload_limits(), hit_idx)?;
            let fb_pos = if n_cell > 1 {
                find_freeblock_insert_pos(data, hit_pgno, usable as usize, pc, cell_len.max(4))
            } else {
                None
            };
            Some((n_cell, pc, cell_len, ovfl, fb_pos))
        } else {
            None
        }
    };

    if let Some((n_cell, pc, cell_len, ovfl, fb_pos)) = leaf_info {
        if ovfl != 0 {
            free_overflow_chain(pager, ovfl)?;
        }
        if n_cell > 1 {
            if let Some((prev_fb, prev_sz, next_fb)) = fb_pos {
                let ptr_off = hdr + 8;
                let alloc = cell_len.max(4);
                let buf = pager.write_page_mut(hit_pgno)?;
                let ptr_end = ptr_off + 2 * n_cell;
                if hit_idx + 1 < n_cell {
                    buf.copy_within(ptr_off + 2 * (hit_idx + 1)..ptr_end, ptr_off + 2 * hit_idx);
                }
                buf[ptr_end - 2..ptr_end].fill(0);
                buf[hdr + 3..hdr + 5].copy_from_slice(&((n_cell - 1) as u16).to_be_bytes());
                free_leaf_region_in_place_at(
                    buf,
                    hit_pgno,
                    usable as usize,
                    pc,
                    alloc,
                    prev_fb,
                    prev_sz,
                    next_fb,
                );
                return Ok(true);
            }
            let (mut cells, _) = {
                let data = pager.read_page(hit_pgno)?;
                parse_index_cells(data, hit_pgno, usable)?
            };
            cells.remove(hit_idx);
            let raw: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            pack_btree_page(pager, hit_pgno, BtreePageType::IndexLeaf, 0, &raw)?;
            return Ok(true);
        }
        if path.is_empty() {
            let buf = pager.write_page_mut(hit_pgno)?;
            init_empty_btree_page(buf, hit_pgno, BtreePageType::IndexLeaf, usable);
            return Ok(true);
        }
        pager.free_page(hit_pgno)?;
        index_remove_empty_child(pager, root_pgno, &path)?;
        return Ok(true);
    }

    let data = pager.read_page(hit_pgno)?.to_vec();

    // Deleting from an interior index page: replace cell `hit_idx` with the rightmost
    // cell in `cells[hit_idx].left_child` (its in-order predecessor on a leaf)!
    let (cells, right_child) = parse_index_cells(&data, hit_pgno, usable)?;
    let old_ovfl = cells[hit_idx].overflow_pgno;
    let left_sub_root = cells[hit_idx].left_child;

    // Descend to rightmost leaf of `left_sub_root`.
    let mut pred_path = path;
    pred_path.push((hit_pgno, hit_idx));
    let mut cur_pg = left_sub_root;
    let pred_body: Vec<u8>;
    let pred_leaf_became_empty: bool;
    loop {
        if pred_path.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(
                "index B-tree exceeds max depth during predecessor descent",
            ));
        }
        let pdata = pager.read_page(cur_pg)?.to_vec();
        let phdr = if cur_pg == 1 { HEADER_SIZE } else { 0 };
        let p_is_leaf = pdata[phdr] == BtreePageType::IndexLeaf as u8;
        let (mut pcells, prc) = parse_index_cells(&pdata, cur_pg, usable)?;
        if p_is_leaf {
            let pred = pcells
                .pop()
                .ok_or_else(|| SqliteError::corrupt("empty non-root index leaf"))?;
            pred_body = pred.as_leaf_raw(true);
            pred_leaf_became_empty = pcells.is_empty();
            // Save leaf state (if empty, we'll detach it AFTER installing pred into hit_pgno).
            if !pred_leaf_became_empty {
                let raw: Vec<Vec<u8>> = pcells.into_iter().map(|c| c.raw).collect();
                pack_btree_page(pager, cur_pg, BtreePageType::IndexLeaf, 0, &raw)?;
            }
            break;
        }
        let next_slot = pcells.len();
        pred_path.push((cur_pg, next_slot));
        cur_pg = prc;
    }

    // Install predecessor into `hit_pgno` at `hit_idx`, preserving `left_child`.

    let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
    let mut replacement = Vec::with_capacity(4 + pred_body.len());
    replacement.extend_from_slice(&left_sub_root.to_be_bytes());
    replacement.extend_from_slice(&pred_body);
    raw_cells[hit_idx] = replacement;

    if cells_fit(hit_pgno, false, usable as usize, &raw_cells) {
        if old_ovfl != 0 {
            free_overflow_chain(pager, old_ovfl)?;
        }
        pack_btree_page(
            pager,
            hit_pgno,
            BtreePageType::IndexInterior,
            right_child,
            &raw_cells,
        )?;
        if pred_leaf_became_empty {
            pager.free_page(cur_pg)?;
            index_remove_empty_child(pager, root_pgno, &pred_path)?;
        }
    } else if !pred_leaf_became_empty {
        // `cur_pg` still has cells and was already repacked; `hit_pgno` & `path` are fresh!
        if old_ovfl != 0 {
            free_overflow_chain(pager, old_ovfl)?;
        }
        index_split_interior(pager, root_pgno, hit_pgno, &path, raw_cells, right_child)?;
    } else if cur_pg == left_sub_root {
        // `left_sub_root` was a 1-cell leaf right under `hit_idx`:
        // extract `pred_body`'s full payload, delete `hit_idx` + free `cur_pg`, then re-insert `pred`!
        let (psize, n1) = get_payload_size_varint(&pred_body, 0);
        let limits = payload_limits(usable);
        let local = local_payload_size(psize, limits.max_local, limits.min_local, usable);
        let pred_ovfl = if psize > local as u32 {
            read_u32_be(&pred_body, n1 + local)
        } else {
            0
        };
        let full_pred = read_full_payload(
            pager,
            &pred_body[n1..n1 + local],
            psize as usize,
            pred_ovfl,
        )?;
        if old_ovfl != 0 {
            free_overflow_chain(pager, old_ovfl)?;
        }
        if pred_ovfl != 0 {
            free_overflow_chain(pager, pred_ovfl)?;
        }
        pager.free_page(cur_pg)?;
        raw_cells.remove(hit_idx);
        if raw_cells.is_empty() {
            pack_btree_page(
                pager,
                hit_pgno,
                BtreePageType::IndexInterior,
                right_child,
                &[],
            )?;
            if path.is_empty() {
                let _ = try_collapse_child_into_root(pager, root_pgno, right_child)?;
            } else {
                // Non-root interior dropped to 0 cells: rebalance with sibling so leaf depth stays uniform.
                index_rebalance_zero_cell_interior(pager, root_pgno, hit_pgno, right_child, &path)?;
            }
        } else {
            pack_btree_page(
                pager,
                hit_pgno,
                BtreePageType::IndexInterior,
                right_child,
                &raw_cells,
            )?;
        }
        let _ = index_insert(pager, root_pgno, &full_pred, key_info, true)?;
        return Ok(true);
    } else {
        // `cur_pg` is strictly deeper than `left_sub_root` and became empty:
        // detach `cur_pg` first while `hit_pgno` still holds the untouched `key_vals`, then
        // re-find `key_vals` so we NEVER operate on stale `raw_cells` or stale `path`!
        pager.free_page(cur_pg)?;
        index_remove_empty_child(pager, root_pgno, &pred_path)?;

        let Ok((fresh_pg, fresh_idx, fresh_path)) = index_find(pager, root_pgno, key_vals, key_info)?
        else {
            return Err(SqliteError::corrupt(
                "index_delete: target key vanished after detaching empty predecessor leaf",
            ));
        };
        let fdata = pager.read_page(fresh_pg)?.to_vec();
        let fhdr = if fresh_pg == 1 { HEADER_SIZE } else { 0 };
        let f_is_leaf = fdata[fhdr] == BtreePageType::IndexLeaf as u8;
        let (fcells, frc) = parse_index_cells(&fdata, fresh_pg, usable)?;
        if fcells[fresh_idx].overflow_pgno != 0 {
            free_overflow_chain(pager, fcells[fresh_idx].overflow_pgno)?;
        }
        let mut fresh_raw: Vec<Vec<u8>> = fcells.iter().map(|c| c.raw.clone()).collect();
        fresh_raw[fresh_idx] = if f_is_leaf {
            pred_body
        } else {
            let mut v = Vec::with_capacity(4 + pred_body.len());
            v.extend_from_slice(&fcells[fresh_idx].left_child.to_be_bytes());
            v.extend_from_slice(&pred_body);
            v
        };
        if cells_fit(fresh_pg, f_is_leaf, usable as usize, &fresh_raw) {
            let ptype = if f_is_leaf {
                BtreePageType::IndexLeaf
            } else {
                BtreePageType::IndexInterior
            };
            pack_btree_page(pager, fresh_pg, ptype, frc, &fresh_raw)?;
        } else if f_is_leaf {
            index_split_leaf(pager, root_pgno, fresh_pg, &fresh_path, fresh_raw)?;
        } else {
            index_split_interior(pager, root_pgno, fresh_pg, &fresh_path, fresh_raw, frc)?;
        }
    }

    Ok(true)
}

fn index_remove_empty_child(
    pager: &mut WritePager,
    root_pgno: u32,
    path: &[(u32, usize)],
) -> SqliteResult<()> {
    let (parent_pg, child_slot) = *path.last().expect("path non-empty");
    let usable = pager.usable_size();
    let (mut cells, mut right_child) = {
        let data = pager.read_page(parent_pg)?;
        parse_index_cells(data, parent_pg, usable)?
    };

    // In an Index B-tree, interior cells ARE live keys (not duplicates)!
    // When child at `child_slot` becomes empty, we MUST NOT drop the adjacent separator key:
    // move the adjacent separator key DOWN into the neighboring sibling subtree!
    if cells.is_empty() {
        if path.len() == 1 {
            let buf = pager.write_page_mut(root_pgno)?;
            init_empty_btree_page(buf, root_pgno, BtreePageType::IndexLeaf, usable);
            return Ok(());
        }
        pager.free_page(parent_pg)?;
        return index_remove_empty_child(pager, root_pgno, &path[..path.len() - 1]);
    }

    let (demoted, sibling_pg, insert_at_front) = if child_slot < cells.len() {
        // Demote `cells[child_slot]` into the leftmost leaf of its right sibling!
        let sep = cells.remove(child_slot);
        let right_sib = if child_slot < cells.len() {
            cells[child_slot].left_child
        } else {
            right_child
        };
        (sep, right_sib, true)
    } else {
        // Empty child was `right_child`: demote last cell into rightmost leaf of `last.left_child`!
        let sep = cells.pop().unwrap();
        let left_sib = sep.left_child;
        right_child = left_sib;
        (sep, left_sib, false)
    };

    let demoted_leaf_raw = demoted.as_leaf_raw(false);

    // Update parent first (or rebalance parent if it now has 0 cells).
    if cells.is_empty() {
        if path.len() == 1 {
            // Root now has only `sibling_pg`! Collapse `sibling_pg` into root!
            let _ = try_collapse_child_into_root(pager, root_pgno, sibling_pg)?;
            // Insert demoted key into the root subtree!
            let target_pg = root_pgno;
            return index_insert_raw_at_edge(
                pager,
                root_pgno,
                target_pg,
                demoted_leaf_raw,
                insert_at_front,
            );
        }
        // Temporarily pack `parent_pg` with 0 cells and `right_child = sibling_pg`, then insert
        // `demoted_leaf_raw` into `sibling_pg`. If `sibling_pg` splits, `parent_pg` receives a
        // promoted key (now >= 1 cell and balanced!); otherwise rebalance `parent_pg` with an
        // adjacent sibling interior page under its grandparent so ALL leaves stay at uniform depth!
        pack_btree_page(
            pager,
            parent_pg,
            BtreePageType::IndexInterior,
            sibling_pg,
            &[],
        )?;
        let mut sib_path = path.to_vec();
        sib_path.last_mut().unwrap().1 = 0;
        index_insert_raw_at_edge_with_prefix(
            pager,
            root_pgno,
            &sib_path,
            sibling_pg,
            demoted_leaf_raw,
            insert_at_front,
        )?;
        let (recheck_cells, recheck_rc) = {
            let data = pager.read_page(parent_pg)?;
            parse_index_cells(data, parent_pg, usable)?
        };
        if !recheck_cells.is_empty() {
            return Ok(());
        }
        return index_rebalance_zero_cell_interior(
            pager,
            root_pgno,
            parent_pg,
            recheck_rc,
            &path[..path.len() - 1],
        );
    }

    let parent_raw: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
    pack_btree_page(
        pager,
        parent_pg,
        BtreePageType::IndexInterior,
        right_child,
        &parent_raw,
    )?;

    // Build path from root down to `sibling_pg` and insert `demoted_leaf_raw` at the edge!
    let mut sib_path = path.to_vec();
    let sib_slot = if child_slot < parent_raw.len() {
        child_slot
    } else {
        parent_raw.len()
    };
    sib_path.last_mut().unwrap().1 = sib_slot;
    index_insert_raw_at_edge_with_prefix(
        pager,
        root_pgno,
        &sib_path,
        sibling_pg,
        demoted_leaf_raw,
        insert_at_front,
    )
}

/// Rebalance a non-root `IndexInterior` page `empty_pg` that dropped to 0 cells (with single
/// remaining child `sole_child`) by merging into or borrowing from an adjacent sibling interior
/// page under its parent (`gp_path`), preserving uniform leaf depth across the entire B-tree.
fn index_rebalance_zero_cell_interior(
    pager: &mut WritePager,
    root_pgno: u32,
    empty_pg: u32,
    sole_child: u32,
    gp_path: &[(u32, usize)],
) -> SqliteResult<()> {
    let usable = pager.usable_size();
    let usable_usize = usable as usize;
    if gp_path.is_empty() {
        let _ = try_collapse_child_into_root(pager, root_pgno, sole_child)?;
        return Ok(());
    }

    let (gp_pg, gp_slot) = *gp_path.last().unwrap();
    let (mut gp_cells, mut gp_rc) = {
        let data = pager.read_page(gp_pg)?;
        parse_index_cells(data, gp_pg, usable)?
    };
    if gp_cells.is_empty() {
        pager.free_page(empty_pg)?;
        return index_rebalance_zero_cell_interior(
            pager,
            root_pgno,
            gp_pg,
            sole_child,
            &gp_path[..gp_path.len() - 1],
        );
    }

    if gp_slot < gp_cells.len() {
        // Merge/redistribute with RIGHT sibling of `empty_pg`.
        let sep_cell = gp_cells.remove(gp_slot);
        let sib_pg = if gp_slot < gp_cells.len() {
            gp_cells[gp_slot].left_child
        } else {
            gp_rc
        };
        let (sib_cells, sib_rc) = {
            let data = pager.read_page(sib_pg)?;
            parse_index_cells(data, sib_pg, usable)?
        };
        let mut pulled = Vec::with_capacity(sep_cell.raw.len());
        pulled.extend_from_slice(&sole_child.to_be_bytes());
        pulled.extend_from_slice(&sep_cell.raw[4..]);

        let mut merged_raw = Vec::with_capacity(1 + sib_cells.len());
        merged_raw.push(pulled);
        merged_raw.extend(sib_cells.into_iter().map(|c| c.raw));

        if cells_fit(sib_pg, false, usable_usize, &merged_raw) {
            pack_btree_page(pager, sib_pg, BtreePageType::IndexInterior, sib_rc, &merged_raw)?;
            pager.free_page(empty_pg)?;
            if gp_cells.is_empty() {
                return index_rebalance_zero_cell_interior(
                    pager,
                    root_pgno,
                    gp_pg,
                    sib_pg,
                    &gp_path[..gp_path.len() - 1],
                );
            }
            let gp_raw: Vec<Vec<u8>> = gp_cells.into_iter().map(|c| c.raw).collect();
            return pack_btree_page(pager, gp_pg, BtreePageType::IndexInterior, gp_rc, &gp_raw);
        }

        // Redistribute across `empty_pg` (left) and `sib_pg` (right).
        let mid = choose_index_split_mid(&merged_raw);
        let left_rc = read_u32_be(&merged_raw[mid], 0);
        let mut promoted = Vec::with_capacity(merged_raw[mid].len());
        promoted.extend_from_slice(&empty_pg.to_be_bytes());
        promoted.extend_from_slice(&merged_raw[mid][4..]);
        pack_btree_page(
            pager,
            empty_pg,
            BtreePageType::IndexInterior,
            left_rc,
            &merged_raw[..mid],
        )?;
        pack_btree_page(
            pager,
            sib_pg,
            BtreePageType::IndexInterior,
            sib_rc,
            &merged_raw[mid + 1..],
        )?;
        let mut gp_raw: Vec<Vec<u8>> = gp_cells.into_iter().map(|c| c.raw).collect();
        gp_raw.insert(gp_slot, promoted);
        if cells_fit(gp_pg, false, usable_usize, &gp_raw) {
            pack_btree_page(pager, gp_pg, BtreePageType::IndexInterior, gp_rc, &gp_raw)
        } else {
            index_split_interior(
                pager,
                root_pgno,
                gp_pg,
                &gp_path[..gp_path.len() - 1],
                gp_raw,
                gp_rc,
            )
        }
    } else {
        // `empty_pg` is `gp_rc`: merge/redistribute with LEFT sibling.
        let sep_cell = gp_cells.pop().unwrap();
        let sib_pg = sep_cell.left_child;
        gp_rc = sib_pg;
        let (sib_cells, sib_rc) = {
            let data = pager.read_page(sib_pg)?;
            parse_index_cells(data, sib_pg, usable)?
        };
        let mut pulled = Vec::with_capacity(sep_cell.raw.len());
        pulled.extend_from_slice(&sib_rc.to_be_bytes());
        pulled.extend_from_slice(&sep_cell.raw[4..]);

        let mut merged_raw = Vec::with_capacity(sib_cells.len() + 1);
        merged_raw.extend(sib_cells.into_iter().map(|c| c.raw));
        merged_raw.push(pulled);

        if cells_fit(sib_pg, false, usable_usize, &merged_raw) {
            pack_btree_page(
                pager,
                sib_pg,
                BtreePageType::IndexInterior,
                sole_child,
                &merged_raw,
            )?;
            pager.free_page(empty_pg)?;
            if gp_cells.is_empty() {
                return index_rebalance_zero_cell_interior(
                    pager,
                    root_pgno,
                    gp_pg,
                    sib_pg,
                    &gp_path[..gp_path.len() - 1],
                );
            }
            let gp_raw: Vec<Vec<u8>> = gp_cells.into_iter().map(|c| c.raw).collect();
            return pack_btree_page(pager, gp_pg, BtreePageType::IndexInterior, gp_rc, &gp_raw);
        }

        // Redistribute across `sib_pg` (left) and `empty_pg` (right).
        let mid = choose_index_split_mid(&merged_raw);
        let left_rc = read_u32_be(&merged_raw[mid], 0);
        let mut promoted = Vec::with_capacity(merged_raw[mid].len());
        promoted.extend_from_slice(&sib_pg.to_be_bytes());
        promoted.extend_from_slice(&merged_raw[mid][4..]);
        pack_btree_page(
            pager,
            sib_pg,
            BtreePageType::IndexInterior,
            left_rc,
            &merged_raw[..mid],
        )?;
        pack_btree_page(
            pager,
            empty_pg,
            BtreePageType::IndexInterior,
            sole_child,
            &merged_raw[mid + 1..],
        )?;
        let mut gp_raw: Vec<Vec<u8>> = gp_cells.into_iter().map(|c| c.raw).collect();
        gp_raw.push(promoted);
        gp_rc = empty_pg;
        if cells_fit(gp_pg, false, usable_usize, &gp_raw) {
            pack_btree_page(pager, gp_pg, BtreePageType::IndexInterior, gp_rc, &gp_raw)
        } else {
            index_split_interior(
                pager,
                root_pgno,
                gp_pg,
                &gp_path[..gp_path.len() - 1],
                gp_raw,
                gp_rc,
            )
        }
    }
}

fn index_insert_raw_at_edge(
    pager: &mut WritePager,
    root_pgno: u32,
    start_pg: u32,
    leaf_raw: Vec<u8>,
    at_front: bool,
) -> SqliteResult<()> {
    index_insert_raw_at_edge_with_prefix(pager, root_pgno, &[], start_pg, leaf_raw, at_front)
}

fn index_insert_raw_at_edge_with_prefix(
    pager: &mut WritePager,
    root_pgno: u32,
    prefix_path: &[(u32, usize)],
    start_pg: u32,
    leaf_raw: Vec<u8>,
    at_front: bool,
) -> SqliteResult<()> {
    let usable = pager.usable_size();
    let mut path = prefix_path.to_vec();
    let mut cur = start_pg;
    loop {
        if path.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(
                "index B-tree exceeds max depth during edge insert",
            ));
        }
        let data = pager.read_page(cur)?.to_vec();
        let hdr = if cur == 1 { HEADER_SIZE } else { 0 };
        let is_leaf = data[hdr] == BtreePageType::IndexLeaf as u8;
        let (cells, rc) = parse_index_cells(&data, cur, usable)?;
        if is_leaf {
            let mut raw: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            if at_front {
                raw.insert(0, leaf_raw);
            } else {
                raw.push(leaf_raw);
            }
            if cells_fit(cur, true, usable as usize, &raw) {
                return pack_btree_page(pager, cur, BtreePageType::IndexLeaf, 0, &raw);
            }
            return index_split_leaf(pager, root_pgno, cur, &path, raw);
        }
        if at_front {
            if cells.is_empty() {
                return Err(SqliteError::corrupt(format!(
                    "page {cur}: empty index interior page during rebalance"
                )));
            }
            path.push((cur, 0));
            cur = cells[0].left_child;
        } else {
            path.push((cur, cells.len()));
            cur = rc;
        }
    }
}

/// Partition `cells` (`len >= 2`) into `k >= 2` non-empty contiguous chunks where EVERY chunk
/// fits on a non-root TableLeaf page (`usable_size - 8` bytes).
///
/// Prefers the most balanced 2-way split when one exists; falls back to greedy multi-way packing
/// (e.g. 3 chunks for `[250B, 490B, 250B]` on a 512-byte page where no 2-way split fits).
fn partition_table_leaf_cells(
    cells: &[Vec<u8>],
    usable_size: usize,
) -> SqliteResult<Vec<(usize, usize)>> {
    let cap = usable_size.saturating_sub(8);
    let sizes: Vec<usize> = cells.iter().map(|c| cell_storage_len(c) + 2).collect();
    if let Some(&biggest) = sizes.iter().max() {
        if biggest > cap {
            return Err(SqliteError::corrupt(format!(
                "table leaf cell ({biggest} bytes) exceeds page capacity {cap}"
            )));
        }
    }
    let total: usize = sizes.iter().sum();
    let half = total / 2;

    // 1. Try finding the best balanced 2-way split where BOTH halves fit in `cap`.
    let mut best_split: Option<(usize, usize)> = None; // (split_idx, distance_from_half)
    let mut left_sum = 0usize;
    for (i, &sz) in sizes.iter().enumerate() {
        left_sum += sz;
        let split = i + 1;
        if split >= sizes.len() {
            break;
        }
        let right_sum = total - left_sum;
        if left_sum <= cap && right_sum <= cap {
            let dist = left_sum.abs_diff(half);
            if best_split.is_none_or(|(_, best_d)| dist < best_d) {
                best_split = Some((split, dist));
            }
        }
    }
    if let Some((split, _)) = best_split {
        return Ok(vec![(0, split), (split, cells.len())]);
    }

    // 2. Greedy multi-way packing (guaranteed >= 2 chunks, each <= cap and non-empty).
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < sizes.len() {
        let mut acc = sizes[start];
        let mut end = start + 1;
        while end < sizes.len() && acc + sizes[end] <= cap {
            acc += sizes[end];
            end += 1;
        }
        chunks.push((start, end));
        start = end;
    }
    Ok(chunks)
}

/// Choose a balanced split middle index `1..len-1` for IndexLeaf / IndexInterior
/// (where `cells[mid]` is promoted to parent, leaving non-empty `..mid` and `mid+1..`).
fn choose_index_split_mid(cells: &[Vec<u8>]) -> usize {
    debug_assert!(cells.len() >= 3);
    let total: usize = cells.iter().map(|c| cell_storage_len(c) + 2).sum();
    let half = total / 2;
    let mut acc = 0usize;
    for (i, c) in cells.iter().enumerate() {
        acc += cell_storage_len(c) + 2;
        if acc >= half && i >= 1 && i + 1 < cells.len() {
            return i;
        }
    }
    (cells.len() / 2).clamp(1, cells.len() - 2)
}

// ───────────────────────────── PRAGMA integrity_check ─────────────────────────────

/// Verify complete structural integrity of all B-trees, overflow chains, and freelist.
/// Returns `vec!["ok"]` if everything is clean, or a list of diagnostic error strings.
pub fn check_integrity(pager: &WritePager, root_pages: &[u32]) -> Vec<String> {
    if pager.page_count() == 0 {
        return vec!["ok".to_string()];
    }
    let mut errors = Vec::new();
    let page_count = pager.page_count();
    let usable = pager.usable_size();
    let limits = payload_limits(usable);
    let lbp = lock_byte_page(pager.page_size());
    let mut visited: HashSet<u32> = HashSet::new();

    // 1. Walk all B-trees
    let usable_len = usable as usize;
    for &root in root_pages {
        if root == 0 {
            continue;
        }
        let mut expected_leaf_depth: Option<usize> = None;
        let mut expected_is_table: Option<bool> = None;
        // Stack entries: (pgno, is_root, depth)
        let mut stack = vec![(root, true, 0usize)];
        while let Some((pgno, is_root, depth)) = stack.pop() {
            if pgno < 1 || pgno > page_count {
                errors.push(format!("page {pgno} out of range 1..={page_count}"));
                continue;
            }
            if !visited.insert(pgno) {
                errors.push(format!("page {pgno} is referenced more than once"));
                continue;
            }
            let Ok(data) = pager.read_page(pgno) else {
                errors.push(format!("cannot read page {pgno}"));
                continue;
            };
            let bound = usable_len.min(data.len());
            let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
            if hdr + 8 > bound {
                errors.push(format!("page {pgno}: truncated header"));
                continue;
            }
            let Some(ptype) = BtreePageType::from_byte(data[hdr]) else {
                errors.push(format!("page {pgno}: invalid type 0x{:02x}", data[hdr]));
                continue;
            };
            let n_cell = usize::from(read_u16_be(data, hdr + 3));
            let is_leaf = ptype.is_leaf();
            if !is_root && n_cell == 0 {
                errors.push(format!("page {pgno}: non-root page has 0 cells"));
            }
            if is_root && !is_leaf && n_cell == 0 && pgno != 1 {
                errors.push(format!("page {pgno}: non-page-1 interior root has 0 cells"));
            }
            let is_table = ptype.is_table();
            if let Some(exp_tbl) = expected_is_table {
                if exp_tbl != is_table {
                    errors.push(format!(
                        "page {pgno}: mismatched B-tree page kind in tree rooted at {root}"
                    ));
                }
            } else {
                expected_is_table = Some(is_table);
            }
            if is_leaf {
                if let Some(exp_d) = expected_leaf_depth {
                    if exp_d != depth {
                        errors.push(format!(
                            "page {pgno}: child page depth differs ({depth} != {exp_d})"
                        ));
                    }
                } else {
                    expected_leaf_depth = Some(depth);
                }
            }
            let ptr_off = hdr + if is_leaf { 8 } else { 12 };
            let ptr_end = ptr_off + 2 * n_cell;
            if ptr_end > bound {
                errors.push(format!("page {pgno}: cell pointer array overflows page"));
                continue;
            }
            let content_raw = read_u16_be(data, hdr + 5);
            let content_start = if content_raw == 0 {
                65536usize
            } else {
                usize::from(content_raw)
            };
            if content_start < ptr_end || content_start > bound {
                errors.push(format!(
                    "page {pgno}: content_start {content_start} outside {ptr_end}..={bound}"
                ));
            }
            let frag = data[hdr + 7];
            if frag > 60 {
                errors.push(format!("page {pgno}: fragmented_bytes {frag} > 60"));
            }
            // Validate freeblock chain (strictly ascending offsets inside content_start..bound).
            let mut fb = usize::from(read_u16_be(data, hdr + 1));
            let mut prev_fb_end = 0usize;
            let mut fb_hops = 0usize;
            while fb != 0 {
                fb_hops += 1;
                if fb_hops > bound / 4 + 1 {
                    errors.push(format!("page {pgno}: freeblock cycle"));
                    break;
                }
                if fb < content_start || fb + 4 > bound {
                    errors.push(format!("page {pgno}: freeblock {fb} out of bounds"));
                    break;
                }
                if prev_fb_end > 0 && fb <= prev_fb_end {
                    errors.push(format!(
                        "page {pgno}: freeblocks out of order or adjacent ({fb} <= {prev_fb_end})"
                    ));
                    break;
                }
                let next_fb = usize::from(read_u16_be(data, fb));
                let fb_size = usize::from(read_u16_be(data, fb + 2));
                if fb_size < 4 || fb + fb_size > bound {
                    errors.push(format!(
                        "page {pgno}: freeblock at {fb} has invalid size {fb_size}"
                    ));
                    break;
                }
                prev_fb_end = fb + fb_size;
                fb = next_fb;
            }
            if !is_leaf {
                let rc = read_u32_be(data, hdr + 8);
                stack.push((rc, false, depth + 1));
            }
            let (max_l, min_l) = if is_table && is_leaf {
                (limits.max_leaf, limits.min_leaf)
            } else {
                (limits.max_local, limits.min_local)
            };
            let mut prev_rowid: Option<i64> = None;
            for i in 0..n_cell {
                let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
                if pc < content_start || pc >= bound {
                    errors.push(format!(
                        "page {pgno}: cell {i} pointer {pc} outside {content_start}..{bound}"
                    ));
                    continue;
                }
                let mut pos = pc;
                if !is_leaf {
                    if pos + 4 > bound {
                        errors.push(format!("page {pgno}: cell {i} runs past page end"));
                        continue;
                    }
                    stack.push((read_u32_be(data, pos), false, depth + 1));
                    pos += 4;
                }
                if is_table && !is_leaf {
                    let (rk, klen) = get_varint(data, pos);
                    if pos + klen > bound {
                        errors.push(format!("page {pgno}: cell {i} key runs past page end"));
                        continue;
                    }
                    if let Some(prev) = prev_rowid {
                        if rk <= prev {
                            errors.push(format!("page {pgno}: unsorted table interior keys"));
                        }
                    }
                    prev_rowid = Some(rk);
                    continue;
                }
                let (psize_u32, n1) = get_payload_size_varint(data, pos);
                pos += n1;
                if is_table {
                    let (rk, n2) = get_varint(data, pos);
                    if pos + n2 > bound {
                        errors.push(format!("page {pgno}: cell {i} rowid runs past page end"));
                        continue;
                    }
                    if let Some(prev) = prev_rowid {
                        if rk <= prev {
                            errors.push(format!("page {pgno}: unsorted table leaf rowids"));
                        }
                    }
                    prev_rowid = Some(rk);
                    pos += n2;
                }
                let local = local_payload_size(psize_u32, max_l, min_l, usable);
                let expected_end = pos + local + if psize_u32 > local as u32 { 4 } else { 0 };
                if expected_end > bound {
                    errors.push(format!("page {pgno}: cell {i} payload runs past page end"));
                    continue;
                }
                if psize_u32 > local as u32 {
                    let expected_ovfl = overflow_page_count(psize_u32, local, usable);
                    let mut actual_ovfl = 0u32;
                    let mut ovfl = read_u32_be(data, pos + local);
                    while ovfl != 0 {
                        if ovfl < 2 || ovfl > page_count || ovfl == lbp {
                            errors.push(format!("page {pgno}: bad overflow page {ovfl}"));
                            break;
                        }
                        if !visited.insert(ovfl) {
                            errors.push(format!("overflow page {ovfl} referenced more than once"));
                            break;
                        }
                        actual_ovfl += 1;
                        ovfl = pager.read_page(ovfl).map(|p| read_u32_be(p, 0)).unwrap_or(0);
                    }
                    if actual_ovfl != expected_ovfl {
                        errors.push(format!(
                            "page {pgno}: cell {i} overflow chain length {actual_ovfl} != expected {expected_ovfl}"
                        ));
                    }
                }
            }
        }
    }

    // 2. Walk freelist
    let max_leaves = ((usable as usize).saturating_sub(8)) / 4;
    let mut trunk = pager.freelist_trunk();
    let mut free_seen = 0u32;
    while trunk != 0 {
        if trunk < 2 || trunk > page_count || trunk == lbp {
            errors.push(format!("freelist trunk {trunk} out of range"));
            break;
        }
        if !visited.insert(trunk) {
            errors.push(format!("freelist trunk {trunk} referenced more than once"));
            break;
        }
        free_seen += 1;
        let Ok(tpage) = pager.read_page(trunk) else {
            errors.push(format!("cannot read freelist trunk {trunk}"));
            break;
        };
        if tpage.len() < 8 {
            errors.push(format!("freelist trunk {trunk} truncated"));
            break;
        }
        let next_trunk = read_u32_be(tpage, 0);
        let cnt = read_u32_be(tpage, 4) as usize;
        if cnt > max_leaves {
            errors.push(format!(
                "freelist trunk {trunk}: leaf count {cnt} exceeds max {max_leaves}"
            ));
            break;
        }
        for i in 0..cnt {
            let off = 8 + i * 4;
            let leaf = read_u32_be(tpage, off);
            if leaf < 2 || leaf > page_count || leaf == lbp {
                errors.push(format!("freelist leaf {leaf} out of range"));
                continue;
            }
            if !visited.insert(leaf) {
                errors.push(format!("freelist leaf {leaf} referenced more than once"));
            }
            free_seen += 1;
        }
        trunk = next_trunk;
    }
    if free_seen != pager.freelist_count() {
        errors.push(format!(
            "freelist count mismatch: header={}, actual={free_seen}",
            pager.freelist_count()
        ));
    }

    // 3. Check for leaked / unreferenced pages
    for pgno in 1..=page_count {
        if pgno == lbp {
            continue;
        }
        if !visited.contains(&pgno) {
            errors.push(format!("page {pgno} is never used"));
        }
    }

    if errors.is_empty() {
        vec!["ok".to_string()]
    } else {
        errors
    }
}

// ───────────────────── Direct fast SELECT scans on WritePager ─────────────────────

/// Projection slot for direct table-B-tree fast scans (`ORDER BY ... LIMIT` Top-K).
#[derive(Debug, Clone)]
pub enum DirectTableSlot {
    Rowid,
    Field {
        storage_idx: usize,
        default_val: SqlValue,
        real_back: bool,
    },
}

/// Projection slot for direct covering-index fast scans (`ORDER BY indexed_col LIMIT`).
#[derive(Debug, Clone, Copy)]
pub struct DirectIndexSlot {
    pub idx_pos: usize,
    pub real_back: bool,
}

fn decode_direct_index_payload(
    payload: &[u8],
    slots: &[DirectIndexSlot],
    encoding: TextEncoding,
    meter: &dyn xdb_core::Meter,
) -> SqliteResult<Vec<SqlValue>> {
    let mut row = vec![SqlValue::Null; slots.len()];
    if payload.is_empty() {
        return Err(SqliteError::corrupt("empty index record payload"));
    }
    let max_pos = slots.iter().map(|s| s.idx_pos).max().unwrap_or(0);
    let metered = !meter.is_noop();
    let (hdr_len_u32, mut hpos) = if payload[0] < 0x80 {
        (u32::from(payload[0]), 1usize)
    } else {
        get_varint32(payload, 0)
    };
    let hdr_len = hdr_len_u32 as usize;
    if hdr_len == 0 {
        return Err(SqliteError::corrupt("zero-length index record header"));
    }
    if hdr_len > payload.len() || hpos >= hdr_len {
        return Err(SqliteError::corrupt("index record header out of bounds"));
    }
    let mut boff = hdr_len;
    let mut col = 0usize;
    while hpos < hdr_len && col <= max_pos {
        if metered && (col & 31) == 31 {
            meter.charge_steps(1)?;
        }
        let b = payload[hpos];
        let (st, n) = if b < 0x80 {
            (u32::from(b), 1)
        } else {
            get_varint32(payload, hpos)
        };
        hpos += n;
        if hpos > hdr_len {
            return Err(SqliteError::corrupt("index record header varint overrun"));
        }
        let len = serial_type_length(st);
        let end = boff
            .checked_add(len)
            .ok_or_else(|| SqliteError::corrupt("index record body length overflow"))?;
        if end > payload.len() {
            return Err(SqliteError::corrupt("index record body truncated"));
        }
        for (out_i, sl) in slots.iter().enumerate() {
            if sl.idx_pos == col {
                let v = if !sl.real_back && st <= 9 {
                    if let Some((iv, _)) = decode_int_st_fast(st as u8, payload, boff) {
                        SqlValue::Int(iv)
                    } else {
                        decode_serial_value(&payload[boff..end], st, encoding)
                    }
                } else {
                    let raw = decode_serial_value(&payload[boff..end], st, encoding);
                    if sl.real_back {
                        crate::schema::real_read_back(raw)
                    } else {
                        raw
                    }
                };
                row[out_i] = v;
            }
        }
        boff = end;
        col += 1;
    }
    if hpos == hdr_len && boff != payload.len() {
        return Err(SqliteError::corrupt("index record payload has trailing bytes"));
    }
    Ok(row)
}

struct DirectScanMemGuard<'a> {
    meter: Option<&'a dyn xdb_core::Meter>,
    reserved: u64,
}

impl<'a> DirectScanMemGuard<'a> {
    fn new(meter: Option<&'a dyn xdb_core::Meter>) -> Self {
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

    #[inline]
    fn release(&mut self, bytes: u64) {
        if let Some(m) = self.meter {
            let rel = bytes.min(self.reserved);
            if rel > 0 {
                m.release_memory(rel);
                self.reserved -= rel;
            }
        }
    }
}

impl Drop for DirectScanMemGuard<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.meter {
            if self.reserved > 0 {
                m.release_memory(self.reserved);
                self.reserved = 0;
            }
        }
    }
}

#[inline]
fn estimate_direct_val_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Null | SqlValue::Int(_) | SqlValue::Real(_) => 24,
        SqlValue::Text(b) | SqlValue::Blob(b) => 24u64.saturating_add(b.len() as u64),
    }
}

#[inline]
fn estimate_direct_row_bytes(row: &[SqlValue]) -> u64 {
    let mut total = 24u64;
    for v in row {
        total = total.saturating_add(estimate_direct_val_bytes(v));
    }
    total
}

/// Walk a covering secondary index B-tree directly on `WritePager` in exact `ASC` or `DESC`
/// order, emitting `limit` rows (after skipping `offset`) with zero table seeks and zero
/// `SqliteDatabase` / `Mutex` overhead.
pub fn index_order_scan_covering_direct(
    pager: &WritePager,
    idx_root: u32,
    desc: bool,
    limit: usize,
    offset: usize,
    slots: &[DirectIndexSlot],
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let page_count = pager.page_count();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();

    enum IdxStep {
        Descend(u32, usize),
        /// Lazy interior-node stepper: avoids pushing all `2 * n_cell + 1` children on `LIMIT K`.
        /// For `ASC`: `next_ci` counts `0..=n_cell` (step `i < n_cell` emits cell `i` and descends
        /// `left[i+1]`/`right_child`). For `DESC`: `next_ci` counts `n_cell` down to `0` (`k > 0`
        /// emits cell `k - 1` and descends `left[k - 1]`).
        InteriorNext {
            pgno: u32,
            depth: usize,
            next_ci: usize,
        },
    }

    let mut out = Vec::with_capacity(limit.min(256));
    let mut mem = DirectScanMemGuard::new(metered.then_some(meter.as_ref()));
    let mut skipped = 0usize;
    let mut visited_count = 0u32;
    let mut visited = HashSet::<u32>::new();
    let mut stack: Vec<IdxStep> = Vec::with_capacity(BTREE_MAX_DEPTH * 2);
    stack.push(IdxStep::Descend(idx_root, 0));

    let emit_cell = |data: &[u8],
                     pgno: u32,
                     is_leaf: bool,
                     ptr_off: usize,
                     ci: usize,
                     skipped: &mut usize,
                     out: &mut Vec<Vec<SqlValue>>,
                     mem: &mut DirectScanMemGuard<'_>|
     -> SqliteResult<bool> {
        if metered {
            meter.charge_steps(1)?;
        }
        if *skipped < offset {
            *skipped += 1;
            return Ok(false);
        }
        let bound = (usable as usize).min(data.len());
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
        let pos0 = pc + if is_leaf { 0 } else { 4 };
        if pos0 >= bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} out of bounds"
            )));
        }
        let (psize_u32, n1) = get_payload_size_varint(data, pos0);
        let local = local_payload_size(psize_u32, limits.max_local, limits.min_local, usable);
        let local_start = pos0 + n1;
        let has_ovfl = psize_u32 > local as u32;
        let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
        if cell_end > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} runs past usable page end"
            )));
        }
        let row = if !has_ovfl {
            decode_direct_index_payload(
                &data[local_start..local_start + local],
                slots,
                encoding,
                meter.as_ref(),
            )?
        } else {
            let ovfl = read_u32_be(data, local_start + local);
            let full = read_full_payload(
                pager,
                &data[local_start..local_start + local],
                psize_u32 as usize,
                ovfl,
            )?;
            decode_direct_index_payload(&full, slots, encoding, meter.as_ref())?
        };
        if metered {
            mem.reserve(estimate_direct_row_bytes(&row))?;
        }
        out.push(row);
        Ok(out.len() >= limit)
    };

    while let Some(step) = stack.pop() {
        match step {
            IdxStep::InteriorNext {
                pgno,
                depth,
                next_ci,
            } => {
                let data = pager.read_page(pgno)?;
                let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
                let bound = (usable as usize).min(data.len());
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let ptr_off = hdr + 12;
                if desc {
                    if next_ci > 0 {
                        let ci = next_ci - 1;
                        if emit_cell(
                            data,
                            pgno,
                            false,
                            ptr_off,
                            ci,
                            &mut skipped,
                            &mut out,
                            &mut mem,
                        )? {
                            return Ok(out);
                        }
                        let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                        if pc + 4 > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index interior cell {ci} out of bounds"
                            )));
                        }
                        let left_child = read_u32_be(data, pc);
                        if ci > 0 {
                            stack.push(IdxStep::InteriorNext {
                                pgno,
                                depth,
                                next_ci: ci,
                            });
                        }
                        stack.push(IdxStep::Descend(left_child, depth + 1));
                    }
                } else if next_ci < n_cell {
                    if emit_cell(
                        data,
                        pgno,
                        false,
                        ptr_off,
                        next_ci,
                        &mut skipped,
                        &mut out,
                        &mut mem,
                    )? {
                        return Ok(out);
                    }
                    let next_child = if next_ci + 1 < n_cell {
                        let pc = usize::from(read_u16_be(data, ptr_off + 2 * (next_ci + 1)));
                        if pc + 4 > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index interior cell {} out of bounds",
                                next_ci + 1
                            )));
                        }
                        read_u32_be(data, pc)
                    } else {
                        read_u32_be(data, hdr + 8)
                    };
                    if next_ci + 1 < n_cell {
                        stack.push(IdxStep::InteriorNext {
                            pgno,
                            depth,
                            next_ci: next_ci + 1,
                        });
                    }
                    stack.push(IdxStep::Descend(next_child, depth + 1));
                }
            }
            IdxStep::Descend(pgno, depth) => {
                visited_count = visited_count.saturating_add(1);
                if visited_count > page_count || !visited.insert(pgno) {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} contains a cycle"
                    )));
                }
                if depth >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} exceeds max depth"
                    )));
                }
                let data = pager.read_page(pgno)?;
                let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
                let bound = (usable as usize).min(data.len());
                if hdr + 8 > bound {
                    return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
                }
                match BtreePageType::from_byte(data[hdr]) {
                    Some(BtreePageType::IndexLeaf) => {
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let ptr_off = hdr + 8;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index leaf pointers out of bounds"
                            )));
                        }
                        if desc {
                            for ci in (0..n_cell).rev() {
                                if emit_cell(
                                    data,
                                    pgno,
                                    true,
                                    ptr_off,
                                    ci,
                                    &mut skipped,
                                    &mut out,
                                    &mut mem,
                                )? {
                                    return Ok(out);
                                }
                            }
                        } else {
                            for ci in 0..n_cell {
                                if emit_cell(
                                    data,
                                    pgno,
                                    true,
                                    ptr_off,
                                    ci,
                                    &mut skipped,
                                    &mut out,
                                    &mut mem,
                                )? {
                                    return Ok(out);
                                }
                            }
                        }
                    }
                    Some(BtreePageType::IndexInterior) => {
                        if metered {
                            meter.charge_steps(1)?;
                        }
                        if hdr + 12 > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: truncated index interior header"
                            )));
                        }
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let right_child = read_u32_be(data, hdr + 8);
                        let ptr_off = hdr + 12;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index interior pointers out of bounds"
                            )));
                        }
                        if desc {
                            if n_cell > 0 {
                                stack.push(IdxStep::InteriorNext {
                                    pgno,
                                    depth,
                                    next_ci: n_cell,
                                });
                            }
                            stack.push(IdxStep::Descend(right_child, depth + 1));
                        } else if n_cell == 0 {
                            stack.push(IdxStep::Descend(right_child, depth + 1));
                        } else {
                            let pc = usize::from(read_u16_be(data, ptr_off));
                            if pc + 4 > bound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {pgno}: index interior cell 0 out of bounds"
                                )));
                            }
                            let left_child = read_u32_be(data, pc);
                            stack.push(IdxStep::InteriorNext {
                                pgno,
                                depth,
                                next_ci: 0,
                            });
                            stack.push(IdxStep::Descend(left_child, depth + 1));
                        }
                    }
                    _ => {
                        return Err(SqliteError::corrupt(format!(
                            "index_order_scan_covering_direct: invalid page type 0x{:02x} on page {pgno}",
                            data[hdr]
                        )));
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Direct-on-`WritePager` Top-K scan for `SELECT <cols> FROM tbl ORDER BY <stored_col> [DESC] LIMIT k [OFFSET o]`:
/// walks table leaf pages directly, checks ONLY the sort key inline (skipping `rowid` varint and
/// all other columns on the >99% rejected rows), and returns the final sorted `limit` rows.
#[allow(clippy::too_many_arguments)]
pub fn table_topk_scan_direct(
    pager: &WritePager,
    table_root: u32,
    key_storage_idx: usize,
    key_default: &SqlValue,
    key_real_back: bool,
    desc: bool,
    nulls_first: bool,
    coll: Option<&Collation>,
    limit: usize,
    offset: usize,
    slots: &[DirectTableSlot],
    encoding: TextEncoding,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let cap = offset.saturating_add(limit);
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let page_count = pager.page_count();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();

    // Comparator: `Less` means `a` is BETTER (comes earlier in desired ORDER BY) than `b`.
    let cmp_order = |ka: &SqlValue, sa: u64, kb: &SqlValue, sb: u64| -> Ordering {
        let na = ka.is_null();
        let nb = kb.is_null();
        if na || nb {
            if na && nb {
                return sa.cmp(&sb);
            }
            return if na {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            } else if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let mut o = mem_compare(ka, kb, coll);
        if desc {
            o = o.reverse();
        }
        o.then_with(|| sa.cmp(&sb))
    };

    let decode_projected_row = |payload: &[u8], rid: i64| -> SqliteResult<Vec<SqlValue>> {
        let mut want_fields = [0usize; 32];
        let mut want_len = 0usize;
        let mut want_heap: Option<Vec<usize>> = None;
        let mut max_f = 0usize;
        for sl in slots {
            if let DirectTableSlot::Field { storage_idx, .. } = sl {
                if *storage_idx > max_f {
                    max_f = *storage_idx;
                }
                if want_len < 32 {
                    want_fields[want_len] = *storage_idx;
                    want_len += 1;
                } else {
                    let v = want_heap.get_or_insert_with(|| want_fields.to_vec());
                    v.push(*storage_idx);
                }
            }
        }
        let mut out_row = Vec::with_capacity(slots.len());
        if want_len == 0 {
            for _ in slots {
                out_row.push(SqlValue::Int(rid));
            }
            return Ok(out_row);
        }
        if let Some(ref wv) = want_heap {
            let (mut dec, n_fields) =
                decode_record_sparse_fields_metered(payload, encoding, wv, meter.as_ref())?;
            let mut fi = 0usize;
            for sl in slots {
                match sl {
                    DirectTableSlot::Rowid => out_row.push(SqlValue::Int(rid)),
                    DirectTableSlot::Field {
                        storage_idx,
                        default_val,
                        real_back,
                    } => {
                        let raw = if *storage_idx < n_fields {
                            std::mem::replace(&mut dec[fi], SqlValue::Null)
                        } else {
                            default_val.clone()
                        };
                        fi += 1;
                        out_row.push(if *real_back {
                            crate::schema::real_read_back(raw)
                        } else {
                            raw
                        });
                    }
                }
            }
            return Ok(out_row);
        }
        let mut dec_buf = [const { SqlValue::Null }; 32];
        let n_fields = decode_record_sparse_fields_into_metered(
            payload,
            encoding,
            &want_fields[..want_len],
            &mut dec_buf[..want_len],
            meter.as_ref(),
        )?;
        let mut fi = 0usize;
        for sl in slots {
            match sl {
                DirectTableSlot::Rowid => out_row.push(SqlValue::Int(rid)),
                DirectTableSlot::Field {
                    storage_idx,
                    default_val,
                    real_back,
                } => {
                    let raw = if *storage_idx < n_fields {
                        std::mem::replace(&mut dec_buf[fi], SqlValue::Null)
                    } else {
                        default_val.clone()
                    };
                    fi += 1;
                    out_row.push(if *real_back {
                        crate::schema::real_read_back(raw)
                    } else {
                        raw
                    });
                }
            }
        }
        Ok(out_row)
    };

    let mut heap: Vec<(SqlValue, u64, Vec<SqlValue>)> = Vec::with_capacity(cap.min(1024));
    let mut mem = DirectScanMemGuard::new(metered.then_some(meter.as_ref()));
    let mut worst_int: Option<i64> = None;
    let mut seq = 0u64;
    let mut visited_count = 0u32;
    let mut visited = HashSet::<u32>::new();
    let mut stack: Vec<(u32, usize)> = Vec::with_capacity(BTREE_MAX_DEPTH * 2);
    stack.push((table_root, 0));

    while let Some((pgno, depth)) = stack.pop() {
        visited_count = visited_count.saturating_add(1);
        if visited_count > page_count || !visited.insert(pgno) {
            return Err(SqliteError::corrupt(format!(
                "table B-tree {table_root} contains a cycle"
            )));
        }
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        let bound = (usable as usize).min(data.len());
        if hdr + 8 > bound {
            return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
        }
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableInterior) => {
                if depth + 1 >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "table B-tree {table_root} exceeds max depth"
                    )));
                }
                if metered {
                    meter.charge_steps(1)?;
                }
                if hdr + 12 > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: truncated interior header"
                    )));
                }
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let right_child = read_u32_be(data, hdr + 8);
                let ptr_off = hdr + 12;
                if ptr_off + 2 * n_cell > bound {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: interior cell pointers out of bounds"
                    )));
                }
                stack.push((right_child, depth + 1));
                for ci in (0..n_cell).rev() {
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                    if pc + 4 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: interior cell {ci} out of bounds"
                        )));
                    }
                    stack.push((read_u32_be(data, pc), depth + 1));
                }
            }
            Some(BtreePageType::TableLeaf) => {
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
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
                    let cur_seq = seq;
                    seq = seq.wrapping_add(1);
                    let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                    let (psize_u32, n1) = if pc < bound && data[pc] < 0x80 {
                        (u32::from(data[pc]), 1usize)
                    } else {
                        get_payload_size_varint(data, pc)
                    };
                    // Skip computing rowid value unless row survives key check!
                    let rid_off = pc + n1;
                    let n2 = if rid_off < bound && data[rid_off] < 0x80 {
                        1usize
                    } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                        2usize
                    } else {
                        get_varint(data, rid_off).1
                    };
                    let local = if psize_u32 <= limits.max_leaf {
                        psize_u32 as usize
                    } else {
                        local_payload_size(psize_u32, limits.max_leaf, limits.min_leaf, usable)
                    };
                    let local_start = rid_off + n2;
                    let has_ovfl = psize_u32 > local as u32;
                    let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
                    if cell_end > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: leaf cell {ci} out of bounds"
                        )));
                    }

                    // Inline fast-path key extraction + register rejection (skips rowid + all other cols!)
                    let mut fast_key: Option<SqlValue> = None;
                    let mut inline_ok = false;
                    if !has_ovfl {
                        let payload = &data[local_start..local_start + local];
                        if !payload.is_empty() && payload[0] < 0x80 {
                            let hdr_len = usize::from(payload[0]);
                            if hdr_len > 1 && hdr_len <= payload.len() {
                                let mut hpos = 1usize;
                                let mut boff = hdr_len;
                                let mut col = 0usize;
                                let mut target: Option<(u32, usize, usize)> = None;
                                let mut valid = true;
                                while hpos < hdr_len {
                                    let hb = payload[hpos];
                                    let (st, n) = if hb < 0x80 {
                                        (u32::from(hb), 1usize)
                                    } else {
                                        get_varint32(payload, hpos)
                                    };
                                    hpos += n;
                                    if hpos > hdr_len {
                                        valid = false;
                                        break;
                                    }
                                    let blen = serial_type_length(st);
                                    let Some(end) = boff.checked_add(blen) else {
                                        valid = false;
                                        break;
                                    };
                                    if end > payload.len() {
                                        valid = false;
                                        break;
                                    }
                                    if col == key_storage_idx {
                                        if hpos == hdr_len && end != payload.len() {
                                            valid = false;
                                        } else {
                                            target = Some((st, boff, end));
                                        }
                                        break;
                                    }
                                    boff = end;
                                    col += 1;
                                }
                                if valid && target.is_none() && boff != payload.len() {
                                    valid = false;
                                }
                                if valid {
                                    inline_ok = true;
                                    if let Some((st, s_off, e_off)) = target {
                                        if !key_real_back && st <= 9 {
                                            if let Some((iv, _)) =
                                                decode_int_st_fast(st as u8, payload, s_off)
                                            {
                                                if let Some(w) = worst_int {
                                                    if if desc { iv <= w } else { iv >= w } {
                                                        continue;
                                                    }
                                                }
                                                fast_key = Some(SqlValue::Int(iv));
                                            }
                                        }
                                        if fast_key.is_none() {
                                            let raw = decode_serial_value(
                                                &payload[s_off..e_off],
                                                st,
                                                encoding,
                                            );
                                            let v = if key_real_back {
                                                crate::schema::real_read_back(raw)
                                            } else {
                                                raw
                                            };
                                            if let (Some(w), SqlValue::Int(iv)) = (worst_int, &v) {
                                                if if desc { *iv <= w } else { *iv >= w } {
                                                    continue;
                                                }
                                            }
                                            fast_key = Some(v);
                                        }
                                    } else {
                                        let v = if key_real_back {
                                            crate::schema::real_read_back(key_default.clone())
                                        } else {
                                            key_default.clone()
                                        };
                                        fast_key = Some(v);
                                    }
                                }
                            }
                        }
                    }

                    let mut owned_full: Option<Vec<u8>> = None;
                    let key = if inline_ok {
                        fast_key.expect("key decoded")
                    } else {
                        let payload_slice: &[u8] = if !has_ovfl {
                            &data[local_start..local_start + local]
                        } else {
                            let ovfl = read_u32_be(data, local_start + local);
                            let full = read_full_payload(
                                pager,
                                &data[local_start..local_start + local],
                                psize_u32 as usize,
                                ovfl,
                            )?;
                            owned_full = Some(full);
                            owned_full.as_deref().unwrap()
                        };
                        let mut one = [SqlValue::Null];
                        let n_fields = decode_record_sparse_fields_into_metered(
                            payload_slice,
                            encoding,
                            &[key_storage_idx],
                            &mut one,
                            meter.as_ref(),
                        )?;
                        let raw = if key_storage_idx < n_fields {
                            std::mem::replace(&mut one[0], SqlValue::Null)
                        } else {
                            key_default.clone()
                        };
                        if key_real_back {
                            crate::schema::real_read_back(raw)
                        } else {
                            raw
                        }
                    };

                    if heap.len() == cap
                        && cmp_order(&key, cur_seq, &heap[0].0, heap[0].1) != Ordering::Less
                    {
                        continue;
                    }

                    let (rid, _) = get_varint(data, rid_off);
                    let payload_slice: &[u8] = if let Some(ref f) = owned_full {
                        f.as_slice()
                    } else if !has_ovfl {
                        &data[local_start..local_start + local]
                    } else {
                        let ovfl = read_u32_be(data, local_start + local);
                        let full = read_full_payload(
                            pager,
                            &data[local_start..local_start + local],
                            psize_u32 as usize,
                            ovfl,
                        )?;
                        owned_full = Some(full);
                        owned_full.as_deref().unwrap()
                    };
                    let row = decode_projected_row(payload_slice, rid)?;

                    if heap.len() < cap {
                        if metered {
                            let new_b = estimate_direct_row_bytes(&row)
                                .saturating_add(estimate_direct_val_bytes(&key));
                            mem.reserve(new_b)?;
                        }
                        heap.push((key, cur_seq, row));
                        let mut i = heap.len() - 1;
                        while i > 0 {
                            let parent = (i - 1) >> 1;
                            if cmp_order(&heap[i].0, heap[i].1, &heap[parent].0, heap[parent].1)
                                == Ordering::Greater
                            {
                                heap.swap(i, parent);
                                i = parent;
                            } else {
                                break;
                            }
                        }
                        if heap.len() == cap {
                            worst_int = match heap[0].0 {
                                SqlValue::Int(w) => Some(w),
                                _ => None,
                            };
                        }
                    } else {
                        if metered {
                            let new_b = estimate_direct_row_bytes(&row)
                                .saturating_add(estimate_direct_val_bytes(&key));
                            let old_b = estimate_direct_row_bytes(&heap[0].2)
                                .saturating_add(estimate_direct_val_bytes(&heap[0].0));
                            if new_b > old_b {
                                mem.reserve(new_b - old_b)?;
                            } else {
                                mem.release(old_b - new_b);
                            }
                        }
                        heap[0] = (key, cur_seq, row);
                        let mut i = 0usize;
                        let n = heap.len();
                        loop {
                            let l = (i << 1) + 1;
                            if l >= n {
                                break;
                            }
                            let r = l + 1;
                            let mut worst = l;
                            if r < n
                                && cmp_order(&heap[r].0, heap[r].1, &heap[l].0, heap[l].1)
                                    == Ordering::Greater
                            {
                                worst = r;
                            }
                            if cmp_order(&heap[worst].0, heap[worst].1, &heap[i].0, heap[i].1)
                                == Ordering::Greater
                            {
                                heap.swap(i, worst);
                                i = worst;
                            } else {
                                break;
                            }
                        }
                        worst_int = match heap[0].0 {
                            SqlValue::Int(w) => Some(w),
                            _ => None,
                        };
                    }
                }
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_topk_scan_direct: invalid page type 0x{:02x} on page {pgno}",
                    data[hdr]
                )));
            }
        }
    }

    heap.sort_by(|a, b| cmp_order(&a.0, a.1, &b.0, b.1));
    let mut out = Vec::with_capacity(heap.len().saturating_sub(offset));
    for (_, _, row) in heap.into_iter().skip(offset).take(limit) {
        out.push(row);
    }
    Ok(out)
}

/// Output column mapping for [`DirectIndexEqMode::TableLookup`].
#[derive(Debug, Clone)]
pub enum DirectTableLookupSlot {
    Rowid,
    Field {
        want_pos: usize,
        storage_idx: usize,
        default_val: SqlValue,
        real_back: bool,
    },
}

/// Output projection mode for [`index_eq_scan_direct`].
#[derive(Debug)]
pub enum DirectIndexEqMode<'a> {
    /// All projected columns exist in the index record itself (zero table lookups).
    Covering(&'a [DirectIndexSlot]),
    /// Look up remaining columns from `table_root` using a pinned table-leaf cache.
    TableLookup {
        table_root: u32,
        rid_idx_pos: usize,
        want_fields: &'a [usize],
        slots: &'a [DirectTableLookupSlot],
    },
}

/// Direct-on-`WritePager` equality scan for `SELECT <cols> FROM tbl WHERE <indexed_col> = <val>`:
/// binary-searches `idx_root` for the first entry with leading key `>= lookup_key`, walks
/// contiguous matching entries (stopping immediately on the first non-matching key), and either
/// projects directly from a covering index or fetches needed table columns using a pinned
/// table-leaf cache to skip repeated root-to-leaf descents across same-leaf rowids!
#[allow(clippy::too_many_arguments)]
pub fn index_eq_scan_direct(
    pager: &WritePager,
    idx_root: u32,
    lookup_key: &SqlValue,
    lead_desc: bool,
    lead_coll: Option<&Collation>,
    encoding: TextEncoding,
    mode: DirectIndexEqMode<'_>,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    if lookup_key.is_null() {
        return Ok(Vec::new());
    }
    let usable = pager.usable_size();
    let limits = pager.payload_limits();
    let page_count = pager.page_count();
    let metered = !pager.is_noop_meter();
    let meter = pager.meter();

    let utf16_bin = if encoding != TextEncoding::Utf8 {
        Some(Collation::builtin(BuiltinCollation::Binary, encoding))
    } else {
        None
    };
    let eff_coll = lead_coll.or(utf16_bin.as_ref());

    // Compare `lookup_key` against leading field 0 of an encoded index record payload
    // in index B-tree order (`Ordering::Greater` => `lookup_key` comes AFTER cell in index order).
    let cmp_lead_payload = |payload: &[u8]| -> SqliteResult<Ordering> {
        if payload.is_empty() {
            return Err(SqliteError::corrupt("empty index record"));
        }
        let b0 = payload[0];
        let (hdr_len_u32, hpos) = if b0 < 0x80 {
            (u32::from(b0), 1usize)
        } else {
            get_varint32(payload, 0)
        };
        let hdr_len = hdr_len_u32 as usize;
        if hdr_len <= hpos || hdr_len > payload.len() {
            return Err(SqliteError::corrupt("index record header out of bounds"));
        }
        let hb = payload[hpos];
        let (st, n) = if hb < 0x80 {
            (u32::from(hb), 1usize)
        } else {
            get_varint32(payload, hpos)
        };
        if hpos + n > hdr_len {
            return Err(SqliteError::corrupt("index record header varint overrun"));
        }
        let blen = serial_type_length(st);
        let end = hdr_len
            .checked_add(blen)
            .ok_or_else(|| SqliteError::corrupt("index record body length overflow"))?;
        if end > payload.len() {
            return Err(SqliteError::corrupt("index record body truncated"));
        }
        let mut ord = match (lookup_key, st) {
            (SqlValue::Int(a), 8) => a.cmp(&0),
            (SqlValue::Int(a), 9) => a.cmp(&1),
            (SqlValue::Int(a), 1) => a.cmp(&i64::from(payload[hdr_len] as i8)),
            (SqlValue::Int(a), 2) => a.cmp(&i64::from(i16::from_be_bytes([
                payload[hdr_len],
                payload[hdr_len + 1],
            ]))),
            (SqlValue::Int(a), 3) => {
                let s = &payload[hdr_len..end];
                let v = ((i32::from(s[0] as i8)) << 16)
                    | (i32::from(s[1]) << 8)
                    | i32::from(s[2]);
                a.cmp(&i64::from(v))
            }
            (SqlValue::Int(a), 4) => a.cmp(&i64::from(i32::from_be_bytes([
                payload[hdr_len],
                payload[hdr_len + 1],
                payload[hdr_len + 2],
                payload[hdr_len + 3],
            ]))),
            (SqlValue::Int(a), 5) => {
                let s = &payload[hdr_len..end];
                let v = ((i64::from(s[0] as i8)) << 40)
                    | (i64::from(s[1]) << 32)
                    | (i64::from(s[2]) << 24)
                    | (i64::from(s[3]) << 16)
                    | (i64::from(s[4]) << 8)
                    | i64::from(s[5]);
                a.cmp(&v)
            }
            (SqlValue::Int(a), 6) => a.cmp(&i64::from_be_bytes([
                payload[hdr_len],
                payload[hdr_len + 1],
                payload[hdr_len + 2],
                payload[hdr_len + 3],
                payload[hdr_len + 4],
                payload[hdr_len + 5],
                payload[hdr_len + 6],
                payload[hdr_len + 7],
            ])),
            (SqlValue::Text(a_bytes), s)
                if s >= 13 && (s & 1) == 1 && encoding == TextEncoding::Utf8 =>
            {
                let b_bytes = &payload[hdr_len..end];
                match eff_coll {
                    Some(c) => c.compare_utf8(a_bytes, b_bytes),
                    None => a_bytes.as_ref().cmp(b_bytes),
                }
            }
            (SqlValue::Blob(a_bytes), s) if s >= 12 && (s & 1) == 0 => {
                a_bytes.as_ref().cmp(&payload[hdr_len..end])
            }
            _ => {
                let b_val = decode_serial_value(&payload[hdr_len..end], st, encoding);
                mem_compare(lookup_key, &b_val, eff_coll)
            }
        };
        if lead_desc {
            ord = ord.reverse();
        }
        Ok(ord)
    };

    let cmp_cell_lead = |data: &[u8],
                         pgno: u32,
                         is_leaf: bool,
                         ptr_off: usize,
                         ci: usize|
     -> SqliteResult<Ordering> {
        let bound = (usable as usize).min(data.len());
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
        let pos0 = pc + if is_leaf { 0 } else { 4 };
        if pos0 >= bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} out of bounds"
            )));
        }
        let (psize_u32, n1) = if data[pos0] < 0x80 {
            (u32::from(data[pos0]), 1usize)
        } else {
            get_payload_size_varint(data, pos0)
        };
        let local = if psize_u32 <= limits.max_local {
            psize_u32 as usize
        } else {
            local_payload_size(psize_u32, limits.max_local, limits.min_local, usable)
        };
        let local_start = pos0 + n1;
        let has_ovfl = psize_u32 > local as u32;
        let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
        if cell_end > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} runs past usable page end"
            )));
        }
        if !has_ovfl {
            cmp_lead_payload(&data[local_start..local_start + local])
        } else {
            let ovfl = read_u32_be(data, local_start + local);
            let full = read_full_payload(
                pager,
                &data[local_start..local_start + local],
                psize_u32 as usize,
                ovfl,
            )?;
            cmp_lead_payload(&full)
        }
    };

    let mut out: Vec<Vec<SqlValue>> = Vec::with_capacity(32);
    let mut mem = DirectScanMemGuard::new(metered.then_some(meter.as_ref()));
    // Pinned table leaf cache: `(leaf_pgno, min_rowid, max_rowid, last_hit_ci)`.
    let mut pinned_leaf: Option<(u32, i64, i64, usize)> = None;
    let mut stack_dec = [const { SqlValue::Null }; 32];
    let mut heap_dec: Vec<SqlValue> = Vec::new();
    let mut visited_count = 0u32;
    let mut visited = HashSet::<u32>::new();

    // Check cell at `(pgno, ci)`: returns `Ok(true)` if matched & emitted, `Ok(false)` to STOP scan!
    let mut emit_if_eq = |data: &[u8],
                          pgno: u32,
                          is_leaf: bool,
                          ptr_off: usize,
                          ci: usize,
                          out: &mut Vec<Vec<SqlValue>>,
                          mem: &mut DirectScanMemGuard<'_>|
     -> SqliteResult<bool> {
        if metered {
            meter.charge_steps(1)?;
        }
        let bound = (usable as usize).min(data.len());
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
        let pos0 = pc + if is_leaf { 0 } else { 4 };
        if pos0 >= bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} out of bounds"
            )));
        }
        let (psize_u32, n1) = if data[pos0] < 0x80 {
            (u32::from(data[pos0]), 1usize)
        } else {
            get_payload_size_varint(data, pos0)
        };
        let local = if psize_u32 <= limits.max_local {
            psize_u32 as usize
        } else {
            local_payload_size(psize_u32, limits.max_local, limits.min_local, usable)
        };
        let local_start = pos0 + n1;
        let has_ovfl = psize_u32 > local as u32;
        let cell_end = local_start + local + if has_ovfl { 4 } else { 0 };
        if cell_end > bound {
            return Err(SqliteError::corrupt(format!(
                "page {pgno}: index cell {ci} runs past usable page end"
            )));
        }
        let owned_full: Vec<u8>;
        let payload: &[u8] = if !has_ovfl {
            &data[local_start..local_start + local]
        } else {
            let ovfl = read_u32_be(data, local_start + local);
            owned_full = read_full_payload(
                pager,
                &data[local_start..local_start + local],
                psize_u32 as usize,
                ovfl,
            )?;
            owned_full.as_slice()
        };

        if cmp_lead_payload(payload)? != Ordering::Equal {
            return Ok(false);
        }

        match mode {
            DirectIndexEqMode::Covering(slots) => {
                let row = decode_direct_index_payload(payload, slots, encoding, meter.as_ref())?;
                if metered {
                    mem.reserve(estimate_direct_row_bytes(&row))?;
                }
                out.push(row);
            }
            DirectIndexEqMode::TableLookup {
                table_root,
                rid_idx_pos,
                want_fields,
                slots,
            } => {
                // Fast-path 2-int index record rowid extraction (skips sparse decoder loop!).
                let rid = if rid_idx_pos == 1 && payload.len() >= 3 && payload[0] == 3 {
                    let fast_rid = decode_int_st_fast(payload[1], payload, 3)
                        .and_then(|(_, l0)| decode_int_st_fast(payload[2], payload, 3 + l0))
                        .and_then(|(r, l1)| {
                            let l0 = serial_type_length(u32::from(payload[1]));
                            (3 + l0 + l1 == payload.len()).then_some(r)
                        });
                    if let Some(r) = fast_rid {
                        r
                    } else {
                        let mut one = [SqlValue::Null];
                        let nf = decode_record_sparse_fields_into_metered(
                            payload,
                            encoding,
                            &[rid_idx_pos],
                            &mut one,
                            meter.as_ref(),
                        )?;
                        if rid_idx_pos >= nf {
                            return Err(SqliteError::corrupt("index record missing rowid"));
                        }
                        let SqlValue::Int(r) = one[0] else {
                            return Err(SqliteError::corrupt("index record non-integer rowid"));
                        };
                        r
                    }
                } else {
                    let mut one = [SqlValue::Null];
                    let nf = decode_record_sparse_fields_into_metered(
                        payload,
                        encoding,
                        &[rid_idx_pos],
                        &mut one,
                        meter.as_ref(),
                    )?;
                    if rid_idx_pos >= nf {
                        return Err(SqliteError::corrupt("index record missing rowid"));
                    }
                    let SqlValue::Int(r) = one[0] else {
                        return Err(SqliteError::corrupt("index record non-integer rowid"));
                    };
                    r
                };

                // Look up `rid` on `table_root`, hitting `pinned_leaf` when on the same leaf page
                // and returning `ldata` directly so we never call `read_page(leaf_pgno)` twice!
                let (leaf_pgno, ldata, hit_ci) =
                    if let Some((lpg, min_r, max_r, last_ci)) = pinned_leaf {
                        if rid >= min_r && rid <= max_r {
                            let ldata = pager.read_page(lpg)?;
                            let lhdr = if lpg == 1 { HEADER_SIZE } else { 0 };
                            let lbound = (usable as usize).min(ldata.len());
                            let ln_cell = usize::from(read_u16_be(ldata, lhdr + 3));
                            let lptr = lhdr + 8;
                            // O(1) peek at `last_ci + 1` for adjacent rows on the same leaf.
                            let mut found_ci = None;
                            if last_ci + 1 < ln_cell {
                                let next_pc =
                                    usize::from(read_u16_be(ldata, lptr + 2 * (last_ci + 1)));
                                if next_pc < lbound {
                                    let (_, pn1) = if ldata[next_pc] < 0x80 {
                                        (u32::from(ldata[next_pc]), 1usize)
                                    } else {
                                        get_payload_size_varint(ldata, next_pc)
                                    };
                                    let (next_rid, _) = get_varint(ldata, next_pc + pn1);
                                    if next_rid == rid {
                                        found_ci = Some(last_ci + 1);
                                    }
                                }
                            }
                            let ci = match found_ci {
                                Some(c) => Some(c),
                                None => table_leaf_binary_search(ldata, lpg, usable, rid)?.ok(),
                            };
                            match ci {
                                Some(c) => {
                                    pinned_leaf = Some((lpg, min_r, max_r, c));
                                    (lpg, ldata, c)
                                }
                                None => return Ok(true),
                            }
                        } else {
                            let (lpg, _) = table_descend(pager, table_root, rid)?;
                            let ldata = pager.read_page(lpg)?;
                            let Ok(c) = table_leaf_binary_search(ldata, lpg, usable, rid)? else {
                                return Ok(true);
                            };
                            let lhdr = if lpg == 1 { HEADER_SIZE } else { 0 };
                            let lbound = (usable as usize).min(ldata.len());
                            let ln_cell = usize::from(read_u16_be(ldata, lhdr + 3));
                            let lptr = lhdr + 8;
                            let first_pc = usize::from(read_u16_be(ldata, lptr));
                            if first_pc >= lbound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {lpg}: leaf cell 0 offset out of bounds"
                                )));
                            }
                            let (_, fn1) = get_payload_size_varint(ldata, first_pc);
                            if first_pc + fn1 >= lbound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {lpg}: leaf cell 0 truncated before rowid"
                                )));
                            }
                            let (f_rid, _) = get_varint(ldata, first_pc + fn1);
                            let last_pc =
                                usize::from(read_u16_be(ldata, lptr + 2 * (ln_cell - 1)));
                            if last_pc >= lbound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {lpg}: last leaf cell offset out of bounds"
                                )));
                            }
                            let (_, ln1) = get_payload_size_varint(ldata, last_pc);
                            if last_pc + ln1 >= lbound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {lpg}: last leaf cell truncated before rowid"
                                )));
                            }
                            let (l_rid, _) = get_varint(ldata, last_pc + ln1);
                            pinned_leaf = Some((lpg, f_rid, l_rid, c));
                            (lpg, ldata, c)
                        }
                    } else {
                        let (lpg, _) = table_descend(pager, table_root, rid)?;
                        let ldata = pager.read_page(lpg)?;
                        let Ok(c) = table_leaf_binary_search(ldata, lpg, usable, rid)? else {
                            return Ok(true);
                        };
                        let lhdr = if lpg == 1 { HEADER_SIZE } else { 0 };
                        let lbound = (usable as usize).min(ldata.len());
                        let ln_cell = usize::from(read_u16_be(ldata, lhdr + 3));
                        let lptr = lhdr + 8;
                        let first_pc = usize::from(read_u16_be(ldata, lptr));
                        if first_pc >= lbound {
                            return Err(SqliteError::corrupt(format!(
                                "page {lpg}: leaf cell 0 offset out of bounds"
                            )));
                        }
                        let (_, fn1) = get_payload_size_varint(ldata, first_pc);
                        if first_pc + fn1 >= lbound {
                            return Err(SqliteError::corrupt(format!(
                                "page {lpg}: leaf cell 0 truncated before rowid"
                            )));
                        }
                        let (f_rid, _) = get_varint(ldata, first_pc + fn1);
                        let last_pc = usize::from(read_u16_be(ldata, lptr + 2 * (ln_cell - 1)));
                        if last_pc >= lbound {
                            return Err(SqliteError::corrupt(format!(
                                "page {lpg}: last leaf cell offset out of bounds"
                            )));
                        }
                        let (_, ln1) = get_payload_size_varint(ldata, last_pc);
                        if last_pc + ln1 >= lbound {
                            return Err(SqliteError::corrupt(format!(
                                "page {lpg}: last leaf cell truncated before rowid"
                            )));
                        }
                        let (l_rid, _) = get_varint(ldata, last_pc + ln1);
                        pinned_leaf = Some((lpg, f_rid, l_rid, c));
                        (lpg, ldata, c)
                    };

                let mut row = Vec::with_capacity(slots.len());
                if want_fields.is_empty() {
                    for _ in slots {
                        row.push(SqlValue::Int(rid));
                    }
                } else {
                    let lhdr = if leaf_pgno == 1 { HEADER_SIZE } else { 0 };
                    let lbound = (usable as usize).min(ldata.len());
                    let lpc = usize::from(read_u16_be(ldata, lhdr + 8 + 2 * hit_ci));
                    let (tpsize, tn1) = if lpc < lbound && ldata[lpc] < 0x80 {
                        (u32::from(ldata[lpc]), 1usize)
                    } else {
                        get_payload_size_varint(ldata, lpc)
                    };
                    let (_, tn2) = get_varint(ldata, lpc + tn1);
                    let tlocal = if tpsize <= limits.max_leaf {
                        tpsize as usize
                    } else {
                        local_payload_size(tpsize, limits.max_leaf, limits.min_leaf, usable)
                    };
                    let tlocal_start = lpc + tn1 + tn2;
                    let thas_ovfl = tpsize > tlocal as u32;
                    let tcell_end = tlocal_start + tlocal + if thas_ovfl { 4 } else { 0 };
                    if tcell_end > lbound {
                        return Err(SqliteError::corrupt(format!(
                            "page {leaf_pgno}: table leaf cell {hit_ci} runs past usable page end"
                        )));
                    }
                    let tfull: Vec<u8>;
                    let tpayload: &[u8] = if !thas_ovfl {
                        &ldata[tlocal_start..tlocal_start + tlocal]
                    } else {
                        let tovfl = read_u32_be(ldata, tlocal_start + tlocal);
                        tfull = read_full_payload(
                            pager,
                            &ldata[tlocal_start..tlocal_start + tlocal],
                            tpsize as usize,
                            tovfl,
                        )?;
                        tfull.as_slice()
                    };
                    let dec_slice: &mut [SqlValue] = if want_fields.len() <= 32 {
                        &mut stack_dec[..want_fields.len()]
                    } else {
                        if heap_dec.len() < want_fields.len() {
                            heap_dec.resize(want_fields.len(), SqlValue::Null);
                        }
                        &mut heap_dec[..want_fields.len()]
                    };
                    let n_fields = decode_record_sparse_fields_into_metered(
                        tpayload,
                        encoding,
                        want_fields,
                        dec_slice,
                        meter.as_ref(),
                    )?;
                    for sl in slots {
                        match sl {
                            DirectTableLookupSlot::Rowid => row.push(SqlValue::Int(rid)),
                            DirectTableLookupSlot::Field {
                                want_pos,
                                storage_idx,
                                default_val,
                                real_back,
                            } => {
                                let raw = if *storage_idx < n_fields {
                                    std::mem::replace(&mut dec_slice[*want_pos], SqlValue::Null)
                                } else {
                                    default_val.clone()
                                };
                                row.push(if *real_back {
                                    crate::schema::real_read_back(raw)
                                } else {
                                    raw
                                });
                            }
                        }
                    }
                }
                if metered {
                    mem.reserve(estimate_direct_row_bytes(&row))?;
                }
                out.push(row);
            }
        }
        Ok(true)
    };

    enum EqStep {
        Seek(u32, usize),
        ScanAll(u32, usize),
        InteriorFrom { pgno: u32, next_ci: usize, depth: usize },
    }
    let mut stack: Vec<EqStep> = Vec::with_capacity(BTREE_MAX_DEPTH);
    stack.push(EqStep::Seek(idx_root, 0));

    while let Some(step) = stack.pop() {
        match step {
            EqStep::InteriorFrom {
                pgno,
                next_ci,
                depth,
            } => {
                let data = pager.read_page(pgno)?;
                let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
                let bound = (usable as usize).min(data.len());
                let n_cell = usize::from(read_u16_be(data, hdr + 3));
                let ptr_off = hdr + 12;
                if next_ci >= n_cell {
                    continue;
                }
                if !emit_if_eq(data, pgno, false, ptr_off, next_ci, &mut out, &mut mem)? {
                    return Ok(out);
                }
                if next_ci + 1 < n_cell {
                    let next_pc = usize::from(read_u16_be(data, ptr_off + 2 * (next_ci + 1)));
                    if next_pc + 4 > bound {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: index interior cell {} out of bounds",
                            next_ci + 1
                        )));
                    }
                    let next_left = read_u32_be(data, next_pc);
                    stack.push(EqStep::InteriorFrom {
                        pgno,
                        next_ci: next_ci + 1,
                        depth,
                    });
                    stack.push(EqStep::ScanAll(next_left, depth + 1));
                } else {
                    let right_child = read_u32_be(data, hdr + 8);
                    stack.push(EqStep::ScanAll(right_child, depth + 1));
                }
            }
            EqStep::Seek(pgno, depth) => {
                visited_count = visited_count.saturating_add(1);
                if visited_count > page_count || !visited.insert(pgno) {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} contains a cycle"
                    )));
                }
                if depth >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} exceeds max depth"
                    )));
                }
                let data = pager.read_page(pgno)?;
                let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
                let bound = (usable as usize).min(data.len());
                if hdr + 8 > bound {
                    return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
                }
                match BtreePageType::from_byte(data[hdr]) {
                    Some(BtreePageType::IndexLeaf) => {
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let ptr_off = hdr + 8;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index leaf pointers out of bounds"
                            )));
                        }
                        let mut lo = 0usize;
                        let mut hi = n_cell;
                        while lo < hi {
                            let mid = lo + (hi - lo) / 2;
                            if cmp_cell_lead(data, pgno, true, ptr_off, mid)? == Ordering::Greater {
                                lo = mid + 1;
                            } else {
                                hi = mid;
                            }
                        }
                        for ci in lo..n_cell {
                            if !emit_if_eq(data, pgno, true, ptr_off, ci, &mut out, &mut mem)? {
                                return Ok(out);
                            }
                        }
                    }
                    Some(BtreePageType::IndexInterior) => {
                        if metered {
                            meter.charge_steps(1)?;
                        }
                        if hdr + 12 > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: truncated index interior header"
                            )));
                        }
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let ptr_off = hdr + 12;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index interior pointers out of bounds"
                            )));
                        }
                        let mut lo = 0usize;
                        let mut hi = n_cell;
                        while lo < hi {
                            let mid = lo + (hi - lo) / 2;
                            if cmp_cell_lead(data, pgno, false, ptr_off, mid)? == Ordering::Greater
                            {
                                lo = mid + 1;
                            } else {
                                hi = mid;
                            }
                        }
                        if lo < n_cell {
                            let pc = usize::from(read_u16_be(data, ptr_off + 2 * lo));
                            if pc + 4 > bound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {pgno}: index interior cell {lo} out of bounds"
                                )));
                            }
                            let left_child = read_u32_be(data, pc);
                            stack.push(EqStep::InteriorFrom {
                                pgno,
                                next_ci: lo,
                                depth,
                            });
                            stack.push(EqStep::Seek(left_child, depth + 1));
                        } else {
                            let right_child = read_u32_be(data, hdr + 8);
                            stack.push(EqStep::Seek(right_child, depth + 1));
                        }
                    }
                    _ => {
                        return Err(SqliteError::corrupt(format!(
                            "index_eq_scan_direct: invalid page type 0x{:02x} on page {pgno}",
                            data[hdr]
                        )));
                    }
                }
            }
            EqStep::ScanAll(pgno, depth) => {
                visited_count = visited_count.saturating_add(1);
                if visited_count > page_count || !visited.insert(pgno) {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} contains a cycle"
                    )));
                }
                if depth >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "index B-tree {idx_root} exceeds max depth"
                    )));
                }
                let data = pager.read_page(pgno)?;
                let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
                let bound = (usable as usize).min(data.len());
                if hdr + 8 > bound {
                    return Err(SqliteError::corrupt(format!("page {pgno}: truncated header")));
                }
                match BtreePageType::from_byte(data[hdr]) {
                    Some(BtreePageType::IndexLeaf) => {
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let ptr_off = hdr + 8;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index leaf pointers out of bounds"
                            )));
                        }
                        for ci in 0..n_cell {
                            if !emit_if_eq(data, pgno, true, ptr_off, ci, &mut out, &mut mem)? {
                                return Ok(out);
                            }
                        }
                    }
                    Some(BtreePageType::IndexInterior) => {
                        if metered {
                            meter.charge_steps(1)?;
                        }
                        if hdr + 12 > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: truncated index interior header"
                            )));
                        }
                        let n_cell = usize::from(read_u16_be(data, hdr + 3));
                        let ptr_off = hdr + 12;
                        if ptr_off + 2 * n_cell > bound {
                            return Err(SqliteError::corrupt(format!(
                                "page {pgno}: index interior pointers out of bounds"
                            )));
                        }
                        if n_cell > 0 {
                            let pc = usize::from(read_u16_be(data, ptr_off));
                            if pc + 4 > bound {
                                return Err(SqliteError::corrupt(format!(
                                    "page {pgno}: index interior cell 0 out of bounds"
                                )));
                            }
                            let left_child = read_u32_be(data, pc);
                            stack.push(EqStep::InteriorFrom {
                                pgno,
                                next_ci: 0,
                                depth,
                            });
                            stack.push(EqStep::ScanAll(left_child, depth + 1));
                        } else {
                            let right_child = read_u32_be(data, hdr + 8);
                            stack.push(EqStep::ScanAll(right_child, depth + 1));
                        }
                    }
                    _ => {
                        return Err(SqliteError::corrupt(format!(
                            "index_eq_scan_direct: invalid page type 0x{:02x} on page {pgno}",
                            data[hdr]
                        )));
                    }
                }
            }
        }
    }

    Ok(out)
}
