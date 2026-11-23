//! Hardened WebAssembly embedding of XDB (`#![forbid(unsafe_code)]`).
//!
//! Built as an isolated workspace matching `zipp-wasm`'s profile (`overflow-checks = true`,
//! `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`, `wasm-bindgen = "=0.2.126"`).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use wasm_bindgen::prelude::*;
use xdb_core::{
    value::fp_format::{real_to_text, DEFAULT_FP_DIGITS},
    BudgetMeter, Meter, ResourceLimits, SqlValue, TextEncoding,
};
use xdb_parser::Dialect;
use xdb_sql::functions::json::{parse_json, JsonVal};
use xdb_sqlite::{
    open_database_from_bytes, ColumnHiddenKind, ColumnSpec, ConnectionOptions, ExecResult,
    OpenOptions, QueryParams, QueryResult, ScanOptions, SnapshotMode, SqliteConnection,
    SqliteDatabase,
};

/// A sandboxed, read-only SQLite 3 database handle exposed to JavaScript / ZIPP hosts.
#[derive(Debug)]
#[wasm_bindgen]
pub struct WasmDatabase {
    db: SqliteDatabase,
    meter: Arc<BudgetMeter>,
    snapshot_bytes: AtomicU64,
}

impl Drop for WasmDatabase {
    fn drop(&mut self) {
        let held = self.snapshot_bytes.swap(0, Ordering::AcqRel);
        if held > 0 {
            self.meter.release_memory(held);
        }
    }
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
        let input_bytes = (main_bytes.len() as u64)
            .saturating_add(wal_bytes.as_ref().map_or(0, |b| b.len() as u64))
            .saturating_add(journal_bytes.as_ref().map_or(0, |b| b.len() as u64));
        if input_bytes > 0 {
            meter
                .reserve_memory(input_bytes)
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let db = match open_database_from_bytes(
            main_bytes,
            wal_bytes.as_deref(),
            journal_bytes.as_deref(),
            OpenOptions {
                meter: Some(meter.clone()),
                ..OpenOptions::default()
            },
        ) {
            Ok(db) => db,
            Err(e) => {
                if input_bytes > 0 {
                    meter.release_memory(input_bytes);
                }
                return Err(JsValue::from_str(&e.to_string()));
            }
        };
        Ok(WasmDatabase {
            db,
            meter,
            snapshot_bytes: AtomicU64::new(input_bytes),
        })
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
    pub fn catalog_json(&self) -> Result<String, JsValue> {
        if self.db.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        format_catalog_json_metered(self.db.schema(), &self.meter).map_err(|e| JsValue::from_str(&e))
    }

    /// Return `sqlite_schema` entries as JSON.
    pub fn schema_json(&self) -> Result<String, JsValue> {
        if self.db.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        format_schema_json_metered(self.db.schema(), &self.meter).map_err(|e| JsValue::from_str(&e))
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
        let mut w = MeteredJsonWriter::new(&self.meter);
        w.push_row_object(tbl.columns(), &vals)
            .map_err(|e| JsValue::from_str(&e))?;
        Ok(w.into_string())
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
        let mut w = MeteredJsonWriter::new(&self.meter);
        w.push_str("[").map_err(|e| JsValue::from_str(&e))?;
        let mut count = 0u32;
        while (limit == 0 || count < limit)
            && cur.next().map_err(|e| JsValue::from_str(&e.to_string()))?
        {
            if count > 0 {
                w.push_str(",").map_err(|e| JsValue::from_str(&e))?;
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            w.push_row_object(tbl.columns(), &vals)
                .map_err(|e| JsValue::from_str(&e))?;
            count += 1;
        }
        w.push_str("]").map_err(|e| JsValue::from_str(&e))?;
        Ok(w.into_string())
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
        let mut w = MeteredJsonWriter::new(&self.meter);
        w.push_str("[").map_err(|e| JsValue::from_str(&e))?;
        let mut count = 0u32;
        while (limit == 0 || count < limit)
            && cur.next().map_err(|e| JsValue::from_str(&e.to_string()))?
        {
            if count > 0 {
                w.push_str(",").map_err(|e| JsValue::from_str(&e))?;
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            w.push_str("[").map_err(|e| JsValue::from_str(&e))?;
            for (j, v) in vals.iter().enumerate() {
                if j > 0 {
                    w.push_str(",").map_err(|e| JsValue::from_str(&e))?;
                }
                w.push_sql_value(v).map_err(|e| JsValue::from_str(&e))?;
            }
            w.push_str("]").map_err(|e| JsValue::from_str(&e))?;
            count += 1;
        }
        w.push_str("]").map_err(|e| JsValue::from_str(&e))?;
        Ok(w.into_string())
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
        if self.db.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        let params = match params_json {
            None => QueryParams::empty(),
            Some(s) if s.trim().is_empty() => QueryParams::empty(),
            Some(s) => {
                let _guard =
                    reserve_params_budget(&self.meter, s).map_err(|e| JsValue::from_str(&e))?;
                parse_params_json(s).map_err(|e| JsValue::from_str(&e))?
            }
        };
        let mut opts = self.db.default_query_options();
        opts.dialect = dialect;
        let result = self
            .db
            .query_with_options(sql, &params, &opts)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        format_query_result_json_metered(&result, &self.meter).map_err(|e| JsValue::from_str(&e))
    }

    /// Close the database and release cached page memory from the meter.
    pub fn close(&self) -> Result<(), JsValue> {
        let res = self
            .db
            .close()
            .map_err(|e| JsValue::from_str(&e.to_string()));
        let held = self.snapshot_bytes.swap(0, Ordering::AcqRel);
        if held > 0 {
            self.meter.release_memory(held);
        }
        res
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
#[derive(Debug)]
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
        let input_bytes = initial_bytes
            .as_ref()
            .map_or(0u64, |b| b.len() as u64)
            .saturating_add(wal_bytes.as_ref().map_or(0, |b| b.len() as u64))
            .saturating_add(journal_bytes.as_ref().map_or(0, |b| b.len() as u64));
        // Reserve incoming buffer bytes against `meter` while opening so huge initial/WAL/journal
        // buffers respect `max_memory_bytes` before/during snapshot ingestion. Released on drop
        // once `WritePager` holds its own metered pages and drops the raw input buffers.
        let input_guard =
            MemGuard::reserve(&meter, input_bytes).map_err(|e| JsValue::from_str(&e))?;
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
        drop(initial_bytes);
        drop(wal_bytes);
        drop(journal_bytes);
        drop(input_guard);
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
        if self.conn.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        let params = match params_json {
            None => QueryParams::empty(),
            Some(s) if s.trim().is_empty() => QueryParams::empty(),
            Some(s) => {
                let _guard =
                    reserve_params_budget(&self.meter, s).map_err(|e| JsValue::from_str(&e))?;
                parse_params_json(s).map_err(|e| JsValue::from_str(&e))?
            }
        };
        let res = self
            .conn
            .execute_with_dialect(sql, &params, dialect)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        format_exec_result_json_metered(&res, &self.meter).map_err(|e| JsValue::from_str(&e))
    }

    /// Seed this database from a declarative JSON seed document (`{ schema, tables, statements, mode }`
    /// or shorthand `{ tableName: [{ col: val }, ...] }` with automatic table creation).
    /// Runs atomically in a transaction and returns a JSON report:
    /// `{"tablesCreated":N,"tablesSeeded":M,"rowsInserted":R,"statementsExecuted":S}`.
    pub fn seed_json(&mut self, seed_json: &str) -> Result<String, JsValue> {
        if self.conn.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        let _guard =
            reserve_params_budget(&self.meter, seed_json).map_err(|e| JsValue::from_str(&e))?;
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
    pub fn export_bytes(&self) -> Result<Vec<u8>, JsValue> {
        if self.conn.is_closed() {
            return Err(JsValue::from_str("the database is closed"));
        }
        let est = u64::from(self.conn.page_count()).saturating_mul(u64::from(self.conn.page_size()));
        let _guard = check_export_budget(&self.meter, est).map_err(|e| JsValue::from_str(&e))?;
        Ok(self.conn.to_bytes())
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

/// RAII memory reservation guard against a [`BudgetMeter`].
struct MemGuard<'a> {
    meter: &'a BudgetMeter,
    reserved: u64,
}

impl<'a> MemGuard<'a> {
    fn reserve(meter: &'a BudgetMeter, bytes: u64) -> Result<Self, String> {
        if bytes > 0 {
            meter.reserve_memory(bytes).map_err(|e| e.to_string())?;
        }
        Ok(Self {
            meter,
            reserved: bytes,
        })
    }
}

impl Drop for MemGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            self.meter.release_memory(self.reserved);
            self.reserved = 0;
        }
    }
}

/// Cumulative memory- and step-metered JSON string builder.
///
/// Reserves memory *before* growing the underlying buffer and keeps the cumulative total
/// reserved until dropped, preventing unbounded output allocation under tight `max_memory_bytes`.
struct MeteredJsonWriter<'a> {
    out: String,
    meter: &'a BudgetMeter,
    reserved: u64,
    step_pending: u64,
}

impl<'a> MeteredJsonWriter<'a> {
    fn new(meter: &'a BudgetMeter) -> Self {
        Self {
            out: String::new(),
            meter,
            reserved: 0,
            step_pending: 0,
        }
    }

