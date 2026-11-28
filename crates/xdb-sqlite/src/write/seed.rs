//! Declarative JSON database seeding for [`SqliteConnection`].
//!
//! Accepts flexible JSON seed documents to boot an application database cleanly inside
//! sandboxed/WASM environments (such as SoftN) without hand-writing SQL inserts:
//!
//! 1. **Structured form**:
//!    ```json
//!    {
//!      "mode": "ignore",
//!      "schema": ["CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)"],
//!      "tables": {
//!        "users": [{ "id": 1, "name": "Alice" }]
//!      },
//!      "statements": [
//!        { "sql": "INSERT OR IGNORE INTO meta (k, v) VALUES (?, ?)", "params": ["ver", "1"] }
//!      ]
//!    }
//!    ```
//!    `tables` may also be an array of `{ "name", "schema"?, "columns"?, "rows" }` objects.
//! 2. **Shorthand table map**:
//!    ```json
//!    {
//!      "users": [{ "id": 1, "name": "Alice", "admin": true }],
//!      "posts": [{ "id": 1, "user_id": 1, "title": "Hello" }]
//!    }
//!    ```
//!    Any table missing from `sqlite_schema` is automatically created with column types
//!    inferred from the seed rows.

use std::collections::HashMap;
use std::sync::Arc;

use xdb_core::{Meter, SqlValue, TextEncoding};
use xdb_parser::ConflictAction;
use xdb_sql::functions::json::{parse_json_metered_with, render_json_compact_metered, JsonVal};
use xdb_sql::QueryParams;

use crate::errors::{SqliteError, SqliteResult};
use crate::write::connection::SqliteConnection;
use crate::write::record::BorrowedRecordVal;

/// Conflict resolution strategy used for row inserts during JSON seeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeedInsertMode {
    /// `INSERT OR IGNORE` — idempotent boot seeding (default).
    #[default]
    Ignore,
    /// `INSERT OR REPLACE` — overwrite rows with matching primary/unique keys.
    Replace,
    /// Strict `INSERT` — fail on constraint violation.
    Insert,
}

impl SeedInsertMode {
    fn sql_prefix(self) -> &'static str {
        match self {
            SeedInsertMode::Ignore => "INSERT OR IGNORE INTO ",
            SeedInsertMode::Replace => "INSERT OR REPLACE INTO ",
            SeedInsertMode::Insert => "INSERT INTO ",
        }
    }

    fn conflict_action(self) -> Option<ConflictAction> {
        match self {
            SeedInsertMode::Ignore => Some(ConflictAction::Ignore),
            SeedInsertMode::Replace => Some(ConflictAction::Replace),
            SeedInsertMode::Insert => None,
        }
    }

    fn parse(s: &str) -> SqliteResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ignore" | "insert_or_ignore" => Ok(SeedInsertMode::Ignore),
            "replace" | "upsert" | "insert_or_replace" => Ok(SeedInsertMode::Replace),
            "insert" | "strict" => Ok(SeedInsertMode::Insert),
            other => Err(SqliteError::Misuse(format!(
                "invalid seed mode '{other}' (expected 'ignore', 'replace', or 'insert')"
            ))),
        }
    }
}

/// Summary metrics returned by [`SqliteConnection::seed_json`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub tables_created: usize,
    pub tables_seeded: usize,
    pub rows_inserted: usize,
    pub statements_executed: usize,
}

impl SqliteConnection {
    /// Seed this connection from a JSON seed document inside a transaction.
    ///
    /// Runs atomically (unless a transaction is already active on this connection, in which
    /// case it joins the active transaction). Every DDL/DML operation executes through the
    /// normal metered engine so sandbox resource limits are strictly enforced.
    pub fn seed_json(&mut self, seed_json: &str) -> SqliteResult<SeedReport> {
        self.check_open()?;
        self.check_writable()?;
        let trimmed = seed_json.trim();
        if trimmed.is_empty() {
            return Ok(SeedReport::default());
        }
        if self.encoding() == TextEncoding::Utf8 {
            if let Some(report) = self.try_seed_shorthand_streaming(trimmed)? {
                return Ok(report);
            }
        }
        let meter_arc = self.meter().clone();
        let root = parse_json_metered_with(trimmed, false, Some(meter_arc.as_ref())).map_err(
            |e| match e {
                xdb_sql::SqlError::ResourceLimit(r) => SqliteError::ResourceLimit(r),
                other => SqliteError::Misuse(format!("invalid seed JSON: {other}")),
            },
        )?;
        // `parse_json_metered_with` releases its parser reservation on return; re-reserve the
        // retained `JsonVal` AST for the duration of `apply_seed_entries`!
        let mut ast_mem = SeedMemGuard::new(Arc::clone(&meter_arc));
        ast_mem.reserve(estimate_json_val_bytes(&root))?;
        let entries = match root {
            JsonVal::Object(entries) => entries,
            _ => {
                return Err(SqliteError::Misuse(
                    "seed JSON root must be an object".to_string(),
                ));
            }
        };

        let started_tx = !self.in_transaction();
        if started_tx {
            self.execute("BEGIN")?;
        }
        let result = self.apply_seed_entries(entries, started_tx);
        match result {
            Ok(report) => {
                if started_tx {
                    if let Err(err) = self.execute("COMMIT") {
                        if self.in_transaction() {
                            let _ = self.execute("ROLLBACK");
                        }
                        return Err(err);
                    }
                }
                Ok(report)
            }
            Err(err) => {
                if started_tx && self.in_transaction() {
                    let _ = self.execute("ROLLBACK");
                }
                Err(err)
            }
        }
    }

