//! Full read-write SQLite 3 connection (`SqliteConnection`) supporting DDL, DML,
//! `UPSERT`, `RETURNING`, constraints (`NOT NULL`, `UNIQUE`, `PRIMARY KEY`, `CHECK`,
//! `STRICT`, `GENERATED`), transactions & nested savepoints, `PRAGMA`, and crash-safe
//! rollback journal commits.

use std::collections::HashSet;
use std::sync::Arc;

use xdb_core::value::{apply_affinity, boolean_value, mem_compare, must_be_int, CollationRegistry};
use xdb_core::{Affinity, Meter, SqlValue, StorageClass, TextEncoding, Vfs};
use xdb_parser::{
    build_index, build_table, parse_expression, parse_statement_at_metered, AlterTableAction,
    AlterTableStmt, BuildOptions, ColumnConstraint, ConflictAction, CreateIndexStmt,
    CreateTableStmt, CreateViewStmt, DefaultForm, DeleteStmt, Dialect, DropObjectType, DropStmt,
    Expr, GeneratedStorage, InsertStmt, OnError, ParseOptions, PragmaStmt, ResultColumn,
    SelectBody, SelectCore, SelectStmt, SetClause, Statement, TableConstraint, UpdateStmt,
    UpsertAction,
};
use xdb_sql::{
    eval_expr, execute_select_stmt_detailed, CatalogSubqueryRunner, EvalEnv, EvalScope,
    NoSubqueries, QueryOptions, QueryParams, QueryResult, ScopeColumn,
};

use crate::access::{key_info_for_index, make_key_info, KeyFieldSpec, KeyInfo};
use crate::database::{open_from_page_source_cached, ColumnSpec, ScanOptions, SqliteDatabase};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::BtreePageType;
use crate::schema::catalog::{GeneratedKind, SchemaCatalog};
use crate::schema::{ColumnHiddenKind, IndexInfo, SchemaObject, TableInfo};
use crate::write::btree::{
    check_integrity, create_btree, drop_btree, index_contains_prefix, index_delete, index_insert,
    table_contains_rowid, table_delete, table_insert, table_max_rowid,
};
use crate::write::pager::WritePager;
use crate::write::record::encode_record_with_format;

/// Options for opening a read-write [`SqliteConnection`].
#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    /// Initial page size when creating a brand-new database (default `4096`).
    pub page_size: u32,
    /// SQL dialect (`Dialect::Sqlite` or `Dialect::Xdb`).
    pub dialect: Dialect,
    /// Optional sandbox resource meter.
    pub meter: Option<Arc<dyn Meter>>,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            page_size: 4096,
            dialect: Dialect::Sqlite,
            meter: None,
        }
    }
}

/// Output of [`SqliteConnection::execute`] / [`SqliteConnection::execute_with_params`].
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    /// Output column names (populated for `SELECT`, `PRAGMA`, and `RETURNING`).
    pub columns: Vec<String>,
    /// Output rows (populated for `SELECT`, `PRAGMA`, and `RETURNING`).
    pub rows: Vec<Vec<SqlValue>>,
    /// Number of rows inserted, updated, or deleted by the last DML statement.
    pub changes: i64,
    /// Rowid of the most recently inserted rowid-table row.
    pub last_insert_rowid: i64,
}

/// Read-write SQLite 3 database connection with ACID rollback journal & nested savepoints.
#[derive(Debug)]
pub struct SqliteConnection {
    pager: WritePager,
    dialect: Dialect,
    changes: i64,
    total_changes: i64,
    last_insert_rowid: i64,
    foreign_keys: bool,
    busy_timeout: i64,
    synchronous: i64,
    closed: bool,
    pending_error_conflict: Option<ConflictAction>,
    cached_schema: std::sync::Mutex<Option<(u64, Arc<SchemaCatalog>)>>,
}

