//! Hardened WebAssembly embedding of XDB (`#![forbid(unsafe_code)]`).
//!
//! Built as an isolated workspace matching `zipp-wasm`'s profile (`overflow-checks = true`,
//! `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`, `wasm-bindgen = "=0.2.126"`).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use wasm_bindgen::prelude::*;
use xdb_core::{
    value::fp_format::{real_to_text_buf, DEFAULT_FP_DIGITS},
    BudgetMeter, Meter, ResourceLimits, SqlValue, TextEncoding,
};
use xdb_parser::Dialect;
use xdb_sql::functions::json::{parse_json_metered_with, render_json_compact_metered, JsonVal};
use xdb_sqlite::{
    open_database_from_owned_bytes, ColumnHiddenKind, ColumnSpec, ConnectionOptions, ExecResult,
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
        main_bytes: Vec<u8>,
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
                .charge_steps((input_bytes / 4096).saturating_add(1))
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            meter
                .reserve_memory(input_bytes)
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let db = match open_database_from_owned_bytes(
            main_bytes,
            wal_bytes,
            journal_bytes,
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

    fn charge_name_budget(&self, name: &str) -> Result<MemGuard<'_>, JsValue> {
        charge_name_budget(&self.meter, name).map_err(|e| JsValue::from_str(&e))
    }

    /// Count rows in a named table.
    pub fn count_table(&self, name: &str) -> Result<u64, JsValue> {
        let _name_guard = self.charge_name_budget(name)?;
        let tbl = self
            .db
            .table(name)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        tbl.count().map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Count entries in a named index.
    pub fn count_index(&self, name: &str) -> Result<u64, JsValue> {
        let _name_guard = self.charge_name_budget(name)?;
        let ix = self
            .db
            .index(name)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        ix.count().map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Look up one row by rowid from a named rowid table as JSON (`null` if not found).
    pub fn get_row_json(&self, table: &str, rowid: i64) -> Result<String, JsValue> {
        let _name_guard = self.charge_name_budget(table)?;
        let tbl = self
            .db
            .table(table)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let stored_count = tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .count();
        self.meter
            .charge_steps((stored_count as u64).saturating_add(1))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let _spec_guard = MemGuard::reserve(
            &self.meter,
            (stored_count as u64).saturating_mul(std::mem::size_of::<ColumnSpec>() as u64),
        )
        .map_err(|e| JsValue::from_str(&e))?;
        let mut stored: Vec<ColumnSpec> = Vec::new();
        stored
            .try_reserve_exact(stored_count)
            .map_err(|_| JsValue::from_str("column spec allocation failed"))?;
        for c in tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
        {
            stored.push(ColumnSpec::Cid(c.cid));
        }
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
        w.into_string().map_err(|e| JsValue::from_str(&e))
    }

    /// Scan up to `limit` logical rows (`0` = all) from a named table (stored columns) as JSON.
    pub fn scan_table_json(&self, table: &str, limit: u32) -> Result<String, JsValue> {
        let _name_guard = self.charge_name_budget(table)?;
        let tbl = self
            .db
            .table(table)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        // Compute exact escaped key size and charge steps + memory BEFORE allocating!
        let mut stored_count: usize = 0;
        let mut keys_text_bytes: u64 = 0;
        for col in tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
        {
            let prefix = if stored_count > 0 { 4u64 } else { 3u64 }; // `,"`..`":` or `"`..`":`
            keys_text_bytes = keys_text_bytes
                .saturating_add(prefix)
                .saturating_add(json_escaped_len(&col.name));
            stored_count += 1;
        }
        let overhead_bytes = (stored_count as u64).saturating_mul(
            (std::mem::size_of::<ColumnSpec>() + std::mem::size_of::<(usize, usize)>()) as u64,
        );
        let total_keys_mem = keys_text_bytes.saturating_add(overhead_bytes);
        self.meter
            .charge_steps((keys_text_bytes / 64).saturating_add(stored_count as u64).saturating_add(1))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let _keys_guard =
            MemGuard::reserve(&self.meter, total_keys_mem).map_err(|e| JsValue::from_str(&e))?;

        let keys_cap = usize::try_from(keys_text_bytes)
            .map_err(|_| JsValue::from_str("table column keys exceed addressable memory"))?;
        let mut stored: Vec<ColumnSpec> = Vec::new();
        stored
            .try_reserve_exact(stored_count)
            .map_err(|_| JsValue::from_str("column spec allocation failed"))?;
        let mut key_buf = String::new();
        key_buf
            .try_reserve_exact(keys_cap)
            .map_err(|_| JsValue::from_str("column key buffer allocation failed"))?;
        let mut key_ranges: Vec<(usize, usize)> = Vec::new();
        key_ranges
            .try_reserve_exact(stored_count)
            .map_err(|_| JsValue::from_str("column key range allocation failed"))?;

        for (i, col) in tbl
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .enumerate()
        {
            stored.push(ColumnSpec::Cid(col.cid));
            let start = key_buf.len();
            if i > 0 {
                key_buf.push(',');
            }
            key_buf.push('"');
            push_json_escaped_inner(&mut key_buf, &col.name);
            key_buf.push_str("\":");
            key_ranges.push((start, key_buf.len()));
        }
        let opts = ScanOptions {
            columns: Some(stored),
            rowid: false,
        };
        let mut cur = tbl
            .cursor(Some(&opts))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut w = MeteredJsonWriter::new(&self.meter);
        w.push_byte(b'[').map_err(|e| JsValue::from_str(&e))?;
        let mut count = 0u32;
        while limit == 0 || count < limit {
            w.maybe_flush_credit();
            if !cur.next().map_err(|e| JsValue::from_str(&e.to_string()))? {
                break;
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            if count > 0 {
                w.push_byte(b',').map_err(|e| JsValue::from_str(&e))?;
            }
            w.push_byte(b'{').map_err(|e| JsValue::from_str(&e))?;
            for (&(ks, ke), v) in key_ranges.iter().zip(vals.iter()) {
                w.push_str(&key_buf[ks..ke])
                    .map_err(|e| JsValue::from_str(&e))?;
                w.push_sql_value(v).map_err(|e| JsValue::from_str(&e))?;
            }
            w.push_byte(b'}').map_err(|e| JsValue::from_str(&e))?;
            count += 1;
        }
        w.push_byte(b']').map_err(|e| JsValue::from_str(&e))?;
        w.into_string().map_err(|e| JsValue::from_str(&e))
    }

    /// Scan up to `limit` entries (`0` = all) from a named index as JSON arrays.
    pub fn scan_index_json(&self, index: &str, limit: u32) -> Result<String, JsValue> {
        let _name_guard = self.charge_name_budget(index)?;
        let ix = self
            .db
            .index(index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut cur = ix
            .cursor()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut w = MeteredJsonWriter::new(&self.meter);
        w.push_byte(b'[').map_err(|e| JsValue::from_str(&e))?;
        let mut count = 0u32;
        while limit == 0 || count < limit {
            w.maybe_flush_credit();
            if !cur.next().map_err(|e| JsValue::from_str(&e.to_string()))? {
                break;
            }
            let vals = cur.row().map_err(|e| JsValue::from_str(&e.to_string()))?;
            if count > 0 {
                w.push_byte(b',').map_err(|e| JsValue::from_str(&e))?;
            }
            w.push_byte(b'[').map_err(|e| JsValue::from_str(&e))?;
            for (j, v) in vals.iter().enumerate() {
                if j > 0 {
                    w.push_byte(b',').map_err(|e| JsValue::from_str(&e))?;
                }
                w.push_sql_value(v).map_err(|e| JsValue::from_str(&e))?;
            }
            w.push_byte(b']').map_err(|e| JsValue::from_str(&e))?;
            count += 1;
        }
        w.push_byte(b']').map_err(|e| JsValue::from_str(&e))?;
        w.into_string().map_err(|e| JsValue::from_str(&e))
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
        self.meter
            .charge_steps((sql.len() as u64 / 64).saturating_add(1))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let _sql_guard =
            MemGuard::reserve(&self.meter, sql.len() as u64).map_err(|e| JsValue::from_str(&e))?;
        let (_params_guard, params) = match params_json {
            None | Some("") => (None, QueryParams::empty()),
            Some(s) => {
                let mut guard =
                    reserve_params_budget(&self.meter, s).map_err(|e| JsValue::from_str(&e))?;
                if s.trim().is_empty() {
                    (Some(guard), QueryParams::empty())
                } else {
                    let parsed = parse_params_json_guarded(s, &self.meter, &mut guard)
                        .map_err(|e| JsValue::from_str(&e))?;
                    (Some(guard), parsed)
                }
            }
        };
        let mut opts = self.db.default_query_options();
        opts.dialect = dialect;
        let result = self
            .db
            .query_with_options(sql, &params, &opts)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        drop(params);
        drop(_params_guard);
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
        if input_bytes > 0 {
            meter
                .charge_steps((input_bytes / 4096).saturating_add(1))
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let input_guard =
            MemGuard::reserve(&meter, input_bytes).map_err(|e| JsValue::from_str(&e))?;
        let opts = ConnectionOptions {
            page_size: if page_size == 0 { 4096 } else { page_size },
            dialect: Dialect::Sqlite,
            meter: Some(meter.clone()),
        };
        let conn = match initial_bytes {
            Some(b) if !b.is_empty() => {
                SqliteConnection::open_from_owned_bytes(b, wal_bytes, journal_bytes, opts)
            }
            _ => SqliteConnection::open_in_memory(opts),
        }
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
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
        self.meter
            .charge_steps((sql.len() as u64 / 64).saturating_add(1))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let _sql_guard =
            MemGuard::reserve(&self.meter, sql.len() as u64).map_err(|e| JsValue::from_str(&e))?;
        let (_params_guard, params) = match params_json {
            None | Some("") => (None, QueryParams::empty()),
            Some(s) => {
                let mut guard =
                    reserve_params_budget(&self.meter, s).map_err(|e| JsValue::from_str(&e))?;
                if s.trim().is_empty() {
                    (Some(guard), QueryParams::empty())
                } else {
                    let parsed = parse_params_json_guarded(s, &self.meter, &mut guard)
                        .map_err(|e| JsValue::from_str(&e))?;
                    (Some(guard), parsed)
                }
            }
        };
        let res = self
            .conn
            .execute_with_dialect(sql, &params, dialect)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        drop(params);
        drop(_params_guard);
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

    /// Database text encoding (`"UTF-8"`, `"UTF-16le"`, or `"UTF-16be"`).
    pub fn encoding(&self) -> String {
        match self.conn.encoding() {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf16Le => "UTF-16le",
            TextEncoding::Utf16Be => "UTF-16be",
        }
        .to_string()
    }

    /// Return parsed schema metadata (tables with columns, indexes, views, triggers) as JSON.
    pub fn catalog_json(&self) -> Result<String, JsValue> {
        let schema = self
            .conn
            .schema()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        format_catalog_json_metered(&schema, &self.meter).map_err(|e| JsValue::from_str(&e))
    }

    /// Return `sqlite_schema` entries as JSON.
    pub fn schema_json(&self) -> Result<String, JsValue> {
        let schema = self
            .conn
            .schema()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        format_schema_json_metered(&schema, &self.meter).map_err(|e| JsValue::from_str(&e))
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

fn charge_name_budget<'a>(meter: &'a BudgetMeter, name: &str) -> Result<MemGuard<'a>, String> {
    let len = name.len() as u64;
    meter
        .charge_steps((len / 64).saturating_add(1))
        .map_err(|e| e.to_string())?;
    MemGuard::reserve(meter, len)
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

    fn add(&mut self, bytes: u64) -> Result<(), String> {
        if bytes > 0 {
            self.meter
                .reserve_memory(bytes)
                .map_err(|e| e.to_string())?;
            self.reserved = self.reserved.saturating_add(bytes);
        }
        Ok(())
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

const JSON_WRITER_CHUNK: u64 = 4096;

/// Cumulative memory- and step-metered JSON string builder.
///
/// Reserves memory *before* growing the underlying buffer (batching in 4 KiB chunks when
/// headroom allows to minimize atomic operations) and keeps the cumulative total reserved
/// until dropped, preventing unbounded output allocation under tight `max_memory_bytes`.
struct MeteredJsonWriter<'a> {
    out: String,
    meter: &'a BudgetMeter,
    reserved: u64,
    credit: u64,
    step_pending: u64,
}

impl<'a> MeteredJsonWriter<'a> {
    fn new(meter: &'a BudgetMeter) -> Self {
        Self {
            out: String::new(),
            meter,
            reserved: 0,
            credit: 0,
            step_pending: 0,
        }
    }

    fn flush_credit(&mut self) {
        if self.credit > 0 {
            let release = self.credit.min(self.reserved);
            self.reserved -= release;
            self.credit = 0;
            if release > 0 {
                self.meter.release_memory(release);
            }
        }
    }

    fn maybe_flush_credit(&mut self) {
        if self.credit == 0 {
            return;
        }
        if let Some(lim) = self.meter.limits().max_memory_bytes {
            let avail = lim.saturating_sub(self.meter.memory_bytes());
            if avail < JSON_WRITER_CHUNK * 4 {
                self.flush_credit();
            }
        }
    }

    fn reserve_extra(&mut self, bytes: u64) -> Result<(), String> {
        self.step_pending = self.step_pending.saturating_add(bytes);
        if self.step_pending >= 256 {
            let steps = self.step_pending / 64;
            self.step_pending %= 64;
            self.meter.charge_steps(steps).map_err(|e| e.to_string())?;
        }
        if bytes <= self.credit {
            self.credit -= bytes;
            return Ok(());
        }
        let deficit = bytes - self.credit;
        let chunk = match self.meter.limits().max_memory_bytes {
            Some(lim) => {
                let avail = lim.saturating_sub(self.meter.memory_bytes());
                if avail >= JSON_WRITER_CHUNK * 4 {
                    deficit.max(JSON_WRITER_CHUNK)
                } else {
                    deficit
                }
            }
            None => deficit.max(JSON_WRITER_CHUNK),
        };
        let target_spare = self.credit.saturating_add(chunk);
        self.meter
            .reserve_memory(chunk)
            .map_err(|e| e.to_string())?;
        if let Err(e) = self.try_reserve_out(target_spare) {
            self.meter.release_memory(chunk);
            return Err(e);
        }
        self.reserved = self.reserved.saturating_add(chunk);
        self.credit = target_spare - bytes;
        Ok(())
    }

    #[inline]
    fn try_reserve_out(&mut self, extra: u64) -> Result<(), String> {
        let cap = usize::try_from(extra)
            .map_err(|_| "JSON output exceeds addressable memory".to_string())?;
        self.out
            .try_reserve(cap)
            .map_err(|_| "JSON output allocation failed".to_string())
    }

    #[inline]
    fn push_byte(&mut self, b: u8) -> Result<(), String> {
        if self.credit >= 1 && self.step_pending < 255 {
            self.credit -= 1;
            self.step_pending += 1;
            self.out.push(b as char);
            return Ok(());
        }
        self.reserve_extra(1)?;
        self.out.push(b as char);
        Ok(())
    }

    #[inline]
    fn push_str(&mut self, s: &str) -> Result<(), String> {
        let len = s.len() as u64;
        if len <= self.credit && self.step_pending.saturating_add(len) < 256 {
            self.credit -= len;
            self.step_pending += len;
            self.out.push_str(s);
            return Ok(());
        }
        self.reserve_extra(len)?;
        self.out.push_str(s);
        Ok(())
    }

    fn push_i64(&mut self, n: i64) -> Result<(), String> {
        let mut buf = [0u8; 21];
        let s = format_i64_stack(n, &mut buf);
        self.push_str(s)
    }

    fn push_u64(&mut self, n: u64) -> Result<(), String> {
        let mut buf = [0u8; 21];
        let s = format_u64_stack(n, &mut buf);
        self.push_str(s)
    }

    fn push_json_string(&mut self, s: &str) -> Result<(), String> {
        // Fast SWAR-accelerated scan to the first byte needing JSON escaping (`< 0x20`, `"`, `\`).
        let first_esc = find_first_json_escape(s.as_bytes());
        let Some(first) = first_esc else {
            let needed = (s.len() as u64).saturating_add(2);
            self.reserve_extra(needed)?;
            self.out.push('"');
            self.out.push_str(s);
            self.out.push('"');
            return Ok(());
        };
        // Reserve exact escaped output length BEFORE pushing bytes (scan only suffix from `first`).
        let esc_len = (first as u64).saturating_add(json_escaped_len(&s[first..]));
        let needed = 2u64.saturating_add(esc_len);
        self.reserve_extra(needed)?;
        self.out.push('"');
        if first > 0 {
            self.out.push_str(&s[..first]);
        }
        push_json_escaped_inner(&mut self.out, &s[first..]);
        self.out.push('"');
        Ok(())
    }

    fn push_json_lossy_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        // Stream invalid UTF-8 chunks directly with zero temporary String allocation and
        // exact 1:1 reservation (accounting for 3-byte U+FFFD replacement characters).
        let mut needed: u64 = 2;
        for chunk in bytes.utf8_chunks() {
            needed = needed.saturating_add(json_escaped_len(chunk.valid()));
            if !chunk.invalid().is_empty() {
                needed = needed.saturating_add(3);
            }
        }
        self.reserve_extra(needed)?;
        self.out.push('"');
        for chunk in bytes.utf8_chunks() {
            if !chunk.valid().is_empty() {
                push_json_escaped_inner(&mut self.out, chunk.valid());
            }
            if !chunk.invalid().is_empty() {
                self.out.push('\u{FFFD}');
            }
        }
        self.out.push('"');
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
            SqlValue::Int(i) => self.push_i64(*i),
            SqlValue::Real(r) => {
                if r.is_finite() {
                    let mut buf = [0u8; 48];
                    let s = real_to_text_buf(*r, DEFAULT_FP_DIGITS, &mut buf);
                    self.push_str(s)
                } else {
                    self.push_str("null")
                }
            }
            SqlValue::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => self.push_json_string(s),
                Err(_) => self.push_json_lossy_bytes(bytes),
            },
            SqlValue::Blob(bytes) => {
                let needed = (bytes.len() as u64).saturating_mul(2).saturating_add(10);
                self.reserve_extra(needed)?;
                self.out.push_str("{\"hex\":\"");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let mut hex_buf = [0u8; 128];
                for chunk in bytes.chunks(64) {
                    let mut p = 0usize;
                    for &b in chunk {
                        hex_buf[p] = HEX[(b >> 4) as usize];
                        hex_buf[p + 1] = HEX[(b & 0x0f) as usize];
                        p += 2;
                    }
                    self.out
                        .push_str(std::str::from_utf8(&hex_buf[..p]).expect("ASCII hex"));
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
            match vals.get(k) {
                Some(v) => self.push_sql_value(v)?,
                None => self.push_str("null")?,
            }
            k += 1;
        }
        self.push_str("}")
    }

    fn into_string(mut self) -> Result<String, String> {
        if self.step_pending > 0 {
            let steps = self.step_pending.div_ceil(64);
            self.step_pending = 0;
            self.meter.charge_steps(steps).map_err(|e| e.to_string())?;
        }
        Ok(std::mem::take(&mut self.out))
    }
}

#[inline]
fn find_first_json_escape(bytes: &[u8]) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    const SPACES: u64 = 0x2020_2020_2020_2020;
    const QUOTES: u64 = 0x2222_2222_2222_2222;
    const SLASHES: u64 = 0x5C5C_5C5C_5C5C_5C5C;

    let mut i = 0usize;
    for chunk in bytes.chunks_exact(8) {
        let w = u64::from_ne_bytes(chunk.try_into().unwrap());
        let lt20 = w.wrapping_sub(SPACES) & !w & HIGHS;
        let q = w ^ QUOTES;
        let eq_q = q.wrapping_sub(ONES) & !q & HIGHS;
        let s = w ^ SLASHES;
        let eq_s = s.wrapping_sub(ONES) & !s & HIGHS;
        if (lt20 | eq_q | eq_s) != 0 {
            for (k, &b) in chunk.iter().enumerate() {
                if b < 0x20 || b == b'"' || b == b'\\' {
                    return Some(i + k);
                }
            }
        }
        i += 8;
    }
    for (k, &b) in bytes[i..].iter().enumerate() {
        if b < 0x20 || b == b'"' || b == b'\\' {
            return Some(i + k);
        }
    }
    None
}

const DEC_PAIRS: &[u8; 200] = b"\
    0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

#[inline]
fn write_u64_digits(mut n: u64, buf: &mut [u8; 21]) -> usize {
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
        return i;
    }
    while n >= 100 {
        let d = ((n % 100) as usize) * 2;
        n /= 100;
        i -= 2;
        buf[i] = DEC_PAIRS[d];
        buf[i + 1] = DEC_PAIRS[d + 1];
    }
    if n >= 10 {
        let d = (n as usize) * 2;
        i -= 2;
        buf[i] = DEC_PAIRS[d];
        buf[i + 1] = DEC_PAIRS[d + 1];
    } else {
        i -= 1;
        buf[i] = b'0' + (n as u8);
    }
    i
}

fn format_u64_stack(n: u64, buf: &mut [u8; 21]) -> &str {
    let i = write_u64_digits(n, buf);
    std::str::from_utf8(&buf[i..]).expect("ASCII digits")
}

fn format_i64_stack(n: i64, buf: &mut [u8; 21]) -> &str {
    let mut i = write_u64_digits(n.unsigned_abs(), buf);
    if n < 0 {
        i -= 1;
        buf[i] = b'-';
    }
    std::str::from_utf8(&buf[i..]).expect("ASCII digits")
}

impl Drop for MeteredJsonWriter<'_> {
    fn drop(&mut self) {
        if self.step_pending > 0 {
            let _ = self.meter.charge_steps(self.step_pending.div_ceil(64));
            self.step_pending = 0;
        }
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
        w.push_u64(u64::from(t.root_page))?;
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
            w.push_u64(c.pk as u64)?;
            w.push_str(",\"hidden\":")?;
            w.push_u64(u64::from(c.hidden as u8))?;
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
        w.push_u64(u64::from(ix.root_page))?;
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
    w.into_string()
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
        w.push_i64(e.rowid)?;
        w.push_str(",\"type\":")?;
        w.push_opt_str(e.obj_type.as_deref())?;
        w.push_str(",\"name\":")?;
        w.push_opt_str(e.name.as_deref())?;
        w.push_str(",\"tbl_name\":")?;
        w.push_opt_str(e.table_name.as_deref())?;
        w.push_str(",\"rootpage\":")?;
        w.push_u64(u64::from(e.root_page))?;
        w.push_str(",\"sql\":")?;
        w.push_opt_str(e.sql.as_deref())?;
        w.push_str("}")?;
    }
    w.push_str("]")?;
    w.into_string()
}

fn estimate_json_val_bytes(v: &JsonVal) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<&JsonVal> = vec![v];
    while let Some(node) = stack.pop() {
        total = total.saturating_add(std::mem::size_of::<JsonVal>() as u64);
        match node {
            JsonVal::Str(s) => total = total.saturating_add(s.len() as u64),
            JsonVal::Array(items) => {
                for it in items {
                    stack.push(it);
                }
            }
            JsonVal::Object(entries) => {
                for (k, val) in entries {
                    total = total.saturating_add(k.len() as u64 + 24);
                    stack.push(val);
                }
            }
            JsonVal::Null | JsonVal::Bool(_) | JsonVal::Int(_) | JsonVal::Real(_) => {}
        }
    }
    total
}

fn estimate_sql_rows_bytes(cols: &[String], rows: &[Vec<SqlValue>]) -> u64 {
    let mut total: u64 = (cols.len() as u64).saturating_mul(24);
    for c in cols {
        total = total.saturating_add(c.len() as u64);
    }
    let val_sz = std::mem::size_of::<SqlValue>() as u64;
    for row in rows {
        total = total
            .saturating_add(24)
            .saturating_add((row.len() as u64).saturating_mul(val_sz));
        for v in row {
            if let SqlValue::Text(b) | SqlValue::Blob(b) = v {
                total = total.saturating_add(b.len() as u64);
            }
        }
    }
    total
}

fn format_exec_result_json_metered(
    res: &ExecResult,
    meter: &BudgetMeter,
) -> Result<String, String> {
    let _res_guard = MemGuard::reserve(meter, estimate_sql_rows_bytes(&res.columns, &res.rows))?;
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("{\"columns\":[")?;
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            w.push_byte(b',')?;
        }
        w.push_json_string(col)?;
    }
    w.push_str("],\"rows\":[")?;
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            w.push_byte(b',')?;
        }
        w.push_byte(b'[')?;
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                w.push_byte(b',')?;
            }
            w.push_sql_value(cell)?;
        }
        w.push_byte(b']')?;
    }
    w.push_str("],\"changes\":")?;
    w.push_i64(res.changes)?;
    w.push_str(",\"lastInsertRowid\":")?;
    w.push_i64(res.last_insert_rowid)?;
    w.push_str("}")?;
    w.into_string()
}

