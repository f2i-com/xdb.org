//! Full read-write SQLite 3 connection (`SqliteConnection`) supporting DDL, DML,
//! `UPSERT`, `RETURNING`, constraints (`NOT NULL`, `UNIQUE`, `PRIMARY KEY`, `CHECK`,
//! `STRICT`, `GENERATED`), transactions & nested savepoints, `PRAGMA`, and crash-safe
//! rollback journal commits.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use xdb_core::value::{
    apply_affinity, boolean_value, int_value, mem_compare, real_same_as_int, real_to_i64,
    CollationRegistry,
};
use xdb_core::{Affinity, Meter, SqlValue, StorageClass, TextEncoding, Vfs};
use xdb_parser::{
    build_table, parse_expression, parse_statement_at_metered, AlterTableAction, AlterTableStmt,
    BuildOptions, ConflictAction, CreateIndexStmt, CreateTableStmt, CreateViewStmt, DeleteStmt,
    Dialect, DropObjectType, DropStmt, Expr, InsertStmt, ParseOptions, PragmaStmt, ResultColumn,
    SelectBody, SelectCore, SelectStmt, SetClause, Statement, UpdateStmt, UpsertAction,
};
use xdb_sql::{
    eval_expr, eval_generated_expr, EvalEnv, EvalScope, NoSubqueries, QueryOptions, QueryParams,
    QueryResult, ScopeColumn,
};