    fn try_seed_shorthand_streaming(
        &mut self,
        trimmed: &str,
    ) -> SqliteResult<Option<SeedReport>> {
        let meter_arc = self.meter().clone();
        let Some(specs) = scan_shorthand_specs(trimmed, meter_arc.as_ref())? else {
            return Ok(None);
        };
        for spec in &specs {
            if self.table_exists(spec.name)? {
                return Ok(None);
            }
        }

        let started_tx = !self.in_transaction();
        if started_tx {
            self.execute("BEGIN")?;
        }
        let result: SqliteResult<SeedReport> = (|| {
            let mut report = SeedReport::default();
            let bytes = trimmed.as_bytes();
            for spec in &specs {
                let owned_cols: Vec<(String, InferredType)> = spec
                    .cols
                    .iter()
                    .map(|&(k, ty)| (k.to_string(), ty))
                    .collect();
                let ddl = build_create_table_ddl(spec.name, &owned_cols);
                self.execute(&ddl)?;
                report.tables_created += 1;
                report.statements_executed += 1;

                let ncols = spec.cols.len();
                let body_start = spec.array_body_start;
                let body_end = spec.array_body_end;
                let inserted =
                    self.stream_insert_new_seed_table(spec.name, started_tx, |emit| {
                        let mut pos = body_start;
                        let mut row_vals = [BorrowedRecordVal::Null; 16];
                        loop {
                            pos = skip_json_ws(bytes, pos);
                            debug_assert!(pos < body_end && bytes[pos] == b'{');
                            pos += 1;
                            for (ci, slot) in row_vals.iter_mut().enumerate().take(ncols) {
                                pos = skip_json_ws(bytes, pos);
                                // Skip verified unescaped key `"..."`
                                debug_assert!(bytes[pos] == b'"');
                                pos += 1 + spec.cols[ci].0.len() + 1;
                                pos = skip_json_ws(bytes, pos);
                                debug_assert!(bytes[pos] == b':');
                                pos += 1;
                                pos = skip_json_ws(bytes, pos);
                                let (val, _, next_pos) = scan_simple_scalar(trimmed, bytes, pos)
                                    .expect("Pass 1 verified scalar");
                                *slot = val;
                                pos = skip_json_ws(bytes, next_pos);
                                if ci + 1 < ncols {
                                    debug_assert!(bytes[pos] == b',');
                                    pos += 1;
                                }
                            }
                            debug_assert!(bytes[pos] == b'}');
                            pos += 1;
                            emit(&mut row_vals[..ncols])?;
                            pos = skip_json_ws(bytes, pos);
                            if pos >= body_end {
                                break;
                            }
                            debug_assert!(bytes[pos] == b',');
                            pos += 1;
                        }
                        Ok(())
                    })?;
                report.statements_executed += 1;
                report.rows_inserted += inserted;
                report.tables_seeded += 1;
            }
            Ok(report)
        })();

        match result {
            Ok(report) => {
                if started_tx {
                    if let Err(err) = self.execute("COMMIT") {
                        if self.in_transaction() {
                            let _ = self.execute("ROLLBACK");
                        }
                        return Err(err);
                    }
                }
                Ok(Some(report))
            }
            Err(err) => {
                if started_tx && self.in_transaction() {
                    let _ = self.execute("ROLLBACK");
                }
                Err(err)
            }
        }
    }

    fn apply_seed_entries(
        &mut self,
        entries: Vec<(String, JsonVal)>,
        owned_outer_tx: bool,
    ) -> SqliteResult<SeedReport> {
        let mut report = SeedReport::default();
        if !is_structured_seed(&entries) {
            // Shorthand form: { "table_name": [ { col: val }, ... ], ... }
            for (table_name, rows_val) in entries {
                if table_name.starts_with('$') {
                    // Allow JSON metadata fields like "$schema" or "$comment"
                    continue;
                }
                let rows = match rows_val {
                    JsonVal::Array(rows) => rows,
                    JsonVal::Null => continue,
                    _ => {
                        return Err(SqliteError::Misuse(format!(
                            "seed table '{table_name}' must be an array of row objects"
                        )));
                    }
                };
                self.seed_table_rows(
                    &table_name,
                    None,
                    rows,
                    SeedInsertMode::Ignore,
                    owned_outer_tx,
                    &mut report,
                )?;
            }
            return Ok(report);
        }

        let mut mode = SeedInsertMode::Ignore;
        let mut pragmas: Option<JsonVal> = None;
        let mut schema: Option<JsonVal> = None;
        let mut tables: Option<JsonVal> = None;
        let mut statements: Option<JsonVal> = None;

        for (k, v) in entries {
            match k.as_str() {
                "mode" => {
                    if let JsonVal::Str(s) = v {
                        mode = SeedInsertMode::parse(&s)?;
                    } else if !matches!(v, JsonVal::Null) {
                        return Err(SqliteError::Misuse(
                            "seed 'mode' must be a string ('ignore', 'replace', or 'insert')"
                                .to_string(),
                        ));
                    }
                }
                "pragmas" => pragmas = Some(v),
                "schema" => schema = Some(v),
                "tables" => tables = Some(v),
                "statements" => statements = Some(v),
                // Ignore benign metadata fields (version, name, description, $schema, etc.)
                _ => {}
            }
        }

        if let Some(p) = pragmas {
            self.run_seed_sql_list(p, &mut report)?;
        }
        if let Some(s) = schema {
            self.run_seed_sql_list(s, &mut report)?;
        }
        if let Some(t) = tables {
            match t {
                JsonVal::Null => {}
                JsonVal::Object(tbl_entries) => {
                    for (table_name, rows_val) in tbl_entries {
                        match rows_val {
                            JsonVal::Array(rows) => {
                                self.seed_table_rows(
                                    &table_name,
                                    None,
                                    rows,
                                    mode,
                                    owned_outer_tx,
                                    &mut report,
                                )?;
                            }
                            JsonVal::Object(spec_fields) => {
                                self.seed_table_spec(
                                    Some(table_name),
                                    spec_fields,
                                    mode,
                                    owned_outer_tx,
                                    &mut report,
                                )?;
                            }
                            JsonVal::Null => {}
                            _ => {
                                return Err(SqliteError::Misuse(format!(
                                    "seed table '{table_name}' must be an array of rows or a table spec object"
                                )));
                            }
                        }
                    }
                }
                JsonVal::Array(items) => {
                    for item in items {
                        let JsonVal::Object(spec_fields) = item else {
                            return Err(SqliteError::Misuse(
                                "each entry in seed 'tables' array must be an object".to_string(),
                            ));
                        };
                        self.seed_table_spec(
                            None,
                            spec_fields,
                            mode,
                            owned_outer_tx,
                            &mut report,
                        )?;
                    }
                }
                _ => {
                    return Err(SqliteError::Misuse(
                        "seed 'tables' must be an object or array".to_string(),
                    ));
                }
            }
        }
        if let Some(stmts) = statements {
            self.run_seed_sql_list(stmts, &mut report)?;
        }

        Ok(report)
    }