impl SqliteConnection {
    /// Open a brand-new empty in-memory read-write SQLite database.
    pub fn open_in_memory(options: ConnectionOptions) -> SqliteResult<Self> {
        let pager = WritePager::new_in_memory(options.page_size, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    /// Load a raw SQLite `.db` byte image (plus optional `-wal`/`-journal`) into a writable
    /// in-memory connection.
    pub fn open_from_bytes(
        main_bytes: &[u8],
        wal_bytes: Option<&[u8]>,
        journal_bytes: Option<&[u8]>,
        options: ConnectionOptions,
    ) -> SqliteResult<Self> {
        let pager = WritePager::from_bytes(main_bytes, wal_bytes, journal_bytes, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    /// Open (or create) a durable read-write SQLite database over `vfs` at `path`.
    pub fn open(
        vfs: Arc<dyn Vfs>,
        path: &str,
        options: ConnectionOptions,
    ) -> SqliteResult<Self> {
        let pager = WritePager::open_vfs(vfs, path, options.page_size, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    fn from_pager(pager: WritePager, dialect: Dialect) -> Self {
        Self {
            pager,
            dialect,
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            foreign_keys: false,
            busy_timeout: 0,
            synchronous: 2,
            closed: false,
            pending_error_conflict: None,
            cached_schema: std::sync::Mutex::new(None),
        }
    }

    fn constraint_err(&mut self, effective: ConflictAction, msg: String) -> SqliteError {
        self.pending_error_conflict = Some(effective);
        SqliteError::Sql(msg)
    }

    pub(super) fn check_open(&self) -> SqliteResult<()> {
        if self.closed {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn page_size(&self) -> u32 {
        self.pager.page_size()
    }

    pub fn page_count(&self) -> u32 {
        self.pager.page_count()
    }

    pub fn encoding(&self) -> TextEncoding {
        self.pager.encoding()
    }

    pub fn changes(&self) -> i64 {
        self.changes
    }

    pub fn total_changes(&self) -> i64 {
        self.total_changes
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    pub fn in_transaction(&self) -> bool {
        self.pager.in_transaction()
    }

    pub fn meter(&self) -> &Arc<dyn Meter> {
        self.pager.meter()
    }

    /// Export the live SQLite 3 binary database image (readable directly by C SQLite).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.pager.to_bytes()
    }

    /// Returns `true` if this connection has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close this connection (rolling back any uncommitted transaction and releasing pages).
    pub fn close(&mut self) -> SqliteResult<()> {
        if self.closed {
            return Ok(());
        }
        if self.pager.in_transaction() {
            let _ = self.pager.rollback();
        }
        self.pager.close();
        if let Ok(mut guard) = self.cached_schema.lock() {
            *guard = None;
        }
        self.closed = true;
        Ok(())
    }

    /// Build a live read-only [`SqliteDatabase`] view over the current uncommitted pager state.
    pub fn live_view(&self) -> SqliteResult<SqliteDatabase> {
        self.check_open()?;
        let gen = self.pager.schema_gen();
        let cached = self
            .cached_schema
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|(cg, s)| (*cg == gen).then(|| Arc::clone(s))));
        let hit = cached.is_some();
        let view = open_from_page_source_cached(
            self.pager.snapshot_page_source(),
            self.pager.path().to_string(),
            Some(self.pager.meter().clone()),
            cached,
        )?;
        if !hit {
            if let Ok(mut guard) = self.cached_schema.lock() {
                *guard = Some((gen, Arc::clone(view.schema_arc())));
            }
        }
        Ok(view)
    }

    fn query_options(&self, dialect: Dialect) -> QueryOptions {
        QueryOptions {
            dialect,
            collations: CollationRegistry::new(self.pager.encoding()),
            meter: Some(self.pager.meter().clone()),
            changes: self.changes,
            total_changes: self.total_changes,
            last_insert_rowid: self.last_insert_rowid,
            ..QueryOptions::default()
        }
    }

    /// Run a read-only `SELECT` query against the live connection state.
    pub fn query(&self, sql: &str) -> SqliteResult<QueryResult> {
        self.query_with_params(sql, &QueryParams::empty())
    }

    /// Run a parameterized read-only `SELECT` query against the live connection state.
    pub fn query_with_params(&self, sql: &str, params: &QueryParams) -> SqliteResult<QueryResult> {
        let view = self.live_view()?;
        let opts = self.query_options(self.dialect);
        view.query_with_options(sql, params, &opts)
    }

    /// Execute one or more `;`-separated SQL statements (DDL, DML, transactions, PRAGMA, SELECT).
    pub fn execute(&mut self, sql: &str) -> SqliteResult<ExecResult> {
        self.execute_with_params(sql, &QueryParams::empty())
    }

    /// Execute one or more `;`-separated SQL statements with bound parameters.
    pub fn execute_with_params(
        &mut self,
        sql: &str,
        params: &QueryParams,
    ) -> SqliteResult<ExecResult> {
        self.execute_with_dialect(sql, params, self.dialect)
    }

    /// Execute one or more `;`-separated SQL statements using an explicit SQL dialect.
    pub fn execute_with_dialect(
        &mut self,
        sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        self.check_open()?;
        let parse_opts = ParseOptions {
            dialect,
            ..ParseOptions::default()
        };
        let mut offset = 0usize;
        let mut last = ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        };

        while offset < sql.len() {
            let parsed =
                parse_statement_at_metered(sql, offset, &parse_opts, self.pager.meter().as_ref())
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
            let stmt_end = parsed.end;
            if let Some(stmt) = parsed.statement {
                let raw_stmt_sql = sql[offset..stmt_end].trim().trim_end_matches(';').trim();
                last = self.exec_one_stmt(&stmt, sql, raw_stmt_sql, params, dialect)?;
            }
            if stmt_end <= offset {
                break;
            }
            offset = stmt_end;
        }
        Ok(last)
    }

    fn exec_one_stmt(
        &mut self,
        stmt: &Statement,
        full_sql: &str,
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        match stmt {
            Statement::Select(sel) => {
                let view = self.live_view()?;
                let opts = self.query_options(dialect);
                let res = self.eval_select_stmt(&view, sel, params, &opts)?;
                Ok(ExecResult {
                    columns: res.columns,
                    rows: res.rows,
                    changes: self.changes,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            Statement::Begin { .. } => {
                self.pager.begin()?;
                Ok(self.empty_exec_result())
            }
            Statement::Commit { .. } => {
                self.pager.commit()?;
                Ok(self.empty_exec_result())
            }
            Statement::Rollback { savepoint, .. } => {
                if let Some(sp) = savepoint {
                    self.pager.rollback_to_savepoint(&sp.name)?;
                } else {
                    self.pager.rollback()?;
                }
                Ok(self.empty_exec_result())
            }
            Statement::Savepoint { name, .. } => {
                self.pager.push_savepoint(Some(name.name.clone()));
                Ok(self.empty_exec_result())
            }
            Statement::Release { name, .. } => {
                self.pager.release_savepoint(&name.name)?;
                Ok(self.empty_exec_result())
            }
            Statement::Pragma(p) => self.exec_pragma(p),
            Statement::CreateTable(ct) => self.run_mutating_stmt(|conn| {
                conn.exec_create_table(ct, full_sql, raw_sql, params, dialect)
            }),
            Statement::CreateIndex(ci) => {
                self.run_mutating_stmt(|conn| conn.exec_create_index(ci, raw_sql))
            }
            Statement::CreateView(cv) => {
                self.run_mutating_stmt(|conn| conn.exec_create_view(cv, raw_sql))
            }
            Statement::Drop(dr) => self.run_mutating_stmt(|conn| conn.exec_drop(dr)),
            Statement::AlterTable(al) => {
                self.run_mutating_stmt(|conn| conn.exec_alter_table(al, full_sql))
            }
            Statement::Insert(ins) => {
                self.run_mutating_stmt(|conn| conn.exec_insert(ins, params, dialect))
            }
            Statement::Update(upd) => {
                self.run_mutating_stmt(|conn| conn.exec_update(upd, params, dialect))
            }
            Statement::Delete(del) => {
                self.run_mutating_stmt(|conn| conn.exec_delete(del, params, dialect))
            }
            Statement::Vacuum { .. } => {
                // Re-compact in-place by committing current clean pages.
                if self.pager.in_transaction() {
                    return Err(SqliteError::Sql(
                        "cannot VACUUM from within a transaction".to_string(),
                    ));
                }
                self.pager.autocommit_flush()?;
                Ok(self.empty_exec_result())
            }
            Statement::Analyze { .. } | Statement::Reindex { .. } => {
                Ok(self.empty_exec_result())
            }
            other => Err(SqliteError::Sql(format!(
                "unsupported statement in Phase 3 engine: {other:?}"
            ))),
        }
    }

    fn empty_exec_result(&self) -> ExecResult {
        ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        }
    }

    /// Execute a mutating statement inside an automatic statement savepoint with proper
    /// `ON CONFLICT ABORT`/`FAIL`/`ROLLBACK` and auto-commit handling.
    fn run_mutating_stmt<F>(&mut self, f: F) -> SqliteResult<ExecResult>
    where
        F: FnOnce(&mut Self) -> SqliteResult<ExecResult>,
    {
        self.pending_error_conflict = None;
        self.pager.push_savepoint(None);
        match f(self) {
            Ok(res) => {
                self.pager.commit_statement_and_autocommit()?;
                Ok(res)
            }
            Err(e) => {
                let eff = self.pending_error_conflict.take();
                match eff {
                    Some(ConflictAction::Rollback) if self.pager.in_transaction() => {
                        let _ = self.pager.rollback();
                    }
                    Some(ConflictAction::Fail) => {
                        let _ = self.pager.commit_statement_and_autocommit();
                    }
                    _ => {
                        self.pager.rollback_statement_savepoint();
                    }
                }
                Err(e)
            }
        }
    }

    // ───────────────────────────── sqlite_schema helpers ─────────────────────────────

    fn insert_schema_row(
        &mut self,
        obj_type: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: Option<&str>,
    ) -> SqliteResult<i64> {
        self.pager.ensure_initialized()?;
        let next_rid = table_max_rowid(&self.pager, 1)?.unwrap_or(0) + 1;
        let vals = vec![
            SqlValue::text(obj_type),
            SqlValue::text(name),
            SqlValue::text(tbl_name),
            SqlValue::Int(i64::from(root_page)),
            sql.map_or(SqlValue::Null, SqlValue::text),
        ];
        let payload = encode_record_with_format(
            &vals,
            self.pager.encoding(),
            self.pager.schema_format(),
        );
        table_insert(&mut self.pager, 1, next_rid, &payload, false)?;
        self.pager.invalidate_schema_cache();
        Ok(next_rid)
    }

    fn update_schema_row(
        &mut self,
        rowid: i64,
        obj_type: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: Option<&str>,
    ) -> SqliteResult<()> {
        let vals = vec![
            SqlValue::text(obj_type),
            SqlValue::text(name),
            SqlValue::text(tbl_name),
            SqlValue::Int(i64::from(root_page)),
            sql.map_or(SqlValue::Null, SqlValue::text),
        ];
        let payload = encode_record_with_format(
            &vals,
            self.pager.encoding(),
            self.pager.schema_format(),
        );
        table_insert(&mut self.pager, 1, rowid, &payload, true)?;
        self.pager.invalidate_schema_cache();
        Ok(())
    }

    fn ensure_sqlite_sequence(&mut self) -> SqliteResult<u32> {
        let view = self.live_view()?;
        if let Some(seq) = view.schema().table("sqlite_sequence") {
            return Ok(seq.root_page);
        }
        let root = create_btree(&mut self.pager, BtreePageType::TableLeaf)?;
        self.insert_schema_row(
            "table",
            "sqlite_sequence",
            "sqlite_sequence",
            root,
            Some("CREATE TABLE sqlite_sequence(name,seq)"),
        )?;
        Ok(root)
    }

    fn get_sqlite_sequence(&self, table_name: &str) -> SqliteResult<Option<(i64, i64)>> {
        let view = self.live_view()?;
        let Some(seq_tbl) = view.schema().table("sqlite_sequence") else {
            return Ok(None);
        };
        let reader = view.table(&seq_tbl.name)?;
        let opts = ScanOptions {
            columns: None,
            rowid: true,
        };
        let mut cur = reader.cursor(Some(&opts))?;
        while cur.next()? {
            let r = cur.row()?;
            if let (Some(rid), Some(name), Some(seq)) = (r[0].as_i64(), r[1].as_str(), r[2].as_i64())
            {
                if name.eq_ignore_ascii_case(table_name) {
                    return Ok(Some((rid, seq)));
                }
            }
        }
        Ok(None)
    }

    fn update_sqlite_sequence(&mut self, table_name: &str, new_seq: i64) -> SqliteResult<()> {
        let clamped = new_seq.max(0);
        let seq_root = self.ensure_sqlite_sequence()?;
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        if let Some((rid, old_seq)) = self.get_sqlite_sequence(table_name)? {
            if clamped > old_seq {
                let vals = vec![SqlValue::text(table_name), SqlValue::Int(clamped)];
                let payload = encode_record_with_format(&vals, enc, fmt);
                table_insert(&mut self.pager, seq_root, rid, &payload, true)?;
            }
        } else {
            let rid = table_max_rowid(&self.pager, seq_root)?.unwrap_or(0) + 1;
            let vals = vec![SqlValue::text(table_name), SqlValue::Int(clamped)];
            let payload = encode_record_with_format(&vals, enc, fmt);
            table_insert(&mut self.pager, seq_root, rid, &payload, false)?;
        }
        Ok(())
    }

    // ───────────────────────────── DDL ─────────────────────────────

    fn exec_create_table(
        &mut self,
        stmt: &CreateTableStmt,
        full_sql: &str,
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        if stmt.temp {
            return Err(SqliteError::Sql(
                "TEMP tables are not supported in single-database mode".to_string(),
            ));
        }
        self.pager.ensure_initialized()?;
        let tname = &stmt.name.name;
        if tname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {tname}"
            )));
        }
        {
            let view = self.live_view()?;
            if view.schema().index(tname).is_some() {
                return Err(SqliteError::Sql(format!(
                    "there is already an index named {tname}"
                )));
            }
            if view.schema().object(tname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("table {tname} already exists")));
            }
        }

        // Handle `CREATE TABLE t AS SELECT ...`
        if let Some(ref sel) = stmt.as_select {
            let view = self.live_view()?;
            let opts = self.query_options(dialect);
            let (qcols, qrows) = self.eval_select_stmt_detailed(&view, sel, params, &opts)?;
            drop(view);

            let mut used_names: HashSet<String> = HashSet::with_capacity(qcols.len());
            let mut col_defs = Vec::with_capacity(qcols.len());
            for sc in &qcols {
                let cname = &sc.name;
                let mut unique_name = cname.clone();
                if !used_names.insert(unique_name.to_ascii_lowercase()) {
                    let mut suffix = 1u64;
                    loop {
                        let cand = format!("{cname}:{suffix}");
                        if used_names.insert(cand.to_ascii_lowercase()) {
                            unique_name = cand;
                            break;
                        }
                        suffix += 1;
                    }
                }
                let col_type = match sc.affinity {
                    Affinity::Integer => "INT",
                    Affinity::Text => "TEXT",
                    Affinity::Real => "REAL",
                    Affinity::Numeric | Affinity::Flexnum => "NUM",
                    Affinity::Blob | Affinity::None => "",
                };
                let escaped = unique_name.replace('"', "\"\"");
                if col_type.is_empty() {
                    col_defs.push(format!("\"{escaped}\""));
                } else {
                    col_defs.push(format!("\"{escaped}\" {col_type}"));
                }
            }
            let escaped_tname = tname.replace('"', "\"\"");
            let synth_sql = format!("CREATE TABLE \"{escaped_tname}\"({})", col_defs.join(","));
            let root_pg = create_btree(&mut self.pager, BtreePageType::TableLeaf)?;
            self.insert_schema_row("table", tname, tname, root_pg, Some(&synth_sql))?;
            self.pager.bump_schema_cookie()?;

            let enc = self.pager.encoding();
            let fmt = self.pager.schema_format();
            let mut rid = 0i64;
            for row in qrows {
                rid += 1;
                let payload = encode_record_with_format(&row, enc, fmt);
                table_insert(&mut self.pager, root_pg, rid, &payload, false)?;
            }
            return Ok(self.empty_exec_result());
        }

        // Validate table definition with SQLite's exact `build_table` (AST spans index `full_sql`)!
        let build_opts = BuildOptions {
            schema: false,
            file_format: self.pager.schema_format().clamp(1, 4),
            ..BuildOptions::default()
        };
        let def = build_table(stmt, full_sql, &build_opts)
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        let _ = generated_eval_order_from_defs(&def.name, &def.columns)?;

        let stored_sql = strip_if_not_exists(raw_sql, stmt.if_not_exists);

        let root_kind = if def.without_rowid {
            BtreePageType::IndexLeaf
        } else {
            BtreePageType::TableLeaf
        };
        let table_root = create_btree(&mut self.pager, root_kind)?;
        self.insert_schema_row("table", tname, tname, table_root, Some(&stored_sql))?;

        // Create `sqlite_autoindex_*` B-trees for UNIQUE / non-IPK PRIMARY KEY constraints.
        for idx_def in &def.indexes {
            if idx_def.without_rowid_primary_key {
                continue; // WITHOUT ROWID PK shares `table_root`!
            }
            let idx_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
            self.insert_schema_row("index", &idx_def.name, tname, idx_root, None)?;
        }

        // Create `sqlite_sequence` if table uses `AUTOINCREMENT`.
        if def.autoincrement {
            self.ensure_sqlite_sequence()?;
        }

        self.pager.bump_schema_cookie()?;
        Ok(self.empty_exec_result())
    }

    fn exec_create_index(
        &mut self,
        stmt: &CreateIndexStmt,
        raw_sql: &str,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        self.pager.ensure_initialized()?;
        let iname = &stmt.name.name;
        if iname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {iname}"
            )));
        }
        let tname = &stmt.table.name;
        let table_name_canon: String;
        {
            let view = self.live_view()?;
            if view.schema().index(iname).is_some() || view.schema().object(iname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("index {iname} already exists")));
            }
            let Some(tbl) = view.schema().table(tname) else {
                return Err(SqliteError::Sql(format!("no such table: main.{tname}")));
            };
            if tbl.system {
                return Err(SqliteError::Sql(format!(
                    "table {} may not be indexed",
                    tbl.name
                )));
            }
            // Validate columns, expressions, NULLS FIRST/LAST, subqueries, parameters, and
            // non-deterministic functions upfront BEFORE allocating pages or mutating sqlite_schema.
            let build_opts = BuildOptions {
                schema: false,
                file_format: self.pager.schema_format().clamp(1, 4),
                ..BuildOptions::default()
            };
            let idx_def = build_index(stmt, &tbl.definition, &build_opts)
                .map_err(|e| SqliteError::Sql(e.to_string()))?;
            for col in &idx_def.columns {
                if let Some(ref expr) = col.expr {
                    validate_index_expr_deterministic(expr, "index expressions")?;
                }
            }
            if let Some(ref where_expr) = idx_def.where_clause {
                validate_index_expr_deterministic(where_expr, "partial index WHERE clauses")?;
            }
            table_name_canon = tbl.name.clone();
        }

        let stored_sql = strip_if_not_exists(raw_sql, stmt.if_not_exists);
        let idx_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
        self.insert_schema_row(
            "index",
            iname,
            &table_name_canon,
            idx_root,
            Some(&stored_sql),
        )?;
        self.pager.bump_schema_cookie()?;

        // Populate index with all existing rows of the table!
        let view = self.live_view()?;
        let table = view
            .schema()
            .table(&table_name_canon)
            .expect("table exists")
            .clone();
        let idx_info = view
            .schema()
            .index(iname)
            .expect("newly created index exists")
            .clone();
        let existing_rows = self.read_all_table_rows(&view, &table)?;
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let colls = CollationRegistry::new(enc);
        let key_info = key_info_for_index(&idx_info, enc, Some(&colls))?;
        let qopts = self.query_options(self.dialect);

        for (rowid, full_row) in existing_rows {
            if let Some(entry_vals) =
                build_index_entry_values(&table, &idx_info, rowid, &full_row, &qopts)?
            {
                if idx_info.unique && !prefix_has_null(&entry_vals[..idx_info.key_count])
                    && index_contains_prefix(
                        &self.pager,
                        idx_info.root_page,
                        &entry_vals[..idx_info.key_count],
                        &key_info,
                    )?
                    .is_some()
                {
                    return Err(SqliteError::Sql(format!(
                        "UNIQUE constraint failed: index {iname}"
                    )));
                }
                let payload = encode_record_with_format(&entry_vals, enc, fmt);
                index_insert(&mut self.pager, idx_info.root_page, &payload, &key_info, false)?;
            }
        }
        Ok(self.empty_exec_result())
    }

    fn exec_create_view(&mut self, stmt: &CreateViewStmt, raw_sql: &str) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        if stmt.temp {
            return Err(SqliteError::Sql(
                "TEMP views are not supported in single-database mode".to_string(),
            ));
        }
        self.pager.ensure_initialized()?;
        let vname = &stmt.name.name;
        if vname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {vname}"
            )));
        }
        {
            let view = self.live_view()?;
            if view.schema().index(vname).is_some() {
                return Err(SqliteError::Sql(format!(
                    "there is already an index named {vname}"
                )));
            }
            if view.schema().object(vname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("view {vname} already exists")));
            }
        }
        let stored_sql = strip_if_not_exists(raw_sql, stmt.if_not_exists);
        self.insert_schema_row("view", vname, vname, 0, Some(&stored_sql))?;
        self.pager.bump_schema_cookie()?;
        Ok(self.empty_exec_result())
    }

    fn exec_drop(&mut self, stmt: &DropStmt) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        let name = &stmt.name.name;
        let view = self.live_view()?;

        match stmt.object_type {
            DropObjectType::Table => {
                let tbl = match view.schema().object(name) {
                    Some(SchemaObject::Table(tbl)) => tbl,
                    Some(SchemaObject::View(_)) => {
                        return Err(SqliteError::Sql(format!(
                            "use DROP VIEW to delete view {name}"
                        )));
                    }
                    _ => {
                        if stmt.if_exists {
                            return Ok(self.empty_exec_result());
                        }
                        return Err(SqliteError::Sql(format!("no such table: {name}")));
                    }
                };
                if tbl.system {
                    return Err(SqliteError::Sql(format!(
                        "table {} may not be dropped",
                        tbl.name
                    )));
                }
                let tbl_name = tbl.name.clone();
                let tbl_root = tbl.root_page;
                let idx_roots: Vec<u32> = tbl
                    .indexes
                    .iter()
                    .filter(|ix| !ix.without_rowid_primary_key)
                    .map(|ix| ix.root_page)
                    .collect();
                let schema_rowids: Vec<i64> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.table_name
                            .as_deref()
                            .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl_name))
                    })
                    .map(|e| e.rowid)
                    .collect();
                let seq_del = self.get_sqlite_sequence(&tbl_name)?;
                let seq_root = view.schema().table("sqlite_sequence").map(|s| s.root_page);
                drop(view);

                for r in idx_roots {
                    drop_btree(&mut self.pager, r)?;
                }
                drop_btree(&mut self.pager, tbl_root)?;
                for srid in schema_rowids {
                    table_delete(&mut self.pager, 1, srid)?;
                }
                if let (Some(sr), Some((rid, _))) = (seq_root, seq_del) {
                    table_delete(&mut self.pager, sr, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::Index => {
                let Some(ix) = view.schema().index(name) else {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such index: {name}")));
                };
                if ix.auto {
                    return Err(SqliteError::Sql(
                        "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped"
                            .to_string(),
                    ));
                }
                let ix_root = ix.root_page;
                let ix_name = ix.name.clone();
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&ix_name))
                    })
                    .map(|e| e.rowid);
                drop(view);

                drop_btree(&mut self.pager, ix_root)?;
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::View => {
                let v = match view.schema().object(name) {
                    Some(SchemaObject::View(v)) => v,
                    Some(SchemaObject::Table(_)) | Some(SchemaObject::VirtualTable(_)) => {
                        return Err(SqliteError::Sql(format!(
                            "use DROP TABLE to delete table {name}"
                        )));
                    }
                    _ => {
                        if stmt.if_exists {
                            return Ok(self.empty_exec_result());
                        }
                        return Err(SqliteError::Sql(format!("no such view: {name}")));
                    }
                };
                let vname = v.name.clone();
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("view")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&vname))
                    })
                    .map(|e| e.rowid);
                drop(view);
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::Trigger => {
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("trigger")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .map(|e| e.rowid);
                if srid.is_none() {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such trigger: {name}")));
                }
                drop(view);
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
        }
    }

    fn exec_alter_table(
        &mut self,
        stmt: &AlterTableStmt,
        full_sql: &str,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        let view = self.live_view()?;
        let Some(tbl) = view.schema().table(&stmt.table.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name
            )));
        };
        if tbl.system {
            return Err(SqliteError::Sql(format!(
                "table {} may not be altered",
                tbl.name
            )));
        }
        let table_entry = view
            .schema()
            .entries
            .iter()
            .find(|e| {
                e.obj_type.as_deref() == Some("table")
                    && e.name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
            })
            .cloned()
            .ok_or_else(|| SqliteError::corrupt("missing sqlite_schema entry for table"))?;
        let old_sql = table_entry
            .sql
            .as_deref()
            .ok_or_else(|| SqliteError::corrupt("missing CREATE TABLE sql"))?
            .to_string();

        match &stmt.action {
            AlterTableAction::RenameTable { new_name, .. } => {
                if new_name.name.to_ascii_lowercase().starts_with("sqlite_") {
                    return Err(SqliteError::Sql(format!(
                        "object name reserved for internal use: {}",
                        new_name.name
                    )));
                }
                if view.schema().object(&new_name.name).is_some()
                    || view.schema().index(&new_name.name).is_some()
                {
                    return Err(SqliteError::Sql(format!(
                        "there is already another table or index with this name: {}",
                        new_name.name
                    )));
                }
                let updated_sql = rewrite_create_table_name(&old_sql, &new_name.name)?;
                let dep_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.rowid != table_entry.rowid
                            && e.table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
                let seq_root = view.schema().table("sqlite_sequence").map(|s| s.root_page);
                let seq_entry = self.get_sqlite_sequence(&tbl.name)?;
                drop(view);

                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &new_name.name,
                    &new_name.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                let old_auto_prefix = format!("sqlite_autoindex_{}_", tbl.name);
                for dep in dep_entries {
                    let dep_name_orig = dep.name.as_deref().unwrap_or("");
                    let dep_name_new = if dep.sql.is_none()
                        && dep_name_orig
                            .as_bytes()
                            .get(..old_auto_prefix.len())
                            .is_some_and(|p| p.eq_ignore_ascii_case(old_auto_prefix.as_bytes()))
                    {
                        format!(
                            "sqlite_autoindex_{}_{}",
                            new_name.name,
                            &dep_name_orig[old_auto_prefix.len()..]
                        )
                    } else {
                        dep_name_orig.to_string()
                    };
                    let dep_sql = dep
                        .sql
                        .as_deref()
                        .map(|s| rewrite_on_table_name(s, &tbl.name, &new_name.name));
                    self.update_schema_row(
                        dep.rowid,
                        dep.obj_type.as_deref().unwrap_or("index"),
                        &dep_name_new,
                        &new_name.name,
                        dep.root_page,
                        dep_sql.as_deref(),
                    )?;
                }
                if let (Some(sr), Some((seq_rid, seq_val))) = (seq_root, seq_entry) {
                    let vals = vec![SqlValue::text(&new_name.name), SqlValue::Int(seq_val)];
                    let payload = encode_record_with_format(
                        &vals,
                        self.pager.encoding(),
                        self.pager.schema_format(),
                    );
                    table_insert(&mut self.pager, sr, seq_rid, &payload, true)?;
                }
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::AddColumn { column, .. } => {
                if tbl
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&column.name.name))
                {
                    return Err(SqliteError::Sql(format!(
                        "duplicate column name: {}",
                        column.name.name
                    )));
                }
                let mut has_not_null = false;
                let mut has_non_null_default = false;
                let mut is_generated = false;
                for c in &column.constraints {
                    match c {
                        ColumnConstraint::PrimaryKey { .. } => {
                            return Err(SqliteError::Sql(
                                "Cannot add a PRIMARY KEY column".to_string(),
                            ));
                        }
                        ColumnConstraint::Unique { .. } => {
                            return Err(SqliteError::Sql(
                                "Cannot add a UNIQUE column".to_string(),
                            ));
                        }
                        ColumnConstraint::Generated { storage, .. } => {
                            if *storage == Some(GeneratedStorage::Stored) {
                                return Err(SqliteError::Sql(
                                    "cannot add a STORED column".to_string(),
                                ));
                            }
                            is_generated = true;
                        }
                        ColumnConstraint::NotNull { .. } => {
                            has_not_null = true;
                        }
                        ColumnConstraint::Default { form, expr, .. } => {
                            if *form == DefaultForm::Paren
                                || matches!(expr, Expr::CurrentTime { .. })
                            {
                                return Err(SqliteError::Sql(
                                    "Cannot add a column with non-constant default".to_string(),
                                ));
                            }
                            if !matches!(expr, Expr::Null { .. }) {
                                has_non_null_default = true;
                            }
                        }
                        _ => {}
                    }
                }
                if has_not_null && !is_generated && !has_non_null_default {
                    return Err(SqliteError::Sql(
                        "Cannot add a NOT NULL column with default value NULL".to_string(),
                    ));
                }
                let col_def_sql = full_sql[column.span.start..column.span.end].trim();
                let updated_sql = append_column_to_create_table(&old_sql, col_def_sql)?;
                // Verify the new CREATE TABLE SQL parses cleanly via `build_table`!
                let p_opts = ParseOptions::default();
                let parsed = xdb_parser::parse_statement(&updated_sql, &p_opts)
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::CreateTable(ct) = parsed {
                    let b_opts = BuildOptions {
                        schema: false,
                        file_format: self.pager.schema_format().clamp(1, 4),
                        ..BuildOptions::default()
                    };
                    let def = build_table(&ct, &updated_sql, &b_opts)
                        .map_err(|e| SqliteError::Sql(e.to_string()))?;
                    let _ = generated_eval_order_from_defs(&def.name, &def.columns)?;
                }
                drop(view);
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::RenameColumn {
                column, new_name, ..
            } => {
                let Some(cid) = tbl
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column.name))
                else {
                    return Err(SqliteError::Sql(format!(
                        "no such column: \"{}\"",
                        column.name
                    )));
                };
                if tbl
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&new_name.name))
                {
                    return Err(SqliteError::Sql(format!(
                        "duplicate column name: {}",
                        new_name.name
                    )));
                }
                let old_col_name = tbl.columns[cid].name.clone();
                let updated_sql = rewrite_column_name_in_ddl(
                    &old_sql,
                    &tbl.name,
                    cid,
                    &old_col_name,
                    &new_name.name,
                )?;
                let dep_indexes: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.sql.is_some()
                            && e.table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
                drop(view);
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                for dep in dep_indexes {
                    if let Some(ref dep_sql) = dep.sql {
                        let new_dep_sql = rewrite_column_name_in_index_ddl(
                            dep_sql,
                            &tbl.name,
                            &old_col_name,
                            &new_name.name,
                        )?;
                        if new_dep_sql != *dep_sql {
                            self.update_schema_row(
                                dep.rowid,
                                "index",
                                dep.name.as_deref().unwrap_or(""),
                                &tbl.name,
                                dep.root_page,
                                Some(&new_dep_sql),
                            )?;
                        }
                    }
                }
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::DropColumn { column, .. } => {
                let Some(cid) = tbl
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column.name))
                else {
                    return Err(SqliteError::Sql(format!(
                        "no such column: \"{}\"",
                        column.name
                    )));
                };
                if tbl.columns.len() <= 1 {
                    return Err(SqliteError::Sql(
                        "cannot drop the only column of a table".to_string(),
                    ));
                }
                if tbl.columns[cid].pk > 0 {
                    return Err(SqliteError::Sql(format!(
                        "cannot drop PRIMARY KEY column \"{}\"",
                        column.name
                    )));
                }
                for ix in &tbl.indexes {
                    if ix
                        .columns
                        .iter()
                        .take(ix.key_count)
                        .any(|ic| ic.cid == cid as i32)
                    {
                        return Err(SqliteError::Sql(format!(
                            "error in index {} after drop column: no such column: {}",
                            ix.name, column.name
                        )));
                    }
                }
                let old_col_name = tbl.columns[cid].name.clone();
                check_column_unreferenced_in_ddl(&old_sql, &tbl.name, cid, &old_col_name)?;
                for dep in &view.schema().entries {
                    if dep.obj_type.as_deref() == Some("index")
                        && dep
                            .table_name
                            .as_deref()
                            .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    {
                        if let Some(ref dep_sql) = dep.sql {
                            check_column_unreferenced_in_index_ddl(
                                dep_sql,
                                dep.name.as_deref().unwrap_or(""),
                                &tbl.name,
                                &old_col_name,
                            )?;
                        }
                    }
                }
                for v in &view.schema().views {
                    check_column_unreferenced_in_view_ddl(&v.sql, &v.name, &tbl.name, &old_col_name)?;
                }
                for tr in &view.schema().triggers {
                    check_column_unreferenced_in_trigger_ddl(
                        &tr.sql,
                        &tr.name,
                        &tbl.name,
                        &old_col_name,
                    )?;
                }
                let existing = self.read_all_table_rows(&view, &tbl)?;
                let updated_sql = remove_column_from_ddl(&old_sql, cid)?;
                drop(view);

                // Update schema first, then rewrite stored table rows without the dropped column!
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                self.pager.bump_schema_cookie()?;

                let new_view = self.live_view()?;
                if tbl.columns[cid].hidden != ColumnHiddenKind::Virtual {
                    let new_tbl = new_view
                        .schema()
                        .table(&tbl.name)
                        .expect("table exists")
                        .clone();
                    let enc = self.pager.encoding();
                    let fmt = self.pager.schema_format();
                    if !tbl.without_rowid {
                        for (rid, mut row) in existing {
                            row.remove(cid);
                            let rec = encode_table_record(&new_tbl, &row, enc, fmt);
                            table_insert(
                                &mut self.pager,
                                new_tbl.root_page,
                                rid.unwrap(),
                                &rec,
                                true,
                            )?;
                        }
                    } else if let Some(pk_idx) =
                        new_tbl.indexes.iter().find(|ix| ix.without_rowid_primary_key)
                    {
                        let colls = CollationRegistry::new(enc);
                        let ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
                        for (_, mut row) in existing {
                            row.remove(cid);
                            let rec_vals = build_without_rowid_record_values(&new_tbl, &row);
                            let payload = encode_record_with_format(&rec_vals, enc, fmt);
                            index_insert(&mut self.pager, new_tbl.root_page, &payload, &ki, true)?;
                        }
                    }
                }
                Ok(self.empty_exec_result())
            }
            other => Err(SqliteError::Sql(format!(
                "unsupported ALTER TABLE action: {other:?}"
            ))),
        }
    }

    // ───────────────────────────── DML: INSERT / UPSERT ─────────────────────────────

    fn exec_insert(
        &mut self,
        stmt: &InsertStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }

        let stmt_conflict = if stmt.replace_into {
            Some(ConflictAction::Replace)
        } else {
            stmt.or_conflict
        };
        let target_alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .clone();

        // Determine target column indices (may include synthetic `rowid` / `_rowid_` / `oid`).
        #[derive(Clone, Copy)]
        enum TargetCol {
            Col(usize),
            Rowid,
        }
        let target_cols: Vec<TargetCol> = if let Some(ref cols) = stmt.columns {
            let mut v = Vec::with_capacity(cols.len());
            for c in cols {
                if let Some(pos) = table
                    .columns
                    .iter()
                    .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                {
                    if table.columns[pos].generated.is_some() {
                        return Err(SqliteError::Sql(format!(
                            "cannot INSERT into generated column \"{}\"",
                            c.name
                        )));
                    }
                    v.push(TargetCol::Col(pos));
                } else if !table.without_rowid
                    && matches!(
                        c.name.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    )
                {
                    v.push(TargetCol::Rowid);
                } else {
                    return Err(SqliteError::Sql(format!(
                        "table {} has no column named {}",
                        table.name, c.name
                    )));
                }
            }
            v
        } else {
            // Default column list: all non-generated columns!
            table
                .columns
                .iter()
                .filter(|c| c.generated.is_none())
                .map(|c| TargetCol::Col(c.cid))
                .collect()
        };

        // Evaluate source rows (VALUES / SELECT / DEFAULT VALUES) before mutating!
        let source_rows: Vec<Vec<SqlValue>> = if let Some(ref sel) = stmt.select {
            let mut sel_with = (**sel).clone();
            if stmt.with.is_some() && sel_with.with.is_none() {
                sel_with.with = stmt.with.clone();
            }
            let opts = self.query_options(dialect);
            let qres = self.eval_select_stmt(&view, &sel_with, params, &opts)?;
            if qres.columns.len() != target_cols.len() {
                return Err(SqliteError::Sql(format!(
                    "table {} has {} columns but {} values were supplied",
                    table.name,
                    target_cols.len(),
                    qres.columns.len()
                )));
            }
            qres.rows
        } else {
            // `INSERT INTO t DEFAULT VALUES`
            vec![Vec::new()]
        };
        drop(view);

        let qopts = self.query_options(dialect);
        let mut inserted_count = 0i64;
        let mut returning_rows: Vec<Vec<SqlValue>> = Vec::new();
        let returning_cols = stmt
            .returning
            .as_deref()
            .map(|rc| returning_column_names(&table, rc))
            .unwrap_or_default();

        for src in source_rows {
            // Build candidate row with defaults
            let mut row = Vec::with_capacity(table.columns.len());
            for (cid, col) in table.columns.iter().enumerate() {
                if col.generated.is_some()
                    || (Some(cid) == table.ipk_column && !table.without_rowid)
                {
                    // Generated columns are computed later; SQLite ignores DEFAULT on an omitted
                    // INTEGER PRIMARY KEY and assigns a fresh rowid instead.
                    row.push(SqlValue::Null);
                } else if let Some(ref dflt_expr) = table.definition.columns[cid].default_expr {
                    let v = eval_standalone_expr(dflt_expr, params, &qopts)?;
                    row.push(v);
                } else {
                    row.push(col.default_value.clone());
                }
            }

            let mut ipk_candidate: Option<SqlValue> = None;
            let mut assigned_cols = vec![false; table.columns.len()];
            let mut assigned_rowid = false;
            for (tcol, val) in target_cols.iter().zip(src.into_iter()) {
                match *tcol {
                    TargetCol::Col(cid) => {
                        // Match SQLite: when the same target column is listed twice, the first
                        // value wins; when both IPK column and `rowid` are listed, the later
                        // column in the target list overrides the rowid slot.
                        if !assigned_cols[cid] {
                            if Some(cid) == table.ipk_column && !table.without_rowid {
                                ipk_candidate = Some(val.clone());
                            }
                            row[cid] = val;
                            assigned_cols[cid] = true;
                        }
                    }
                    TargetCol::Rowid => {
                        if !assigned_rowid {
                            ipk_candidate = Some(val);
                            assigned_rowid = true;
                        }
                    }
                }
            }

            // Determine rowid for rowid tables
            let mut rowid: Option<i64> = if table.without_rowid {
                None
            } else if let Some(ipk) = table.ipk_column {
                let candidate = ipk_candidate.as_ref().unwrap_or(&row[ipk]);
                if candidate.is_null() {
                    let next = self.allocate_next_rowid(&table)?;
                    row[ipk] = SqlValue::Int(next);
                    Some(next)
                } else {
                    let rid = coerce_ipk_value(candidate)?;
                    row[ipk] = SqlValue::Int(rid);
                    Some(rid)
                }
            } else if let Some(ref er) = ipk_candidate {
                if er.is_null() {
                    Some(self.allocate_next_rowid(&table)?)
                } else {
                    Some(coerce_ipk_value(er)?)
                }
            } else {
                Some(self.allocate_next_rowid(&table)?)
            };

            // Apply affinities, STRICT checks, evaluate generated columns, NOT NULL, CHECK!
            match self.finalize_and_validate_row(
                &table,
                rowid,
                &mut row,
                stmt_conflict,
                params,
                &qopts,
            ) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = inserted_count;
                        self.total_changes += inserted_count;
                    }
                    return Err(e);
                }
            }

            // Check PRIMARY KEY / UNIQUE constraints & handle ON CONFLICT / UPSERT!
            match self.insert_row_with_constraints(
                &table,
                &target_alias,
                &mut rowid,
                &mut row,
                stmt_conflict,
                &stmt.upsert,
                stmt.with.as_ref(),
                params,
                &qopts,
                None,
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    was_insert,
                }) => {
                    inserted_count += 1;
                    if was_insert {
                        if let Some(rid) = final_rowid {
                            self.last_insert_rowid = rid;
                            if table.definition.autoincrement {
                                self.update_sqlite_sequence(&table.name, rid)?;
                            }
                        }
                    }
                    if let Some(ref ret) = stmt.returning {
                        returning_rows.push(eval_returning(
                            &table,
                            final_rowid,
                            &final_row,
                            ret,
                            params,
                            &qopts,
                        )?);
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {}
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = inserted_count;
                        self.total_changes += inserted_count;
                    }
                    return Err(e);
                }
            }
        }

        self.changes = inserted_count;
        self.total_changes += inserted_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: inserted_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    // ───────────────────────────── DML: UPDATE ─────────────────────────────

    fn exec_update(
        &mut self,
        stmt: &UpdateStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }
        let stmt_conflict = stmt.or_conflict;
        let qopts = self.query_options(dialect);
        let target_alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .clone();

        // Read all candidate rows matching WHERE (and optional UPDATE ... FROM).
        let existing_rows = self.read_all_table_rows(&view, &table)?;
        let from_rows_extra = if let Some(ref from_src) = stmt.from {
            // Execute `SELECT * FROM <from>` with full column metadata (preserving multi-table
            // join table qualifiers, affinities, and collations).
            let from_select = SelectStmt {
                span: from_src.span(),
                with: stmt.with.clone(),
                body: SelectBody::Core(Box::new(SelectCore {
                    span: from_src.span(),
                    distinct: None,
                    columns: vec![ResultColumn::Star(xdb_parser::ResultStar {
                        span: from_src.span(),
                        table: None,
                        exclude: None,
                        replace: None,
                    })],
                    from: Some(from_src.clone()),
                    where_clause: None,
                    group_by: None,
                    group_by_all: false,
                    having: None,
                    windows: None,
                    from_first: false,
                })),
                order_by: None,
                order_by_all: None,
                limit: None,
            };
            Some(self.eval_select_stmt_detailed(&view, &from_select, params, &qopts)?)
        } else {
            None
        };

        // Snapshot all rows to update first (SQLite Halloween problem protection!).
        let base_scope_cols = build_table_scope_columns(&table, &target_alias);
        type PendingUpdate = (Option<i64>, Vec<SqlValue>, Option<i64>, Vec<SqlValue>);
        let mut updates: Vec<PendingUpdate> = Vec::new();
        let catalog = crate::sql::live_catalog_for(&view, &qopts);
        let sub_runner =
            CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
        let eval_env = EvalEnv::new(&qopts, params, &sub_runner);

        for (old_rowid, old_row) in existing_rows {
            let mut full_vals = old_row.clone();
            if !table.without_rowid {
                let rid_v = old_rowid.map_or(SqlValue::Null, SqlValue::Int);
                for _ in base_scope_cols.len() - table.columns.len().. {
                    if full_vals.len() >= base_scope_cols.len() {
                        break;
                    }
                    full_vals.push(rid_v.clone());
                }
            }

            if let Some((ref extra_cols, ref extra_rows)) = from_rows_extra {
                let mut combined_cols = base_scope_cols.clone();
                combined_cols.extend(extra_cols.iter().cloned());
                for erow in extra_rows {
                    let mut combined_vals = full_vals.clone();
                    combined_vals.extend_from_slice(erow);
                    let scope = [EvalScope::simple(&combined_cols, &combined_vals)];
                    if let Some(ref we) = stmt.where_clause {
                        let cond = eval_expr(we, &scope, &eval_env)?;
                        if !boolean_value(&cond, false) {
                            continue;
                        }
                    }
                    let (new_rid, new_row) = apply_set_clauses(
                        &table,
                        old_rowid,
                        &old_row,
                        &stmt.set,
                        &scope,
                        &eval_env,
                    )?;
                    updates.push((old_rowid, old_row.clone(), new_rid, new_row));
                    break; // SQLite UPDATE FROM updates each target row at most once
                }
            } else {
                let scope = [EvalScope::simple(&base_scope_cols, &full_vals)];
                if let Some(ref we) = stmt.where_clause {
                    let cond = eval_expr(we, &scope, &eval_env)?;
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }
                let (new_rid, new_row) =
                    apply_set_clauses(&table, old_rowid, &old_row, &stmt.set, &scope, &eval_env)?;
                updates.push((old_rowid, old_row, new_rid, new_row));
            }
        }
        drop(view);

        let enc = self.pager.encoding();
        let colls = CollationRegistry::new(enc);
        let wr_pk_info: Option<(&IndexInfo, KeyInfo)> = if table.without_rowid {
            if let Some(pk_idx) = table.indexes.iter().find(|ix| ix.without_rowid_primary_key) {
                Some((pk_idx, key_info_for_without_rowid_pk(pk_idx, enc, &colls)?))
            } else {
                None
            }
        } else {
            None
        };

        let mut updated_count = 0i64;
        let mut returning_rows = Vec::new();
        let returning_cols = stmt
            .returning
            .as_deref()
            .map(|rc| returning_column_names(&table, rc))
            .unwrap_or_default();
        let mut replaced = ReplacedTracker::default();

        for (old_rowid, old_row, mut new_rowid, mut new_row) in updates {
            // Skip pending updates whose original target row was already deleted by an earlier
            // `OR REPLACE` conflict resolution in this same UPDATE statement.
            if !table.without_rowid {
                if old_rowid.is_some_and(|r| replaced.rowids.contains(&r)) {
                    continue;
                }
            } else if let Some((pk_idx, ref pk_ki)) = wr_pk_info {
                let old_pk: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                    .iter()
                    .map(|ic| old_row[ic.cid as usize].clone())
                    .collect();
                if replaced.contains_wr_pk(&old_pk, pk_ki) {
                    continue;
                }
            }

            match self.finalize_and_validate_row(
                &table,
                new_rowid,
                &mut new_row,
                stmt_conflict,
                params,
                &qopts,
            ) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(e);
                }
            }

            // Always wrap each row's delete+re-insert in a mini savepoint so `OR IGNORE`,
            // `OR FAIL`, and schema-level `ON CONFLICT IGNORE/FAIL` leave the current row intact!
            self.pager.push_savepoint(None);

            if let Err(e) = self.delete_row_and_indexes(&table, old_rowid, &old_row, &qopts) {
                self.pager.rollback_statement_savepoint();
                return Err(e);
            }

            let mut row_replaced = ReplacedTracker::default();
            match self.insert_row_with_constraints(
                &table,
                &target_alias,
                &mut new_rowid,
                &mut new_row,
                stmt_conflict,
                &[],
                stmt.with.as_ref(),
                params,
                &qopts,
                Some(&mut row_replaced),
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    ..
                }) => {
                    self.pager.commit_statement_savepoint();
                    replaced.extend(row_replaced);
                    updated_count += 1;
                    if let Some(ref ret) = stmt.returning {
                        returning_rows.push(eval_returning(
                            &table,
                            final_rowid,
                            &final_row,
                            ret,
                            params,
                            &qopts,
                        )?);
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {
                    self.pager.rollback_statement_savepoint();
                }
                Err(e) => {
                    self.pager.rollback_statement_savepoint();
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(e);
                }
            }
        }

        self.changes = updated_count;
        self.total_changes += updated_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: updated_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    // ───────────────────────────── DML: DELETE ─────────────────────────────

    fn exec_delete(
        &mut self,
        stmt: &DeleteStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }

        let qopts = self.query_options(dialect);
        let alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .clone();
        let existing_rows = self.read_all_table_rows(&view, &table)?;
        let scope_cols = build_table_scope_columns(&table, &alias);
        let catalog = crate::sql::live_catalog_for(&view, &qopts);
        let sub_runner =
            CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
        let eval_env = EvalEnv::new(&qopts, params, &sub_runner);

        let mut to_delete = Vec::new();
        for (rid, row) in existing_rows {
            if let Some(ref we) = stmt.where_clause {
                let mut vals = row.clone();
                if !table.without_rowid {
                    let rid_v = rid.map_or(SqlValue::Null, SqlValue::Int);
                    while vals.len() < scope_cols.len() {
                        vals.push(rid_v.clone());
                    }
                }
                let scope = [EvalScope::simple(&scope_cols, &vals)];
                let cond = eval_expr(we, &scope, &eval_env)?;
                if !boolean_value(&cond, false) {
                    continue;
                }
            }
            to_delete.push((rid, row));
        }
        drop(view);

        let mut deleted_count = 0i64;
        let mut returning_rows = Vec::new();
        let returning_cols = stmt
            .returning
            .as_deref()
            .map(|rc| returning_column_names(&table, rc))
            .unwrap_or_default();

        for (rid, row) in to_delete {
            self.delete_row_and_indexes(&table, rid, &row, &qopts)?;
            deleted_count += 1;
            if let Some(ref ret) = stmt.returning {
                returning_rows.push(eval_returning(&table, rid, &row, ret, params, &qopts)?);
            }
        }

        self.changes = deleted_count;
        self.total_changes += deleted_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: deleted_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    // ───────────────────────────── Row & Index write primitives ─────────────────────────────

    fn allocate_next_rowid(&self, table: &TableInfo) -> SqliteResult<i64> {
        let mut max_id = table_max_rowid(&self.pager, table.root_page)?.unwrap_or(0);
        if table.definition.autoincrement {
            max_id = max_id.max(0);
            if let Some((_, seq)) = self.get_sqlite_sequence(&table.name)? {
                if seq > max_id {
                    max_id = seq;
                }
            }
            return max_id
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()));
        }
        if let Some(next) = max_id.checked_add(1) {
            return Ok(next);
        }
        // Non-AUTOINCREMENT table has a row with rowid == i64::MAX: probe up to 100
        // pseudorandom positive rowids (matching SQLite's OP_NewRowid algorithm).
        let mut seed: u64 = (u64::from(table.root_page) << 32)
            ^ u64::from(self.pager.page_count())
            ^ (self.total_changes as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ 0x243f_6a88_85a3_08d3;
        for _ in 0..100 {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            let cand = (z as i64) & i64::MAX;
            if cand == 0 {
                continue;
            }
            if !table_contains_rowid(&self.pager, table.root_page, cand)? {
                return Ok(cand);
            }
        }
        Err(SqliteError::Sql("database or disk is full".to_string()))
    }

    /// Returns `Ok(true)` if the row is valid, `Ok(false)` if a `NOT NULL` or `CHECK`
    /// constraint was violated under `ConflictAction::Ignore`, or `Err` on `STRICT`
    /// datatype errors or non-ignored constraint failures.
    fn finalize_and_validate_row(
        &mut self,
        table: &TableInfo,
        rowid: Option<i64>,
        row: &mut [SqlValue],
        stmt_conflict: Option<ConflictAction>,
        params: &QueryParams,
        qopts: &QueryOptions,
    ) -> SqliteResult<bool> {
        // 1. Apply column affinity & STRICT type validation on non-generated columns.
        // Note: SQLite ALWAYS rejects STRICT datatype mismatches even under `OR IGNORE`.
        for (cid, col) in table.columns.iter().enumerate() {
            if col.generated.is_some() {
                continue;
            }
            if table.strict {
                row[cid] =
                    validate_strict_value(row[cid].clone(), &col.decl_type, &table.name, &col.name)?;
            } else {
                row[cid] = apply_affinity(row[cid].clone(), col.affinity);
            }
        }

        // 2. Evaluate STORED & VIRTUAL generated columns in topological dependency order
        //    (detecting cycles and enforcing STRICT type validation on generated columns too).
        let gen_order = generated_eval_order_from_defs(&table.name, &table.definition.columns)?;
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
        for &cid in &gen_order {
            if let Some(ref expr) = table.definition.columns[cid].generated {
                row[cid] = eval_generated_column_value(table, cid, expr, &scope_cols, row, qopts)?;
            }
        }

        // 3. NOT NULL constraints (including WITHOUT ROWID PK columns & ON CONFLICT REPLACE DEFAULT!).
        let mut substituted_default = false;
        for (cid, col) in table.columns.iter().enumerate() {
            if (col.not_null || (table.without_rowid && col.pk > 0)) && row[cid].is_null() {
                let eff = resolve_conflict(stmt_conflict, table.definition.columns[cid].not_null_on_error);
                match eff {
                    ConflictAction::Ignore => return Ok(false),
                    ConflictAction::Replace => {
                        if col.generated.is_none() {
                            let dflt_val = if let Some(ref dflt_expr) =
                                table.definition.columns[cid].default_expr
                            {
                                eval_standalone_expr(dflt_expr, params, qopts)?
                            } else {
                                col.default_value.clone()
                            };
                            if !dflt_val.is_null() {
                                let coerced = if table.strict {
                                    validate_strict_value(
                                        dflt_val,
                                        &col.decl_type,
                                        &table.name,
                                        &col.name,
                                    )?
                                } else {
                                    apply_affinity(dflt_val, col.affinity)
                                };
                                if !coerced.is_null() {
                                    row[cid] = coerced;
                                    substituted_default = true;
                                    continue;
                                }
                            }
                        }
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                        ));
                    }
                    ConflictAction::Abort | ConflictAction::Fail | ConflictAction::Rollback => {
                        return Err(self.constraint_err(
                            eff,
                            format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                        ));
                    }
                }
            }
        }
        // Re-evaluate generated columns (and re-check their NOT NULL constraints) if any
        // non-generated column received a default substitution via ON CONFLICT REPLACE.
        if substituted_default && !gen_order.is_empty() {
            for &cid in &gen_order {
                if let Some(ref expr) = table.definition.columns[cid].generated {
                    row[cid] =
                        eval_generated_column_value(table, cid, expr, &scope_cols, row, qopts)?;
                }
            }
            for &cid in &gen_order {
                let col = &table.columns[cid];
                if col.not_null && row[cid].is_null() {
                    let eff = resolve_conflict(
                        stmt_conflict,
                        table.definition.columns[cid].not_null_on_error,
                    );
                    if eff == ConflictAction::Ignore {
                        return Ok(false);
                    }
                    let err_eff = if eff == ConflictAction::Replace {
                        ConflictAction::Abort
                    } else {
                        eff
                    };
                    return Err(self.constraint_err(
                        err_eff,
                        format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                    ));
                }
            }
        }

        // 4. CHECK constraints (`NULL` passes CHECK in SQLite, only explicit false `0` fails!).
        //    Include synthetic `rowid`/`_rowid_`/`oid` aliases in CHECK scope for rowid tables.
        if !table.definition.checks.is_empty() {
            let check_cols = build_table_scope_columns(table, &table.name);
            let mut check_vals = row.to_vec();
            if !table.without_rowid {
                let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
                while check_vals.len() < check_cols.len() {
                    check_vals.push(rid_v.clone());
                }
            }
            let no_sub = NoSubqueries;
            let empty_params = QueryParams::empty();
            let mut env = EvalEnv::new(qopts, &empty_params, &no_sub);
            env.dqs = true;
            let scope = [EvalScope::simple(&check_cols, &check_vals)];
            let eff = stmt_conflict.unwrap_or(ConflictAction::Abort);
            for chk in &table.definition.checks {
                let res = eval_expr(&chk.expr, &scope, &env)?;
                if !res.is_null() && !boolean_value(&res, true) {
                    if eff == ConflictAction::Ignore {
                        return Ok(false);
                    }
                    let cname = if chk.name.is_empty() {
                        table.name.as_str()
                    } else {
                        chk.name.as_str()
                    };
                    let err_eff = if eff == ConflictAction::Replace {
                        ConflictAction::Abort
                    } else {
                        eff
                    };
                    return Err(self.constraint_err(
                        err_eff,
                        format!("CHECK constraint failed: {cname}"),
                    ));
                }
            }
        }

        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row_with_constraints(
        &mut self,
        table: &TableInfo,
        target_alias: &str,
        rowid: &mut Option<i64>,
        row: &mut [SqlValue],
        stmt_conflict: Option<ConflictAction>,
        upserts: &[xdb_parser::UpsertClause],
        with_clause: Option<&xdb_parser::WithClause>,
        params: &QueryParams,
        qopts: &QueryOptions,
        mut replaced: Option<&mut ReplacedTracker>,
    ) -> SqliteResult<RowWriteOutcome> {
        let enc = self.pager.encoding();
        let colls = CollationRegistry::new(enc);

        // Detect any conflict (rowid PK, WITHOUT ROWID PK, or UNIQUE secondary index),
        // retaining each constraint's schema-level `OnError` action.
        let mut conflicting_rows: Vec<(Option<i64>, Vec<SqlValue>, String, OnError)> = Vec::new();

        let wr_pk_idx = if table.without_rowid {
            table.indexes.iter().find(|ix| ix.without_rowid_primary_key)
        } else {
            None
        };

        if !table.without_rowid {
            let rid = rowid.expect("rowid table has rowid");
            if table_contains_rowid(&self.pager, table.root_page, rid)? {
                let old_row = self.fetch_rowid_row(table, rid, qopts)?;
                let col_label = table
                    .ipk_column
                    .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                    .unwrap_or_else(|| format!("{}.rowid", table.name));
                conflicting_rows.push((
                    Some(rid),
                    old_row,
                    col_label,
                    table.definition.ipk_on_error,
                ));
            }
        } else if let Some(pk_idx) = wr_pk_idx {
            // WITHOUT ROWID primary key index shares `table.root_page`!
            let ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
            let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| row[ic.cid as usize].clone())
                .collect();
            if let Some(hit_entry) =
                index_contains_prefix(&self.pager, table.root_page, &pk_vals, &ki)?
            {
                let old_row = decode_without_rowid_full_row(table, &hit_entry, qopts)?;
                conflicting_rows.push((
                    None,
                    old_row,
                    format!("{}.PRIMARY KEY", table.name),
                    pk_idx.definition.on_error,
                ));
            }
        }

        // Check UNIQUE secondary indexes
        for ix in &table.indexes {
            if !ix.unique || ix.without_rowid_primary_key {
                continue;
            }
            let Some(entry_vals) = build_index_entry_values(table, ix, *rowid, row, qopts)? else {
                continue;
            };
            let key_prefix = &entry_vals[..ix.key_count];
            if prefix_has_null(key_prefix) {
                continue; // NULLs are always distinct in SQLite UNIQUE indexes
            }
            let ki = key_info_for_index(ix, enc, Some(&colls))?;
            if let Some(hit_entry) =
                index_contains_prefix(&self.pager, ix.root_page, key_prefix, &ki)?
            {
                if !table.without_rowid {
                    let hit_rid = hit_entry.last().and_then(SqlValue::as_i64);
                    if let Some(hrid) = hit_rid {
                        let old_row = self.fetch_rowid_row(table, hrid, qopts)?;
                        conflicting_rows.push((
                            Some(hrid),
                            old_row,
                            format!("{}.{}", table.name, ix.name),
                            ix.definition.on_error,
                        ));
                    } else {
                        conflicting_rows.push((
                            None,
                            Vec::new(),
                            format!("{}.{}", table.name, ix.name),
                            ix.definition.on_error,
                        ));
                    }
                } else if let Some(pk_idx) = wr_pk_idx {
                    let mut pk_vals = Vec::with_capacity(pk_idx.key_count);
                    let mut complete = true;
                    for pk_col in &pk_idx.columns[..pk_idx.key_count] {
                        if let Some(pos) = ix.columns.iter().position(|ic| ic.cid == pk_col.cid) {
                            if let Some(v) = hit_entry.get(pos) {
                                pk_vals.push(v.clone());
                                continue;
                            }
                        }
                        complete = false;
                        break;
                    }
                    let old_row = if complete {
                        let pk_ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
                        if let Some(full_entry) =
                            index_contains_prefix(&self.pager, table.root_page, &pk_vals, &pk_ki)?
                        {
                            decode_without_rowid_full_row(table, &full_entry, qopts)?
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };
                    conflicting_rows.push((
                        None,
                        old_row,
                        format!("{}.{}", table.name, ix.name),
                        ix.definition.on_error,
                    ));
                } else {
                    conflicting_rows.push((
                        None,
                        Vec::new(),
                        format!("{}.{}", table.name, ix.name),
                        ix.definition.on_error,
                    ));
                }
            }
        }

        if !conflicting_rows.is_empty() {
            // Check UPSERT (`ON CONFLICT ...`) first!
            if !upserts.is_empty() {
                for up in upserts {
                    if let Some(cidx) = upsert_target_matches(table, up, &conflicting_rows) {
                        match up.action {
                            UpsertAction::Nothing => return Ok(RowWriteOutcome::Ignored),
                            UpsertAction::Update => {
                                let (old_rid, old_row, _, _) = conflicting_rows.remove(cidx);
                                let mut upd_row = old_row.clone();
                                let mut upd_rid = old_rid;
                                let scope_cols = build_upsert_scope_columns(table, target_alias);
                                let mut scope_vals = old_row.clone();
                                if !table.without_rowid {
                                    let rv = old_rid.map_or(SqlValue::Null, SqlValue::Int);
                                    for _ in 0..count_synth_rowid_aliases(table) {
                                        scope_vals.push(rv.clone());
                                    }
                                }
                                // Append `excluded.*` values
                                scope_vals.extend(row.iter().cloned());
                                let should_update = {
                                    let view = self.live_view()?;
                                    let catalog = crate::sql::live_catalog_for(&view, qopts);
                                    let sub_runner = CatalogSubqueryRunner::with_ctes(
                                        &catalog,
                                        qopts,
                                        params,
                                        with_clause,
                                    )?;
                                    let eval_env = EvalEnv::new(qopts, params, &sub_runner);
                                    let scope = [EvalScope::simple(&scope_cols, &scope_vals)];

                                    if let Some(ref we) = up.where_clause {
                                        let cond = eval_expr(we, &scope, &eval_env)?;
                                        if !boolean_value(&cond, false) {
                                            false
                                        } else {
                                            if let Some(ref sets) = up.set {
                                                let (nr, nrow) = apply_set_clauses(
                                                    table,
                                                    old_rid,
                                                    &old_row,
                                                    sets,
                                                    &scope,
                                                    &eval_env,
                                                )?;
                                                upd_rid = nr;
                                                upd_row = nrow;
                                            }
                                            true
                                        }
                                    } else {
                                        if let Some(ref sets) = up.set {
                                            let (nr, nrow) = apply_set_clauses(
                                                table,
                                                old_rid,
                                                &old_row,
                                                sets,
                                                &scope,
                                                &eval_env,
                                            )?;
                                            upd_rid = nr;
                                            upd_row = nrow;
                                        }
                                        true
                                    }
                                };
                                if !should_update {
                                    return Ok(RowWriteOutcome::Ignored);
                                }
                                if !self.finalize_and_validate_row(
                                    table,
                                    upd_rid,
                                    &mut upd_row,
                                    stmt_conflict,
                                    params,
                                    qopts,
                                )? {
                                    return Ok(RowWriteOutcome::Ignored);
                                }
                                self.pager.push_savepoint(None);
                                if let Err(e) =
                                    self.delete_row_and_indexes(table, old_rid, &old_row, qopts)
                                {
                                    self.pager.rollback_statement_savepoint();
                                    return Err(e);
                                }
                                // Re-check all remaining PK/UNIQUE constraints on the updated row!
                                match self.insert_row_with_constraints(
                                    table,
                                    target_alias,
                                    &mut upd_rid,
                                    &mut upd_row,
                                    stmt_conflict,
                                    &[],
                                    with_clause,
                                    params,
                                    qopts,
                                    None,
                                ) {
                                    Ok(RowWriteOutcome::Written {
                                        final_rowid,
                                        final_row,
                                        ..
                                    }) => {
                                        self.pager.commit_statement_savepoint();
                                        return Ok(RowWriteOutcome::Written {
                                            final_rowid,
                                            final_row,
                                            was_insert: false,
                                        });
                                    }
                                    Ok(RowWriteOutcome::Ignored) => {
                                        self.pager.rollback_statement_savepoint();
                                        return Ok(RowWriteOutcome::Ignored);
                                    }
                                    Err(e) => {
                                        self.pager.rollback_statement_savepoint();
                                        return Err(e);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Resolve non-UPSERT conflicts in order using statement-level or schema-level ON CONFLICT.
            for (_, _, label, on_err) in &conflicting_rows {
                let eff = resolve_conflict(stmt_conflict, *on_err);
                match eff {
                    ConflictAction::Ignore => return Ok(RowWriteOutcome::Ignored),
                    ConflictAction::Replace => {}
                    ConflictAction::Abort | ConflictAction::Fail | ConflictAction::Rollback => {
                        return Err(self.constraint_err(
                            eff,
                            format!("UNIQUE constraint failed: {label}"),
                        ));
                    }
                }
            }

            // All conflicts resolve to REPLACE: delete each distinct conflicting row once.
            let mut deleted_rids: HashSet<i64> = HashSet::new();
            let mut deleted_wr_pks: Vec<Vec<SqlValue>> = Vec::new();
            let wr_pk_ki = if let Some(pk_idx) = wr_pk_idx {
                Some(key_info_for_without_rowid_pk(pk_idx, enc, &colls)?)
            } else {
                None
            };
            for (crid, crow, _, _) in conflicting_rows {
                if crow.is_empty() {
                    continue;
                }
                if !table.without_rowid {
                    if let Some(rid) = crid {
                        if !deleted_rids.insert(rid) {
                            continue;
                        }
                        self.delete_row_and_indexes(table, Some(rid), &crow, qopts)?;
                        if let Some(ref mut tr) = replaced {
                            tr.rowids.insert(rid);
                        }
                    }
                } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &wr_pk_ki) {
                    let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                        .iter()
                        .map(|ic| crow[ic.cid as usize].clone())
                        .collect();
                    if deleted_wr_pks
                        .iter()
                        .any(|p| key_tuples_equal(p, &pk_vals, pk_ki))
                    {
                        continue;
                    }
                    self.delete_row_and_indexes(table, None, &crow, qopts)?;
                    deleted_wr_pks.push(pk_vals.clone());
                    if let Some(ref mut tr) = replaced {
                        tr.wr_pks.push(pk_vals);
                    }
                }
            }
        }

        self.write_row_and_indexes(table, *rowid, row, qopts)?;
        Ok(RowWriteOutcome::Written {
            final_rowid: *rowid,
            final_row: row.to_owned(),
            was_insert: true,
        })
    }

    fn write_row_and_indexes(
        &mut self,
        table: &TableInfo,
        rowid: Option<i64>,
        row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let colls = CollationRegistry::new(enc);

        if table.without_rowid {
            let pk_idx = table
                .indexes
                .iter()
                .find(|ix| ix.without_rowid_primary_key)
                .ok_or_else(|| SqliteError::corrupt("WITHOUT ROWID table missing PK index"))?;
            let ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
            let rec_vals = build_without_rowid_record_values(table, row);
            let payload = encode_record_with_format(&rec_vals, enc, fmt);
            index_insert(&mut self.pager, table.root_page, &payload, &ki, true)?;
        } else {
            let rid = rowid.expect("rowid table has rowid");
            let payload = encode_table_record(table, row, enc, fmt);
            table_insert(&mut self.pager, table.root_page, rid, &payload, true)?;
        }

        for ix in &table.indexes {
            if ix.without_rowid_primary_key {
                continue;
            }
            if let Some(entry_vals) = build_index_entry_values(table, ix, rowid, row, qopts)? {
                let ki = key_info_for_index(ix, enc, Some(&colls))?;
                let payload = encode_record_with_format(&entry_vals, enc, fmt);
                index_insert(&mut self.pager, ix.root_page, &payload, &ki, true)?;
            }
        }
        Ok(())
    }

    fn delete_row_and_indexes(
        &mut self,
        table: &TableInfo,
        rowid: Option<i64>,
        row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        let enc = self.pager.encoding();
        let colls = CollationRegistry::new(enc);

        for ix in &table.indexes {
            if ix.without_rowid_primary_key {
                continue;
            }
            if let Some(entry_vals) = build_index_entry_values(table, ix, rowid, row, qopts)? {
                let ki = key_info_for_index(ix, enc, Some(&colls))?;
                index_delete(&mut self.pager, ix.root_page, &entry_vals, &ki)?;
            }
        }

        if table.without_rowid {
            let pk_idx = table
                .indexes
                .iter()
                .find(|ix| ix.without_rowid_primary_key)
                .ok_or_else(|| SqliteError::corrupt("WITHOUT ROWID table missing PK index"))?;
            let ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
            let rec_vals = build_without_rowid_record_values(table, row);
            index_delete(&mut self.pager, table.root_page, &rec_vals, &ki)?;
        } else {
            let rid = rowid.expect("rowid table has rowid");
            table_delete(&mut self.pager, table.root_page, rid)?;
        }
        Ok(())
    }

    fn fetch_rowid_row(
        &self,
        table: &TableInfo,
        rowid: i64,
        qopts: &QueryOptions,
    ) -> SqliteResult<Vec<SqlValue>> {
        let view = self.live_view()?;
        let reader = view.table(&table.name)?;
        let stored_cids: Vec<usize> = table
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let stored = reader
            .get(rowid, Some(&scan_opts))?
            .ok_or_else(|| SqliteError::corrupt(format!("missing rowid {rowid}")))?;
        let mut full = vec![SqlValue::Null; table.columns.len()];
        for (&cid, val) in stored_cids.iter().zip(stored) {
            full[cid] = val;
        }
        if table.has_virtual_columns {
            eval_virtual_columns(table, &mut full, qopts)?;
        }
        Ok(full)
    }

    fn read_all_table_rows(
        &self,
        view: &SqliteDatabase,
        table: &TableInfo,
    ) -> SqliteResult<Vec<(Option<i64>, Vec<SqlValue>)>> {
        let reader = view.table(&table.name)?;
        let stored_cids: Vec<usize> = table
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let has_rowid = !table.without_rowid;
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: has_rowid,
        };
        let mut cur = reader.cursor(Some(&scan_opts))?;
        let qopts = self.query_options(self.dialect);
        let mut out = Vec::new();
        while cur.next()? {
            let mut raw = cur.row()?;
            let rid = if has_rowid {
                raw.remove(0).as_i64()
            } else {
                None
            };
            let mut full = vec![SqlValue::Null; table.columns.len()];
            for (&cid, val) in stored_cids.iter().zip(raw) {
                full[cid] = val;
            }
            if table.has_virtual_columns {
                eval_virtual_columns(table, &mut full, &qopts)?;
            }
            out.push((rid, full));
        }
        Ok(out)
    }

    fn eval_select_stmt(
        &self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<QueryResult> {
        let catalog = crate::sql::live_catalog_for(view, options);
        xdb_sql::execute_select_stmt(&catalog, stmt, params, options).map_err(SqliteError::from)
    }

    fn eval_select_stmt_detailed(
        &self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        let catalog = crate::sql::live_catalog_for(view, options);
        execute_select_stmt_detailed(&catalog, stmt, params, options).map_err(SqliteError::from)
    }

    // ───────────────────────────── PRAGMA ─────────────────────────────

    fn exec_pragma(&mut self, stmt: &PragmaStmt) -> SqliteResult<ExecResult> {
        let name = stmt.name.name.to_ascii_lowercase();
        let val_text = stmt.value.as_ref().map(|v| {
            let t = v.text.trim();
            xdb_parser::dequote(t)
        });

        let single_row = |col: &str, v: SqlValue| ExecResult {
            columns: vec![col.to_string()],
            rows: vec![vec![v]],
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        };

        match name.as_str() {
            "page_size" => {
                if let Some(v) = val_text {
                    if let Ok(ps) = v.parse::<u32>() {
                        self.pager.set_page_size(ps)?;
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row("page_size", SqlValue::Int(i64::from(self.pager.page_size()))))
            }
            "page_count" => Ok(single_row(
                "page_count",
                SqlValue::Int(i64::from(self.pager.page_count())),
            )),
            "freelist_count" => Ok(single_row(
                "freelist_count",
                SqlValue::Int(i64::from(self.pager.freelist_count())),
            )),
            "schema_version" => Ok(single_row(
                "schema_version",
                SqlValue::Int(i64::from(self.pager.schema_cookie())),
            )),
            "user_version" => {
                if let Some(v) = val_text {
                    if let Ok(uv) = v.parse::<i32>() {
                        return self.run_mutating_stmt(|conn| {
                            conn.pager.set_user_version(uv)?;
                            Ok(conn.empty_exec_result())
                        });
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "user_version",
                    SqlValue::Int(i64::from(self.pager.user_version())),
                ))
            }
            "application_id" => {
                if let Some(v) = val_text {
                    if let Ok(aid) = v.parse::<i32>() {
                        return self.run_mutating_stmt(|conn| {
                            conn.pager.set_application_id(aid)?;
                            Ok(conn.empty_exec_result())
                        });
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "application_id",
                    SqlValue::Int(i64::from(self.pager.application_id())),
                ))
            }
            "encoding" => {
                if let Some(v) = val_text {
                    let enc = match v.trim().to_ascii_lowercase().as_str() {
                        "utf-8" | "utf8" => Some(TextEncoding::Utf8),
                        "utf-16" | "utf16" | "utf-16le" | "utf16le" => Some(TextEncoding::Utf16Le),
                        "utf-16be" | "utf16be" => Some(TextEncoding::Utf16Be),
                        _ => None,
                    };
                    if let Some(e) = enc {
                        self.pager.set_encoding(e);
                    }
                    return Ok(self.empty_exec_result());
                }
                let s = match self.pager.encoding() {
                    TextEncoding::Utf8 => "UTF-8",
                    TextEncoding::Utf16Le => "UTF-16le",
                    TextEncoding::Utf16Be => "UTF-16be",
                };
                Ok(single_row("encoding", SqlValue::text(s)))
            }
            "synchronous" => {
                if let Some(v) = val_text {
                    match v.trim().to_ascii_lowercase().as_str() {
                        "0" | "off" => self.synchronous = 0,
                        "1" | "normal" => self.synchronous = 1,
                        "2" | "full" => self.synchronous = 2,
                        "3" | "extra" => self.synchronous = 3,
                        _ => {}
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row("synchronous", SqlValue::Int(self.synchronous)))
            }
            "journal_mode" => Ok(single_row("journal_mode", SqlValue::text("delete"))),
            "busy_timeout" => {
                if let Some(v) = val_text {
                    if let Ok(ms) = v.parse::<i64>() {
                        self.busy_timeout = ms.max(0);
                    }
                    return Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)));
                }
                Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)))
            }
            "foreign_keys" => {
                if let Some(v) = val_text {
                    self.foreign_keys =
                        matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes");
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "foreign_keys",
                    SqlValue::Int(i64::from(self.foreign_keys)),
                ))
            }
            "integrity_check" | "quick_check" => {
                let view = self.live_view()?;
                let mut roots = vec![1u32];
                for t in view.tables() {
                    if t.root_page > 1 {
                        roots.push(t.root_page);
                    }
                }
                for ix in view.indexes() {
                    if !ix.without_rowid_primary_key && ix.root_page > 1 {
                        roots.push(ix.root_page);
                    }
                }
                let msgs = check_integrity(&self.pager, &roots);
                Ok(ExecResult {
                    columns: vec![name],
                    rows: msgs.into_iter().map(|m| vec![SqlValue::text(m)]).collect(),
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            "table_info" | "table_xinfo" | "index_list" | "index_info" | "index_xinfo"
            | "database_list" => {
                let view = self.live_view()?;
                let sql = match val_text {
                    Some(arg) => {
                        let esc = arg.replace('\'', "''");
                        format!("SELECT * FROM pragma_{name}('{esc}')")
                    }
                    None => format!("SELECT * FROM pragma_{name}"),
                };
                let qres = view.query(&sql)?;
                Ok(ExecResult {
                    columns: qres.columns,
                    rows: qres.rows,
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            _ => Ok(self.empty_exec_result()),
        }
    }
}

// ───────────────────────────── Helper functions ─────────────────────────────

fn validate_main_schema(schema: Option<&xdb_parser::Name>) -> SqliteResult<()> {
    if let Some(s) = schema {
        if !s.name.eq_ignore_ascii_case("main") {
            return Err(SqliteError::Sql(format!("unknown database {}", s.name)));
        }
    }
    Ok(())
}

fn resolve_conflict(
    stmt_conflict: Option<ConflictAction>,
    schema_on_error: OnError,
) -> ConflictAction {
    if let Some(c) = stmt_conflict {
        return c;
    }
    match schema_on_error {
        OnError::Rollback => ConflictAction::Rollback,
        OnError::Abort | OnError::Default | OnError::None => ConflictAction::Abort,
        OnError::Fail => ConflictAction::Fail,
        OnError::Ignore => ConflictAction::Ignore,
        OnError::Replace => ConflictAction::Replace,
    }
}

#[derive(Default)]
struct ReplacedTracker {
    rowids: HashSet<i64>,
    wr_pks: Vec<Vec<SqlValue>>,
}

impl ReplacedTracker {
    fn extend(&mut self, other: Self) {
        self.rowids.extend(other.rowids);
        self.wr_pks.extend(other.wr_pks);
    }

    fn contains_wr_pk(&self, pk: &[SqlValue], ki: &KeyInfo) -> bool {
        self.wr_pks.iter().any(|p| key_tuples_equal(p, pk, ki))
    }
}

fn key_tuples_equal(a: &[SqlValue], b: &[SqlValue], ki: &KeyInfo) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let default_bin =
        xdb_core::value::Collation::builtin(xdb_core::value::BuiltinCollation::Binary, ki.encoding);
    for (i, (va, vb)) in a.iter().zip(b.iter()).enumerate() {
        let coll = ki.collations.get(i).and_then(|c| c.as_ref());
        let effective = match coll {
            Some(c) => Some(c),
            None if ki.encoding != TextEncoding::Utf8 => Some(&default_bin),
            None => None,
        };
        if mem_compare(va, vb, effective) != std::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}

fn validate_index_expr_deterministic(expr: &Expr, ctx: &str) -> SqliteResult<()> {
    match expr {
        Expr::CurrentTime { .. } => {
            return Err(SqliteError::Sql(format!(
                "non-deterministic functions prohibited in {ctx}"
            )));
        }
        Expr::Function(f) => {
            let lower = f.name.name.to_ascii_lowercase();
            let non_det = match lower.as_str() {
                "random" | "randomblob" | "changes" | "total_changes" | "last_insert_rowid" => true,
                "date" | "time" | "datetime" | "julianday" | "unixepoch" => {
                    f.args.is_empty()
                        || f.args.iter().any(|a| {
                            matches!(a, Expr::String(s) if s.value.eq_ignore_ascii_case("now")
                                || s.value.eq_ignore_ascii_case("localtime")
                                || s.value.eq_ignore_ascii_case("utc"))
                        })
                }
                "strftime" => {
                    f.args.len() < 2
                        || f.args.iter().skip(1).any(|a| {
                            matches!(a, Expr::String(s) if s.value.eq_ignore_ascii_case("now")
                                || s.value.eq_ignore_ascii_case("localtime")
                                || s.value.eq_ignore_ascii_case("utc"))
                        })
                }
                _ => false,
            };
            if non_det {
                return Err(SqliteError::Sql(format!(
                    "non-deterministic functions prohibited in {ctx}"
                )));
            }
        }
        _ => {}
    }
    for child in xdb_parser::expr_children(expr) {
        validate_index_expr_deterministic(child, ctx)?;
    }
    Ok(())
}

fn collect_generated_col_deps(
    expr: &Expr,
    table_name: &str,
    cols: &[xdb_parser::ColumnDefinition],
    deps: &mut Vec<usize>,
) {
    if let Expr::Column(c) = expr {
        let tbl_ok = c.table.is_none()
            || c.table
                .as_ref()
                .is_some_and(|t| t.name.eq_ignore_ascii_case(table_name));
        if tbl_ok {
            if let Some(pos) = cols
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
            {
                if !deps.contains(&pos) {
                    deps.push(pos);
                }
            }
        }
    }
    for child in xdb_parser::expr_children(expr) {
        collect_generated_col_deps(child, table_name, cols, deps);
    }
}

fn generated_eval_order_from_defs(
    table_name: &str,
    cols: &[xdb_parser::ColumnDefinition],
) -> SqliteResult<Vec<usize>> {
    let n = cols.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut has_gen = false;
    for (cid, col) in cols.iter().enumerate() {
        if let Some(ref expr) = col.generated {
            has_gen = true;
            collect_generated_col_deps(expr, table_name, cols, &mut deps[cid]);
        }
    }
    if !has_gen {
        return Ok(Vec::new());
    }
    // 0 = unvisited, 1 = visiting, 2 = done
    let mut state = vec![0u8; n];
    let mut order = Vec::new();
    fn dfs(
        u: usize,
        cols: &[xdb_parser::ColumnDefinition],
        deps: &[Vec<usize>],
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> SqliteResult<()> {
        state[u] = 1;
        for &v in &deps[u] {
            if cols[v].generated.is_none() {
                continue;
            }
            match state[v] {
                1 => {
                    return Err(SqliteError::Sql(format!(
                        "generated column loop on \"{}\"",
                        cols[v].name
                    )));
                }
                0 => dfs(v, cols, deps, state, order)?,
                _ => {}
            }
        }
        state[u] = 2;
        order.push(u);
        Ok(())
    }
    for (cid, col) in cols.iter().enumerate() {
        if col.generated.is_some() && state[cid] == 0 {
            dfs(cid, cols, &deps, &mut state, &mut order)?;
        }
    }
    Ok(order)
}

fn eval_generated_column_value(
    table: &TableInfo,
    cid: usize,
    expr: &Expr,
    scope_cols: &[ScopeColumn],
    row: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<SqlValue> {
    let params = QueryParams::empty();
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(qopts, &params, &no_sub);
    env.dqs = true;
    env.fn_ctx.pure_context = Some("a generated column");

    let mut scope = EvalScope::simple(scope_cols, row);
    scope.no_rowid_aliases = true;
    let scopes = [scope];
    let raw = eval_expr(expr, &scopes, &env)?;
    let col = &table.columns[cid];
    if table.strict {
        validate_strict_value(raw, &col.decl_type, &table.name, &col.name)
    } else {
        Ok(apply_affinity(raw, col.affinity))
    }
}

enum RowWriteOutcome {
    Written {
        final_rowid: Option<i64>,
        final_row: Vec<SqlValue>,
        was_insert: bool,
    },
    Ignored,
}

fn encode_table_record(
    table: &TableInfo,
    row: &[SqlValue],
    enc: TextEncoding,
    schema_format: u32,
) -> Vec<u8> {
    let mut stored_vals = Vec::with_capacity(table.columns.len());
    for (cid, col) in table.columns.iter().enumerate() {
        if col.storage_index.is_none() {
            continue; // VIRTUAL generated column not stored on disk
        }
        if Some(cid) == table.ipk_column && !table.without_rowid {
            // SQLite stores NULL in the IPK slot; its integer value lives in the B-tree cell rowid.
            stored_vals.push(SqlValue::Null);
        } else {
            stored_vals.push(row[cid].clone());
        }
    }
    encode_record_with_format(&stored_vals, enc, schema_format)
}

fn eval_virtual_columns(
    table: &TableInfo,
    full: &mut [SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    let gen_order = generated_eval_order_from_defs(&table.name, &table.definition.columns)?;
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
    for cid in gen_order {
        if table.columns[cid].generated == Some(GeneratedKind::Virtual) {
            if let Some(ref expr) = table.definition.columns[cid].generated {
                full[cid] =
                    eval_generated_column_value(table, cid, expr, &scope_cols, full, qopts)?;
            }
        }
    }
    Ok(())
}

fn build_without_rowid_record_values(table: &TableInfo, row: &[SqlValue]) -> Vec<SqlValue> {
    let n_stored = table
        .columns
        .iter()
        .filter(|c| c.storage_index.is_some())
        .count();
    let mut out = vec![SqlValue::Null; n_stored];
    for (cid, col) in table.columns.iter().enumerate() {
        if let Some(si) = col.storage_index {
            if si < out.len() {
                out[si] = row[cid].clone();
            }
        }
    }
    out
}

fn decode_without_rowid_full_row(
    table: &TableInfo,
    record_vals: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<Vec<SqlValue>> {
    let mut full = vec![SqlValue::Null; table.columns.len()];
    for (cid, col) in table.columns.iter().enumerate() {
        if let Some(si) = col.storage_index {
            let mut v = record_vals
                .get(si)
                .cloned()
                .unwrap_or_else(|| col.default_value.clone());
            // SQLite compactly stores integer-valued REALs as Int on disk; convert back on read.
            if col.affinity == Affinity::Real {
                if let SqlValue::Int(i) = v {
                    v = SqlValue::Real(i as f64);
                }
            }
            full[cid] = v;
        }
    }
    if table.has_virtual_columns {
        eval_virtual_columns(table, &mut full, qopts)?;
    }
    Ok(full)
}

fn build_index_entry_values(
    table: &TableInfo,
    index: &IndexInfo,
    rowid: Option<i64>,
    row: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let params = QueryParams::empty();
    let no_sub = NoSubqueries;

    // Partial index WHERE predicate check (rowid aliases are permitted in partial index WHERE)
    if let Some(ref where_expr) = index.definition.where_clause {
        let where_cols = build_table_scope_columns(table, &table.name);
        let mut where_vals = row.to_vec();
        if !table.without_rowid {
            let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
            while where_vals.len() < where_cols.len() {
                where_vals.push(rid_v.clone());
            }
        }
        let mut where_env = EvalEnv::new(qopts, &params, &no_sub);
        where_env.dqs = true;
        where_env.fn_ctx.pure_context = Some("partial index WHERE clauses");
        let where_scope = [EvalScope::simple(&where_cols, &where_vals)];
        let cond = eval_expr(where_expr, &where_scope, &where_env)?;
        if !boolean_value(&cond, false) {
            return Ok(None);
        }
    }

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
    let mut env = EvalEnv::new(qopts, &params, &no_sub);
    env.dqs = true;
    env.fn_ctx.pure_context = Some("index expressions");
    let mut expr_scope = EvalScope::simple(&scope_cols, row);
    expr_scope.no_rowid_aliases = true;
    let scope = [expr_scope];

    let mut out = Vec::with_capacity(index.columns.len());
    for (pos, ic) in index.columns.iter().enumerate() {
        let val = if ic.cid >= 0 {
            row[ic.cid as usize].clone()
        } else if ic.cid == -1 {
            rowid.map_or(SqlValue::Null, SqlValue::Int)
        } else if let Some(ref expr) = index.definition.columns[pos].expr {
            eval_expr(expr, &scope, &env)?
        } else if let Some(ref expr_sql) = ic.expr {
            let parsed = parse_expression(expr_sql, &ParseOptions::default())
                .map_err(|e| SqliteError::Sql(e.to_string()))?;
            eval_expr(&parsed, &scope, &env)?
        } else {
            SqlValue::Null
        };
        out.push(val);
    }
    Ok(Some(out))
}

fn prefix_has_null(vals: &[SqlValue]) -> bool {
    vals.iter().any(SqlValue::is_null)
}

fn coerce_ipk_value(v: &SqlValue) -> SqliteResult<i64> {
    must_be_int(v).ok_or_else(|| SqliteError::Sql("datatype mismatch".to_string()))
}

fn validate_strict_value(
    v: SqlValue,
    decl_type: &str,
    table_name: &str,
    col_name: &str,
) -> SqliteResult<SqlValue> {
    if v.is_null() {
        return Ok(v);
    }
    let dt = decl_type.trim().to_ascii_uppercase();
    match dt.as_str() {
        "ANY" => Ok(v),
        "INT" | "INTEGER" => {
            let coerced = apply_affinity(v, Affinity::Integer);
            if coerced.storage_class() == StorageClass::Integer {
                Ok(coerced)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in {dt} column {table_name}.{col_name}",
                    storage_label(&coerced)
                )))
            }
        }
        "REAL" => {
            let coerced = apply_affinity(v, Affinity::Real);
            if coerced.storage_class() == StorageClass::Real {
                Ok(coerced)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in REAL column {table_name}.{col_name}",
                    storage_label(&coerced)
                )))
            }
        }
        "TEXT" => {
            if v.storage_class() == StorageClass::Text {
                Ok(v)
            } else {
                let coerced = apply_affinity(v, Affinity::Text);
                if coerced.storage_class() == StorageClass::Text {
                    Ok(coerced)
                } else {
                    Err(SqliteError::Sql(format!(
                        "cannot store {} value in TEXT column {table_name}.{col_name}",
                        storage_label(&coerced)
                    )))
                }
            }
        }
        "BLOB" => {
            if v.storage_class() == StorageClass::Blob {
                Ok(v)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in BLOB column {table_name}.{col_name}",
                    storage_label(&v)
                )))
            }
        }
        _ => Ok(v),
    }
}

fn storage_label(v: &SqlValue) -> &'static str {
    match v {
        SqlValue::Null => "NULL",
        SqlValue::Int(_) => "INT",
        SqlValue::Real(_) => "REAL",
        SqlValue::Text(_) => "TEXT",
        SqlValue::Blob(_) => "BLOB",
    }
}