    fn reserve_extra(&mut self, bytes: u64) -> Result<(), String> {
        self.step_pending = self.step_pending.saturating_add(bytes);
        if self.step_pending >= 64 {
            let steps = self.step_pending / 64;
            self.step_pending %= 64;
            self.meter.charge_steps(steps).map_err(|e| e.to_string())?;
        }
        if bytes > 0 {
            self.meter
                .reserve_memory(bytes)
                .map_err(|e| e.to_string())?;
            self.reserved = self.reserved.saturating_add(bytes);
        }
        Ok(())
    }

    fn push_str(&mut self, s: &str) -> Result<(), String> {
        self.reserve_extra(s.len() as u64)?;
        self.out.push_str(s);
        Ok(())
    }

    fn push_json_string(&mut self, s: &str) -> Result<(), String> {
        // Reserve exact escaped output length BEFORE pushing bytes so even a single huge
        // string with control characters cannot exceed `max_memory_bytes`.
        let mut needed: u64 = 2; // quotes
        for ch in s.chars() {
            needed = needed.saturating_add(match ch {
                '"' | '\\' | '\n' | '\r' | '\t' => 2,
                c if (c as u32) < 0x20 => 6,
                c => c.len_utf8() as u64,
            });
        }
        self.reserve_extra(needed)?;
        push_json_string_unchecked(&mut self.out, s);
        Ok(())
    }