    fn seed_table_spec(
        &mut self,
        default_name: Option<String>,
        fields: Vec<(String, JsonVal)>,
        default_mode: SeedInsertMode,
        owned_outer_tx: bool,
        report: &mut SeedReport,
    ) -> SqliteResult<()> {
        let mut name = default_name;
        let mut schema_val: Option<JsonVal> = None;
        let mut columns: Option<Vec<String>> = None;
        let mut rows: Vec<JsonVal> = Vec::new();
        let mut mode = default_mode;

        for (k, v) in fields {
            match k.as_str() {
                "name" | "table" => {
                    if let JsonVal::Str(s) = v {
                        name = Some(s);
                    }
                }
                "schema" | "ddl" => schema_val = Some(v),
                "mode" => {
                    if let JsonVal::Str(s) = v {
                        mode = SeedInsertMode::parse(&s)?;
                    }
                }
                "columns" => {
                    if let JsonVal::Array(cols) = v {
                        let mut out = Vec::with_capacity(cols.len());
                        for c in cols {
                            let JsonVal::Str(cs) = c else {
                                return Err(SqliteError::Misuse(
                                    "seed table 'columns' entries must be strings".to_string(),
                                ));
                            };
                            out.push(cs);
                        }
                        columns = Some(out);
                    }
                }
                "rows" | "data" => {
                    if let JsonVal::Array(r) = v {
                        rows = r;
                    } else if !matches!(v, JsonVal::Null) {
                        return Err(SqliteError::Misuse(
                            "seed table 'rows' must be an array".to_string(),
                        ));
                    }
                }
                _ => {}
            }
        }

        if let Some(s) = schema_val {
            self.run_seed_sql_list(s, report)?;
        }
        let table_name = name.ok_or_else(|| {
            SqliteError::Misuse("seed table spec is missing 'name'".to_string())
        })?;
        self.seed_table_rows(
            &table_name,
            columns.as_deref(),
            rows,
            mode,
            owned_outer_tx,
            report,
        )
    }

    fn run_seed_sql_list(&mut self, val: JsonVal, report: &mut SeedReport) -> SqliteResult<()> {
        match val {
            JsonVal::Null => Ok(()),
            JsonVal::Str(sql) => {
                if !sql.trim().is_empty() {
                    let before = self.total_changes();
                    let _ = self.execute(&sql)?;
                    report.statements_executed += 1;
                    let delta = self.total_changes().saturating_sub(before);
                    if delta > 0 {
                        report.rows_inserted += delta as usize;
                    }
                }
                Ok(())
            }
            JsonVal::Array(items) => {
                for item in items {
                    match item {
                        JsonVal::Str(sql) => {
                            if !sql.trim().is_empty() {
                                let before = self.total_changes();
                                let _ = self.execute(&sql)?;
                                report.statements_executed += 1;
                                let delta = self.total_changes().saturating_sub(before);
                                if delta > 0 {
                                    report.rows_inserted += delta as usize;
                                }
                            }
                        }
                        JsonVal::Object(fields) => {
                            let mut sql: Option<String> = None;
                            let mut params = QueryParams::empty();
                            for (k, v) in fields {
                                match k.as_str() {
                                    "sql" => {
                                        if let JsonVal::Str(s) = v {
                                            sql = Some(s);
                                        }
                                    }
                                    "params" => {
                                        let m = self.meter().clone();
                                        params = json_val_to_query_params(v, Some(m.as_ref()))?;
                                    }
                                    _ => {}
                                }
                            }
                            let sql = sql.ok_or_else(|| {
                                SqliteError::Misuse(
                                    "seed statement object must have a string 'sql' field"
                                        .to_string(),
                                )
                            })?;
                            if !sql.trim().is_empty() {
                                let before = self.total_changes();
                                let _ = self.execute_with_params(&sql, &params)?;
                                report.statements_executed += 1;
                                let delta = self.total_changes().saturating_sub(before);
                                if delta > 0 {
                                    report.rows_inserted += delta as usize;
                                }
                            }
                        }
                        _ => {
                            return Err(SqliteError::Misuse(
                                "seed SQL list entries must be strings or {sql, params} objects"
                                    .to_string(),
                            ));
                        }
                    }
                }
                Ok(())
            }
            _ => Err(SqliteError::Misuse(
                "seed SQL field must be a string or an array".to_string(),
            )),
        }
    }

    fn table_exists(&mut self, table_name: &str) -> SqliteResult<bool> {
        let cat = self.schema_arc()?;
        Ok(cat.table(table_name).is_some()
            || cat.view(table_name).is_some()
            || cat.virtual_table(table_name).is_some())
    }