fn eval_standalone_expr(
    expr: &Expr,
    params: &QueryParams,
    qopts: &QueryOptions,
) -> SqliteResult<SqlValue> {
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(qopts, params, &no_sub);
    env.dqs = true;
    eval_expr(expr, &[], &env).map_err(SqliteError::from)
}

fn count_synth_rowid_aliases(table: &TableInfo) -> usize {
    if table.without_rowid {
        return 0;
    }
    let mut n = 0;
    for rname in ["rowid", "_rowid_", "oid"] {
        if !table
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(rname))
        {
            n += 1;
        }
    }
    n
}

fn build_table_scope_columns(table: &TableInfo, alias: &str) -> Vec<ScopeColumn> {
    let mut cols: Vec<ScopeColumn> = table
        .columns
        .iter()
        .map(|c| ScopeColumn {
            table: Some(alias.to_string()),
            name: c.name.clone(),
            affinity: c.affinity,
            collation: c.collation.clone(),
            is_rowid_alias: false,
        })
        .collect();
    if !table.without_rowid {
        for rname in ["rowid", "_rowid_", "oid"] {
            if !table
                .columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(rname))
            {
                cols.push(ScopeColumn {
                    table: Some(alias.to_string()),
                    name: rname.to_string(),
                    affinity: Affinity::Integer,
                    collation: "BINARY".to_string(),
                    is_rowid_alias: true,
                });
            }
        }
    }
    cols
}