    fn push_opt_str(&mut self, s: Option<&str>) -> Result<(), String> {
        match s {
            Some(v) => self.push_json_string(v),
            None => self.push_str("null"),
        }
    }

    fn push_sql_value(&mut self, v: &SqlValue) -> Result<(), String> {
        match v {
            SqlValue::Null => self.push_str("null"),
            SqlValue::Int(i) => {
                let s = i.to_string();
                self.push_str(&s)
            }
            SqlValue::Real(r) => {
                if r.is_finite() {
                    let s = real_to_text(*r, DEFAULT_FP_DIGITS);
                    self.push_str(&s)
                } else {
                    self.push_str("null")
                }
            }
            SqlValue::Text(bytes) => {
                let s = String::from_utf8_lossy(bytes);
                self.push_json_string(&s)
            }
            SqlValue::Blob(bytes) => {
                let needed = (bytes.len() as u64).saturating_mul(2).saturating_add(10);
                self.reserve_extra(needed)?;
                self.out.push_str("{\"hex\":\"");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for &b in bytes.iter() {
                    self.out.push(HEX[(b >> 4) as usize] as char);
                    self.out.push(HEX[(b & 0x0f) as usize] as char);
                }
                self.out.push_str("\"}");
                Ok(())
            }
        }
    }

    fn push_row_object(
        &mut self,
        columns: &[xdb_sqlite::ColumnInfo],
        vals: &[SqlValue],
    ) -> Result<(), String> {
        self.push_str("{")?;
        let mut k = 0usize;
        for col in columns {
            if col.hidden == ColumnHiddenKind::Virtual {
                continue;
            }
            if k > 0 {
                self.push_str(",")?;
            }
            self.push_json_string(&col.name)?;
            self.push_str(":")?;
            self.push_sql_value(&vals[k])?;
            k += 1;
        }
        self.push_str("}")
    }

    fn into_string(mut self) -> String {
        std::mem::take(&mut self.out)
    }
}

impl Drop for MeteredJsonWriter<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            self.meter.release_memory(self.reserved);
            self.reserved = 0;
        }
    }
}