    fn seed_table_rows(
        &mut self,
        table_name: &str,
        explicit_columns: Option<&[String]>,
        mut rows: Vec<JsonVal>,
        mode: SeedInsertMode,
        owned_outer_tx: bool,
        report: &mut SeedReport,
    ) -> SqliteResult<()> {
        if table_name.trim().is_empty() {
            return Err(SqliteError::Misuse("seed table name cannot be empty".to_string()));
        }

        // Auto-create table if it does not yet exist and we have columns or row objects.
        if !self.table_exists(table_name)? {
            let inferred_cols = infer_columns_from_rows(explicit_columns, &rows)?;
            if !inferred_cols.is_empty() {
                let ddl = build_create_table_ddl(table_name, &inferred_cols);
                self.execute(&ddl)?;
                report.tables_created += 1;
                report.statements_executed += 1;
            }
        }

        if rows.is_empty() {
            report.tables_seeded += 1;
            return Ok(());
        }

        let quoted_table = quote_ident(table_name);
        let prefix = mode.sql_prefix();
        const MAX_BATCH_ROWS: usize = 100;
        const MAX_BATCH_PARAMS: usize = 900;

        let mut i = 0usize;
        while i < rows.len() {
            let is_obj = matches!(rows[i], JsonVal::Object(_));
            let is_arr = matches!(rows[i], JsonVal::Array(_));
            if is_obj {
                let ncols = match &rows[i] {
                    JsonVal::Object(f) => f.len(),
                    _ => unreachable!(),
                };
                if ncols == 0 {
                    let sql = format!("{prefix}{quoted_table} DEFAULT VALUES");
                    let res = self.execute(&sql)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                    i += 1;
                    continue;
                }
                let mut batch_end = i + 1;
                {
                    let JsonVal::Object(ref fields) = rows[i] else {
                        unreachable!();
                    };
                    while batch_end < rows.len() {
                        let JsonVal::Object(ref next_fields) = rows[batch_end] else {
                            break;
                        };
                        if next_fields.len() != ncols
                            || !next_fields
                                .iter()
                                .zip(fields.iter())
                                .all(|((a, _), (b, _))| a == b)
                        {
                            break;
                        }
                        batch_end += 1;
                    }
                }

                // Direct insertion path: moves JSON strings straight into `self.row_scratch` on
                // the fly with zero intermediate `Vec<Vec<SqlValue>>` and avoids SQL re-parsing!
                let col_names_owned: Vec<String> = match &rows[i] {
                    JsonVal::Object(f) => f.iter().map(|(c, _)| c.clone()).collect(),
                    _ => unreachable!(),
                };
                let col_names: Vec<&str> = col_names_owned.iter().map(String::as_str).collect();
                if let Some(inserted) = self.try_insert_seed_json_batch_direct(
                    table_name,
                    Some(&col_names),
                    &mut rows[i..batch_end],
                    true,
                    mode.conflict_action(),
                    owned_outer_tx,
                )? {
                    report.statements_executed += 1;
                    report.rows_inserted += inserted;
                    i = batch_end;
                    continue;
                }

                // Fallback SQL path for exotic tables (GENERATED/CHECK/STRICT/WITHOUT ROWID):
                let meter_arc = self.meter().clone();
                let mut batch_mem = SeedMemGuard::new(Arc::clone(&meter_arc));
                let mut batch_rows = Vec::with_capacity(batch_end - i);
                for row_item in &mut rows[i..batch_end] {
                    if let JsonVal::Object(f) = std::mem::replace(row_item, JsonVal::Null) {
                        let mut r = Vec::with_capacity(ncols);
                        let mut row_bytes = 24u64.saturating_add((ncols as u64).saturating_mul(32));
                        for (_, val) in f {
                            let sv = seed_json_to_sql_value_owned_metered(
                                val,
                                Some(meter_arc.as_ref()),
                            )?;
                            row_bytes = row_bytes.saturating_add(sql_val_heap_bytes(&sv));
                            r.push(sv);
                        }
                        batch_mem.reserve(row_bytes)?;
                        batch_rows.push(r);
                    }
                }
                let max_rows = (MAX_BATCH_PARAMS / ncols.max(1)).clamp(1, MAX_BATCH_ROWS);
                let mut sub_start = 0usize;
                while sub_start < batch_rows.len() {
                    let sub_end = (sub_start + max_rows).min(batch_rows.len());
                    let sub_len = sub_end - sub_start;
                    let mut cols_sql = String::new();
                    let mut row_ph = String::with_capacity(ncols * 3);
                    for (ci, col) in col_names_owned.iter().enumerate() {
                        if ci > 0 {
                            cols_sql.push_str(", ");
                            row_ph.push_str(", ");
                        }
                        cols_sql.push_str(&quote_ident(col));
                        row_ph.push('?');
                    }
                    let mut sql = String::with_capacity(
                        prefix.len()
                            + quoted_table.len()
                            + cols_sql.len()
                            + sub_len * (row_ph.len() + 4)
                            + 16,
                    );
                    sql.push_str(prefix);
                    sql.push_str(&quoted_table);
                    sql.push_str(" (");
                    sql.push_str(&cols_sql);
                    sql.push_str(") VALUES ");
                    let mut values = Vec::with_capacity(sub_len * ncols);
                    for (bi, r) in batch_rows[sub_start..sub_end].iter_mut().enumerate() {
                        if bi > 0 {
                            sql.push_str(", ");
                        }
                        sql.push('(');
                        sql.push_str(&row_ph);
                        sql.push(')');
                        values.append(r);
                    }
                    let params = QueryParams::positional(values);
                    let res = self.execute_with_params(&sql, &params)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                    sub_start = sub_end;
                }
                i = batch_end;
            } else if is_arr {
                let ncols = match &rows[i] {
                    JsonVal::Array(c) => c.len(),
                    _ => unreachable!(),
                };
                if let Some(cols) = explicit_columns {
                    if cols.len() != ncols {
                        return Err(SqliteError::Misuse(format!(
                            "seed row for '{table_name}' has {} values but {} columns were declared",
                            ncols,
                            cols.len()
                        )));
                    }
                }
                if ncols == 0 {
                    let sql = format!("{prefix}{quoted_table} DEFAULT VALUES");
                    let res = self.execute(&sql)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                    i += 1;
                    continue;
                }
                let mut batch_end = i + 1;
                while batch_end < rows.len() {
                    let JsonVal::Array(ref next_cells) = rows[batch_end] else {
                        break;
                    };
                    if next_cells.len() != ncols {
                        break;
                    }
                    batch_end += 1;
                }
                let col_refs: Option<Vec<&str>> =
                    explicit_columns.map(|cs| cs.iter().map(String::as_str).collect());
                if let Some(inserted) = self.try_insert_seed_json_batch_direct(
                    table_name,
                    col_refs.as_deref(),
                    &mut rows[i..batch_end],
                    false,
                    mode.conflict_action(),
                    owned_outer_tx,
                )? {
                    report.statements_executed += 1;
                    report.rows_inserted += inserted;
                    i = batch_end;
                    continue;
                }
                let meter_arc = self.meter().clone();
                let mut batch_mem = SeedMemGuard::new(Arc::clone(&meter_arc));
                let mut batch_rows = Vec::with_capacity(batch_end - i);
                for row_item in &mut rows[i..batch_end] {
                    if let JsonVal::Array(cells) = std::mem::replace(row_item, JsonVal::Null) {
                        let mut r = Vec::with_capacity(ncols);
                        let mut row_bytes = 24u64.saturating_add((ncols as u64).saturating_mul(32));
                        for cell in cells {
                            let sv = seed_json_to_sql_value_owned_metered(
                                cell,
                                Some(meter_arc.as_ref()),
                            )?;
                            row_bytes = row_bytes.saturating_add(sql_val_heap_bytes(&sv));
                            r.push(sv);
                        }
                        batch_mem.reserve(row_bytes)?;
                        batch_rows.push(r);
                    }
                }
                let max_rows = (MAX_BATCH_PARAMS / ncols.max(1)).clamp(1, MAX_BATCH_ROWS);
                let mut sub_start = 0usize;
                while sub_start < batch_rows.len() {
                    let sub_end = (sub_start + max_rows).min(batch_rows.len());
                    let sub_len = sub_end - sub_start;
                    let mut row_ph = String::with_capacity(ncols * 3);
                    for ci in 0..ncols {
                        if ci > 0 {
                            row_ph.push_str(", ");
                        }
                        row_ph.push('?');
                    }
                    let mut sql = String::new();
                    sql.push_str(prefix);
                    sql.push_str(&quoted_table);
                    if let Some(cols) = explicit_columns {
                        sql.push_str(" (");
                        for (ci, c) in cols.iter().enumerate() {
                            if ci > 0 {
                                sql.push_str(", ");
                            }
                            sql.push_str(&quote_ident(c));
                        }
                        sql.push(')');
                    }
                    sql.push_str(" VALUES ");
                    let mut values = Vec::with_capacity(sub_len * ncols);
                    for (bi, r) in batch_rows[sub_start..sub_end].iter_mut().enumerate() {
                        if bi > 0 {
                            sql.push_str(", ");
                        }
                        sql.push('(');
                        sql.push_str(&row_ph);
                        sql.push(')');
                        values.append(r);
                    }
                    let params = QueryParams::positional(values);
                    let res = self.execute_with_params(&sql, &params)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                    sub_start = sub_end;
                }
                i = batch_end;
            } else {
                return Err(SqliteError::Misuse(format!(
                    "seed rows for '{table_name}' must be objects or arrays"
                )));
            }
        }

        report.tables_seeded += 1;
        Ok(())
    }
}

