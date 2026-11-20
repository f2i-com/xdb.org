//! Storage-agnostic read catalog traits, query parameters/options/results, and an
//! in-memory catalog (`MemoryCatalog`) for standalone SQL execution and testing.

use std::collections::HashMap;
use std::sync::Arc;

use xdb_core::value::CollationRegistry;
use xdb_core::{Affinity, Meter, SqlValue};
use xdb_parser::Dialect;

use crate::errors::{SqlError, SqlResult};
use crate::functions::{current_julian_ms, FunctionLimits, FunctionRegistry, TableValuedResult};

/// Column metadata exposed by a [`SqlCatalog`] table.
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub affinity: Affinity,
    pub collation: String,
    /// True for `GENERATED ALWAYS AS (...) VIRTUAL` or `STORED` columns.
    pub is_generated: bool,
    /// True when the column is hidden from `SELECT *` (SQLite hidden columns).
    pub is_hidden: bool,
}

impl ColumnMeta {
    pub fn simple(name: impl Into<String>, affinity: Affinity) -> Self {
        Self {
            name: name.into(),
            affinity,
            collation: "BINARY".to_string(),
            is_generated: false,
            is_hidden: false,
        }
    }
}

/// Secondary index metadata exposed to the query planner for optional index seeks.
#[derive(Debug, Clone)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    /// Table column indices covered by the index keys (`None` for expression keys).
    pub key_columns: Vec<Option<usize>>,
    /// Collation name for each key column.
    pub key_collations: Vec<String>,
    /// True if descending (`DESC`) for each key column.
    pub key_desc: Vec<bool>,
    /// True if this is a partial index (`WHERE ...`) — planner skips pushdown unless matched.
    pub partial: bool,
}

/// Table metadata exposed by a [`SqlCatalog`].
#[derive(Debug, Clone)]
pub struct TableMeta {
    pub name: String,
    pub columns: Vec<ColumnMeta>,
    /// True for normal rowid tables (`WITHOUT ROWID` tables have `has_rowid = false`).
    pub has_rowid: bool,
    /// Column index of the `INTEGER PRIMARY KEY` alias for `rowid`, if any.
    pub ipk_column: Option<usize>,
    /// Secondary indexes available on this table.
    pub indexes: Vec<IndexMeta>,
}

/// View metadata exposed by a [`SqlCatalog`].
#[derive(Debug, Clone)]
pub struct ViewMeta {
    pub name: String,
    /// The `SELECT` SQL defining the view (full `CREATE VIEW ... AS <select>` or raw `<select>`).
    pub select_sql: String,
    /// Optional explicit view column names from `CREATE VIEW v(c1, c2) AS ...`.
    pub columns: Option<Vec<String>>,
}

/// Range bound for rowid or secondary-index pushdown.
#[derive(Debug, Clone)]
pub enum Bound {
    Unbounded,
    Included(SqlValue),
    Excluded(SqlValue),
}

/// Optional access hint produced by the planner from simple `WHERE` predicates.
#[derive(Debug, Clone, Default)]
pub enum TableScanHint {
    #[default]
    FullScan,
    /// Rowid point lookup or range (`rowid = ?`, `rowid >= ? AND rowid <= ?`).
    RowidRange {
        low: Bound,
        high: Bound,
    },
    /// Secondary index equality prefix on `(index_name, leading key values)`.
    IndexEq {
        index_name: String,
        values: Vec<SqlValue>,
    },
}

/// One scanned row returned by [`SqlCatalog::scan_table`].
#[derive(Debug, Clone)]
pub struct ScannedRow {
    pub rowid: Option<i64>,
    pub values: Vec<SqlValue>,
}

