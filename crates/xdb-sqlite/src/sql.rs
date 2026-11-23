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

use xdb_core::value::{mem_compare, CollationRegistry};
use xdb_core::{Affinity, Meter, SqlValue};
use xdb_parser::{expr_children, Expr, IndexOrigin};
use xdb_sql::{
    eval_generated_expr, execute_query, Bound, ColumnMeta, IndexMeta, QueryOptions, QueryParams,
    QueryResult, ScannedRow, ScopeColumn, SqlCatalog, SqlError, SqlResult, TableMeta,
    TableScanHint, TableValuedResult, ViewMeta,
};

use crate::access::SearchKey;
use crate::btree::{SeekOp, SeekResult};
use crate::database::{ColumnSpec, ScanOptions, SqliteDatabase, TableReader};
use crate::errors::{SqliteError, SqliteResult};
use crate::schema::catalog::GeneratedKind;
use crate::schema::{ColumnHiddenKind, SchemaObject, TableInfo};

type RawRowList = Vec<(Option<i64>, Vec<SqlValue>)>;

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
            Some(s) => s.eq_ignore_ascii_case("main") || s.eq_ignore_ascii_case("temp"),
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
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>> {
        let info = reader.info();
        let stored_specs: Vec<ColumnSpec> = info
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| ColumnSpec::Cid(c.cid))
            .collect();
        let stored_cids: Vec<usize> = info
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();

        let scan_opts = ScanOptions {
            columns: Some(stored_specs),
            rowid: !info.without_rowid,
        };

        let raw_rows: Vec<(Option<i64>, Vec<SqlValue>)> =
            self.collect_raw_rows(reader, hint, &scan_opts, meter)?;

        // Reconstruct full column vector (length = info.columns.len()) and compute VIRTUAL columns.
        let total_cols = info.columns.len();
        let virt_plan = if info.has_virtual_columns {
            Some(VirtualColumnPlan::build(info)?)
        } else {
            None
        };

        let mut out = Vec::with_capacity(raw_rows.len());
        for (rowid, stored_vals) in raw_rows {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let mut full = if info.has_virtual_columns {
                let mut v = vec![SqlValue::Null; total_cols];
                for (&cid, val) in stored_cids.iter().zip(stored_vals) {
                    v[cid] = val;
                }
                v
            } else {
                stored_vals
            };
            if let Some(ref plan) = virt_plan {
                plan.evaluate(&mut full, self.options)?;
            }
            out.push(ScannedRow {
                rowid,
                values: full,
            });
        }
        Ok(out)
    }

    fn collect_raw_rows(
        &self,
        reader: &TableReader,
        hint: &TableScanHint,
        scan_opts: &ScanOptions,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<RawRowList> {
        let info = reader.info();
        let has_rowid = !info.without_rowid;

        match hint {
            TableScanHint::RowidRange { low, high } if has_rowid => {
                if let Some(rows) =
                    self.try_rowid_range_scan(reader, low, high, scan_opts, meter)?
                {
                    return Ok(rows);
                }
            }
            TableScanHint::IndexEq { index_name, values } if has_rowid => {
                if let Some(rows) =
                    self.try_index_eq_scan(reader, index_name, values, scan_opts, meter)?
                {
                    return Ok(rows);
                }
            }
            _ => {}
        }

        // Default full B-tree scan
        let mut cur = reader.cursor(Some(scan_opts)).map_err(map_sqlite_err)?;
        let mut rows = Vec::new();
        while cur.next().map_err(map_sqlite_err)? {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let mut r = cur.row().map_err(map_sqlite_err)?;
            if has_rowid {
                let rid = r.remove(0).as_i64();
                rows.push((rid, r));
            } else {
                rows.push((None, r));
            }
        }
        Ok(rows)
    }

    fn try_rowid_range_scan(
        &self,
        reader: &TableReader,
        low: &Bound,
        high: &Bound,
        scan_opts: &ScanOptions,
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
                // Fast point lookup via `TableReader::get`
                let point_opts = ScanOptions {
                    columns: scan_opts.columns.clone(),
                    rowid: false,
                };
                if let Some(m) = meter {
                    m.charge_steps(1)?;
                }
                return match reader.get(lo, Some(&point_opts)).map_err(map_sqlite_err)? {
                    Some(vals) => Ok(Some(vec![(Some(lo), vals)])),
                    None => Ok(Some(Vec::new())),
                };
            }
        }

        // Range scan using `RowCursor::seek` + sequential leaf traversal (true O(log N + K)!)
        let range_opts = ScanOptions {
            columns: scan_opts.columns.clone(),
            rowid: false,
        };
        let mut rcur = reader.cursor(Some(&range_opts)).map_err(map_sqlite_err)?;
        let positioned = if let Some(lo) = lo_i64 {
            rcur.seek(lo).map_err(map_sqlite_err)? != SeekResult::Eof
        } else {
            rcur.next().map_err(map_sqlite_err)?
        };
        if !positioned {
            return Ok(Some(Vec::new()));
        }

        let mut rows = Vec::new();
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
            let vals = rcur.row().map_err(map_sqlite_err)?;
            rows.push((Some(rid), vals));
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
        scan_opts: &ScanOptions,
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
        let Ok(mut search_key) = SearchKey::new(key_info.clone(), prefix_values, 1) else {
            return Ok(None);
        };
        let mut icur = idx_reader.cursor().map_err(map_sqlite_err)?;
        if !icur
            .seek_key(&mut search_key, SeekOp::Ge)
            .map_err(map_sqlite_err)?
        {
            return Ok(Some(Vec::new()));
        }

        let point_opts = ScanOptions {
            columns: scan_opts.columns.clone(),
            rowid: false,
        };
        let mut rows = Vec::new();
        let mut seen_rowids = HashSet::new();
        let prefix_len = prefix_values.len();

        loop {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let entry = icur.row().map_err(map_sqlite_err)?;
            if entry.len() < prefix_len + 1 {
                break;
            }
            // Check prefix still matches under the index key collations.
            let mut matches = true;
            for (k, target) in prefix_values.iter().enumerate() {
                let coll = key_info.collations[k].as_ref();
                if mem_compare(&entry[k], target, coll) != Ordering::Equal {
                    matches = false;
                    break;
                }
            }
            if !matches {
                break;
            }
            // Trailing field of a rowid secondary index entry is the table rowid.
            let Some(rid) = entry.last().and_then(SqlValue::as_i64) else {
                return Ok(None);
            };
            if seen_rowids.insert(rid) {
                if let Some(vals) = reader.get(rid, Some(&point_opts)).map_err(map_sqlite_err)? {
                    rows.push((Some(rid), vals));
                }
            }
            if !icur.next().map_err(map_sqlite_err)? {
                break;
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
        _needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>> {
        if !Self::check_schema(schema) {
            return Err(SqlError::runtime(format!(
                "no such table: {}.{name}",
                schema.unwrap_or("")
            )));
        }
        let reader = self.db.table(name).map_err(map_sqlite_err)?;
        self.scan_with_virtuals(&reader, hint, meter)
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
    fn build(table: &'a TableInfo) -> SqlResult<Self> {
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

        // Collect all VIRTUAL generated columns and their dependencies on other VIRTUAL columns.
        let mut virt_cids: Vec<usize> = Vec::new();
        for col in &table.columns {
            if col.generated == Some(GeneratedKind::Virtual) {
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
        QueryOptions {
            collations: CollationRegistry::new(self.encoding()),
            meter: Some(self.btree().pager().meter().clone()),
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
        let mut effective = options.clone();
        if effective.meter.is_none() {
            effective.meter = Some(self.btree().pager().meter().clone());
        }
        let catalog = SqliteSqlCatalog {
            db: self,
            options: &effective,
        };
        execute_query(&catalog, sql, params, &effective).map_err(SqliteError::from)
    }
}