struct StreamTableSpec<'a> {
    name: &'a str,
    cols: Vec<(&'a str, InferredType)>,
    array_body_start: usize,
    array_body_end: usize,
}

#[inline]
fn skip_json_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b'\n' | b'\r') {
        pos += 1;
    }
    pos
}

#[inline]
fn scan_simple_str<'a>(src: &'a str, bytes: &[u8], pos: usize) -> Option<(&'a str, usize)> {
    if pos >= bytes.len() || bytes[pos] != b'"' {
        return None;
    }
    let start = pos + 1;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Some((&src[start..i], i + 1));
        }
        if b == b'\\' || b < 0x20 {
            return None;
        }
        i += 1;
    }
    None
}

#[inline]
fn scan_simple_scalar<'a>(
    src: &'a str,
    bytes: &[u8],
    pos: usize,
) -> Option<(BorrowedRecordVal<'a>, InferredType, usize)> {
    let b = *bytes.get(pos)?;
    match b {
        b'"' => {
            let (s, next) = scan_simple_str(src, bytes, pos)?;
            Some((BorrowedRecordVal::Text(s.as_bytes()), InferredType::Text, next))
        }
        b'n' => bytes[pos..]
            .starts_with(b"null")
            .then_some((BorrowedRecordVal::Null, InferredType::Unknown, pos + 4)),
        b't' => bytes[pos..]
            .starts_with(b"true")
            .then_some((BorrowedRecordVal::Int(1), InferredType::Integer, pos + 4)),
        b'f' => bytes[pos..]
            .starts_with(b"false")
            .then_some((BorrowedRecordVal::Int(0), InferredType::Integer, pos + 5)),
        b'-' | b'0'..=b'9' => {
            let start = pos;
            let mut i = pos;
            let neg = if bytes[i] == b'-' {
                i += 1;
                true
            } else {
                false
            };
            let int_start = i;
            let first_d = *bytes.get(i)?;
            if !first_d.is_ascii_digit() {
                return None;
            }
            if first_d == b'0' {
                if neg {
                    // Defer `-0` / `-0.x` to full parser for exact sign parity.
                    return None;
                }
                i += 1;
                if bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
            } else {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let mut is_float = false;
            if bytes.get(i) == Some(&b'.') {
                is_float = true;
                i += 1;
                let frac_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == frac_start {
                    return None;
                }
            }
            if matches!(bytes.get(i), Some(b'e' | b'E')) {
                is_float = true;
                i += 1;
                if matches!(bytes.get(i), Some(b'+' | b'-')) {
                    i += 1;
                }
                let exp_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == exp_start {
                    return None;
                }
            }
            if is_float {
                let f = src[start..i].parse::<f64>().ok()?;
                if !f.is_finite() {
                    return None;
                }
                Some((BorrowedRecordVal::Real(f), InferredType::Real, i))
            } else {
                let mut acc = 0i64;
                if neg {
                    for &d in &bytes[int_start..i] {
                        acc = acc.checked_mul(10)?.checked_sub(i64::from(d - b'0'))?;
                    }
                } else {
                    for &d in &bytes[int_start..i] {
                        acc = acc.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
                    }
                }
                Some((BorrowedRecordVal::Int(acc), InferredType::Integer, i))
            }
        }
        _ => None,
    }
}

fn scan_shorthand_specs<'a>(
    src: &'a str,
    meter: &dyn xdb_core::Meter,
) -> SqliteResult<Option<Vec<StreamTableSpec<'a>>>> {
    let bytes = src.as_bytes();
    let metered = !meter.is_noop();
    if metered {
        meter.charge_steps((bytes.len() / 64).max(1) as u64)?;
    }
    let mut pos = skip_json_ws(bytes, 0);
    if pos >= bytes.len() || bytes[pos] != b'{' {
        return Ok(None);
    }
    pos = skip_json_ws(bytes, pos + 1);
    if pos < bytes.len() && bytes[pos] == b'}' {
        return Ok(None);
    }

    let mut specs: Vec<StreamTableSpec<'a>> = Vec::new();
    let mut rows_scanned = 0u64;
    loop {
        let Some((tname, next)) = scan_simple_str(src, bytes, pos) else {
            return Ok(None);
        };
        if tname.trim().is_empty()
            || tname.starts_with('$')
            || matches!(
                tname,
                "mode" | "schema" | "tables" | "statements" | "pragmas"
            )
            || (tname.len() >= 7 && tname.as_bytes()[..7].eq_ignore_ascii_case(b"sqlite_"))
            || specs.iter().any(|s| s.name.eq_ignore_ascii_case(tname))
        {
            return Ok(None);
        }
        pos = skip_json_ws(bytes, next);
        if bytes.get(pos) != Some(&b':') {
            return Ok(None);
        }
        pos = skip_json_ws(bytes, pos + 1);
        if bytes.get(pos) != Some(&b'[') {
            return Ok(None);
        }
        let array_body_start = skip_json_ws(bytes, pos + 1);
        pos = array_body_start;
        if bytes.get(pos) != Some(&b'{') {
            // Empty array or array of non-objects -> defer to full parser.
            return Ok(None);
        }

        let mut cols: Vec<(&'a str, InferredType)> = Vec::new();
        let mut is_first_row = true;
        let array_body_end;
        loop {
            if metered {
                rows_scanned += 1;
                if (rows_scanned & 31) == 0 {
                    meter.charge_steps(1)?;
                }
            }
            if bytes.get(pos) != Some(&b'{') {
                return Ok(None);
            }
            pos = skip_json_ws(bytes, pos + 1);
            if bytes.get(pos) == Some(&b'}') {
                return Ok(None);
            }
            if is_first_row {
                loop {
                    let Some((k, next_k)) = scan_simple_str(src, bytes, pos) else {
                        return Ok(None);
                    };
                    if k.is_empty()
                        || cols.len() >= 16
                        || cols.iter().any(|&(ec, _)| ec.eq_ignore_ascii_case(k))
                    {
                        return Ok(None);
                    }
                    pos = skip_json_ws(bytes, next_k);
                    if bytes.get(pos) != Some(&b':') {
                        return Ok(None);
                    }
                    pos = skip_json_ws(bytes, pos + 1);
                    let Some((_, ty, next_v)) = scan_simple_scalar(src, bytes, pos) else {
                        return Ok(None);
                    };
                    cols.push((k, ty));
                    pos = skip_json_ws(bytes, next_v);
                    match bytes.get(pos) {
                        Some(&b',') => {
                            pos = skip_json_ws(bytes, pos + 1);
                        }
                        Some(&b'}') => {
                            pos += 1;
                            break;
                        }
                        _ => return Ok(None),
                    }
                }
                is_first_row = false;
            } else {
                let ncols = cols.len();
                for (ci, (col_name, col_ty)) in cols.iter_mut().enumerate() {
                    let Some((k, next_k)) = scan_simple_str(src, bytes, pos) else {
                        return Ok(None);
                    };
                    if k != *col_name {
                        return Ok(None);
                    }
                    pos = skip_json_ws(bytes, next_k);
                    if bytes.get(pos) != Some(&b':') {
                        return Ok(None);
                    }
                    pos = skip_json_ws(bytes, pos + 1);
                    let Some((_, ty, next_v)) = scan_simple_scalar(src, bytes, pos) else {
                        return Ok(None);
                    };
                    let prev = *col_ty;
                    let merged = prev.merge(ty);
                    // Defer mixed numeric+text columns so full affinity formatting matches exactly.
                    if merged == InferredType::Text
                        && ((prev != InferredType::Unknown && prev != InferredType::Text)
                            || (ty != InferredType::Unknown && ty != InferredType::Text))
                    {
                        return Ok(None);
                    }
                    *col_ty = merged;
                    pos = skip_json_ws(bytes, next_v);
                    if ci + 1 < ncols {
                        if bytes.get(pos) != Some(&b',') {
                            return Ok(None);
                        }
                        pos = skip_json_ws(bytes, pos + 1);
                    }
                }
                if bytes.get(pos) != Some(&b'}') {
                    return Ok(None);
                }
                pos += 1;
            }

            let after_obj = pos;
            pos = skip_json_ws(bytes, pos);
            match bytes.get(pos) {
                Some(&b',') => {
                    pos = skip_json_ws(bytes, pos + 1);
                }
                Some(&b']') => {
                    array_body_end = after_obj;
                    pos += 1;
                    break;
                }
                _ => return Ok(None),
            }
        }

        specs.push(StreamTableSpec {
            name: tname,
            cols,
            array_body_start,
            array_body_end,
        });

        pos = skip_json_ws(bytes, pos);
        match bytes.get(pos) {
            Some(&b',') => {
                pos = skip_json_ws(bytes, pos + 1);
            }
            Some(&b'}') => {
                pos = skip_json_ws(bytes, pos + 1);
                break;
            }
            _ => return Ok(None),
        }
    }

    if pos != bytes.len() || specs.is_empty() {
        return Ok(None);
    }
    Ok(Some(specs))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferredType {
    Unknown,
    Integer,
    Real,
    Blob,
    Text,
}

impl InferredType {
    fn merge(self, next: InferredType) -> InferredType {
        match (self, next) {
            (InferredType::Unknown, x) | (x, InferredType::Unknown) => x,
            (a, b) if a == b => a,
            (InferredType::Integer, InferredType::Real)
            | (InferredType::Real, InferredType::Integer) => InferredType::Real,
            _ => InferredType::Text,
        }
    }

    fn sql_name(self) -> &'static str {
        match self {
            InferredType::Integer => "INTEGER",
            InferredType::Real => "REAL",
            InferredType::Blob => "BLOB",
            InferredType::Text | InferredType::Unknown => "TEXT",
        }
    }
}

