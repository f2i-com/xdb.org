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

use std::collections::HashSet;

use xdb_core::SqlValue;
use xdb_sql::functions::json::{parse_json, JsonVal};
use xdb_sql::QueryParams;

use crate::errors::{SqliteError, SqliteResult};
use crate::write::connection::SqliteConnection;

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
        let trimmed = seed_json.trim();
        if trimmed.is_empty() {
            return Ok(SeedReport::default());
        }
        let root = parse_json(trimmed, false)
            .map_err(|e| SqliteError::Misuse(format!("invalid seed JSON: {e}")))?;
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
        let result = self.apply_seed_entries(entries);
        match result {
            Ok(report) => {
                if started_tx {
                    self.execute("COMMIT")?;
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

    fn apply_seed_entries(&mut self, entries: Vec<(String, JsonVal)>) -> SqliteResult<SeedReport> {
        let mut report = SeedReport::default();
        let has_structured_keys = entries.iter().any(|(k, _)| {
            matches!(
                k.as_str(),
                "schema" | "tables" | "statements" | "mode" | "pragmas"
            )
        });

        if !has_structured_keys {
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
                self.seed_table_rows(&table_name, None, &rows, SeedInsertMode::Ignore, &mut report)?;
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
                                self.seed_table_rows(&table_name, None, &rows, mode, &mut report)?;
                            }
                            JsonVal::Object(spec_fields) => {
                                self.seed_table_spec(Some(table_name), spec_fields, mode, &mut report)?;
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
                        self.seed_table_spec(None, spec_fields, mode, &mut report)?;
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
        self.seed_table_rows(&table_name, columns.as_deref(), &rows, mode, report)
    }

    fn run_seed_sql_list(&mut self, val: JsonVal, report: &mut SeedReport) -> SqliteResult<()> {
        match val {
            JsonVal::Null => Ok(()),
            JsonVal::Str(sql) => {
                if !sql.trim().is_empty() {
                    self.execute(&sql)?;
                    report.statements_executed += 1;
                }
                Ok(())
            }
            JsonVal::Array(items) => {
                for item in items {
                    match item {
                        JsonVal::Str(sql) => {
                            if !sql.trim().is_empty() {
                                self.execute(&sql)?;
                                report.statements_executed += 1;
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
                                        params = json_val_to_query_params(v)?;
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
                                let res = self.execute_with_params(&sql, &params)?;
                                report.statements_executed += 1;
                                if res.changes > 0 {
                                    report.rows_inserted += res.changes as usize;
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
        let params = QueryParams::positional(vec![SqlValue::text(table_name)]);
        let res = self.execute_with_params(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND lower(name) = lower(?) LIMIT 1",
            &params,
        )?;
        Ok(!res.rows.is_empty())
    }

    fn seed_table_rows(
        &mut self,
        table_name: &str,
        explicit_columns: Option<&[String]>,
        rows: &[JsonVal],
        mode: SeedInsertMode,
        report: &mut SeedReport,
    ) -> SqliteResult<()> {
        if table_name.trim().is_empty() {
            return Err(SqliteError::Misuse("seed table name cannot be empty".to_string()));
        }

        // Auto-create table if it does not yet exist and we have columns or row objects.
        if !self.table_exists(table_name)? {
            let inferred_cols = infer_columns_from_rows(explicit_columns, rows)?;
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

        for row in rows {
            match row {
                JsonVal::Object(fields) => {
                    if fields.is_empty() {
                        let sql = format!("{prefix}{quoted_table} DEFAULT VALUES");
                        let res = self.execute(&sql)?;
                        report.statements_executed += 1;
                        if res.changes > 0 {
                            report.rows_inserted += res.changes as usize;
                        }
                        continue;
                    }
                    let mut cols_sql = String::new();
                    let mut placeholders = String::new();
                    let mut values = Vec::with_capacity(fields.len());
                    for (i, (col, val)) in fields.iter().enumerate() {
                        if i > 0 {
                            cols_sql.push_str(", ");
                            placeholders.push_str(", ");
                        }
                        cols_sql.push_str(&quote_ident(col));
                        placeholders.push('?');
                        values.push(seed_json_to_sql_value(val));
                    }
                    let sql = format!(
                        "{prefix}{quoted_table} ({cols_sql}) VALUES ({placeholders})"
                    );
                    let params = QueryParams::positional(values);
                    let res = self.execute_with_params(&sql, &params)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                }
                JsonVal::Array(cells) => {
                    let mut placeholders = String::new();
                    let mut values = Vec::with_capacity(cells.len());
                    for (i, cell) in cells.iter().enumerate() {
                        if i > 0 {
                            placeholders.push_str(", ");
                        }
                        placeholders.push('?');
                        values.push(seed_json_to_sql_value(cell));
                    }
                    let sql = if let Some(cols) = explicit_columns {
                        if cols.len() != cells.len() {
                            return Err(SqliteError::Misuse(format!(
                                "seed row for '{table_name}' has {} values but {} columns were declared",
                                cells.len(),
                                cols.len()
                            )));
                        }
                        let cols_sql = cols
                            .iter()
                            .map(|c| quote_ident(c))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{prefix}{quoted_table} ({cols_sql}) VALUES ({placeholders})")
                    } else {
                        format!("{prefix}{quoted_table} VALUES ({placeholders})")
                    };
                    let params = QueryParams::positional(values);
                    let res = self.execute_with_params(&sql, &params)?;
                    report.statements_executed += 1;
                    if res.changes > 0 {
                        report.rows_inserted += res.changes as usize;
                    }
                }
                _ => {
                    return Err(SqliteError::Misuse(format!(
                        "seed rows for '{table_name}' must be objects or arrays"
                    )));
                }
            }
        }

        report.tables_seeded += 1;
        Ok(())
    }
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

fn infer_value_type(v: &JsonVal) -> InferredType {
    match v {
        JsonVal::Null => InferredType::Unknown,
        JsonVal::Bool(_) | JsonVal::Int(_) => InferredType::Integer,
        JsonVal::Real(_) => InferredType::Real,
        JsonVal::Str(_) | JsonVal::Array(_) => InferredType::Text,
        JsonVal::Object(entries) if entries.len() == 1 => {
            if entries[0].0 == "__blob" || entries[0].0 == "hex" {
                if matches!(seed_json_to_sql_value(v), SqlValue::Blob(_)) {
                    return InferredType::Blob;
                }
            }
            InferredType::Text
        }
        JsonVal::Object(_) => InferredType::Text,
    }
}

fn infer_columns_from_rows(
    explicit_columns: Option<&[String]>,
    rows: &[JsonVal],
) -> SqliteResult<Vec<(String, InferredType)>> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut types: Vec<InferredType> = Vec::new();

    if let Some(cols) = explicit_columns {
        for c in cols {
            if seen.insert(c.clone()) {
                order.push(c.clone());
                types.push(InferredType::Unknown);
            }
        }
    }

    for row in rows {
        match row {
            JsonVal::Object(fields) => {
                for (k, v) in fields {
                    let t = infer_value_type(v);
                    if let Some(idx) = order.iter().position(|existing| existing == k) {
                        types[idx] = types[idx].merge(t);
                    } else {
                        seen.insert(k.clone());
                        order.push(k.clone());
                        types.push(t);
                    }
                }
            }
            JsonVal::Array(cells) => {
                if let Some(cols) = explicit_columns {
                    for (i, cell) in cells.iter().enumerate() {
                        if i < cols.len() {
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

fn json_val_to_query_params(val: JsonVal) -> SqliteResult<QueryParams> {
    match val {
        JsonVal::Null => Ok(QueryParams::empty()),
        JsonVal::Array(items) => Ok(QueryParams::positional(
            items.iter().map(seed_json_to_sql_value).collect::<Vec<_>>(),
        )),
        JsonVal::Object(entries) => {
            let mut p = QueryParams::empty();
            for (k, v) in entries {
                p = p.with_named(k, seed_json_to_sql_value(&v));
            }
            Ok(p)
        }
        _ => Err(SqliteError::Misuse(
            "seed statement 'params' must be an array or object".to_string(),
        )),
    }
}

/// Convert a parsed [`JsonVal`] into a [`SqlValue`] (supporting `{ "__blob": [...] }` and
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
                    let mut bytes = Vec::with_capacity(items.len());
                    let mut ok = true;
                    for item in items {
                        match item {
                            JsonVal::Int(n) => bytes.push((*n & 0xff) as u8),
                            JsonVal::Real(f) => bytes.push(((f.trunc() as i64) & 0xff) as u8),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
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
    if raw.len() % 2 != 0 {
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
