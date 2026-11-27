//! SQLite 3.54.1 expression evaluator with exact affinity, collation propagation,
//! row-value vector comparisons, correlated subqueries, and `VIRTUAL` generated column support.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use xdb_core::value::{
    affinity_of_decl_type, apply_affinity, apply_comparison_affinity, arith_enc, bit_not_enc,
    bitwise_enc, boolean_value_enc, cast_to_type, compare_op, comparison_affinity,
    comparison_collation_name, in_affinity, integer_literal_value, mem_compare, negate_enc,
    real_literal_value, real_to_exact_i64, ArithOp, BitOp, BuiltinCollation, Collation, CompareOp,
    OperandCollation,
};
use xdb_core::{Affinity, SqlValue, TextEncoding};
use xdb_parser::{
    BinaryOp, ColumnRef, CurrentTimeKeyword, Expr, LikeOperator, QuoteStyle, SelectStmt, UnaryOp,
};

use crate::catalog::{QueryOptions, QueryParams, SqlCatalog};
use crate::errors::{SqlError, SqlResult};
use crate::functions::{
    date::{date_named, datetime_named, time_named},
    json::eval_json_arrow_metered,
    like_invoke, FunctionContext, GLOB_INFO, LIKE_CASE_SENSITIVE_INFO, LIKE_INFO,
};
use crate::utf8::{text_bytes_of, MemoryGuard};

/// One column binding inside an evaluation row scope.
#[derive(Debug, Clone)]
pub struct ScopeColumn {
    /// Table alias (or table name) that owns this column; `None` for anonymous expressions.
    pub table: Option<String>,
    /// Column name.
    pub name: String,
    pub affinity: Affinity,
    pub collation: String,
    /// True if this column is a hidden rowid alias (`rowid`, `_rowid_`, `oid`).
    pub is_rowid_alias: bool,
}

/// Maximum recursive expression evaluation depth to prevent host stack overflow.
const MAX_EVAL_DEPTH: u32 = 1000;

/// Evaluation scope for one query level (a table row, join row, group row, or outer correlated row).
#[derive(Debug, Clone)]
pub struct EvalScope<'a> {
    pub columns: &'a [ScopeColumn],
    pub values: &'a [SqlValue],
    /// Pre-computed aggregate/window expression results keyed by AST byte span `(start, end)`.
    pub precomputed: Option<&'a [(usize, usize, SqlValue)]>,
    /// Optional parallel JSON-subtype flags for `precomputed` entries.
    pub precomputed_json: Option<&'a [bool]>,
    /// Optional per-column flag hiding duplicate `USING`/`NATURAL JOIN` columns from unqualified lookup.
    pub hidden_unqualified: Option<&'a [bool]>,
    /// Optional per-column source-table ID; columns from the same source ID do not conflict
    /// (e.g. `SELECT x FROM (SELECT 1 AS x, 2 AS x)`), while columns with the same alias from
    /// distinct sources (`SELECT t.x FROM t JOIN t`) are reported as ambiguous.
    pub source_ids: Option<&'a [u32]>,
    /// Optional alias scope (e.g., `SELECT` result aliases visible to `GROUP BY` / `HAVING` / `ORDER BY`).
    pub aliases: Option<&'a [(String, SqlValue, Affinity, String)]>,
    /// True for generated column evaluation (disallows `rowid`/`_rowid_`/`oid` unless real column).
    pub no_rowid_aliases: bool,
}

impl<'a> EvalScope<'a> {
    pub fn simple(columns: &'a [ScopeColumn], values: &'a [SqlValue]) -> Self {
        Self {
            columns,
            values,
            precomputed: None,
            precomputed_json: None,
            hidden_unqualified: None,
            source_ids: None,
            aliases: None,
            no_rowid_aliases: false,
        }
    }
}

/// Result of executing a subquery with column names, affinities, and rows.
pub type SelectWithAffinities = (Vec<String>, Vec<Option<Affinity>>, Vec<Vec<SqlValue>>);

/// Result of scanning an `IN table` target with column affinities and rows.
pub type TableRowsWithAffinities = (Vec<Option<Affinity>>, Vec<Vec<SqlValue>>);

/// Result of executing a subquery with column names, affinities, collations, and rows.
pub type SelectWithMeta = (
    Vec<String>,
    Vec<Option<Affinity>>,
    Vec<ExprCollation>,
    Vec<Vec<SqlValue>>,
);

/// Result of scanning an `IN table` target with column affinities, collations, and rows.
pub type TableRowsWithMeta = (Vec<Option<Affinity>>, Vec<ExprCollation>, Vec<Vec<SqlValue>>);

/// Result of scanning a single-column `IN (SELECT col FROM table)` subquery directly.
pub type OneColInScan = (Option<Affinity>, ExprCollation, Vec<SqlValue>);

/// Callback invoked by the expression evaluator to execute subqueries (`EXISTS`, scalar, `IN (SELECT ...)`).
pub trait SubqueryRunner {
    fn run_select(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Vec<SqlValue>>)>;

    fn run_select_with_affinities(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectWithAffinities> {
        let (cols, rows) = self.run_select(stmt, outer_scopes)?;
        let affs = vec![None; cols.len()];
        Ok((cols, affs, rows))
    }

    fn run_select_with_meta(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectWithMeta> {
        let (cols, affs, rows) = self.run_select_with_affinities(stmt, outer_scopes)?;
        let colls = vec![ExprCollation::none(); cols.len()];
        Ok((cols, affs, colls, rows))
    }

    fn run_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>>;

    fn run_in_table_with_affinities(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<TableRowsWithAffinities> {
        let rows = self.run_in_table(schema, table, args)?;
        let width = rows.first().map_or(0, Vec::len);
        Ok((vec![None; width], rows))
    }

    fn run_in_table_with_meta(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<TableRowsWithMeta> {
        let (affs, rows) = self.run_in_table_with_affinities(schema, table, args)?;
        let colls = vec![ExprCollation::none(); affs.len()];
        Ok((affs, colls, rows))
    }

    /// Returns `true` if `stmt` is guaranteed deterministic and uncorrelated with `outer_scopes`
    /// and executes against an immutable snapshot for the lifetime of the current `EvalEnv`.
    fn is_select_uncorrelated(&self, _stmt: &SelectStmt, _outer_scopes: &[EvalScope<'_>]) -> bool {
        false
    }

    /// Returns `true` if parameterless `IN table` scans execute against an immutable snapshot
    /// for the lifetime of the current `EvalEnv`.
    fn is_in_table_cacheable(&self) -> bool {
        false
    }

    /// Returns `true` if a specific parameterless `IN [schema.]table` target can be safely cached
    /// for the lifetime of the current `EvalEnv` (e.g. returns `false` for non-deterministic views).
    fn is_in_table_target_cacheable(&self, _schema: Option<&str>, _table: &str) -> bool {
        self.is_in_table_cacheable()
    }

    /// Optional fast path for simple 1-column `IN (SELECT col FROM table)` subqueries:
    /// scans the single column directly into a flat `Vec<SqlValue>` without materializing
    /// `Vec<Vec<SqlValue>>` or `OrderEvalContext`. Return `Ok(None)` to use `run_select_with_meta`.
    fn try_scan_1col_in_select(&self, _stmt: &SelectStmt) -> SqlResult<Option<OneColInScan>> {
        Ok(None)
    }
}

/// No-op subquery runner used for standalone expressions and `VIRTUAL` generated columns.
#[derive(Debug, Default)]
pub struct NoSubqueries;

impl SubqueryRunner for NoSubqueries {
    fn run_select(
        &self,
        _stmt: &SelectStmt,
        _outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Vec<SqlValue>>)> {
        Err(SqlError::runtime(
            "subqueries are not allowed in this context",
        ))
    }

    fn run_in_table(
        &self,
        _schema: Option<&str>,
        _table: &str,
        _args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        Err(SqlError::runtime(
            "table references are not allowed in this context",
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CollKey {
    None,
    Binary,
    Nocase,
    Rtrim,
    Custom(String),
}

impl CollKey {
    #[inline]
    fn from_opt(name: Option<&str>) -> Self {
        match name {
            None => CollKey::None,
            Some(s) if s.eq_ignore_ascii_case("BINARY") => CollKey::Binary,
            Some(s) if s.eq_ignore_ascii_case("NOCASE") => CollKey::Nocase,
            Some(s) if s.eq_ignore_ascii_case("RTRIM") => CollKey::Rtrim,
            Some(s) => CollKey::Custom(s.to_ascii_uppercase()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum InHashKey {
    Int(i64),
    Real(u64),
    Text(Box<[u8]>),
    Blob(Box<[u8]>),
}

impl InHashKey {
    fn from_value(v: &SqlValue, coll: BuiltinCollation) -> Option<Self> {
        match v {
            SqlValue::Null => None,
            SqlValue::Int(i) => Some(InHashKey::Int(*i)),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    None
                } else if let Some(i) = real_to_exact_i64(*r) {
                    Some(InHashKey::Int(i))
                } else {
                    Some(InHashKey::Real(if *r == 0.0 {
                        0.0f64.to_bits()
                    } else {
                        r.to_bits()
                    }))
                }
            }
            SqlValue::Text(b) => match coll {
                BuiltinCollation::Binary => Some(InHashKey::Text(b.clone())),
                BuiltinCollation::Nocase => {
                    let folded: Vec<u8> = b.iter().map(|c| c.to_ascii_lowercase()).collect();
                    Some(InHashKey::Text(folded.into_boxed_slice()))
                }
                BuiltinCollation::Rtrim => {
                    let mut n = b.len();
                    while n > 0 && b[n - 1] == b' ' {
                        n -= 1;
                    }
                    Some(InHashKey::Text(Box::from(&b[..n])))
                }
            },
            SqlValue::Blob(b) => Some(InHashKey::Blob(b.clone())),
        }
    }
}

enum InLookupIndex {
    /// Direct O(1) bitset for dense integer sets (`max - min <= 65535`): zero hashing, DoS-proof.
    IntBitset {
        min: i64,
        max: i64,
        bits: Rc<[u64]>,
    },
    /// Compact sorted `i64` slice for sparse integer sets (L1 binary search, zero hashing).
    IntSorted(Box<[i64]>),
    Hashed {
        coll: BuiltinCollation,
        set: HashSet<InHashKey>,
    },
    Sorted(Vec<SqlValue>),
}

struct SortedInLookup<'a> {
    index: InLookupIndex,
    has_null: bool,
    _mem: MemoryGuard<'a>,
}

type SortedIn1ColCache<'a> = Vec<((Affinity, CollKey), Rc<SortedInLookup<'a>>)>;

struct CachedSelect<'a> {
    cols: Vec<String>,
    affs: Vec<Option<Affinity>>,
    colls: Vec<ExprCollation>,
    rows: Vec<Vec<SqlValue>>,
    sorted_1col: RefCell<SortedIn1ColCache<'a>>,
    _mem: MemoryGuard<'a>,
}

fn estimate_value_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Null | SqlValue::Int(_) | SqlValue::Real(_) => 24,
        SqlValue::Text(b) | SqlValue::Blob(b) => 24u64.saturating_add(b.len() as u64),
    }
}

fn estimate_rows_bytes(rows: &[Vec<SqlValue>]) -> u64 {
    let mut total = (rows.len() as u64).saturating_mul(24);
    for r in rows {
        for v in r {
            total = total.saturating_add(estimate_value_bytes(v));
        }
    }
    total
}

type SubqueryCacheVec<'a> = Vec<((usize, usize, usize), Option<Rc<CachedSelect<'a>>>)>;
type InTableCacheMap<'a> = HashMap<(Option<String>, String), Rc<CachedSelect<'a>>>;

/// Shared execution environment for expression evaluation.
pub struct EvalEnv<'a> {
    pub options: &'a QueryOptions,
    pub params: &'a QueryParams,
    pub fn_ctx: FunctionContext<'a>,
    pub subqueries: &'a dyn SubqueryRunner,
    pub dqs: bool,
    cache_subqueries: bool,
    eval_depth: Cell<u32>,
    subquery_cache: RefCell<SubqueryCacheVec<'a>>,
    in_table_cache: RefCell<InTableCacheMap<'a>>,
}

struct EvalDepthGuard<'a>(&'a Cell<u32>);

impl Drop for EvalDepthGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl std::fmt::Debug for EvalEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalEnv")
            .field("dqs", &self.dqs)
            .field("fn_ctx", &self.fn_ctx)
            .finish()
    }
}

impl<'a> EvalEnv<'a> {
    pub fn new(
        options: &'a QueryOptions,
        params: &'a QueryParams,
        subqueries: &'a dyn SubqueryRunner,
    ) -> Self {
        Self::new_with_now(options, params, subqueries, options.effective_now_julian_ms())
    }

    pub(crate) fn new_with_now(
        options: &'a QueryOptions,
        params: &'a QueryParams,
        subqueries: &'a dyn SubqueryRunner,
        now_julian_ms: i64,
    ) -> Self {
        let mut fn_ctx = FunctionContext::new(options.meter.as_deref(), now_julian_ms);
        fn_ctx.limits = options.limits;
        fn_ctx.local_tz_offset_minutes = options.local_tz_offset_minutes;
        fn_ctx.changes = options.changes;
        fn_ctx.total_changes = options.total_changes;
        fn_ctx.last_insert_rowid = options.last_insert_rowid;
        fn_ctx.case_sensitive_like = options.case_sensitive_like;
        Self {
            options,
            params,
            fn_ctx,
            subqueries,
            dqs: options.dqs,
            cache_subqueries: false,
            eval_depth: Cell::new(0),
            subquery_cache: RefCell::new(Vec::new()),
            in_table_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Pin `'now'` timestamp (`now_julian_ms`) so all evaluations within a query observe the
    /// exact same clock instant.
    pub fn with_now_julian_ms(mut self, now_julian_ms: i64) -> Self {
        self.fn_ctx.now_julian_ms = now_julian_ms;
        self
    }

    /// Enable pointer-keyed uncorrelated subquery caching. Only safe when every `SelectStmt`
    /// evaluated against this `EvalEnv` remains alive at its address for the lifetime of `self`.
    pub fn with_subquery_cache(mut self, enabled: bool) -> Self {
        self.cache_subqueries = enabled;
        self
    }

    fn get_cached_select(
        &self,
        select: &SelectStmt,
        scopes: &[EvalScope<'_>],
    ) -> SqlResult<Option<Rc<CachedSelect<'a>>>> {
        if !self.cache_subqueries {
            return Ok(None);
        }
        let key = (
            select as *const SelectStmt as usize,
            select.span.start,
            select.span.end,
        );
        {
            let cache = self.subquery_cache.borrow();
            if let Some((_, entry)) = cache.iter().find(|(k, _)| *k == key) {
                return Ok(entry.clone());
            }
        }
        if !self.subqueries.is_select_uncorrelated(select, scopes) {
            self.subquery_cache.borrow_mut().push((key, None));
            return Ok(None);
        }
        self.get_cached_select_verified(select, key)
    }

    fn get_cached_select_verified(
        &self,
        select: &SelectStmt,
        key: (usize, usize, usize),
    ) -> SqlResult<Option<Rc<CachedSelect<'a>>>> {
        if !self.cache_subqueries {
            return Ok(None);
        }
        {
            let cache = self.subquery_cache.borrow();
            if let Some((_, entry)) = cache.iter().find(|(k, _)| *k == key) {
                return Ok(entry.clone());
            }
        }
        let (cols, affs, colls, rows) = self.subqueries.run_select_with_meta(select, &[])?;
        let mut mem = MemoryGuard::new(self.fn_ctx.meter);
        mem.reserve(estimate_rows_bytes(&rows))?;
        let cached = Rc::new(CachedSelect {
            cols,
            affs,
            colls,
            rows,
            sorted_1col: RefCell::new(Vec::new()),
            _mem: mem,
        });
        self.subquery_cache
            .borrow_mut()
            .push((key, Some(Rc::clone(&cached))));
        Ok(Some(cached))
    }

    fn get_cached_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> SqlResult<Option<Rc<CachedSelect<'a>>>> {
        if !self.subqueries.is_in_table_target_cacheable(schema, table) {
            return Ok(None);
        }
        let key = (
            schema.map(|s| s.to_ascii_lowercase()),
            table.to_ascii_lowercase(),
        );
        if let Some(entry) = self.in_table_cache.borrow().get(&key) {
            return Ok(Some(Rc::clone(entry)));
        }
        let (affs, colls, rows) = self.subqueries.run_in_table_with_meta(schema, table, None)?;
        let mut mem = MemoryGuard::new(self.fn_ctx.meter);
        mem.reserve(estimate_rows_bytes(&rows))?;
        let cached = Rc::new(CachedSelect {
            cols: Vec::new(),
            affs,
            colls,
            rows,
            sorted_1col: RefCell::new(Vec::new()),
            _mem: mem,
        });
        self.in_table_cache
            .borrow_mut()
            .insert(key, Rc::clone(&cached));
        Ok(Some(cached))
    }