fn build_upsert_scope_columns(table: &TableInfo, alias: &str) -> Vec<ScopeColumn> {
    let mut cols = build_table_scope_columns(table, alias);
    for c in &table.columns {
        cols.push(ScopeColumn {
            table: Some("excluded".to_string()),
            name: c.name.clone(),
            affinity: c.affinity,
            collation: c.collation.clone(),
            is_rowid_alias: false,
        });
    }
    cols
}

fn apply_set_clauses(
    table: &TableInfo,
    old_rowid: Option<i64>,
    old_row: &[SqlValue],
    sets: &[SetClause],
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqliteResult<(Option<i64>, Vec<SqlValue>)> {
    let mut new_row = old_row.to_vec();
    let mut new_rowid = old_rowid;

    for sc in sets {
        if sc.columns.len() == 1 {
            let cname = &sc.columns[0].name;
            let val = eval_expr(&sc.expr, scopes, env)?;
            assign_column(table, cname, val, &mut new_rowid, &mut new_row)?;
        } else if let Expr::Vector { items, .. } = &sc.expr {
            if items.len() != sc.columns.len() {
                return Err(SqliteError::Sql(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    items.len()
                )));
            }
            for (c, e) in sc.columns.iter().zip(items.iter()) {
                let val = eval_expr(e, scopes, env)?;
                assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)?;
            }
        } else if let Expr::Subquery { select, .. } = &sc.expr {
            let (cols, rows) = env.subqueries.run_select(select, scopes)?;
            if cols.len() != sc.columns.len() {
                return Err(SqliteError::Sql(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    cols.len()
                )));
            }
            if let Some(first_row) = rows.into_iter().next() {
                for (c, val) in sc.columns.iter().zip(first_row.into_iter()) {
                    assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)?;
                }
            } else {
                for c in &sc.columns {
                    assign_column(table, &c.name, SqlValue::Null, &mut new_rowid, &mut new_row)?;
                }
            }
        } else {
            return Err(SqliteError::Sql(
                "multi-column SET requires row value".to_string(),
            ));
        }
    }
    Ok((new_rowid, new_row))
}