fn is_valid_blob_object(entries: &[(String, JsonVal)]) -> bool {
    if entries.len() != 1 {
        return false;
    }
    if entries[0].0 == "__blob" {
        if let JsonVal::Array(items) = &entries[0].1 {
            return items
                .iter()
                .all(|it| matches!(it, JsonVal::Int(n) if (0..=255).contains(n)));
        }
    } else if entries[0].0 == "hex" {
        if let JsonVal::Str(hex) = &entries[0].1 {
            let raw = hex.as_bytes();
            return raw.len().is_multiple_of(2)
                && raw.iter().all(|b| b.is_ascii_hexdigit());
        }
    }
    false
}

fn infer_value_type(v: &JsonVal) -> InferredType {
    match v {
        JsonVal::Null => InferredType::Unknown,
        JsonVal::Bool(_) | JsonVal::Int(_) => InferredType::Integer,
        JsonVal::Real(_) => InferredType::Real,
        JsonVal::Str(_) | JsonVal::Array(_) => InferredType::Text,
        JsonVal::Object(entries) if is_valid_blob_object(entries) => InferredType::Blob,
        JsonVal::Object(_) => InferredType::Text,
    }
}

fn is_structured_seed(entries: &[(String, JsonVal)]) -> bool {
    let mut has_structured_key = false;
    for (k, v) in entries {
        match k.as_str() {
            "mode" => {
                if matches!(v, JsonVal::Array(_)) {
                    return false;
                }
                has_structured_key = true;
            }
            "tables" => {
                if let JsonVal::Array(items) = v {
                    // A structured `tables` array holds table-spec objects (`{name/table, rows/data/schema/ddl/columns}`).
                    let looks_like_specs = !items.is_empty()
                        && items.iter().all(|it| match it {
                            JsonVal::Object(fs) => {
                                let has_name =
                                    fs.iter().any(|(fk, _)| matches!(fk.as_str(), "name" | "table"));
                                let has_spec = fs.iter().any(|(fk, _)| {
                                    matches!(
                                        fk.as_str(),
                                        "rows" | "data" | "schema" | "ddl" | "columns" | "mode"
                                    )
                                });
                                has_name && has_spec
                            }
                            _ => false,
                        });
                    if !looks_like_specs {
                        return false;
                    }
                }
                has_structured_key = true;
            }
            "schema" | "pragmas" | "statements" => {
                if let JsonVal::Array(items) = v {
                    let looks_like_sql = items.iter().all(|it| match it {
                        JsonVal::Str(_) => true,
                        JsonVal::Object(fs) => fs.iter().any(|(fk, _)| fk == "sql"),
                        _ => false,
                    });
                    if !looks_like_sql {
                        return false;
                    }
                }
                has_structured_key = true;
            }
            other if !other.starts_with('$') => {
                // If any sibling key holds an array (table rows), treat the document as a
                // shorthand table map rather than silently ignoring user tables!
                if matches!(v, JsonVal::Array(_)) {
                    return false;
                }
            }
            _ => {}
        }
    }
    has_structured_key
}