#[cfg(test)]
fn parse_params_json(src: &str, meter: &BudgetMeter) -> Result<QueryParams, String> {
    let mut guard = MemGuard::reserve(meter, 0)?;
    parse_params_json_guarded(src, meter, &mut guard)
}

fn parse_params_json_guarded(
    src: &str,
    meter: &BudgetMeter,
    guard: &mut MemGuard<'_>,
) -> Result<QueryParams, String> {
    let val = parse_json_metered_with(src, false, Some(meter))
        .map_err(|e| format!("invalid params JSON: {e}"))?;
    let _ast_guard = MemGuard::reserve(meter, estimate_json_val_bytes(&val))?;
    let slot_bytes = std::mem::size_of::<SqlValue>() as u64;
    match val {
        JsonVal::Null => Ok(QueryParams::empty()),
        JsonVal::Array(items) => {
            guard.add((items.len() as u64).saturating_mul(slot_bytes))?;
            let mut vals = Vec::new();
            vals.try_reserve(items.len())
                .map_err(|_| "params allocation failed".to_string())?;
            let mut pending_bytes = 0u64;
            for item in items {
                let sv = json_param_to_sql(item, meter)?;
                if let SqlValue::Text(ref b) | SqlValue::Blob(ref b) = sv {
                    pending_bytes = pending_bytes.saturating_add(b.len() as u64);
                    if pending_bytes >= 4096 {
                        guard.add(pending_bytes)?;
                        pending_bytes = 0;
                    }
                }
                vals.push(sv);
            }
            if pending_bytes > 0 {
                guard.add(pending_bytes)?;
            }
            Ok(QueryParams::positional(vals))
        }
        JsonVal::Object(entries) => {
            guard.add((entries.len() as u64).saturating_mul(slot_bytes.saturating_add(48)))?;
            let mut p = QueryParams::empty();
            p.named
                .try_reserve(entries.len())
                .map_err(|_| "params map allocation failed".to_string())?;
            let mut pending_bytes = 0u64;
            for (k, v) in entries {
                let klen = k.len() as u64;
                let sv = json_param_to_sql(v, meter)?;
                let vlen = match &sv {
                    SqlValue::Text(b) | SqlValue::Blob(b) => b.len() as u64,
                    _ => 0,
                };
                pending_bytes = pending_bytes.saturating_add(klen.saturating_add(vlen));
                if pending_bytes >= 4096 {
                    guard.add(pending_bytes)?;
                    pending_bytes = 0;
                }
                p = p.with_named(k, sv);
            }
            if pending_bytes > 0 {
                guard.add(pending_bytes)?;
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
    // Account for both the parsed JSON AST and converted QueryParams representation.
    MemGuard::reserve(meter, len.saturating_mul(2))
}

fn check_export_budget<'a>(meter: &'a BudgetMeter, est: u64) -> Result<MemGuard<'a>, String> {
    meter
        .charge_steps((est / 4096).saturating_add(1))
        .map_err(|e| e.to_string())?;
    MemGuard::reserve(meter, est)
}

fn json_param_to_sql(v: JsonVal, meter: &BudgetMeter) -> Result<SqlValue, String> {
    match v {
        JsonVal::Null => Ok(SqlValue::Null),
        JsonVal::Bool(b) => Ok(SqlValue::Int(i64::from(b))),
        JsonVal::Int(i) => Ok(SqlValue::Int(i)),
        JsonVal::Real(r) => Ok(SqlValue::real(r)),
        JsonVal::Str(s) => Ok(SqlValue::from(s)),
        JsonVal::Object(ref entries) if entries.len() == 1 => {
            if entries[0].0 == "__blob" {
                if let JsonVal::Array(ref items) = entries[0].1 {
                    let _blob_guard = MemGuard::reserve(meter, items.len() as u64)?;
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve(items.len())
                        .map_err(|_| "blob param allocation failed".to_string())?;
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
                        return Ok(SqlValue::Blob(bytes.into_boxed_slice()));
                    }
                }
            } else if entries[0].0 == "__i64" {
                if let JsonVal::Str(ref s) = entries[0].1 {
                    if let Ok(n) = s.trim().parse::<i64>() {
                        return Ok(SqlValue::Int(n));
                    }
                }
            } else if entries[0].0 == "hex" {
                if let JsonVal::Str(ref hex) = entries[0].1 {
                    let _hex_guard = MemGuard::reserve(meter, (hex.len() / 2) as u64)?;
                    if let Some(bytes) = decode_hex_bytes(hex)? {
                        return Ok(SqlValue::Blob(bytes.into_boxed_slice()));
                    }
                }
            }
            let rendered =
                render_json_compact_metered(&v, Some(meter)).map_err(|e| e.to_string())?;
            Ok(SqlValue::from(rendered))
        }
        other => {
            let rendered =
                render_json_compact_metered(&other, Some(meter)).map_err(|e| e.to_string())?;
            Ok(SqlValue::from(rendered))
        }
    }
}