fn assign_column(
    table: &TableInfo,
    cname: &str,
    val: SqlValue,
    new_rowid: &mut Option<i64>,
    new_row: &mut [SqlValue],
) -> SqliteResult<()> {
    if let Some(cid) = table
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(cname))
    {
        if table.columns[cid].generated.is_some() {
            return Err(SqliteError::Sql(format!(
                "cannot UPDATE generated column \"{cname}\""
            )));
        }
        if Some(cid) == table.ipk_column && !table.without_rowid {
            let rid = coerce_ipk_value(&val)?;
            *new_rowid = Some(rid);
            new_row[cid] = SqlValue::Int(rid);
        } else {
            new_row[cid] = val;
        }
        return Ok(());
    }
    if !table.without_rowid
        && matches!(
            cname.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
        )
    {
        let rid = coerce_ipk_value(&val)?;
        *new_rowid = Some(rid);
        if let Some(ipk) = table.ipk_column {
            new_row[ipk] = SqlValue::Int(rid);
        }
        return Ok(());
    }
    Err(SqliteError::Sql(format!("no such column: {cname}")))
}

fn unwrap_collate(mut e: &Expr) -> &Expr {
    while let Expr::Collate { operand, .. } = e {
        e = operand;
    }
    e
}

fn expr_structural_eq_ci(a: &Expr, b: &Expr) -> bool {
    match (unwrap_collate(a), unwrap_collate(b)) {
        (Expr::Column(ca), Expr::Column(cb)) => {
            ca.column.name.eq_ignore_ascii_case(&cb.column.name)
        }
        (Expr::Number(na), Expr::Number(nb)) => na.text == nb.text,
        (Expr::String(sa), Expr::String(sb)) => sa.value == sb.value,
        (Expr::Blob(ba), Expr::Blob(bb)) => ba.value == bb.value,
        (Expr::Null { .. }, Expr::Null { .. }) => true,
        (Expr::Boolean { value: va, .. }, Expr::Boolean { value: vb, .. }) => va == vb,
        (
            Expr::Unary {
                op: oa,
                operand: xa,
                ..
            },
            Expr::Unary {
                op: ob,
                operand: xb,
                ..
            },
        ) => oa == ob && expr_structural_eq_ci(xa, xb),
        (
            Expr::Binary {
                op: oa,
                left: la,
                right: ra,
                ..
            },
            Expr::Binary {
                op: ob,
                left: lb,
                right: rb,
                ..
            },
        ) => oa == ob && expr_structural_eq_ci(la, lb) && expr_structural_eq_ci(ra, rb),
        (Expr::Function(fa), Expr::Function(fb)) => {
            fa.name.name.eq_ignore_ascii_case(&fb.name.name)
                && fa.star == fb.star
                && fa.quantifier == fb.quantifier
                && fa.args.len() == fb.args.len()
                && fa
                    .args
                    .iter()
                    .zip(&fb.args)
                    .all(|(x, y)| expr_structural_eq_ci(x, y))
        }
        (
            Expr::Cast {
                operand: oa,
                type_name: ta,
                ..
            },
            Expr::Cast {
                operand: ob,
                type_name: tb,
                ..
            },
        ) => {
            expr_structural_eq_ci(oa, ob)
                && ta.as_ref().map(|t| t.text.to_ascii_uppercase())
                    == tb.as_ref().map(|t| t.text.to_ascii_uppercase())
        }
        _ => false,
    }
}

