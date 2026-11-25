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
}

pub(crate) fn live_catalog_for<'a>(
    db: &'a SqliteDatabase,
    options: &'a QueryOptions,
) -> SqliteSqlCatalog<'a> {
    SqliteSqlCatalog { db, options }
}

impl<'a> SqliteSqlCatalog<'a> {
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

    fn eval_pragma_tvf(&self, lower_name: &str, args: &[SqlValue]) -> SqlResult<Option<TableValuedResult>> {
        let arg_str = |idx: usize| -> Option<String> {
            args.get(idx).and_then(|v| {
                xdb_sql::utf8::text_bytes_of(v).map(|b| String::from_utf8_lossy(&b).into_owned())
            })
        };

        match lower_name {
            "pragma_table_info" | "pragma_table_xinfo" => {
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
                let Some(table) = self.db.schema().table(&tname) else {
                    return Ok(Some(TableValuedResult {
                        columns,
                        rows: Vec::new(),
                    }));
                };
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
                Ok(Some(TableValuedResult { columns, rows }))
            }
            "pragma_index_list" => {
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
            _ => Ok(None),
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
        _meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<TableValuedResult>> {
        if !Self::check_schema(schema) {
            return Ok(None);
        }
        self.eval_pragma_tvf(&name.to_ascii_lowercase(), args)
    }
}

// ───────────────────────────── VIRTUAL generated columns ─────────────────────────────

struct VirtualColumnPlan<'a> {
    scope_cols: Vec<ScopeColumn>,
    /// Evaluation order of VIRTUAL columns: `(cid, generated_expr, target_affinity)`.
    eval_order: Vec<(usize, &'a Expr, Affinity)>,
}

impl<'a> VirtualColumnPlan<'a> {
    fn build_for(table: &'a TableInfo, needed_columns: Option<&[usize]>) -> SqlResult<Self> {
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

    fn evaluate(&self, row: &mut [SqlValue], options: &QueryOptions) -> SqlResult<()> {
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
            let catalog = SqliteSqlCatalog {
                db: self,
                options: &effective,
            };
            return execute_query(&catalog, sql, params, &effective).map_err(SqliteError::from);
        }
        let catalog = SqliteSqlCatalog { db: self, options };
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
            let catalog = SqliteSqlCatalog {
                db: self,
                options: &effective,
            };
            return execute_select_stmt(&catalog, stmt, params, &effective)
                .map_err(SqliteError::from);
        }
        let catalog = SqliteSqlCatalog { db: self, options };
        execute_select_stmt(&catalog, stmt, params, options).map_err(SqliteError::from)
    }
}