/// Read-only storage catalog trait implemented by `xdb-sqlite` (or [`MemoryCatalog`]).
pub trait SqlCatalog {
    /// Look up a table by name (ASCII case-insensitive). `schema` is `None` or e.g. `"main"`.
    fn get_table(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<TableMeta>>;

    /// Look up a view by name (ASCII case-insensitive).
    fn get_view(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<ViewMeta>> {
        let _ = (schema, name);
        Ok(None)
    }

    /// Scan rows of a table. `needed_columns` is `None` for all columns or a list of column
    /// indices that will actually be referenced (the returned `ScannedRow.values` always has
    /// length `table.columns.len()`, with unrequested columns allowed to be `SqlValue::Null`).
    fn scan_table(
        &self,
        schema: Option<&str>,
        name: &str,
        hint: &TableScanHint,
        needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>>;

    /// Resolve a catalog-provided table-valued function (e.g., `pragma_table_info('t')`).
    /// Return `Ok(None)` to fall back to built-in TVFs (`json_each`, `json_tree`, `generate_series`).
    fn call_tvf(
        &self,
        schema: Option<&str>,
        name: &str,
        args: &[SqlValue],
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Option<TableValuedResult>> {
        let _ = (schema, name, args, meter);
        Ok(None)
    }
}

/// Bound SQL query parameters (`?`, `?NNN`, `:name`, `@name`, `$name`).
#[derive(Debug, Clone, Default)]
pub struct QueryParams {
    /// 1-based positional bindings (`params[0]` is `?1`).
    pub positional: Vec<SqlValue>,
    /// Named bindings keyed by exact token text (e.g. `":id"`, `"@x"`, `"$v"`) or bare name (`"id"`).
    pub named: HashMap<String, SqlValue>,
}

impl QueryParams {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn positional(values: impl Into<Vec<SqlValue>>) -> Self {
        Self {
            positional: values.into(),
            named: HashMap::new(),
        }
    }

    pub fn with_named(mut self, name: impl Into<String>, value: SqlValue) -> Self {
        self.named.insert(name.into(), value);
        self
    }

    /// Resolve a parsed SQLite variable (`index` is 1-based, `text` is e.g. `"?"`, `"?2"`, `":foo"`).
    pub fn resolve(&self, index: usize, text: &str) -> SqlValue {
        if !text.is_empty() && !text.starts_with('?') {
            if let Some(v) = self.named.get(text) {
                return v.clone();
            }
            if let Some(bare) = text.get(1..) {
                if let Some(v) = self.named.get(bare) {
                    return v.clone();
                }
            }
        }
        if index >= 1 {
            if let Some(v) = self.positional.get(index - 1) {
                return v.clone();
            }
        }
        SqlValue::Null
    }
}

/// Execution options for SQL queries.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub dialect: Dialect,
    pub limits: FunctionLimits,
    pub functions: FunctionRegistry,
    pub collations: CollationRegistry,
    pub meter: Option<Arc<dyn Meter>>,
    /// Statement UTC timestamp in Julian-day milliseconds (`None` = current time / deterministic WASM epoch).
    pub now_julian_ms: Option<i64>,
    /// Deterministic local timezone offset in minutes for `'localtime'` / `'utc'`.
    pub local_tz_offset_minutes: i32,
    /// Double-quoted string literal fallback (SQLite `DQS_DML`, false by default).
    pub dqs: bool,
    /// Case-sensitive `LIKE` (`PRAGMA case_sensitive_like`, false by default).
    pub case_sensitive_like: bool,
    /// Max recursive CTE iterations (guard against runaway recursion even without a meter).
    pub max_cte_iterations: usize,
    /// Connection counters for `changes()`, `total_changes()`, `last_insert_rowid()`.
    pub changes: i64,
    pub total_changes: i64,
    pub last_insert_rowid: i64,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            dialect: Dialect::Sqlite,
            limits: FunctionLimits::default(),
            functions: FunctionRegistry::new(),
            collations: CollationRegistry::new(xdb_core::TextEncoding::Utf8),
            meter: None,
            now_julian_ms: None,
            local_tz_offset_minutes: 0,
            dqs: false,
            case_sensitive_like: false,
            max_cte_iterations: 100_000,
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
        }
    }
}

impl QueryOptions {
    pub fn effective_now_julian_ms(&self) -> i64 {
        self.now_julian_ms.unwrap_or_else(current_julian_ms)
    }
}

/// Tabular output of a `SELECT` query.
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

/// Simple in-memory [`SqlCatalog`] for standalone SQL queries and tests.
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    tables: HashMap<String, (TableMeta, Vec<ScannedRow>)>,
    views: HashMap<String, ViewMeta>,
}

impl MemoryCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_table(
        &mut self,
        name: impl Into<String>,
        columns: Vec<ColumnMeta>,
        rows: Vec<Vec<SqlValue>>,
    ) {
        let name = name.into();
        let meta = TableMeta {
            name: name.clone(),
            columns,
            has_rowid: true,
            ipk_column: None,
            indexes: Vec::new(),
        };
        let scanned = rows
            .into_iter()
            .enumerate()
            .map(|(i, values)| ScannedRow {
                rowid: Some((i + 1) as i64),
                values,
            })
            .collect();
        self.tables.insert(name.to_ascii_lowercase(), (meta, scanned));
    }

    pub fn add_table_with_meta(&mut self, meta: TableMeta, rows: Vec<ScannedRow>) {
        self.tables
            .insert(meta.name.to_ascii_lowercase(), (meta, rows));
    }

    pub fn add_view(
        &mut self,
        name: impl Into<String>,
        select_sql: impl Into<String>,
        columns: Option<Vec<String>>,
    ) {
        let name = name.into();
        self.views.insert(
            name.to_ascii_lowercase(),
            ViewMeta {
                name,
                select_sql: select_sql.into(),
                columns,
            },
        );
    }
}

impl SqlCatalog for MemoryCatalog {
    fn get_table(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<TableMeta>> {
        if let Some(s) = schema {
            if !s.eq_ignore_ascii_case("main") && !s.eq_ignore_ascii_case("temp") {
                return Ok(None);
            }
        }
        Ok(self
            .tables
            .get(&name.to_ascii_lowercase())
            .map(|(m, _)| m.clone()))
    }

    fn get_view(&self, schema: Option<&str>, name: &str) -> SqlResult<Option<ViewMeta>> {
        if let Some(s) = schema {
            if !s.eq_ignore_ascii_case("main") && !s.eq_ignore_ascii_case("temp") {
                return Ok(None);
            }
        }
        Ok(self.views.get(&name.to_ascii_lowercase()).cloned())
    }

    fn scan_table(
        &self,
        _schema: Option<&str>,
        name: &str,
        _hint: &TableScanHint,
        _needed_columns: Option<&[usize]>,
        meter: Option<&dyn Meter>,
    ) -> SqlResult<Vec<ScannedRow>> {
        let Some((_, rows)) = self.tables.get(&name.to_ascii_lowercase()) else {
            return Err(SqlError::runtime(format!("no such table: {name}")));
        };
        if let Some(m) = meter {
            m.charge_steps(rows.len() as u64 + 1)?;
        }
        Ok(rows.clone())
    }
}