fn upsert_target_matches(
    table: &TableInfo,
    up: &xdb_parser::UpsertClause,
    conflicts: &[(Option<i64>, Vec<SqlValue>, String, OnError)],
) -> Option<usize> {
    if conflicts.is_empty() {
        return None;
    }
    let Some(ref target) = up.target else {
        return Some(0); // `ON CONFLICT DO NOTHING` / catch-all matches any conflict
    };
    if target.is_empty() {
        return None;
    }
    let all_simple_cols = target
        .iter()
        .all(|t| matches!(unwrap_collate(&t.expr), Expr::Column(_)));
    let target_col_set: HashSet<String> = if all_simple_cols {
        target
            .iter()
            .filter_map(|t| match unwrap_collate(&t.expr) {
                Expr::Column(c) => Some(c.column.name.to_ascii_lowercase()),
                _ => None,
            })
            .collect()
    } else {
        HashSet::new()
    };

    // Match against IPK or any UNIQUE index matching target columns/expressions & partial status.
    for (idx, (_, _, label, _)) in conflicts.iter().enumerate() {
        if all_simple_cols && up.target_where.is_none() {
            if let Some(ipk) = table.ipk_column {
                let ipk_name = table.columns[ipk].name.to_ascii_lowercase();
                if target.len() == 1
                    && target_col_set.len() == 1
                    && target_col_set.contains(&ipk_name)
                    && label.eq_ignore_ascii_case(&format!("{}.{ipk_name}", table.name))
                {
                    return Some(idx);
                }
            }
        }
        for ix in &table.indexes {
            if !ix.unique || ix.key_count != target.len() {
                continue;
            }
            if up.target_where.is_some() != ix.partial {
                continue;
            }
            let label_matches = label.eq_ignore_ascii_case(&format!("{}.{}", table.name, ix.name))
                || (ix.without_rowid_primary_key && label.ends_with("PRIMARY KEY"));
            if !label_matches {
                continue;
            }
            if all_simple_cols {
                let all_ix_cols_named = ix.columns[..ix.key_count]
                    .iter()
                    .all(|ic| ic.cid >= 0 && ic.name.is_some());
                if !all_ix_cols_named {
                    continue;
                }
                let ix_cols: HashSet<String> = ix.columns[..ix.key_count]
                    .iter()
                    .filter_map(|ic| ic.name.as_ref().map(|n| n.to_ascii_lowercase()))
                    .collect();
                if ix_cols == target_col_set {
                    return Some(idx);
                }
            } else {
                // Match each target term against a distinct key slot of `ix`.
                let mut used = vec![false; ix.key_count];
                let mut all_matched = true;
                for term in target {
                    let mut found = false;
                    for (k, ic) in ix.columns[..ix.key_count].iter().enumerate() {
                        if used[k] {
                            continue;
                        }
                        let matched = if ic.cid >= 0 {
                            matches!(
                                unwrap_collate(&term.expr),
                                Expr::Column(c) if ic.name.as_ref().is_some_and(|n| n.eq_ignore_ascii_case(&c.column.name))
                            )
                        } else if let Some(ref ix_expr) = ix.definition.columns[k].expr {
                            expr_structural_eq_ci(&term.expr, ix_expr)
                        } else {
                            false
                        };
                        if matched {
                            used[k] = true;
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        all_matched = false;
                        break;
                    }
                }
                if all_matched {
                    return Some(idx);
                }
            }
        }
    }
    None
}

fn returning_column_names(table: &TableInfo, ret: &[ResultColumn]) -> Vec<String> {
    let mut names = Vec::new();
    for rc in ret {
        match rc {
            ResultColumn::Star(_) => {
                for c in &table.columns {
                    names.push(c.name.clone());
                }
            }
            ResultColumn::Expr(re) => {
                let name = match &re.alias {
                    Some(a) => a.name.clone(),
                    None => match &re.expr {
                        Expr::Column(c) => c.column.name.clone(),
                        _ => re.text.trim().to_string(),
                    },
                };
                names.push(name);
            }
        }
    }
    names
}

fn eval_returning(
    table: &TableInfo,
    rowid: Option<i64>,
    row: &[SqlValue],
    ret: &[ResultColumn],
    params: &QueryParams,
    qopts: &QueryOptions,
) -> SqliteResult<Vec<SqlValue>> {
    let scope_cols = build_table_scope_columns(table, &table.name);
    let mut vals = row.to_vec();
    if !table.without_rowid {
        let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
        while vals.len() < scope_cols.len() {
            vals.push(rid_v.clone());
        }
    }
    let no_sub = NoSubqueries;
    let env = EvalEnv::new(qopts, params, &no_sub);
    let scope = [EvalScope::simple(&scope_cols, &vals)];
    let mut out = Vec::new();
    for rc in ret {
        match rc {
            ResultColumn::Star(_) => out.extend_from_slice(row),
            ResultColumn::Expr(re) => out.push(eval_expr(&re.expr, &scope, &env)?),
        }
    }
    Ok(out)
}

// ───────────────────────────── DDL SQL rewrite helpers ─────────────────────────────

fn strip_if_not_exists(raw_sql: &str, has_if_not_exists: bool) -> String {
    // Match SQLite's `sqlite_schema.sql` normalization: strip leading whitespace & SQL
    // comments (so stored DDL always starts at `CREATE`) and trailing whitespace/comments/`;`,
    // then remove the header `IF NOT EXISTS` clause only when present in the AST.
    let raw_tokens = xdb_parser::raw_tokens(raw_sql, Dialect::Sqlite);
    let mut first_idx = 0;
    while first_idx < raw_tokens.len()
        && matches!(
            raw_tokens[first_idx].kind,
            xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
        )
    {
        first_idx += 1;
    }
    let mut last_idx = raw_tokens.len();
    while last_idx > first_idx
        && matches!(
            raw_tokens[last_idx - 1].kind,
            xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT | xdb_parser::tk::SEMI
        )
    {
        last_idx -= 1;
    }
    let sql = if first_idx < last_idx {
        &raw_sql[raw_tokens[first_idx].start..raw_tokens[last_idx - 1].end]
    } else {
        raw_sql.trim()
    };
    if !has_if_not_exists {
        return sql.to_string();
    }
    let tokens = xdb_parser::raw_tokens(sql, Dialect::Sqlite);
    let mut i = 0;
    while i + 2 < tokens.len() {
        // Stop scanning once we enter the column list or `AS` clause.
        if tokens[i].kind == xdb_parser::tk::LP || tokens[i].kind == xdb_parser::tk::AS {
            break;
        }
        let t0 = &sql[tokens[i].start..tokens[i].end];
        if t0.eq_ignore_ascii_case("IF") {
            let mut j = i + 1;
            while j < tokens.len()
                && matches!(
                    tokens[j].kind,
                    xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                )
            {
                j += 1;
            }
            if j < tokens.len()
                && sql[tokens[j].start..tokens[j].end].eq_ignore_ascii_case("NOT")
            {
                let mut k = j + 1;
                while k < tokens.len()
                    && matches!(
                        tokens[k].kind,
                        xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                    )
                {
                    k += 1;
                }
                if k < tokens.len()
                    && sql[tokens[k].start..tokens[k].end].eq_ignore_ascii_case("EXISTS")
                {
                    let mut after = tokens[k].end;
                    while after < sql.len() && sql.as_bytes()[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    let mut out = String::with_capacity(sql.len());
                    out.push_str(&sql[..tokens[i].start]);
                    out.push_str(&sql[after..]);
                    return out;
                }
            }
        }
        i += 1;
    }
    sql.to_string()
}

fn quote_ident_if_needed(name: &str) -> String {
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
        && xdb_parser::keyword_code(name.as_bytes()) == xdb_parser::tk::ID
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn rewrite_create_table_name(ddl: &str, new_name: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.name.span;
    let mut out = String::with_capacity(ddl.len() + new_name.len());
    out.push_str(&ddl[..sp.start]);
    out.push_str(&quote_ident_if_needed(new_name));
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

fn collect_expr_table_ref_spans(expr: &Expr, old_name: &str, out: &mut Vec<xdb_parser::Span>) {
    if let Expr::Column(c) = expr {
        if let Some(ref t) = c.table {
            if t.name.eq_ignore_ascii_case(old_name) {
                out.push(t.span);
            }
        }
    }
    for child in xdb_parser::expr_children(expr) {
        collect_expr_table_ref_spans(child, old_name, out);
    }
}

fn rewrite_on_table_name(ddl: &str, old_name: &str, new_name: &str) -> String {
    match xdb_parser::parse_statement(ddl, &ParseOptions::default()) {
        Ok(Statement::CreateIndex(ci)) => {
            let mut spans = vec![ci.table.span];
            for ot in &ci.columns {
                collect_expr_table_ref_spans(&ot.expr, old_name, &mut spans);
            }
            if let Some(ref we) = ci.where_clause {
                collect_expr_table_ref_spans(we, old_name, &mut spans);
            }
            let quoted = quote_ident_if_needed(new_name);
            apply_span_replacements(ddl, spans, &quoted)
        }
        Ok(Statement::CreateTrigger(ct)) => {
            if ct.table.name.eq_ignore_ascii_case(old_name) {
                let quoted = quote_ident_if_needed(new_name);
                apply_span_replacements(ddl, vec![ct.table.span], &quoted)
            } else {
                ddl.to_string()
            }
        }
        _ => ddl.to_string(),
    }
}

fn append_column_to_create_table(ddl: &str, col_sql: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    // Insert `, <col_sql>` right after the last column definition (BEFORE any trailing
    // table constraints like `UNIQUE (...)` / `PRIMARY KEY (...)` / `FOREIGN KEY (...)`,
    // which must follow the column list in SQLite's CREATE TABLE grammar).
    let last_end = ct
        .columns
        .last()
        .map(|c| c.span.end)
        .ok_or_else(|| SqliteError::corrupt("empty CREATE TABLE"))?;
    let mut out = String::with_capacity(ddl.len() + col_sql.len() + 2);
    out.push_str(&ddl[..last_end]);
    out.push_str(", ");
    out.push_str(col_sql);
    out.push_str(&ddl[last_end..]);
    Ok(out)
}

fn collect_expr_col_ref_spans(
    expr: &Expr,
    table_name: &str,
    old_col: &str,
    out: &mut Vec<xdb_parser::Span>,
) {
    match expr {
        Expr::Column(c) => {
            let tbl_ok = c.table.is_none()
                || c.table
                    .as_ref()
                    .is_some_and(|t| t.name.eq_ignore_ascii_case(table_name));
            if tbl_ok && c.column.name.eq_ignore_ascii_case(old_col) {
                out.push(c.column.span);
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_col_ref_spans(left, table_name, old_col, out);
            collect_expr_col_ref_spans(right, table_name, old_col, out);
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_expr_col_ref_spans(left, table_name, old_col, out);
            collect_expr_col_ref_spans(right, table_name, old_col, out);
            if let Some(e) = escape {
                collect_expr_col_ref_spans(e, table_name, old_col, out);
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
            collect_expr_col_ref_spans(low, table_name, old_col, out);
            collect_expr_col_ref_spans(high, table_name, old_col, out);
        }
        Expr::InList { operand, items, .. } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
            for item in items {
                collect_expr_col_ref_spans(item, table_name, old_col, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                collect_expr_col_ref_spans(op, table_name, old_col, out);
            }
            for w in whens {
                collect_expr_col_ref_spans(&w.when, table_name, old_col, out);
                collect_expr_col_ref_spans(&w.then, table_name, old_col, out);
            }
            if let Some(el) = else_expr {
                collect_expr_col_ref_spans(el, table_name, old_col, out);
            }
        }
        Expr::Function(f) => {
            for a in &f.args {
                collect_expr_col_ref_spans(a, table_name, old_col, out);
            }
            if let Some(ref obs) = f.order_by {
                for ob in obs {
                    collect_expr_col_ref_spans(&ob.expr, table_name, old_col, out);
                }
            }
            if let Some(ref flt) = f.filter {
                collect_expr_col_ref_spans(flt, table_name, old_col, out);
            }
        }
        Expr::Vector { items, .. } => {
            for item in items {
                collect_expr_col_ref_spans(item, table_name, old_col, out);
            }
        }
        Expr::Raise {
            message: Some(m), ..
        } => {
            collect_expr_col_ref_spans(m, table_name, old_col, out);
        }
        _ => {}
    }
}

fn collect_create_table_col_refs(
    ct: &CreateTableStmt,
    table_name: &str,
    skip_cid: Option<usize>,
    old_col: &str,
    out: &mut Vec<xdb_parser::Span>,
) {
    for (i, col) in ct.columns.iter().enumerate() {
        if skip_cid == Some(i) {
            continue;
        }
        for con in &col.constraints {
            match con {
                ColumnConstraint::Check(chk) => {
                    collect_expr_col_ref_spans(&chk.expr, table_name, old_col, out);
                }
                ColumnConstraint::Generated { expr, .. } => {
                    collect_expr_col_ref_spans(expr, table_name, old_col, out);
                }
                _ => {}
            }
        }
    }
    for con in &ct.constraints {
        match con {
            TableConstraint::PrimaryKey { columns, .. }
            | TableConstraint::Unique { columns, .. } => {
                for ot in columns {
                    collect_expr_col_ref_spans(&ot.expr, table_name, old_col, out);
                }
            }
            TableConstraint::Check(chk) => {
                collect_expr_col_ref_spans(&chk.expr, table_name, old_col, out);
            }
            TableConstraint::ForeignKey { columns, .. } => {
                for item in columns {
                    if item.name.name.eq_ignore_ascii_case(old_col) {
                        out.push(item.name.span);
                    }
                }
            }
        }
    }
}

fn apply_span_replacements(ddl: &str, mut spans: Vec<xdb_parser::Span>, replacement: &str) -> String {
    if spans.is_empty() {
        return ddl.to_string();
    }
    spans.sort_by(|a, b| b.start.cmp(&a.start).then_with(|| b.end.cmp(&a.end)));
    spans.dedup();
    let mut out = ddl.to_string();
    let mut last_start = usize::MAX;
    for sp in spans {
        if sp.end <= out.len() && sp.start <= sp.end && sp.end <= last_start {
            out.replace_range(sp.start..sp.end, replacement);
            last_start = sp.start;
        }
    }
    out
}

fn rewrite_column_name_in_ddl(
    ddl: &str,
    table_name: &str,
    cid: usize,
    old_col: &str,
    new_name: &str,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let mut spans = vec![ct.columns[cid].name.span];
    collect_create_table_col_refs(&ct, table_name, None, old_col, &mut spans);
    let quoted = quote_ident_if_needed(new_name);
    Ok(apply_span_replacements(ddl, spans, &quoted))
}

fn rewrite_column_name_in_index_ddl(
    ddl: &str,
    table_name: &str,
    old_col: &str,
    new_name: &str,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateIndex(ci) = stmt else {
        return Ok(ddl.to_string());
    };
    let mut spans = Vec::new();
    for ot in &ci.columns {
        collect_expr_col_ref_spans(&ot.expr, table_name, old_col, &mut spans);
    }
    if let Some(ref we) = ci.where_clause {
        collect_expr_col_ref_spans(we, table_name, old_col, &mut spans);
    }
    let quoted = quote_ident_if_needed(new_name);
    Ok(apply_span_replacements(ddl, spans, &quoted))
}

fn check_column_unreferenced_in_ddl(
    ddl: &str,
    table_name: &str,
    cid: usize,
    old_col: &str,
) -> SqliteResult<()> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Ok(());
    };
    let mut spans = Vec::new();
    collect_create_table_col_refs(&ct, table_name, Some(cid), old_col, &mut spans);
    if !spans.is_empty() {
        return Err(SqliteError::Sql(format!(
            "error in table {table_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn check_column_unreferenced_in_index_ddl(
    ddl: &str,
    idx_name: &str,
    table_name: &str,
    old_col: &str,
) -> SqliteResult<()> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateIndex(ci) = stmt else {
        return Ok(());
    };
    let mut spans = Vec::new();
    for ot in &ci.columns {
        collect_expr_col_ref_spans(&ot.expr, table_name, old_col, &mut spans);
    }
    if let Some(ref we) = ci.where_clause {
        collect_expr_col_ref_spans(we, table_name, old_col, &mut spans);
    }
    if !spans.is_empty() {
        return Err(SqliteError::Sql(format!(
            "error in index {idx_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn collect_from_aliases_for_table(
    from: &xdb_parser::FromSource,
    table_name: &str,
    aliases: &mut Vec<String>,
    subqueries: &mut Vec<SelectStmt>,
) {
    match from {
        xdb_parser::FromSource::Table(t) => {
            if t.name.name.eq_ignore_ascii_case(table_name) {
                let alias = t.alias.as_ref().unwrap_or(&t.name).name.clone();
                aliases.push(alias);
            }
        }
        xdb_parser::FromSource::Subquery(s) => {
            subqueries.push((*s.select).clone());
        }
        xdb_parser::FromSource::Group(g) => {
            collect_from_aliases_for_table(&g.source, table_name, aliases, subqueries);
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            collect_from_aliases_for_table(left, table_name, aliases, subqueries);
            collect_from_aliases_for_table(right, table_name, aliases, subqueries);
        }
        xdb_parser::FromSource::Function(_) => {}
    }
}

fn expr_refs_any_alias_col(
    expr: &Expr,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
) -> bool {
    match expr {
        Expr::Column(c) => {
            if c.column.name.eq_ignore_ascii_case(old_col) {
                match &c.table {
                    None => {
                        if allow_unqualified {
                            return true;
                        }
                    }
                    Some(t) => {
                        if aliases.iter().any(|a| t.name.eq_ignore_ascii_case(a)) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            if let Expr::InSelect { operand, .. } = expr {
                if expr_refs_any_alias_col(operand, aliases, allow_unqualified, table_name, old_col)
                {
                    return true;
                }
            }
            select_refs_table_col(select, table_name, old_col, aliases, allow_unqualified)
        }
        _ => xdb_parser::expr_children(expr).into_iter().any(|ch| {
            expr_refs_any_alias_col(ch, aliases, allow_unqualified, table_name, old_col)
        }),
    }
}

fn from_join_clauses_ref_col(
    from: &xdb_parser::FromSource,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
) -> bool {
    match from {
        xdb_parser::FromSource::Join {
            left,
            right,
            on,
            using,
            ..
        } => {
            if from_join_clauses_ref_col(left, aliases, allow_unqualified, table_name, old_col)
                || from_join_clauses_ref_col(right, aliases, allow_unqualified, table_name, old_col)
            {
                return true;
            }
            if let Some(on_e) = on {
                if expr_refs_any_alias_col(on_e, aliases, allow_unqualified, table_name, old_col) {
                    return true;
                }
            }
            if let Some(us) = using {
                if !aliases.is_empty()
                    && us.iter().any(|u| u.name.eq_ignore_ascii_case(old_col))
                {
                    return true;
                }
            }
            false
        }
        xdb_parser::FromSource::Group(g) => {
            from_join_clauses_ref_col(&g.source, aliases, allow_unqualified, table_name, old_col)
        }
        _ => false,
    }
}

fn select_core_refs_table_col(
    core: &SelectCore,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
) -> bool {
    let mut aliases = outer_aliases.to_vec();
    let mut local_aliases = Vec::new();
    let mut subqueries = Vec::new();
    if let Some(ref from) = core.from {
        collect_from_aliases_for_table(from, table_name, &mut local_aliases, &mut subqueries);
    }
    aliases.extend(local_aliases.iter().cloned());
    let allow_unq = outer_unqualified || !local_aliases.is_empty();

    for sq in &subqueries {
        if select_refs_table_col(sq, table_name, old_col, outer_aliases, outer_unqualified) {
            return true;
        }
    }
    if let Some(ref from) = core.from {
        if from_join_clauses_ref_col(from, &aliases, allow_unq, table_name, old_col) {
            return true;
        }
    }
    for rc in &core.columns {
        if let ResultColumn::Expr(re) = rc {
            if expr_refs_any_alias_col(&re.expr, &aliases, allow_unq, table_name, old_col) {
                return true;
            }
        }
    }
    if let Some(ref we) = core.where_clause {
        if expr_refs_any_alias_col(we, &aliases, allow_unq, table_name, old_col) {
            return true;
        }
    }
    if let Some(ref gb) = core.group_by {
        if gb
            .iter()
            .any(|e| expr_refs_any_alias_col(e, &aliases, allow_unq, table_name, old_col))
        {
            return true;
        }
    }
    if let Some(ref hv) = core.having {
        if expr_refs_any_alias_col(hv, &aliases, allow_unq, table_name, old_col) {
            return true;
        }
    }
    if let Some(ref wins) = core.windows {
        for w in wins {
            if let Some(ref pb) = w.window.partition_by {
                if pb
                    .iter()
                    .any(|e| expr_refs_any_alias_col(e, &aliases, allow_unq, table_name, old_col))
                {
                    return true;
                }
            }
            if let Some(ref ob) = w.window.order_by {
                if ob
                    .iter()
                    .any(|o| expr_refs_any_alias_col(&o.expr, &aliases, allow_unq, table_name, old_col))
                {
                    return true;
                }
            }
        }
    }
    false
}

fn select_body_refs_table_col(
    body: &SelectBody,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
) -> bool {
    match body {
        SelectBody::Core(core) => {
            select_core_refs_table_col(core, table_name, old_col, outer_aliases, outer_unqualified)
        }
        SelectBody::Compound { left, right, .. } => {
            select_body_refs_table_col(left, table_name, old_col, outer_aliases, outer_unqualified)
                || select_body_refs_table_col(
                    right,
                    table_name,
                    old_col,
                    outer_aliases,
                    outer_unqualified,
                )
        }
        SelectBody::Values(v) => v.rows.iter().any(|r| {
            r.items.iter().any(|e| {
                expr_refs_any_alias_col(e, outer_aliases, outer_unqualified, table_name, old_col)
            })
        }),
    }
}

fn select_refs_table_col(
    stmt: &SelectStmt,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
) -> bool {
    if let Some(ref with) = stmt.with {
        for cte in &with.ctes {
            if select_refs_table_col(&cte.select, table_name, old_col, &[], false) {
                return true;
            }
        }
    }
    if select_body_refs_table_col(
        &stmt.body,
        table_name,
        old_col,
        outer_aliases,
        outer_unqualified,
    ) {
        return true;
    }
    if let Some(ref ob) = stmt.order_by {
        let mut top_aliases = outer_aliases.to_vec();
        let mut subq = Vec::new();
        if let SelectBody::Core(ref core) = stmt.body {
            if let Some(ref from) = core.from {
                collect_from_aliases_for_table(from, table_name, &mut top_aliases, &mut subq);
            }
        }
        let allow_unq = outer_unqualified || !top_aliases.is_empty();
        if ob
            .iter()
            .any(|o| expr_refs_any_alias_col(&o.expr, &top_aliases, allow_unq, table_name, old_col))
        {
            return true;
        }
    }
    false
}

fn check_column_unreferenced_in_view_ddl(
    ddl: &str,
    view_name: &str,
    table_name: &str,
    old_col: &str,
) -> SqliteResult<()> {
    let Ok(Statement::CreateView(cv)) = xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return Ok(());
    };
    if select_refs_table_col(&cv.select, table_name, old_col, &[], false) {
        return Err(SqliteError::Sql(format!(
            "error in view {view_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn check_column_unreferenced_in_trigger_ddl(
    ddl: &str,
    trigger_name: &str,
    table_name: &str,
    old_col: &str,
) -> SqliteResult<()> {
    let Ok(Statement::CreateTrigger(tr)) =
        xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return Ok(());
    };
    let on_this_table = tr.table.name.eq_ignore_ascii_case(table_name);
    let mut trigger_aliases: Vec<String> = Vec::new();
    if on_this_table {
        trigger_aliases.push("NEW".to_string());
        trigger_aliases.push("OLD".to_string());
        if let Some(ref cols) = tr.update_of {
            if cols.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)) {
                return Err(SqliteError::Sql(format!(
                    "error in trigger {trigger_name} after drop column: no such column: {old_col}"
                )));
            }
        }
        if let Some(ref when) = tr.when {
            if expr_refs_any_alias_col(when, &trigger_aliases, false, table_name, old_col) {
                return Err(SqliteError::Sql(format!(
                    "error in trigger {trigger_name} after drop column: no such column: {old_col}"
                )));
            }
        }
    }
    for step in &tr.body {
        let hit = match step {
            xdb_parser::TriggerStep::Select(s) => {
                select_refs_table_col(s, table_name, old_col, &trigger_aliases, false)
            }
            xdb_parser::TriggerStep::Insert(ins) => {
                let target_hit = ins.table.name.name.eq_ignore_ascii_case(table_name)
                    && ins
                        .columns
                        .as_ref()
                        .is_some_and(|cols| cols.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)));
                let sel_hit = ins.select.as_ref().is_some_and(|s| {
                    select_refs_table_col(s, table_name, old_col, &trigger_aliases, false)
                });
                target_hit || sel_hit
            }
            xdb_parser::TriggerStep::Update(upd) => {
                let same_tbl = upd.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(upd.table.alias.as_ref().unwrap_or(&upd.table.name).name.clone());
                }
                let set_hit = upd.set.iter().any(|sc| {
                    (same_tbl && sc.columns.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)))
                        || expr_refs_any_alias_col(&sc.expr, &aliases, same_tbl, table_name, old_col)
                });
                let where_hit = upd.where_clause.as_ref().is_some_and(|w| {
                    expr_refs_any_alias_col(w, &aliases, same_tbl, table_name, old_col)
                });
                set_hit || where_hit
            }
            xdb_parser::TriggerStep::Delete(del) => {
                let same_tbl = del.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(del.table.alias.as_ref().unwrap_or(&del.table.name).name.clone());
                }
                del.where_clause.as_ref().is_some_and(|w| {
                    expr_refs_any_alias_col(w, &aliases, same_tbl, table_name, old_col)
                })
            }
        };
        if hit {
            return Err(SqliteError::Sql(format!(
                "error in trigger {trigger_name} after drop column: no such column: {old_col}"
            )));
        }
    }
    Ok(())
}

fn remove_column_from_ddl(ddl: &str, cid: usize) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.columns[cid].span;
    // Use SQL lexer tokens so SQL comments between column definition and `,` are skipped cleanly.
    let tokens = xdb_parser::raw_tokens(ddl, Dialect::Sqlite);
    if let Some(next_tok) = tokens
        .iter()
        .find(|t| t.start >= sp.end && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT))
    {
        if next_tok.kind == xdb_parser::tk::COMMA {
            let mut end = next_tok.end;
            let bytes = ddl.as_bytes();
            while end < bytes.len() && matches!(bytes[end], b' ' | b'\t') {
                end += 1;
            }
            let mut out = String::with_capacity(ddl.len());
            out.push_str(&ddl[..sp.start]);
            out.push_str(&ddl[end..]);
            return Ok(out);
        }
    }
    if let Some(prev_tok) = tokens
        .iter()
        .rev()
        .find(|t| t.end <= sp.start && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT))
    {
        if prev_tok.kind == xdb_parser::tk::COMMA {
            let mut out = String::with_capacity(ddl.len());
            out.push_str(&ddl[..prev_tok.start]);
            out.push_str(&ddl[sp.end..]);
            return Ok(out);
        }
    }
    let mut out = String::with_capacity(ddl.len());
    out.push_str(&ddl[..sp.start]);
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

/// Build a [`KeyInfo`] covering ONLY the primary-key columns of a `WITHOUT ROWID` table's
/// primary-key B-tree (non-PK columns are payload, never B-tree sort/equality keys).
fn key_info_for_without_rowid_pk(
    pk_idx: &IndexInfo,
    encoding: TextEncoding,
    colls: &CollationRegistry,
) -> SqliteResult<KeyInfo> {
    let specs: Vec<KeyFieldSpec<'_>> = pk_idx
        .columns
        .iter()
        .take(pk_idx.key_count)
        .map(|c| KeyFieldSpec {
            collation: &c.collation,
            desc: c.desc,
        })
        .collect();
    make_key_info(encoding, &specs, pk_idx.key_count, Some(colls))
}