fn format_catalog_json_metered(
    schema: &xdb_sqlite::SchemaCatalog,
    meter: &BudgetMeter,
) -> Result<String, String> {
    meter.charge_steps(1).map_err(|e| e.to_string())?;
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("{\"tables\":[")?;
    for (i, t) in schema.tables.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_str("{\"name\":")?;
        w.push_json_string(&t.name)?;
        w.push_str(",\"rootPage\":")?;
        w.push_str(&t.root_page.to_string())?;
        w.push_str(",\"withoutRowid\":")?;
        w.push_str(if t.without_rowid { "true" } else { "false" })?;
        w.push_str(",\"strict\":")?;
        w.push_str(if t.strict { "true" } else { "false" })?;
        w.push_str(",\"columns\":[")?;
        for (j, c) in t.columns.iter().enumerate() {
            if j > 0 {
                w.push_str(",")?;
            }
            w.push_str("{\"name\":")?;
            w.push_json_string(&c.name)?;
            w.push_str(",\"type\":")?;
            w.push_json_string(&c.decl_type)?;
            w.push_str(",\"notNull\":")?;
            w.push_str(if c.not_null { "true" } else { "false" })?;
            w.push_str(",\"pk\":")?;
            w.push_str(&c.pk.to_string())?;
            w.push_str(",\"hidden\":")?;
            w.push_str(&(c.hidden as u8).to_string())?;
            w.push_str("}")?;
        }
        w.push_str("]}")?;
    }
    w.push_str("],\"indexes\":[")?;
    for (i, ix) in schema.indexes.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_str("{\"name\":")?;
        w.push_json_string(&ix.name)?;
        w.push_str(",\"table\":")?;
        w.push_json_string(&ix.table_name)?;
        w.push_str(",\"unique\":")?;
        w.push_str(if ix.unique { "true" } else { "false" })?;
        w.push_str(",\"partial\":")?;
        w.push_str(if ix.partial { "true" } else { "false" })?;
        w.push_str(",\"rootPage\":")?;
        w.push_str(&ix.root_page.to_string())?;
        w.push_str("}")?;
    }
    w.push_str("],\"views\":[")?;
    for (i, v) in schema.views.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_json_string(&v.name)?;
    }
    w.push_str("],\"triggers\":[")?;
    for (i, tr) in schema.triggers.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_json_string(&tr.name)?;
    }
    w.push_str("]}")?;
    Ok(w.into_string())
}

fn format_schema_json_metered(
    schema: &xdb_sqlite::SchemaCatalog,
    meter: &BudgetMeter,
) -> Result<String, String> {
    meter.charge_steps(1).map_err(|e| e.to_string())?;
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("[")?;
    for (i, e) in schema.entries.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_str("{\"rowid\":")?;
        w.push_str(&e.rowid.to_string())?;
        w.push_str(",\"type\":")?;
        w.push_opt_str(e.obj_type.as_deref())?;
        w.push_str(",\"name\":")?;
        w.push_opt_str(e.name.as_deref())?;
        w.push_str(",\"tbl_name\":")?;
        w.push_opt_str(e.table_name.as_deref())?;
        w.push_str(",\"rootpage\":")?;
        w.push_str(&e.root_page.to_string())?;
        w.push_str(",\"sql\":")?;
        w.push_opt_str(e.sql.as_deref())?;
        w.push_str("}")?;
    }
    w.push_str("]")?;
    Ok(w.into_string())
}

fn format_exec_result_json_metered(
    res: &ExecResult,
    meter: &BudgetMeter,
) -> Result<String, String> {
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("{\"columns\":[")?;
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_json_string(col)?;
    }
    w.push_str("],\"rows\":[")?;
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_str("[")?;
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                w.push_str(",")?;
            }
            w.push_sql_value(cell)?;
        }
        w.push_str("]")?;
    }
    w.push_str("],\"changes\":")?;
    w.push_str(&res.changes.to_string())?;
    w.push_str(",\"lastInsertRowid\":")?;
    w.push_str(&res.last_insert_rowid.to_string())?;
    w.push_str("}")?;
    Ok(w.into_string())
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

fn reserve_params_budget<'a>(meter: &'a BudgetMeter, src: &str) -> Result<MemGuard<'a>, String> {
    let len = src.len() as u64;
    meter
        .charge_steps((len / 64).saturating_add(1))
        .map_err(|e| e.to_string())?;
    MemGuard::reserve(meter, len)
}

