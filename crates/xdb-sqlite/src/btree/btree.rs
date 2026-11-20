//! High-level read-only B-tree API over a [`PageSource`]: header validation,
//! metered LRU pager, cursors, raw `sqlite_schema` scan, freelist, pointer maps,
//! and `dbstat`-equivalent per-page space statistics.

use std::collections::HashSet;
use std::sync::Arc;

use xdb_core::{Meter, SqlValue};

use super::cursor::{IndexCursor, TableCursor};
use super::pager::{Pager, DEFAULT_CACHE_BYTES};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    empty_database_info, is_ptrmap_page, overflow_page_count, ptrmap_entry_offset, ptrmap_page_for,
    read_u32_be, validate_database_header, BtreePageType, DatabaseInfo, PtrmapType,
};
use crate::page_source::PageSource;

/// Options for [`Btree::open`].
#[derive(Debug, Clone, Default)]
pub struct BtreeOptions {
    /// Page-cache capacity in pages (default ~2000 KiB worth, at least 16 pages).
    pub cache_pages: Option<usize>,
    /// Main file page count (`ceil(bytes / page_size)`) for `lockBtree`'s file-size check.
    pub file_page_count: Option<u32>,
    /// Optional sandbox resource meter.
    pub meter: Option<Arc<dyn Meter>>,
}

/// Walked freelist pages (trunks and leaves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freelist {
    pub trunks: Vec<u32>,
    pub leaves: Vec<u32>,
    pub total: usize,
}

/// One pointer-map entry (auto-vacuum databases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtrmapEntry {
    pub kind: PtrmapType,
    pub parent: u32,
}

/// One raw row of `sqlite_schema` (page 1).
#[derive(Debug, Clone)]
pub struct RawSchemaRow {
    pub rowid: i64,
    pub kind: SqlValue,
    pub name: SqlValue,
    pub tbl_name: SqlValue,
    pub rootpage: SqlValue,
    pub sql: SqlValue,
    pub column_count: usize,
}

/// Per-cell space stats (matching `dbstat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellStats {
    pub child: u32,
    pub payload_size: u32,
    pub local_size: usize,
    pub overflow_pages: Vec<u32>,
}

/// Per-page space stats (matching `dbstat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageStats {
    pub pgno: u32,
    pub page_type: BtreePageType,
    pub is_leaf: bool,
    pub is_table: bool,
    pub n_cell: usize,
    pub payload: usize,
    pub unused: usize,
    pub mx_payload: u32,
    pub right_child: u32,
    pub cells: Vec<CellStats>,
}

/// B-tree root kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtreeKind {
    Table,
    Index,
}

fn default_cache_pages(page_size: u32) -> usize {
    (DEFAULT_CACHE_BYTES / (page_size.max(512) as usize)).max(16)
}

/// Read-only B-tree database handle over any [`PageSource`].
#[derive(Debug)]
pub struct Btree {
    info: DatabaseInfo,
    pager: Pager,
}

impl Btree {
    /// Open a B-tree handle over `source`, validating page 1 as SQLite's `lockBtree` does.
    pub fn open(mut source: Box<dyn PageSource>, options: BtreeOptions) -> SqliteResult<Self> {
        let page_count = source.page_count();
        let source_ps = source.page_size();
        let cache_pages = options
            .cache_pages
            .unwrap_or_else(|| default_cache_pages(source_ps));

        if page_count == 0 {
            let info = empty_database_info(source_ps);
            let pager = Pager::new(source, info, cache_pages, options.meter);
            return Ok(Self { info, pager });
        }

        if let Some(ref meter) = options.meter {
            meter.charge_page_reads(1)?;
        }
        let page1 = source.read_page(1)?;
        let info = validate_database_header(&page1, page_count, source_ps, options.file_page_count)?;
        let pager = Pager::new(source, info, cache_pages, options.meter);
        pager.prime(1, page1)?;
        Ok(Self { info, pager })
    }

    #[inline]
    pub fn info(&self) -> &DatabaseInfo {
        &self.info
    }

    #[inline]
    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    #[inline]
    pub fn page_size(&self) -> u32 {
        self.info.page_size
    }

    #[inline]
    pub fn usable_size(&self) -> u32 {
        self.info.usable_size
    }

    #[inline]
    pub fn page_count(&self) -> u32 {
        self.info.page_count
    }

    pub fn table_cursor(&self, root_page: u32) -> SqliteResult<TableCursor> {
        TableCursor::new(self.pager.clone(), root_page)
    }

    pub fn index_cursor(&self, root_page: u32) -> SqliteResult<IndexCursor> {
        IndexCursor::new(self.pager.clone(), root_page)
    }

    pub fn btree_kind(&self, root_page: u32) -> SqliteResult<BtreeKind> {
        if root_page == 1 && self.info.page_count == 0 {
            return Ok(BtreeKind::Table);
        }
        let page = self.pager.get_btree_page(root_page, "b-tree root")?;
        Ok(if page.is_table {
            BtreeKind::Table
        } else {
            BtreeKind::Index
        })
    }

    /// Read every row of `sqlite_schema` (root page 1) in rowid order.
    pub fn schema_rows(&self) -> SqliteResult<Vec<RawSchemaRow>> {
        let mut rows = Vec::new();
        let mut cur = self.table_cursor(1)?;
        if cur.first()? {
            loop {
                let rowid = cur.rowid()?;
                let rec = cur.record()?;
                let kind = rec.value(0)?;
                let name = rec.value(1)?;
                let tbl_name = rec.value(2)?;
                let rootpage = rec.value(3)?;
                let sql = rec.value(4)?;
                let column_count = rec.column_count()?;
                rows.push(RawSchemaRow {
                    rowid,
                    kind,
                    name,
                    tbl_name,
                    rootpage,
                    sql,
                    column_count,
                });
                if !cur.next()? {
                    break;
                }
            }
        }
        Ok(rows)
    }

