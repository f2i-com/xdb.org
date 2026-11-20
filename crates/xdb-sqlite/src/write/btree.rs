//! Pure safe Rust SQLite 3 binary B-tree writer (`TableLeaf`/`TableInterior` and
//! `IndexLeaf`/`IndexInterior`) with exact overflow page chains, multi-level splitting,
//! interior deletion, freelist reclamation, and `PRAGMA integrity_check`.

use std::cmp::Ordering;
use std::collections::HashSet;

use xdb_core::value::mem_compare;
use xdb_core::SqlValue;

use crate::access::KeyInfo;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    decode_serial_value, get_varint, local_payload_size, lock_byte_page, payload_limits,
    read_u32_be, serial_type_length, BtreePageType, HEADER_SIZE,
};
use crate::format::page::read_u16_be;
use crate::write::pager::{init_empty_btree_page, WritePager};
use crate::write::record::put_varint;

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
    let limits = payload_limits(usable);
    let mut stack = vec![root_pgno];
    let mut to_free = Vec::new();

    while let Some(pgno) = stack.pop() {
        let data = pager.read_page(pgno)?.to_vec();
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
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
                let left_child = read_u32_be(&data, pos);
                if left_child != 0 {
                    stack.push(left_child);
                }
                pos += 4;
            }
            if is_table && !is_leaf {
                continue;
            }
            let (psize, n1) = get_varint(&data, pos);
            pos += n1;
            if is_table {
                let (_, n2) = get_varint(&data, pos);
                pos += n2;
            }
            let psize_u32 = psize as u32;
            let local = local_payload_size(psize_u32, max_l, min_l, usable);
            if psize_u32 > local as u32 {
                let ovfl = read_u32_be(&data, pos + local);
                free_overflow_chain(pager, ovfl)?;
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

fn free_overflow_chain(pager: &mut WritePager, mut pgno: u32) -> SqliteResult<()> {
    let mut steps = 0usize;
    let max_steps = pager.page_count() as usize + 1;
    while pgno != 0 {
        steps += 1;
        if steps > max_steps {
            return Err(SqliteError::corrupt("overflow page cycle"));
        }
        let next = {
            let p = pager.read_page(pgno)?;
            read_u32_be(p, 0)
        };
        pager.free_page(pgno)?;
        pgno = next;
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
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(local);
    let chunk_cap = (pager.usable_size() as usize) - 4;
    while out.len() < total_size {
        if ovfl_pgno == 0 {
            return Err(SqliteError::corrupt("truncated overflow chain"));
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
        debug_assert!(content_pos >= ptr_off + cells.len() * 2);
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

// ───────────────────────────── Table B-Tree (rowid -> payload) ─────────────────────────────

#[derive(Debug, Clone)]
struct TableLeafCell {
    rowid: i64,
    raw: Vec<u8>,
    overflow_pgno: u32,
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
    if data[hdr] != BtreePageType::TableLeaf as u8 {
        return Err(SqliteError::corrupt(format!(
            "expected TableLeaf on page {pgno}, got 0x{:02x}",
            data[hdr]
        )));
    }
    let limits = payload_limits(usable);
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let ptr_off = hdr + 8;
    let mut out = Vec::with_capacity(n_cell);
    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        let (psize, n1) = get_varint(data, pc);
        let (rowid_u64, n2) = get_varint(data, pc + n1);
        let local = local_payload_size(psize as u32, limits.max_leaf, limits.min_leaf, usable);
        let has_ovfl = (psize as u32) > (local as u32);
        let cell_end = pc + n1 + n2 + local + if has_ovfl { 4 } else { 0 };
        let overflow_pgno = if has_ovfl {
            read_u32_be(data, pc + n1 + n2 + local)
        } else {
            0
        };
        out.push(TableLeafCell {
            rowid: rowid_u64,
            raw: data[pc..cell_end].to_vec(),
            overflow_pgno,
        });
    }
    Ok(out)
}

fn parse_table_interior_cells(
    data: &[u8],
    pgno: u32,
) -> SqliteResult<(Vec<TableInteriorCell>, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    if data[hdr] != BtreePageType::TableInterior as u8 {
        return Err(SqliteError::corrupt(format!(
            "expected TableInterior on page {pgno}, got 0x{:02x}",
            data[hdr]
        )));
    }
    let n_cell = usize::from(read_u16_be(data, hdr + 3));
    let right_child = read_u32_be(data, hdr + 8);
    let ptr_off = hdr + 12;
    let mut out = Vec::with_capacity(n_cell);
    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        let left_child = read_u32_be(data, pc);
        let (key_u64, _) = get_varint(data, pc + 4);
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
    Ok(TableLeafCell {
        rowid,
        raw,
        overflow_pgno: ovfl,
    })
}

/// Find the maximum existing rowid in a table B-tree (`None` if empty).
pub fn table_max_rowid(pager: &WritePager, root_pgno: u32) -> SqliteResult<Option<i64>> {
    let usable = pager.usable_size();
    let mut pgno = root_pgno;
    loop {
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => {
                let cells = parse_table_leaf_cells(data, pgno, usable)?;
                return Ok(cells.last().map(|c| c.rowid));
            }
            Some(BtreePageType::TableInterior) => {
                let right_child = read_u32_be(data, hdr + 8);
                pgno = right_child;
            }
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "table_max_rowid: invalid page type on page {pgno}"
                )));
            }
        }
    }
}