    pub fn lookup_collation(&self, name: &str) -> SqlResult<&'a Collation> {
        self.options
            .collations
            .find(name)
            .ok_or_else(|| SqlError::runtime(format!("no such collation sequence: {name}")))
    }

    pub fn lookup_opt_collation(&self, name: Option<&str>) -> SqlResult<Option<&'a Collation>> {
        match name {
            Some(n) => self.lookup_collation(n).map(Some),
            None => Ok(None),
        }
    }
}

/// Owned operand collation metadata derived from an AST expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprCollation {
    pub name: Option<String>,
    pub explicit: bool,
}

impl ExprCollation {
    pub fn none() -> Self {
        Self {
            name: None,
            explicit: false,
        }
    }

    pub fn implicit(name: String) -> Self {
        Self {
            name: Some(name),
            explicit: false,
        }
    }

    pub fn explicit(name: String) -> Self {
        Self {
            name: Some(name),
            explicit: true,
        }
    }

    pub fn as_operand(&self) -> Option<OperandCollation<'_>> {
        self.name.as_deref().map(|n| OperandCollation {
            name: Some(n),
            explicit: self.explicit,
        })
    }
}

/// Resolved column info from a scope stack.
#[derive(Debug, Clone)]
pub struct ResolvedCol {
    pub value: SqlValue,
    pub affinity: Affinity,
    pub collation: String,
}

/// Zero-allocation borrowed collation descriptor for hot comparison paths.
#[derive(Debug, Clone, Copy)]
struct BorrowedCollation<'a> {
    name: Option<&'a str>,
    explicit: bool,
}

impl<'a> BorrowedCollation<'a> {
    const NONE: Self = Self {
        name: None,
        explicit: false,
    };

    #[inline]
    fn as_operand(self) -> Option<OperandCollation<'a>> {
        self.name.map(|n| OperandCollation {
            name: Some(n),
            explicit: self.explicit,
        })
    }
}

/// Resolve a column reference across `scopes` without allocating a collation `String`.
/// Returns `(value, column_affinity, implicit_collation)` where `None` indicates a
/// boolean-literal (`true`/`false`) or DQS fallback rather than a real column/alias.
fn resolve_column_ref<'s>(
    col_ref: &ColumnRef,
    scopes: &[EvalScope<'s>],
    dqs: bool,
) -> SqlResult<(SqlValue, Option<Affinity>, Option<&'s str>)> {
    let want_schema = col_ref.schema.as_ref().map(|n| n.name.as_str());
    let want_table = col_ref.table.as_ref().map(|n| n.name.as_str());
    let want_col = col_ref.column.name.as_str();

    // Validate schema qualifier if present (`main` and `temp` are valid SQLite schemas).
    if let Some(ws) = want_schema {
        if !is_valid_schema_name(Some(ws)) {
            let full = match want_table {
                Some(t) => format!("{ws}.{t}.{want_col}"),
                None => format!("{ws}.{want_col}"),
            };
            return Err(SqlError::runtime(format!("no such column: {full}")));
        }
    }

    for scope in scopes.iter().rev() {
        let mut found_idx: Option<usize> = None;
        let mut ambiguous = false;
        for (idx, col) in scope.columns.iter().enumerate() {
            if col.is_rowid_alias && scope.no_rowid_aliases {
                continue;
            }
            if want_table.is_none()
                && scope
                    .hidden_unqualified
                    .and_then(|h| h.get(idx).copied())
                    .unwrap_or(false)
            {
                continue;
            }
            if let Some(wt) = want_table {
                let Some(ref ct) = col.table else {
                    continue;
                };
                if !ct.eq_ignore_ascii_case(wt) {
                    continue;
                }
            }
            if col.name.eq_ignore_ascii_case(want_col) {
                // Non-rowid columns take precedence over synthetic rowid aliases (`rowid`, `_rowid_`, `oid`).
                if let Some(prev) = found_idx {
                    if scope.columns[prev].is_rowid_alias && !col.is_rowid_alias {
                        found_idx = Some(idx);
                        ambiguous = false;
                    } else if !scope.columns[prev].is_rowid_alias && col.is_rowid_alias {
                        // keep real column
                    } else {
                        let same_source = scope.source_ids.is_some_and(|sids| {
                            sids.get(prev)
                                .copied()
                                .is_some_and(|a| sids.get(idx).copied() == Some(a))
                        });
                        if !same_source && (want_table.is_none() || scope.source_ids.is_some()) {
                            ambiguous = true;
                        }
                    }
                } else {
                    found_idx = Some(idx);
                }
            }
        }
        if ambiguous {
            let full = match want_table {
                Some(t) => format!("{t}.{want_col}"),
                None => want_col.to_string(),
            };
            return Err(SqlError::runtime(format!("ambiguous column name: {full}")));
        }
        if let Some(idx) = found_idx {
            let c = &scope.columns[idx];
            return Ok((
                scope.values.get(idx).cloned().unwrap_or(SqlValue::Null),
                Some(c.affinity),
                (!c.collation.is_empty()).then_some(c.collation.as_str()),
            ));
        }
        // Check SELECT aliases (only when unqualified)
        if want_table.is_none() {
            if let Some(aliases) = scope.aliases {
                for (aname, aval, aaff, acoll) in aliases {
                    if aname.eq_ignore_ascii_case(want_col) {
                        return Ok((
                            aval.clone(),
                            Some(*aaff),
                            (!acoll.is_empty()).then_some(acoll.as_str()),
                        ));
                    }
                }
            }
        }
    }

    // SQLite 3.23+ boolean literal fallback: unquoted `true` / `false` evaluate to 1 / 0
    // when no matching column exists in scope (e.g. `DEFAULT (true)`, `CHECK (x = true)`).
    if want_table.is_none()
        && want_schema.is_none()
        && col_ref.column.quote == QuoteStyle::None
    {
        if want_col.eq_ignore_ascii_case("true") {
            return Ok((SqlValue::Int(1), None, None));
        }
        if want_col.eq_ignore_ascii_case("false") {
            return Ok((SqlValue::Int(0), None, None));
        }
    }

    // SQLite DQS fallback: double-quoted identifier becomes a string literal when `dqs` is true.
    if dqs
        && want_table.is_none()
        && col_ref.schema.is_none()
        && col_ref.column.quote == QuoteStyle::Double
    {
        return Ok((SqlValue::text(want_col), None, None));
    }

    let full = match (want_schema, want_table) {
        (Some(s), Some(t)) => format!("{s}.{t}.{want_col}"),
        (None, Some(t)) => format!("{t}.{want_col}"),
        (Some(s), None) => format!("{s}.{want_col}"),
        (None, None) => want_col.to_string(),
    };
    Err(SqlError::runtime(format!("no such column: {full}")))
}

