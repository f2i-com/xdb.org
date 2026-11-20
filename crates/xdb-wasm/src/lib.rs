//! Hardened WebAssembly embedding of XDB (`#![forbid(unsafe_code)]`).
//!
//! Built as an isolated workspace matching `zipp-wasm`'s profile (`overflow-checks = true`,
//! `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`, `wasm-bindgen = "=0.2.126"`).

#![forbid(unsafe_code)]

use std::sync::Arc;

use wasm_bindgen::prelude::*;
use xdb_core::{
    value::fp_format::{real_to_text, DEFAULT_FP_DIGITS},
    BudgetMeter, ResourceLimits, SqlValue, TextEncoding,
};
use xdb_parser::Dialect;
use xdb_sql::functions::json::{parse_json, JsonVal};
use xdb_sqlite::{
    open_database_from_bytes, ColumnHiddenKind, ColumnSpec, ConnectionOptions, ExecResult,
    OpenOptions, QueryParams, QueryResult, ScanOptions, SnapshotMode, SqliteConnection,
    SqliteDatabase,
};

/// A sandboxed, read-only SQLite 3 database handle exposed to JavaScript / ZIPP hosts.
#[wasm_bindgen]
pub struct WasmDatabase {
    db: SqliteDatabase,
    meter: Arc<BudgetMeter>,
}

#[wasm_bindgen]
impl WasmDatabase {
    /// Open a SQLite database from raw bytes (with optional `-wal` and `-journal` bytes)
    /// under explicit sandbox step, memory, and page-read ceilings (`0` = unlimited).
    #[wasm_bindgen(constructor)]
    pub fn open(
        main_bytes: &[u8],
        wal_bytes: Option<Vec<u8>>,
        journal_bytes: Option<Vec<u8>>,
        max_steps: u64,
        max_memory_bytes: u64,
        max_page_reads: u64,
    ) -> Result<WasmDatabase, JsValue> {
        let limits = ResourceLimits {
            max_steps: (max_steps > 0).then_some(max_steps),
            max_memory_bytes: (max_memory_bytes > 0).then_some(max_memory_bytes),
            max_page_reads: (max_page_reads > 0).then_some(max_page_reads),
        };
        let meter = Arc::new(BudgetMeter::new(limits));
        let db = open_database_from_bytes(
            main_bytes,
            wal_bytes.as_deref(),
            journal_bytes.as_deref(),
            OpenOptions {
                meter: Some(meter.clone()),
                ..OpenOptions::default()
            },
        )
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(WasmDatabase { db, meter })
    }

    /// Database page size in bytes.
    pub fn page_size(&self) -> u32 {
        self.db.page_size()
    }

    /// Effective page count in this snapshot.
    pub fn page_count(&self) -> u32 {
        self.db.page_count()
    }

    /// Database text encoding (`"UTF-8"`, `"UTF-16le"`, or `"UTF-16be"`).
    pub fn encoding(&self) -> String {
        match self.db.encoding() {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf16Le => "UTF-16le",
            TextEncoding::Utf16Be => "UTF-16be",
        }
        .to_string()
    }

    /// Snapshot mode (`"empty"`, `"rollback"`, `"wal"`, or `"hot-journal-overlay"`).
    pub fn snapshot_mode(&self) -> String {
        match self.db.mode() {
            SnapshotMode::Empty => "empty",
            SnapshotMode::Rollback => "rollback",
            SnapshotMode::Wal => "wal",
            SnapshotMode::HotJournalOverlay => "hot-journal-overlay",
        }
        .to_string()
    }