fn infer_columns_from_rows(
    explicit_columns: Option<&[String]>,
    rows: &[JsonVal],
) -> SqliteResult<Vec<(String, InferredType)>> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut types: Vec<InferredType> = Vec::new();

    if let Some(cols) = explicit_columns {
        for c in cols {
            let key = c.to_ascii_lowercase();
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key) {
                let idx = order.len();
                e.insert(idx);
                order.push(c.clone());
                types.push(InferredType::Unknown);
            }
        }
    }

    for row in rows {
        match row {
            JsonVal::Object(fields) => {
                for (fi, (k, v)) in fields.iter().enumerate() {
                    let t = infer_value_type(v);
                    if let Some(existing) = order.get(fi) {
                        if existing.eq_ignore_ascii_case(k) {
                            types[fi] = types[fi].merge(t);
                            continue;
                        }
                    }
                    let key = k.to_ascii_lowercase();
                    if let Some(&idx) = seen.get(&key) {
                        types[idx] = types[idx].merge(t);
                    } else {
                        let idx = order.len();
                        seen.insert(key, idx);
                        order.push(k.clone());
                        types.push(t);
                    }
                }
            }
            JsonVal::Array(cells) => {
                if explicit_columns.is_some() {
                    for (i, cell) in cells.iter().enumerate() {
                        if i < types.len() {
                            types[i] = types[i].merge(infer_value_type(cell));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(order.into_iter().zip(types).collect())
}

fn build_create_table_ddl(table_name: &str, cols: &[(String, InferredType)]) -> String {
    let mut out = format!("CREATE TABLE IF NOT EXISTS {} (", quote_ident(table_name));
    for (i, (name, ty)) in cols.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_ident(name));
        out.push(' ');
        if name.eq_ignore_ascii_case("id") && matches!(ty, InferredType::Integer | InferredType::Unknown) {
            out.push_str("INTEGER PRIMARY KEY");
        } else {
            out.push_str(ty.sql_name());
        }
    }
    out.push(')');
    out
}

fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

struct SeedMemGuard {
    meter: Arc<dyn Meter>,
    reserved: u64,
}

impl SeedMemGuard {
    fn new(meter: Arc<dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    fn reserve(&mut self, bytes: u64) -> SqliteResult<()> {
        if bytes > 0 {
            self.meter.reserve_memory(bytes)?;
            self.reserved = self.reserved.saturating_add(bytes);
        }
        Ok(())
    }
}

impl Drop for SeedMemGuard {
    fn drop(&mut self) {
        if self.reserved > 0 {
            self.meter.release_memory(self.reserved);
        }
    }
}

#[inline]
fn sql_val_heap_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Text(b) | SqlValue::Blob(b) => b.len() as u64,
        _ => 0,
    }
}

fn estimate_json_val_bytes(v: &JsonVal) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![v];
    while let Some(cur) = stack.pop() {
        total = total.saturating_add(32);
        match cur {
            JsonVal::Str(s) => total = total.saturating_add(s.len() as u64),
            JsonVal::Array(items) => {
                total = total.saturating_add((items.len() as u64).saturating_mul(32));
                for it in items {
                    stack.push(it);
                }
            }
            JsonVal::Object(entries) => {
                total = total.saturating_add((entries.len() as u64).saturating_mul(56));
                for (k, val) in entries {
                    total = total.saturating_add(k.len() as u64);
                    stack.push(val);
                }
            }
            _ => {}
        }
    }
    total
}

fn json_val_to_query_params(
    val: JsonVal,
    meter: Option<&dyn Meter>,
) -> SqliteResult<QueryParams> {
    match val {
        JsonVal::Null => Ok(QueryParams::empty()),
        JsonVal::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                out.push(seed_json_to_sql_value_metered(item, meter)?);
            }
            Ok(QueryParams::positional(out))
        }
        JsonVal::Object(entries) => {
            let mut p = QueryParams::empty();
            for (k, v) in entries {
                let sv = seed_json_to_sql_value_metered(&v, meter)?;
                p = p.with_named(k, sv);
            }
            Ok(p)
        }
        _ => Err(SqliteError::Misuse(
            "seed statement 'params' must be an array or object".to_string(),
        )),
    }
}

pub(crate) fn seed_json_to_sql_value_owned_metered(
    v: JsonVal,
    meter: Option<&dyn Meter>,
) -> SqliteResult<SqlValue> {
    match v {
        JsonVal::Null => Ok(SqlValue::Null),
        JsonVal::Bool(b) => Ok(SqlValue::Int(i64::from(b))),
        JsonVal::Int(i) => Ok(SqlValue::Int(i)),
        JsonVal::Real(r) => Ok(SqlValue::real(r)),
        JsonVal::Str(s) => Ok(SqlValue::Text(s.into_bytes().into_boxed_slice())),
        other => seed_json_to_sql_value_metered(&other, meter),
    }
}

pub(crate) fn seed_json_to_sql_value_metered(
    v: &JsonVal,
    meter: Option<&dyn Meter>,
) -> SqliteResult<SqlValue> {
    match v {
        JsonVal::Null => Ok(SqlValue::Null),
        JsonVal::Bool(b) => Ok(SqlValue::Int(i64::from(*b))),
        JsonVal::Int(i) => Ok(SqlValue::Int(*i)),
        JsonVal::Real(r) => Ok(SqlValue::real(*r)),
        JsonVal::Str(s) => Ok(SqlValue::text(s.as_str())),
        JsonVal::Object(entries) if entries.len() == 1 => {
            if entries[0].0 == "__blob" {
                if let JsonVal::Array(items) = &entries[0].1 {
                    if items
                        .iter()
                        .all(|it| matches!(it, JsonVal::Int(n) if (0..=255).contains(n)))
                    {
                        if let Some(m) = meter {
                            m.charge_steps((items.len() as u64 / 16).max(1))?;
                            m.reserve_memory(items.len() as u64)?;
                            m.release_memory(items.len() as u64);
                        }
                        let mut bytes = Vec::with_capacity(items.len());
                        for item in items {
                            if let JsonVal::Int(n) = item {
                                bytes.push(*n as u8);
                            }
                        }
                        return Ok(SqlValue::Blob(bytes.into_boxed_slice()));
                    }
                }
            } else if entries[0].0 == "hex" {
                if let JsonVal::Str(hex) = &entries[0].1 {
                    let out_len = hex.len() / 2;
                    if let Some(m) = meter {
                        m.charge_steps((out_len as u64 / 16).max(1))?;
                        m.reserve_memory(out_len as u64)?;
                        m.release_memory(out_len as u64);
                    }
                    if let Some(bytes) = decode_hex(hex) {
                        return Ok(SqlValue::Blob(bytes.into_boxed_slice()));
                    }
                }
            }
            let rendered = render_json_compact_metered(v, meter).map_err(|e| match e {
                xdb_sql::SqlError::ResourceLimit(r) => SqliteError::ResourceLimit(r),
                other => SqliteError::Sql(other.to_string()),
            })?;
            Ok(SqlValue::text(rendered))
        }
        other => {
            let rendered = render_json_compact_metered(other, meter).map_err(|e| match e {
                xdb_sql::SqlError::ResourceLimit(r) => SqliteError::ResourceLimit(r),
                other => SqliteError::Sql(other.to_string()),
            })?;
            Ok(SqlValue::text(rendered))
        }
    }
}

