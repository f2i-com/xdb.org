//! SQL query execution bridge connecting [`SqliteDatabase`] to [`xdb_sql`].
//!
//! Provides:
//! - [`SqliteDatabase::query`], [`SqliteDatabase::query_with_params`], and
//!   [`SqliteDatabase::query_with_options`]
//! - Full [`SqlCatalog`] implementation exposing tables, views, `sqlite_schema` /
//!   `sqlite_master`, and pragma table-valued functions (`pragma_table_info`,
//!   `pragma_table_xinfo`, `pragma_index_list`, `pragma_index_info`, `pragma_index_xinfo`,
//!   `pragma_database_list`)
//! - Topological evaluation of `VIRTUAL` generated columns (with cycle detection,
//!   DQS support, pure-context enforcement, and column affinity)
//! - Rowid B-tree seek (`TableScanHint::RowidRange`) and secondary index B-tree seek
//!   (`TableScanHint::IndexEq`) pushdown

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use xdb_core::value::{mem_compare, BuiltinCollation, Collation, CollationRegistry, TextEncoding};
use xdb_core::{Affinity, Meter, SqlValue};
use xdb_parser::{expr_children, Expr, IndexOrigin, SelectStmt};
use xdb_sql::{
    eval_generated_expr, execute_query, execute_select_stmt, Bound, ColumnMeta, IndexMeta,
    QueryOptions, QueryParams, QueryResult, ScannedRow, ScopeColumn, SqlCatalog, SqlError,
    SqlResult, TableMeta, TableScanHint, TableValuedResult, ViewMeta,
};

use crate::access::SearchKey;
use crate::btree::{SeekOp, SeekResult};
use crate::database::{FastPeekVal, RowCursor, SqliteDatabase, TableReader};
use crate::errors::{SqliteError, SqliteResult};
use crate::schema::catalog::GeneratedKind;
use crate::schema::{ColumnHiddenKind, SchemaObject, TableInfo};

type RawRowList = Vec<ScannedRow>;

struct ScanMemGuard<'a> {
    meter: Option<&'a dyn Meter>,
    reserved: u64,
}

impl<'a> ScanMemGuard<'a> {
    fn new(meter: Option<&'a dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    fn reserve(&mut self, bytes: u64) -> SqlResult<()> {
        if bytes > 0 {
            if let Some(m) = self.meter {
                m.reserve_memory(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
            }
        }
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        if bytes > 0 {
            let rel = bytes.min(self.reserved);
            if rel > 0 {
                self.reserved -= rel;
                if let Some(m) = self.meter {
                    m.release_memory(rel);
                }
            }
        }
    }
}

impl Drop for ScanMemGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(m) = self.meter {
                m.release_memory(self.reserved);
            }
        }
    }
}

fn estimate_vals_bytes(vals: &[SqlValue]) -> u64 {
    let mut total = (vals.len() as u64).saturating_mul(24).saturating_add(32);
    for v in vals {
        match v {
            SqlValue::Text(b) | SqlValue::Blob(b) => {
                total = total.saturating_add(b.len() as u64);
            }
            _ => {}
        }
    }
    total
}

/// Map a [`SqliteError`] to an [`xdb_sql::SqlError`], preserving `ResourceLimit`.
fn map_sqlite_err(e: SqliteError) -> SqlError {
    match e {
        SqliteError::ResourceLimit(r) => SqlError::ResourceLimit(r),
        SqliteError::ObjectNotFound { message, .. } => SqlError::Runtime(message),
        SqliteError::UnknownCollation(c) => {
            SqlError::Runtime(format!("no such collation sequence: {c}"))
        }
        SqliteError::DatabaseClosed => SqlError::Runtime("the database is closed".to_string()),
        other => SqlError::Storage(other.to_string()),
    }
}

/// Catalog adapter binding a [`SqliteDatabase`] and active [`QueryOptions`] for `xdb-sql`.
pub(crate) struct SqliteSqlCatalog<'a> {
    db: &'a SqliteDatabase,
    options: &'a QueryOptions,
    in_table_list: AtomicBool,
    in_table_info: AtomicBool,
}

pub(crate) fn live_catalog_for<'a>(
    db: &'a SqliteDatabase,
    options: &'a QueryOptions,
) -> SqliteSqlCatalog<'a> {
    SqliteSqlCatalog::new(db, options)
}

impl<'a> SqliteSqlCatalog<'a> {
    fn new(db: &'a SqliteDatabase, options: &'a QueryOptions) -> Self {
        Self {
            db,
            options,
            in_table_list: AtomicBool::new(false),
            in_table_info: AtomicBool::new(false),
        }
    }
    fn check_schema(schema: Option<&str>) -> bool {
        match schema {
            None => true,
            Some(s) => s.eq_ignore_ascii_case("main"),
        }
    }

    fn build_table_meta(t: &TableInfo) -> TableMeta {
        let columns = t
            .columns
            .iter()
            .map(|c| ColumnMeta {
                name: c.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_generated: c.generated.is_some(),
                // Generated columns (VIRTUAL/STORED) are visible in `SELECT *` in SQLite!
                is_hidden: false,
            })
            .collect();
        let indexes = t
            .indexes
            .iter()
            .filter(|idx| !idx.without_rowid_primary_key)
            .map(|idx| {
                let key_columns = idx.columns[..idx.key_count]
                    .iter()
                    .map(|ic| if ic.cid >= 0 { Some(ic.cid as usize) } else { None })
                    .collect();
                let key_collations = idx.columns[..idx.key_count]
                    .iter()
                    .map(|ic| ic.collation.clone())
                    .collect();
                let key_desc = idx.columns[..idx.key_count]
                    .iter()
                    .map(|ic| ic.desc)
                    .collect();
                IndexMeta {
                    name: idx.name.clone(),
                    unique: idx.unique,
                    key_columns,
                    key_collations,
                    key_desc,
                    partial: idx.partial,
                    where_clause: idx.definition.where_clause.clone(),
                }
            })
            .collect();
        TableMeta {
            name: t.name.clone(),
            columns,
            has_rowid: !t.without_rowid,
            ipk_column: t.ipk_column,
            indexes,
        }
    }