/// Descend a table B-tree from `root_pgno` to the leaf responsible for `rowid`.
/// Returns `(leaf_pgno, path_of_(parent_pgno, child_slot))` where `child_slot == n_cell` means `right_child`.
fn table_descend(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
) -> SqliteResult<(u32, Vec<(u32, usize)>)> {
    let mut pgno = root_pgno;
    let mut path = Vec::new();
    loop {
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::TableLeaf) => return Ok((pgno, path)),
            Some(BtreePageType::TableInterior) => {
                let (cells, right_child) = parse_table_interior_cells(data, pgno)?;
                let slot = cells.partition_point(|c| c.key < rowid);
                let next = if slot < cells.len() {
                    cells[slot].left_child
                } else {
                    right_child
                };
                path.push((pgno, slot));
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

/// Look up whether `rowid` exists in a table B-tree.
pub fn table_contains_rowid(
    pager: &WritePager,
    root_pgno: u32,
    rowid: i64,
) -> SqliteResult<bool> {
    let (leaf_pgno, _) = table_descend(pager, root_pgno, rowid)?;
    let data = pager.read_page(leaf_pgno)?;
    let cells = parse_table_leaf_cells(data, leaf_pgno, pager.usable_size())?;
    Ok(cells.binary_search_by_key(&rowid, |c| c.rowid).is_ok())
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
    let mut cells = {
        let data = pager.read_page(leaf_pgno)?;
        parse_table_leaf_cells(data, leaf_pgno, usable)?
    };

    let replaced;
    match cells.binary_search_by_key(&rowid, |c| c.rowid) {
        Ok(idx) => {
            if !replace {
                return Err(SqliteError::Sql(format!(
                    "UNIQUE constraint failed: rowid {rowid}"
                )));
            }
            let old_ovfl = cells[idx].overflow_pgno;
            if old_ovfl != 0 {
                free_overflow_chain(pager, old_ovfl)?;
            }
            cells[idx] = build_table_leaf_cell(pager, rowid, payload)?;
            replaced = true;
        }
        Err(idx) => {
            let new_cell = build_table_leaf_cell(pager, rowid, payload)?;
            cells.insert(idx, new_cell);
            replaced = false;
        }
    }

    let raw_cells: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
    if cells_fit(leaf_pgno, true, usable as usize, &raw_cells) {
        pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
        return Ok(replaced);
    }

    // Leaf overflowed: split at ~half byte size!
    let split = choose_split_point(&raw_cells);
    let left_raw = &raw_cells[..split];
    let right_raw = &raw_cells[split..];
    let divider_key = cells[split - 1].rowid;

    if path.is_empty() {
        // Splitting the root leaf: allocate 2 new child leaves and turn root into TableInterior.
        let left_pg = pager.allocate_page()?;
        let right_pg = pager.allocate_page()?;
        pack_btree_page(pager, left_pg, BtreePageType::TableLeaf, 0, left_raw)?;
        pack_btree_page(pager, right_pg, BtreePageType::TableLeaf, 0, right_raw)?;
        let divider = TableInteriorCell {
            left_child: left_pg,
            key: divider_key,
        };
        pack_btree_page(
            pager,
            root_pgno,
            BtreePageType::TableInterior,
            right_pg,
            &[divider.encode()],
        )?;
        return Ok(replaced);
    }

    // Non-root leaf split: keep left half on `leaf_pgno`, put right half on `right_pg`.
    let right_pg = pager.allocate_page()?;
    pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, left_raw)?;
    pack_btree_page(pager, right_pg, BtreePageType::TableLeaf, 0, right_raw)?;

    table_insert_into_parent(pager, root_pgno, &path, leaf_pgno, divider_key, right_pg)?;
    Ok(replaced)
}

fn table_insert_into_parent(
    pager: &mut WritePager,
    root_pgno: u32,
    path: &[(u32, usize)],
    left_pg: u32,
    divider_key: i64,
    right_pg: u32,
) -> SqliteResult<()> {
    let (parent_pg, child_slot) = *path.last().expect("path non-empty");
    let usable = pager.usable_size() as usize;
    let (mut cells, mut right_child) = {
        let data = pager.read_page(parent_pg)?;
        parse_table_interior_cells(data, parent_pg)?
    };

    // Previously child at `child_slot` was `left_pg`. Now `left_pg` has keys <= `divider_key`
    // and `right_pg` has keys > `divider_key` (taking over the old slot!).
    if child_slot < cells.len() {
        cells[child_slot].left_child = right_pg;
        cells.insert(
            child_slot,
            TableInteriorCell {
                left_child: left_pg,
                key: divider_key,
            },
        );
    } else {
        cells.push(TableInteriorCell {
            left_child: left_pg,
            key: divider_key,
        });
        right_child = right_pg;
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
    table_insert_into_parent(
        pager,
        root_pgno,
        &path[..path.len() - 1],
        parent_pg,
        promoted_key,
        new_right,
    )
}

/// Delete `rowid` from a table B-tree. Returns `Ok(true)` if deleted, `Ok(false)` if absent.
pub fn table_delete(pager: &mut WritePager, root_pgno: u32, rowid: i64) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let (leaf_pgno, path) = table_descend(pager, root_pgno, rowid)?;
    let mut cells = {
        let data = pager.read_page(leaf_pgno)?;
        parse_table_leaf_cells(data, leaf_pgno, usable)?
    };
    let Ok(idx) = cells.binary_search_by_key(&rowid, |c| c.rowid) else {
        return Ok(false);
    };
    let removed = cells.remove(idx);
    if removed.overflow_pgno != 0 {
        free_overflow_chain(pager, removed.overflow_pgno)?;
    }

    if !cells.is_empty() || path.is_empty() {
        let raw_cells: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
        pack_btree_page(pager, leaf_pgno, BtreePageType::TableLeaf, 0, &raw_cells)?;
        return Ok(true);
    }

    // Non-root leaf became empty: free it and detach from parent!
    pager.free_page(leaf_pgno)?;
    table_remove_empty_child(pager, root_pgno, &path)?;
    Ok(true)
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
        // Non-root parent has 0 cells: replace parent's pointer in grandparent with `right_child` and free `parent_pg`!
        let (gp_pg, gp_slot) = path[path.len() - 2];
        let (mut gp_cells, mut gp_rc) = {
            let data = pager.read_page(gp_pg)?;
            parse_table_interior_cells(data, gp_pg)?
        };
        if gp_slot < gp_cells.len() {
            gp_cells[gp_slot].left_child = right_child;
        } else {
            gp_rc = right_child;
        }
        let raw: Vec<Vec<u8>> = gp_cells.iter().map(TableInteriorCell::encode).collect();
        pack_btree_page(pager, gp_pg, BtreePageType::TableInterior, gp_rc, &raw)?;
        pager.free_page(parent_pg)?;
        return Ok(());
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
            if cells.len() <= 1 {
                // Single cell always fits on page 1 since max_leaf already accounts for page size.
                let raw: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
                pack_btree_page(pager, root_pgno, BtreePageType::TableLeaf, 0, &raw)?;
                pager.free_page(child_pg)?;
                return Ok(());
            }
            let raw: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
            let split = choose_split_point(&raw);
            let right_pg = pager.allocate_page()?;
            pack_btree_page(pager, child_pg, BtreePageType::TableLeaf, 0, &raw[..split])?;
            pack_btree_page(pager, right_pg, BtreePageType::TableLeaf, 0, &raw[split..])?;
            let div = TableInteriorCell {
                left_child: child_pg,
                key: cells[split - 1].rowid,
            };
            pack_btree_page(
                pager,
                root_pgno,
                BtreePageType::TableInterior,
                right_pg,
                &[div.encode()],
            )
        }
        BtreePageType::TableInterior => {
            let (cells, rc) = parse_table_interior_cells(&data, child_pg)?;
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
    payload_size: usize,
    local_offset: usize,
    local_size: usize,
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

    fn full_payload(&self, pager: &WritePager) -> SqliteResult<Vec<u8>> {
        let local = &self.raw[self.local_offset..self.local_offset + self.local_size];
        read_full_payload(pager, local, self.payload_size, self.overflow_pgno)
    }
}

fn parse_index_cells(data: &[u8], pgno: u32, usable: u32) -> SqliteResult<(Vec<IndexCell>, u32)> {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
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
    let right_child = if is_leaf {
        0
    } else {
        read_u32_be(data, hdr + 8)
    };
    let ptr_off = hdr + if is_leaf { 8 } else { 12 };
    let mut out = Vec::with_capacity(n_cell);

    for i in 0..n_cell {
        let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
        let mut pos = pc;
        let left_child = if is_leaf {
            0
        } else {
            let lc = read_u32_be(data, pos);
            pos += 4;
            lc
        };
        let (psize, n1) = get_varint(data, pos);
        pos += n1;
        let psize_u32 = psize as u32;
        let local = local_payload_size(psize_u32, limits.max_local, limits.min_local, usable);
        let has_ovfl = psize_u32 > local as u32;
        let ovfl = if has_ovfl {
            read_u32_be(data, pos + local)
        } else {
            0
        };
        let cell_end = pos + local + if has_ovfl { 4 } else { 0 };
        out.push(IndexCell {
            left_child,
            raw: data[pc..cell_end].to_vec(),
            payload_size: psize as usize,
            local_offset: pos - pc,
            local_size: local,
            overflow_pgno: ovfl,
        });
    }
    Ok((out, right_child))
}

fn build_index_cell_body(pager: &mut WritePager, payload: &[u8]) -> SqliteResult<Vec<u8>> {
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
        return Ok(Vec::new());
    }
    let (hdr_len, mut hpos) = get_varint(payload, 0);
    let hdr_end = hdr_len as usize;
    if hdr_end > payload.len() {
        return Err(SqliteError::corrupt("index record header exceeds payload"));
    }
    let mut serials = Vec::new();
    while hpos < hdr_end {
        let (st, n) = get_varint(payload, hpos);
        serials.push(st as u32);
        hpos += n;
    }
    let mut boff = hdr_end;
    let mut vals = Vec::with_capacity(serials.len());
    for st in serials {
        let blen = serial_type_length(st);
        if boff + blen > payload.len() {
            return Err(SqliteError::corrupt("index record body truncated"));
        }
        vals.push(decode_serial_value(
            &payload[boff..boff + blen],
            st,
            key_info.encoding,
        ));
        boff += blen;
    }
    Ok(vals)
}