    /// Walk and validate the freelist from page 1's header (`btree.c checkList:10769`).
    pub fn freelist(&self) -> SqliteResult<Freelist> {
        let expected = self.info.freelist_count as usize;
        let mut trunks = Vec::new();
        let mut leaves = Vec::new();
        let mut seen = HashSet::new();
        let max_leaves = (self.info.usable_size as usize / 4).saturating_sub(2);
        let limit = expected.min(self.info.page_count as usize);
        let mut trunk = self.info.first_freelist_trunk;

        let mut check = |pgno: u32, what: &str| -> SqliteResult<()> {
            if pgno < 2 {
                return Err(SqliteError::corrupt(format!(
                    "freelist {what} page {pgno} is invalid"
                )));
            }
            self.pager.check_pgno(pgno, &format!("freelist {what}"))?;
            if !seen.insert(pgno) {
                return Err(SqliteError::corrupt(format!(
                    "freelist page {pgno} appears twice"
                )));
            }
            Ok(())
        };

        while trunk != 0 {
            if trunks.len() + leaves.len() >= limit {
                return Err(SqliteError::corrupt(format!(
                    "freelist is longer than the header's count {expected}"
                )));
            }
            check(trunk, "trunk")?;
            trunks.push(trunk);
            let data = self.pager.get_page(trunk, "freelist trunk")?;
            let n = read_u32_be(&data, 4) as usize;
            if n > max_leaves {
                return Err(SqliteError::corrupt(format!(
                    "freelist trunk {trunk} lists {n} leaves (max {max_leaves})"
                )));
            }
            if trunks.len() + leaves.len() + n > limit {
                return Err(SqliteError::corrupt(format!(
                    "freelist is longer than the header's count {expected}"
                )));
            }
            for i in 0..n {
                let leaf = read_u32_be(&data, 8 + 4 * i);
                check(leaf, "leaf")?;
                leaves.push(leaf);
            }
            trunk = read_u32_be(&data, 0);
        }

        let total = trunks.len() + leaves.len();
        if total != expected {
            return Err(SqliteError::corrupt(format!(
                "freelist holds {total} pages but the header says {expected}"
            )));
        }
        Ok(Freelist {
            trunks,
            leaves,
            total,
        })
    }

    pub fn is_ptrmap_page(&self, pgno: u32) -> bool {
        self.info.auto_vacuum && is_ptrmap_page(pgno, self.info.usable_size, self.info.page_size)
    }

    pub fn ptrmap_page_for(&self, pgno: u32) -> u32 {
        ptrmap_page_for(pgno, self.info.usable_size, self.info.page_size)
    }

    pub fn ptrmap_entry(&self, pgno: u32) -> SqliteResult<PtrmapEntry> {
        if !self.info.auto_vacuum {
            return Err(SqliteError::misuse(
                "not an auto-vacuum database: there are no pointer-map pages",
            ));
        }
        let map = self.ptrmap_page_for(pgno);
        let Some(offset) = ptrmap_entry_offset(map, pgno) else {
            return Err(SqliteError::corrupt(format!(
                "page {pgno} has no pointer-map entry"
            )));
        };
        if map == 0 {
            return Err(SqliteError::corrupt(format!(
                "page {pgno} has no pointer-map entry"
            )));
        }
        let data = self.pager.get_page(map, "pointer-map page")?;
        let raw_type = data[offset];
        let Some(kind) = PtrmapType::from_byte(raw_type) else {
            return Err(SqliteError::corrupt(format!(
                "pointer-map page {map}: entry for page {pgno} has type {raw_type}"
            )));
        };
        Ok(PtrmapEntry {
            kind,
            parent: read_u32_be(&data, offset + 1),
        })
    }

    /// `dbstat`-equivalent per-page space analysis.
    pub fn page_stats(&self, pgno: u32) -> SqliteResult<PageStats> {
        let page = self.pager.get_btree_page(pgno, "page_stats")?;
        let usable = self.info.usable_size;
        let mut cells = Vec::with_capacity(page.n_cell);
        let mut payload = 0usize;
        let mut mx_payload = 0u32;
        for i in 0..page.n_cell {
            let ci = page.check_cell(i)?;
            let mut chain = Vec::new();
            if ci.payload_size > ci.local_size as u32 {
                let n = overflow_page_count(ci.payload_size, ci.local_size, usable);
                if n > self.pager.page_count().saturating_sub(1) {
                    return Err(SqliteError::corrupt(format!(
                        "page {pgno}: cell {i} needs {n} overflow pages"
                    )));
                }
                let mut next = ci.overflow_page;
                for k in 0..n {
                    if next == 0 {
                        return Err(SqliteError::corrupt(format!(
                            "page {pgno}: cell {i} overflow chain ends after {k} of {n} pages"
                        )));
                    }
                    chain.push(next);
                    let data = self
                        .pager
                        .get_page(next, &format!("overflow chain of page {pgno}"))?;
                    next = read_u32_be(&data, 0);
                }
            }
            payload += ci.local_size;
            if ci.payload_size > mx_payload {
                mx_payload = ci.payload_size;
            }
            cells.push(CellStats {
                child: ci.child,
                payload_size: ci.payload_size,
                local_size: ci.local_size,
                overflow_pages: chain,
            });
        }
        Ok(PageStats {
            pgno,
            page_type: page.page_type,
            is_leaf: page.is_leaf,
            is_table: page.is_table,
            n_cell: page.n_cell,
            payload,
            unused: page.free_space()?,
            mx_payload,
            right_child: page.right_child,
            cells,
        })
    }
}