fn decode_hex_bytes(s: &str) -> Result<Option<Vec<u8>>, String> {
    let raw = s.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return Ok(None);
    }
    let mut out = Vec::new();
    out.try_reserve(raw.len() / 2)
        .map_err(|_| "hex param allocation failed".to_string())?;
    for pair in raw.chunks_exact(2) {
        let Some(hi) = hex_nibble(pair[0]) else {
            return Ok(None);
        };
        let Some(lo) = hex_nibble(pair[1]) else {
            return Ok(None);
        };
        out.push((hi << 4) | lo);
    }
    Ok(Some(out))
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
    let _res_guard = MemGuard::reserve(meter, estimate_sql_rows_bytes(&res.columns, &res.rows))?;
    let mut w = MeteredJsonWriter::new(meter);
    w.push_str("{\"columns\":[")?;
    for (i, col) in res.columns.iter().enumerate() {
        if i > 0 {
            w.push_byte(b',')?;
        }
        w.push_json_string(col)?;
    }
    w.push_str("],\"rows\":[")?;
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            w.push_byte(b',')?;
        }
        w.push_byte(b'[')?;
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                w.push_byte(b',')?;
            }
            w.push_sql_value(cell)?;
        }
        w.push_byte(b']')?;
    }
    w.push_str("]}")?;
    w.into_string()
}