/// Resolve a column reference across `scopes` (innermost = last in slice).
pub fn resolve_column(
    col_ref: &ColumnRef,
    scopes: &[EvalScope<'_>],
    dqs: bool,
) -> SqlResult<ResolvedCol> {
    let (value, affinity, coll) = resolve_column_ref(col_ref, scopes, dqs)?;
    let eff_aff = match affinity {
        Some(a) => a,
        None => {
            if matches!(value, SqlValue::Text(_)) {
                Affinity::Blob
            } else {
                Affinity::None
            }
        }
    };
    Ok(ResolvedCol {
        value,
        affinity: eff_aff,
        collation: coll.unwrap_or("BINARY").to_string(),
    })
}

/// Lightweight, zero-allocation column metadata lookup for affinity/collation queries.
fn resolve_column_meta<'a>(
    col_ref: &ColumnRef,
    scopes: &[EvalScope<'a>],
) -> Option<(Affinity, &'a str)> {
    let want_schema = col_ref.schema.as_ref().map(|n| n.name.as_str());
    if let Some(ws) = want_schema {
        if !is_valid_schema_name(Some(ws)) {
            return None;
        }
    }
    let want_table = col_ref.table.as_ref().map(|n| n.name.as_str());
    let want_col = col_ref.column.name.as_str();

    for scope in scopes.iter().rev() {
        let mut found_idx: Option<usize> = None;
        let mut ambiguous = false;
        for (idx, col) in scope.columns.iter().enumerate() {
            if col.is_rowid_alias && scope.no_rowid_aliases {
                continue;
            }
            if want_table.is_none()
                && scope
                    .hidden_unqualified
                    .and_then(|h| h.get(idx).copied())
                    .unwrap_or(false)
            {
                continue;
            }
            if let Some(wt) = want_table {
                let Some(ref ct) = col.table else {
                    continue;
                };
                if !ct.eq_ignore_ascii_case(wt) {
                    continue;
                }
            }
            if col.name.eq_ignore_ascii_case(want_col) {
                if let Some(prev) = found_idx {
                    if scope.columns[prev].is_rowid_alias && !col.is_rowid_alias {
                        found_idx = Some(idx);
                        ambiguous = false;
                    } else if !scope.columns[prev].is_rowid_alias && col.is_rowid_alias {
                        // keep real column
                    } else {
                        let same_source = scope.source_ids.is_some_and(|sids| {
                            sids.get(prev)
                                .copied()
                                .is_some_and(|a| sids.get(idx).copied() == Some(a))
                        });
                        if !same_source && (want_table.is_none() || scope.source_ids.is_some()) {
                            ambiguous = true;
                        }
                    }
                } else {
                    found_idx = Some(idx);
                }
            }
        }
        if ambiguous {
            return None;
        }
        if let Some(idx) = found_idx {
            let c = &scope.columns[idx];
            return Some((c.affinity, c.collation.as_str()));
        }
        if want_table.is_none() {
            if let Some(aliases) = scope.aliases {
                for (aname, _, aaff, acoll) in aliases {
                    if aname.eq_ignore_ascii_case(want_col) {
                        return Some((*aaff, acoll.as_str()));
                    }
                }
            }
        }
    }
    None
}

/// Static affinity of an expression (`sqlite3ExprAffinity`, `expr.c:324`).
/// Returns `None` (`SQLITE_AFF_NONE`) for literals and general expressions with no affinity.
pub fn expr_affinity(expr: &Expr, scopes: &[EvalScope<'_>]) -> Option<Affinity> {
    match expr {
        Expr::Column(col_ref) => resolve_column_meta(col_ref, scopes).map(|(a, _)| a),
        Expr::Cast { type_name, .. } => match type_name {
            Some(t) => Some(affinity_of_decl_type(Some(t.text.as_bytes()))),
            None => Some(Affinity::Blob),
        },
        Expr::Collate { operand, .. } => expr_affinity(operand, scopes),
        Expr::Subquery { select, .. } => {
            if let xdb_parser::SelectBody::Core(core) = &select.body {
                if let Some(xdb_parser::ResultColumn::Expr(re)) = core.columns.first() {
                    if let Expr::Cast { type_name, .. } = &re.expr {
                        return Some(match type_name {
                            Some(t) => affinity_of_decl_type(Some(t.text.as_bytes())),
                            None => Affinity::Blob,
                        });
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Zero-allocation collation inspection for an expression.
fn expr_collation_ref<'a>(expr: &'a Expr, scopes: &[EvalScope<'a>]) -> BorrowedCollation<'a> {
    match expr {
        Expr::Collate { collation, .. } => BorrowedCollation {
            name: Some(collation.name.as_str()),
            explicit: true,
        },
        Expr::Column(col_ref) => match resolve_column_meta(col_ref, scopes) {
            Some((_, coll)) if !coll.is_empty() => BorrowedCollation {
                name: Some(coll),
                explicit: false,
            },
            _ => BorrowedCollation::NONE,
        },
        Expr::Cast { operand, .. } => expr_collation_ref(operand, scopes),
        Expr::Unary {
            op: UnaryOp::Pos,
            operand,
            ..
        } => expr_collation_ref(operand, scopes),
        Expr::Binary { left, right, .. } => {
            let lc = expr_collation_ref(left, scopes);
            if lc.explicit {
                lc
            } else {
                let rc = expr_collation_ref(right, scopes);
                if rc.explicit {
                    rc
                } else {
                    BorrowedCollation::NONE
                }
            }
        }
        Expr::Case {
            whens, else_expr, ..
        } => {
            for w in whens {
                let tc = expr_collation_ref(&w.then, scopes);
                if tc.explicit {
                    return tc;
                }
            }
            if let Some(el) = else_expr {
                let ec = expr_collation_ref(el, scopes);
                if ec.explicit {
                    return ec;
                }
            }
            BorrowedCollation::NONE
        }
        _ => BorrowedCollation::NONE,
    }
}

/// Collation of an expression (`sqlite3ExprCollSeq`, `expr.c:180`).
pub fn expr_collation(expr: &Expr, scopes: &[EvalScope<'_>]) -> ExprCollation {
    let bc = expr_collation_ref(expr, scopes);
    ExprCollation {
        name: bc.name.map(str::to_string),
        explicit: bc.explicit,
    }
}

/// Evaluate an expression, returning `(SqlValue, is_json_subtype)`.
pub fn eval_expr_tagged(
    expr: &Expr,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    let next_depth = env.eval_depth.get() + 1;
    if next_depth > MAX_EVAL_DEPTH {
        return Err(SqlError::runtime(format!(
            "Expression tree is too large (maximum depth {MAX_EVAL_DEPTH})"
        )));
    }
    env.eval_depth.set(next_depth);
    let _depth_guard = EvalDepthGuard(&env.eval_depth);
    env.fn_ctx.charge_steps(1)?;

    // Check precomputed aggregate/window slot on the innermost scope first.
    let span = expr.span();
    if let Some(inner) = scopes.last() {
        if let Some(pre) = inner.precomputed {
            for (i, (s, e, v)) in pre.iter().enumerate() {
                if *s == span.start && *e == span.end {
                    let is_json = inner
                        .precomputed_json
                        .and_then(|fl| fl.get(i).copied())
                        .unwrap_or(false);
                    return Ok((v.clone(), is_json));
                }
            }
        }
    }

    match expr {
        Expr::Null { .. } => Ok((SqlValue::Null, false)),
        Expr::Boolean { value, .. } => Ok((SqlValue::Int(i64::from(*value)), false)),
        Expr::String(s) => Ok((SqlValue::text(s.value.as_str()), false)),
        Expr::Blob(b) => Ok((SqlValue::blob(b.value.clone()), false)),
        Expr::Number(n) => {
            let val = if n.is_float {
                real_literal_value(n.text.as_bytes(), false)
            } else {
                integer_literal_value(n.text.as_bytes(), false)
                    .ok_or_else(|| SqlError::runtime("hex literal too big"))?
            };
            Ok((val, false))
        }
        Expr::CurrentTime { keyword, .. } => {
            let val = match keyword {
                CurrentTimeKeyword::CurrentDate => date_named(&[], &env.fn_ctx, "current_date")?,
                CurrentTimeKeyword::CurrentTime => time_named(&[], &env.fn_ctx, "current_time")?,
                CurrentTimeKeyword::CurrentTimestamp => {
                    datetime_named(&[], &env.fn_ctx, "current_timestamp")?
                }
            };
            Ok((val, false))
        }
        Expr::Variable(v) => Ok((env.params.resolve(v.index, &v.text), false)),
        Expr::Column(c) => {
            let (val, _, _) = resolve_column_ref(c, scopes, env.dqs)?;
            Ok((val, false))
        }
        Expr::Unary { op, operand, .. } => {
            // SQLite literal negation (`-9223372036854775808`, `-0x8000...`, `-1.5`):
            if *op == UnaryOp::Neg {
                if let Expr::Number(n) = operand.as_ref() {
                    let val = if n.is_float {
                        real_literal_value(n.text.as_bytes(), true)
                    } else {
                        integer_literal_value(n.text.as_bytes(), true)
                            .ok_or_else(|| SqlError::runtime("hex literal too big"))?
                    };
                    return Ok((val, false));
                }
            }
            let (v, _) = eval_expr_tagged(operand, scopes, env)?;
            let enc = env.options.collations.encoding();
            let out = match op {
                UnaryOp::Pos => v,
                UnaryOp::Neg => negate_enc(&v, enc),
                UnaryOp::BitNot => bit_not_enc(&v, enc),
                UnaryOp::Not => {
                    if v.is_null() {
                        SqlValue::Null
                    } else {
                        SqlValue::Int(i64::from(!boolean_value_enc(&v, false, enc)))
                    }
                }
            };
            Ok((out, false))
        }
        Expr::Binary {
            op, left, right, ..
        } => eval_binary(*op, left, right, scopes, env),
        Expr::IsNull { not, operand, .. } => {
            let (v, _) = eval_expr_tagged(operand, scopes, env)?;
            let res = if *not { !v.is_null() } else { v.is_null() };
            Ok((SqlValue::Int(i64::from(res)), false))
        }
        Expr::Like {
            op,
            not,
            left,
            right,
            escape,
            ilike,
            ..
        } => {
            let (s_val, _) = eval_expr_tagged(left, scopes, env)?;
            let (p_val, _) = eval_expr_tagged(right, scopes, env)?;
            let mut args = vec![p_val, s_val];
            if let Some(esc) = escape {
                let (e_val, _) = eval_expr_tagged(esc, scopes, env)?;
                args.push(e_val);
            }
            let matched = match op {
                LikeOperator::Like => {
                    let info = if *ilike || !env.options.case_sensitive_like {
                        LIKE_INFO
                    } else {
                        LIKE_CASE_SENSITIVE_INFO
                    };
                    like_invoke(&args, &env.fn_ctx, info)?
                }
                LikeOperator::Glob => {
                    if escape.is_some() {
                        return Err(SqlError::runtime(
                            "wrong number of arguments to function glob()",
                        ));
                    }
                    like_invoke(&args, &env.fn_ctx, GLOB_INFO)?
                }
                LikeOperator::Regexp => {
                    if escape.is_some() {
                        return Err(SqlError::runtime(
                            "wrong number of arguments to function regexp()",
                        ));
                    }
                    if env.options.functions.has_custom_scalar("regexp", args.len()) {
                        env.options
                            .functions
                            .call_scalar("regexp", &args, &env.fn_ctx)?
                            .0
                    } else {
                        return Err(SqlError::runtime("no such function: REGEXP"));
                    }
                }
                LikeOperator::Match => {
                    if escape.is_some() {
                        return Err(SqlError::runtime(
                            "wrong number of arguments to function match()",
                        ));
                    }
                    if env.options.functions.has_custom_scalar("match", args.len()) {
                        env.options
                            .functions
                            .call_scalar("match", &args, &env.fn_ctx)?
                            .0
                    } else {
                        return Err(SqlError::runtime("no such function: MATCH"));
                    }
                }
            };
            let out = if *not {
                match matched {
                    SqlValue::Null => SqlValue::Null,
                    SqlValue::Int(i) => SqlValue::Int(i64::from(i == 0)),
                    other => SqlValue::Int(i64::from(!boolean_value_enc(
                        &other,
                        false,
                        env.options.collations.encoding(),
                    ))),
                }
            } else {
                matched
            };
            Ok((out, false))
        }
        Expr::Between {
            not,
            operand,
            low,
            high,
            ..
        } => {
            let has_vector_or_subq = is_row_value_expr(operand)
                || is_row_value_expr(low)
                || is_row_value_expr(high)
                || matches!(operand.as_ref(), Expr::Subquery { .. })
                || matches!(low.as_ref(), Expr::Subquery { .. })
                || matches!(high.as_ref(), Expr::Subquery { .. });
            if has_vector_or_subq {
                let (xvals, xaffs, mut xcolls) = eval_row_vector(operand, scopes, env)?;
                let (lvals, laffs, mut lcolls) = eval_row_vector(low, scopes, env)?;
                let (hvals, haffs, mut hcolls) = eval_row_vector(high, scopes, env)?;
                if xvals.len() != lvals.len() || xvals.len() != hvals.len() {
                    return Err(SqlError::runtime("row value misused"));
                }
                if xvals.len() == 1 {
                    if matches!(operand.as_ref(), Expr::Subquery { .. }) {
                        xcolls[0] = ExprCollation::none();
                    }
                    if matches!(low.as_ref(), Expr::Subquery { .. }) {
                        lcolls[0] = ExprCollation::none();
                    }
                    if matches!(high.as_ref(), Expr::Subquery { .. }) {
                        hcolls[0] = ExprCollation::none();
                    }
                }
                let ge_lo = compare_row_tuples(
                    CompareOp::Ge,
                    &xvals,
                    &xaffs,
                    &xcolls,
                    &lvals,
                    &laffs,
                    &lcolls,
                    env,
                )?;
                let le_hi = compare_row_tuples(
                    CompareOp::Le,
                    &xvals,
                    &xaffs,
                    &xcolls,
                    &hvals,
                    &haffs,
                    &hcolls,
                    env,
                )?;
                let in_range = match (ge_lo, le_hi) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                let out = match in_range {
                    None => SqlValue::Null,
                    Some(b) => SqlValue::Int(i64::from(if *not { !b } else { b })),
                };
                return Ok((out, false));
            }
            let (x, x_aff, op_coll) = eval_compare_operand(operand, scopes, env)?;
            let (lo, lo_aff, lo_coll) = eval_compare_operand(low, scopes, env)?;
            let (hi, hi_aff, hi_coll) = eval_compare_operand(high, scopes, env)?;
            let aff_lo = comparison_affinity(x_aff, lo_aff);
            let aff_hi = comparison_affinity(x_aff, hi_aff);
            let coll_name_lo =
                comparison_collation_name(op_coll.as_operand(), lo_coll.as_operand());
            let coll_name_hi =
                comparison_collation_name(op_coll.as_operand(), hi_coll.as_operand());
            let coll_lo = env.lookup_opt_collation(coll_name_lo)?;
            let coll_hi = env.lookup_opt_collation(coll_name_hi)?;

            let ge_lo = compare_op(CompareOp::Ge, x.clone(), lo, Some(aff_lo), coll_lo);
            let le_hi = compare_op(CompareOp::Le, x, hi, Some(aff_hi), coll_hi);
            // 3-valued AND: false AND null == false
            let in_range = match (ge_lo, le_hi) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            let out = match in_range {
                None => SqlValue::Null,
                Some(b) => SqlValue::Int(i64::from(if *not { !b } else { b })),
            };
            Ok((out, false))
        }
        Expr::InList {
            not,
            operand,
            items,
            ..
        } => eval_in_list(*not, operand, items, scopes, env),
        Expr::InSelect {
            not,
            operand,
            select,
            ..
        } => eval_in_select(*not, operand, select, scopes, env),
        Expr::InTable {
            not,
            operand,
            schema,
            table,
            args,
            ..
        } => {
            let schema_str = schema.as_ref().map(|n| n.name.as_str());
            if args.is_none() {
                if let Some(cached) = env.get_cached_in_table(schema_str, &table.name)? {
                    return eval_in_cached(*not, operand, &cached, false, scopes, env);
                }
            }
            let eval_args = match args {
                Some(list) => {
                    let mut v = Vec::with_capacity(list.len());
                    for a in list {
                        v.push(eval_expr(a, scopes, env)?);
                    }
                    Some(v)
                }
                None => None,
            };
            let (rhs_affs, rhs_colls, rows) = env.subqueries.run_in_table_with_meta(
                schema_str,
                &table.name,
                eval_args.as_deref(),
            )?;
            eval_in_rows(*not, operand, &rhs_affs, &rhs_colls, &rows, scopes, env)
        }
        Expr::Exists { select, .. } => {
            if let Some(cached) = env.get_cached_select(select, scopes)? {
                return Ok((SqlValue::Int(i64::from(!cached.rows.is_empty())), false));
            }
            let (_, rows) = env.subqueries.run_select(select, scopes)?;
            Ok((SqlValue::Int(i64::from(!rows.is_empty())), false))
        }
        Expr::Subquery { select, .. } => {
            if let Some(cached) = env.get_cached_select(select, scopes)? {
                if cached.cols.len() != 1 {
                    return Err(SqlError::runtime(
                        "sub-select returns more than 1 column",
                    ));
                }
                let val = cached
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .cloned()
                    .unwrap_or(SqlValue::Null);
                return Ok((val, false));
            }
            let (cols, rows) = env.subqueries.run_select(select, scopes)?;
            if cols.len() != 1 {
                return Err(SqlError::runtime(
                    "sub-select returns more than 1 column",
                ));
            }
            let val = rows
                .first()
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or(SqlValue::Null);
            Ok((val, false))
        }
        Expr::Cast {
            operand, type_name, ..
        } => {
            let (v, _) = eval_expr_tagged(operand, scopes, env)?;
            let decl = type_name.as_ref().map_or("", |t| t.text.as_str());
            let enc = env.options.collations.encoding();
            Ok((cast_to_type(v, decl.as_bytes(), enc), false))
        }
        Expr::Collate {
            operand, collation, ..
        } => {
            // Validate collation exists!
            let _ = env.lookup_collation(&collation.name)?;
            eval_expr_tagged(operand, scopes, env)
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(base_expr) = operand {
                let is_vector_case = is_row_value_expr(base_expr)
                    || matches!(base_expr.as_ref(), Expr::Subquery { .. })
                    || whens.iter().any(|w| {
                        is_row_value_expr(&w.when) || matches!(w.when, Expr::Subquery { .. })
                    });
                if is_vector_case {
                    let (lvals, laffs, mut lcolls) = eval_row_vector(base_expr, scopes, env)?;
                    if lvals.len() == 1 && matches!(base_expr.as_ref(), Expr::Subquery { .. }) {
                        lcolls[0] = ExprCollation::none();
                    }
                    for w in whens {
                        let (rvals, raffs, mut rcolls) = eval_row_vector(&w.when, scopes, env)?;
                        if lvals.len() != rvals.len() {
                            return Err(SqlError::runtime(format!(
                                "row value misused: {} vs {} columns",
                                lvals.len(),
                                rvals.len()
                            )));
                        }
                        if rvals.len() == 1 && matches!(w.when, Expr::Subquery { .. }) {
                            rcolls[0] = ExprCollation::none();
                        }
                        if compare_row_tuples(
                            CompareOp::Eq,
                            &lvals,
                            &laffs,
                            &lcolls,
                            &rvals,
                            &raffs,
                            &rcolls,
                            env,
                        )? == Some(true)
                        {
                            return eval_expr_tagged(&w.then, scopes, env);
                        }
                    }
                } else {
                    let (base_val, base_aff, base_coll) =
                        eval_compare_operand(base_expr, scopes, env)?;
                    for w in whens {
                        let (wv, w_aff, w_coll) = eval_compare_operand(&w.when, scopes, env)?;
                        if !base_val.is_null() && !wv.is_null() {
                            let aff = comparison_affinity(base_aff, w_aff);
                            let cname = comparison_collation_name(
                                base_coll.as_operand(),
                                w_coll.as_operand(),
                            );
                            let coll = env.lookup_opt_collation(cname)?;
                            if compare_op(CompareOp::Eq, base_val.clone(), wv, Some(aff), coll)
                                == Some(true)
                            {
                                return eval_expr_tagged(&w.then, scopes, env);
                            }
                        }
                    }
                }
            } else {
                let enc = env.options.collations.encoding();
                for w in whens {
                    let (cond, _) = eval_expr_tagged(&w.when, scopes, env)?;
                    if boolean_value_enc(&cond, false, enc) {
                        return eval_expr_tagged(&w.then, scopes, env);
                    }
                }
            }
            match else_expr {
                Some(e) => eval_expr_tagged(e, scopes, env),
                None => Ok((SqlValue::Null, false)),
            }
        }
        Expr::Function(f) => {
            if f.over.is_some() {
                return Err(SqlError::runtime(format!(
                    "misuse of window function {}()",
                    f.name.name
                )));
            }
            if env
                .options
                .functions
                .is_aggregate(&f.name.name, f.args.len(), f.star)
            {
                return Err(SqlError::runtime(format!(
                    "misuse of aggregate: {}()",
                    f.name.name
                )));
            }
            if f.filter.is_some() {
                return Err(SqlError::runtime(format!(
                    "FILTER may not be used with non-aggregate {}()",
                    f.name.name
                )));
            }
            if f.order_by.is_some() {
                return Err(SqlError::runtime(format!(
                    "ORDER BY may not be used with non-aggregate {}()",
                    f.name.name
                )));
            }
            // Short-circuit built-in conditional functions (SQLite evaluates only taken branches).
            let fn_name = f.name.name.as_str();
            if fn_name.eq_ignore_ascii_case("likelihood")
                && f.args.len() == 2
                && !env.options.functions.has_custom_scalar(fn_name, 2)
            {
                let mut prob_expr = &f.args[1];
                while let Expr::Unary {
                    op: UnaryOp::Pos,
                    operand,
                    ..
                } = prob_expr
                {
                    prob_expr = operand;
                }
                let ok_lit = match prob_expr {
                    Expr::Number(n) if n.is_float => {
                        let p = xdb_core::value::atof(n.text.as_bytes()).value;
                        (0.0..=1.0).contains(&p)
                    }
                    _ => false,
                };
                if !ok_lit {
                    return Err(SqlError::runtime(
                        "second argument of likelihood() must be a constant between 0.0 and 1.0",
                    ));
                }
            }
            let is_coalesce = fn_name.eq_ignore_ascii_case("coalesce");
            let is_ifnull = !is_coalesce && fn_name.eq_ignore_ascii_case("ifnull");
            let is_iif = !is_coalesce
                && !is_ifnull
                && (fn_name.eq_ignore_ascii_case("iif") || fn_name.eq_ignore_ascii_case("if"));
            if (is_coalesce || is_ifnull || is_iif)
                && !env
                    .options
                    .functions
                    .has_custom_scalar(fn_name, f.args.len())
            {
                if is_coalesce {
                    if f.args.len() < 2 {
                        return Err(SqlError::runtime(
                            "wrong number of arguments to function coalesce()",
                        ));
                    }
                    for a in &f.args {
                        let (v, is_j) = eval_expr_tagged(a, scopes, env)?;
                        if !v.is_null() {
                            return Ok((v, is_j));
                        }
                    }
                    return Ok((SqlValue::Null, false));
                }
                if is_ifnull {
                    if f.args.len() != 2 {
                        return Err(SqlError::runtime(
                            "wrong number of arguments to function ifnull()",
                        ));
                    }
                    let (v0, j0) = eval_expr_tagged(&f.args[0], scopes, env)?;
                    if !v0.is_null() {
                        return Ok((v0, j0));
                    }
                    return eval_expr_tagged(&f.args[1], scopes, env);
                }
                if f.args.len() < 2 {
                    return Err(SqlError::runtime(format!(
                        "wrong number of arguments to function {}()",
                        f.name.name
                    )));
                }
                let enc = env.options.collations.encoding();
                let mut idx = 0;
                while idx + 1 < f.args.len() {
                    let (cond, _) = eval_expr_tagged(&f.args[idx], scopes, env)?;
                    if boolean_value_enc(&cond, false, enc) {
                        return eval_expr_tagged(&f.args[idx + 1], scopes, env);
                    }
                    idx += 2;
                }
                if idx < f.args.len() {
                    return eval_expr_tagged(&f.args[idx], scopes, env);
                }
                return Ok((SqlValue::Null, false));
            }
            let mut arg_vals = Vec::with_capacity(f.args.len());
            let mut json_mask: u64 = 0;
            let mut json_extra: Vec<bool> = if f.args.len() > 64 {
                Vec::with_capacity(f.args.len() - 64)
            } else {
                Vec::new()
            };
            // Explicit COLLATE on any arg overrides implicit collation (matching SQLite).
            let mut explicit_coll: Option<&Collation> = None;
            let mut implicit_coll: Option<&Collation> = None;
            for (idx, a) in f.args.iter().enumerate() {
                let oc = expr_collation_ref(a, scopes);
                if let Some(cname) = oc.name {
                    if oc.explicit {
                        if explicit_coll.is_none() {
                            explicit_coll = Some(env.lookup_collation(cname)?);
                        }
                    } else if implicit_coll.is_none() {
                        implicit_coll = Some(env.lookup_collation(cname)?);
                    }
                }
                let (v, is_j) = eval_expr_tagged(a, scopes, env)?;
                if idx < 64 {
                    if is_j {
                        json_mask |= 1u64 << idx;
                    }
                } else {
                    json_extra.push(is_j);
                }
                arg_vals.push(v);
            }
            let coll_opt = explicit_coll.or(implicit_coll);
            let extra_slice = (!json_extra.is_empty()).then_some(json_extra.as_slice());
            let call_ctx = env
                .fn_ctx
                .with_collation_and_json_flags(coll_opt, json_mask, extra_slice);
            env.options
                .functions
                .call_scalar(&f.name.name, &arg_vals, &call_ctx)
        }
        Expr::Vector { items, .. } => {
            if items.len() == 1 {
                eval_expr_tagged(&items[0], scopes, env)
            } else {
                Err(SqlError::runtime("row value misused"))
            }
        }
        Expr::Raise { .. } => Err(SqlError::runtime(
            "RAISE() may only be used within a trigger-program",
        )),
    }
}

/// Evaluate an expression to a [`SqlValue`].
#[inline]
pub fn eval_expr(expr: &Expr, scopes: &[EvalScope<'_>], env: &EvalEnv<'_>) -> SqlResult<SqlValue> {
    eval_expr_tagged(expr, scopes, env).map(|(v, _)| v)
}

fn eval_binary(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    // SQL 3-valued logic with short-circuiting for AND/OR
    let enc = env.options.collations.encoding();
    match op {
        BinaryOp::And => {
            let (lv, _) = eval_expr_tagged(left, scopes, env)?;
            if !lv.is_null() && !boolean_value_enc(&lv, false, enc) {
                return Ok((SqlValue::Int(0), false));
            }
            let (rv, _) = eval_expr_tagged(right, scopes, env)?;
            if !rv.is_null() && !boolean_value_enc(&rv, false, enc) {
                return Ok((SqlValue::Int(0), false));
            }
            if lv.is_null() || rv.is_null() {
                return Ok((SqlValue::Null, false));
            }
            return Ok((SqlValue::Int(1), false));
        }
        BinaryOp::Or => {
            let (lv, _) = eval_expr_tagged(left, scopes, env)?;
            if !lv.is_null() && boolean_value_enc(&lv, false, enc) {
                return Ok((SqlValue::Int(1), false));
            }
            let (rv, _) = eval_expr_tagged(right, scopes, env)?;
            if !rv.is_null() && boolean_value_enc(&rv, false, enc) {
                return Ok((SqlValue::Int(1), false));
            }
            if lv.is_null() || rv.is_null() {
                return Ok((SqlValue::Null, false));
            }
            return Ok((SqlValue::Int(0), false));
        }
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Gt
        | BinaryOp::Ge
        | BinaryOp::Is
        | BinaryOp::IsNot => {
            let cop = match op {
                BinaryOp::Eq => CompareOp::Eq,
                BinaryOp::Ne => CompareOp::Ne,
                BinaryOp::Lt => CompareOp::Lt,
                BinaryOp::Le => CompareOp::Le,
                BinaryOp::Gt => CompareOp::Gt,
                BinaryOp::Ge => CompareOp::Ge,
                BinaryOp::Is => CompareOp::Is,
                BinaryOp::IsNot => CompareOp::IsNot,
                _ => unreachable!(),
            };
            // Row-value vector comparisons: `(a, b) = (c, d)`, `(a, b) = (SELECT x, y)`,
            // or `(SELECT 1, 2) = (SELECT 1, 2)`
            if is_row_value_expr(left)
                || is_row_value_expr(right)
                || matches!(
                    (left, right),
                    (Expr::Subquery { .. }, Expr::Subquery { .. })
                )
            {
                return eval_vector_compare(cop, left, right, scopes, env);
            }
            let (lv, laff, lcoll) = eval_compare_operand(left, scopes, env)?;
            let (rv, raff, rcoll) = eval_compare_operand(right, scopes, env)?;
            let cname = comparison_collation_name(lcoll.as_operand(), rcoll.as_operand());
            if laff != Some(Affinity::Text) && raff != Some(Affinity::Text) {
                if let (SqlValue::Int(a), SqlValue::Int(b)) = (&lv, &rv) {
                    if let Some(cn) = cname {
                        if !cn.eq_ignore_ascii_case("BINARY")
                            && !cn.eq_ignore_ascii_case("NOCASE")
                            && !cn.eq_ignore_ascii_case("RTRIM")
                        {
                            let _ = env.lookup_collation(cn)?;
                        }
                    }
                    let c = a.cmp(b);
                    let res = match cop {
                        CompareOp::Eq | CompareOp::Is => c == Ordering::Equal,
                        CompareOp::Ne | CompareOp::IsNot => c != Ordering::Equal,
                        CompareOp::Lt => c == Ordering::Less,
                        CompareOp::Le => c != Ordering::Greater,
                        CompareOp::Gt => c == Ordering::Greater,
                        CompareOp::Ge => c != Ordering::Less,
                    };
                    return Ok((SqlValue::Int(i64::from(res)), false));
                }
            }
            let aff = comparison_affinity(laff, raff);
            let coll = env.lookup_opt_collation(cname)?;
            let out = match compare_op(cop, lv, rv, Some(aff), coll) {
                None => SqlValue::Null,
                Some(b) => SqlValue::Int(i64::from(b)),
            };
            return Ok((out, false));
        }
        _ => {}
    }

    let (lv, _) = eval_expr_tagged(left, scopes, env)?;
    let (rv, _) = eval_expr_tagged(right, scopes, env)?;

    let out = match op {
        BinaryOp::Add => arith_enc(ArithOp::Add, &lv, &rv, enc),
        BinaryOp::Sub => arith_enc(ArithOp::Subtract, &lv, &rv, enc),
        BinaryOp::Mul => arith_enc(ArithOp::Multiply, &lv, &rv, enc),
        BinaryOp::Div => arith_enc(ArithOp::Divide, &lv, &rv, enc),
        BinaryOp::Rem => arith_enc(ArithOp::Remainder, &lv, &rv, enc),
        BinaryOp::BitAnd => bitwise_enc(BitOp::And, &lv, &rv, enc),
        BinaryOp::BitOr => bitwise_enc(BitOp::Or, &lv, &rv, enc),
        BinaryOp::LShift => bitwise_enc(BitOp::ShiftLeft, &lv, &rv, enc),
        BinaryOp::RShift => bitwise_enc(BitOp::ShiftRight, &lv, &rv, enc),
        BinaryOp::Concat => {
            let (Some(lb), Some(rb)) = (text_bytes_of(&lv), text_bytes_of(&rv)) else {
                return Ok((SqlValue::Null, false));
            };
            let total = lb
                .len()
                .checked_add(rb.len())
                .ok_or_else(SqlError::too_big)?;
            if total > env.fn_ctx.limits.max_length {
                return Err(SqlError::too_big());
            }
            env.fn_ctx.charge_steps((total / 256) as u64 + 1)?;
            if let Some(m) = env.fn_ctx.meter {
                m.reserve_memory(total as u64)?;
                m.release_memory(total as u64);
            }
            let mut buf = Vec::with_capacity(total);
            buf.extend_from_slice(&lb);
            buf.extend_from_slice(&rb);
            SqlValue::Text(buf.into_boxed_slice())
        }
        BinaryOp::Ptr => {
            let v = eval_json_arrow_metered(&lv, &rv, false, &env.fn_ctx)?;
            let is_j = !v.is_null();
            return Ok((v, is_j));
        }
        BinaryOp::Ptr2 => {
            return Ok((eval_json_arrow_metered(&lv, &rv, true, &env.fn_ctx)?, false));
        }
        _ => unreachable!(),
    };
    Ok((out, false))
}

fn is_row_value_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Vector { items, .. } if items.len() > 1)
}

type RowVector = (Vec<SqlValue>, Vec<Option<Affinity>>, Vec<ExprCollation>);

fn eval_compare_operand<'a>(
    expr: &'a Expr,
    scopes: &[EvalScope<'a>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, Option<Affinity>, BorrowedCollation<'a>)> {
    match expr {
        Expr::Column(c) => {
            env.fn_ctx.charge_steps(1)?;
            let (val, aff, coll) = resolve_column_ref(c, scopes, env.dqs)?;
            let bc = BorrowedCollation {
                name: coll,
                explicit: false,
            };
            Ok((val, aff, bc))
        }
        Expr::Number(n) => {
            env.fn_ctx.charge_steps(1)?;
            let val = if n.is_float {
                real_literal_value(n.text.as_bytes(), false)
            } else {
                integer_literal_value(n.text.as_bytes(), false)
                    .ok_or_else(|| SqlError::runtime("hex literal too big"))?
            };
            Ok((val, None, BorrowedCollation::NONE))
        }
        Expr::String(s) => {
            env.fn_ctx.charge_steps(1)?;
            Ok((SqlValue::text(s.value.as_str()), None, BorrowedCollation::NONE))
        }
        Expr::Null { .. } => {
            env.fn_ctx.charge_steps(1)?;
            Ok((SqlValue::Null, None, BorrowedCollation::NONE))
        }
        Expr::Boolean { value, .. } => {
            env.fn_ctx.charge_steps(1)?;
            Ok((SqlValue::Int(i64::from(*value)), None, BorrowedCollation::NONE))
        }
        Expr::Variable(v) => {
            env.fn_ctx.charge_steps(1)?;
            Ok((
                env.params.resolve(v.index, &v.text),
                None,
                BorrowedCollation::NONE,
            ))
        }
        Expr::Subquery { select, .. } => {
            env.fn_ctx.charge_steps(1)?;
            if let Some(cached) = env.get_cached_select(select, scopes)? {
                if cached.cols.len() != 1 {
                    return Err(SqlError::runtime(
                        "sub-select returns more than 1 column",
                    ));
                }
                let val = cached
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .cloned()
                    .unwrap_or(SqlValue::Null);
                let aff = cached.affs.first().copied().flatten();
                return Ok((val, aff, BorrowedCollation::NONE));
            }
            let (cols, affs, _colls, rows) = env.subqueries.run_select_with_meta(select, scopes)?;
            if cols.len() != 1 {
                return Err(SqlError::runtime(
                    "sub-select returns more than 1 column",
                ));
            }
            let val = rows
                .first()
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or(SqlValue::Null);
            let aff = affs.first().copied().flatten();
            Ok((val, aff, BorrowedCollation::NONE))
        }
        Expr::Collate {
            operand, collation, ..
        } => {
            env.fn_ctx.charge_steps(1)?;
            let depth = env.eval_depth.get() + 1;
            if depth > MAX_EVAL_DEPTH {
                return Err(SqlError::runtime("Expression tree is too large"));
            }
            env.eval_depth.set(depth);
            let _depth_guard = EvalDepthGuard(&env.eval_depth);
            let _ = env.lookup_collation(&collation.name)?;
            let (val, aff, _) = eval_compare_operand(operand, scopes, env)?;
            Ok((
                val,
                aff,
                BorrowedCollation {
                    name: Some(collation.name.as_str()),
                    explicit: true,
                },
            ))
        }
        _ => {
            let (v, _) = eval_expr_tagged(expr, scopes, env)?;
            Ok((v, expr_affinity(expr, scopes), expr_collation_ref(expr, scopes)))
        }
    }
}

fn eval_row_vector(
    expr: &Expr,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<RowVector> {
    match expr {
        Expr::Vector { items, .. } => {
            let mut vals = Vec::with_capacity(items.len());
            let mut affs = Vec::with_capacity(items.len());
            let mut colls = Vec::with_capacity(items.len());
            for item in items {
                vals.push(eval_expr(item, scopes, env)?);
                affs.push(expr_affinity(item, scopes));
                colls.push(expr_collation(item, scopes));
            }
            Ok((vals, affs, colls))
        }
        Expr::Subquery { select, .. } => {
            if let Some(cached) = env.get_cached_select(select, scopes)? {
                let width = cached.cols.len();
                let vals = cached
                    .rows
                    .first()
                    .cloned()
                    .unwrap_or_else(|| vec![SqlValue::Null; width]);
                return Ok((vals, cached.affs.clone(), cached.colls.clone()));
            }
            let (cols, affs, colls, rows) = env.subqueries.run_select_with_meta(select, scopes)?;
            let width = cols.len();
            let vals = rows
                .into_iter()
                .next()
                .unwrap_or_else(|| vec![SqlValue::Null; width]);
            Ok((vals, affs, colls))
        }
        Expr::Collate {
            operand, collation, ..
        } => {
            let _ = env.lookup_collation(&collation.name)?;
            let (vals, affs, mut colls) = eval_row_vector(operand, scopes, env)?;
            if colls.len() == 1 {
                colls[0] = ExprCollation::explicit(collation.name.clone());
            }
            Ok((vals, affs, colls))
        }
        other => Ok((
            vec![eval_expr(other, scopes, env)?],
            vec![expr_affinity(other, scopes)],
            vec![expr_collation(other, scopes)],
        )),
    }
}

fn eval_vector_compare(
    cop: CompareOp,
    left: &Expr,
    right: &Expr,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    let (lvals, laffs, mut lcolls) = eval_row_vector(left, scopes, env)?;
    let (rvals, raffs, mut rcolls) = eval_row_vector(right, scopes, env)?;
    if lvals.len() != rvals.len() {
        return Err(SqlError::runtime(format!(
            "row value misused: {} vs {} columns",
            lvals.len(),
            rvals.len()
        )));
    }
    if lvals.len() == 1 {
        if matches!(left, Expr::Subquery { .. }) {
            lcolls[0] = ExprCollation::none();
        }
        if matches!(right, Expr::Subquery { .. }) {
            rcolls[0] = ExprCollation::none();
        }
    }
    let res = compare_row_tuples(cop, &lvals, &laffs, &lcolls, &rvals, &raffs, &rcolls, env)?;
    Ok((
        match res {
            None => SqlValue::Null,
            Some(b) => SqlValue::Int(i64::from(b)),
        },
        false,
    ))
}

#[allow(clippy::too_many_arguments)]
fn compare_row_tuples(
    cop: CompareOp,
    lvals: &[SqlValue],
    laffs: &[Option<Affinity>],
    lcolls: &[ExprCollation],
    rvals: &[SqlValue],
    raffs: &[Option<Affinity>],
    rcolls: &[ExprCollation],
    env: &EvalEnv<'_>,
) -> SqlResult<Option<bool>> {
    let n = lvals.len();
    let null_eq = matches!(cop, CompareOp::Is | CompareOp::IsNot);
    let mut saw_null = false;

    for i in 0..n {
        let aff = comparison_affinity(laffs[i], raffs[i]);
        let cname = comparison_collation_name(lcolls[i].as_operand(), rcolls[i].as_operand());
        let coll = env.lookup_opt_collation(cname)?;
        let (lv, rv) = apply_comparison_affinity(lvals[i].clone(), rvals[i].clone(), Some(aff));

        if lv.is_null() || rv.is_null() {
            if null_eq {
                if !(lv.is_null() && rv.is_null()) {
                    return Ok(Some(cop == CompareOp::IsNot));
                }
                continue;
            }
            saw_null = true;
            if matches!(cop, CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge) {
                return Ok(None);
            }
            continue;
        }
        let ord = mem_compare(&lv, &rv, coll);
        if ord != Ordering::Equal {
            return Ok(Some(match cop {
                CompareOp::Eq | CompareOp::Is => false,
                CompareOp::Ne | CompareOp::IsNot => true,
                CompareOp::Lt | CompareOp::Le => ord == Ordering::Less,
                CompareOp::Gt | CompareOp::Ge => ord == Ordering::Greater,
            }));
        }
    }

    if saw_null {
        return Ok(None);
    }
    Ok(Some(match cop {
        CompareOp::Eq | CompareOp::Is | CompareOp::Le | CompareOp::Ge => true,
        CompareOp::Ne | CompareOp::IsNot | CompareOp::Lt | CompareOp::Gt => false,
    }))
}

fn eval_in_list(
    not: bool,
    operand: &Expr,
    items: &[Expr],
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    if items.is_empty() {
        return Ok((SqlValue::Int(i64::from(not)), false));
    }
    // Row-value IN list: `(a, b) IN ((1, 2), (3, NULL))` or `(SELECT a, b) IN ((1, 2))`
    if is_row_value_expr(operand) || matches!(operand, Expr::Subquery { .. }) {
        let (lvals, laffs, mut lcolls) = eval_row_vector(operand, scopes, env)?;
        let width = lvals.len();
        if width == 1 && matches!(operand, Expr::Subquery { .. }) {
            lcolls[0] = ExprCollation::none();
        }
        let mut saw_null = false;
        for item in items {
            let (rvals, raffs, rcolls) = eval_row_vector(item, scopes, env)?;
            if rvals.len() != width {
                return Err(SqlError::runtime(format!(
                    "row value misused: {} vs {} columns",
                    width,
                    rvals.len()
                )));
            }
            let mut row_null = false;
            let mut row_match = true;
            for i in 0..width {
                let eff_aff = if items.len() == 1 {
                    comparison_affinity(laffs[i], raffs[i])
                } else {
                    in_affinity(laffs[i], None)
                };
                let (lv, rv) =
                    apply_comparison_affinity(lvals[i].clone(), rvals[i].clone(), Some(eff_aff));
                if lv.is_null() || rv.is_null() {
                    row_null = true;
                    continue;
                }
                let rhs_op = if items.len() == 1 {
                    rcolls[i].as_operand()
                } else {
                    None
                };
                let cname = comparison_collation_name(lcolls[i].as_operand(), rhs_op);
                let coll = env.lookup_opt_collation(cname)?;
                if mem_compare(&lv, &rv, coll) != Ordering::Equal {
                    row_match = false;
                    break;
                }
            }
            if row_match && !row_null {
                return Ok((SqlValue::Int(i64::from(!not)), false));
            }
            if row_match && row_null {
                saw_null = true;
            }
        }
        return Ok((
            if saw_null {
                SqlValue::Null
            } else {
                SqlValue::Int(i64::from(not))
            },
            false,
        ));
    }

    let (lhs_val, lhs_aff, lhs_coll) = eval_compare_operand(operand, scopes, env)?;
    let aff = in_affinity(lhs_aff, None);

    if lhs_val.is_null() {
        return Ok((SqlValue::Null, false));
    }
    let mut saw_null = false;
    for item in items {
        let rv = eval_expr(item, scopes, env)?;
        if rv.is_null() {
            saw_null = true;
            continue;
        }
        // SQLite only folds `x IN (rhs)` to `x = rhs` (inheriting RHS collation) when `rhs` is
        // a column-free constant expression; column-bearing RHS keeps LHS collation!
        let rcoll = if items.len() == 1 && !expr_contains_column_ref(item, scopes) {
            expr_collation_ref(item, scopes)
        } else {
            BorrowedCollation::NONE
        };
        let cname = comparison_collation_name(lhs_coll.as_operand(), rcoll.as_operand());
        let coll = env.lookup_opt_collation(cname)?;
        let (lv, rhs_coerced) = apply_comparison_affinity(lhs_val.clone(), rv, Some(aff));
        if mem_compare(&lv, &rhs_coerced, coll) == Ordering::Equal {
            return Ok((SqlValue::Int(i64::from(!not)), false));
        }
    }
    if saw_null {
        Ok((SqlValue::Null, false))
    } else {
        Ok((SqlValue::Int(i64::from(not)), false))
    }
}

fn expr_contains_column_ref(expr: &Expr, scopes: &[EvalScope<'_>]) -> bool {
    match expr {
        Expr::Column(c) => {
            // Unquoted `true`/`false` without qualifiers are boolean literals unless a column
            // literally named `true`/`false` resolves in scope!
            if c.schema.is_none()
                && c.table.is_none()
                && c.column.quote == QuoteStyle::None
                && (c.column.name.eq_ignore_ascii_case("true")
                    || c.column.name.eq_ignore_ascii_case("false"))
            {
                return resolve_column_meta(c, scopes).is_some();
            }
            true
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_contains_column_ref(operand, scopes),
        Expr::Binary { left, right, .. } => {
            expr_contains_column_ref(left, scopes) || expr_contains_column_ref(right, scopes)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_contains_column_ref(left, scopes)
                || expr_contains_column_ref(right, scopes)
                || escape
                    .as_deref()
                    .is_some_and(|e| expr_contains_column_ref(e, scopes))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_contains_column_ref(operand, scopes)
                || expr_contains_column_ref(low, scopes)
                || expr_contains_column_ref(high, scopes)
        }
        Expr::InList { operand, items, .. } => {
            expr_contains_column_ref(operand, scopes)
                || items.iter().any(|i| expr_contains_column_ref(i, scopes))
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            expr_contains_column_ref(operand, scopes)
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_some_and(|e| expr_contains_column_ref(e, scopes))
                || whens.iter().any(|w| {
                    expr_contains_column_ref(&w.when, scopes)
                        || expr_contains_column_ref(&w.then, scopes)
                })
                || else_expr
                    .as_deref()
                    .is_some_and(|e| expr_contains_column_ref(e, scopes))
        }
        Expr::Function(f) => f.args.iter().any(|a| expr_contains_column_ref(a, scopes)),
        Expr::Vector { items, .. } => items.iter().any(|i| expr_contains_column_ref(i, scopes)),
        _ => false,
    }
}

impl<'a> CachedSelect<'a> {
    fn get_or_build_1col_lookup(
        &self,
        eff_aff: Affinity,
        cname: Option<&str>,
        coll: Option<&Collation>,
        env: &EvalEnv<'a>,
    ) -> SqlResult<Rc<SortedInLookup<'a>>> {
        let key = (eff_aff, CollKey::from_opt(cname));
        let mut entries = self.sorted_1col.borrow_mut();
        if let Some((_, existing)) = entries.iter().find(|(k, _)| *k == key) {
            return Ok(Rc::clone(existing));
        }
        let hash_coll: Option<BuiltinCollation> = match coll {
            None => Some(BuiltinCollation::Binary),
            Some(Collation::Builtin { kind, encoding }) => {
                if *encoding != TextEncoding::Utf8 {
                    None
                } else {
                    Some(*kind)
                }
            }
            Some(Collation::Custom { .. }) => None,
        };
        let has_meter = env.fn_ctx.meter.is_some();
        let mut mem = MemoryGuard::new(env.fn_ctx.meter);

        // Zero-clone fast path when affinity preserves integers and every row is Int or Null.
        if hash_coll.is_some() && eff_aff != Affinity::Text && eff_aff != Affinity::Real {
            let mut has_null = false;
            let mut all_ints = true;
            let mut non_null_count = 0usize;
            let mut min_v = i64::MAX;
            let mut max_v = i64::MIN;
            for row in &self.rows {
                if row.len() != 1 {
                    return Err(SqlError::runtime("row value width mismatch in IN"));
                }
                match row[0] {
                    SqlValue::Int(i) => {
                        if i < min_v {
                            min_v = i;
                        }
                        if i > max_v {
                            max_v = i;
                        }
                        non_null_count += 1;
                    }
                    SqlValue::Null => has_null = true,
                    _ => {
                        all_ints = false;
                        break;
                    }
                }
            }
            if all_ints {
                if has_meter {
                    env.fn_ctx.charge_steps(self.rows.len() as u64)?;
                }
                let index = if non_null_count > 0 && (i128::from(max_v) - i128::from(min_v)) <= 65_535
                {
                    let span = (i128::from(max_v) - i128::from(min_v)) as usize;
                    let words = (span >> 6) + 1;
                    if has_meter {
                        mem.reserve((words as u64).saturating_mul(8))?;
                    }
                    let mut bits = vec![0u64; words];
                    for row in &self.rows {
                        if let SqlValue::Int(i) = row[0] {
                            let off = i.wrapping_sub(min_v) as u64 as usize;
                            bits[off >> 6] |= 1u64 << (off & 63);
                        }
                    }
                    InLookupIndex::IntBitset {
                        min: min_v,
                        max: max_v,
                        bits: Rc::from(bits),
                    }
                } else {
                    if has_meter {
                        mem.reserve((non_null_count as u64).saturating_mul(8))?;
                    }
                    let mut ints = Vec::with_capacity(non_null_count);
                    for row in &self.rows {
                        if let SqlValue::Int(i) = row[0] {
                            ints.push(i);
                        }
                    }
                    ints.sort_unstable();
                    ints.dedup();
                    InLookupIndex::IntSorted(ints.into_boxed_slice())
                };
                let lookup = Rc::new(SortedInLookup {
                    index,
                    has_null,
                    _mem: mem,
                });
                entries.push((key, Rc::clone(&lookup)));
                return Ok(lookup);
            }
        }

        let mut has_null = false;
        let mut all_ints = hash_coll.is_some();
        let mut nocase_has_nul = false;
        let mut coerced = Vec::with_capacity(self.rows.len());
        let mut bytes = (self.rows.len() as u64).saturating_mul(24);
        for row in &self.rows {
            if has_meter {
                env.fn_ctx.charge_steps(1)?;
            }
            if row.len() != 1 {
                return Err(SqlError::runtime("row value width mismatch in IN"));
            }
            let rv = apply_affinity(row[0].clone(), eff_aff);
            if rv.is_null() {
                has_null = true;
            } else {
                if !matches!(rv, SqlValue::Int(_)) {
                    all_ints = false;
                }
                if hash_coll == Some(BuiltinCollation::Nocase) {
                    if let SqlValue::Text(ref b) = rv {
                        if b.contains(&0) {
                            nocase_has_nul = true;
                        }
                    }
                }
                bytes = bytes.saturating_add(estimate_value_bytes(&rv));
                coerced.push(rv);
            }
        }
        if has_meter {
            mem.reserve(bytes)?;
        }
        let index = if all_ints {
            let mut ints = Vec::with_capacity(coerced.len());
            let mut min_v = i64::MAX;
            let mut max_v = i64::MIN;
            for v in coerced {
                if let SqlValue::Int(i) = v {
                    if i < min_v {
                        min_v = i;
                    }
                    if i > max_v {
                        max_v = i;
                    }
                    ints.push(i);
                }
            }
            if !ints.is_empty() && (i128::from(max_v) - i128::from(min_v)) <= 65_535 {
                let span = (i128::from(max_v) - i128::from(min_v)) as usize;
                let words = (span >> 6) + 1;
                mem.reserve((words as u64).saturating_mul(8))?;
                let mut bits = vec![0u64; words];
                for i in ints {
                    let off = (i128::from(i) - i128::from(min_v)) as usize;
                    bits[off >> 6] |= 1u64 << (off & 63);
                }
                InLookupIndex::IntBitset {
                    min: min_v,
                    max: max_v,
                    bits: Rc::from(bits),
                }
            } else {
                ints.sort_unstable();
                ints.dedup();
                InLookupIndex::IntSorted(ints.into_boxed_slice())
            }
        } else if let Some(hc) = hash_coll.filter(|_| !nocase_has_nul) {
            let mut set = HashSet::with_capacity(coerced.len());
            for v in &coerced {
                if let Some(hk) = InHashKey::from_value(v, hc) {
                    set.insert(hk);
                }
            }
            InLookupIndex::Hashed { coll: hc, set }
        } else {
            coerced.sort_by(|a, b| mem_compare(a, b, coll));
            coerced.dedup_by(|a, b| mem_compare(a, b, coll) == Ordering::Equal);
            InLookupIndex::Sorted(coerced)
        };
        let lookup = Rc::new(SortedInLookup {
            index,
            has_null,
            _mem: mem,
        });
        entries.push((key, Rc::clone(&lookup)));
        Ok(lookup)
    }
}

/// Build a [`SortedInLookup`] directly from a flat 1-column slice (used by the streaming
/// `IN (SELECT col FROM tbl)` fast path to avoid materializing `Vec<Vec<SqlValue>>`).
fn build_1col_lookup_from_vals<'a>(
    vals: &[SqlValue],
    eff_aff: Affinity,
    _cname: Option<&str>,
    coll: Option<&Collation>,
    env: &EvalEnv<'a>,
) -> SqlResult<Rc<SortedInLookup<'a>>> {
    let hash_coll: Option<BuiltinCollation> = match coll {
        None => Some(BuiltinCollation::Binary),
        Some(Collation::Builtin { kind, encoding }) => {
            if *encoding != TextEncoding::Utf8 {
                None
            } else {
                Some(*kind)
            }
        }
        Some(Collation::Custom { .. }) => None,
    };
    let has_meter = env.fn_ctx.meter.is_some();
    let mut mem = MemoryGuard::new(env.fn_ctx.meter);

    if hash_coll.is_some() && eff_aff != Affinity::Text && eff_aff != Affinity::Real {
        let mut has_null = false;
        let mut all_ints = true;
        let mut non_null_count = 0usize;
        let mut min_v = i64::MAX;
        let mut max_v = i64::MIN;
        for v in vals {
            match *v {
                SqlValue::Int(i) => {
                    if i < min_v {
                        min_v = i;
                    }
                    if i > max_v {
                        max_v = i;
                    }
                    non_null_count += 1;
                }
                SqlValue::Null => has_null = true,
                _ => {
                    all_ints = false;
                    break;
                }
            }
        }
        if all_ints {
            if has_meter {
                env.fn_ctx.charge_steps(vals.len() as u64)?;
            }
            let index = if non_null_count > 0 && (i128::from(max_v) - i128::from(min_v)) <= 65_535 {
                let span = (i128::from(max_v) - i128::from(min_v)) as usize;
                let words = (span >> 6) + 1;
                if has_meter {
                    mem.reserve((words as u64).saturating_mul(8))?;
                }
                let mut bits = vec![0u64; words];
                for v in vals {
                    if let SqlValue::Int(i) = *v {
                        let off = i.wrapping_sub(min_v) as u64 as usize;
                        bits[off >> 6] |= 1u64 << (off & 63);
                    }
                }
                InLookupIndex::IntBitset {
                    min: min_v,
                    max: max_v,
                    bits: Rc::from(bits),
                }
            } else {
                if has_meter {
                    mem.reserve((non_null_count as u64).saturating_mul(8))?;
                }
                let mut ints = Vec::with_capacity(non_null_count);
                for v in vals {
                    if let SqlValue::Int(i) = *v {
                        ints.push(i);
                    }
                }
                ints.sort_unstable();
                ints.dedup();
                InLookupIndex::IntSorted(ints.into_boxed_slice())
            };
            return Ok(Rc::new(SortedInLookup {
                index,
                has_null,
                _mem: mem,
            }));
        }
    }

    let mut has_null = false;
    let mut all_ints = hash_coll.is_some();
    let mut nocase_has_nul = false;
    let mut coerced = Vec::with_capacity(vals.len());
    let mut bytes = (vals.len() as u64).saturating_mul(24);
    for v in vals {
        if has_meter {
            env.fn_ctx.charge_steps(1)?;
        }
        let rv = apply_affinity(v.clone(), eff_aff);
        if rv.is_null() {
            has_null = true;
        } else {
            if !matches!(rv, SqlValue::Int(_)) {
                all_ints = false;
            }
            if hash_coll == Some(BuiltinCollation::Nocase) {
                if let SqlValue::Text(ref b) = rv {
                    if b.contains(&0) {
                        nocase_has_nul = true;
                    }
                }
            }
            bytes = bytes.saturating_add(estimate_value_bytes(&rv));
            coerced.push(rv);
        }
    }
    if has_meter {
        mem.reserve(bytes)?;
    }
    let index = if all_ints {
        let mut ints = Vec::with_capacity(coerced.len());
        let mut min_v = i64::MAX;
        let mut max_v = i64::MIN;
        for v in coerced {
            if let SqlValue::Int(i) = v {
                if i < min_v {
                    min_v = i;
                }
                if i > max_v {
                    max_v = i;
                }
                ints.push(i);
            }
        }
        if !ints.is_empty() && (i128::from(max_v) - i128::from(min_v)) <= 65_535 {
            let span = (i128::from(max_v) - i128::from(min_v)) as usize;
            let words = (span >> 6) + 1;
            mem.reserve((words as u64).saturating_mul(8))?;
            let mut bits = vec![0u64; words];
            for i in ints {
                let off = i.wrapping_sub(min_v) as u64 as usize;
                bits[off >> 6] |= 1u64 << (off & 63);
            }
            InLookupIndex::IntBitset {
                min: min_v,
                max: max_v,
                bits: Rc::from(bits),
            }
        } else {
            ints.sort_unstable();
            ints.dedup();
            InLookupIndex::IntSorted(ints.into_boxed_slice())
        }
    } else if let Some(hc) = hash_coll.filter(|_| !nocase_has_nul) {
        let mut set = HashSet::with_capacity(coerced.len());
        for v in &coerced {
            if let Some(hk) = InHashKey::from_value(v, hc) {
                set.insert(hk);
            }
        }
        InLookupIndex::Hashed { coll: hc, set }
    } else {
        coerced.sort_by(|a, b| mem_compare(a, b, coll));
        coerced.dedup_by(|a, b| mem_compare(a, b, coll) == Ordering::Equal);
        InLookupIndex::Sorted(coerced)
    };
    Ok(Rc::new(SortedInLookup {
        index,
        has_null,
        _mem: mem,
    }))
}

impl SortedInLookup<'_> {
    #[inline]
    fn contains_coerced(&self, lhs_coerced: &SqlValue, coll: Option<&Collation>) -> bool {
        match &self.index {
            InLookupIndex::IntBitset { min, max, bits } => {
                let opt_i = match *lhs_coerced {
                    SqlValue::Int(i) => Some(i),
                    SqlValue::Real(r) => real_to_exact_i64(r),
                    _ => None,
                };
                opt_i.is_some_and(|i| {
                    i >= *min && i <= *max && {
                        let off = (i128::from(i) - i128::from(*min)) as usize;
                        (bits[off >> 6] >> (off & 63)) & 1 != 0
                    }
                })
            }
            InLookupIndex::IntSorted(sorted) => match *lhs_coerced {
                SqlValue::Int(i) => sorted.binary_search(&i).is_ok(),
                SqlValue::Real(r) => {
                    real_to_exact_i64(r).is_some_and(|i| sorted.binary_search(&i).is_ok())
                }
                _ => false,
            },
            InLookupIndex::Hashed { coll: hc, set } => {
                InHashKey::from_value(lhs_coerced, *hc).is_some_and(|hk| set.contains(&hk))
            }
            InLookupIndex::Sorted(sorted) => sorted
                .binary_search_by(|v| mem_compare(v, lhs_coerced, coll))
                .is_ok(),
        }
    }
}

fn eval_in_cached<'a>(
    not: bool,
    operand: &Expr,
    cached: &CachedSelect<'a>,
    check_cols: bool,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'a>,
) -> SqlResult<(SqlValue, bool)> {
    if !matches!(operand, Expr::Vector { .. } | Expr::Subquery { .. }) {
        if check_cols && cached.cols.len() != 1 {
            return Err(SqlError::runtime(format!(
                "sub-select returns {} columns - expected 1",
                cached.cols.len()
            )));
        }
        if !cached.affs.is_empty() && cached.affs.len() != 1 {
            return Err(SqlError::runtime(format!(
                "sub-select returns {} columns - expected 1",
                cached.affs.len()
            )));
        }
        let (lhs_val, lhs_aff, lhs_coll) = eval_compare_operand(operand, scopes, env)?;
        if cached.rows.is_empty() {
            return Ok((SqlValue::Int(i64::from(not)), false));
        }
        if lhs_val.is_null() {
            return Ok((SqlValue::Null, false));
        }
        let eff_aff = in_affinity(lhs_aff, Some(cached.affs.first().copied().flatten()));
        let rcoll = cached.colls.first().and_then(ExprCollation::as_operand);
        let cname = comparison_collation_name(lhs_coll.as_operand(), rcoll);
        let coll = env.lookup_opt_collation(cname)?;
        let lookup = cached.get_or_build_1col_lookup(eff_aff, cname, coll, env)?;
        let lhs_coerced = apply_affinity(lhs_val, eff_aff);
        env.fn_ctx.charge_steps(1)?;
        if lookup.contains_coerced(&lhs_coerced, coll) {
            return Ok((SqlValue::Int(i64::from(!not)), false));
        }
        return Ok((
            if lookup.has_null {
                SqlValue::Null
            } else {
                SqlValue::Int(i64::from(not))
            },
            false,
        ));
    }
    if check_cols {
        let (lvals, _, _) = eval_row_vector(operand, scopes, env)?;
        if cached.cols.len() != lvals.len() {
            return Err(SqlError::runtime(format!(
                "sub-select returns {} columns - expected {}",
                cached.cols.len(),
                lvals.len()
            )));
        }
    }
    eval_in_rows(
        not,
        operand,
        &cached.affs,
        &cached.colls,
        &cached.rows,
        scopes,
        env,
    )
}

fn eval_in_select(
    not: bool,
    operand: &Expr,
    select: &SelectStmt,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    if let Some(cached) = env.get_cached_select(select, scopes)? {
        return eval_in_cached(not, operand, &cached, true, scopes, env);
    }
    let (cols, rhs_affs, rhs_colls, rows) = env.subqueries.run_select_with_meta(select, scopes)?;
    let width = if let Expr::Vector { items, .. } = operand {
        items.len()
    } else if matches!(operand, Expr::Subquery { .. }) {
        let (lvals, _, _) = eval_row_vector(operand, scopes, env)?;
        lvals.len()
    } else {
        1
    };
    if cols.len() != width {
        return Err(SqlError::runtime(format!(
            "sub-select returns {} columns - expected {width}",
            cols.len()
        )));
    }
    eval_in_rows(not, operand, &rhs_affs, &rhs_colls, &rows, scopes, env)
}

fn eval_in_rows(
    not: bool,
    operand: &Expr,
    rhs_affs: &[Option<Affinity>],
    rhs_colls: &[ExprCollation],
    rows: &[Vec<SqlValue>],
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    let (lvals, laffs, lcolls) = eval_row_vector(operand, scopes, env)?;
    let width = lvals.len();
    if !rhs_affs.is_empty() && rhs_affs.len() != width {
        return Err(SqlError::runtime(format!(
            "sub-select returns {} columns - expected {width}",
            rhs_affs.len()
        )));
    }
    if rows.is_empty() {
        return Ok((SqlValue::Int(i64::from(not)), false));
    }
    let eff_affs: Vec<Affinity> = (0..width)
        .map(|i| in_affinity(laffs[i], Some(rhs_affs.get(i).copied().flatten())))
        .collect();
    let eff_colls: Vec<Option<&Collation>> = (0..width)
        .map(|i| {
            let rcoll = rhs_colls.get(i).and_then(ExprCollation::as_operand);
            let cname = comparison_collation_name(lcolls[i].as_operand(), rcoll);
            env.lookup_opt_collation(cname)
        })
        .collect::<SqlResult<Vec<_>>>()?;
    let lvals_coerced: Vec<SqlValue> = lvals
        .into_iter()
        .enumerate()
        .map(|(i, v)| apply_affinity(v, eff_affs[i]))
        .collect();

    let mut saw_null = false;
    for row in rows {
        env.fn_ctx.charge_steps(1)?;
        if row.len() != width {
            return Err(SqlError::runtime("row value width mismatch in IN"));
        }
        let mut row_null = false;
        let mut row_match = true;
        for i in 0..width {
            let lv = &lvals_coerced[i];
            let rv = apply_affinity(row[i].clone(), eff_affs[i]);
            if lv.is_null() || rv.is_null() {
                row_null = true;
                continue;
            }
            if mem_compare(lv, &rv, eff_colls[i]) != Ordering::Equal {
                row_match = false;
                break;
            }
        }
        if row_match && !row_null {
            return Ok((SqlValue::Int(i64::from(!not)), false));
        }
        if row_match && row_null {
            saw_null = true;
        }
    }
    if saw_null {
        Ok((SqlValue::Null, false))
    } else {
        Ok((SqlValue::Int(i64::from(not)), false))
    }
}

/// Evaluate a standalone `VIRTUAL` generated column expression (`sqlite3ExprCodeGeneratedColumn`)
/// against a table row, enforcing SQLite's rules:
/// - `dqs = true` (double-quoted strings allowed in schema DDL expressions)
/// - `pure_context = Some("a generated column")` (forbids `random()`, `'now'`, etc.)
/// - `no_rowid_aliases = true` (`NC_GenCol`: cannot reference `rowid`/`_rowid_`/`oid` unless declared)
/// - Result passes through `apply_affinity(..., col_affinity)`
pub fn eval_generated_expr(
    expr: &Expr,
    columns: &[ScopeColumn],
    row_values: &[SqlValue],
    target_affinity: Affinity,
    options: &QueryOptions,
) -> SqlResult<SqlValue> {
    let params = QueryParams::empty();
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(options, &params, &no_sub);
    env.dqs = true;
    env.fn_ctx.pure_context = Some("a generated column");

    let mut scope = EvalScope::simple(columns, row_values);
    scope.no_rowid_aliases = true;
    let scopes = [scope];
    let raw = eval_expr(expr, &scopes, &env)?;
    Ok(apply_affinity(raw, target_affinity))
}

/// Convenience helper: check whether a catalog has a table/view (used by planner/exec).
pub fn is_valid_schema_name(schema: Option<&str>) -> bool {
    match schema {
        None => true,
        Some(s) => s.eq_ignore_ascii_case("main") || s.eq_ignore_ascii_case("temp"),
    }
}

/// Helper to check whether `expr` contains any aggregate function call (not inside a subquery
/// or window `OVER (...)`).
pub fn expr_contains_aggregate(expr: &Expr, options: &QueryOptions) -> bool {
    match expr {
        Expr::Function(f) => {
            if f.over.is_some() {
                // Window function's args or partition/order might contain aggregates (e.g. `sum(sum(x)) OVER ()`),
                // check args/window expressions!
                if f.args.iter().any(|a| expr_contains_aggregate(a, options)) {
                    return true;
                }
                if let Some(ref flt) = f.filter {
                    if expr_contains_aggregate(flt, options) {
                        return true;
                    }
                }
                if let Some(xdb_parser::WindowRef::Spec(ws)) = &f.over {
                    if let Some(ref pb) = ws.partition_by {
                        if pb.iter().any(|e| expr_contains_aggregate(e, options)) {
                            return true;
                        }
                    }
                    if let Some(ref ob) = ws.order_by {
                        if ob.iter().any(|o| expr_contains_aggregate(&o.expr, options)) {
                            return true;
                        }
                    }
                }
                return false;
            }
            if options
                .functions
                .is_aggregate(&f.name.name, f.args.len(), f.star)
            {
                return true;
            }
            f.args.iter().any(|a| expr_contains_aggregate(a, options))
                || f.filter
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e, options))
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_contains_aggregate(operand, options),
        Expr::Binary { left, right, .. } => {
            expr_contains_aggregate(left, options) || expr_contains_aggregate(right, options)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_contains_aggregate(left, options)
                || expr_contains_aggregate(right, options)
                || escape
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e, options))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_contains_aggregate(operand, options)
                || expr_contains_aggregate(low, options)
                || expr_contains_aggregate(high, options)
        }
        Expr::InList { operand, items, .. } => {
            expr_contains_aggregate(operand, options)
                || items.iter().any(|i| expr_contains_aggregate(i, options))
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            expr_contains_aggregate(operand, options)
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|e| expr_contains_aggregate(e, options))
                || whens.iter().any(|w| {
                    expr_contains_aggregate(&w.when, options)
                        || expr_contains_aggregate(&w.then, options)
                })
                || else_expr
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e, options))
        }
        Expr::Vector { items, .. } => items.iter().any(|i| expr_contains_aggregate(i, options)),
        Expr::Raise { message, .. } => message
            .as_ref()
            .is_some_and(|m| expr_contains_aggregate(m, options)),
        // Subqueries have their own aggregation scope.
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::CurrentTime { .. }
        | Expr::Variable(_)
        | Expr::Column(_) => false,
    }
}

/// Helper to check whether `expr` contains a direct (non-window-internal) aggregate function.
/// Used by SQLite's `HAVING` check, where `sum(a) OVER (ORDER BY count(*))` does NOT satisfy
/// `HAVING` without `GROUP BY`.
pub fn expr_contains_non_window_aggregate(expr: &Expr, options: &QueryOptions) -> bool {
    match expr {
        Expr::Function(f) => {
            if f.over.is_some() {
                return false;
            }
            options
                .functions
                .is_aggregate(&f.name.name, f.args.len(), f.star)
                || f.args
                    .iter()
                    .any(|a| expr_contains_non_window_aggregate(a, options))
                || f.filter
                    .as_ref()
                    .is_some_and(|e| expr_contains_non_window_aggregate(e, options))
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_contains_non_window_aggregate(operand, options),
        Expr::Binary { left, right, .. } => {
            expr_contains_non_window_aggregate(left, options)
                || expr_contains_non_window_aggregate(right, options)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_contains_non_window_aggregate(left, options)
                || expr_contains_non_window_aggregate(right, options)
                || escape
                    .as_ref()
                    .is_some_and(|e| expr_contains_non_window_aggregate(e, options))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_contains_non_window_aggregate(operand, options)
                || expr_contains_non_window_aggregate(low, options)
                || expr_contains_non_window_aggregate(high, options)
        }
        Expr::InList { operand, items, .. } => {
            expr_contains_non_window_aggregate(operand, options)
                || items
                    .iter()
                    .any(|i| expr_contains_non_window_aggregate(i, options))
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            expr_contains_non_window_aggregate(operand, options)
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|e| expr_contains_non_window_aggregate(e, options))
                || whens.iter().any(|w| {
                    expr_contains_non_window_aggregate(&w.when, options)
                        || expr_contains_non_window_aggregate(&w.then, options)
                })
                || else_expr
                    .as_ref()
                    .is_some_and(|e| expr_contains_non_window_aggregate(e, options))
        }
        Expr::Vector { items, .. } => items
            .iter()
            .any(|i| expr_contains_non_window_aggregate(i, options)),
        Expr::Raise { message, .. } => message
            .as_ref()
            .is_some_and(|m| expr_contains_non_window_aggregate(m, options)),
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::CurrentTime { .. }
        | Expr::Variable(_)
        | Expr::Column(_) => false,
    }
}

/// Helper to check whether `expr` contains any window function (`OVER ...`).
pub fn expr_contains_window(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            f.over.is_some()
                || f.args.iter().any(expr_contains_window)
                || f.filter.as_deref().is_some_and(expr_contains_window)
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_contains_window(operand),
        Expr::Binary { left, right, .. } => {
            expr_contains_window(left) || expr_contains_window(right)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_contains_window(left)
                || expr_contains_window(right)
                || escape.as_deref().is_some_and(expr_contains_window)
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_contains_window(operand)
                || expr_contains_window(low)
                || expr_contains_window(high)
        }
        Expr::InList { operand, items, .. } => {
            expr_contains_window(operand) || items.iter().any(expr_contains_window)
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            expr_contains_window(operand)
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand.as_deref().is_some_and(expr_contains_window)
                || whens
                    .iter()
                    .any(|w| expr_contains_window(&w.when) || expr_contains_window(&w.then))
                || else_expr.as_deref().is_some_and(expr_contains_window)
        }
        Expr::Vector { items, .. } => items.iter().any(expr_contains_window),
        Expr::Raise { message, .. } => message.as_deref().is_some_and(expr_contains_window),
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::CurrentTime { .. }
        | Expr::Variable(_)
        | Expr::Column(_) => false,
    }
}