    /// Return parsed schema metadata (tables with columns, indexes, views, triggers) as JSON.
    pub fn catalog_json(&self) -> String {
        let schema = self.db.schema();
        let mut out = String::from("{\"tables\":[");
        for (i, t) in schema.tables.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            push_json_string(&mut out, &t.name);
            out.push_str(",\"rootPage\":");
            out.push_str(&t.root_page.to_string());
            out.push_str(",\"withoutRowid\":");
            out.push_str(if t.without_rowid { "true" } else { "false" });
            out.push_str(",\"strict\":");
            out.push_str(if t.strict { "true" } else { "false" });
            out.push_str(",\"columns\":[");
            for (j, c) in t.columns.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                push_json_string(&mut out, &c.name);
                out.push_str(",\"type\":");
                push_json_string(&mut out, &c.decl_type);
                out.push_str(",\"notNull\":");
                out.push_str(if c.not_null { "true" } else { "false" });
                out.push_str(",\"pk\":");
                out.push_str(&c.pk.to_string());
                out.push_str(",\"hidden\":");
                out.push_str(&(c.hidden as u8).to_string());
                out.push('}');
            }
            out.push_str("]}");
        }
        out.push_str("],\"indexes\":[");
        for (i, ix) in schema.indexes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            push_json_string(&mut out, &ix.name);
            out.push_str(",\"table\":");
            push_json_string(&mut out, &ix.table_name);
            out.push_str(",\"unique\":");
            out.push_str(if ix.unique { "true" } else { "false" });
            out.push_str(",\"partial\":");
            out.push_str(if ix.partial { "true" } else { "false" });
            out.push_str(",\"rootPage\":");
            out.push_str(&ix.root_page.to_string());
            out.push('}');
        }
        out.push_str("],\"views\":[");
        for (i, v) in schema.views.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_string(&mut out, &v.name);
        }
        out.push_str("],\"triggers\":[");
        for (i, tr) in schema.triggers.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_string(&mut out, &tr.name);
        }
        out.push_str("]}");
        out
    }

    /// Return `sqlite_schema` entries as JSON.
    pub fn schema_json(&self) -> String {
        let mut out = String::from("[");
        for (i, e) in self.db.schema().entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"rowid\":");
            out.push_str(&e.rowid.to_string());
            out.push_str(",\"type\":");
            push_opt_str_json(&mut out, e.obj_type.as_deref());
            out.push_str(",\"name\":");
            push_opt_str_json(&mut out, e.name.as_deref());
            out.push_str(",\"tbl_name\":");
            push_opt_str_json(&mut out, e.table_name.as_deref());
            out.push_str(",\"rootpage\":");
            out.push_str(&e.root_page.to_string());
            out.push_str(",\"sql\":");
            push_opt_str_json(&mut out, e.sql.as_deref());
            out.push('}');
        }
        out.push(']');
        out
    }

    /// Count rows in a named table.
    pub fn count_table(&self, name: &str) -> Result<u64, JsValue> {
        let tbl = self
            .db
            .table(name)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        tbl.count().map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Count entries in a named index.
    pub fn count_index(&self, name: &str) -> Result<u64, JsValue> {
        let ix = self
            .db
            .index(name)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        ix.count().map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Look up one row by rowid from a named rowid table as JSON (`null` if not found).
    pub fn get_row_json(&self, table: &str, rowid: i64) -> Result<String, JsValue> {
        let tbl = self
            .db
            .table(table)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let stored: Vec<ColumnSpec> = tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| ColumnSpec::Cid(c.cid))
            .collect();
        let opts = ScanOptions {
            columns: Some(stored),
            rowid: false,
        };
        let row = tbl
            .get(rowid, Some(&opts))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let Some(vals) = row else {
            return Ok("null".to_string());
        };
        let mut out = String::new();
        push_row_object_json(&mut out, tbl.columns(), &vals);
        Ok(out)
    }

    /// Scan up to `limit` logical rows (`0` = all) from a named table (stored columns) as JSON.
    pub fn scan_table_json(&self, table: &str, limit: u32) -> Result<String, JsValue> {
        let tbl = self
            .db
            .table(table)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let stored: Vec<ColumnSpec> = tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| ColumnSpec::Cid(c.cid))
            .collect();
        let opts = ScanOptions {
            columns: Some(stored),
            rowid: false,
        };
        let mut cur = tbl
            .cursor(Some(&opts))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut out = String::from("[");
        let mut count = 0u32;
        while cur.next().map_err(|e| JsValue::from_str(&e.to_string()))? {
            if limit > 0 && count >= limit {
                break;
            }
            if count > 0 {
                out.push(',');
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            push_row_object_json(&mut out, tbl.columns(), &vals);
            count += 1;
        }
        out.push(']');
        Ok(out)
    }

    /// Scan up to `limit` entries (`0` = all) from a named index as JSON arrays.
    pub fn scan_index_json(&self, index: &str, limit: u32) -> Result<String, JsValue> {
        let ix = self
            .db
            .index(index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut cur = ix
            .cursor()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut out = String::from("[");
        let mut count = 0u32;
        while cur.next().map_err(|e| JsValue::from_str(&e.to_string()))? {
            if limit > 0 && count >= limit {
                break;
            }
            if count > 0 {
                out.push(',');
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            out.push('[');
            for (j, v) in vals.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                push_sql_value_json(&mut out, v);
            }
            out.push(']');
            count += 1;
        }
        out.push(']');
        Ok(out)
    }

    /// Execute a SQLite `SELECT` / `VALUES` / `WITH` query and return
    /// `{"columns":[...],"rows":[[...]]}` as JSON.
    ///
    /// Optional `params_json` may be a JSON array (`[1, "a"]` for positional `?`)
    /// or JSON object (`{":id": 1}` / `{"id": 1}` for named parameters).
    pub fn query_json(&self, sql: &str, params_json: Option<String>) -> Result<String, JsValue> {
        self.run_query_json(sql, params_json.as_deref(), Dialect::Sqlite)
    }

    /// Execute an XDB-dialect SQL query (`FROM`-first, `GROUP BY ALL`, `ORDER BY ALL`,
    /// `::`, `* EXCLUDE`/`REPLACE`, `ILIKE`) and return `{"columns":[...],"rows":[[...]]}` as JSON.
    pub fn query_xdb_json(
        &self,
        sql: &str,
        params_json: Option<String>,
    ) -> Result<String, JsValue> {
        self.run_query_json(sql, params_json.as_deref(), Dialect::Xdb)
    }

    fn run_query_json(
        &self,
        sql: &str,
        params_json: Option<&str>,
        dialect: Dialect,
    ) -> Result<String, JsValue> {
        let params = match params_json {
            None => QueryParams::empty(),
            Some(s) if s.trim().is_empty() => QueryParams::empty(),
            Some(s) => parse_params_json(s).map_err(|e| JsValue::from_str(&e))?,
        };
        let mut opts = self.db.default_query_options();
        opts.dialect = dialect;
        let result = self
            .db
            .query_with_options(sql, &params, &opts)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(format_query_result_json(&result))
    }

    /// Close the database and release cached page memory from the meter.
    pub fn close(&self) -> Result<(), JsValue> {
        self.db
            .close()
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Current sandbox meter usage as JSON.
    pub fn meter_usage_json(&self) -> String {
        format_meter_usage(&self.meter)
    }
}

/// A sandboxed, read-write SQLite 3 connection exposed to JavaScript / ZIPP hosts.
///
/// Supports full DDL (`CREATE TABLE/INDEX/VIEW`, `ALTER TABLE`, `DROP`), DML (`INSERT`,
/// `UPSERT`, `UPDATE`, `DELETE`, `RETURNING`), transactions & nested savepoints, `PRAGMA`,
/// full SQLite 3.54.1 + XDB `SELECT`, deterministic sandbox metering, and exporting valid
/// SQLite 3 `.db` binary images via [`WasmConnection::export_bytes`].
#[wasm_bindgen]
pub struct WasmConnection {
    conn: SqliteConnection,
    meter: Arc<BudgetMeter>,
}

#[wasm_bindgen]
impl WasmConnection {
    /// Open a sandboxed read-write SQLite connection.
    ///
    /// - Pass `initial_bytes = undefined/null` (or empty) to create a brand-new in-memory DB.
    /// - Pass an existing SQLite `.db` `Uint8Array` (and optional `wal_bytes` / `journal_bytes`)
    ///   to load it into a sandboxed writable copy.
    /// - `page_size`: `0` uses SQLite's default `4096`.
    /// - `max_steps`, `max_memory_bytes`, `max_page_reads`: `0` = unlimited.
    #[wasm_bindgen(constructor)]
    pub fn open(
        initial_bytes: Option<Vec<u8>>,
        wal_bytes: Option<Vec<u8>>,
        journal_bytes: Option<Vec<u8>>,
        page_size: u32,
        max_steps: u64,
        max_memory_bytes: u64,
        max_page_reads: u64,
    ) -> Result<WasmConnection, JsValue> {
        let limits = ResourceLimits {
            max_steps: (max_steps > 0).then_some(max_steps),
            max_memory_bytes: (max_memory_bytes > 0).then_some(max_memory_bytes),
            max_page_reads: (max_page_reads > 0).then_some(max_page_reads),
        };
        let meter = Arc::new(BudgetMeter::new(limits));
        let opts = ConnectionOptions {
            page_size: if page_size == 0 { 4096 } else { page_size },
            dialect: Dialect::Sqlite,
            meter: Some(meter.clone()),
        };
        let conn = match initial_bytes {
            Some(ref b) if !b.is_empty() => SqliteConnection::open_from_bytes(
                b,
                wal_bytes.as_deref(),
                journal_bytes.as_deref(),
                opts,
            ),
            _ => SqliteConnection::open_in_memory(opts),
        }
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(WasmConnection { conn, meter })
    }

    /// Execute one or more SQLite statements (DDL, DML, transactions, PRAGMA, SELECT) and return
    /// `{"columns":[...],"rows":[[...]],"changes":N,"lastInsertRowid":R}` as JSON.
    pub fn execute_json(&mut self, sql: &str, params_json: Option<String>) -> Result<String, JsValue> {
        self.run_exec_json(sql, params_json.as_deref(), Dialect::Sqlite)
    }

    /// Execute one or more XDB-dialect SQL statements (`FROM`-first, `GROUP BY ALL`, `ORDER BY ALL`,
    /// `::`, `* EXCLUDE`/`REPLACE`, `ILIKE`, plus full DDL/DML/transactions).
    pub fn execute_xdb_json(
        &mut self,
        sql: &str,
        params_json: Option<String>,
    ) -> Result<String, JsValue> {
        self.run_exec_json(sql, params_json.as_deref(), Dialect::Xdb)
    }

    fn run_exec_json(
        &mut self,
        sql: &str,
        params_json: Option<&str>,
        dialect: Dialect,
    ) -> Result<String, JsValue> {
        let params = match params_json {
            None => QueryParams::empty(),
            Some(s) if s.trim().is_empty() => QueryParams::empty(),
            Some(s) => parse_params_json(s).map_err(|e| JsValue::from_str(&e))?,
        };
        let res = self
            .conn
            .execute_with_dialect(sql, &params, dialect)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(format_exec_result_json(&res))
    }

    /// Seed this database from a declarative JSON seed document (`{ schema, tables, statements, mode }`
    /// or shorthand `{ tableName: [{ col: val }, ...] }` with automatic table creation).
    /// Runs atomically in a transaction and returns a JSON report:
    /// `{"tablesCreated":N,"tablesSeeded":M,"rowsInserted":R,"statementsExecuted":S}`.
    pub fn seed_json(&mut self, seed_json: &str) -> Result<String, JsValue> {
        let report = self
            .conn
            .seed_json(seed_json)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(format!(
            "{{\"tablesCreated\":{},\"tablesSeeded\":{},\"rowsInserted\":{},\"statementsExecuted\":{}}}",
            report.tables_created,
            report.tables_seeded,
            report.rows_inserted,
            report.statements_executed
        ))
    }

    /// Export the live database as a standalone SQLite 3 binary `.db` file (`Uint8Array`).
    pub fn export_bytes(&self) -> Vec<u8> {
        self.conn.to_bytes()
    }

    pub fn page_size(&self) -> u32 {
        self.conn.page_size()
    }

    pub fn page_count(&self) -> u32 {
        self.conn.page_count()
    }

    pub fn in_transaction(&self) -> bool {
        self.conn.in_transaction()
    }

    pub fn changes(&self) -> i64 {
        self.conn.changes()
    }

    pub fn total_changes(&self) -> i64 {
        self.conn.total_changes()
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.conn.last_insert_rowid()
    }

    pub fn meter_usage_json(&self) -> String {
        format_meter_usage(&self.meter)
    }

    pub fn close(&mut self) -> Result<(), JsValue> {
        self.conn
            .close()
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}

fn format_meter_usage(meter: &BudgetMeter) -> String {
    let u = meter.usage();
    format!(
        "{{\"steps\":{},\"memory_bytes\":{},\"peak_memory_bytes\":{},\"page_reads\":{},\"exhausted\":{}}}",
        u.steps, u.memory_bytes, u.peak_memory_bytes, u.page_reads, u.exhausted
    )
}

fn format_exec_result_json(res: &ExecResult) -> String {
    let mut out = String::from("{\"columns\":[");
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(&mut out, col);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            push_sql_value_json(&mut out, cell);
        }
        out.push(']');
    }
    out.push_str("],\"changes\":");
    out.push_str(&res.changes.to_string());
    out.push_str(",\"lastInsertRowid\":");
    out.push_str(&res.last_insert_rowid.to_string());
    out.push('}');
    out
}

fn push_row_object_json(
    out: &mut String,
    columns: &[xdb_sqlite::ColumnInfo],
    vals: &[SqlValue],
) {
    out.push('{');
    let mut k = 0usize;
    for col in columns {
        if col.hidden == ColumnHiddenKind::Virtual {
            continue;
        }
        if k > 0 {
            out.push(',');
        }
        push_json_string(out, &col.name);
        out.push(':');
        push_sql_value_json(out, &vals[k]);
        k += 1;
    }
    out.push('}');
}

fn push_opt_str_json(out: &mut String, s: Option<&str>) {
    match s {
        Some(v) => push_json_string(out, v),
        None => out.push_str("null"),
    }
}

fn push_sql_value_json(out: &mut String, v: &SqlValue) {
    match v {
        SqlValue::Null => out.push_str("null"),
        SqlValue::Int(i) => out.push_str(&i.to_string()),
        SqlValue::Real(r) => {
            if r.is_finite() {
                out.push_str(&real_to_text(*r, DEFAULT_FP_DIGITS));
            } else {
                out.push_str("null");
            }
        }
        SqlValue::Text(bytes) => {
            let s = String::from_utf8_lossy(bytes);
            push_json_string(out, &s);
        }
        SqlValue::Blob(bytes) => {
            out.push_str("{\"hex\":\"");
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for &b in bytes.iter() {
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
            out.push_str("\"}");
        }
    }
}

fn parse_params_json(src: &str) -> Result<QueryParams, String> {
    let val = parse_json(src, false).map_err(|e| format!("invalid params JSON: {e}"))?;
    match val {
        JsonVal::Null => Ok(QueryParams::empty()),
        JsonVal::Array(items) => Ok(QueryParams::positional(
            items.into_iter().map(json_param_to_sql).collect::<Vec<_>>(),
        )),
        JsonVal::Object(entries) => {
            let mut p = QueryParams::empty();
            for (k, v) in entries {
                p = p.with_named(k, json_param_to_sql(v));
            }
            Ok(p)
        }
        _ => Err("params JSON must be an array or object".to_string()),
    }
}

fn json_param_to_sql(v: JsonVal) -> SqlValue {
    match v {
        JsonVal::Null => SqlValue::Null,
        JsonVal::Bool(b) => SqlValue::Int(i64::from(b)),
        JsonVal::Int(i) => SqlValue::Int(i),
        JsonVal::Real(r) => SqlValue::real(r),
        JsonVal::Str(s) => SqlValue::text(s),
        JsonVal::Object(ref entries) if entries.len() == 1 => {
            if entries[0].0 == "__blob" {
                if let JsonVal::Array(ref items) = entries[0].1 {
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
                if let JsonVal::Str(ref hex) = entries[0].1 {
                    if let Some(bytes) = decode_hex_bytes(hex) {
                        return SqlValue::Blob(bytes.into_boxed_slice());
                    }
                }
            }
            v.to_sql_json()
        }
        other => other.to_sql_json(),
    }
}

fn decode_hex_bytes(s: &str) -> Option<Vec<u8>> {
    let raw = s.as_bytes();
    if raw.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn format_query_result_json(res: &QueryResult) -> String {
    let mut out = String::from("{\"columns\":[");
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(&mut out, col);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            push_sql_value_json(&mut out, cell);
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