/// Compare two decoded index records using SQLite `KeyInfo` (collations & ASC/DESC).
pub fn compare_index_values(a: &[SqlValue], b: &[SqlValue], key_info: &KeyInfo) -> Ordering {
    let n = a.len().min(b.len()).min(key_info.n_all_field);
    for i in 0..n {
        let coll = key_info.collations.get(i).and_then(|c| c.as_ref());
        let mut ord = mem_compare(&a[i], &b[i], coll);
        if ord != Ordering::Equal {
            if key_info.desc.get(i).copied().unwrap_or(false) {
                ord = ord.reverse();
            }
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn compare_key_to_cell(
    pager: &WritePager,
    key_vals: &[SqlValue],
    cell: &IndexCell,
    key_info: &KeyInfo,
) -> SqliteResult<Ordering> {
    let cell_payload = cell.full_payload(pager)?;
    let cell_vals = decode_record_fields(&cell_payload, key_info)?;
    Ok(compare_index_values(key_vals, &cell_vals, key_info))
}

/// Search an index B-tree for `key_vals` (full key or prefix).
/// Returns:
/// - `Ok(Ok((pgno, cell_idx, path)))` if an exact match was found (may be leaf OR interior!)
/// - `Ok(Err((leaf_pgno, insert_idx, path)))` if absent (gives exact leaf insertion slot!)
#[allow(clippy::type_complexity)]
fn index_find(
    pager: &WritePager,
    root_pgno: u32,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<Result<(u32, usize, Vec<(u32, usize)>), (u32, usize, Vec<(u32, usize)>)>> {
    let usable = pager.usable_size();
    let mut pgno = root_pgno;
    let mut path = Vec::new();

    loop {
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
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
        let (cells, right_child) = parse_index_cells(data, pgno, usable)?;

        // Binary search cells on this page.
        let mut lo = 0usize;
        let mut hi = cells.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match compare_key_to_cell(pager, key_vals, &cells[mid], key_info)? {
                Ordering::Equal => return Ok(Ok((pgno, mid, path))),
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
        if is_leaf {
            return Ok(Err((pgno, lo, path)));
        }
        let child = if lo < cells.len() {
            cells[lo].left_child
        } else {
            right_child
        };
        path.push((pgno, lo));
        pgno = child;
    }
}

/// Check if an index B-tree already contains an entry matching the leading `prefix_vals`
/// (used for `UNIQUE` constraint checks on secondary indexes and `WITHOUT ROWID` PKs).
pub fn index_contains_prefix(
    pager: &WritePager,
    root_pgno: u32,
    prefix_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let usable = pager.usable_size();
    let mut pgno = root_pgno;
    loop {
        let data = pager.read_page(pgno)?;
        let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
        let is_leaf = match BtreePageType::from_byte(data[hdr]) {
            Some(BtreePageType::IndexLeaf) => true,
            Some(BtreePageType::IndexInterior) => false,
            _ => {
                return Err(SqliteError::corrupt(format!(
                    "index_contains_prefix: invalid page type on page {pgno}"
                )));
            }
        };
        let (cells, right_child) = parse_index_cells(data, pgno, usable)?;
        let mut lo = 0usize;
        let mut hi = cells.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let payload = cells[mid].full_payload(pager)?;
            let vals = decode_record_fields(&payload, key_info)?;
            let ord = compare_prefix(prefix_vals, &vals, key_info);
            match ord {
                Ordering::Equal => return Ok(Some(vals)),
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
        if is_leaf {
            return Ok(None);
        }
        pgno = if lo < cells.len() {
            cells[lo].left_child
        } else {
            right_child
        };
    }
}

fn compare_prefix(prefix: &[SqlValue], full: &[SqlValue], key_info: &KeyInfo) -> Ordering {
    let n = prefix.len().min(full.len());
    for i in 0..n {
        let coll = key_info.collations.get(i).and_then(|c| c.as_ref());
        let mut ord = mem_compare(&prefix[i], &full[i], coll);
        if ord != Ordering::Equal {
            if key_info.desc.get(i).copied().unwrap_or(false) {
                ord = ord.reverse();
            }
            return ord;
        }
    }
    Ordering::Equal
}

/// Insert (or replace if `replace == true`) a full index record `payload` into an index B-tree.
pub fn index_insert(
    pager: &mut WritePager,
    root_pgno: u32,
    payload: &[u8],
    key_info: &KeyInfo,
    replace: bool,
) -> SqliteResult<bool> {
    let key_vals = decode_record_fields(payload, key_info)?;
    let usable = pager.usable_size();

    match index_find(pager, root_pgno, &key_vals, key_info)? {
        Ok((hit_pgno, hit_idx, path)) => {
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
            let new_body = build_index_cell_body(pager, payload)?;
            let mut raw_cells: Vec<Vec<u8>> = cells.iter().map(|c| c.raw.clone()).collect();
            raw_cells[hit_idx] = if is_leaf {
                new_body
            } else {
                let mut v = Vec::with_capacity(4 + new_body.len());
                v.extend_from_slice(&cells[hit_idx].left_child.to_be_bytes());
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
            let new_body = build_index_cell_body(pager, payload)?;
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

/// Delete an exact entry `key_vals` from an index B-tree. Returns `Ok(true)` if deleted.
pub fn index_delete(
    pager: &mut WritePager,
    root_pgno: u32,
    key_vals: &[SqlValue],
    key_info: &KeyInfo,
) -> SqliteResult<bool> {
    let usable = pager.usable_size();
    let Ok((hit_pgno, hit_idx, path)) = index_find(pager, root_pgno, key_vals, key_info)? else {
        return Ok(false);
    };

    let data = pager.read_page(hit_pgno)?.to_vec();
    let hdr = if hit_pgno == 1 { HEADER_SIZE } else { 0 };
    let is_leaf = data[hdr] == BtreePageType::IndexLeaf as u8;

    if is_leaf {
        let (mut cells, _) = parse_index_cells(&data, hit_pgno, usable)?;
        let removed = cells.remove(hit_idx);
        if removed.overflow_pgno != 0 {
            free_overflow_chain(pager, removed.overflow_pgno)?;
        }
        if !cells.is_empty() || path.is_empty() {
            let raw: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
            pack_btree_page(pager, hit_pgno, BtreePageType::IndexLeaf, 0, &raw)?;
            return Ok(true);
        }
        pager.free_page(hit_pgno)?;
        index_remove_empty_child(pager, root_pgno, &path)?;
        return Ok(true);
    }

    // Deleting from an interior index page: replace cell `hit_idx` with the rightmost
    // cell in `cells[hit_idx].left_child` (its in-order predecessor on a leaf)!
    let (cells, right_child) = parse_index_cells(&data, hit_pgno, usable)?;
    let old_ovfl = cells[hit_idx].overflow_pgno;
    let left_sub_root = cells[hit_idx].left_child;

    // Descend to rightmost leaf of `left_sub_root`.
    let mut pred_path = path.clone();
    pred_path.push((hit_pgno, hit_idx));
    let mut cur_pg = left_sub_root;
    let pred_body: Vec<u8>;
    loop {
        let pdata = pager.read_page(cur_pg)?.to_vec();
        let phdr = if cur_pg == 1 { HEADER_SIZE } else { 0 };
        let p_is_leaf = pdata[phdr] == BtreePageType::IndexLeaf as u8;
        let (mut pcells, prc) = parse_index_cells(&pdata, cur_pg, usable)?;
        if p_is_leaf {
            let pred = pcells
                .pop()
                .ok_or_else(|| SqliteError::corrupt("empty non-root index leaf"))?;
            pred_body = pred.as_leaf_raw(true);
            // Save leaf state (if empty, we'll detach it AFTER installing pred into hit_pgno).
            if !pcells.is_empty() {
                let raw: Vec<Vec<u8>> = pcells.into_iter().map(|c| c.raw).collect();
                pack_btree_page(pager, cur_pg, BtreePageType::IndexLeaf, 0, &raw)?;
            }
            break;
        }
        let next_slot = pcells.len();
        pred_path.push((cur_pg, next_slot));
        cur_pg = prc;
    }

    // Free the deleted interior cell's overflow chain.
    if old_ovfl != 0 {
        free_overflow_chain(pager, old_ovfl)?;
    }

    // Install predecessor into `hit_pgno` at `hit_idx`, preserving `left_child`.
    let pred_leaf_became_empty = {
        let ldata = pager.read_page(cur_pg)?;
        let l_cells = parse_index_cells(ldata, cur_pg, usable)?.0;
        // If we didn't write pcells above because it became empty, l_cells still had 1 cell.
        l_cells.len() == 1 && l_cells[0].raw == pred_body
    };

    let mut raw_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| c.raw).collect();
    let mut replacement = Vec::with_capacity(4 + pred_body.len());
    replacement.extend_from_slice(&left_sub_root.to_be_bytes());
    replacement.extend_from_slice(&pred_body);
    raw_cells[hit_idx] = replacement;

    if cells_fit(hit_pgno, false, usable as usize, &raw_cells) {
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
    } else {
        // Rare case: predecessor cell is slightly wider than deleted cell and overflows interior page.
        // Detach empty leaf first (if needed) while path is valid, then split interior page!
        if pred_leaf_became_empty {
            // Re-pack `hit_pgno` temporarily? Better: split `hit_pgno` first, then re-find & remove if empty.
            // Actually, if `cur_pg` only had 1 cell (the predecessor), free it cleanly by replacing
            // the cell in `hit_pgno` ONLY after detaching `cur_pg`! Wait: detaching `cur_pg` might
            // change `hit_pgno` if `cur_pg == left_sub_root`!
            if cur_pg == left_sub_root {
                // `left_sub_root` was a 1-cell leaf right under `hit_idx`:
                // removing `hit_idx` AND freeing `cur_pg` simply deletes `hit_idx` from `hit_pgno`!
                // Wait — we ALREADY stole its single cell `pred_body` to replace `hit_idx`, so instead
                // `pred_body` shouldn't remain as a separator with an empty left_child!
                // Simply delete `hit_idx` from `hit_pgno` and move `pred_body` into right sibling's leftmost leaf!
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
                    // Collapse single remaining child into hit_pgno
                    if path.is_empty() {
                        let _ = try_collapse_child_into_root(pager, root_pgno, right_child)?;
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
                // Re-insert `pred_body`'s payload cleanly into the tree!
                let (psize, n1) = get_varint(&pred_body, 0);
                let limits = payload_limits(usable);
                let local = local_payload_size(psize as u32, limits.max_local, limits.min_local, usable);
                let ovfl = if (psize as u32) > local as u32 {
                    read_u32_be(&pred_body, n1 + local)
                } else {
                    0
                };
                let full_pred = read_full_payload(
                    pager,
                    &pred_body[n1..n1 + local],
                    psize as usize,
                    ovfl,
                )?;
                if ovfl != 0 {
                    free_overflow_chain(pager, ovfl)?;
                }
                let _ = index_insert(pager, root_pgno, &full_pred, key_info, true)?;
                return Ok(true);
            }
            pager.free_page(cur_pg)?;
            index_remove_empty_child(pager, root_pgno, &pred_path)?;
        }
        // Re-read hit_pgno in case detaching updated it, then split.
        index_split_interior(pager, root_pgno, hit_pgno, &path, raw_cells, right_child)?;
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

    // Update parent first (or collapse parent if it now has 0 cells).
    if cells.is_empty() {
        if path.len() == 1 {
            // Root now has only `sibling_pg`! Collapse `sibling_pg` into root!
            let _ = try_collapse_child_into_root(pager, root_pgno, sibling_pg)?;
            // Insert demoted key into the root subtree!
            let target_pg = root_pgno;
            return index_insert_raw_at_edge(pager, root_pgno, target_pg, demoted_leaf_raw, insert_at_front);
        }
        // Non-root parent has 0 cells: point grandparent directly at `sibling_pg` and free `parent_pg`!
        let (gp_pg, gp_slot) = path[path.len() - 2];
        let (mut gp_cells, mut gp_rc) = {
            let data = pager.read_page(gp_pg)?;
            parse_index_cells(data, gp_pg, usable)?
        };
        if gp_slot < gp_cells.len() {
            gp_cells[gp_slot].raw[0..4].copy_from_slice(&sibling_pg.to_be_bytes());
        } else {
            gp_rc = sibling_pg;
        }
        let gp_raw: Vec<Vec<u8>> = gp_cells.into_iter().map(|c| c.raw).collect();
        pack_btree_page(pager, gp_pg, BtreePageType::IndexInterior, gp_rc, &gp_raw)?;
        pager.free_page(parent_pg)?;
        return index_insert_raw_at_edge_with_prefix(
            pager,
            root_pgno,
            &path[..path.len() - 1],
            sibling_pg,
            demoted_leaf_raw,
            insert_at_front,
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
            path.push((cur, 0));
            cur = cells[0].left_child;
        } else {
            path.push((cur, cells.len()));
            cur = rc;
        }
    }
}

/// Choose a balanced split index `1..len` for TableLeaf cells (by byte weight).
fn choose_split_point(cells: &[Vec<u8>]) -> usize {
    let total: usize = cells.iter().map(|c| cell_storage_len(c) + 2).sum();
    let half = total / 2;
    let mut acc = 0usize;
    for (i, c) in cells.iter().enumerate() {
        acc += cell_storage_len(c) + 2;
        if acc >= half && i >= 1 && i < cells.len() - 1 {
            return i + 1;
        }
    }
    (cells.len() / 2).max(1)
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
    for &root in root_pages {
        if root == 0 {
            continue;
        }
        let mut stack = vec![(root, true)];
        while let Some((pgno, is_root)) = stack.pop() {
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
            let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
            let Some(ptype) = BtreePageType::from_byte(data[hdr]) else {
                errors.push(format!("page {pgno}: invalid type 0x{:02x}", data[hdr]));
                continue;
            };
            let n_cell = usize::from(read_u16_be(data, hdr + 3));
            if !is_root && n_cell == 0 {
                errors.push(format!("page {pgno}: non-root page has 0 cells"));
            }
            let is_leaf = ptype.is_leaf();
            let is_table = ptype.is_table();
            let ptr_off = hdr + if is_leaf { 8 } else { 12 };
            if !is_leaf {
                let rc = read_u32_be(data, hdr + 8);
                stack.push((rc, false));
            }
            let (max_l, min_l) = if is_table && is_leaf {
                (limits.max_leaf, limits.min_leaf)
            } else {
                (limits.max_local, limits.min_local)
            };
            let mut prev_rowid: Option<i64> = None;
            for i in 0..n_cell {
                let pc = usize::from(read_u16_be(data, ptr_off + 2 * i));
                let mut pos = pc;
                if !is_leaf {
                    stack.push((read_u32_be(data, pos), false));
                    pos += 4;
                }
                if is_table && !is_leaf {
                    let (rk, _) = get_varint(data, pos);
                    if let Some(prev) = prev_rowid {
                        if rk <= prev {
                            errors.push(format!("page {pgno}: unsorted table interior keys"));
                        }
                    }
                    prev_rowid = Some(rk);
                    continue;
                }
                let (psize, n1) = get_varint(data, pos);
                pos += n1;
                if is_table {
                    let (rk, n2) = get_varint(data, pos);
                    if let Some(prev) = prev_rowid {
                        if rk <= prev {
                            errors.push(format!("page {pgno}: unsorted table leaf rowids"));
                        }
                    }
                    prev_rowid = Some(rk);
                    pos += n2;
                }
                let local = local_payload_size(psize as u32, max_l, min_l, usable);
                if (psize as u32) > local as u32 {
                    let mut ovfl = read_u32_be(data, pos + local);
                    while ovfl != 0 {
                        if ovfl < 2 || ovfl > page_count {
                            errors.push(format!("page {pgno}: bad overflow page {ovfl}"));
                            break;
                        }
                        if !visited.insert(ovfl) {
                            errors.push(format!("overflow page {ovfl} referenced more than once"));
                            break;
                        }
                        ovfl = pager.read_page(ovfl).map(|p| read_u32_be(p, 0)).unwrap_or(0);
                    }
                }
            }
        }
    }

    // 2. Walk freelist
    let mut trunk = pager.freelist_trunk();
    let mut free_seen = 0u32;
    while trunk != 0 {
        if trunk < 2 || trunk > page_count {
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
        let next_trunk = read_u32_be(tpage, 0);
        let cnt = read_u32_be(tpage, 4);
        for i in 0..cnt {
            let off = 8 + (i as usize) * 4;
            let leaf = read_u32_be(tpage, off);
            if leaf < 2 || leaf > page_count {
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