/// Convert a parsed [`JsonVal`] into a [`SqlValue`] (supporting `{ "__blob": [0..255, ...] }` and
/// `{ "hex": "..." }` blob encodings, booleans as 0/1, and nested objects/arrays as JSON text).
pub fn seed_json_to_sql_value(v: &JsonVal) -> SqlValue {
    match v {
        JsonVal::Null => SqlValue::Null,
        JsonVal::Bool(b) => SqlValue::Int(i64::from(*b)),
        JsonVal::Int(i) => SqlValue::Int(*i),
        JsonVal::Real(r) => SqlValue::real(*r),
        JsonVal::Str(s) => SqlValue::text(s.as_str()),
        JsonVal::Object(entries) if entries.len() == 1 => {
            if entries[0].0 == "__blob" {
                if let JsonVal::Array(items) = &entries[0].1 {
                    if items
                        .iter()
                        .all(|it| matches!(it, JsonVal::Int(n) if (0..=255).contains(n)))
                    {
                        let mut bytes = Vec::with_capacity(items.len());
                        for item in items {
                            if let JsonVal::Int(n) = item {
                                bytes.push(*n as u8);
                            }
                        }
                        return SqlValue::Blob(bytes.into_boxed_slice());
                    }
                }
            } else if entries[0].0 == "hex" {
                if let JsonVal::Str(hex) = &entries[0].1 {
                    if let Some(bytes) = decode_hex(hex) {
                        return SqlValue::Blob(bytes.into_boxed_slice());
                    }
                }
            }
            v.to_sql_json()
        }
        other => other.to_sql_json(),
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let raw = s.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        let hi = match pair[0] {
            b'0'..=b'9' => pair[0] - b'0',
            b'a'..=b'f' => pair[0] - b'a' + 10,
            b'A'..=b'F' => pair[0] - b'A' + 10,
            _ => return None,
        };
        let lo = match pair[1] {
            b'0'..=b'9' => pair[1] - b'0',
            b'a'..=b'f' => pair[1] - b'a' + 10,
            b'A'..=b'F' => pair[1] - b'A' + 10,
            _ => return None,
        };
        out.push((hi << 4) | lo);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::ConnectionOptions;

    #[test]
    fn seed_json_structured_schema_tables_and_statements() {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        let seed = r#"{
            "mode": "ignore",
            "schema": [
                "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT, meta TEXT, avatar BLOB)",
                "CREATE TABLE IF NOT EXISTS settings (k TEXT PRIMARY KEY, v TEXT)"
            ],
            "tables": {
                "users": [
                    { "id": 1, "email": "alice@example.com", "name": "Alice", "meta": { "role": "admin" }, "avatar": { "hex": "deadbeef" } },
                    { "id": 2, "email": "bob@example.com", "name": "Bob", "meta": ["a", "b"], "avatar": { "__blob": [1, 2, 3] } }
                ]
            },
            "statements": [
                { "sql": "INSERT OR IGNORE INTO settings (k, v) VALUES (?, ?)", "params": ["site_name", "SoftN Demo"] }
            ]
        }"#;
        let report = conn.seed_json(seed).unwrap();
        assert_eq!(report.tables_seeded, 1);
        assert_eq!(report.rows_inserted, 3);

        // Re-running is idempotent in "ignore" mode
        let report2 = conn.seed_json(seed).unwrap();
        assert_eq!(report2.rows_inserted, 0);

        let rows = conn
            .execute("SELECT id, email, name, meta, hex(avatar) FROM users ORDER BY id")
            .unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.rows[0][1].as_str(), Some("alice@example.com"));
        assert_eq!(rows.rows[0][3].as_str(), Some("{\"role\":\"admin\"}"));
        assert_eq!(rows.rows[0][4].as_str(), Some("DEADBEEF"));
        assert_eq!(rows.rows[1][4].as_str(), Some("010203"));
    }

    #[test]
    fn seed_json_shorthand_auto_creates_tables() {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        let seed = r#"{
            "posts": [
                { "id": 1, "title": "Hello SoftN", "likes": 42, "published": true },
                { "id": 2, "title": "Second Post", "likes": 7, "published": false, "rating": 4.5 }
            ],
            "tags": [
                { "id": 1, "slug": "wasm" }
            ]
        }"#;
        let report = conn.seed_json(seed).unwrap();
        assert_eq!(report.tables_created, 2);
        assert_eq!(report.tables_seeded, 2);
        assert_eq!(report.rows_inserted, 3);

        let res = conn
            .execute("SELECT id, title, likes, published, rating FROM posts ORDER BY id")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][1].as_str(), Some("Hello SoftN"));
        assert!(matches!(res.rows[0][3], SqlValue::Int(1)));
        assert!(matches!(res.rows[1][4], SqlValue::Real(r) if (r - 4.5).abs() < 1e-9));
    }

    #[test]
    fn seed_json_rolls_back_on_error() {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        let bad_seed = r#"{
            "mode": "insert",
            "schema": ["CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)"],
            "tables": {
                "items": [
                    { "id": 1, "name": "ok" },
                    { "id": 1, "name": "dup_pk" }
                ]
            }
        }"#;
        assert!(conn.seed_json(bad_seed).is_err());
        assert!(!conn.in_transaction());
        // Table creation itself was rolled back cleanly
        let check = conn
            .execute("SELECT name FROM sqlite_master WHERE type='table' AND name='items'")
            .unwrap();
        assert!(check.rows.is_empty());
    }
}