/// Collect all non-window aggregate function calls (`&FunctionExpr`) inside `expr`.
pub fn collect_aggregates<'a>(
    expr: &'a Expr,
    options: &QueryOptions,
    out: &mut Vec<&'a xdb_parser::FunctionExpr>,
) {
    match expr {
        Expr::Function(f) => {
            if f.over.is_some() {
                for a in &f.args {
                    collect_aggregates(a, options, out);
                }
                if let Some(ref flt) = f.filter {
                    collect_aggregates(flt, options, out);
                }
                if let Some(xdb_parser::WindowRef::Spec(ws)) = &f.over {
                    if let Some(ref pb) = ws.partition_by {
                        for e in pb {
                            collect_aggregates(e, options, out);
                        }
                    }
                    if let Some(ref ob) = ws.order_by {
                        for o in ob {
                            collect_aggregates(&o.expr, options, out);
                        }
                    }
                }
                return;
            }
            if options
                .functions
                .is_aggregate(&f.name.name, f.args.len(), f.star)
            {
                out.push(f);
                return;
            }
            for a in &f.args {
                collect_aggregates(a, options, out);
            }
            if let Some(ref flt) = f.filter {
                collect_aggregates(flt, options, out);
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => collect_aggregates(operand, options, out),
        Expr::Binary { left, right, .. } => {
            collect_aggregates(left, options, out);
            collect_aggregates(right, options, out);
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_aggregates(left, options, out);
            collect_aggregates(right, options, out);
            if let Some(e) = escape {
                collect_aggregates(e, options, out);
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_aggregates(operand, options, out);
            collect_aggregates(low, options, out);
            collect_aggregates(high, options, out);
        }
        Expr::InList { operand, items, .. } => {
            collect_aggregates(operand, options, out);
            for item in items {
                collect_aggregates(item, options, out);
            }
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            collect_aggregates(operand, options, out);
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(e) = operand {
                collect_aggregates(e, options, out);
            }
            for w in whens {
                collect_aggregates(&w.when, options, out);
                collect_aggregates(&w.then, options, out);
            }
            if let Some(e) = else_expr {
                collect_aggregates(e, options, out);
            }
        }
        Expr::Vector { items, .. } => {
            for item in items {
                collect_aggregates(item, options, out);
            }
        }
        Expr::Raise {
            message: Some(m), ..
        } => {
            collect_aggregates(m, options, out);
        }
        _ => {}
    }
}

/// Collect all window function calls (`f.over.is_some()`) inside `expr`.
pub fn collect_windows<'a>(expr: &'a Expr, out: &mut Vec<&'a xdb_parser::FunctionExpr>) {
    match expr {
        Expr::Function(f) => {
            if f.over.is_some() {
                out.push(f);
            } else {
                for a in &f.args {
                    collect_windows(a, out);
                }
                if let Some(ref flt) = f.filter {
                    collect_windows(flt, out);
                }
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => collect_windows(operand, out),
        Expr::Binary { left, right, .. } => {
            collect_windows(left, out);
            collect_windows(right, out);
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_windows(left, out);
            collect_windows(right, out);
            if let Some(e) = escape {
                collect_windows(e, out);
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_windows(operand, out);
            collect_windows(low, out);
            collect_windows(high, out);
        }
        Expr::InList { operand, items, .. } => {
            collect_windows(operand, out);
            for item in items {
                collect_windows(item, out);
            }
        }
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            collect_windows(operand, out);
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(e) = operand {
                collect_windows(e, out);
            }
            for w in whens {
                collect_windows(&w.when, out);
                collect_windows(&w.then, out);
            }
            if let Some(e) = else_expr {
                collect_windows(e, out);
            }
        }
        Expr::Vector { items, .. } => {
            for item in items {
                collect_windows(item, out);
            }
        }
        Expr::Raise {
            message: Some(m), ..
        } => {
            collect_windows(m, out);
        }
        _ => {}
    }
}

/// Pre-compiled hot single-table row predicate for zero-interpretation `WHERE` filtering.
/// Returns `Ok(None)` from [`CompiledRowPred::try_compile`] for any expression outside the
/// fast subset so callers seamlessly fall back to [`eval_expr`].
pub(crate) struct CompiledRowPred<'ast, 'env>(CompiledRowPredKind<'ast, 'env>);

enum CompiledRowPredKind<'ast, 'env> {
    And(Vec<CompiledRowPredKind<'ast, 'env>>),
    ColCmpInt {
        col: usize,
        aff: Affinity,
        coll: Option<&'env Collation>,
        op: CompareOp,
        rhs: i64,
    },
    ColBetweenInt {
        col: usize,
        aff: Affinity,
        coll: Option<&'env Collation>,
        not: bool,
        low: i64,
        high: i64,
    },
    ColInSelect {
        col: usize,
        not: bool,
        state: Box<InSelectPredState<'ast, 'env>>,
    },
}

enum InSelectPredState<'ast, 'env> {
    Pending {
        select: &'ast SelectStmt,
        col_aff: Affinity,
        col_coll: Option<String>,
    },
    Empty,
    ReadyIntBitset {
        min: i64,
        max: i64,
        has_null: bool,
        bits: Rc<[u64]>,
        eff_aff: Affinity,
        coll: Option<&'env Collation>,
        lookup: Rc<SortedInLookup<'env>>,
    },
    Ready {
        eff_aff: Affinity,
        coll: Option<&'env Collation>,
        lookup: Rc<SortedInLookup<'env>>,
    },
}

fn resolve_pred_col(
    expr: &Expr,
    columns: &[ScopeColumn],
    using_hidden: &[bool],
    source_ids: &[u32],
) -> Option<usize> {
    let Expr::Column(c) = expr else {
        return None;
    };
    if let Some(ref ws) = c.schema {
        if !is_valid_schema_name(Some(ws.name.as_str())) {
            return None;
        }
    }
    let wt = c.table.as_ref().map(|n| n.name.as_str());
    let wc = c.column.name.as_str();
    let mut found: Option<usize> = None;
    for (idx, col) in columns.iter().enumerate() {
        if wt.is_none() && using_hidden.get(idx).copied().unwrap_or(false) {
            continue;
        }
        if let Some(t) = wt {
            if col.table.as_deref().is_none_or(|ct| !ct.eq_ignore_ascii_case(t)) {
                continue;
            }
        }
        if col.name.eq_ignore_ascii_case(wc) {
            if let Some(prev) = found {
                if columns[prev].is_rowid_alias && !col.is_rowid_alias {
                    found = Some(idx);
                } else if !columns[prev].is_rowid_alias && col.is_rowid_alias {
                    // keep real column
                } else {
                    let same_source = source_ids
                        .get(prev)
                        .copied()
                        .is_some_and(|a| source_ids.get(idx).copied() == Some(a));
                    if !same_source {
                        return None;
                    }
                }
            } else {
                found = Some(idx);
            }
        }
    }
    found
}

fn pred_const_i64(expr: &Expr, env: &EvalEnv<'_>) -> Option<i64> {
    match expr {
        Expr::Number(n) if !n.is_float => match integer_literal_value(n.text.as_bytes(), false) {
            Some(SqlValue::Int(i)) => Some(i),
            _ => None,
        },
        Expr::Unary {
            op: UnaryOp::Pos,
            operand,
            ..
        } => pred_const_i64(operand, env),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => {
            if let Expr::Number(n) = operand.as_ref() {
                if !n.is_float {
                    return match integer_literal_value(n.text.as_bytes(), true) {
                        Some(SqlValue::Int(i)) => Some(i),
                        _ => None,
                    };
                }
            }
            None
        }
        Expr::Variable(v) => match env.params.resolve(v.index, &v.text) {
            SqlValue::Int(i) => Some(i),
            _ => None,
        },
        _ => None,
    }
}

fn pred_cmp_op(op: BinaryOp) -> Option<CompareOp> {
    match op {
        BinaryOp::Eq => Some(CompareOp::Eq),
        BinaryOp::Ne => Some(CompareOp::Ne),
        BinaryOp::Lt => Some(CompareOp::Lt),
        BinaryOp::Le => Some(CompareOp::Le),
        BinaryOp::Gt => Some(CompareOp::Gt),
        BinaryOp::Ge => Some(CompareOp::Ge),
        _ => None,
    }
}

fn flip_compare_op(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
        other => other,
    }
}

impl<'ast, 'env> CompiledRowPred<'ast, 'env> {
    pub(crate) fn try_compile(
        expr: &'ast Expr,
        columns: &[ScopeColumn],
        using_hidden: &[bool],
        source_ids: &[u32],
        env: &EvalEnv<'env>,
    ) -> SqlResult<Option<Self>> {
        CompiledRowPredKind::try_compile(expr, columns, using_hidden, source_ids, env)
            .map(|opt| opt.map(CompiledRowPred))
    }

    #[inline]
    pub(crate) fn has_subquery(&self) -> bool {
        self.0.has_subquery()
    }

    #[inline]
    pub(crate) fn eval_row(&mut self, values: &[SqlValue], env: &EvalEnv<'env>) -> SqlResult<bool> {
        self.0.eval_row(values, env)
    }

    #[inline]
    pub(crate) fn as_single_int_cmp(&self) -> Option<(usize, CompareOp, i64)> {
        match self.0 {
            CompiledRowPredKind::ColCmpInt {
                col,
                aff,
                op,
                rhs,
                ..
            } if aff != Affinity::Text && aff != Affinity::Real => Some((col, op, rhs)),
            _ => None,
        }
    }
}

impl<'ast, 'env> CompiledRowPredKind<'ast, 'env> {
    #[inline]
    fn has_subquery(&self) -> bool {
        match self {
            CompiledRowPredKind::And(parts) => parts.iter().any(Self::has_subquery),
            CompiledRowPredKind::ColInSelect { .. } => true,
            _ => false,
        }
    }

    fn try_compile(
        expr: &'ast Expr,
        columns: &[ScopeColumn],
        using_hidden: &[bool],
        source_ids: &[u32],
        env: &EvalEnv<'env>,
    ) -> SqlResult<Option<Self>> {
        match expr {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
                ..
            } => {
                let mut parts = Vec::with_capacity(2);
                let Some(l) = Self::try_compile(left, columns, using_hidden, source_ids, env)?
                else {
                    return Ok(None);
                };
                match l {
                    CompiledRowPredKind::And(v) => parts.extend(v),
                    other => parts.push(other),
                }
                let Some(r) = Self::try_compile(right, columns, using_hidden, source_ids, env)?
                else {
                    return Ok(None);
                };
                match r {
                    CompiledRowPredKind::And(v) => parts.extend(v),
                    other => parts.push(other),
                }
                Ok(Some(CompiledRowPredKind::And(parts)))
            }
            Expr::Binary {
                op, left, right, ..
            } => {
                let Some(cmp) = pred_cmp_op(*op) else {
                    return Ok(None);
                };
                let (col, cmp_op, rhs) = if let (Some(c), Some(v)) = (
                    resolve_pred_col(left, columns, using_hidden, source_ids),
                    pred_const_i64(right, env),
                ) {
                    (c, cmp, v)
                } else if let (Some(v), Some(c)) = (
                    pred_const_i64(left, env),
                    resolve_pred_col(right, columns, using_hidden, source_ids),
                ) {
                    (c, flip_compare_op(cmp), v)
                } else {
                    return Ok(None);
                };
                let aff = comparison_affinity(Some(columns[col].affinity), None);
                let cname = (!columns[col].collation.is_empty())
                    .then_some(columns[col].collation.as_str());
                let coll = env.lookup_opt_collation(cname)?;
                Ok(Some(CompiledRowPredKind::ColCmpInt {
                    col,
                    aff,
                    coll,
                    op: cmp_op,
                    rhs,
                }))
            }
            Expr::Between {
                not,
                operand,
                low,
                high,
                ..
            } => {
                let Some(col) = resolve_pred_col(operand, columns, using_hidden, source_ids) else {
                    return Ok(None);
                };
                let (Some(lo), Some(hi)) = (pred_const_i64(low, env), pred_const_i64(high, env))
                else {
                    return Ok(None);
                };
                let aff = comparison_affinity(Some(columns[col].affinity), None);
                let cname = (!columns[col].collation.is_empty())
                    .then_some(columns[col].collation.as_str());
                let coll = env.lookup_opt_collation(cname)?;
                Ok(Some(CompiledRowPredKind::ColBetweenInt {
                    col,
                    aff,
                    coll,
                    not: *not,
                    low: lo,
                    high: hi,
                }))
            }
            Expr::InSelect {
                not,
                operand,
                select,
                ..
            } => {
                if !env.cache_subqueries {
                    return Ok(None);
                }
                let Some(col) = resolve_pred_col(operand, columns, using_hidden, source_ids) else {
                    return Ok(None);
                };
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
                sample.source_ids = Some(source_ids);
                if !env.subqueries.is_select_uncorrelated(select, &[sample]) {
                    return Ok(None);
                }
                // Defer subquery execution until the first row actually reaches this predicate
                // (so 0-row tables and short-circuited `AND` branches never evaluate runtime
                // subquery errors; compile-time schema validation is run by callers).
                let col_aff = columns[col].affinity;
                let col_coll = (!columns[col].collation.is_empty())
                    .then(|| columns[col].collation.clone());
                Ok(Some(CompiledRowPredKind::ColInSelect {
                    col,
                    not: *not,
                    state: Box::new(InSelectPredState::Pending {
                        select: select.as_ref(),
                        col_aff,
                        col_coll,
                    }),
                }))
            }
            _ => Ok(None),
        }
    }

    #[inline]
    fn eval_row(&mut self, values: &[SqlValue], env: &EvalEnv<'env>) -> SqlResult<bool> {
        match self {
            CompiledRowPredKind::And(parts) => {
                for p in parts {
                    if !p.eval_row(values, env)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            CompiledRowPredKind::ColCmpInt {
                col,
                aff,
                coll,
                op,
                rhs,
            } => {
                if env.fn_ctx.meter.is_some() {
                    env.fn_ctx.charge_steps(1)?;
                }
                let v = &values[*col];
                if *aff != Affinity::Text && *aff != Affinity::Real {
                    if let SqlValue::Int(a) = *v {
                        return Ok(match op {
                            CompareOp::Eq | CompareOp::Is => a == *rhs,
                            CompareOp::Ne | CompareOp::IsNot => a != *rhs,
                            CompareOp::Lt => a < *rhs,
                            CompareOp::Le => a <= *rhs,
                            CompareOp::Gt => a > *rhs,
                            CompareOp::Ge => a >= *rhs,
                        });
                    }
                }
                if v.is_null() {
                    return Ok(false);
                }
                Ok(compare_op(*op, v.clone(), SqlValue::Int(*rhs), Some(*aff), *coll) == Some(true))
            }
            CompiledRowPredKind::ColBetweenInt {
                col,
                aff,
                coll,
                not,
                low,
                high,
            } => {
                if env.fn_ctx.meter.is_some() {
                    env.fn_ctx.charge_steps(1)?;
                }
                let v = &values[*col];
                if *aff != Affinity::Text && *aff != Affinity::Real {
                    if let SqlValue::Int(a) = *v {
                        return Ok(if *not {
                            a < *low || a > *high
                        } else {
                            a >= *low && a <= *high
                        });
                    }
                }
                if v.is_null() {
                    return Ok(false);
                }
                let lo_ok =
                    compare_op(CompareOp::Ge, v.clone(), SqlValue::Int(*low), Some(*aff), *coll);
                let hi_ok =
                    compare_op(CompareOp::Le, v.clone(), SqlValue::Int(*high), Some(*aff), *coll);
                Ok(if *not {
                    lo_ok == Some(false) || hi_ok == Some(false)
                } else {
                    lo_ok == Some(true) && hi_ok == Some(true)
                })
            }
            CompiledRowPredKind::ColInSelect { col, not, state } => {
                if env.fn_ctx.meter.is_some() {
                    env.fn_ctx.charge_steps(1)?;
                }
                loop {
                    match state.as_mut() {
                        InSelectPredState::ReadyIntBitset {
                            min,
                            max,
                            has_null,
                            bits,
                            eff_aff,
                            coll,
                            lookup,
                        } => {
                            let val = &values[*col];
                            if let SqlValue::Int(i) = *val {
                                let found = i >= *min && i <= *max && {
                                    let off = i.wrapping_sub(*min) as u64 as usize;
                                    (bits[off >> 6] >> (off & 63)) & 1 != 0
                                };
                                return Ok(if found {
                                    !*not
                                } else if *has_null {
                                    false
                                } else {
                                    *not
                                });
                            }
                            if val.is_null() {
                                return Ok(false);
                            }
                            let coerced = apply_affinity(val.clone(), *eff_aff);
                            let found = lookup.contains_coerced(&coerced, *coll);
                            return Ok(if found {
                                !*not
                            } else if *has_null {
                                false
                            } else {
                                *not
                            });
                        }
                        InSelectPredState::Ready {
                            eff_aff,
                            coll,
                            lookup,
                        } => {
                            let val = &values[*col];
                            if val.is_null() {
                                return Ok(false);
                            }
                            let found = if *eff_aff != Affinity::Text && *eff_aff != Affinity::Real
                            {
                                match (&lookup.index, val) {
                                    (
                                        InLookupIndex::IntBitset { min, max, bits },
                                        SqlValue::Int(i),
                                    ) => {
                                        *i >= *min && *i <= *max && {
                                            let off = i.wrapping_sub(*min) as u64 as usize;
                                            (bits[off >> 6] >> (off & 63)) & 1 != 0
                                        }
                                    }
                                    (InLookupIndex::IntSorted(sorted), SqlValue::Int(i)) => {
                                        sorted.binary_search(i).is_ok()
                                    }
                                    _ => {
                                        let coerced = apply_affinity(val.clone(), *eff_aff);
                                        lookup.contains_coerced(&coerced, *coll)
                                    }
                                }
                            } else {
                                let coerced = apply_affinity(val.clone(), *eff_aff);
                                lookup.contains_coerced(&coerced, *coll)
                            };
                            return Ok(if found {
                                !*not
                            } else if lookup.has_null {
                                false
                            } else {
                                *not
                            });
                        }
                        InSelectPredState::Empty => return Ok(*not),
                        InSelectPredState::Pending {
                            select,
                            col_aff,
                            col_coll,
                        } => {
                            let select = *select;
                            let col_aff = *col_aff;
                            // Fast streaming 1-column `IN (SELECT col FROM tbl)` builder: skips
                            // `Vec<Vec<SqlValue>>` allocation and full `exec_select_stmt` setup!
                            if let Some((rhs_aff, rhs_coll, vals)) =
                                env.subqueries.try_scan_1col_in_select(select)?
                            {
                                **state = if vals.is_empty() {
                                    InSelectPredState::Empty
                                } else {
                                    let eff_aff = in_affinity(Some(col_aff), Some(rhs_aff));
                                    let lcoll = col_coll.as_deref().map(|n| OperandCollation {
                                        name: Some(n),
                                        explicit: false,
                                    });
                                    let cname =
                                        comparison_collation_name(lcoll, rhs_coll.as_operand());
                                    let coll = env.lookup_opt_collation(cname)?;
                                    let lookup = build_1col_lookup_from_vals(
                                        &vals, eff_aff, cname, coll, env,
                                    )?;
                                    if eff_aff != Affinity::Text && eff_aff != Affinity::Real {
                                        if let InLookupIndex::IntBitset { min, max, ref bits } =
                                            lookup.index
                                        {
                                            InSelectPredState::ReadyIntBitset {
                                                min,
                                                max,
                                                has_null: lookup.has_null,
                                                bits: Rc::clone(bits),
                                                eff_aff,
                                                coll,
                                                lookup,
                                            }
                                        } else {
                                            InSelectPredState::Ready {
                                                eff_aff,
                                                coll,
                                                lookup,
                                            }
                                        }
                                    } else {
                                        InSelectPredState::Ready {
                                            eff_aff,
                                            coll,
                                            lookup,
                                        }
                                    }
                                };
                                continue;
                            }
                            let key = (
                                select as *const SelectStmt as usize,
                                select.span.start,
                                select.span.end,
                            );
                            let cached = env
                                .get_cached_select_verified(select, key)?
                                .ok_or_else(|| {
                                    SqlError::runtime(
                                        "unexpected correlated subquery in compiled predicate",
                                    )
                                })?;
                            if cached.cols.len() != 1 {
                                return Err(SqlError::runtime(format!(
                                    "sub-select returns {} columns - expected 1",
                                    cached.cols.len()
                                )));
                            }
                            if !cached.affs.is_empty() && cached.affs.len() != 1 {
                                return Err(SqlError::runtime(format!(
                                    "sub-select returns {} columns - expected 1",
                                    cached.affs.len()
                                )));
                            }
                            **state = if cached.rows.is_empty() {
                                InSelectPredState::Empty
                            } else {
                                let eff_aff = in_affinity(
                                    Some(col_aff),
                                    Some(cached.affs.first().copied().flatten()),
                                );
                                let lcoll = col_coll.as_deref().map(|n| OperandCollation {
                                    name: Some(n),
                                    explicit: false,
                                });
                                let rcoll =
                                    cached.colls.first().and_then(ExprCollation::as_operand);
                                let cname = comparison_collation_name(lcoll, rcoll);
                                let coll = env.lookup_opt_collation(cname)?;
                                let lookup =
                                    cached.get_or_build_1col_lookup(eff_aff, cname, coll, env)?;
                                if eff_aff != Affinity::Text && eff_aff != Affinity::Real {
                                    if let InLookupIndex::IntBitset { min, max, ref bits } =
                                        lookup.index
                                    {
                                        InSelectPredState::ReadyIntBitset {
                                            min,
                                            max,
                                            has_null: lookup.has_null,
                                            bits: Rc::clone(bits),
                                            eff_aff,
                                            coll,
                                            lookup,
                                        }
                                    } else {
                                        InSelectPredState::Ready {
                                            eff_aff,
                                            coll,
                                            lookup,
                                        }
                                    }
                                } else {
                                    InSelectPredState::Ready {
                                        eff_aff,
                                        coll,
                                        lookup,
                                    }
                                }
                            };
                        }
                    }
                }
            }
        }
    }
}

/// Ensure `SqlCatalog` trait is imported cleanly if needed by callers.
#[allow(dead_code)]
fn _assert_catalog_object_safe(_: &dyn SqlCatalog) {}