fn json_escaped_len(s: &str) -> u64 {
    let mut needed = 0u64;
    for &b in s.as_bytes() {
        needed = needed.saturating_add(match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        });
    }
    needed
}

fn push_json_escaped_inner(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = s.as_bytes();
    let mut run_start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0x00..=0x1f => None,
            _ => continue,
        };
        if i > run_start {
            out.push_str(&s[run_start..i]);
        }
        if let Some(lit) = esc {
            out.push_str(lit);
        } else {
            out.push_str("\\u00");
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        run_start = i + 1;
    }
    if run_start < bytes.len() {
        out.push_str(&s[run_start..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_json_strict_blob_and_i64_roundtrip() {
        let meter = BudgetMeter::new(ResourceLimits::default());
        let p = parse_params_json(
            r#"[{"__blob":[0,127,255]}, {"__i64":"9223372036854775807"}, {"hex":"deadBEEF"}]"#,
            &meter,
        )
        .unwrap();
        assert_eq!(p.positional.len(), 3);
        assert!(p.positional[0].exact_eq(&SqlValue::blob(vec![0, 127, 255])));
        assert!(p.positional[1].exact_eq(&SqlValue::Int(i64::MAX)));
        assert!(p.positional[2].exact_eq(&SqlValue::blob(vec![0xde, 0xad, 0xbe, 0xef])));

        // Out-of-range or float values in __blob must NOT silently truncate into a BLOB.
        let bad = parse_params_json(
            r#"[{"__blob":[256]}, {"__blob":[-1]}, {"__blob":[1.5]}]"#,
            &meter,
        )
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

    #[test]
    fn pass3_json_writer_stack_ints_escaping_chunking_and_invalid_utf8_metering() {
        // 1. Stack integer formatting & bulk-copy escaping across all boundary values.
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let res = QueryResult {
            columns: vec!["min".into(), "zero".into(), "max".into(), "txt".into()],
            rows: vec![vec![
                SqlValue::Int(i64::MIN),
                SqlValue::Int(0),
                SqlValue::Int(i64::MAX),
                SqlValue::text("café \"\\\n\r\t\x01\x1f 🚀"),
            ]],
        };
        let json = format_query_result_json_metered(&res, &meter).unwrap();
        assert_eq!(
            json,
            r#"{"columns":["min","zero","max","txt"],"rows":[[-9223372036854775808,0,9223372036854775807,"café \"\\\n\r\t\u0001\u001f 🚀"]]}"#
        );
        assert_eq!(meter.usage().memory_bytes, 0);

        // 2. Large (> 8 KiB) JSON output exercises 4 KiB credit-pool batching & releases cleanly.
        let big_rows: Vec<Vec<SqlValue>> = (0..300)
            .map(|i| vec![SqlValue::Int(i), SqlValue::text("abcdefghijklmnopqrstuvwxyz012345")])
            .collect();
        let big_res = QueryResult {
            columns: vec!["id".into(), "s".into()],
            rows: big_rows,
        };
        let big_json = format_query_result_json_metered(&big_res, &meter).unwrap();
        assert!(big_json.len() > 8192);
        assert_eq!(meter.usage().memory_bytes, 0);

        // 3. Invalid UTF-8 bytes are pre-reserved before lossy conversion under tight memory cap.
        let tight = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100_000),
            max_memory_bytes: Some(128),
            max_page_reads: None,
        });
        let bad_utf8 = QueryResult {
            columns: vec!["b".into()],
            rows: vec![vec![SqlValue::Text(vec![0xff; 256].into_boxed_slice())]],
        };
        assert!(format_query_result_json_metered(&bad_utf8, &tight).is_err());
        assert_eq!(tight.usage().memory_bytes, 0);

        // 4. reserve_params_budget accounts for 2x raw JSON length and releases cleanly on drop.
        let pm = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100_000),
            max_memory_bytes: Some(1024),
            max_page_reads: None,
        });
        {
            let _g = reserve_params_budget(&pm, "[1,2,3,4,5]").unwrap();
            assert_eq!(pm.usage().memory_bytes, 22);
        }
        assert_eq!(pm.usage().memory_bytes, 0);
    }

    #[test]
    fn pass4_wasm_streaming_utf8_credit_flush_and_dense_params_metering() {
        // 1. Streaming lossy UTF-8 encodes mixed valid + invalid bytes + escapes identically.
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let mixed: Vec<u8> = vec![b'a', b'"', 0xff, b'\n', 0xc3, 0xa9, 0x80, b'z'];
        let res = QueryResult {
            columns: vec!["x".into()],
            rows: vec![vec![SqlValue::Text(mixed.into_boxed_slice())]],
        };
        let json = format_query_result_json_metered(&res, &meter).unwrap();
        assert_eq!(json, "{\"columns\":[\"x\"],\"rows\":[[\"a\\\"\u{FFFD}\\né\u{FFFD}z\"]]}");
        assert_eq!(meter.usage().memory_bytes, 0);

        // 2. Credit-pool flush_credit() returns unused chunk credit so concurrent allocations succeed.
        let cm = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100_000),
            max_memory_bytes: Some(JSON_WRITER_CHUNK * 5),
            max_page_reads: None,
        });
        {
            let mut w = MeteredJsonWriter::new(&cm);
            w.push_str("hello").unwrap();
            assert!(cm.usage().memory_bytes >= JSON_WRITER_CHUNK);
            w.flush_credit();
            assert_eq!(cm.usage().memory_bytes, 5);
        }
        assert_eq!(cm.usage().memory_bytes, 0);

        // 3. Dense JSON array params (`[0,0,...]`) fail cleanly under a tight memory limit.
        let dense = format!("[{}]", vec!["0"; 4000].join(","));
        let tight = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(16 * 1024),
            max_page_reads: None,
        });
        assert!(parse_params_json(&dense, &tight).is_err());
        assert_eq!(tight.usage().memory_bytes, 0);
    }

    #[test]
    fn pass5_wasm_params_real_buf_and_sql_budget() {
        // 1. Real formatting via stack `real_to_text_buf` produces valid JSON for finite & non-finite floats.
        let meter = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let res = QueryResult {
            columns: vec!["z".into(), "f".into(), "inf".into(), "nan".into()],
            rows: vec![vec![
                SqlValue::Real(0.0),
                SqlValue::Real(1.25),
                SqlValue::Real(f64::INFINITY),
                SqlValue::Real(f64::NAN),
            ]],
        };
        let json = format_query_result_json_metered(&res, &meter).unwrap();
        assert_eq!(json, r#"{"columns":["z","f","inf","nan"],"rows":[[0.0,1.25,null,null]]}"#);
        assert_eq!(meter.usage().memory_bytes, 0);

        // 2. Dense JSON object params reserve actual representation bytes and release cleanly.
        let dense_obj = format!(
            "{{{}}}",
            (0..500)
                .map(|i| format!(r#""k{i}":1"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let tight = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(8 * 1024),
            max_page_reads: None,
        });
        assert!(parse_params_json(&dense_obj, &tight).is_err());
        assert_eq!(tight.usage().memory_bytes, 0);

        // 3. `MemGuard::add` accumulates & releases cleanly, and `maybe_flush_credit` only flushes
        //    under tight budgets.
        let tight_m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(JSON_WRITER_CHUNK * 4 + 512),
            max_page_reads: None,
        });
        {
            let mut g = MemGuard::reserve(&tight_m, 128).unwrap();
            g.add(256).unwrap();
            assert_eq!(tight_m.usage().memory_bytes, 384);
            let mut w = MeteredJsonWriter::new(&tight_m);
            w.push_str("hi").unwrap();
            assert!(tight_m.usage().memory_bytes >= 384 + JSON_WRITER_CHUNK);
            w.maybe_flush_credit();
            assert_eq!(tight_m.usage().memory_bytes, 384 + 2);
        }
        assert_eq!(tight_m.usage().memory_bytes, 0);

        // `format_exec_result_json_metered` formats Real/Int/Text cleanly and releases memory.
        let exec_res = ExecResult {
            columns: vec!["x".into()],
            rows: vec![vec![SqlValue::Real(-0.5)]],
            changes: 1,
            last_insert_rowid: 7,
        };
        let exec_json = format_exec_result_json_metered(&exec_res, &meter).unwrap();
        assert_eq!(
            exec_json,
            r#"{"columns":["x"],"rows":[[-0.5]],"changes":1,"lastInsertRowid":7}"#
        );
        assert_eq!(meter.usage().memory_bytes, 0);
    }

    #[test]
    fn pass6_wasm_whitespace_params_step_flush_and_formatters() {
        // 1. Whitespace-only huge `params_json` is bounded by `reserve_params_budget` BEFORE trimming.
        let tight = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024),
            max_page_reads: None,
        });
        let huge_ws = " ".repeat(4096);
        assert!(reserve_params_budget(&tight, &huge_ws).is_err());
        assert_eq!(tight.usage().memory_bytes, 0);

        // 2. `MeteredJsonWriter::into_string` flushes sub-64-byte pending steps (`div_ceil(64)`).
        let sm = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let mut w = MeteredJsonWriter::new(&sm);
        w.push_str("ok").unwrap();
        assert_eq!(w.into_string().unwrap(), "ok");
        assert!(sm.usage().steps >= 1, "into_string must flush sub-64-byte pending steps");
        assert_eq!(sm.usage().memory_bytes, 0);

        // 3. Fast single-pass `push_json_string`, chunked `Blob` hex (>64B), and 2-digit int formatter.
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let mut buf = [0u8; 21];
        for n in [i64::MIN, -1000, -99, -10, -9, -1, 0, 1, 9, 10, 99, 100, 9999, i64::MAX] {
            assert_eq!(format_i64_stack(n, &mut buf), n.to_string());
        }
        for n in [0u64, 1, 9, 10, 99, 100, 1234567890, u64::MAX] {
            assert_eq!(format_u64_stack(n, &mut buf), n.to_string());
        }
        let big_blob: Vec<u8> = (0..130u8).collect();
        let expected_hex: String = big_blob.iter().map(|b| format!("{b:02x}")).collect();
        let res = QueryResult {
            columns: vec!["plain".into(), "esc".into(), "blob".into()],
            rows: vec![vec![
                SqlValue::text("simple_ascii_123"),
                SqlValue::text("a\"b\\c\nd"),
                SqlValue::Blob(big_blob.into_boxed_slice()),
            ]],
        };
        let json = format_query_result_json_metered(&res, &m).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"columns":["plain","esc","blob"],"rows":[["simple_ascii_123","a\"b\\c\nd",{{"hex":"{expected_hex}"}}]]}}"#
            )
        );
        assert_eq!(m.usage().memory_bytes, 0);

        // 4. `open_database_from_owned_bytes` loads an existing SQLite image cleanly.
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute("CREATE TABLE t(a INT); INSERT INTO t VALUES (42);").unwrap();
        let bytes = conn.to_bytes();
        let db = xdb_sqlite::open_database_from_owned_bytes(
            bytes,
            None,
            None,
            xdb_sqlite::OpenOptions::default(),
        )
        .unwrap();
        let qr = db.query("SELECT a FROM t").unwrap();
        assert_eq!(qr.rows.len(), 1);
        assert!(qr.rows[0][0].exact_eq(&SqlValue::Int(42)));
    }

    #[test]
    fn pass7_wasm_step_limits_and_fast_json_writer() {
        // 1. `MeteredJsonWriter` plain-string fast check, `push_byte`, and `memory_bytes()` cleanup.
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let mut w = MeteredJsonWriter::new(&m);
        w.push_byte(b'[').unwrap();
        w.push_json_string("plain_ascii_0123456789_abcdef").unwrap();
        w.push_byte(b',').unwrap();
        w.push_json_string("has \"quote\" and \\slash\\ and \x01ctrl").unwrap();
        w.push_byte(b']').unwrap();
        assert_eq!(
            w.into_string().unwrap(),
            r#"["plain_ascii_0123456789_abcdef","has \"quote\" and \\slash\\ and \u0001ctrl"]"#
        );
        assert_eq!(m.memory_bytes(), 0);

        // 2. Step limit enforced on huge SQL input via `BudgetMeter` (matches `run_query_json` / `execute_json`).
        let tight_steps = std::sync::Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(10),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        }));
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
            meter: Some(tight_steps.clone()),
            ..ConnectionOptions::default()
        })
        .unwrap();
        let huge_sql = format!("SELECT 1 {}", " ".repeat(2000));
        assert!(conn.execute(&huge_sql).is_err());

        // 3. `parse_params_json_guarded` batched 4 KiB memory accounting for named & positional params.
        let pm = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        });
        let named_json = r#"{":a":1,":b":"hello",":c":true,":d":null}"#;
        let mut guard = MemGuard::reserve(&pm, named_json.len() as u64).unwrap();
        let params = parse_params_json_guarded(named_json, &pm, &mut guard).unwrap();
        assert!(params.resolve(0, ":a").exact_eq(&SqlValue::Int(1)));
        assert!(params.resolve(0, ":b").exact_eq(&SqlValue::text("hello")));
        assert!(params.resolve(0, ":c").exact_eq(&SqlValue::Int(1)));
        assert!(params.resolve(0, ":d").is_null());
        drop(guard);
        assert_eq!(pm.memory_bytes(), 0);

        let tight_pm = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(512),
            max_page_reads: None,
        });
        let mut big_named = String::from("{");
        for i in 0..100 {
            if i > 0 {
                big_named.push(',');
            }
            big_named.push_str(&format!("\":k{i}\":\"{}\"", "v".repeat(20)));
        }
        big_named.push('}');
        let mut tguard = MemGuard::reserve(&tight_pm, 0).unwrap();
        assert!(parse_params_json_guarded(&big_named, &tight_pm, &mut tguard).is_err());
        drop(tguard);
        assert_eq!(tight_pm.memory_bytes(), 0);
    }

    #[test]
    fn pass8_wasm_name_metering_peak_memory_json_keys_and_drop_flush() {
        // 1. `charge_name_budget` rejects oversized nonexistent table/index names under tiny meter.
        let tiny = BudgetMeter::new(ResourceLimits {
            max_steps: Some(4),
            max_memory_bytes: Some(128),
            max_page_reads: None,
        });
        let huge_name = "x".repeat(4096);
        assert!(charge_name_budget(&tiny, &huge_name).is_err());
        assert_eq!(tiny.memory_bytes(), 0);

        // 2. `MeteredJsonWriter` Drop flushes pending un-charged steps (< 64).
        let step_m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100),
            max_memory_bytes: Some(64 * 1024),
            max_page_reads: None,
        });
        {
            let mut w = MeteredJsonWriter::new(&step_m);
            w.push_json_string("plain_ascii_hello_world").unwrap();
            w.push_json_string("escaped\n\"quote\"\\tab\t\x01").unwrap();
            let s = w.into_string().unwrap();
            assert_eq!(
                s,
                "\"plain_ascii_hello_world\"\"escaped\\n\\\"quote\\\"\\\\tab\\t\\u0001\""
            );
        }
        assert!(step_m.usage().steps > 0, "Drop must flush pending steps");
        assert_eq!(step_m.memory_bytes(), 0);

        // 3. `format_query_result_json_metered` & `format_exec_result_json_metered` honor tight
        //    memory limits and release all reservations cleanly.
        let tight_fmt = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(256),
            max_page_reads: None,
        });
        let big_qr = xdb_sqlite::QueryResult {
            columns: vec!["s".to_string()],
            rows: vec![vec![SqlValue::text("a".repeat(1024))]],
        };
        assert!(format_query_result_json_metered(&big_qr, &tight_fmt).is_err());
        assert_eq!(tight_fmt.memory_bytes(), 0);

        let tight_exec = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(256),
            max_page_reads: None,
        });
        let big_er = xdb_sqlite::ExecResult {
            columns: vec!["s".to_string()],
            rows: vec![vec![SqlValue::text("b".repeat(1024))]],
            changes: 1,
            last_insert_rowid: 1,
        };
        assert!(format_exec_result_json_metered(&big_er, &tight_exec).is_err());
        assert_eq!(tight_exec.memory_bytes(), 0);

        // 4. `parse_params_json_guarded` rejects oversized hex blob param before allocation.
        let tight_hex = BudgetMeter::new(ResourceLimits {
            max_steps: Some(1_000_000),
            max_memory_bytes: Some(512),
            max_page_reads: None,
        });
        let big_hex = format!(r#"[{{"hex":"{}"}}]"#, "ab".repeat(2000));
        let mut hguard = MemGuard::reserve(&tight_hex, 0).unwrap();
        assert!(parse_params_json_guarded(&big_hex, &tight_hex, &mut hguard).is_err());
        drop(hguard);
        assert_eq!(tight_hex.memory_bytes(), 0);
    }

    #[test]
    fn pass9_wasm_reserve_extra_credit_scan_keys_precheck_open_owned_and_swar_escape() {
        // 1. `MeteredJsonWriter::reserve_extra` keeps `capacity - len >= credit` across repeated writes.
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100_000),
            max_memory_bytes: Some(128 * 1024),
            max_page_reads: None,
        });
        {
            let mut w = MeteredJsonWriter::new(&m);
            for i in 0..200 {
                w.push_json_string(&format!("row_payload_number_{i}_abcdefghij"))
                    .unwrap();
                assert!(
                    (w.out.capacity() - w.out.len()) as u64 >= w.credit,
                    "spare capacity must cover remaining credit"
                );
            }
            let out = w.into_string().unwrap();
            assert!(out.len() > 6000);
        }
        assert_eq!(m.memory_bytes(), 0);

        // 2. SWAR `find_first_json_escape` handles plain >8-byte ASCII, non-ASCII UTF-8, quotes,
        //    backslashes, and every control char `< 0x20` at every lane offset 0..15!
        let plain = "0123456789abcdef_UTF8_\u{00E9}\u{1F680}_tail";
        assert_eq!(find_first_json_escape(plain.as_bytes()), None);
        for lane in 0..16 {
            for special in [b'"', b'\\', b'\n', b'\r', b'\t', 0x00, 0x01, 0x1F] {
                let mut buf = vec![b'a'; 24];
                buf[lane] = special;
                assert_eq!(
                    find_first_json_escape(&buf),
                    Some(lane),
                    "SWAR escape mismatch at lane {lane} for byte 0x{special:02X}"
                );
            }
        }

        // 3. `WasmDatabase::open` takes owned `Vec<u8>` directly and enforces `scan_table_json`
        //    pre-allocation key metering without leaking reservations.
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        let long_col = "c".repeat(200);
        conn.execute(&format!(
            "CREATE TABLE t (\"{long_col}\" TEXT); INSERT INTO t VALUES ('ok');"
        ))
        .unwrap();
        let bytes = conn.to_bytes();
        let db = WasmDatabase::open(bytes, None, None, 100_000, 128 * 1024, 1000).unwrap();
        let scanned = db.scan_table_json("t", 10).unwrap();
        assert!(scanned.contains(&long_col));
        assert!(scanned.contains("\"ok\""));
        db.close().unwrap();
    }

    #[test]
    fn pass10_wasm_conn_owned_open_metadata_parity_and_get_row_metering() {
        let mut src = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        src.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE INDEX ix_items_name ON items(name);
             CREATE VIEW v_items AS SELECT id, name FROM items;
             INSERT INTO items VALUES (1, 'alpha'), (2, 'beta');",
        )
        .unwrap();
        let bytes = src.to_bytes();

        // 1. `WasmConnection::open` takes owned `Vec<u8>` and exposes `encoding`, `catalog_json`, `schema_json`!
        let mut wconn = WasmConnection::open(
            Some(bytes.clone()),
            None,
            None,
            0,
            100_000,
            256 * 1024,
            1000,
        )
        .unwrap();
        assert_eq!(wconn.encoding(), "UTF-8");
        let cat = wconn.catalog_json().unwrap();
        assert!(cat.contains("\"items\""));
        assert!(cat.contains("\"ix_items_name\""));
        assert!(cat.contains("\"v_items\""));
        let sch = wconn.schema_json().unwrap();
        assert!(sch.contains("CREATE TABLE items"));
        wconn.close().unwrap();

        // 2. `WasmDatabase::get_row_json` meters memory & steps cleanly without leaking reservations!
        let db = WasmDatabase::open(bytes, None, None, 100_000, 256 * 1024, 1000).unwrap();
        let row2 = db.get_row_json("items", 2).unwrap();
        assert!(row2.contains("\"beta\""));
        let miss = db.get_row_json("items", 999).unwrap();
        assert_eq!(miss, "null");
        db.close().unwrap();
    }
}