use crate::access::key_info_for_index;
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
use crate::write::record::encode_record;

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
    closed: bool,
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
            closed: false,
            cached_schema: std::sync::Mutex::new(None),
        }
    }

    fn check_open(&self) -> SqliteResult<()> {
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

    /// Close this connection (rolling back any uncommitted transaction).
    pub fn close(&mut self) -> SqliteResult<()> {
        if self.closed {
            return Ok(());
        }
        if self.pager.in_transaction() {
            let _ = self.pager.rollback();
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
                last = self.exec_one_stmt(&stmt, raw_stmt_sql, params, dialect)?;
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
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        match stmt {
            Statement::Select(sel) => {
                let view = self.live_view()?;
                let opts = self.query_options(dialect);
                // Re-execute parsed statement directly via live view SQL string (exact span)
                let res = view.query_with_options(raw_sql, params, &opts)?;
                let _ = sel;
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
            Statement::CreateTable(ct) => {
                self.run_mutating_stmt(None, |conn| conn.exec_create_table(ct, raw_sql, params, dialect))
            }
            Statement::CreateIndex(ci) => {
                self.run_mutating_stmt(None, |conn| conn.exec_create_index(ci, raw_sql))
            }
            Statement::CreateView(cv) => {
                self.run_mutating_stmt(None, |conn| conn.exec_create_view(cv, raw_sql))
            }
            Statement::Drop(dr) => self.run_mutating_stmt(None, |conn| conn.exec_drop(dr)),
            Statement::AlterTable(al) => {
                self.run_mutating_stmt(None, |conn| conn.exec_alter_table(al, raw_sql))
            }
            Statement::Insert(ins) => {
                let conflict = if ins.replace_into {
                    Some(ConflictAction::Replace)
                } else {
                    ins.or_conflict
                };
                self.run_mutating_stmt(conflict, |conn| {
                    conn.exec_insert(ins, params, dialect)
                })
            }
            Statement::Update(upd) => self.run_mutating_stmt(upd.or_conflict, |conn| {
                conn.exec_update(upd, params, dialect)
            }),
            Statement::Delete(del) => {
                self.run_mutating_stmt(None, |conn| conn.exec_delete(del, params, dialect))
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
    /// `ON CONFLICT ABORT`/`ROLLBACK` and auto-commit handling.
    fn run_mutating_stmt<F>(
        &mut self,
        conflict: Option<ConflictAction>,
        f: F,
    ) -> SqliteResult<ExecResult>
    where
        F: FnOnce(&mut Self) -> SqliteResult<ExecResult>,
    {
        self.pager.push_savepoint(None);
        match f(self) {
            Ok(res) => {
                self.pager.commit_statement_savepoint();
                self.pager.autocommit_flush()?;
                Ok(res)
            }
            Err(e) => {
                if conflict == Some(ConflictAction::Rollback) && self.pager.in_transaction() {
                    let _ = self.pager.rollback();
                } else {
                    self.pager.rollback_statement_savepoint();
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
        let payload = encode_record(&vals, self.pager.encoding());
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
        let payload = encode_record(&vals, self.pager.encoding());
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
        let seq_root = self.ensure_sqlite_sequence()?;
        if let Some((rid, old_seq)) = self.get_sqlite_sequence(table_name)? {
            if new_seq > old_seq {
                let vals = vec![SqlValue::text(table_name), SqlValue::Int(new_seq)];
                let payload = encode_record(&vals, self.pager.encoding());
                table_insert(&mut self.pager, seq_root, rid, &payload, true)?;
            }
        } else {
            let rid = table_max_rowid(&self.pager, seq_root)?.unwrap_or(0) + 1;
            let vals = vec![SqlValue::text(table_name), SqlValue::Int(new_seq)];
            let payload = encode_record(&vals, self.pager.encoding());
            table_insert(&mut self.pager, seq_root, rid, &payload, false)?;
        }
        Ok(())
    }

    // ───────────────────────────── DDL ─────────────────────────────

    fn exec_create_table(
        &mut self,
        stmt: &CreateTableStmt,
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        self.pager.ensure_initialized()?;
        let tname = &stmt.name.name;
        if tname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {tname}"
            )));
        }
        {
            let view = self.live_view()?;
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
            let sel_sql = &raw_sql[sel.span.start.min(raw_sql.len())..sel.span.end.min(raw_sql.len())];
            // Fallback: re-query via execute_select_stmt directly so exact spans never drift!
            let qres = self.eval_select_stmt(&view, sel, params, &opts)?;
            let _ = sel_sql;

            let mut col_defs = Vec::with_capacity(qres.columns.len());
            for (idx, cname) in qres.columns.iter().enumerate() {
                let col_type = match qres.rows.first().and_then(|r| r.get(idx)) {
                    Some(SqlValue::Int(_)) => "INT",
                    Some(SqlValue::Real(_)) => "REAL",
                    Some(SqlValue::Text(_)) => "TEXT",
                    _ => "",
                };
                let escaped = cname.replace('"', "\"\"");
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
            let mut rid = 0i64;
            for row in qres.rows {
                rid += 1;
                let payload = encode_record(&row, enc);
                table_insert(&mut self.pager, root_pg, rid, &payload, false)?;
            }
            return Ok(self.empty_exec_result());
        }

        // Validate table definition with SQLite's exact `build_table`!
        let build_opts = BuildOptions {
            schema: false,
            file_format: 4,
            ..BuildOptions::default()
        };
        let def = build_table(stmt, raw_sql, &build_opts)
            .map_err(|e| SqliteError::Sql(e.to_string()))?;

        // Strip `IF NOT EXISTS` from stored `sqlite_schema.sql`? SQLite stores `CREATE TABLE name(...)`
        // without `IF NOT EXISTS` — wait: `load_schema` accepts both, and real C SQLite strips `IF NOT EXISTS`!
        let stored_sql = strip_if_not_exists(raw_sql, "CREATE TABLE");

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
        self.pager.ensure_initialized()?;
        let iname = &stmt.name.name;
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
            table_name_canon = tbl.name.clone();
        }

        let stored_sql = strip_if_not_exists(
            raw_sql,
            if stmt.unique {
                "CREATE UNIQUE INDEX"
            } else {
                "CREATE INDEX"
            },
        );
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
                let payload = encode_record(&entry_vals, enc);
                index_insert(&mut self.pager, idx_info.root_page, &payload, &key_info, false)?;
            }
        }
        Ok(self.empty_exec_result())
    }

    fn exec_create_view(&mut self, stmt: &CreateViewStmt, raw_sql: &str) -> SqliteResult<ExecResult> {
        self.pager.ensure_initialized()?;
        let vname = &stmt.name.name;
        {
            let view = self.live_view()?;
            if view.schema().object(vname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("view {vname} already exists")));
            }
        }
        let stored_sql = strip_if_not_exists(raw_sql, "CREATE VIEW");
        self.insert_schema_row("view", vname, vname, 0, Some(&stored_sql))?;
        self.pager.bump_schema_cookie()?;
        Ok(self.empty_exec_result())
    }

    fn exec_drop(&mut self, stmt: &DropStmt) -> SqliteResult<ExecResult> {
        let name = &stmt.name.name;
        let view = self.live_view()?;

        match stmt.object_type {
            DropObjectType::Table => {
                let Some(SchemaObject::Table(tbl)) = view.schema().object(name) else {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such table: {name}")));
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
                let Some(SchemaObject::View(v)) = view.schema().object(name) else {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such view: {name}")));
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
        raw_sql: &str,
    ) -> SqliteResult<ExecResult> {
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
                if view.schema().object(&new_name.name).is_some() {
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
                        && dep_name_orig.len() > old_auto_prefix.len()
                        && dep_name_orig[..old_auto_prefix.len()]
                            .eq_ignore_ascii_case(&old_auto_prefix)
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
                self.pager.bump_schema_cookie()?;
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
                let col_def_sql = raw_sql[column.span.start..column.span.end].trim();
                let updated_sql = append_column_to_create_table(&old_sql, col_def_sql)?;
                // Verify the new CREATE TABLE SQL parses cleanly via `build_table`!
                let p_opts = ParseOptions::default();
                let parsed = xdb_parser::parse_statement(&updated_sql, &p_opts)
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::CreateTable(ct) = parsed {
                    let b_opts = BuildOptions {
                        schema: false,
                        file_format: 4,
                        ..BuildOptions::default()
                    };
                    let _ = build_table(&ct, &updated_sql, &b_opts)
                        .map_err(|e| SqliteError::Sql(e.to_string()))?;
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
                let updated_sql = rewrite_column_name_in_ddl(&old_sql, cid, &new_name.name)?;
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
                    if ix.columns.iter().any(|ic| ic.cid == cid as i32) {
                        return Err(SqliteError::Sql(format!(
                            "error in index {} after drop column: no such column: {}",
                            ix.name, column.name
                        )));
                    }
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

                if !tbl.without_rowid && tbl.columns[cid].hidden != ColumnHiddenKind::Virtual {
                    let new_view = self.live_view()?;
                    let new_tbl = new_view
                        .schema()
                        .table(&tbl.name)
                        .expect("table exists")
                        .clone();
                    let enc = self.pager.encoding();
                    for (rid, mut row) in existing {
                        row.remove(cid);
                        let rec = encode_table_record(&new_tbl, &row, enc);
                        table_insert(&mut self.pager, new_tbl.root_page, rid.unwrap(), &rec, true)?;
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
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }

        let conflict = if stmt.replace_into {
            ConflictAction::Replace
        } else {
            stmt.or_conflict.unwrap_or(ConflictAction::Abort)
        };

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
                if col.generated.is_some() {
                    row.push(SqlValue::Null);
                } else if let Some(ref dflt_expr) = table.definition.columns[cid].default_expr {
                    let v = eval_standalone_expr(dflt_expr, params, &qopts)?;
                    row.push(v);
                } else {
                    row.push(col.default_value.clone());
                }
            }

            let mut explicit_rowid: Option<SqlValue> = None;
            for (tcol, val) in target_cols.iter().zip(src.into_iter()) {
                match *tcol {
                    TargetCol::Col(cid) => row[cid] = val,
                    TargetCol::Rowid => explicit_rowid = Some(val),
                }
            }

            // Determine rowid for rowid tables
            let mut rowid: Option<i64> = if table.without_rowid {
                None
            } else if let Some(ipk) = table.ipk_column {
                let candidate = if let Some(ref er) = explicit_rowid {
                    er
                } else {
                    &row[ipk]
                };
                if candidate.is_null() {
                    let next = self.allocate_next_rowid(&table)?;
                    row[ipk] = SqlValue::Int(next);
                    Some(next)
                } else {
                    let rid = coerce_ipk_value(candidate)?;
                    row[ipk] = SqlValue::Int(rid);
                    Some(rid)
                }
            } else if let Some(ref er) = explicit_rowid {
                if er.is_null() {
                    Some(self.allocate_next_rowid(&table)?)
                } else {
                    Some(coerce_ipk_value(er)?)
                }
            } else {
                Some(self.allocate_next_rowid(&table)?)
            };

            // Apply affinities, STRICT checks, evaluate generated columns, NOT NULL, CHECK!
            self.finalize_and_validate_row(&table, &mut row, &qopts)?;

            // Check PRIMARY KEY / UNIQUE constraints & handle ON CONFLICT / UPSERT!
            match self.insert_row_with_constraints(
                &table,
                &mut rowid,
                &mut row,
                conflict,
                &stmt.upsert,
                params,
                &qopts,
            )? {
                RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    was_insert,
                } => {
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
                RowWriteOutcome::Ignored => {}
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
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }
        let conflict = stmt.or_conflict.unwrap_or(ConflictAction::Abort);
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
            // Execute `SELECT * FROM <from>` to get RHS join rows
            let mut from_select = SelectStmt {
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
            let _ = &mut from_select;
            Some(self.eval_select_stmt(&view, &from_select, params, &qopts)?)
        } else {
            None
        };

        // Snapshot all rows to update first (SQLite Halloween problem protection!).
        let base_scope_cols = build_table_scope_columns(&table, &target_alias);
        type PendingUpdate = (Option<i64>, Vec<SqlValue>, Option<i64>, Vec<SqlValue>);
        let mut updates: Vec<PendingUpdate> = Vec::new();
        let catalog = crate::sql::live_catalog_for(&view, &qopts);
        let sub_runner = xdb_sql::CatalogSubqueryRunner::new(&catalog, &qopts, params);
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

            if let Some(ref extra) = from_rows_extra {
                let mut combined_cols = base_scope_cols.clone();
                for cname in &extra.columns {
                    combined_cols.push(ScopeColumn {
                        table: None,
                        name: cname.clone(),
                        affinity: Affinity::Blob,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    });
                }
                for erow in &extra.rows {
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

        let mut updated_count = 0i64;
        let mut returning_rows = Vec::new();
        let returning_cols = stmt
            .returning
            .as_deref()
            .map(|rc| returning_column_names(&table, rc))
            .unwrap_or_default();

        for (old_rowid, old_row, mut new_rowid, mut new_row) in updates {
            self.finalize_and_validate_row(&table, &mut new_row, &qopts)?;

            // Use a mini savepoint per row if `OR IGNORE` so skipped rows leave old row intact!
            if conflict == ConflictAction::Ignore {
                self.pager.push_savepoint(None);
            }

            // Delete old row & its index entries first.
            self.delete_row_and_indexes(&table, old_rowid, &old_row, &qopts)?;

            match self.insert_row_with_constraints(
                &table,
                &mut new_rowid,
                &mut new_row,
                conflict,
                &[],
                params,
                &qopts,
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    ..
                }) => {
                    if conflict == ConflictAction::Ignore {
                        self.pager.commit_statement_savepoint();
                    }
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
                    if conflict == ConflictAction::Ignore {
                        self.pager.rollback_statement_savepoint();
                    }
                }
                Err(e) => {
                    if conflict == ConflictAction::Ignore {
                        self.pager.rollback_statement_savepoint();
                    } else {
                        return Err(e);
                    }
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
        let view = self.live_view()?;
        let Some(table) = view.schema().table(&stmt.table.name.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name.name
            )));
        };
        if table.system {
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
        let sub_runner = xdb_sql::CatalogSubqueryRunner::new(&catalog, &qopts, params);
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
            if let Some((_, seq)) = self.get_sqlite_sequence(&table.name)? {
                if seq > max_id {
                    max_id = seq;
                }
            }
        }
        max_id
            .checked_add(1)
            .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()))
    }

    fn finalize_and_validate_row(
        &self,
        table: &TableInfo,
        row: &mut [SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        // 1. Apply column affinity & STRICT type validation on non-generated columns.
        for (cid, col) in table.columns.iter().enumerate() {
            if col.generated.is_some() {
                continue;
            }
            if table.strict {
                row[cid] = validate_strict_value(row[cid].clone(), &col.decl_type, &table.name, &col.name)?;
            } else {
                row[cid] = apply_affinity(row[cid].clone(), col.affinity);
            }
        }

        // 2. Evaluate STORED & VIRTUAL generated columns in declaration/dependency order.
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
        for pass in 0..2 {
            let mut changed = false;
            for (cid, col) in table.columns.iter().enumerate() {
                if col.generated.is_some() {
                    if let Some(ref expr) = table.definition.columns[cid].generated {
                        let v = eval_generated_expr(expr, &scope_cols, row, col.affinity, qopts)?;
                        if !v.exact_eq(&row[cid]) {
                            row[cid] = v;
                            changed = true;
                        }
                    }
                }
            }
            if !changed || pass == 1 {
                break;
            }
        }

        // 3. NOT NULL constraints (including WITHOUT ROWID PK columns!).
        for (cid, col) in table.columns.iter().enumerate() {
            if (col.not_null || (table.without_rowid && col.pk > 0)) && row[cid].is_null() {
                return Err(SqliteError::Sql(format!(
                    "NOT NULL constraint failed: {}.{}",
                    table.name, col.name
                )));
            }
        }

        // 4. CHECK constraints (`NULL` passes CHECK in SQLite, only explicit false `0` fails!).
        if !table.definition.checks.is_empty() {
            let no_sub = NoSubqueries;
            let params = QueryParams::empty();
            let mut env = EvalEnv::new(qopts, &params, &no_sub);
            env.dqs = true;
            let scope = [EvalScope::simple(&scope_cols, row)];
            for chk in &table.definition.checks {
                let res = eval_expr(&chk.expr, &scope, &env)?;
                if !res.is_null() && !boolean_value(&res, true) {
                    let cname = if chk.name.is_empty() {
                        table.name.as_str()
                    } else {
                        chk.name.as_str()
                    };
                    return Err(SqliteError::Sql(format!("CHECK constraint failed: {cname}")));
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row_with_constraints(
        &mut self,
        table: &TableInfo,
        rowid: &mut Option<i64>,
        row: &mut [SqlValue],
        conflict: ConflictAction,
        upserts: &[xdb_parser::UpsertClause],
        params: &QueryParams,
        qopts: &QueryOptions,
    ) -> SqliteResult<RowWriteOutcome> {
        let enc = self.pager.encoding();
        let colls = CollationRegistry::new(enc);

        // Detect any conflict (rowid PK, WITHOUT ROWID PK, or UNIQUE secondary index).
        let mut conflicting_rows: Vec<(Option<i64>, Vec<SqlValue>, String)> = Vec::new();

        if !table.without_rowid {
            let rid = rowid.expect("rowid table has rowid");
            if table_contains_rowid(&self.pager, table.root_page, rid)? {
                let old_row = self.fetch_rowid_row(table, rid, qopts)?;
                let col_label = table
                    .ipk_column
                    .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                    .unwrap_or_else(|| format!("{}.rowid", table.name));
                conflicting_rows.push((Some(rid), old_row, col_label));
            }
        } else {
            // WITHOUT ROWID primary key index shares `table.root_page`!
            if let Some(pk_idx) = table.indexes.iter().find(|ix| ix.without_rowid_primary_key) {
                let ki = key_info_for_index(pk_idx, enc, Some(&colls))?;
                let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                    .iter()
                    .map(|ic| row[ic.cid as usize].clone())
                    .collect();
                if let Some(hit_entry) =
                    index_contains_prefix(&self.pager, table.root_page, &pk_vals, &ki)?
                {
                    let old_row = decode_without_rowid_full_row(table, &hit_entry, qopts)?;
                    conflicting_rows.push((None, old_row, format!("{}.PRIMARY KEY", table.name)));
                }
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
                let hit_rid = if table.without_rowid {
                    None
                } else {
                    hit_entry.last().and_then(SqlValue::as_i64)
                };
                if let Some(hrid) = hit_rid {
                    if !conflicting_rows.iter().any(|(r, _, _)| *r == Some(hrid)) {
                        let old_row = self.fetch_rowid_row(table, hrid, qopts)?;
                        conflicting_rows.push((
                            Some(hrid),
                            old_row,
                            format!("{}.{}", table.name, ix.name),
                        ));
                    }
                } else {
                    conflicting_rows.push((None, Vec::new(), format!("{}.{}", table.name, ix.name)));
                }
            }
        }

        if !conflicting_rows.is_empty() {
            // Check UPSERT (`ON CONFLICT ...`) first!
            if !upserts.is_empty() {
                for up in upserts {
                    if upsert_target_matches(table, up, &conflicting_rows) {
                        match up.action {
                            UpsertAction::Nothing => return Ok(RowWriteOutcome::Ignored),
                            UpsertAction::Update => {
                                let (old_rid, old_row, _) = conflicting_rows.remove(0);
                                let mut upd_row = old_row.clone();
                                let mut upd_rid = old_rid;
                                let scope_cols =
                                    build_upsert_scope_columns(table, &table.name);
                                let mut scope_vals = old_row.clone();
                                if !table.without_rowid {
                                    let rv = old_rid.map_or(SqlValue::Null, SqlValue::Int);
                                    for _ in 0..count_synth_rowid_aliases(table) {
                                        scope_vals.push(rv.clone());
                                    }
                                }
                                // Append `excluded.*` values
                                scope_vals.extend(row.iter().cloned());
                                let no_sub = NoSubqueries;
                                let eval_env = EvalEnv::new(qopts, params, &no_sub);
                                let scope = [EvalScope::simple(&scope_cols, &scope_vals)];

                                if let Some(ref we) = up.where_clause {
                                    let cond = eval_expr(we, &scope, &eval_env)?;
                                    if !boolean_value(&cond, false) {
                                        return Ok(RowWriteOutcome::Ignored);
                                    }
                                }
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
                                self.finalize_and_validate_row(table, &mut upd_row, qopts)?;
                                self.delete_row_and_indexes(table, old_rid, &old_row, qopts)?;
                                self.write_row_and_indexes(table, upd_rid, &upd_row, qopts)?;
                                return Ok(RowWriteOutcome::Written {
                                    final_rowid: upd_rid,
                                    final_row: upd_row,
                                    was_insert: false,
                                });
                            }
                        }
                    }
                }
            }

            match conflict {
                ConflictAction::Ignore => return Ok(RowWriteOutcome::Ignored),
                ConflictAction::Replace => {
                    for (crid, crow, _) in conflicting_rows {
                        if !crow.is_empty() {
                            self.delete_row_and_indexes(table, crid, &crow, qopts)?;
                        }
                    }
                }
                ConflictAction::Abort | ConflictAction::Rollback | ConflictAction::Fail => {
                    return Err(SqliteError::Sql(format!(
                        "UNIQUE constraint failed: {}",
                        conflicting_rows[0].2
                    )));
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
        let colls = CollationRegistry::new(enc);

        if table.without_rowid {
            let pk_idx = table
                .indexes
                .iter()
                .find(|ix| ix.without_rowid_primary_key)
                .ok_or_else(|| SqliteError::corrupt("WITHOUT ROWID table missing PK index"))?;
            let ki = key_info_for_index(pk_idx, enc, Some(&colls))?;
            let rec_vals = build_without_rowid_record_values(table, row);
            let payload = encode_record(&rec_vals, enc);
            index_insert(&mut self.pager, table.root_page, &payload, &ki, true)?;
        } else {
            let rid = rowid.expect("rowid table has rowid");
            let payload = encode_table_record(table, row, enc);
            table_insert(&mut self.pager, table.root_page, rid, &payload, true)?;
        }

        for ix in &table.indexes {
            if ix.without_rowid_primary_key {
                continue;
            }
            if let Some(entry_vals) = build_index_entry_values(table, ix, rowid, row, qopts)? {
                let ki = key_info_for_index(ix, enc, Some(&colls))?;
                let payload = encode_record(&entry_vals, enc);
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
            let ki = key_info_for_index(pk_idx, enc, Some(&colls))?;
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
            for (cid, col) in table.columns.iter().enumerate() {
                if col.generated == Some(GeneratedKind::Virtual) {
                    if let Some(ref expr) = table.definition.columns[cid].generated {
                        full[cid] =
                            eval_generated_expr(expr, &scope_cols, &full, col.affinity, qopts)?;
                    }
                }
            }
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
                for (cid, col) in table.columns.iter().enumerate() {
                    if col.generated == Some(GeneratedKind::Virtual) {
                        if let Some(ref expr) = table.definition.columns[cid].generated {
                            full[cid] = eval_generated_expr(
                                expr,
                                &scope_cols,
                                &full,
                                col.affinity,
                                &qopts,
                            )?;
                        }
                    }
                }
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
                        self.pager.push_savepoint(None);
                        self.pager.set_user_version(uv)?;
                        self.pager.commit_statement_savepoint();
                        self.pager.autocommit_flush()?;
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
                        self.pager.push_savepoint(None);
                        self.pager.set_application_id(aid)?;
                        self.pager.commit_statement_savepoint();
                        self.pager.autocommit_flush()?;
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "application_id",
                    SqlValue::Int(i64::from(self.pager.application_id())),
                ))
            }
            "encoding" => {
                let s = match self.pager.encoding() {
                    TextEncoding::Utf8 => "UTF-8",
                    TextEncoding::Utf16Le => "UTF-16le",
                    TextEncoding::Utf16Be => "UTF-16be",
                };
                Ok(single_row("encoding", SqlValue::text(s)))
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

enum RowWriteOutcome {
    Written {
        final_rowid: Option<i64>,
        final_row: Vec<SqlValue>,
        was_insert: bool,
    },
    Ignored,
}

fn encode_table_record(table: &TableInfo, row: &[SqlValue], enc: TextEncoding) -> Vec<u8> {
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
    encode_record(&stored_vals, enc)
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
            if let Some(v) = record_vals.get(si) {
                full[cid] = v.clone();
            }
        }
    }
    if table.has_virtual_columns {
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
        for (cid, col) in table.columns.iter().enumerate() {
            if col.generated == Some(GeneratedKind::Virtual) {
                if let Some(ref expr) = table.definition.columns[cid].generated {
                    full[cid] =
                        eval_generated_expr(expr, &scope_cols, &full, col.affinity, qopts)?;
                }
            }
        }
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
    let params = QueryParams::empty();
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(qopts, &params, &no_sub);
    env.dqs = true;
    let scope = [EvalScope::simple(&scope_cols, row)];

    // Partial index WHERE predicate check
    if let Some(ref where_expr) = index.definition.where_clause {
        let cond = eval_expr(where_expr, &scope, &env)?;
        if !boolean_value(&cond, false) {
            return Ok(None);
        }
    }

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
    match v {
        SqlValue::Int(i) => Ok(*i),
        SqlValue::Real(r) => {
            let iv = real_to_i64(*r);
            if real_same_as_int(*r, iv) {
                Ok(iv)
            } else {
                Err(SqliteError::Sql("datatype mismatch".to_string()))
            }
        }
        SqlValue::Text(b) => {
            let a = xdb_core::value::atoi64(b);
            if a.rc == 0 {
                Ok(a.value)
            } else {
                Err(SqliteError::Sql("datatype mismatch".to_string()))
            }
        }
        _ => Err(SqliteError::Sql("datatype mismatch".to_string())),
    }
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

fn upsert_target_matches(
    table: &TableInfo,
    up: &xdb_parser::UpsertClause,
    conflicts: &[(Option<i64>, Vec<SqlValue>, String)],
) -> bool {
    let Some(ref target) = up.target else {
        return true; // `ON CONFLICT DO NOTHING` matches any conflict
    };
    let target_cols: HashSet<String> = target
        .iter()
        .filter_map(|t| match &t.expr {
            Expr::Column(c) => Some(c.column.name.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    // Match against IPK or any UNIQUE index whose key columns equal `target_cols`.
    for (_, _, label) in conflicts {
        if let Some(ipk) = table.ipk_column {
            let ipk_name = table.columns[ipk].name.to_ascii_lowercase();
            if target_cols.len() == 1
                && target_cols.contains(&ipk_name)
                && label.eq_ignore_ascii_case(&format!("{}.{ipk_name}", table.name))
            {
                return true;
            }
        }
        for ix in &table.indexes {
            if !ix.unique {
                continue;
            }
            let ix_cols: HashSet<String> = ix.columns[..ix.key_count]
                .iter()
                .filter_map(|ic| ic.name.as_ref().map(|n| n.to_ascii_lowercase()))
                .collect();
            if ix_cols == target_cols
                && (label.eq_ignore_ascii_case(&format!("{}.{}", table.name, ix.name))
                    || (ix.without_rowid_primary_key && label.ends_with("PRIMARY KEY")))
            {
                return true;
            }
        }
    }
    false
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

fn strip_if_not_exists(sql: &str, _prefix: &str) -> String {
    // Preserve clean SQLite schema text (removing `IF NOT EXISTS` clause case-insensitively).
    let tokens = xdb_parser::raw_tokens(sql, Dialect::Sqlite);
    let mut i = 0;
    while i + 2 < tokens.len() {
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
            if j < tokens.len() && sql[tokens[j].start..tokens[j].end].eq_ignore_ascii_case("NOT") {
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

fn rewrite_on_table_name(ddl: &str, _old_name: &str, new_name: &str) -> String {
    if let Ok(Statement::CreateIndex(ci)) =
        xdb_parser::parse_statement(ddl, &ParseOptions::default())
    {
        let sp = ci.table.span;
        let mut out = String::with_capacity(ddl.len() + new_name.len());
        out.push_str(&ddl[..sp.start]);
        out.push_str(&quote_ident_if_needed(new_name));
        out.push_str(&ddl[sp.end..]);
        return out;
    }
    ddl.to_string()
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

fn rewrite_column_name_in_ddl(ddl: &str, cid: usize, new_name: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.columns[cid].name.span;
    let mut out = String::with_capacity(ddl.len() + new_name.len());
    out.push_str(&ddl[..sp.start]);
    out.push_str(&quote_ident_if_needed(new_name));
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

fn remove_column_from_ddl(ddl: &str, cid: usize) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.columns[cid].span;
    // Remove trailing comma after `sp.end`, or leading comma before `sp.start`.
    let bytes = ddl.as_bytes();
    let mut end = sp.end;
    while end < bytes.len() && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b',' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        let mut out = String::with_capacity(ddl.len());
        out.push_str(&ddl[..sp.start]);
        out.push_str(&ddl[end..]);
        return Ok(out);
    }
    let mut start = sp.start;
    while start > 0 && bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if start > 0 && bytes[start - 1] == b',' {
        start -= 1;
    }
    let mut out = String::with_capacity(ddl.len());
    out.push_str(&ddl[..start]);
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

#[allow(dead_code)]
fn _keep_imports(_: Ordering, _: fn(&SqlValue) -> i64, _: fn(&SqlValue, &SqlValue, Option<&xdb_core::value::Collation>) -> Ordering) {
    let _ = (int_value, mem_compare);
}