    fn scan_with_virtuals(
        &self,
        reader: &TableReader,
        hint: &TableScanHint,
        needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>> {
        let info = reader.info();
        let total_cols = info.columns.len();

        // Determine whether any VIRTUAL column is actually needed by this query.
        let needed_has_virtual = info.has_virtual_columns
            && match needed_columns {
                None => true,
                Some(needed) => needed
                    .iter()
                    .any(|&cid| cid < total_cols && info.columns[cid].hidden == ColumnHiddenKind::Virtual),
            };

        let virt_plan = if needed_has_virtual {
            Some(VirtualColumnPlan::build_for(info, needed_columns)?)
        } else {
            None
        };

        // Choose which stored columns to decode:
        // - If VIRTUAL columns are evaluated, decode all stored columns (to satisfy any
        //   transitive column dependencies inside generated expressions).
        // - Otherwise, if `needed_columns` is provided, decode ONLY the requested stored columns!
        let (stored_cids, full_stored_identity): (Vec<usize>, bool) = if !needed_has_virtual {
            if let Some(needed) = needed_columns {
                let mut cids = Vec::with_capacity(needed.len());
                for &cid in needed {
                    if cid < total_cols && info.columns[cid].hidden != ColumnHiddenKind::Virtual {
                        cids.push(cid);
                    }
                }
                let is_identity =
                    cids.len() == total_cols && cids.iter().enumerate().all(|(i, &c)| i == c);
                (cids, is_identity)
            } else {
                let cids: Vec<usize> = info
                    .columns
                    .iter()
                    .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
                    .map(|c| c.cid)
                    .collect();
                let is_identity = !info.has_virtual_columns && cids.len() == total_cols;
                (cids, is_identity)
            }
        } else {
            let cids = info
                .columns
                .iter()
                .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
                .map(|c| c.cid)
                .collect();
            (cids, false)
        };

        let sparse = if full_stored_identity {
            None
        } else {
            Some((total_cols, stored_cids.as_slice()))
        };

        let mut rows = self.collect_raw_rows(reader, hint, &stored_cids, sparse, meter)?;

        // Fast single-pass return when no VIRTUAL generated columns need evaluation:
        // `collect_raw_rows` already decoded every column directly into its final slot position!
        let Some(ref plan) = virt_plan else {
            return Ok(rows);
        };

        let mut mem = ScanMemGuard::new(meter);
        for srow in &mut rows {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let prev_bytes = estimate_vals_bytes(&srow.values);
            plan.evaluate(&mut srow.values, self.options)?;
            let new_bytes = estimate_vals_bytes(&srow.values);
            if new_bytes > prev_bytes {
                mem.reserve(new_bytes - prev_bytes)?;
            }
        }
        Ok(rows)
    }

    #[allow(clippy::type_complexity)]
    fn visit_with_virtuals(
        &self,
        reader: &TableReader,
        hint: &TableScanHint,
        needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
        visit: &mut dyn FnMut(Option<i64>, &[SqlValue]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        let info = reader.info();
        let total_cols = info.columns.len();
        let needed_has_virtual = info.has_virtual_columns
            && match needed_columns {
                None => true,
                Some(needed) => needed
                    .iter()
                    .any(|&cid| cid < total_cols && info.columns[cid].hidden == ColumnHiddenKind::Virtual),
            };
        if needed_has_virtual {
            let rows = self.scan_with_virtuals(reader, hint, needed_columns, meter)?;
            for row in rows {
                if !visit(row.rowid, &row.values)? {
                    break;
                }
            }
            return Ok(());
        }

        let stored_cids: Vec<usize> = if let Some(needed) = needed_columns {
            let mut cids = Vec::with_capacity(needed.len());
            for &cid in needed {
                if cid < total_cols && info.columns[cid].hidden != ColumnHiddenKind::Virtual {
                    cids.push(cid);
                }
            }
            cids
        } else {
            info.columns
                .iter()
                .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
                .map(|c| c.cid)
                .collect()
        };

        let has_rowid = !info.without_rowid;
        let mut buf = vec![SqlValue::Null; total_cols];

        match hint {
            TableScanHint::FullScan => {
                let mut cur = reader.cursor_cids(&stored_cids).map_err(map_sqlite_err)?;
                cur.visit_sparse_range(
                    &mut buf,
                    &stored_cids,
                    None,
                    None,
                    meter,
                    map_sqlite_err,
                    SqlError::ResourceLimit,
                    visit,
                )?;
                return Ok(());
            }
            TableScanHint::RowidRange { low, high } if has_rowid => {
                let lo_opt: Option<Option<i64>> = match low {
                    Bound::Unbounded => Some(None),
                    Bound::Included(v) => bound_to_int(v).map(Some),
                    Bound::Excluded(v) => match bound_to_int(v) {
                        Some(i) => match i.checked_add(1) {
                            Some(n) => Some(Some(n)),
                            None => return Ok(()),
                        },
                        None => None,
                    },
                };
                let hi_opt: Option<Option<i64>> = match high {
                    Bound::Unbounded => Some(None),
                    Bound::Included(v) => bound_to_int(v).map(Some),
                    Bound::Excluded(v) => match bound_to_int(v) {
                        Some(i) => match i.checked_sub(1) {
                            Some(n) => Some(Some(n)),
                            None => return Ok(()),
                        },
                        None => None,
                    },
                };
                if let (Some(lo_i64), Some(hi_i64)) = (lo_opt, hi_opt) {
                    if let (Some(lo), Some(hi)) = (lo_i64, hi_i64) {
                        if lo > hi {
                            return Ok(());
                        }
                        if lo == hi {
                            if let Some(m) = meter {
                                m.charge_steps(1)?;
                            }
                            let mut rcur =
                                reader.cursor_cids(&stored_cids).map_err(map_sqlite_err)?;
                            if rcur.seek(lo).map_err(map_sqlite_err)? == SeekResult::Exact {
                                rcur.decode_into_sparse_buf(&mut buf, &stored_cids)
                                    .map_err(map_sqlite_err)?;
                                let _ = visit(Some(lo), &buf)?;
                            }
                            return Ok(());
                        }
                    }
                    let mut rcur = reader.cursor_cids(&stored_cids).map_err(map_sqlite_err)?;
                    rcur.visit_sparse_range(
                        &mut buf,
                        &stored_cids,
                        lo_i64,
                        hi_i64,
                        meter,
                        map_sqlite_err,
                        SqlError::ResourceLimit,
                        visit,
                    )?;
                    return Ok(());
                }
            }
            TableScanHint::IndexEq { index_name, values } if has_rowid => {
                if let Ok(idx_reader) = self.db.index(index_name) {
                    let idx_info = idx_reader.info();
                    if !idx_info.without_rowid_primary_key && !idx_info.partial && !values.is_empty() {
                        if let Ok(key_info) = idx_reader.key_info(Some(&self.options.collations)) {
                            if let Ok(mut search_key) = SearchKey::new(key_info.clone(), values, 1) {
                                let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
                                let width = icur.width();
                                if width > values.len() {
                                    let rid_col = width - 1;
                                    let binary_fallback = (key_info.encoding != TextEncoding::Utf8)
                                        .then(|| {
                                            Collation::builtin(
                                                BuiltinCollation::Binary,
                                                key_info.encoding,
                                            )
                                        });
                                    if !icur
                                        .seek_key(&mut search_key, SeekOp::Ge)
                                        .map_err(map_sqlite_err)?
                                    {
                                        return Ok(());
                                    }
                                    let mut rcur = reader
                                        .cursor_cids(&stored_cids)
                                        .map_err(map_sqlite_err)?;
                                    let mut fell_back = false;
                                    let mut visited_any = false;
                                    loop {
                                        if let Some(m) = meter {
                                            m.charge_steps(1)?;
                                        }
                                        let mut matches = true;
                                        for (k, target) in values.iter().enumerate() {
                                            let kv = icur.value(k).map_err(map_sqlite_err)?;
                                            let coll = key_info.collations[k]
                                                .as_ref()
                                                .or(binary_fallback.as_ref());
                                            if mem_compare(&kv, target, coll) != Ordering::Equal {
                                                matches = false;
                                                break;
                                            }
                                        }
                                        if !matches {
                                            break;
                                        }
                                        let rid_val =
                                            icur.value(rid_col).map_err(map_sqlite_err)?;
                                        let Some(rid) = rid_val.as_i64() else {
                                            if visited_any {
                                                return Err(SqlError::storage(
                                                    "non-integer rowid in secondary index",
                                                ));
                                            }
                                            fell_back = true;
                                            break;
                                        };
                                        if rcur.seek(rid).map_err(map_sqlite_err)?
                                            == SeekResult::Exact
                                        {
                                            rcur.decode_into_sparse_buf(&mut buf, &stored_cids)
                                                .map_err(map_sqlite_err)?;
                                            visited_any = true;
                                            if !visit(Some(rid), &buf)? {
                                                break;
                                            }
                                        }
                                        if !icur.next().map_err(map_sqlite_err)? {
                                            break;
                                        }
                                    }
                                    if !fell_back {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        // Fallback for remaining hints (RowidIn, IndexRange, IndexIn, RowidOrder, IndexOrder, TopK)
        let rows = self.scan_with_virtuals(reader, hint, needed_columns, meter)?;
        for row in rows {
            if !visit(row.rowid, &row.values)? {
                break;
            }
        }
        Ok(())
    }

    #[inline]
    fn decode_cursor_row(
        cur: &mut RowCursor,
        sparse: Option<(usize, &[usize])>,
    ) -> SqlResult<Vec<SqlValue>> {
        match sparse {
            None => cur.row().map_err(map_sqlite_err),
            Some((total_cols, cids)) => cur
                .row_into_sparse(total_cols, cids)
                .map_err(map_sqlite_err),
        }
    }

    fn collect_raw_rows(
        &self,
        reader: &TableReader,
        hint: &TableScanHint,
        stored_cids: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<RawRowList> {
        let info = reader.info();
        let has_rowid = !info.without_rowid;

        match hint {
            TableScanHint::RowidRange { low, high } if has_rowid => {
                if let Some(rows) =
                    self.try_rowid_range_scan(reader, low, high, stored_cids, sparse, meter)?
                {
                    return Ok(rows);
                }
            }
            TableScanHint::RowidIn { values } if has_rowid => {
                return self.try_rowid_in_scan(reader, values, stored_cids, sparse, meter);
            }
            TableScanHint::IndexEq { index_name, values } if has_rowid => {
                if let Some(rows) =
                    self.try_index_eq_scan(reader, index_name, values, stored_cids, sparse, meter)?
                {
                    return Ok(rows);
                }
            }
            TableScanHint::IndexRange {
                index_name,
                low,
                high,
            } if has_rowid => {
                if let Some(rows) = self.try_index_range_scan(
                    reader, index_name, low, high, stored_cids, sparse, meter,
                )? {
                    return Ok(rows);
                }
            }
            TableScanHint::IndexIn { index_name, values } if has_rowid => {
                if let Some(rows) =
                    self.try_index_in_scan(reader, index_name, values, stored_cids, sparse, meter)?
                {
                    return Ok(rows);
                }
            }
            TableScanHint::RowidOrder { desc, limit } if has_rowid || !*desc => {
                return self.try_rowid_order_scan(reader, *desc, *limit, stored_cids, sparse, meter);
            }
            TableScanHint::IndexOrder {
                index_name,
                desc,
                limit,
            } if has_rowid => {
                if let Some(rows) = self.try_index_order_scan(
                    reader, index_name, *desc, *limit, stored_cids, sparse, meter,
                )? {
                    return Ok(rows);
                }
            }
            TableScanHint::TopK {
                col_idx,
                desc,
                nulls_first,
                collation,
                limit,
            } => {
                if let Some(rows) = self.try_topk_scan(
                    reader,
                    *col_idx,
                    *desc,
                    *nulls_first,
                    collation,
                    *limit,
                    stored_cids,
                    sparse,
                    meter,
                )? {
                    return Ok(rows);
                }
            }
            _ => {}
        }

        // Default full B-tree scan (`rowid: false` + direct `cur.rowid()` avoids `r.remove(0)`!)
        let mut cur = reader.cursor_cids(stored_cids).map_err(map_sqlite_err)?;
        let mut rows = Vec::new();
        let mut mem = ScanMemGuard::new(meter);
        while cur.next().map_err(map_sqlite_err)? {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let rid = if has_rowid {
                Some(cur.rowid().map_err(map_sqlite_err)?)
            } else {
                None
            };
            let r = Self::decode_cursor_row(&mut cur, sparse)?;
            if meter.is_some() {
                mem.reserve(estimate_vals_bytes(&r))?;
            }
            rows.push(ScannedRow {
                rowid: rid,
                values: r,
            });
        }
        Ok(rows)
    }

    fn try_rowid_order_scan(
        &self,
        reader: &TableReader,
        desc: bool,
        limit: usize,
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<RawRowList> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let has_rowid = !reader.info().without_rowid;
        let mut cur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let mut rows = Vec::with_capacity(limit.min(1024));
        let mut mem = ScanMemGuard::new(meter);
        let positioned = if desc {
            cur.last().map_err(map_sqlite_err)?
        } else {
            cur.next().map_err(map_sqlite_err)?
        };
        if !positioned {
            return Ok(rows);
        }
        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let rid = if has_rowid {
                Some(cur.rowid().map_err(map_sqlite_err)?)
            } else {
                None
            };
            let vals = Self::decode_cursor_row(&mut cur, sparse)?;
            if meter.is_some() {
                mem.reserve(estimate_vals_bytes(&vals))?;
            }
            rows.push(ScannedRow {
                rowid: rid,
                values: vals,
            });
            if rows.len() >= limit {
                break;
            }
            let more = if desc {
                cur.prev().map_err(map_sqlite_err)?
            } else {
                cur.next().map_err(map_sqlite_err)?
            };
            if !more {
                break;
            }
        }
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_index_order_scan(
        &self,
        reader: &TableReader,
        index_name: &str,
        desc: bool,
        limit: usize,
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        if limit == 0 {
            return Ok(Some(Vec::new()));
        }
        let Ok(idx_reader) = self.db.index(index_name) else {
            return Ok(None);
        };
        let idx_info = idx_reader.info();
        if idx_info.without_rowid_primary_key || idx_info.partial || idx_info.key_count == 0 {
            return Ok(None);
        }
        let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
        let width = icur.width();
        if width < 2 {
            return Ok(None);
        }
        let rid_col = width - 1;
        let positioned = if desc {
            icur.last().map_err(map_sqlite_err)?
        } else {
            icur.next().map_err(map_sqlite_err)?
        };
        if !positioned {
            return Ok(Some(Vec::new()));
        }
        let mut rows = Vec::with_capacity(limit.min(1024));
        let mut mem = ScanMemGuard::new(meter);
        // Covering-index check: if every requested column in `scan_opts` is present in
        // `idx_info.columns` (or is the table's IPK = `rid_col`), emit rows directly from
        // the index cursor with ZERO table-B-tree seeks!
        let tbl_info = reader.info();
        let mut cover_map: Vec<(usize, bool)> = Vec::with_capacity(scan_opts.len());
        let mut is_covering = !tbl_info.without_rowid;
        if is_covering {
            for &cid in scan_opts {
                if Some(cid) == tbl_info.ipk_column {
                    cover_map.push((rid_col, false));
                } else if let Some(pos) = idx_info
                    .columns
                    .iter()
                    .take(idx_info.key_count)
                    .position(|ic| ic.cid == cid as i32)
                {
                    let real_back = tbl_info
                        .columns
                        .get(cid)
                        .is_some_and(|c| c.affinity == Affinity::Real);
                    cover_map.push((pos, real_back));
                } else {
                    is_covering = false;
                    break;
                }
            }
        }
        if is_covering {
            loop {
                if let Some(m) = meter {
                    m.charge_steps(1)?;
                }
                let rid_val = icur.value(rid_col).map_err(map_sqlite_err)?;
                let Some(rid) = rid_val.as_i64() else {
                    return Ok(None);
                };
                let vals = if let Some((full_width, cids)) = sparse {
                    let mut v = vec![SqlValue::Null; full_width];
                    for (&cid, &(ipos, real_back)) in cids.iter().zip(&cover_map) {
                        let raw = if ipos == rid_col {
                            SqlValue::Int(rid)
                        } else {
                            icur.value(ipos).map_err(map_sqlite_err)?
                        };
                        v[cid] = if real_back {
                            crate::schema::real_read_back(raw)
                        } else {
                            raw
                        };
                    }
                    v
                } else {
                    let mut v = Vec::with_capacity(cover_map.len());
                    for &(ipos, real_back) in &cover_map {
                        let raw = if ipos == rid_col {
                            SqlValue::Int(rid)
                        } else {
                            icur.value(ipos).map_err(map_sqlite_err)?
                        };
                        v.push(if real_back {
                            crate::schema::real_read_back(raw)
                        } else {
                            raw
                        });
                    }
                    v
                };
                if meter.is_some() {
                    mem.reserve(estimate_vals_bytes(&vals))?;
                }
                rows.push(ScannedRow {
                    rowid: Some(rid),
                    values: vals,
                });
                if rows.len() >= limit {
                    break;
                }
                let more = if desc {
                    icur.prev().map_err(map_sqlite_err)?
                } else {
                    icur.next().map_err(map_sqlite_err)?
                };
                if !more {
                    break;
                }
            }
            return Ok(Some(rows));
        }

        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let rid_val = icur.value(rid_col).map_err(map_sqlite_err)?;
            let Some(rid) = rid_val.as_i64() else {
                return Ok(None);
            };
            if rcur.seek(rid).map_err(map_sqlite_err)? == SeekResult::Exact {
                let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                if meter.is_some() {
                    mem.reserve(estimate_vals_bytes(&vals))?;
                }
                rows.push(ScannedRow {
                    rowid: Some(rid),
                    values: vals,
                });
                if rows.len() >= limit {
                    break;
                }
            }
            let more = if desc {
                icur.prev().map_err(map_sqlite_err)?
            } else {
                icur.next().map_err(map_sqlite_err)?
            };
            if !more {
                break;
            }
        }
        Ok(Some(rows))
    }

    #[allow(clippy::too_many_arguments)]
    fn try_topk_scan(
        &self,
        reader: &TableReader,
        col_idx: usize,
        desc: bool,
        nulls_first: bool,
        collation: &str,
        limit: usize,
        stored_cids: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        if limit == 0 {
            return Ok(Some(Vec::new()));
        }
        let Some(coll) = self.options.collations.find(collation) else {
            return Ok(None);
        };
        let Some(key_slot) = stored_cids.iter().position(|&c| c == col_idx) else {
            return Ok(None);
        };
        let has_rowid = !reader.info().without_rowid;
        let mut cur = reader.cursor_cids(stored_cids).map_err(map_sqlite_err)?;

        // Comparator returning `Ordering::Less` if `a` is BETTER (comes earlier in desired ORDER BY)
        // than `b`, and `Ordering::Greater` if `a` is WORSE than `b`.
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
            let mut o = mem_compare(ka, kb, Some(coll));
            if desc {
                o = o.reverse();
            }
            o.then_with(|| sa.cmp(&sb))
        };

        // Bounded max-heap of size `limit`: `heap[0]` is always the WORST kept row.
        let mut mem = ScanMemGuard::new(meter);
        let mut heap: Vec<(SqlValue, u64, ScannedRow)> = Vec::with_capacity(limit);
        let mut worst_int: Option<i64> = None;
        let mut seq = 0u64;
        while let Some(peeked) = cur.next_peek_key_fast(key_slot).map_err(map_sqlite_err)? {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let cur_seq = seq;
            seq = seq.wrapping_add(1);
            // Fast register rejection when heap is full and worst item is an Int:
            // since `cur_seq > heap[0].1`, ties are strictly worse than `heap[0]` too!
            if let (Some(w), FastPeekVal::Int(k)) = (worst_int, &peeked) {
                if if desc { *k <= w } else { *k >= w } {
                    continue;
                }
            }
            let key = match peeked {
                FastPeekVal::Int(k) => SqlValue::Int(k),
                FastPeekVal::Other(v) => v,
            };

            if heap.len() < limit {
                let rid = if has_rowid {
                    Some(cur.rowid().map_err(map_sqlite_err)?)
                } else {
                    None
                };
                let vals = Self::decode_cursor_row(&mut cur, sparse)?;
                if meter.is_some() {
                    let key_extra = match &key {
                        SqlValue::Text(b) | SqlValue::Blob(b) => b.len() as u64,
                        _ => 0,
                    };
                    mem.reserve(estimate_vals_bytes(&vals).saturating_add(key_extra))?;
                }
                heap.push((
                    key,
                    cur_seq,
                    ScannedRow {
                        rowid: rid,
                        values: vals,
                    },
                ));
                // Sift up: keep worst item at index 0.
                let mut idx = heap.len() - 1;
                while idx > 0 {
                    let parent = (idx - 1) / 2;
                    if cmp_order(&heap[idx].0, heap[idx].1, &heap[parent].0, heap[parent].1)
                        == Ordering::Greater
                    {
                        heap.swap(idx, parent);
                        idx = parent;
                    } else {
                        break;
                    }
                }
                if heap.len() == limit {
                    worst_int = match heap[0].0 {
                        SqlValue::Int(w) => Some(w),
                        _ => None,
                    };
                }
            } else {
                let is_better = match (&key, &heap[0].0) {
                    (SqlValue::Int(a), SqlValue::Int(b)) => {
                        let o = if desc { b.cmp(a) } else { a.cmp(b) };
                        o == Ordering::Less || (o == Ordering::Equal && cur_seq < heap[0].1)
                    }
                    _ => cmp_order(&key, cur_seq, &heap[0].0, heap[0].1) == Ordering::Less,
                };
                if is_better {
                    // Strictly better than current worst item: decode full row and replace `heap[0]`.
                    let rid = if has_rowid {
                        Some(cur.rowid().map_err(map_sqlite_err)?)
                    } else {
                        None
                    };
                    let vals = Self::decode_cursor_row(&mut cur, sparse)?;
                    if meter.is_some() {
                        let key_extra = |v: &SqlValue| -> u64 {
                            match v {
                                SqlValue::Text(b) | SqlValue::Blob(b) => b.len() as u64,
                                _ => 0,
                            }
                        };
                        let new_b = estimate_vals_bytes(&vals).saturating_add(key_extra(&key));
                        let old_b = estimate_vals_bytes(&heap[0].2.values)
                            .saturating_add(key_extra(&heap[0].0));
                        if new_b > old_b {
                            mem.reserve(new_b - old_b)?;
                        } else {
                            mem.release(old_b - new_b);
                        }
                    }
                    heap[0] = (
                        key,
                        cur_seq,
                        ScannedRow {
                            rowid: rid,
                            values: vals,
                        },
                    );
                    // Sift down from index 0.
                    let n = heap.len();
                    let mut idx = 0;
                    loop {
                        let left = 2 * idx + 1;
                        if left >= n {
                            break;
                        }
                        let right = left + 1;
                        let mut worst = left;
                        if right < n
                            && cmp_order(
                                &heap[right].0,
                                heap[right].1,
                                &heap[left].0,
                                heap[left].1,
                            ) == Ordering::Greater
                        {
                            worst = right;
                        }
                        if cmp_order(&heap[worst].0, heap[worst].1, &heap[idx].0, heap[idx].1)
                            == Ordering::Greater
                        {
                            heap.swap(idx, worst);
                            idx = worst;
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

        let mut rows = Vec::with_capacity(heap.len());
        for (_, _, srow) in heap {
            rows.push(srow);
        }
        Ok(Some(rows))
    }

    fn try_rowid_in_scan(
        &self,
        reader: &TableReader,
        values: &[i64],
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<RawRowList> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let mut rows = Vec::with_capacity(values.len());
        let mut mem = ScanMemGuard::new(meter);
        for &rid in values {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            if rcur.seek(rid).map_err(map_sqlite_err)? == SeekResult::Exact {
                let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                if meter.is_some() {
                    mem.reserve(estimate_vals_bytes(&vals))?;
                }
                rows.push(ScannedRow {
                    rowid: Some(rid),
                    values: vals,
                });
            }
        }
        Ok(rows)
    }

    fn try_rowid_range_scan(
        &self,
        reader: &TableReader,
        low: &Bound,
        high: &Bound,
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        // Parse integer lower/upper rowid bounds safely; on non-integer or exotic bounds,
        // return Ok(None) to fall back to full scan (WHERE will still evaluate exact SQLite semantics).
        let lo_i64: Option<i64> = match low {
            Bound::Unbounded => None,
            Bound::Included(v) => match bound_to_int(v) {
                Some(i) => Some(i),
                None => return Ok(None),
            },
            Bound::Excluded(v) => match bound_to_int(v) {
                Some(i) => match i.checked_add(1) {
                    Some(n) => Some(n),
                    None => return Ok(Some(Vec::new())),
                },
                None => return Ok(None),
            },
        };
        let hi_i64: Option<i64> = match high {
            Bound::Unbounded => None,
            Bound::Included(v) => match bound_to_int(v) {
                Some(i) => Some(i),
                None => return Ok(None),
            },
            Bound::Excluded(v) => match bound_to_int(v) {
                Some(i) => match i.checked_sub(1) {
                    Some(n) => Some(n),
                    None => return Ok(Some(Vec::new())),
                },
                None => return Ok(None),
            },
        };

        if let (Some(lo), Some(hi)) = (lo_i64, hi_i64) {
            if lo > hi {
                return Ok(Some(Vec::new()));
            }
            if lo == hi {
                if let Some(m) = meter {
                    m.charge_steps(1)?;
                }
                let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
                return if rcur.seek(lo).map_err(map_sqlite_err)? == SeekResult::Exact {
                    let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                    Ok(Some(vec![ScannedRow {
                        rowid: Some(lo),
                        values: vals,
                    }]))
                } else {
                    Ok(Some(Vec::new()))
                };
            }
        }

        // Range scan using `RowCursor::seek` + sequential leaf traversal (true O(log N + K)!)
        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let positioned = if let Some(lo) = lo_i64 {
            rcur.seek(lo).map_err(map_sqlite_err)? != SeekResult::Eof
        } else {
            rcur.next().map_err(map_sqlite_err)?
        };
        if !positioned {
            return Ok(Some(Vec::new()));
        }

        let mut rows = Vec::new();
        let mut mem = ScanMemGuard::new(meter);
        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let rid = rcur.rowid().map_err(map_sqlite_err)?;
            if let Some(hi) = hi_i64 {
                if rid > hi {
                    break;
                }
            }
            let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
            if meter.is_some() {
                mem.reserve(estimate_vals_bytes(&vals))?;
            }
            rows.push(ScannedRow {
                rowid: Some(rid),
                values: vals,
            });
            if !rcur.next().map_err(map_sqlite_err)? {
                break;
            }
        }
        Ok(Some(rows))
    }

    fn try_index_eq_scan(
        &self,
        reader: &TableReader,
        index_name: &str,
        prefix_values: &[SqlValue],
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        let Ok(idx_reader) = self.db.index(index_name) else {
            return Ok(None);
        };
        let idx_info = idx_reader.info();
        if idx_info.without_rowid_primary_key || idx_info.partial || prefix_values.is_empty() {
            return Ok(None);
        }
        let Ok(key_info) = idx_reader.key_info(Some(&self.options.collations)) else {
            return Ok(None);
        };
        let binary_fallback = (key_info.encoding != TextEncoding::Utf8)
            .then(|| Collation::builtin(BuiltinCollation::Binary, key_info.encoding));
        let Ok(mut search_key) = SearchKey::new(key_info.clone(), prefix_values, 1) else {
            return Ok(None);
        };
        let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
        let width = icur.width();
        let prefix_len = prefix_values.len();
        if width < prefix_len + 1 {
            return Ok(None);
        }
        let rid_col = width - 1;
        if !icur
            .seek_key(&mut search_key, SeekOp::Ge)
            .map_err(map_sqlite_err)?
        {
            return Ok(Some(Vec::new()));
        }

        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let mut rows = Vec::new();
        let mut mem = ScanMemGuard::new(meter);

        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let mut matches = true;
            for (k, target) in prefix_values.iter().enumerate() {
                let kv = icur.value(k).map_err(map_sqlite_err)?;
                let coll = key_info.collations[k].as_ref().or(binary_fallback.as_ref());
                if mem_compare(&kv, target, coll) != Ordering::Equal {
                    matches = false;
                    break;
                }
            }
            if !matches {
                break;
            }
            let rid_val = icur.value(rid_col).map_err(map_sqlite_err)?;
            let Some(rid) = rid_val.as_i64() else {
                return Ok(None);
            };
            if rcur.seek(rid).map_err(map_sqlite_err)? == SeekResult::Exact {
                let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                if meter.is_some() {
                    mem.reserve(estimate_vals_bytes(&vals))?;
                }
                rows.push(ScannedRow {
                    rowid: Some(rid),
                    values: vals,
                });
            }
            if !icur.next().map_err(map_sqlite_err)? {
                break;
            }
        }
        Ok(Some(rows))
    }

    #[allow(clippy::too_many_arguments)]
    fn try_index_range_scan(
        &self,
        reader: &TableReader,
        index_name: &str,
        low: &Bound,
        high: &Bound,
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        let Ok(idx_reader) = self.db.index(index_name) else {
            return Ok(None);
        };
        let idx_info = idx_reader.info();
        if idx_info.without_rowid_primary_key || idx_info.partial || idx_info.key_count == 0 {
            return Ok(None);
        }
        let Ok(key_info) = idx_reader.key_info(Some(&self.options.collations)) else {
            return Ok(None);
        };
        let is_desc = key_info.desc.first().copied().unwrap_or(false);
        let binary_fallback = (key_info.encoding != TextEncoding::Utf8)
            .then(|| Collation::builtin(BuiltinCollation::Binary, key_info.encoding));
        let lead_coll = key_info
            .collations
            .first()
            .and_then(|c| c.as_ref())
            .or(binary_fallback.as_ref());

        // For ASC indexes, scan from `low` upward until `high`.
        // For DESC indexes, index order is reversed, so scan from `high` downward until `low`.
        let (start_bound, stop_bound) = if is_desc {
            (high, low)
        } else {
            (low, high)
        };

        let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
        let width = icur.width();
        if width < 2 {
            return Ok(None);
        }
        let rid_col = width - 1;
        let positioned = match start_bound {
            Bound::Unbounded => icur.next().map_err(map_sqlite_err)?,
            Bound::Included(v) => {
                let Ok(mut sk) = SearchKey::new(key_info.clone(), std::slice::from_ref(v), 1)
                else {
                    return Ok(None);
                };
                icur.seek_key(&mut sk, SeekOp::Ge).map_err(map_sqlite_err)?
            }
            Bound::Excluded(v) => {
                let Ok(mut sk) = SearchKey::new(key_info.clone(), std::slice::from_ref(v), -1)
                else {
                    return Ok(None);
                };
                icur.seek_key(&mut sk, SeekOp::Gt).map_err(map_sqlite_err)?
            }
        };
        if !positioned {
            return Ok(Some(Vec::new()));
        }

        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let mut rows = Vec::new();
        let mut mem = ScanMemGuard::new(meter);

        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let k0 = icur.value(0).map_err(map_sqlite_err)?;
            if !k0.is_null() {
                let past_stop = match stop_bound {
                    Bound::Unbounded => false,
                    Bound::Included(sv) => {
                        let cmp = mem_compare(&k0, sv, lead_coll);
                        if is_desc {
                            cmp == Ordering::Less
                        } else {
                            cmp == Ordering::Greater
                        }
                    }
                    Bound::Excluded(sv) => {
                        let cmp = mem_compare(&k0, sv, lead_coll);
                        if is_desc {
                            cmp != Ordering::Greater
                        } else {
                            cmp != Ordering::Less
                        }
                    }
                };
                if past_stop {
                    break;
                }
                let rid_val = icur.value(rid_col).map_err(map_sqlite_err)?;
                let Some(rid) = rid_val.as_i64() else {
                    return Ok(None);
                };
                if rcur.seek(rid).map_err(map_sqlite_err)? == SeekResult::Exact {
                    let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                    if meter.is_some() {
                        mem.reserve(estimate_vals_bytes(&vals))?;
                    }
                    rows.push(ScannedRow {
                        rowid: Some(rid),
                        values: vals,
                    });
                }
            } else if is_desc {
                // In DESC order NULL sorts last; once we hit NULL, no non-null keys follow.
                break;
            }
            if !icur.next().map_err(map_sqlite_err)? {
                break;
            }
        }
        Ok(Some(rows))
    }

    fn try_index_in_scan(
        &self,
        reader: &TableReader,
        index_name: &str,
        values: &[SqlValue],
        scan_opts: &[usize],
        sparse: Option<(usize, &[usize])>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<RawRowList>> {
        if values.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let Ok(idx_reader) = self.db.index(index_name) else {
            return Ok(None);
        };
        let idx_info = idx_reader.info();
        if idx_info.without_rowid_primary_key || idx_info.partial || idx_info.key_count == 0 {
            return Ok(None);
        }
        let Ok(key_info) = idx_reader.key_info(Some(&self.options.collations)) else {
            return Ok(None);
        };
        let binary_fallback = (key_info.encoding != TextEncoding::Utf8)
            .then(|| Collation::builtin(BuiltinCollation::Binary, key_info.encoding));
        let lead_coll = key_info
            .collations
            .first()
            .and_then(|c| c.as_ref())
            .or(binary_fallback.as_ref());
        let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
        let width = icur.width();
        if width < 2 {
            return Ok(None);
        }
        let rid_col = width - 1;
        let mut rcur = reader.cursor_cids(scan_opts).map_err(map_sqlite_err)?;
        let mut rows = Vec::new();
        let mut seen_rowids = HashSet::new();
        let mut mem = ScanMemGuard::new(meter);

        for target in values {
            let Ok(mut sk) = SearchKey::new(key_info.clone(), std::slice::from_ref(target), 1)
            else {
                return Ok(None);
            };
            if !icur.seek_key(&mut sk, SeekOp::Ge).map_err(map_sqlite_err)? {
                continue;
            }
            loop {
                if let Some(m) = meter {
                    m.charge_steps(1)?;
                }
                let k0 = icur.value(0).map_err(map_sqlite_err)?;
                if mem_compare(&k0, target, lead_coll) != Ordering::Equal {
                    break;
                }
                let rid_val = icur.value(rid_col).map_err(map_sqlite_err)?;
                let Some(rid) = rid_val.as_i64() else {
                    return Ok(None);
                };
                if seen_rowids.insert(rid)
                    && rcur.seek(rid).map_err(map_sqlite_err)? == SeekResult::Exact
                {
                    let vals = Self::decode_cursor_row(&mut rcur, sparse)?;
                    if meter.is_some() {
                        mem.reserve(estimate_vals_bytes(&vals))?;
                    }
                    rows.push(ScannedRow {
                        rowid: Some(rid),
                        values: vals,
                    });
                }
                if !icur.next().map_err(map_sqlite_err)? {
                    break;
                }
            }
        }
        Ok(Some(rows))
    }

    fn eval_pragma_tvf(
        &self,
        lower_name: &str,
        args: &[SqlValue],
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<TableValuedResult>> {
        let arg_str = |idx: usize| -> Option<String> {
            args.get(idx).and_then(|v| {
                xdb_sql::utf8::text_bytes_of(v).map(|b| String::from_utf8_lossy(&b).into_owned())
            })
        };

        let check_schema_arg = |idx: usize| -> SqlResult<()> {
            if let Some(s) = arg_str(idx) {
                if !s.is_empty() && !Self::check_schema(Some(&s)) {
                    return Err(SqlError::runtime(format!("unknown database '{s}'")));
                }
            }
            Ok(())
        };

        match lower_name {
            "pragma_table_info" | "pragma_table_xinfo" => {
                check_schema_arg(1)?;
                let include_hidden = lower_name == "pragma_table_xinfo";
                let mut columns = vec![
                    "cid".to_string(),
                    "name".to_string(),
                    "type".to_string(),
                    "notnull".to_string(),
                    "dflt_value".to_string(),
                    "pk".to_string(),
                ];
                if include_hidden {
                    columns.push("hidden".to_string());
                }
                let Some(tname) = arg_str(0) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                if let Some(table) = self.db.schema().table(&tname) {
                    let mut rows = Vec::new();
                    let mut visible_cid = 0i64;
                    for col in &table.columns {
                        if !include_hidden && col.hidden != ColumnHiddenKind::Normal {
                            continue;
                        }
                        let cid_val = if include_hidden {
                            col.cid as i64
                        } else {
                            let c = visible_cid;
                            visible_cid += 1;
                            c
                        };
                        let mut row = vec![
                            SqlValue::Int(cid_val),
                            SqlValue::text(col.name.as_str()),
                            SqlValue::text(col.decl_type.as_str()),
                            SqlValue::Int(i64::from(col.not_null)),
                            col.default_sql
                                .as_deref()
                                .map_or(SqlValue::Null, SqlValue::text),
                            SqlValue::Int(col.pk as i64),
                        ];
                        if include_hidden {
                            row.push(SqlValue::Int(i64::from(col.hidden as u8)));
                        }
                        rows.push(row);
                    }
                    return Ok(Some(TableValuedResult { columns, rows }));
                }
                if let Some(view) = self.db.schema().view(&tname) {
                    if self.in_table_info.swap(true, AtomicOrdering::AcqRel) {
                        return Ok(Some(TableValuedResult {
                            columns,
                            rows: Vec::new(),
                        }));
                    }
                    struct TableInfoGuard<'b>(&'b AtomicBool);
                    impl Drop for TableInfoGuard<'_> {
                        fn drop(&mut self) {
                            self.0.store(false, AtomicOrdering::Release);
                        }
                    }
                    let _ti_guard = TableInfoGuard(&self.in_table_info);
                    let mut visiting = vec![view.name.to_ascii_lowercase()];
                    let vcols = self.infer_view_cols(
                        &view.sql,
                        view.column_names.as_deref(),
                        &mut visiting,
                        meter,
                        true,
                    )?;
                    let mut rows = Vec::with_capacity(vcols.len());
                    for (cid, (cname, ctype)) in vcols.into_iter().enumerate() {
                        let mut row = vec![
                            SqlValue::Int(cid as i64),
                            SqlValue::text(cname),
                            SqlValue::text(ctype),
                            SqlValue::Int(0),
                            SqlValue::Null,
                            SqlValue::Int(0),
                        ];
                        if include_hidden {
                            row.push(SqlValue::Int(0));
                        }
                        rows.push(row);
                    }
                    return Ok(Some(TableValuedResult { columns, rows }));
                }
                Ok(Some(TableValuedResult {
                    columns,
                    rows: Vec::new(),
                }))
            }
            "pragma_index_list" => {
                check_schema_arg(1)?;
                let columns = vec![
                    "seq".to_string(),
                    "name".to_string(),
                    "unique".to_string(),
                    "origin".to_string(),
                    "partial".to_string(),
                ];
                let Some(tname) = arg_str(0) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let Some(table) = self.db.schema().table(&tname) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let mut rows = Vec::new();
                for (seq, idx) in table.indexes.iter().enumerate() {
                    let origin = match idx.origin {
                        IndexOrigin::CreateIndex => "c",
                        IndexOrigin::Unique => "u",
                        IndexOrigin::PrimaryKey => "pk",
                    };
                    rows.push(vec![
                        SqlValue::Int(seq as i64),
                        SqlValue::text(idx.name.as_str()),
                        SqlValue::Int(i64::from(idx.unique)),
                        SqlValue::text(origin),
                        SqlValue::Int(i64::from(idx.partial)),
                    ]);
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_index_info" | "pragma_index_xinfo" => {
                check_schema_arg(1)?;
                let extended = lower_name == "pragma_index_xinfo";
                let mut columns = vec![
                    "seqno".to_string(),
                    "cid".to_string(),
                    "name".to_string(),
                ];
                if extended {
                    columns.extend(["desc".to_string(), "coll".to_string(), "key".to_string()]);
                }
                let Some(iname) = arg_str(0) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let Some(idx) = self.db.schema().index(&iname) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let mut rows = Vec::new();
                for (seqno, col) in idx.columns.iter().enumerate() {
                    if !extended && !col.key {
                        continue;
                    }
                    let mut row = vec![
                        SqlValue::Int(seqno as i64),
                        SqlValue::Int(i64::from(col.cid)),
                        col.name.as_deref().map_or(SqlValue::Null, SqlValue::text),
                    ];
                    if extended {
                        row.push(SqlValue::Int(i64::from(col.desc)));
                        row.push(SqlValue::text(col.collation.as_str()));
                        row.push(SqlValue::Int(i64::from(col.key)));
                    }
                    rows.push(row);
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_database_list" => {
                let columns = vec!["seq".to_string(), "name".to_string(), "file".to_string()];
                let rows = vec![vec![
                    SqlValue::Int(0),
                    SqlValue::text("main"),
                    SqlValue::text(self.db.path()),
                ]];
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_foreign_key_list" => {
                check_schema_arg(1)?;
                let columns = vec![
                    "id".to_string(),
                    "seq".to_string(),
                    "table".to_string(),
                    "from".to_string(),
                    "to".to_string(),
                    "on_update".to_string(),
                    "on_delete".to_string(),
                    "match".to_string(),
                ];
                let Some(tname) = arg_str(0) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let Some(table) = self.db.schema().table(&tname) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
                let fk_action_str = |act: xdb_parser::ForeignKeyAction| match act {
                    xdb_parser::ForeignKeyAction::SetNull => "SET NULL",
                    xdb_parser::ForeignKeyAction::SetDefault => "SET DEFAULT",
                    xdb_parser::ForeignKeyAction::Cascade => "CASCADE",
                    xdb_parser::ForeignKeyAction::Restrict => "RESTRICT",
                    xdb_parser::ForeignKeyAction::NoAction => "NO ACTION",
                };
                let mut rows = Vec::new();
                // SQLite prepends foreign keys onto a linked list, so `id` counts from the
                // last-declared foreign key (`0`) back to the first.
                for (fk_id, fk) in table.definition.foreign_keys.iter().rev().enumerate() {
                    let on_upd = fk_action_str(fk.on_update);
                    let on_del = fk_action_str(fk.on_delete);
                    for (seq, &from_cid) in fk.from.iter().enumerate() {
                        let from_name = table
                            .columns
                            .get(from_cid)
                            .map_or("", |c| c.name.as_str());
                        let to_val = fk
                            .to
                            .as_ref()
                            .and_then(|v| v.get(seq))
                            .map_or(SqlValue::Null, |s| SqlValue::text(s.as_str()));
                        rows.push(vec![
                            SqlValue::Int(fk_id as i64),
                            SqlValue::Int(seq as i64),
                            SqlValue::text(fk.table.as_str()),
                            SqlValue::text(from_name),
                            to_val,
                            SqlValue::text(on_upd),
                            SqlValue::text(on_del),
                            SqlValue::text("NONE"),
                        ]);
                    }
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_table_list" => {
                check_schema_arg(1)?;
                let columns = vec![
                    "schema".to_string(),
                    "name".to_string(),
                    "type".to_string(),
                    "ncol".to_string(),
                    "wr".to_string(),
                    "strict".to_string(),
                ];
                if self.in_table_list.swap(true, AtomicOrdering::AcqRel) {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                }
                struct TableListGuard<'b>(&'b AtomicBool);
                impl Drop for TableListGuard<'_> {
                    fn drop(&mut self) {
                        self.0.store(false, AtomicOrdering::Release);
                    }
                }
                let _tl_guard = TableListGuard(&self.in_table_list);
                let filter = arg_str(0);
                let matches_filter = |n: &str| {
                    filter
                        .as_deref()
                        .is_none_or(|f| n.eq_ignore_ascii_case(f))
                };
                let mut rows = Vec::new();
                if matches_filter("sqlite_schema") || matches_filter("sqlite_master") {
                    rows.push(vec![
                        SqlValue::text("main"),
                        SqlValue::text("sqlite_schema"),
                        SqlValue::text("table"),
                        SqlValue::Int(5),
                        SqlValue::Int(0),
                        SqlValue::Int(0),
                    ]);
                }
                for t in &self.db.schema().tables {
                    if t.name.eq_ignore_ascii_case("sqlite_master")
                        || t.name.eq_ignore_ascii_case("sqlite_schema")
                    {
                        continue;
                    }
                    if !matches_filter(&t.name) {
                        continue;
                    }
                    rows.push(vec![
                        SqlValue::text("main"),
                        SqlValue::text(t.name.as_str()),
                        SqlValue::text("table"),
                        SqlValue::Int(t.columns.len() as i64),
                        SqlValue::Int(i64::from(t.without_rowid)),
                        SqlValue::Int(i64::from(t.strict)),
                    ]);
                }
                for v in &self.db.schema().views {
                    if !matches_filter(&v.name) {
                        continue;
                    }
                    let mut visiting = vec![v.name.to_ascii_lowercase()];
                    let ncol = match self.infer_view_cols(
                        &v.sql,
                        v.column_names.as_deref(),
                        &mut visiting,
                        meter,
                        true,
                    ) {
                        Ok(cols) => cols.len(),
                        Err(SqlError::ResourceLimit(rl)) => return Err(SqlError::ResourceLimit(rl)),
                        Err(_) => 0,
                    };
                    rows.push(vec![
                        SqlValue::text("main"),
                        SqlValue::text(v.name.as_str()),
                        SqlValue::text("view"),
                        SqlValue::Int(ncol as i64),
                        SqlValue::Int(0),
                        SqlValue::Int(0),
                    ]);
                }
                for vt in &self.db.schema().virtual_tables {
                    if !matches_filter(&vt.name) {
                        continue;
                    }
                    rows.push(vec![
                        SqlValue::text("main"),
                        SqlValue::text(vt.name.as_str()),
                        SqlValue::text("virtual"),
                        SqlValue::Int(0),
                        SqlValue::Int(0),
                        SqlValue::Int(0),
                    ]);
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_foreign_key_check" => {
                check_schema_arg(1)?;
                let columns = vec![
                    "table".to_string(),
                    "rowid".to_string(),
                    "parent".to_string(),
                    "fkid".to_string(),
                ];
                let filter = arg_str(0);
                let rows = crate::write::fk::collect_fk_violations_db(
                    self.db,
                    filter.as_deref(),
                    self.options,
                )
                .map_err(map_sqlite_err)?;
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_collation_list" => {
                let columns = vec!["seq".to_string(), "name".to_string()];
                let mut rows = vec![
                    vec![SqlValue::Int(0), SqlValue::text("RTRIM")],
                    vec![SqlValue::Int(1), SqlValue::text("NOCASE")],
                    vec![SqlValue::Int(2), SqlValue::text("BINARY")],
                ];
                for cname in self.options.collations.custom_names() {
                    let seq = rows.len() as i64;
                    rows.push(vec![SqlValue::Int(seq), SqlValue::text(cname)]);
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_compile_options" => {
                let columns = vec!["compile_options".to_string()];
                let rows = xdb_sql::functions::core::COMPILE_OPTIONS
                    .iter()
                    .map(|&opt| vec![SqlValue::text(opt)])
                    .collect();
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_pragma_list" => {
                let columns = vec!["name".to_string()];
                let names = [
                    "application_id",
                    "auto_vacuum",
                    "busy_timeout",
                    "case_sensitive_like",
                    "collation_list",
                    "compile_options",
                    "data_version",
                    "database_list",
                    "defer_foreign_keys",
                    "encoding",
                    "foreign_key_check",
                    "foreign_key_list",
                    "foreign_keys",
                    "freelist_count",
                    "function_list",
                    "ignore_check_constraints",
                    "index_info",
                    "index_list",
                    "index_xinfo",
                    "integrity_check",
                    "journal_mode",
                    "page_count",
                    "page_size",
                    "pragma_list",
                    "query_only",
                    "quick_check",
                    "recursive_triggers",
                    "schema_version",
                    "synchronous",
                    "table_info",
                    "table_list",
                    "table_xinfo",
                    "user_version",
                ];
                let rows = names
                    .into_iter()
                    .map(|n| vec![SqlValue::text(n)])
                    .collect();
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_function_list" => {
                let columns = vec![
                    "name".to_string(),
                    "builtin".to_string(),
                    "type".to_string(),
                    "enc".to_string(),
                    "narg".to_string(),
                    "flags".to_string(),
                ];
                // Built-in SQLite + JSON1 + math + date + aggregate + window + TVF catalog.
                let builtins: &[(&str, &str, i64)] = &[
                    // Core scalar functions
                    ("abs", "s", 1),
                    ("changes", "s", 0),
                    ("char", "s", -1),
                    ("coalesce", "s", -1),
                    ("concat", "s", -1),
                    ("concat_ws", "s", -1),
                    ("format", "s", -1),
                    ("glob", "s", 2),
                    ("hex", "s", 1),
                    ("if", "s", -1),
                    ("ifnull", "s", 2),
                    ("iif", "s", -1),
                    ("instr", "s", 2),
                    ("last_insert_rowid", "s", 0),
                    ("length", "s", 1),
                    ("like", "s", 2),
                    ("like", "s", 3),
                    ("likelihood", "s", 2),
                    ("likely", "s", 1),
                    ("lower", "s", 1),
                    ("ltrim", "s", 1),
                    ("ltrim", "s", 2),
                    ("max", "s", -1),
                    ("min", "s", -1),
                    ("nullif", "s", 2),
                    ("octet_length", "s", 1),
                    ("printf", "s", -1),
                    ("quote", "s", 1),
                    ("random", "s", 0),
                    ("randomblob", "s", 1),
                    ("replace", "s", 3),
                    ("round", "s", 1),
                    ("round", "s", 2),
                    ("rtrim", "s", 1),
                    ("rtrim", "s", 2),
                    ("sign", "s", 1),
                    ("sqlite_compileoption_get", "s", 1),
                    ("sqlite_compileoption_used", "s", 1),
                    ("sqlite_source_id", "s", 0),
                    ("sqlite_version", "s", 0),
                    ("substr", "s", 2),
                    ("substr", "s", 3),
                    ("substring", "s", 2),
                    ("substring", "s", 3),
                    ("total_changes", "s", 0),
                    ("trim", "s", 1),
                    ("trim", "s", 2),
                    ("typeof", "s", 1),
                    ("unhex", "s", 1),
                    ("unhex", "s", 2),
                    ("unicode", "s", 1),
                    ("unistr", "s", 1),
                    ("unistr_quote", "s", 1),
                    ("unlikely", "s", 1),
                    ("upper", "s", 1),
                    ("zeroblob", "s", 1),
                    // Math functions
                    ("acos", "s", 1),
                    ("acosh", "s", 1),
                    ("asin", "s", 1),
                    ("asinh", "s", 1),
                    ("atan", "s", 1),
                    ("atan2", "s", 2),
                    ("atanh", "s", 1),
                    ("ceil", "s", 1),
                    ("ceiling", "s", 1),
                    ("cos", "s", 1),
                    ("cosh", "s", 1),
                    ("degrees", "s", 1),
                    ("exp", "s", 1),
                    ("floor", "s", 1),
                    ("ln", "s", 1),
                    ("log", "s", 1),
                    ("log", "s", 2),
                    ("log10", "s", 1),
                    ("log2", "s", 1),
                    ("mod", "s", 2),
                    ("pi", "s", 0),
                    ("pow", "s", 2),
                    ("power", "s", 2),
                    ("radians", "s", 1),
                    ("sin", "s", 1),
                    ("sinh", "s", 1),
                    ("sqrt", "s", 1),
                    ("tan", "s", 1),
                    ("tanh", "s", 1),
                    ("trunc", "s", 1),
                    // Date / Time functions
                    ("date", "s", -1),
                    ("datetime", "s", -1),
                    ("julianday", "s", -1),
                    ("strftime", "s", -1),
                    ("time", "s", -1),
                    ("timediff", "s", 2),
                    ("unixepoch", "s", -1),
                    // JSON functions
                    ("json", "s", 1),
                    ("json_array", "s", -1),
                    ("json_array_length", "s", 1),
                    ("json_array_length", "s", 2),
                    ("json_error_position", "s", 1),
                    ("json_extract", "s", -1),
                    ("json_insert", "s", -1),
                    ("json_object", "s", -1),
                    ("json_patch", "s", 2),
                    ("json_pretty", "s", 1),
                    ("json_pretty", "s", 2),
                    ("json_quote", "s", 1),
                    ("json_remove", "s", -1),
                    ("json_replace", "s", -1),
                    ("json_set", "s", -1),
                    ("json_type", "s", 1),
                    ("json_type", "s", 2),
                    ("json_valid", "s", 1),
                    ("json_valid", "s", 2),
                    // Aggregate functions
                    ("avg", "w", 1),
                    ("count", "w", 0),
                    ("count", "w", 1),
                    ("group_concat", "w", 1),
                    ("group_concat", "w", 2),
                    ("json_group_array", "w", 1),
                    ("json_group_object", "w", 2),
                    ("max", "w", 1),
                    ("median", "w", 1),
                    ("min", "w", 1),
                    ("percentile", "w", 2),
                    ("percentile_cont", "w", 2),
                    ("percentile_disc", "w", 2),
                    ("string_agg", "w", 2),
                    ("sum", "w", 1),
                    ("total", "w", 1),
                    // Window-only functions
                    ("cume_dist", "w", 0),
                    ("dense_rank", "w", 0),
                    ("first_value", "w", 1),
                    ("lag", "w", 1),
                    ("lag", "w", 2),
                    ("lag", "w", 3),
                    ("last_value", "w", 1),
                    ("lead", "w", 1),
                    ("lead", "w", 2),
                    ("lead", "w", 3),
                    ("nth_value", "w", 2),
                    ("ntile", "w", 1),
                    ("percent_rank", "w", 0),
                    ("rank", "w", 0),
                    ("row_number", "w", 0),
                    // Built-in table-valued functions
                    ("generate_series", "v", -1),
                    ("json_each", "v", 1),
                    ("json_each", "v", 2),
                    ("json_tree", "v", 1),
                    ("json_tree", "v", 2),
                ];
                let mut rows: Vec<Vec<SqlValue>> = builtins
                    .iter()
                    .map(|&(fname, ftype, narg)| {
                        vec![
                            SqlValue::text(fname),
                            SqlValue::Int(1),
                            SqlValue::text(ftype),
                            SqlValue::text("utf8"),
                            SqlValue::Int(narg),
                            SqlValue::Int(0),
                        ]
                    })
                    .collect();
                for (fname, arity) in self.options.functions.custom_scalars() {
                    rows.push(vec![
                        SqlValue::text(fname),
                        SqlValue::Int(0),
                        SqlValue::text("s"),
                        SqlValue::text("utf8"),
                        SqlValue::Int(arity.map_or(-1, |a| a as i64)),
                        SqlValue::Int(0),
                    ]);
                }
                Ok(Some(TableValuedResult { columns, rows }))
            }
            _ => Ok(None),
        }
    }

    /// Statically infer a view's output `(column_name, decl_type)` pairs from its parsed `SELECT`
    /// AST + catalog schema, avoiding view materialization, runtime errors, and `pragma_table_list`
    /// recursion, and handling `NATURAL JOIN` / `USING` deduplication accurately.
    fn infer_expr_decl_type(
        mut expr: &xdb_parser::Expr,
        all_from_cols: &[(Option<String>, String, String)],
    ) -> (Option<String>, String) {
        while let xdb_parser::Expr::Collate { operand, .. } = expr {
            expr = operand;
        }
        match expr {
            xdb_parser::Expr::Column(c) => {
                let want_tbl = c.table.as_ref().map(|t| t.name.as_str());
                let cname = c.column.name.as_str();
                if let Some((_, decl_name, ty)) = all_from_cols.iter().find(|(src_alias, col_name, _)| {
                    col_name.eq_ignore_ascii_case(cname)
                        && want_tbl.is_none_or(|wt| {
                            src_alias
                                .as_deref()
                                .is_some_and(|sa| sa.eq_ignore_ascii_case(wt))
                        })
                }) {
                    return (Some(decl_name.clone()), ty.clone());
                }
                (None, String::new())
            }
            xdb_parser::Expr::Cast { type_name, .. } => (
                None,
                type_name
                    .as_ref()
                    .map(|t| t.text.clone())
                    .unwrap_or_default(),
            ),
            _ => (None, String::new()),
        }
    }

    fn deduplicate_view_col_names(cols: &mut [(String, String)]) {
        let mut seen: HashSet<String> = HashSet::with_capacity(cols.len());
        for (name, _) in cols.iter_mut() {
            let lower = name.to_ascii_lowercase();
            if seen.insert(lower) {
                continue;
            }
            let base = name.clone();
            let mut n = 1usize;
            loop {
                let cand = format!("{base}:{n}");
                if seen.insert(cand.to_ascii_lowercase()) {
                    *name = cand;
                    break;
                }
                n += 1;
            }
        }
    }

    fn infer_view_cols(
        &self,
        select_sql: &str,
        explicit_names: Option<&[String]>,
        visiting: &mut Vec<String>,
        meter: Option<&dyn Meter>,
        strict_missing: bool,
    ) -> SqlResult<Vec<(String, String)>> {
        let eff_meter = meter.or(self.options.meter.as_deref());
        if let Some(m) = eff_meter {
            m.charge_steps(1)?;
        }
        if visiting.len() > 64 {
            if strict_missing {
                let vname = visiting.first().map_or("", String::as_str);
                return Err(SqlError::runtime(format!(
                    "view {vname} is circularly defined"
                )));
            }
            return Ok(Vec::new());
        }
        let parse_opts = xdb_parser::ParseOptions {
            dialect: self.options.dialect,
            ..xdb_parser::ParseOptions::default()
        };
        let mut parsed = match eff_meter {
            Some(m) => xdb_parser::parse_statement_metered(select_sql, &parse_opts, m),
            None => xdb_parser::parse_statement(select_sql, &parse_opts),
        };
        if let Err(ref e) = parsed {
            if e.kind != xdb_parser::SqlParseErrorKind::ResourceLimit
                && self.options.dialect != xdb_parser::Dialect::Xdb
            {
                let xdb_opts = xdb_parser::ParseOptions {
                    dialect: xdb_parser::Dialect::Xdb,
                    ..xdb_parser::ParseOptions::default()
                };
                parsed = match eff_meter {
                    Some(m) => xdb_parser::parse_statement_metered(select_sql, &xdb_opts, m),
                    None => xdb_parser::parse_statement(select_sql, &xdb_opts),
                };
            }
        }
        let sel = match parsed {
            Ok(xdb_parser::Statement::Select(s)) => s,
            Ok(xdb_parser::Statement::CreateView(cv)) => cv.select,
            Ok(_) => return Ok(Vec::new()),
            Err(e) => {
                if strict_missing || e.kind == xdb_parser::SqlParseErrorKind::ResourceLimit {
                    return Err(SqlError::from(e));
                }
                return Ok(Vec::new());
            }
        };
        // When `strict_missing` is requested at the top-level view (e.g. `PRAGMA table_info(v)`),
        // run full zero-row schema validation so circular views, missing tables in compound right
        // branches, compound arity mismatches, and invalid WHERE/GROUP BY/HAVING/ORDER BY columns
        // fail cleanly just like SQLite's `sqlite3ViewGetColumnNames`.
        if strict_missing && visiting.len() <= 1 {
            let _ = xdb_sql::validate_select_stmt_schema(self, &sel, self.options, &[], None)?;
        }
        let mut cols = self.infer_select_cols(
            &sel,
            select_sql,
            &[],
            visiting,
            eff_meter,
            strict_missing,
        )?;
        if let Some(names) = explicit_names {
            if names.len() == cols.len() {
                for (i, n) in names.iter().enumerate() {
                    cols[i].0 = n.clone();
                }
            } else if strict_missing {
                return Err(SqlError::runtime(format!(
                    "expected {} columns but got {}",
                    names.len(),
                    cols.len()
                )));
            } else {
                cols = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let ty = cols.get(i).map(|c| c.1.clone()).unwrap_or_default();
                        (n.clone(), ty)
                    })
                    .collect();
            }
        }
        Self::deduplicate_view_col_names(&mut cols);
        Ok(cols)
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_select_cols(
        &self,
        sel: &xdb_parser::SelectStmt,
        raw_sql: &str,
        outer_ctes: &[(String, Vec<(String, String)>)],
        visiting: &mut Vec<String>,
        meter: Option<&dyn Meter>,
        strict_missing: bool,
    ) -> SqlResult<Vec<(String, String)>> {
        if let Some(m) = meter {
            m.charge_steps(1)?;
        }
        if visiting.len() > 64 {
            return Ok(Vec::new());
        }
        let mut ctes: Vec<(String, Vec<(String, String)>)> = outer_ctes.to_vec();
        if let Some(ref with) = sel.with {
            for cte in &with.ctes {
                let cols = if let Some(ref explicit) = cte.columns {
                    explicit
                        .iter()
                        .map(|n| (n.name.name.clone(), String::new()))
                        .collect()
                } else {
                    self.infer_select_cols(
                        &cte.select,
                        raw_sql,
                        &ctes,
                        visiting,
                        meter,
                        strict_missing,
                    )?
                };
                ctes.push((cte.name.name.to_ascii_lowercase(), cols));
            }
        }
        self.infer_body_cols(&sel.body, raw_sql, &ctes, visiting, meter, strict_missing)
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_body_cols(
        &self,
        body: &xdb_parser::SelectBody,
        raw_sql: &str,
        ctes: &[(String, Vec<(String, String)>)],
        visiting: &mut Vec<String>,
        meter: Option<&dyn Meter>,
        strict_missing: bool,
    ) -> SqlResult<Vec<(String, String)>> {
        if let Some(m) = meter {
            m.charge_steps(1)?;
        }
        match body {
            xdb_parser::SelectBody::Values(vals) => Ok(vals
                .rows
                .first()
                .map(|r| {
                    (0..r.items.len())
                        .map(|i| (format!("column{}", i + 1), String::new()))
                        .collect()
                })
                .unwrap_or_default()),
            xdb_parser::SelectBody::Compound { left, .. } => {
                self.infer_body_cols(left, raw_sql, ctes, visiting, meter, strict_missing)
            }
            xdb_parser::SelectBody::Core(core) => {
                let all_from_cols = if let Some(ref from) = core.from {
                    self.infer_from_cols(
                        from,
                        raw_sql,
                        None,
                        ctes,
                        visiting,
                        meter,
                        strict_missing,
                    )?
                } else {
                    Vec::new()
                };
                let mut out = Vec::new();
                for rc in &core.columns {
                    match rc {
                        xdb_parser::ResultColumn::Expr(re) => {
                            let (decl_name, inferred_type) =
                                Self::infer_expr_decl_type(&re.expr, &all_from_cols);
                            let mut base_expr = &re.expr;
                            while let xdb_parser::Expr::Collate { operand, .. } = base_expr {
                                base_expr = operand;
                            }
                            let col_name = if let Some(ref a) = re.alias {
                                a.name.clone()
                            } else if let Some(dn) = decl_name {
                                dn
                            } else if let xdb_parser::Expr::Column(c) = base_expr {
                                c.column.name.clone()
                            } else if re.span.end <= raw_sql.len() && re.span.start <= re.span.end {
                                raw_sql[re.span.start..re.span.end].trim().to_string()
                            } else {
                                format!("column{}", out.len() + 1)
                            };
                            out.push((col_name, inferred_type));
                        }
                        xdb_parser::ResultColumn::Star(rs) => {
                            let Some(ref from) = core.from else {
                                return Ok(Vec::new());
                            };
                            let target = rs.table.as_ref().map(|t| t.name.as_str());
                            let mut expanded = self.infer_from_cols(
                                from,
                                raw_sql,
                                target,
                                ctes,
                                visiting,
                                meter,
                                strict_missing,
                            )?;
                            if let Some(ref excl) = rs.exclude {
                                expanded.retain(|(_, cname, _)| {
                                    !excl.iter().any(|ex| ex.name.eq_ignore_ascii_case(cname))
                                });
                            }
                            for (_, cname, mut cty) in expanded {
                                if let Some(ref reps) = rs.replace {
                                    if let Some(rep) =
                                        reps.iter().find(|r| r.name.name.eq_ignore_ascii_case(&cname))
                                    {
                                        cty = Self::infer_expr_decl_type(&rep.expr, &all_from_cols).1;
                                    }
                                }
                                out.push((cname, cty));
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Returns `(source_alias, col_name, decl_type)` entries for a `FROM` clause.
    #[allow(clippy::too_many_arguments)]
    fn infer_from_cols(
        &self,
        from: &xdb_parser::FromSource,
        raw_sql: &str,
        target_alias: Option<&str>,
        ctes: &[(String, Vec<(String, String)>)],
        visiting: &mut Vec<String>,
        meter: Option<&dyn Meter>,
        strict_missing: bool,
    ) -> SqlResult<Vec<(Option<String>, String, String)>> {
        if let Some(m) = meter {
            m.charge_steps(1)?;
        }
        match from {
            xdb_parser::FromSource::Table(ft) => {
                let eff_alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
                if let Some(t) = target_alias {
                    if !eff_alias.eq_ignore_ascii_case(t) {
                        return Ok(Vec::new());
                    }
                }
                let src_tag = Some(eff_alias.to_string());
                let schema = ft.schema.as_ref().map(|s| s.name.as_str());
                if schema.is_none() {
                    if let Some((_, ccols)) = ctes
                        .iter()
                        .rev()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&ft.name.name))
                    {
                        return Ok(ccols
                            .iter()
                            .map(|(n, ty)| (src_tag.clone(), n.clone(), ty.clone()))
                            .collect());
                    }
                }
                if !Self::check_schema(schema) {
                    if strict_missing {
                        return Err(SqlError::runtime(format!(
                            "no such table: {}.{}",
                            schema.unwrap_or("main"),
                            ft.name.name
                        )));
                    }
                    return Ok(Vec::new());
                }
                if let Some(tbl) = self.db.schema().table(&ft.name.name) {
                    return Ok(tbl
                        .columns
                        .iter()
                        .map(|c| (src_tag.clone(), c.name.clone(), c.decl_type.clone()))
                        .collect());
                }
                if let Some(v) = self.db.schema().view(&ft.name.name) {
                    let lower = v.name.to_ascii_lowercase();
                    if visiting.iter().any(|x| x == &lower) {
                        if strict_missing {
                            return Err(SqlError::runtime(format!(
                                "view {} is circularly defined",
                                v.name
                            )));
                        }
                        return Ok(Vec::new());
                    }
                    visiting.push(lower);
                    let res = self.infer_view_cols(
                        &v.sql,
                        v.column_names.as_deref(),
                        visiting,
                        meter,
                        strict_missing,
                    );
                    visiting.pop();
                    return Ok(res?
                        .into_iter()
                        .map(|(n, ty)| (src_tag.clone(), n, ty))
                        .collect());
                }
                if let Some(names) = Self::static_tvf_col_names(&ft.name.name) {
                    return Ok(names
                        .iter()
                        .map(|&n| (src_tag.clone(), n.to_string(), String::new()))
                        .collect());
                }
                if strict_missing {
                    return Err(SqlError::runtime(format!(
                        "no such table: {}.{}",
                        schema.unwrap_or("main"),
                        ft.name.name
                    )));
                }
                Ok(Vec::new())
            }
            xdb_parser::FromSource::Function(ff) => {
                let eff_alias = ff.alias.as_ref().unwrap_or(&ff.name).name.as_str();
                if let Some(t) = target_alias {
                    if !eff_alias.eq_ignore_ascii_case(t) {
                        return Ok(Vec::new());
                    }
                }
                let src_tag = Some(eff_alias.to_string());
                Ok(Self::static_tvf_col_names(&ff.name.name)
                    .map(|names| {
                        names
                            .iter()
                            .map(|&n| (src_tag.clone(), n.to_string(), String::new()))
                            .collect()
                    })
                    .unwrap_or_default())
            }
            xdb_parser::FromSource::Subquery(fs) => {
                let eff_alias = fs.alias.as_ref().map(|a| a.name.as_str());
                if let Some(t) = target_alias {
                    if !eff_alias.is_some_and(|a| a.eq_ignore_ascii_case(t)) {
                        return Ok(Vec::new());
                    }
                }
                let src_tag = eff_alias.map(str::to_string);
                Ok(self
                    .infer_select_cols(
                        &fs.select,
                        raw_sql,
                        ctes,
                        visiting,
                        meter,
                        strict_missing,
                    )?
                    .into_iter()
                    .map(|(n, ty)| (src_tag.clone(), n, ty))
                    .collect())
            }
            xdb_parser::FromSource::Group(fg) => self.infer_from_cols(
                &fg.source,
                raw_sql,
                target_alias,
                ctes,
                visiting,
                meter,
                strict_missing,
            ),
            xdb_parser::FromSource::Join {
                left,
                right,
                operator,
                using,
                ..
            } => {
                let mut lcols = self.infer_from_cols(
                    left,
                    raw_sql,
                    target_alias,
                    ctes,
                    visiting,
                    meter,
                    strict_missing,
                )?;
                let mut rcols = self.infer_from_cols(
                    right,
                    raw_sql,
                    target_alias,
                    ctes,
                    visiting,
                    meter,
                    strict_missing,
                )?;
                if target_alias.is_none() {
                    if let Some(u) = using {
                        rcols.retain(|(_, cname, _)| {
                            !u.iter().any(|un| un.name.eq_ignore_ascii_case(cname))
                        });
                    } else if (operator.flags & xdb_parser::join_flag::NATURAL) != 0 {
                        rcols.retain(|(_, cname, _)| {
                            !lcols
                                .iter()
                                .any(|(_, lname, _)| lname.eq_ignore_ascii_case(cname))
                        });
                    }
                }
                lcols.extend(rcols);
                Ok(lcols)
            }
        }
    }

    fn static_tvf_col_names(name: &str) -> Option<&'static [&'static str]> {
        match name.to_ascii_lowercase().as_str() {
            "pragma_table_info" => Some(&["cid", "name", "type", "notnull", "dflt_value", "pk"]),
            "pragma_table_xinfo" => {
                Some(&["cid", "name", "type", "notnull", "dflt_value", "pk", "hidden"])
            }
            "pragma_table_list" => Some(&["schema", "name", "type", "ncol", "wr", "strict"]),
            "pragma_index_list" => Some(&["seq", "name", "unique", "origin", "partial"]),
            "pragma_index_info" => Some(&["seqno", "cid", "name"]),
            "pragma_index_xinfo" => Some(&["seqno", "cid", "name", "desc", "coll", "key"]),
            "pragma_database_list" => Some(&["seq", "name", "file"]),
            "pragma_foreign_key_list" => {
                Some(&["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"])
            }
            "pragma_foreign_key_check" => Some(&["table", "rowid", "parent", "fkid"]),
            "pragma_collation_list" => Some(&["seq", "name"]),
            "pragma_compile_options" => Some(&["compile_options"]),
            "pragma_pragma_list" => Some(&["name"]),
            "pragma_function_list" => Some(&["name", "builtin", "type", "enc", "narg", "flags"]),
            "json_each" | "json_tree" => {
                Some(&["key", "value", "type", "atom", "id", "parent", "fullkey", "path"])
            }
            "generate_series" => Some(&["value"]),
            _ => None,
        }
    }
}

impl SqlCatalog for SqliteSqlCatalog<'_> {
    fn get_table(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<TableMeta>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        match self.db.schema().object(name) {
            Some(SchemaObject::Table(t)) => Ok(Some(Self::build_table_meta(t))),
            _ => Ok(None),
        }
    }

    fn get_table_arc(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<Arc<TableMeta>>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        match self.db.schema().object(name) {
            Some(SchemaObject::Table(t)) => {
                let meta = t.sql_meta.get_or_init(|| Arc::new(Self::build_table_meta(t)));
                Ok(Some(Arc::clone(meta)))
            }
            _ => Ok(None),
        }
    }

    fn get_without_rowid_pk(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<IndexMeta>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        let Some(SchemaObject::Table(t)) = self.db.schema().object(name) else {
            return Ok(None);
        };
        let Some(pk) = t.indexes.iter().find(|ix| ix.without_rowid_primary_key) else {
            return Ok(None);
        };
        Ok(Some(IndexMeta {
            name: pk.name.clone(),
            unique: pk.unique,
            key_columns: pk.columns[..pk.key_count]
                .iter()
                .map(|ic| if ic.cid >= 0 { Some(ic.cid as usize) } else { None })
                .collect(),
            key_collations: pk.columns[..pk.key_count]
                .iter()
                .map(|ic| ic.collation.clone())
                .collect(),
            key_desc: pk.columns[..pk.key_count]
                .iter()
                .map(|ic| ic.desc)
                .collect(),
            partial: pk.partial,
            where_clause: None,
        }))
    }

    fn has_table_valued_function(&self, name: &str) -> bool {
        Self::static_tvf_col_names(name).is_some()
    }

    fn get_view(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<ViewMeta>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        match self.db.schema().object(name) {
            Some(SchemaObject::View(v)) => Ok(Some(ViewMeta {
                name: v.name.clone(),
                select_sql: v.sql.clone(),
                columns: v.column_names.clone(),
            })),
            _ => Ok(None),
        }
    }

    fn scan_table(
        &self,
        schema: Option<&str>,
        name: &str,
        hint: &TableScanHint,
        needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>> {
        if !Self::check_schema(schema) {
            return Err(SqlError::runtime(format!(
                "no such table: {}.{name}",
                schema.unwrap_or("")
            )));
        }
        let reader = self.db.table(name).map_err(map_sqlite_err)?;
        self.scan_with_virtuals(&reader, hint, needed_columns, meter)
    }

    #[allow(clippy::type_complexity)]
    fn visit_table(
        &self,
        schema: Option<&str>,
        name: &str,
        hint: &TableScanHint,
        needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
        visit: &mut dyn FnMut(Option<i64>, &[SqlValue]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        if !Self::check_schema(schema) {
            return Err(SqlError::runtime(format!(
                "no such table: {}.{name}",
                schema.unwrap_or("")
            )));
        }
        let reader = self.db.table(name).map_err(map_sqlite_err)?;
        self.visit_with_virtuals(&reader, hint, needed_columns, meter, visit)
    }

    fn count_table_rows(
        &self,
        schema: Option<&str>,
        name: &str,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<u64>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        let reader = self.db.table(name).map_err(map_sqlite_err)?;
        let cnt = reader.count().map_err(map_sqlite_err)?;
        if let Some(m) = meter {
            m.charge_steps(1 + (cnt / 64))?;
        }
        Ok(Some(cnt))
    }

    fn call_tvf(
        &self,
        schema: Option<&str>,
        name: &str,
        args: &[SqlValue],
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<TableValuedResult>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        self.eval_pragma_tvf(&name.to_ascii_lowercase(), args, meter)
    }
}

// ───────────────────────────── VIRTUAL generated columns ─────────────────────────────

pub(crate) struct VirtualColumnPlan<'a> {
    scope_cols: Vec<ScopeColumn>,
    /// Evaluation order of VIRTUAL columns: `(cid, generated_expr, target_affinity)`.
    eval_order: Vec<(usize, &'a Expr, Affinity)>,
}

impl<'a> VirtualColumnPlan<'a> {
    pub(crate) fn build_for(table: &'a TableInfo, needed_columns: Option<&[usize]>) -> SqlResult<Self> {
        let scope_cols: Vec<ScopeColumn> = table
            .columns
            .iter()
            .map(|c| ScopeColumn {
                table: Some(table.name.clone()),
                name: c.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_rowid_alias: false,
            })
            .collect();

        // Collect only the requested VIRTUAL generated columns (or all if `needed_columns` is None).
        let mut virt_cids: Vec<usize> = Vec::new();
        for col in &table.columns {
            if col.generated == Some(GeneratedKind::Virtual)
                && needed_columns.is_none_or(|n| n.contains(&col.cid))
            {
                virt_cids.push(col.cid);
            }
        }

        // Topological sort with cycle detection (0 = unvisited, 1 = visiting, 2 = done).
        let mut state = vec![0u8; table.columns.len()];
        let mut order: Vec<usize> = Vec::with_capacity(virt_cids.len());

        fn visit(
            cid: usize,
            table: &TableInfo,
            state: &mut [u8],
            order: &mut Vec<usize>,
        ) -> SqlResult<()> {
            if state[cid] == 2 {
                return Ok(());
            }
            if state[cid] == 1 {
                return Err(SqlError::runtime(format!(
                    "generated column loop on \"{}\"",
                    table.columns[cid].name
                )));
            }
            state[cid] = 1;
            if let Some(ref expr) = table.definition.columns[cid].generated {
                let mut deps = Vec::new();
                collect_col_deps(expr, table, &mut deps);
                for dep_cid in deps {
                    if table.columns[dep_cid].generated == Some(GeneratedKind::Virtual) {
                        visit(dep_cid, table, state, order)?;
                    }
                }
            }
            state[cid] = 2;
            order.push(cid);
            Ok(())
        }

        for &cid in &virt_cids {
            visit(cid, table, &mut state, &mut order)?;
        }

        let mut eval_order = Vec::with_capacity(order.len());
        for cid in order {
            let expr = table.definition.columns[cid].generated.as_ref().ok_or_else(|| {
                SqlError::runtime(format!(
                    "missing generation expression for virtual column \"{}\"",
                    table.columns[cid].name
                ))
            })?;
            eval_order.push((cid, expr, table.columns[cid].affinity));
        }

        Ok(Self {
            scope_cols,
            eval_order,
        })
    }

    pub(crate) fn evaluate(&self, row: &mut [SqlValue], options: &QueryOptions) -> SqlResult<()> {
        for &(cid, expr, aff) in &self.eval_order {
            let val = eval_generated_expr(expr, &self.scope_cols, row, aff, options)?;
            row[cid] = val;
        }
        Ok(())
    }
}

fn collect_col_deps(expr: &Expr, table: &TableInfo, out: &mut Vec<usize>) {
    if let Expr::Column(c) = expr {
        if let Some(col) = table
            .columns
            .iter()
            .find(|col| col.name.eq_ignore_ascii_case(&c.column.name))
        {
            if !out.contains(&col.cid) {
                out.push(col.cid);
            }
        }
        return;
    }
    for child in expr_children(expr) {
        collect_col_deps(child, table, out);
    }
}

fn bound_to_int(v: &SqlValue) -> Option<i64> {
    match v {
        SqlValue::Int(i) => Some(*i),
        SqlValue::Real(r) => {
            let iv = xdb_core::value::real_to_i64(*r);
            if xdb_core::value::real_same_as_int(*r, iv) {
                Some(iv)
            } else {
                None
            }
        }
        SqlValue::Text(b) => {
            let parsed = xdb_core::value::atoi64(b);
            if parsed.rc == 0 {
                Some(parsed.value)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ───────────────────────────── SqliteDatabase SQL methods ─────────────────────────────

impl SqliteDatabase {
    /// Build default [`QueryOptions`] pre-wired with this database's text encoding and [`Meter`].
    pub fn default_query_options(&self) -> QueryOptions {
        let meter = if self.btree().pager().is_noop_meter() {
            None
        } else {
            Some(self.btree().pager().meter().clone())
        };
        QueryOptions {
            collations: CollationRegistry::new(self.encoding()),
            meter,
            ..QueryOptions::default()
        }
    }

    /// Execute a SQLite `SELECT` / `VALUES` / `WITH` query against this database.
    pub fn query(&self, sql: &str) -> SqliteResult<QueryResult> {
        self.query_with_params(sql, &QueryParams::empty())
    }

    /// Execute a parameterized SQLite `SELECT` query against this database.
    pub fn query_with_params(&self, sql: &str, params: &QueryParams) -> SqliteResult<QueryResult> {
        let opts = self.default_query_options();
        self.query_with_options(sql, params, &opts)
    }

    /// Execute a SQL query against this database with full [`QueryOptions`] control
    /// (dialect, custom functions/collations, custom meter, deterministic clock, etc.).
    pub fn query_with_options(
        &self,
        sql: &str,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<QueryResult> {
        if self.is_closed() {
            return Err(SqliteError::DatabaseClosed);
        }
        if options.meter.is_none() && !self.btree().pager().is_noop_meter() {
            let mut effective = options.clone();
            effective.meter = Some(self.btree().pager().meter().clone());
            let catalog = SqliteSqlCatalog::new(self, &effective);
            return execute_query(&catalog, sql, params, &effective).map_err(SqliteError::from);
        }
        let catalog = SqliteSqlCatalog::new(self, options);
        execute_query(&catalog, sql, params, options).map_err(SqliteError::from)
    }

    /// Execute an already-parsed [`SelectStmt`] against this database (avoids re-parsing).
    pub(crate) fn query_parsed_with_options(
        &self,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<QueryResult> {
        if self.is_closed() {
            return Err(SqliteError::DatabaseClosed);
        }
        if options.meter.is_none() && !self.btree().pager().is_noop_meter() {
            let mut effective = options.clone();
            effective.meter = Some(self.btree().pager().meter().clone());
            let catalog = SqliteSqlCatalog::new(self, &effective);
            return execute_select_stmt(&catalog, stmt, params, &effective)
                .map_err(SqliteError::from);
        }
        let catalog = SqliteSqlCatalog::new(self, options);
        execute_select_stmt(&catalog, stmt, params, options).map_err(SqliteError::from)
    }
}