fn check_export_budget<'a>(meter: &'a BudgetMeter, est: u64) -> Result<MemGuard<'a>, String> {
    meter
        .charge_steps((est / 4096).saturating_add(1))
        .map_err(|e| e.to_string())?;
    MemGuard::reserve(meter, est)
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
                            JsonVal::Int(n) if (0..=255).contains(n) => bytes.push(*n as u8),
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
            } else if entries[0].0 == "__i64" {
                if let JsonVal::Str(ref s) = entries[0].1 {
                    if let Ok(n) = s.trim().parse::<i64>() {
                        return SqlValue::Int(n);
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
    if !raw.len().is_multiple_of(2) {
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

fn format_query_result_json_metered(
    res: &QueryResult,
    meter: &BudgetMeter,
) -> Result<String, String> {
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("{\"columns\":[")?;
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_json_string(col)?;
    }
    w.push_str("],\"rows\":[")?;
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            w.push_str(",")?;
        }
        w.push_str("[")?;
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                w.push_str(",")?;
            }
            w.push_sql_value(cell)?;
        }
        w.push_str("]")?;
    }
    w.push_str("]}")?;
    Ok(w.into_string())
}

fn push_json_string_unchecked(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let code = c as u8;
                out.push_str("\\u00");
                out.push(HEX[(code >> 4) as usize] as char);
                out.push(HEX[(code & 0x0f) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_json_strict_blob_and_i64_roundtrip() {
        let p = parse_params_json(
            r#"[{"__blob":[0,127,255]}, {"__i64":"9223372036854775807"}, {"hex":"deadBEEF"}]"#,
        )
        .unwrap();
        assert_eq!(p.positional.len(), 3);
        assert!(p.positional[0].exact_eq(&SqlValue::blob(vec![0, 127, 255])));
        assert!(p.positional[1].exact_eq(&SqlValue::Int(i64::MAX)));
        assert!(p.positional[2].exact_eq(&SqlValue::blob(vec![0xde, 0xad, 0xbe, 0xef])));

        // Out-of-range or float values in __blob must NOT silently truncate into a BLOB.
        let bad = parse_params_json(r#"[{"__blob":[256]}, {"__blob":[-1]}, {"__blob":[1.5]}]"#)
            .unwrap();
        for val in &bad.positional {
            assert!(
                !matches!(val, SqlValue::Blob(_)),
                "invalid __blob should not coerce to Blob: {val:?}"
            );
        }
    }

    #[test]
    fn metered_json_writer_enforces_cumulative_memory_and_releases_on_drop() {
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100_000),
            max_memory_bytes: Some(256),
            max_page_reads: None,
        });
        let rows: Vec<Vec<SqlValue>> = (0..50)
            .map(|i| vec![SqlValue::Int(i), SqlValue::text("0123456789")])
            .collect();
        let res = QueryResult {
            columns: vec!["id".into(), "payload".into()],
            rows,
        };
        let err = format_query_result_json_metered(&res, &meter).unwrap_err();
        assert!(
            err.to_ascii_lowercase().contains("memory") || err.to_ascii_lowercase().contains("limit"),
            "expected memory limit error, got: {err}"
        );
        // RAII drop must release all reserved memory back to 0.
        assert_eq!(meter.usage().memory_bytes, 0);
        assert!(meter.usage().exhausted);
    }

    #[test]
    fn check_export_budget_rejects_exhausted_meter_even_when_empty() {
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1),
            max_memory_bytes: Some(1024),
            max_page_reads: None,
        });
        meter.charge_steps(1).unwrap();
        // Even for 0-byte empty DB, an exhausted meter must fail export_bytes!
        let err = check_export_budget(&meter, 0).err().expect("exhausted meter must error");
        assert!(
            err.to_ascii_lowercase().contains("step") || err.to_ascii_lowercase().contains("limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mem_guard_bounds_initial_bytes_and_releases_cleanly() {
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: None,
            max_memory_bytes: Some(1024),
            max_page_reads: None,
        });
        assert!(MemGuard::reserve(&meter, 2048).is_err());
        assert_eq!(meter.usage().memory_bytes, 0);

        let meter2 = BudgetMeter::new(ResourceLimits {
            max_steps: None,
            max_memory_bytes: Some(1024),
            max_page_reads: None,
        });
        {
            let _g = MemGuard::reserve(&meter2, 512).unwrap();
            assert_eq!(meter2.usage().memory_bytes, 512);
        }
        assert_eq!(meter2.usage().memory_bytes, 0);
    }
}
