//! Full SQLite 3.54.1 + XDB dialect `SELECT` query planner and executor.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use xdb_core::value::{
    apply_affinity, apply_numeric_affinity, boolean_value, compare_op, comparison_affinity,
    comparison_collation_name, int64_to_text, mem_compare, must_be_int, real_to_exact_i64,
    real_to_text, BuiltinCollation, Collation, CompareOp, OperandCollation, TextEncoding,
    DEFAULT_FP_DIGITS,
};
use xdb_core::{Affinity, Meter, SqlValue};
use xdb_parser::{
    join_flag, parse_statement_metered, BinaryOp, CommonTableExpr, CompoundOperator, DistinctKind,
    Expr, FrameBoundType, FrameExclude, FrameUnits, FromSource, FunctionExpr, LikeOperator,
    LimitClause, NullsOrder, OrderByAll, OrderingTerm, ParseOptions, ResultColumn, SelectBody,
    SelectCore, SelectStmt, SortOrder, Statement, WindowDefinition, WindowRef, WindowSpec,
    WithClause,
};

use crate::catalog::{Bound, QueryOptions, QueryParams, QueryResult, SqlCatalog, TableScanHint};
use crate::errors::{SqlError, SqlResult};
use crate::eval::{
    collect_aggregates, collect_windows, eval_expr, expr_affinity, expr_collation,
    expr_contains_aggregate, expr_contains_non_window_aggregate, expr_contains_window,
    is_valid_schema_name, resolve_column, CompiledRowPred, EvalEnv, EvalScope, ExprCollation,
    ScopeColumn, SelectWithMeta, SubqueryRunner, TableRowsWithMeta,
};
use crate::functions::{
    aggregates::{SumAcc, SumKind},
    call_builtin_tvf, compute_frame_bounds, compute_frame_rows, eval_window_only_partition,
    is_builtin_window_only, json::call_builtin_tvf_with_limit, AggregateAcc, PartitionPeers,
    ResolvedFrame,
};
use crate::utf8::{estimate_value_bytes, MemoryGuard};

/// Maximum nested query/compound/join recursion depth to prevent host stack overflow.
const MAX_QUERY_DEPTH: usize = 128;

/// Materialized intermediate relation during query execution.
#[derive(Debug, Clone)]
struct Relation {
    columns: Vec<ScopeColumn>,
    /// Columns hidden from both `*` and `t.*` (hidden table columns, synthetic rowid aliases).
    star_hidden: Vec<bool>,
    /// Columns hidden from unqualified `*` and unqualified name lookup (e.g. `USING` duplicates),
    /// while remaining visible to qualified `t.col` and `t.*`.
    using_hidden: Vec<bool>,
    /// Source-table instance ID per column (distinguishes self-joins from same-subquery duplicate names).
    source_ids: Vec<u32>,
    rows: Vec<Vec<SqlValue>>,
}

#[derive(Debug, Clone)]
struct WindowCol {
    span_start: usize,
    span_end: usize,
    values: Vec<SqlValue>,
}

enum WindowSink<'a> {
    PerRow(&'a mut [PrecomputedRow]),
    Columnar(&'a mut Vec<WindowCol>),
}

enum StreamedGroupBy {
    Direct(SelectCoreResult),
    Rows((Relation, Vec<ProjectedItem>, Vec<Vec<SqlValue>>, Vec<PrecomputedRow>)),
}

struct PrecomputedRow {
    slots: Vec<(usize, usize, SqlValue)>,
    is_json: Vec<bool>,
}

impl PrecomputedRow {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            is_json: Vec::new(),
        }
    }

    fn with_capacity(cap: usize) -> Self {
        Self {
            slots: Vec::with_capacity(cap),
            is_json: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, start: usize, end: usize, val: SqlValue, json: bool) {
        self.slots.push((start, end, val));
        self.is_json.push(json);
    }
}

impl Relation {
    fn single_empty_row() -> Self {
        Self {
            columns: Vec::new(),
            star_hidden: Vec::new(),
            using_hidden: Vec::new(),
            source_ids: Vec::new(),
            rows: vec![Vec::new()],
        }
    }
}

/// Materialized CTE entry visible in the current query scope.
#[derive(Debug, Clone)]
struct OwnedMemGuard {
    meter: Option<Arc<dyn Meter>>,
    reserved: u64,
}

impl OwnedMemGuard {
    fn new(meter: Option<&Arc<dyn Meter>>, bytes: u64) -> SqlResult<Self> {
        if bytes > 0 {
            if let Some(m) = meter {
                m.reserve_memory(bytes)?;
                return Ok(Self {
                    meter: Some(Arc::clone(m)),
                    reserved: bytes,
                });
            }
        }
        Ok(Self {
            meter: None,
            reserved: 0,
        })
    }

    fn empty() -> Self {
        Self {
            meter: None,
            reserved: 0,
        }
    }
}

impl Drop for OwnedMemGuard {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(ref m) = self.meter {
                m.release_memory(self.reserved);
            }
            self.reserved = 0;
        }
    }
}

struct CteTable {
    columns: Vec<ScopeColumn>,
    rows: Vec<Vec<SqlValue>>,
    _mem: OwnedMemGuard,
}

impl CteTable {
    fn new_metered(
        columns: Vec<ScopeColumn>,
        rows: Vec<Vec<SqlValue>>,
        meter: Option<&Arc<dyn Meter>>,
    ) -> SqlResult<Self> {
        let bytes = if meter.is_some() {
            rows.iter()
                .fold(0u64, |acc, r| acc.saturating_add(estimate_row_bytes(r)))
        } else {
            0
        };
        let _mem = OwnedMemGuard::new(meter, bytes)?;
        Ok(Self {
            columns,
            rows,
            _mem,
        })
    }

    fn untracked(columns: Vec<ScopeColumn>, rows: Vec<Vec<SqlValue>>) -> Self {
        Self {
            columns,
            rows,
            _mem: OwnedMemGuard::empty(),
        }
    }
}

/// Public [`SubqueryRunner`] backed by any [`SqlCatalog`], allowing DML statements
/// (`UPDATE`, `DELETE`, `INSERT`) to evaluate correlated and uncorrelated subqueries.
pub struct CatalogSubqueryRunner<'a> {
    exec: QueryExec<'a>,
}

impl std::fmt::Debug for CatalogSubqueryRunner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogSubqueryRunner").finish_non_exhaustive()
    }
}

impl<'a> CatalogSubqueryRunner<'a> {
    pub fn new(
        catalog: &'a dyn SqlCatalog,
        options: &'a QueryOptions,
        params: &'a QueryParams,
    ) -> Self {
        Self {
            exec: QueryExec::new(catalog, options, params),
        }
    }

    /// Build a [`CatalogSubqueryRunner`] pre-populated with any `WITH` CTEs (for `UPDATE`/`DELETE`).
    pub fn with_ctes(
        catalog: &'a dyn SqlCatalog,
        options: &'a QueryOptions,
        params: &'a QueryParams,
        with: Option<&WithClause>,
    ) -> SqlResult<Self> {
        let base = QueryExec::new(catalog, options, params);
        let exec = match with {
            Some(w) => base.materialize_with(w, &[])?,
            None => base,
        };
        Ok(Self { exec })
    }
}

impl SubqueryRunner for CatalogSubqueryRunner<'_> {
    fn run_select(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Vec<SqlValue>>)> {
        self.exec.run_select(stmt, outer_scopes)
    }

    fn run_select_with_affinities(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Option<Affinity>>, Vec<Vec<SqlValue>>)> {
        self.exec.run_select_with_affinities(stmt, outer_scopes)
    }

    fn run_select_with_meta(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectWithMeta> {
        self.exec.run_select_with_meta(stmt, outer_scopes)
    }

    fn run_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        self.exec.run_in_table(schema, table, args)
    }

    fn run_in_table_with_affinities(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<(Vec<Option<Affinity>>, Vec<Vec<SqlValue>>)> {
        self.exec.run_in_table_with_affinities(schema, table, args)
    }

    fn run_in_table_with_meta(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<TableRowsWithMeta> {
        self.exec.run_in_table_with_meta(schema, table, args)
    }

    fn is_select_uncorrelated(&self, stmt: &SelectStmt, outer_scopes: &[EvalScope<'_>]) -> bool {
        self.exec.is_select_uncorrelated(stmt, outer_scopes)
    }

    fn is_in_table_cacheable(&self) -> bool {
        self.exec.is_in_table_cacheable()
    }

    fn is_in_table_target_cacheable(&self, schema: Option<&str>, table: &str) -> bool {
        self.exec.is_in_table_target_cacheable(schema, table)
    }

    fn try_scan_1col_in_select(
        &self,
        stmt: &SelectStmt,
    ) -> SqlResult<Option<crate::eval::OneColInScan>> {
        self.exec.try_scan_1col_in_select(stmt)
    }
}

type SelectRowsMeta = (Vec<ScopeColumn>, Vec<ExprCollation>, Vec<Vec<SqlValue>>);
type SelectCoreResult = (
    Vec<ScopeColumn>,
    Vec<ExprCollation>,
    Vec<Vec<SqlValue>>,
    OrderEvalContext,
);

/// Execution context holding catalog, options, parameters, and active CTE bindings.
struct QueryExec<'a> {
    catalog: &'a dyn SqlCatalog,
    options: &'a QueryOptions,
    params: &'a QueryParams,
    ctes: HashMap<String, Arc<CteTable>>,
    view_depth: usize,
    query_depth: usize,
    now_julian_ms: i64,
}

impl<'a> QueryExec<'a> {
    fn new(
        catalog: &'a dyn SqlCatalog,
        options: &'a QueryOptions,
        params: &'a QueryParams,
    ) -> Self {
        Self {
            catalog,
            options,
            params,
            ctes: HashMap::new(),
            view_depth: 0,
            query_depth: 0,
            now_julian_ms: options.effective_now_julian_ms(),
        }
    }

    fn child_with_ctes(&self, ctes: HashMap<String, Arc<CteTable>>) -> Self {
        Self {
            catalog: self.catalog,
            options: self.options,
            params: self.params,
            ctes,
            view_depth: self.view_depth,
            query_depth: self.query_depth + 1,
            now_julian_ms: self.now_julian_ms,
        }
    }

    fn check_depth(&self, depth: usize) -> SqlResult<()> {
        if depth > MAX_QUERY_DEPTH {
            return Err(SqlError::runtime(format!(
                "query nesting too deep (maximum {MAX_QUERY_DEPTH})"
            )));
        }
        Ok(())
    }

    fn make_mem_guard(&self) -> MemoryGuard<'_> {
        MemoryGuard::new(self.options.meter.as_deref())
    }

    fn charge_steps(&self, n: u64) -> SqlResult<()> {
        if let Some(ref m) = self.options.meter {
            m.charge_steps(n)?;
        }
        Ok(())
    }

    fn make_env(&self) -> EvalEnv<'_> {
        EvalEnv::new_with_now(self.options, self.params, self, self.now_julian_ms)
            .with_subquery_cache(true)
    }

    fn execute_select_stmt(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        let (cols, _expl, rows) = self.execute_select_stmt_meta(stmt, outer_scopes)?;
        Ok((cols, rows))
    }

    fn execute_select_stmt_meta(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectRowsMeta> {
        self.check_depth(self.query_depth)?;
        if let Some(ref with) = stmt.with {
            let child = self.materialize_with(with, outer_scopes)?;
            return child.execute_select_no_with(stmt, outer_scopes);
        }
        self.execute_select_no_with(stmt, outer_scopes)
    }

    fn materialize_with(
        &self,
        with: &WithClause,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<QueryExec<'a>> {
        let mut map = self.ctes.clone();
        let mut seen_in_clause: HashSet<String> = HashSet::with_capacity(with.ctes.len());
        for cte in &with.ctes {
            let lower = cte.name.name.to_ascii_lowercase();
            if !seen_in_clause.insert(lower.clone()) {
                return Err(SqlError::runtime(format!(
                    "duplicate WITH table name: {}",
                    cte.name.name
                )));
            }
            // SQLite treats `WITH` and `WITH RECURSIVE` identically: any CTE whose UNION / UNION ALL
            // branch references itself is evaluated recursively, while invalid self-references error.
            let cte_table = self.eval_recursive_cte(cte, &map, outer_scopes)?;
            map.insert(lower, Arc::new(cte_table));
        }
        Ok(self.child_with_ctes(map))
    }

    fn eval_recursive_cte(
        &self,
        cte: &CommonTableExpr,
        existing: &HashMap<String, Arc<CteTable>>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<CteTable> {
        // If an inner `WITH` inside `cte.select` defines the same CTE name, it shadows the outer
        // CTE name so `cte` is non-recursive and evaluates normally.
        let shadowed_by_inner_with = cte.select.with.as_ref().is_some_and(|w| {
            w.ctes
                .iter()
                .any(|ic| ic.name.name.eq_ignore_ascii_case(&cte.name.name))
        });
        if shadowed_by_inner_with {
            let runner = self.child_with_ctes(existing.clone());
            let (cols, rows) = runner.execute_select_stmt(&cte.select, outer_scopes)?;
            let cte_cols = apply_cte_column_names(cte, &cols)?;
            return CteTable::new_metered(cte_cols, rows, self.options.meter.as_ref());
        }

        // Flatten left-associative compound branches so we handle BOTH single and multiple
        // recursive terms (`base UNION ALL rec1 UNION ALL rec2`), while treating non-self-referencing
        // compound CTEs as normal non-recursive queries!
        let mut flat: Vec<(Option<CompoundOperator>, &SelectBody)> = Vec::new();
        flatten_compound_branches(&cte.select.body, &mut flat);

        // Reject EXCEPT / INTERSECT on any self-referencing recursive branch (SQLite forbids them).
        for (op, b) in flat.iter().skip(1) {
            if matches!(
                op,
                Some(CompoundOperator::Except | CompoundOperator::Intersect)
            ) && select_body_references_table(b, &cte.name.name)
            {
                return Err(SqlError::runtime(format!(
                    "recursive reference in a subquery: {}",
                    cte.name.name
                )));
            }
        }

        let first_rec = flat
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, (op, b))| {
                matches!(
                    op,
                    Some(CompoundOperator::Union | CompoundOperator::UnionAll)
                ) && select_body_references_table(b, &cte.name.name)
            })
            .map(|(i, _)| i);

        if let Some(rec_start) = first_rec {
            if flat[..rec_start]
                .iter()
                .any(|(_, b)| select_body_references_table(b, &cte.name.name))
            {
                return Err(SqlError::runtime(format!(
                    "circular reference: {}",
                    cte.name.name
                )));
            }
            if flat[rec_start..].iter().any(|(op, _)| {
                matches!(
                    op,
                    Some(CompoundOperator::Except | CompoundOperator::Intersect)
                )
            }) {
                return Err(SqlError::runtime(format!(
                    "recursive reference in a subquery: {}",
                    cte.name.name
                )));
            }
            if flat[rec_start..]
                .iter()
                .any(|(_, b)| !select_body_references_table(b, &cte.name.name))
            {
                return Err(SqlError::runtime(format!(
                    "circular reference: {}",
                    cte.name.name
                )));
            }
            if flat[rec_start..]
                .iter()
                .any(|(_, b)| select_body_references_table_in_subquery(b, &cte.name.name))
            {
                return Err(SqlError::runtime(format!(
                    "recursive reference in a subquery: {}",
                    cte.name.name
                )));
            }
            let base_existing = if let Some(ref inner_with) = cte.select.with {
                if inner_with
                    .ctes
                    .iter()
                    .any(|ic| select_stmt_references_table(&ic.select, &cte.name.name))
                {
                    return Err(SqlError::runtime(format!(
                        "circular reference: {}",
                        cte.name.name
                    )));
                }
                let inner_runner = self
                    .child_with_ctes(existing.clone())
                    .materialize_with(inner_with, outer_scopes)?;
                inner_runner.ctes
            } else {
                existing.clone()
            };
            let mut base_body = flat[0].1.clone();
            for (op, b) in &flat[1..rec_start] {
                base_body = SelectBody::Compound {
                    span: base_body.span(),
                    op: op.unwrap(),
                    left: Box::new(base_body),
                    right: Box::new((*b).clone()),
                };
            }
            let base_stmt = SelectStmt {
                span: base_body.span(),
                with: None,
                body: base_body,
                order_by: None,
                order_by_all: None,
                limit: None,
            };
            let rec_branches: Vec<(bool, SelectStmt)> = flat[rec_start..]
                .iter()
                .map(|(op, b)| {
                    (
                        *op == Some(CompoundOperator::Union),
                        SelectStmt {
                            span: b.span(),
                            with: None,
                            body: (*b).clone(),
                            order_by: None,
                            order_by_all: None,
                            limit: None,
                        },
                    )
                })
                .collect();
            let any_distinct = rec_branches.iter().any(|(d, _)| *d);

            let base_runner = self.child_with_ctes(base_existing.clone());
            let (base_cols, base_rows) =
                base_runner.execute_select_stmt(&base_stmt, outer_scopes)?;
            let cte_cols = apply_cte_column_names(cte, &base_cols)?;
            let width = cte_cols.len();
            let env = self.make_env();
            let colls: Vec<&Collation> = cte_cols
                .iter()
                .map(|c| env.lookup_collation(&c.collation))
                .collect::<SqlResult<Vec<_>>>()?;
            let coll_names: Vec<&str> = cte_cols.iter().map(|c| c.collation.as_str()).collect();
            let mut dedup = RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
            let mut mem = self.make_mem_guard();

            let mut all_rows: Vec<Vec<SqlValue>> = Vec::new();
            let mut frontier: Vec<Vec<SqlValue>> = Vec::new();

            let early_stop_rows = if cte.select.order_by_all.is_some() {
                None
            } else if let Some(ref lim) = cte.select.limit {
                let count = eval_limit_int(&lim.count, outer_scopes, &env)?;
                let offset = match &lim.offset {
                    Some(o) => eval_limit_int(o, outer_scopes, &env)?.max(0),
                    None => 0,
                };
                if count == 0 {
                    Some(0)
                } else if count > 0 {
                    Some(usize::try_from(count.saturating_add(offset)).unwrap_or(usize::MAX))
                } else {
                    None
                }
            } else {
                None
            };

            let key = cte.name.name.to_ascii_lowercase();

            if let Some(ref ord_terms) = cte.select.order_by {
                // SQLite recursive CTE `ORDER BY`: min-heap priority-queue traversal popping the
                // minimum frontier row per step in O(log N) time.
                let mut term_colls: Vec<&Collation> = Vec::with_capacity(ord_terms.len());
                for (tidx, term) in ord_terms.iter().enumerate() {
                    if let Some((ord, ov_coll)) = integer_ordinal_with_collation(&term.expr) {
                        if ord < 1 || ord > width {
                            return Err(SqlError::runtime(format!(
                                "{} ORDER BY term out of range - should be between 1 and {width}",
                                ordinal_label(tidx + 1)
                            )));
                        }
                        let cname = ov_coll.unwrap_or(&cte_cols[ord - 1].collation);
                        term_colls.push(env.lookup_collation(cname)?);
                    } else {
                        // SQLite resolves recursive CTE ORDER BY column names against the
                        // initial SELECT's output column names (`base_cols`), not `cte_cols`.
                        let mut sc_vec = outer_scopes.to_vec();
                        sc_vec.push(EvalScope::simple(&base_cols, &[]));
                        let cname = expr_collation(&term.expr, &sc_vec)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        term_colls.push(env.lookup_collation(&cname)?);
                    }
                }
                let compute_keys = |row: &[SqlValue]| -> SqlResult<Vec<SqlValue>> {
                    let mut ks = Vec::with_capacity(ord_terms.len());
                    let mut sc_vec = outer_scopes.to_vec();
                    sc_vec.push(EvalScope::simple(&base_cols, row));
                    for term in ord_terms {
                        if let Some((ord, _)) = integer_ordinal_with_collation(&term.expr) {
                            ks.push(row[ord - 1].clone());
                        } else {
                            ks.push(eval_expr(&term.expr, &sc_vec, &env)?);
                        }
                    }
                    Ok(ks)
                };

                struct PqEntry<'p> {
                    keys: Vec<SqlValue>,
                    row: Vec<SqlValue>,
                    seq: u64,
                    ord_terms: &'p [OrderingTerm],
                    term_colls: &'p [&'p Collation],
                }
                impl PartialEq for PqEntry<'_> {
                    fn eq(&self, other: &Self) -> bool {
                        self.cmp(other) == Ordering::Equal
                    }
                }
                impl Eq for PqEntry<'_> {}
                impl PartialOrd for PqEntry<'_> {
                    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                        Some(self.cmp(other))
                    }
                }
                impl Ord for PqEntry<'_> {
                    fn cmp(&self, other: &Self) -> Ordering {
                        // Reverse for min-heap in Rust's max `BinaryHeap`.
                        compare_sort_keys(&other.keys, &self.keys, self.ord_terms, self.term_colls)
                            .then_with(|| other.seq.cmp(&self.seq))
                    }
                }

                let mut pq: BinaryHeap<PqEntry<'_>> = BinaryHeap::new();
                let mut next_seq: u64 = 0;
                for row in base_rows {
                    self.charge_steps(1)?;
                    if row.len() != width {
                        return Err(SqlError::runtime(
                            "SELECTs to the left and right of UNION do not have the same number of result columns",
                        ));
                    }
                    if any_distinct && !dedup.insert(&row, self)? {
                        continue;
                    }
                    mem.reserve(estimate_row_bytes(&row))?;
                    let ks = compute_keys(&row)?;
                    pq.push(PqEntry {
                        keys: ks,
                        row,
                        seq: next_seq,
                        ord_terms,
                        term_colls: &term_colls,
                    });
                    next_seq += 1;
                }

                let mut iter = 0usize;
                while let Some(entry) = pq.pop() {
                    if let Some(cap) = early_stop_rows {
                        if all_rows.len() >= cap {
                            break;
                        }
                    }
                    iter += 1;
                    if iter > self.options.max_cte_iterations {
                        return Err(SqlError::runtime("recursive CTE iteration limit exceeded"));
                    }
                    self.charge_steps(1)?;
                    let popped = entry.row;
                    all_rows.push(popped.clone());
                    if let Some(cap) = early_stop_rows {
                        if all_rows.len() >= cap {
                            break;
                        }
                    }
                    let mut step_map = base_existing.clone();
                    step_map.insert(
                        key.clone(),
                        Arc::new(CteTable::untracked(cte_cols.clone(), vec![popped])),
                    );
                    let step_runner = self.child_with_ctes(step_map);
                    for (is_dist, step_stmt) in &rec_branches {
                        let (new_cols, new_rows) =
                            step_runner.execute_select_stmt(step_stmt, outer_scopes)?;
                        if new_cols.len() != width {
                            return Err(SqlError::runtime(
                                "SELECTs to the left and right of UNION do not have the same number of result columns",
                            ));
                        }
                        for row in new_rows {
                            self.charge_steps(1)?;
                            if *is_dist {
                                if !dedup.insert(&row, self)? {
                                    continue;
                                }
                            } else if any_distinct {
                                let _ = dedup.insert(&row, self)?;
                            }
                            mem.reserve(estimate_row_bytes(&row))?;
                            let ks = compute_keys(&row)?;
                            pq.push(PqEntry {
                                keys: ks,
                                row,
                                seq: next_seq,
                                ord_terms,
                                term_colls: &term_colls,
                            });
                            next_seq += 1;
                        }
                    }
                }
                drop(dedup);
                drop(pq);
                let final_rows = self.apply_order_and_limit(
                    &cte_cols,
                    all_rows,
                    None,
                    None,
                    None,
                    None,
                    cte.select.limit.as_ref(),
                    outer_scopes,
                )?;
                drop(mem);
                return CteTable::new_metered(cte_cols, final_rows, self.options.meter.as_ref());
            }

            // Standard breadth-first recursive evaluation (no ORDER BY on CTE).
            for row in base_rows {
                self.charge_steps(1)?;
                if row.len() != width {
                    return Err(SqlError::runtime(
                        "SELECTs to the left and right of UNION do not have the same number of result columns",
                    ));
                }
                if any_distinct && !dedup.insert(&row, self)? {
                    continue;
                }
                mem.reserve(estimate_row_bytes(&row))?;
                frontier.push(row.clone());
                all_rows.push(row);
            }

            let mut iter = 0usize;
            while !frontier.is_empty() {
                if let Some(cap) = early_stop_rows {
                    if all_rows.len() >= cap {
                        break;
                    }
                }
                iter += 1;
                if iter > self.options.max_cte_iterations {
                    return Err(SqlError::runtime("recursive CTE iteration limit exceeded"));
                }
                self.charge_steps(1)?;

                let mut step_map = base_existing.clone();
                step_map.insert(
                    key.clone(),
                    Arc::new(CteTable::untracked(
                        cte_cols.clone(),
                        std::mem::take(&mut frontier),
                    )),
                );
                let step_runner = self.child_with_ctes(step_map);
                for (is_dist, step_stmt) in &rec_branches {
                    let (new_cols, new_rows) =
                        step_runner.execute_select_stmt(step_stmt, outer_scopes)?;
                    if new_cols.len() != width {
                        return Err(SqlError::runtime(
                            "SELECTs to the left and right of UNION do not have the same number of result columns",
                        ));
                    }
                    for row in new_rows {
                        self.charge_steps(1)?;
                        if *is_dist {
                            if !dedup.insert(&row, self)? {
                                continue;
                            }
                        } else if any_distinct {
                            let _ = dedup.insert(&row, self)?;
                        }
                        mem.reserve(estimate_row_bytes(&row))?;
                        frontier.push(row.clone());
                        all_rows.push(row);
                        if let Some(cap) = early_stop_rows {
                            if all_rows.len() >= cap {
                                break;
                            }
                        }
                    }
                    if let Some(cap) = early_stop_rows {
                        if all_rows.len() >= cap {
                            break;
                        }
                    }
                }
            }

            drop(dedup);
            drop(frontier);
            let final_rows = self.apply_order_and_limit(
                &cte_cols,
                all_rows,
                None,
                None,
                None,
                cte.select.order_by_all.as_ref(),
                cte.select.limit.as_ref(),
                outer_scopes,
            )?;
            drop(mem);
            return CteTable::new_metered(cte_cols, final_rows, self.options.meter.as_ref());
        }

        // Non-recursive CTE: reject any self-reference as a circular reference.
        if select_stmt_references_table(&cte.select, &cte.name.name) {
            return Err(SqlError::runtime(format!(
                "circular reference: {}",
                cte.name.name
            )));
        }
        let runner = self.child_with_ctes(existing.clone());
        let (cols, rows) = runner.execute_select_stmt(&cte.select, outer_scopes)?;
        let cte_cols = apply_cte_column_names(cte, &cols)?;
        CteTable::new_metered(cte_cols, rows, self.options.meter.as_ref())
    }

    fn execute_select_no_with(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectRowsMeta> {
        match &stmt.body {
            SelectBody::Core(core) => {
                // Keep FROM-scope info available for ORDER BY resolution in single-core queries,
                // and pass outer LIMIT/ORDER BY ALL so eligible single-table scans can push down
                // RowidOrder / IndexOrder / TopK directly into storage.
                let (out_cols, out_expl, out_rows, order_ctx) = self.execute_select_core(
                    core,
                    outer_scopes,
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                )?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    Some(&order_ctx),
                    None,
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                    outer_scopes,
                )?;
                Ok((out_cols, out_expl, final_rows))
            }
            SelectBody::Values(_) => {
                let (out_cols, out_expl, out_rows) =
                    self.execute_select_body(&stmt.body, outer_scopes)?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    None,
                    None,
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                    outer_scopes,
                )?;
                Ok((out_cols, out_expl, final_rows))
            }
            SelectBody::Compound { .. } => {
                let mut branch_col_names: Vec<Vec<String>> = Vec::new();
                let (out_cols, out_expl, out_rows) = self.execute_select_body_depth(
                    &stmt.body,
                    outer_scopes,
                    self.query_depth,
                    Some(&mut branch_col_names),
                )?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    None,
                    Some(&branch_col_names),
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                    outer_scopes,
                )?;
                Ok((out_cols, out_expl, final_rows))
            }
        }
    }

    fn execute_select_body(
        &self,
        body: &SelectBody,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectRowsMeta> {
        self.execute_select_body_depth(body, outer_scopes, self.query_depth, None)
    }

    fn execute_select_body_depth(
        &self,
        body: &SelectBody,
        outer_scopes: &[EvalScope<'_>],
        depth: usize,
        mut branch_names: Option<&mut Vec<Vec<String>>>,
    ) -> SqlResult<SelectRowsMeta> {
        self.check_depth(depth)?;
        match body {
            SelectBody::Core(core) => {
                let (cols, expl, rows, _) =
                    self.execute_select_core(core, outer_scopes, None, None, None)?;
                if let Some(ref mut bn) = branch_names {
                    record_compound_branch_names(bn, &cols);
                }
                Ok((cols, expl, rows))
            }
            SelectBody::Values(values) => {
                let env = self.make_env();
                let mut rows = Vec::with_capacity(values.rows.len());
                let mut mem = self.make_mem_guard();
                let mut width: Option<usize> = None;
                for vrow in &values.rows {
                    self.charge_steps(1)?;
                    if let Some(w) = width {
                        if vrow.items.len() != w {
                            return Err(SqlError::runtime(
                                "all VALUES must have the same number of terms",
                            ));
                        }
                    } else {
                        width = Some(vrow.items.len());
                    }
                    let mut vals = Vec::with_capacity(vrow.items.len());
                    for item in &vrow.items {
                        vals.push(eval_expr(item, outer_scopes, &env)?);
                    }
                    mem.reserve(estimate_row_bytes(&vals))?;
                    rows.push(vals);
                }
                let w = width.unwrap_or(0);
                let cols: Vec<ScopeColumn> = (1..=w)
                    .map(|i| ScopeColumn {
                        table: None,
                        name: format!("column{i}"),
                        affinity: Affinity::None,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    })
                    .collect();
                if let Some(ref mut bn) = branch_names {
                    record_compound_branch_names(bn, &cols);
                }
                Ok((cols, vec![ExprCollation::none(); w], rows))
            }
            SelectBody::Compound {
                op, left, right, ..
            } => {
                let (mut lcols, mut lcolls, lrows) = self.execute_select_body_depth(
                    left,
                    outer_scopes,
                    depth + 1,
                    branch_names.as_deref_mut(),
                )?;
                let (rcols, rcolls, rrows) = self.execute_select_body_depth(
                    right,
                    outer_scopes,
                    depth + 1,
                    branch_names,
                )?;
                if lcols.len() != rcols.len() {
                    return Err(SqlError::runtime(format!(
                        "SELECTs to the left and right of {} do not have the same number of result columns",
                        op.as_str()
                    )));
                }
                // Merge collations across compound branches (`select.c:2641-2656`
                // `multiSelectCollSeq`): leftmost non-`None` collation wins, and compound
                // SELECTs drop column affinity.
                for i in 0..lcols.len() {
                    lcols[i].affinity = Affinity::None;
                    let merged_name = lcolls[i]
                        .name
                        .clone()
                        .or_else(|| rcolls[i].name.clone());
                    let merged_expl = if lcolls[i].name.is_some() {
                        lcolls[i].explicit
                    } else {
                        rcolls[i].explicit
                    };
                    if let Some(ref eff) = merged_name {
                        lcols[i].collation = eff.clone();
                    }
                    lcolls[i] = ExprCollation {
                        name: merged_name,
                        explicit: merged_expl,
                    };
                }
                let env = self.make_env();
                let colls: Vec<&Collation> = lcols
                    .iter()
                    .map(|c| env.lookup_collation(&c.collation))
                    .collect::<SqlResult<Vec<_>>>()?;
                let coll_names: Vec<&str> = lcols.iter().map(|c| c.collation.as_str()).collect();
                let mut mem = self.make_mem_guard();
                let out_rows = match op {
                    CompoundOperator::UnionAll => {
                        let mut out = lrows;
                        for r in rrows {
                            mem.reserve(estimate_row_bytes(&r))?;
                            out.push(r);
                        }
                        out
                    }
                    CompoundOperator::Union => {
                        let mut dedup =
                            RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
                        let mut out = Vec::new();
                        for r in lrows.into_iter().chain(rrows) {
                            self.charge_steps(1)?;
                            if dedup.insert(&r, self)? {
                                mem.reserve(estimate_row_bytes(&r))?;
                                out.push(r);
                            }
                        }
                        out
                    }
                    CompoundOperator::Intersect => {
                        let mut rset =
                            RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
                        for r in &rrows {
                            self.charge_steps(1)?;
                            let _ = rset.insert(r, self)?;
                        }
                        let mut out_set =
                            RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
                        let mut out = Vec::new();
                        for r in lrows {
                            self.charge_steps(1)?;
                            if rset.contains(&r, self)? && out_set.insert(&r, self)? {
                                mem.reserve(estimate_row_bytes(&r))?;
                                out.push(r);
                            }
                        }
                        out
                    }
                    CompoundOperator::Except => {
                        let mut rset =
                            RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
                        for r in &rrows {
                            self.charge_steps(1)?;
                            let _ = rset.insert(r, self)?;
                        }
                        let mut out_set =
                            RowDedup::new(&coll_names, &colls, self.options.meter.as_deref());
                        let mut out = Vec::new();
                        for r in lrows {
                            self.charge_steps(1)?;
                            if !rset.contains(&r, self)? && out_set.insert(&r, self)? {
                                mem.reserve(estimate_row_bytes(&r))?;
                                out.push(r);
                            }
                        }
                        out
                    }
                };
                Ok((lcols, lcolls, out_rows))
            }
        }
    }

    fn execute_select_core(
        &self,
        core: &SelectCore,
        outer_scopes: &[EvalScope<'_>],
        outer_order_by: Option<&[OrderingTerm]>,
        outer_order_by_all: Option<&OrderByAll>,
        outer_limit: Option<&LimitClause>,
    ) -> SqlResult<SelectCoreResult> {
        let env = self.make_env();

        // 0. Fast-path `SELECT count(*) FROM single_table` without decoding row payloads.
        if core.where_clause.is_none()
            && core.group_by.is_none()
            && !core.group_by_all
            && core.having.is_none()
            && core.windows.is_none()
            && !core.from_first
            && core.columns.len() == 1
            && outer_order_by.is_none()
        {
            if let (Some(FromSource::Table(ft)), ResultColumn::Expr(re)) =
                (&core.from, &core.columns[0])
            {
                if let Expr::Function(f) = &re.expr {
                    if f.name.name.eq_ignore_ascii_case("count")
                        && (f.star || f.args.is_empty())
                        && f.filter.is_none()
                        && f.over.is_none()
                        && f.order_by.is_none()
                        && f.quantifier.is_none()
                        && self
                            .options
                            .functions
                            .is_aggregate(&f.name.name, f.args.len(), f.star)
                    {
                        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
                        let is_cte = schema.is_none()
                            && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase());
                        if !is_cte && is_valid_schema_name(schema) {
                            if let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? {
                                if let Some(ref idx_name) = ft.indexed_by {
                                    let exists = tmeta
                                        .indexes
                                        .iter()
                                        .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name));
                                    if !exists {
                                        return Err(SqlError::runtime(format!(
                                            "no such index: {}",
                                            idx_name.name
                                        )));
                                    }
                                }
                                if let Some(cnt) = self.catalog.count_table_rows(
                                    schema,
                                    &ft.name.name,
                                    self.options.meter.as_deref(),
                                )? {
                                    let output_name = match &re.alias {
                                        Some(a) => a.name.clone(),
                                        None => derive_column_name(&re.expr, &re.text),
                                    };
                                    let out_cols = vec![ScopeColumn {
                                        table: None,
                                        name: output_name,
                                        affinity: Affinity::None,
                                        collation: "BINARY".to_string(),
                                        is_rowid_alias: false,
                                    }];
                                    let out_colls = vec![ExprCollation::none()];
                                    let out_rows = vec![vec![SqlValue::Int(cnt as i64)]];
                                    let order_ctx = OrderEvalContext {
                                        from_columns: Vec::new(),
                                        from_using_hidden: Vec::new(),
                                        from_source_ids: Vec::new(),
                                        from_rows: Vec::new(),
                                        precomputed: Vec::new(),
                                        explicit_aliases: vec![re.alias.is_some()],
                                    };
                                    return Ok((out_cols, out_colls, out_rows, order_ctx));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Validate WHERE does not contain aggregate or window functions (even on 0-row tables).
        if let Some(ref where_expr) = core.where_clause {
            if expr_contains_aggregate(where_expr, self.options) {
                return Err(SqlError::runtime("misuse of aggregate function"));
            }
            if expr_contains_window(where_expr) {
                return Err(SqlError::runtime("misuse of window function"));
            }
        }

        // Collect column names referenced in this SELECT (including JOIN ON/USING predicates)
        // so storage can prune unused columns across single tables AND joins!
        let table_needed_names: Option<Vec<&str>> = if core.from.is_some() {
            collect_core_referenced_col_names(core, outer_order_by)
        } else {
            None
        };

        // 0b. Streaming fast-path `SELECT count(*) FROM single_table WHERE <compiled>`
        if outer_scopes.is_empty() && core.where_clause.is_some() {
            if let Some(res) = self.try_streaming_count_where(
                core,
                outer_order_by,
                table_needed_names.as_deref(),
                &env,
            )? {
                return Ok(res);
            }
        }

        // 0b2 / 0b3. Single-pass streaming table scan OR cumulative window fast path.
        // Pre-check whether any top-level SELECT item is a window call so windowed queries skip
        // `try_streaming_select_where` (and non-windowed queries skip `try_streaming_cumulative_window`).
        if outer_scopes.is_empty() && outer_order_by.is_none() && outer_order_by_all.is_none() {
            let has_top_window = core.columns.iter().any(|rc| {
                matches!(
                    rc,
                    ResultColumn::Expr(re)
                        if matches!(&re.expr, Expr::Function(f) if f.over.is_some())
                )
            });
            if !has_top_window {
                if core.where_clause.is_some() || outer_limit.is_none() {
                    if let Some(res) = self.try_streaming_select_where(
                        core,
                        table_needed_names.as_deref(),
                        &env,
                    )? {
                        return Ok(res);
                    }
                }
            } else if let Some(res) = self.try_streaming_cumulative_window(
                core,
                table_needed_names.as_deref(),
                &env,
            )? {
                return Ok(res);
            }
        }

        // 0c. Streaming fast-path 2-table INNER/LEFT hash equijoin with direct column projection
        if outer_scopes.is_empty() && outer_order_by.is_none() && core.where_clause.is_none() {
            if let Some(res) = self.try_streaming_hash_join(
                core,
                table_needed_names.as_deref(),
                &env,
            )? {
                return Ok(res);
            }
        }

        // 0d. Streaming fast-path single-table online `GROUP BY <single_col>`
        let streamed_agg = if outer_scopes.is_empty() {
            match self.try_streaming_group_by(
                core,
                outer_order_by,
                table_needed_names.as_deref(),
                &env,
            )? {
                Some(StreamedGroupBy::Direct(res)) => return Ok(res),
                Some(StreamedGroupBy::Rows(r)) => Some(r),
                None => None,
            }
        } else {
            None
        };

        let (rel, expanded, work_rows, mut precomputed_per_row) =
            if let Some((rel_meta, expanded, work_rows, pre)) = streamed_agg {
                (rel_meta, expanded, work_rows, pre)
            } else {
                // Compute optional ORDER BY / LIMIT pushdown context for simple single-table queries
                // (no WHERE, no GROUP BY/HAVING/DISTINCT, no aggregates or window functions).
                let order_limit_ctx: Option<(&SelectCore, Option<&[OrderingTerm]>, usize)> =
                    if core.where_clause.is_none()
                        && core.group_by.is_none()
                        && !core.group_by_all
                        && core.having.is_none()
                        && core.distinct.is_none()
                        && core.windows.is_none()
                        && !core.from_first
                        && outer_order_by_all.is_none()
                        && (matches!(&core.from, Some(FromSource::Table(_)))
                            || (outer_order_by.is_none()
                                && matches!(&core.from, Some(FromSource::Function(_)))))
                    {
                        if let Some(lim) = outer_limit {
                            let lim_const = expr_is_pushdown_const(&lim.count, self.options)
                                && lim
                                    .offset
                                    .as_ref()
                                    .is_none_or(|o| expr_is_pushdown_const(o, self.options));
                            let has_agg_or_win = core.columns.iter().any(|rc| match rc {
                                ResultColumn::Expr(re) => {
                                    expr_contains_aggregate(&re.expr, self.options)
                                        || expr_contains_window(&re.expr)
                                }
                                ResultColumn::Star(rs) => rs.replace.as_deref().is_some_and(|reps| {
                                    reps.iter().any(|rep| {
                                        expr_contains_aggregate(&rep.expr, self.options)
                                            || expr_contains_window(&rep.expr)
                                    })
                                }),
                            }) || outer_order_by.is_some_and(|ob| {
                                ob.iter().any(|t| {
                                    expr_contains_aggregate(&t.expr, self.options)
                                        || expr_contains_window(&t.expr)
                                })
                            });
                            if lim_const && !has_agg_or_win {
                                let count = eval_limit_int(&lim.count, outer_scopes, &env).ok();
                                let offset = match &lim.offset {
                                    Some(off_expr) => eval_limit_int(off_expr, outer_scopes, &env)
                                        .ok()
                                        .map(|o| usize::try_from(o.max(0)).unwrap_or(usize::MAX)),
                                    None => Some(0),
                                };
                                match (count, offset) {
                                    (Some(c), Some(off)) if c >= 0 => {
                                        let k = if c == 0 {
                                            0
                                        } else {
                                            off.saturating_add(
                                                usize::try_from(c).unwrap_or(usize::MAX),
                                            )
                                        };
                                        Some((core, outer_order_by, k))
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                // 1. Evaluate FROM clause (with optional single-table WHERE/ORDER/LIMIT pushdown & projection pruning)
                let mut rel = match &core.from {
                    None => Relation::single_empty_row(),
                    Some(from_src) => self.eval_from_depth(
                        from_src,
                        core.where_clause.as_ref(),
                        order_limit_ctx,
                        table_needed_names.as_deref(),
                        outer_scopes,
                        self.query_depth,
                    )?,
                };

                // 2. Apply WHERE filter (and validate WHERE column/subquery references even on
                // 0-row tables or short-circuited boolean branches!)
                if let Some(ref where_expr) = core.where_clause {
                    if rel.rows.is_empty() {
                        let mut scopes = outer_scopes.to_vec();
                        let mut sc = EvalScope::simple(&rel.columns, &[]);
                        sc.hidden_unqualified = Some(&rel.using_hidden);
                        sc.source_ids = Some(&rel.source_ids);
                        scopes.push(sc);
                        self.validate_expr_and_subqueries(where_expr, &scopes)?;
                    } else if outer_scopes.is_empty() {
                        if let Some(mut compiled) = CompiledRowPred::try_compile(
                            where_expr,
                            &rel.columns,
                            &rel.using_hidden,
                            &rel.source_ids,
                            &env,
                        )? {
                            if compiled.has_subquery() {
                                let mut sc0 = EvalScope::simple(&rel.columns, &[]);
                                sc0.hidden_unqualified = Some(&rel.using_hidden);
                                sc0.source_ids = Some(&rel.source_ids);
                                self.validate_expr_and_subqueries(where_expr, &[sc0])?;
                            }
                            let mut filtered = Vec::with_capacity(rel.rows.len());
                            for row in rel.rows {
                                self.charge_steps(1)?;
                                if compiled.eval_row(&row, &env)? {
                                    filtered.push(row);
                                }
                            }
                            rel.rows = filtered;
                        } else {
                            let mut sc0 = EvalScope::simple(&rel.columns, &[]);
                            sc0.hidden_unqualified = Some(&rel.using_hidden);
                            sc0.source_ids = Some(&rel.source_ids);
                            self.validate_expr_and_subqueries(where_expr, &[sc0])?;
                            let mut filtered = Vec::with_capacity(rel.rows.len());
                            for row in rel.rows {
                                self.charge_steps(1)?;
                                let mut sc = EvalScope::simple(&rel.columns, &row);
                                sc.hidden_unqualified = Some(&rel.using_hidden);
                                sc.source_ids = Some(&rel.source_ids);
                                let cond = eval_expr(where_expr, &[sc], &env)?;
                                if boolean_value(&cond, false) {
                                    filtered.push(row);
                                }
                            }
                            rel.rows = filtered;
                        }
                    } else {
                        {
                            let mut scopes = outer_scopes.to_vec();
                            let mut sc0 = EvalScope::simple(&rel.columns, &[]);
                            sc0.hidden_unqualified = Some(&rel.using_hidden);
                            sc0.source_ids = Some(&rel.source_ids);
                            scopes.push(sc0);
                            self.validate_expr_and_subqueries(where_expr, &scopes)?;
                        }
                        let mut filtered = Vec::with_capacity(rel.rows.len());
                        for row in rel.rows {
                            self.charge_steps(1)?;
                            let cond = {
                                let mut scopes = outer_scopes.to_vec();
                                let mut sc = EvalScope::simple(&rel.columns, &row);
                                sc.hidden_unqualified = Some(&rel.using_hidden);
                                sc.source_ids = Some(&rel.source_ids);
                                scopes.push(sc);
                                eval_expr(where_expr, &scopes, &env)?
                            };
                            if boolean_value(&cond, false) {
                                filtered.push(row);
                            }
                        }
                        rel.rows = filtered;
                    }
                }

                // 3. Expand SELECT list (*, t.*, * EXCLUDE/REPLACE, FROM-first empty columns -> *)
                let expanded = expand_result_columns(core, &rel)?;

                // Validate non-direct SELECT expression column/subquery references unconditionally
                // (catches short-circuited `0 AND bogus`, `coalesce(1, bogus)`, `NULL IN (bogus)`,
                // and 0-row tables) and always validate named WINDOW definitions.
                if expanded.iter().any(|p| p.direct_col.is_none()) || core.windows.is_some() {
                    let mut scopes = outer_scopes.to_vec();
                    let mut sc = EvalScope::simple(&rel.columns, &[]);
                    sc.hidden_unqualified = Some(&rel.using_hidden);
                    sc.source_ids = Some(&rel.source_ids);
                    scopes.push(sc);
                    for p in &expanded {
                        if p.direct_col.is_none() {
                            self.validate_expr_and_subqueries(&p.expr, &scopes)?;
                        }
                    }
                    if let Some(ref wins) = core.windows {
                        for wdef in wins {
                            validate_window_spec_columns(&wdef.window, &scopes, env.dqs)?;
                        }
                    }
                }

                // 4. Determine if this is an aggregate query (including aggregates inside
                // referenced named WINDOWs, while ignoring unreferenced named WINDOWs!).
                let has_top_level_agg_in_select = expanded.iter().any(|p| {
                    p.direct_col.is_none()
                        && expr_contains_non_window_aggregate(&p.expr, self.options)
                });
                let has_agg_in_select = expanded
                    .iter()
                    .any(|p| p.direct_col.is_none() && expr_contains_aggregate(&p.expr, self.options));
                let has_agg_in_order = outer_order_by.is_some_and(|ob| {
                    ob.iter()
                        .any(|t| expr_contains_aggregate(&t.expr, self.options))
                });
                let used_named_windows = collect_referenced_window_names(
                    &expanded,
                    outer_order_by,
                    core.windows.as_deref().unwrap_or(&[]),
                );
                let has_agg_in_used_named_windows =
                    core.windows.as_deref().unwrap_or(&[]).iter().any(|wd| {
                        used_named_windows.contains(&wd.name.name.to_ascii_lowercase())
                            && (wd
                                .window
                                .partition_by
                                .as_deref()
                                .unwrap_or(&[])
                                .iter()
                                .any(|e| expr_contains_aggregate(e, self.options))
                                || wd
                                    .window
                                    .order_by
                                    .as_deref()
                                    .unwrap_or(&[])
                                    .iter()
                                    .any(|o| expr_contains_aggregate(&o.expr, self.options)))
                    });
                // SQLite rejects `HAVING` unless the query has `GROUP BY` or a direct
                // (non-window-internal) SELECT-list aggregate.
                if core.having.is_some()
                    && !(core.group_by.is_some()
                        || core.group_by_all
                        || has_top_level_agg_in_select)
                {
                    return Err(SqlError::runtime(
                        "HAVING clause on a non-aggregate query",
                    ));
                }
                let is_aggregate = core.group_by.is_some()
                    || core.group_by_all
                    || has_agg_in_select
                    || has_agg_in_used_named_windows;
                if !is_aggregate && has_agg_in_order {
                    let mut ob_aggs = Vec::new();
                    if let Some(ob) = outer_order_by {
                        for t in ob {
                            collect_aggregates(&t.expr, self.options, &mut ob_aggs);
                        }
                    }
                    let fname = ob_aggs
                        .first()
                        .map_or("aggregate", |f| f.name.name.as_str());
                    return Err(SqlError::runtime(format!("misuse of aggregate: {fname}()")));
                }
                if core.having.as_ref().is_some_and(expr_contains_window) {
                    return Err(SqlError::runtime("misuse of window function"));
                }

                // 5. Grouping & Aggregation (produces working rows + per-row precomputed aggregate slots)
                let (work_rows, precomputed_per_row) = if is_aggregate {
                    self.eval_aggregation(core, &rel, &expanded, outer_order_by, outer_scopes)?
                } else {
                    (rel.rows, Vec::new())
                };
                let rel_meta = Relation {
                    columns: rel.columns,
                    star_hidden: rel.star_hidden,
                    using_hidden: rel.using_hidden,
                    source_ids: rel.source_ids,
                    rows: Vec::new(),
                };
                (rel_meta, expanded, work_rows, precomputed_per_row)
            };

        // 6. Window Functions (evaluated over post-GROUP BY / HAVING rows; validated even on 0 rows!)
        let need_order_ctx = outer_order_by.is_some();
        let is_distinct = core.distinct == Some(DistinctKind::Distinct);
        let has_windows = expanded
            .iter()
            .any(|p| p.direct_col.is_none() && expr_contains_window(&p.expr))
            || outer_order_by.is_some_and(|ob| ob.iter().any(|t| expr_contains_window(&t.expr)));
        let mut win_cols: Vec<WindowCol> = Vec::new();
        let use_columnar_windows = has_windows
            && precomputed_per_row.is_empty()
            && !need_order_ctx
            && !is_distinct
            && outer_scopes.is_empty()
            && expanded.iter().all(|p| {
                p.direct_col.is_some()
                    || matches!(&p.expr, Expr::Function(f) if f.over.is_some())
            });
        if has_windows {
            if use_columnar_windows {
                let mut sink = WindowSink::Columnar(&mut win_cols);
                self.eval_windows(
                    core,
                    &rel.columns,
                    &rel.using_hidden,
                    &rel.source_ids,
                    &work_rows,
                    &mut sink,
                    &expanded,
                    outer_order_by,
                    outer_scopes,
                )?;
            } else {
                if precomputed_per_row.len() != work_rows.len() {
                    precomputed_per_row.resize_with(work_rows.len(), PrecomputedRow::new);
                }
                let mut sink = WindowSink::PerRow(&mut precomputed_per_row);
                self.eval_windows(
                    core,
                    &rel.columns,
                    &rel.using_hidden,
                    &rel.source_ids,
                    &work_rows,
                    &mut sink,
                    &expanded,
                    outer_order_by,
                    outer_scopes,
                )?;
            }
        }

        // 7. Evaluate SELECT projections (preserving source table qualifier + collation metadata)
        let mut out_cols = Vec::with_capacity(expanded.len());
        let mut out_colls = Vec::with_capacity(expanded.len());
        {
            let mut sample_scope = EvalScope::simple(&rel.columns, &[]);
            sample_scope.hidden_unqualified = Some(&rel.using_hidden);
            sample_scope.source_ids = Some(&rel.source_ids);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            for p in &expanded {
                if let Some(didx) = p.direct_col {
                    let rc = &rel.columns[didx];
                    let coll = if rc.collation.is_empty() {
                        "BINARY".to_string()
                    } else {
                        rc.collation.clone()
                    };
                    out_cols.push(ScopeColumn {
                        table: if p.explicit_alias.is_none() {
                            rc.table.clone()
                        } else {
                            None
                        },
                        name: p.output_name.clone(),
                        affinity: rc.affinity,
                        collation: coll.clone(),
                        is_rowid_alias: false,
                    });
                    out_colls.push(ExprCollation::implicit(coll));
                } else {
                    let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                    let ec = expr_collation(&p.expr, &scopes);
                    let coll = ec
                        .name
                        .clone()
                        .unwrap_or_else(|| "BINARY".to_string());
                    let source_table = if p.explicit_alias.is_none() {
                        if let Expr::Column(c) = &p.expr {
                            c.table.as_ref().map(|t| t.name.clone()).or_else(|| {
                                rel.columns
                                    .iter()
                                    .find(|rc| rc.name.eq_ignore_ascii_case(&c.column.name))
                                    .and_then(|rc| rc.table.clone())
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    out_cols.push(ScopeColumn {
                        table: source_table,
                        name: p.output_name.clone(),
                        affinity: aff,
                        collation: coll,
                        is_rowid_alias: false,
                    });
                    out_colls.push(ec);
                }
            }
        }

        let has_pre = !precomputed_per_row.is_empty();
        let mut projected_rows = Vec::with_capacity(work_rows.len());
        let mut kept_work_rows = if need_order_ctx {
            Vec::with_capacity(work_rows.len())
        } else {
            Vec::new()
        };
        let mut kept_pre = if need_order_ctx {
            Vec::with_capacity(work_rows.len())
        } else {
            Vec::new()
        };
        let dist_colls: Vec<&Collation> = if is_distinct {
            out_cols
                .iter()
                .map(|c| env.lookup_collation(&c.collation))
                .collect::<SqlResult<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let dist_coll_names: Vec<&str> = if is_distinct {
            out_cols.iter().map(|c| c.collation.as_str()).collect()
        } else {
            Vec::new()
        };
        let mut dist_dedup =
            RowDedup::new(&dist_coll_names, &dist_colls, self.options.meter.as_deref());
        let mut mem = self.make_mem_guard();
        let mut pre_iter = precomputed_per_row.into_iter();

        let can_move_direct = !use_columnar_windows && !need_order_ctx && !is_distinct && {
            let mut seen_cols = 0u128;
            let mut ok = true;
            for p in &expanded {
                match p.direct_col {
                    Some(didx) if didx < 128 => {
                        let bit = 1u128 << didx;
                        if (seen_cols & bit) != 0 {
                            ok = false;
                            break;
                        }
                        seen_cols |= bit;
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            ok
        };

        if use_columnar_windows {
            if !work_rows.is_empty() {
                enum WinProj {
                    ColMove(usize),
                    ColClone(usize),
                    WinMove(usize),
                    WinClone(usize),
                }
                let mut col_use_count = vec![0u32; rel.columns.len()];
                let mut win_use_count = vec![0u32; win_cols.len()];
                for p in &expanded {
                    if let Some(didx) = p.direct_col {
                        col_use_count[didx] += 1;
                    } else if let Expr::Function(f) = &p.expr {
                        if let Some(widx) = win_cols.iter().position(|wc| {
                            wc.span_start == f.span.start && wc.span_end == f.span.end
                        }) {
                            win_use_count[widx] += 1;
                        }
                    }
                }
                let mut proj_plan = Vec::with_capacity(expanded.len());
                for p in &expanded {
                    if let Some(didx) = p.direct_col {
                        if col_use_count[didx] == 1 {
                            proj_plan.push(WinProj::ColMove(didx));
                        } else {
                            proj_plan.push(WinProj::ColClone(didx));
                        }
                    } else if let Expr::Function(f) = &p.expr {
                        let widx = win_cols
                            .iter()
                            .position(|wc| {
                                wc.span_start == f.span.start && wc.span_end == f.span.end
                            })
                            .expect("window column computed");
                        if win_use_count[widx] == 1 {
                            proj_plan.push(WinProj::WinMove(widx));
                        } else {
                            proj_plan.push(WinProj::WinClone(widx));
                        }
                    }
                }
                let has_meter = self.options.meter.is_some();
                for (ridx, mut row) in work_rows.into_iter().enumerate() {
                    self.charge_steps(1)?;
                    let mut out_row = Vec::with_capacity(proj_plan.len());
                    for item in &proj_plan {
                        match *item {
                            WinProj::ColMove(didx) => {
                                out_row.push(std::mem::replace(&mut row[didx], SqlValue::Null));
                            }
                            WinProj::ColClone(didx) => {
                                out_row.push(row[didx].clone());
                            }
                            WinProj::WinMove(widx) => {
                                out_row.push(std::mem::replace(
                                    &mut win_cols[widx].values[ridx],
                                    SqlValue::Null,
                                ));
                            }
                            WinProj::WinClone(widx) => {
                                out_row.push(win_cols[widx].values[ridx].clone());
                            }
                        }
                    }
                    if has_meter {
                        mem.reserve(estimate_row_bytes(&out_row))?;
                    }
                    projected_rows.push(out_row);
                }
            }
        } else if can_move_direct {
            for mut row in work_rows {
                self.charge_steps(1)?;
                let mut out_row = Vec::with_capacity(expanded.len());
                for p in &expanded {
                    let didx = p.direct_col.expect("direct_col");
                    out_row.push(std::mem::replace(&mut row[didx], SqlValue::Null));
                }
                mem.reserve(estimate_row_bytes(&out_row))?;
                projected_rows.push(out_row);
            }
        } else {
            for row in work_rows {
                self.charge_steps(1)?;
                let pre = if has_pre {
                    pre_iter.next().unwrap_or_else(PrecomputedRow::new)
                } else {
                    PrecomputedRow::new()
                };
                let mut out_row = Vec::with_capacity(expanded.len());
                if outer_scopes.is_empty() {
                    let sc = EvalScope {
                        columns: &rel.columns,
                        values: &row,
                        precomputed: if has_pre { Some(&pre.slots) } else { None },
                        precomputed_json: if has_pre { Some(&pre.is_json) } else { None },
                        hidden_unqualified: Some(&rel.using_hidden),
                        source_ids: Some(&rel.source_ids),
                        aliases: None,
                        no_rowid_aliases: false,
                    };
                    let sslice = [sc];
                    for p in &expanded {
                        if let Some(didx) = p.direct_col {
                            out_row.push(row[didx].clone());
                        } else {
                            out_row.push(eval_expr(&p.expr, &sslice, &env)?);
                        }
                    }
                } else {
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(EvalScope {
                        columns: &rel.columns,
                        values: &row,
                        precomputed: if has_pre { Some(&pre.slots) } else { None },
                        precomputed_json: if has_pre { Some(&pre.is_json) } else { None },
                        hidden_unqualified: Some(&rel.using_hidden),
                        source_ids: Some(&rel.source_ids),
                        aliases: None,
                        no_rowid_aliases: false,
                    });
                    for p in &expanded {
                        if let Some(didx) = p.direct_col {
                            out_row.push(row[didx].clone());
                        } else {
                            out_row.push(eval_expr(&p.expr, &scopes, &env)?);
                        }
                    }
                }
                if is_distinct && !dist_dedup.insert(&out_row, self)? {
                    continue;
                }
                mem.reserve(estimate_row_bytes(&out_row))?;
                projected_rows.push(out_row);
                if need_order_ctx {
                    kept_work_rows.push(row);
                    kept_pre.push(pre);
                }
            }
        }

        let order_ctx = OrderEvalContext {
            from_columns: rel.columns,
            from_using_hidden: rel.using_hidden,
            from_source_ids: rel.source_ids,
            from_rows: kept_work_rows,
            precomputed: kept_pre,
            explicit_aliases: expanded.iter().map(|p| p.explicit_alias.is_some()).collect(),
        };
        drop(dist_dedup);
        Ok((out_cols, out_colls, projected_rows, order_ctx))
    }

    fn build_table_rel_meta(tmeta: &crate::catalog::TableMeta, alias: &str) -> Relation {
        let mut columns = Vec::with_capacity(tmeta.columns.len());
        let mut star_hidden = Vec::with_capacity(tmeta.columns.len());
        for col in &tmeta.columns {
            columns.push(ScopeColumn {
                table: Some(alias.to_string()),
                name: col.name.clone(),
                affinity: col.affinity,
                collation: col.collation.clone(),
                is_rowid_alias: false,
            });
            star_hidden.push(col.is_hidden);
        }
        let n = columns.len();
        Relation {
            columns,
            star_hidden,
            using_hidden: vec![false; n],
            source_ids: vec![0u32; n],
            rows: Vec::new(),
        }
    }

    fn synth_rowid_referenced(
        tmeta: &crate::catalog::TableMeta,
        needed_names: Option<&[&str]>,
    ) -> bool {
        if !tmeta.has_rowid {
            return false;
        }
        let Some(names) = needed_names else {
            return true;
        };
        for rname in ["rowid", "_rowid_", "oid"] {
            if names.iter().any(|n| n.eq_ignore_ascii_case(rname))
                && !tmeta
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(rname))
            {
                return true;
            }
        }
        false
    }

    fn try_streaming_count_where(
        &self,
        core: &SelectCore,
        outer_order_by: Option<&[OrderingTerm]>,
        table_needed_names: Option<&[&str]>,
        env: &EvalEnv<'_>,
    ) -> SqlResult<Option<SelectCoreResult>> {
        if core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.windows.is_some()
            || core.from_first
            || core.columns.len() != 1
            || outer_order_by.is_some()
        {
            return Ok(None);
        }
        let (Some(FromSource::Table(ft)), ResultColumn::Expr(re), Some(where_expr)) = (
            &core.from,
            &core.columns[0],
            core.where_clause.as_ref(),
        ) else {
            return Ok(None);
        };
        let Expr::Function(f) = &re.expr else {
            return Ok(None);
        };
        if !(f.name.name.eq_ignore_ascii_case("count")
            && (f.star || f.args.is_empty())
            && f.filter.is_none()
            && f.over.is_none()
            && f.order_by.is_none()
            && f.quantifier.is_none()
            && self
                .options
                .functions
                .is_aggregate(&f.name.name, f.args.len(), f.star))
        {
            return Ok(None);
        }
        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
        if (schema.is_none() && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(schema)
        {
            return Ok(None);
        }
        let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? else {
            return Ok(None);
        };
        if let Some(ref idx_name) = ft.indexed_by {
            let exists = tmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name));
            if !exists {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        if Self::synth_rowid_referenced(&tmeta, table_needed_names) {
            return Ok(None);
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
        let rel_meta = Self::build_table_rel_meta(&tmeta, alias);
        let Some(mut pred) = CompiledRowPred::try_compile(
            where_expr,
            &rel_meta.columns,
            &rel_meta.using_hidden,
            &rel_meta.source_ids,
            env,
        )?
        else {
            return Ok(None);
        };
        if pred.has_subquery() {
            let mut sc = EvalScope::simple(&rel_meta.columns, &[]);
            sc.hidden_unqualified = Some(&rel_meta.using_hidden);
            sc.source_ids = Some(&rel_meta.source_ids);
            self.validate_expr_and_subqueries(where_expr, &[sc])?;
        }
        let hint = if ft.not_indexed {
            TableScanHint::FullScan
        } else {
            self.extract_scan_hint(&tmeta, alias, where_expr, &[], env)
        };
        let needed_indices: Option<Vec<usize>> = table_needed_names.map(|names| {
            tmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });
        let mut count = 0i64;
        let has_meter = self.options.meter.is_some();
        self.catalog.visit_table(
            schema,
            &ft.name.name,
            &hint,
            needed_indices.as_deref(),
            self.options.meter.as_deref(),
            &mut |_rid, vals| {
                if has_meter {
                    self.charge_steps(1)?;
                }
                if pred.eval_row(vals, env)? {
                    count += 1;
                }
                Ok(true)
            },
        )?;
        let output_name = match &re.alias {
            Some(a) => a.name.clone(),
            None => derive_column_name(&re.expr, &re.text),
        };
        let out_cols = vec![ScopeColumn {
            table: None,
            name: output_name,
            affinity: Affinity::None,
            collation: "BINARY".to_string(),
            is_rowid_alias: false,
        }];
        let out_colls = vec![ExprCollation::none()];
        let out_rows = vec![vec![SqlValue::Int(count)]];
        let order_ctx = OrderEvalContext {
            from_columns: Vec::new(),
            from_using_hidden: Vec::new(),
            from_source_ids: Vec::new(),
            from_rows: Vec::new(),
            precomputed: Vec::new(),
            explicit_aliases: vec![re.alias.is_some()],
        };
        Ok(Some((out_cols, out_colls, out_rows, order_ctx)))
    }

    /// Single-pass streaming execution for `SELECT <direct cols / *> FROM single_table [WHERE <compiled>]`
    /// (zero intermediate `Vec<ScannedRow>`, zero second filter pass, zero third projection pass).
    fn try_streaming_select_where(
        &self,
        core: &SelectCore,
        table_needed_names: Option<&[&str]>,
        env: &EvalEnv<'_>,
    ) -> SqlResult<Option<SelectCoreResult>> {
        if core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.distinct.is_some()
            || core.windows.is_some()
            || core.from_first
        {
            return Ok(None);
        }
        let Some(FromSource::Table(ft)) = &core.from else {
            return Ok(None);
        };
        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
        if (schema.is_none() && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(schema)
        {
            return Ok(None);
        }
        let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? else {
            return Ok(None);
        };
        if let Some(ref idx_name) = ft.indexed_by {
            let exists = tmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name));
            if !exists {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        if Self::synth_rowid_referenced(&tmeta, table_needed_names) {
            return Ok(None);
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
        let rel_meta = Self::build_table_rel_meta(&tmeta, alias);
        let expanded = expand_result_columns(core, &rel_meta)?;
        if !expanded.iter().all(|p| p.direct_col.is_some()) {
            return Ok(None);
        }
        let mut pred = if let Some(where_expr) = core.where_clause.as_ref() {
            let Some(p) = CompiledRowPred::try_compile(
                where_expr,
                &rel_meta.columns,
                &rel_meta.using_hidden,
                &rel_meta.source_ids,
                env,
            )?
            else {
                return Ok(None);
            };
            if p.has_subquery() {
                let mut sc = EvalScope::simple(&rel_meta.columns, &[]);
                sc.hidden_unqualified = Some(&rel_meta.using_hidden);
                sc.source_ids = Some(&rel_meta.source_ids);
                self.validate_expr_and_subqueries(where_expr, &[sc])?;
            }
            Some(p)
        } else {
            None
        };
        let hint = match core.where_clause.as_ref() {
            Some(where_expr) if !ft.not_indexed => {
                self.extract_scan_hint(&tmeta, alias, where_expr, &[], env)
            }
            _ => TableScanHint::FullScan,
        };
        let needed_indices: Option<Vec<usize>> = table_needed_names.map(|names| {
            tmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });

        let proj_cids: Vec<usize> = expanded
            .iter()
            .map(|p| p.direct_col.expect("direct_col"))
            .collect();
        let proj_len = proj_cids.len();
        let mut out_cols = Vec::with_capacity(proj_len);
        let mut out_colls = Vec::with_capacity(proj_len);
        let mut explicit_aliases = Vec::with_capacity(proj_len);
        for p in &expanded {
            let didx = p.direct_col.expect("direct_col");
            let rc = &rel_meta.columns[didx];
            let coll = if rc.collation.is_empty() {
                "BINARY".to_string()
            } else {
                rc.collation.clone()
            };
            out_cols.push(ScopeColumn {
                table: if p.explicit_alias.is_none() {
                    rc.table.clone()
                } else {
                    None
                },
                name: p.output_name.clone(),
                affinity: rc.affinity,
                collation: coll.clone(),
                is_rowid_alias: false,
            });
            out_colls.push(ExprCollation::implicit(coll));
            explicit_aliases.push(p.explicit_alias.is_some());
        }

        let has_meter = self.options.meter.is_some();
        let mut mem = self.make_mem_guard();
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(64);

        self.catalog.visit_table(
            schema,
            &ft.name.name,
            &hint,
            needed_indices.as_deref(),
            self.options.meter.as_deref(),
            &mut |_rid, vals| {
                if has_meter {
                    self.charge_steps(1)?;
                }
                if let Some(ref mut p) = pred {
                    if !p.eval_row(vals, env)? {
                        return Ok(true);
                    }
                }
                let mut out_row = Vec::with_capacity(proj_len);
                for &c in &proj_cids {
                    out_row.push(vals[c].clone());
                }
                if has_meter {
                    mem.reserve(estimate_row_bytes(&out_row))?;
                }
                out_rows.push(out_row);
                Ok(true)
            },
        )?;

        let order_ctx = OrderEvalContext {
            from_columns: Vec::new(),
            from_using_hidden: Vec::new(),
            from_source_ids: Vec::new(),
            from_rows: Vec::new(),
            precomputed: Vec::new(),
            explicit_aliases,
        };
        Ok(Some((out_cols, out_colls, out_rows, order_ctx)))
    }

    /// Single-pass streaming cumulative window fast path for
    /// `SELECT <direct cols>, <sum/count/avg/total> OVER (ORDER BY <ipk> ASC) FROM single_table [WHERE ...]`.
    /// Exploits B-tree rowid scan order to compute running aggregates in ONE pass with zero
    /// intermediate `ScannedRow`s, zero sorting, and zero secondary projection pass.
    fn try_streaming_cumulative_window(
        &self,
        core: &SelectCore,
        table_needed_names: Option<&[&str]>,
        env: &EvalEnv<'_>,
    ) -> SqlResult<Option<SelectCoreResult>> {
        if core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.distinct.is_some()
            || core.windows.is_some()
            || core.from_first
        {
            return Ok(None);
        }
        let Some(FromSource::Table(ft)) = &core.from else {
            return Ok(None);
        };
        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
        if (schema.is_none() && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(schema)
        {
            return Ok(None);
        }
        let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? else {
            return Ok(None);
        };
        if !tmeta.has_rowid {
            return Ok(None);
        }
        let Some(ipk_col) = tmeta.ipk_column else {
            return Ok(None);
        };
        if let Some(ref idx_name) = ft.indexed_by {
            let exists = tmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name));
            if !exists {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        if Self::synth_rowid_referenced(&tmeta, table_needed_names) {
            return Ok(None);
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
        let rel_meta = Self::build_table_rel_meta(&tmeta, alias);
        let expanded = expand_result_columns(core, &rel_meta)?;

        enum WinStreamProj {
            Col(usize),
            Win(usize),
        }
        let mut proj_items = Vec::with_capacity(expanded.len());
        let mut win_kinds: Vec<OnlineAggKind> = Vec::new();
        for p in &expanded {
            if let Some(didx) = p.direct_col {
                proj_items.push(WinStreamProj::Col(didx));
                continue;
            }
            let Expr::Function(wcall) = &p.expr else {
                return Ok(None);
            };
            let Some(WindowRef::Spec(spec)) = &wcall.over else {
                return Ok(None);
            };
            if spec.base.is_some()
                || spec.partition_by.is_some()
                || wcall.filter.is_some()
                || wcall.quantifier.is_some()
            {
                return Ok(None);
            }
            let Some(ord_terms) = spec.order_by.as_deref() else {
                return Ok(None);
            };
            if ord_terms.len() != 1 {
                return Ok(None);
            }
            let ot = &ord_terms[0];
            if ot.order == Some(SortOrder::Desc) {
                return Ok(None);
            }
            let Expr::Column(oc) = &ot.expr else {
                return Ok(None);
            };
            let Some(ocol) = resolve_direct_from_col(
                oc,
                &rel_meta.columns,
                &rel_meta.using_hidden,
                &rel_meta.source_ids,
            ) else {
                return Ok(None);
            };
            if ocol != ipk_col {
                return Ok(None);
            }
            let resolved_frame = match &spec.frame {
                Some(fspec) => ResolvedFrame::from_ast(fspec, |b| match &b.expr {
                    Some(e) => Ok(Some(eval_expr(e, &[], env)?)),
                    None => Ok(None),
                })?,
                None => ResolvedFrame::default_for(true),
            };
            if resolved_frame.exclude != FrameExclude::NoOthers
                || resolved_frame.start_kind != FrameBoundType::UnboundedPreceding
                || resolved_frame.end_kind != FrameBoundType::CurrentRow
            {
                return Ok(None);
            }
            if !self
                .options
                .functions
                .is_aggregate(&wcall.name.name, wcall.args.len(), wcall.star)
            {
                return Ok(None);
            }
            let Some(kind) = classify_online_agg(
                wcall,
                &rel_meta.columns,
                &rel_meta.using_hidden,
                &rel_meta.source_ids,
                self.options,
            ) else {
                return Ok(None);
            };
            let widx = win_kinds.len();
            win_kinds.push(kind);
            proj_items.push(WinStreamProj::Win(widx));
        }
        if win_kinds.is_empty() {
            return Ok(None);
        }

        let mut pred = if let Some(ref where_expr) = core.where_clause {
            let Some(p) = CompiledRowPred::try_compile(
                where_expr,
                &rel_meta.columns,
                &rel_meta.using_hidden,
                &rel_meta.source_ids,
                env,
            )?
            else {
                return Ok(None);
            };
            if p.has_subquery() {
                let mut sc = EvalScope::simple(&rel_meta.columns, &[]);
                sc.hidden_unqualified = Some(&rel_meta.using_hidden);
                sc.source_ids = Some(&rel_meta.source_ids);
                self.validate_expr_and_subqueries(where_expr, &[sc])?;
            }
            Some(p)
        } else {
            None
        };

        let hint = match core.where_clause.as_ref() {
            Some(we) if !ft.not_indexed => self.extract_scan_hint(&tmeta, alias, we, &[], env),
            _ => TableScanHint::FullScan,
        };
        // Only rowid-ordered scans (`FullScan` / `RowidRange`) guarantee ascending IPK order!
        if !matches!(
            hint,
            TableScanHint::FullScan | TableScanHint::RowidRange { .. }
        ) {
            return Ok(None);
        }

        let needed_indices: Option<Vec<usize>> = table_needed_names.map(|names| {
            tmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });

        let proj_len = proj_items.len();
        let mut out_cols = Vec::with_capacity(proj_len);
        let mut out_colls = Vec::with_capacity(proj_len);
        let mut explicit_aliases = Vec::with_capacity(proj_len);
        for p in &expanded {
            if let Some(didx) = p.direct_col {
                let rc = &rel_meta.columns[didx];
                let coll = if rc.collation.is_empty() {
                    "BINARY".to_string()
                } else {
                    rc.collation.clone()
                };
                out_cols.push(ScopeColumn {
                    table: if p.explicit_alias.is_none() {
                        rc.table.clone()
                    } else {
                        None
                    },
                    name: p.output_name.clone(),
                    affinity: rc.affinity,
                    collation: coll.clone(),
                    is_rowid_alias: false,
                });
                out_colls.push(ExprCollation::implicit(coll));
            } else {
                out_cols.push(ScopeColumn {
                    table: None,
                    name: p.output_name.clone(),
                    affinity: Affinity::None,
                    collation: "BINARY".to_string(),
                    is_rowid_alias: false,
                });
                out_colls.push(ExprCollation::none());
            }
            explicit_aliases.push(p.explicit_alias.is_some());
        }

        let has_meter = self.options.meter.is_some();
        let mut mem = self.make_mem_guard();
        let init_cap = if has_meter {
            64
        } else if let TableScanHint::RowidRange { low, high } = &hint {
            let lo = match low {
                Bound::Included(SqlValue::Int(v)) => *v,
                Bound::Excluded(SqlValue::Int(v)) => v.saturating_add(1),
                _ => 1,
            };
            let hi = match high {
                Bound::Included(SqlValue::Int(v)) => *v,
                Bound::Excluded(SqlValue::Int(v)) => v.saturating_sub(1),
                _ => lo.saturating_add(255),
            };
            if hi >= lo {
                ((hi.saturating_sub(lo)).saturating_add(1) as usize).clamp(64, 4096)
            } else {
                64
            }
        } else {
            256
        };
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(init_cap);
        let mut prev_ipk: Option<i64> = None;
        let mut order_broken = false;

        // Specialized ultra-tight loop for `SELECT <col>, sum/total/avg(<col>) OVER (ORDER BY <ipk>)`
        // (eliminates `accs` slice loop, `proj_items` loop, and `OnlineAggAcc` enum dispatch!).
        let single_sum_fast = if proj_items.len() == 2 && win_kinds.len() == 1 {
            match (&proj_items[0], &proj_items[1], &win_kinds[0]) {
                (WinStreamProj::Col(c0), WinStreamProj::Win(0), OnlineAggKind::Sum(sum_cid, sk)) => {
                    Some((*c0, *sum_cid, *sk))
                }
                _ => None,
            }
        } else {
            None
        };

        if let Some((c0, sum_cid, sk)) = single_sum_fast {
            let inline_ipk_pred = if !has_meter {
                pred.as_ref()
                    .and_then(|p| p.as_single_int_cmp())
                    .filter(|(c, _, _)| *c == ipk_col)
                    .map(|(_, op, rhs)| (op, rhs))
            } else {
                None
            };
            if matches!(sk, SumKind::Sum | SumKind::Total) {
                // Register-local integer running sum with exact checked_add overflow fallback
                // into `SumAcc` (avoids per-row `SumAcc` struct branches on all-int scans!).
                let mut int_sum: i64 = 0;
                let mut has_val = false;
                let mut fallback: Option<crate::functions::aggregates::SumAcc> = None;
                let is_total = matches!(sk, SumKind::Total);
                let c0_is_ipk = c0 == ipk_col;
                self.catalog.visit_table(
                    schema,
                    &ft.name.name,
                    &hint,
                    needed_indices.as_deref(),
                    self.options.meter.as_deref(),
                    &mut |_rid, vals| {
                        if has_meter {
                            self.charge_steps(1)?;
                        }
                        let cur_ipk = if let Some((op, rhs)) = inline_ipk_pred {
                            let SqlValue::Int(k) = vals[ipk_col] else {
                                order_broken = true;
                                return Ok(false);
                            };
                            let pass = match op {
                                CompareOp::Eq | CompareOp::Is => k == rhs,
                                CompareOp::Ne | CompareOp::IsNot => k != rhs,
                                CompareOp::Lt => k < rhs,
                                CompareOp::Le => k <= rhs,
                                CompareOp::Gt => k > rhs,
                                CompareOp::Ge => k >= rhs,
                            };
                            if !pass {
                                return Ok(true);
                            }
                            k
                        } else {
                            if let Some(ref mut p) = pred {
                                if !p.eval_row(vals, env)? {
                                    return Ok(true);
                                }
                            }
                            let SqlValue::Int(k) = vals[ipk_col] else {
                                order_broken = true;
                                return Ok(false);
                            };
                            k
                        };
                        if prev_ipk.is_some_and(|p| cur_ipk <= p) {
                            order_broken = true;
                            return Ok(false);
                        }
                        prev_ipk = Some(cur_ipk);
                        let run_val = if let Some(ref mut fb) = fallback {
                            fb.step_value(&vals[sum_cid]);
                            fb.value_as(sk)?
                        } else {
                            match &vals[sum_cid] {
                                SqlValue::Int(v) => {
                                    if let Some(s) = int_sum.checked_add(*v) {
                                        int_sum = s;
                                        has_val = true;
                                        if is_total {
                                            SqlValue::Real(int_sum as f64)
                                        } else {
                                            SqlValue::Int(int_sum)
                                        }
                                    } else {
                                        let mut sa = crate::functions::aggregates::SumAcc::new(sk);
                                        if has_val {
                                            sa.step_value(&SqlValue::Int(int_sum));
                                        }
                                        sa.step_value(&vals[sum_cid]);
                                        let rv = sa.value_as(sk)?;
                                        fallback = Some(sa);
                                        rv
                                    }
                                }
                                SqlValue::Null => {
                                    if is_total {
                                        SqlValue::Real(int_sum as f64)
                                    } else if has_val {
                                        SqlValue::Int(int_sum)
                                    } else {
                                        SqlValue::Null
                                    }
                                }
                                other => {
                                    let mut sa = crate::functions::aggregates::SumAcc::new(sk);
                                    if has_val {
                                        sa.step_value(&SqlValue::Int(int_sum));
                                    }
                                    sa.step_value(other);
                                    let rv = sa.value_as(sk)?;
                                    fallback = Some(sa);
                                    rv
                                }
                            }
                        };
                        let v0 = if c0_is_ipk {
                            SqlValue::Int(cur_ipk)
                        } else {
                            vals[c0].clone()
                        };
                        let out_row = vec![v0, run_val];
                        if has_meter {
                            mem.reserve(estimate_row_bytes(&out_row))?;
                        }
                        out_rows.push(out_row);
                        Ok(true)
                    },
                )?;
            } else {
                let mut sum_acc = crate::functions::aggregates::SumAcc::new(sk);
                let c0_is_ipk = c0 == ipk_col;
                self.catalog.visit_table(
                    schema,
                    &ft.name.name,
                    &hint,
                    needed_indices.as_deref(),
                    self.options.meter.as_deref(),
                    &mut |_rid, vals| {
                        if has_meter {
                            self.charge_steps(1)?;
                        }
                        if let Some(ref mut p) = pred {
                            if !p.eval_row(vals, env)? {
                                return Ok(true);
                            }
                        }
                        let SqlValue::Int(cur_ipk) = vals[ipk_col] else {
                            order_broken = true;
                            return Ok(false);
                        };
                        if prev_ipk.is_some_and(|p| cur_ipk <= p) {
                            order_broken = true;
                            return Ok(false);
                        }
                        prev_ipk = Some(cur_ipk);
                        sum_acc.step_value(&vals[sum_cid]);
                        let v0 = if c0_is_ipk {
                            SqlValue::Int(cur_ipk)
                        } else {
                            vals[c0].clone()
                        };
                        let out_row = vec![v0, sum_acc.value_as(sk)?];
                        if has_meter {
                            mem.reserve(estimate_row_bytes(&out_row))?;
                        }
                        out_rows.push(out_row);
                        Ok(true)
                    },
                )?;
            }
        } else {
            let mut accs: Vec<(OnlineAggAcc, Option<SumKind>)> =
                Vec::with_capacity(win_kinds.len());
            for kind in win_kinds {
                match kind {
                    OnlineAggKind::CountStar => accs.push((OnlineAggAcc::CountStar(0), None)),
                    OnlineAggKind::CountCol(cid) => {
                        accs.push((OnlineAggAcc::CountCol(cid, 0), None))
                    }
                    OnlineAggKind::Sum(cid, sk) => {
                        accs.push((
                            OnlineAggAcc::Sum(cid, crate::functions::aggregates::SumAcc::new(sk)),
                            Some(sk),
                        ));
                    }
                }
            }
            self.catalog.visit_table(
                schema,
                &ft.name.name,
                &hint,
                needed_indices.as_deref(),
                self.options.meter.as_deref(),
                &mut |_rid, vals| {
                    if has_meter {
                        self.charge_steps(1)?;
                    }
                    if let Some(ref mut p) = pred {
                        if !p.eval_row(vals, env)? {
                            return Ok(true);
                        }
                    }
                    // Guard against catalogs (e.g. `MemoryCatalog`) whose scan order is not strictly
                    // ascending unique non-NULL `INTEGER PRIMARY KEY`; fall back cleanly to `eval_windows`.
                    let SqlValue::Int(cur_ipk) = vals[ipk_col] else {
                        order_broken = true;
                        return Ok(false);
                    };
                    if prev_ipk.is_some_and(|p| cur_ipk <= p) {
                        order_broken = true;
                        return Ok(false);
                    }
                    prev_ipk = Some(cur_ipk);
                    for (acc, _) in &mut accs {
                        match acc {
                            OnlineAggAcc::CountStar(c) => *c += 1,
                            OnlineAggAcc::CountCol(cid, c) => {
                                if !vals[*cid].is_null() {
                                    *c += 1;
                                }
                            }
                            OnlineAggAcc::Sum(cid, sa) => sa.step_value(&vals[*cid]),
                        }
                    }
                    let mut out_row = Vec::with_capacity(proj_len);
                    for item in &proj_items {
                        match *item {
                            WinStreamProj::Col(c) => out_row.push(vals[c].clone()),
                            WinStreamProj::Win(widx) => {
                                let val = match &accs[widx] {
                                    (
                                        OnlineAggAcc::CountStar(c) | OnlineAggAcc::CountCol(_, c),
                                        _,
                                    ) => SqlValue::Int(*c),
                                    (OnlineAggAcc::Sum(_, sa), Some(sk)) => sa.value_as(*sk)?,
                                    _ => unreachable!(),
                                };
                                out_row.push(val);
                            }
                        }
                    }
                    if has_meter {
                        mem.reserve(estimate_row_bytes(&out_row))?;
                    }
                    out_rows.push(out_row);
                    Ok(true)
                },
            )?;
        }
        if order_broken {
            return Ok(None);
        }

        let order_ctx = OrderEvalContext {
            from_columns: Vec::new(),
            from_using_hidden: Vec::new(),
            from_source_ids: Vec::new(),
            from_rows: Vec::new(),
            precomputed: Vec::new(),
            explicit_aliases,
        };
        Ok(Some((out_cols, out_colls, out_rows, order_ctx)))
    }

    fn try_streaming_hash_join(
        &self,
        core: &SelectCore,
        table_needed_names: Option<&[&str]>,
        env: &EvalEnv<'_>,
    ) -> SqlResult<Option<SelectCoreResult>> {
        if core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.windows.is_some()
            || core.distinct.is_some()
            || core.from_first
            || core.columns.is_empty()
        {
            return Ok(None);
        }
        let Some(FromSource::Join {
            left,
            operator,
            right,
            on: Some(on_expr),
            using: None,
            ..
        }) = &core.from
        else {
            return Ok(None);
        };
        if (operator.flags & (join_flag::RIGHT | join_flag::NATURAL)) != 0 {
            return Ok(None);
        }
        let (FromSource::Table(lt), FromSource::Table(rt)) = (left.as_ref(), right.as_ref()) else {
            return Ok(None);
        };
        let lschema = lt.schema.as_ref().map(|n| n.name.as_str());
        let rschema = rt.schema.as_ref().map(|n| n.name.as_str());
        if (lschema.is_none() && self.ctes.contains_key(&lt.name.name.to_ascii_lowercase()))
            || (rschema.is_none() && self.ctes.contains_key(&rt.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(lschema)
            || !is_valid_schema_name(rschema)
        {
            return Ok(None);
        }
        let (Some(lmeta), Some(rmeta)) = (
            self.catalog.get_table_arc(lschema, &lt.name.name)?,
            self.catalog.get_table_arc(rschema, &rt.name.name)?,
        ) else {
            return Ok(None);
        };
        if Self::synth_rowid_referenced(&lmeta, table_needed_names)
            || Self::synth_rowid_referenced(&rmeta, table_needed_names)
        {
            return Ok(None);
        }
        if let Some(ref idx_name) = lt.indexed_by {
            if !lmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name))
            {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        if let Some(ref idx_name) = rt.indexed_by {
            if !rmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name))
            {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        let lalias = lt.alias.as_ref().unwrap_or(&lt.name).name.as_str();
        let ralias = rt.alias.as_ref().unwrap_or(&rt.name).name.as_str();
        let lrel_meta = Self::build_table_rel_meta(&lmeta, lalias);
        let rrel_meta = Self::build_table_rel_meta(&rmeta, ralias);
        let Some((lk, rk, aff, cname)) =
            detect_simple_equijoin(&lrel_meta, &rrel_meta, Some(on_expr), &[])
        else {
            return Ok(None);
        };
        if aff == Affinity::Text || !cname.eq_ignore_ascii_case("BINARY") {
            return Ok(None);
        }
        if !matches!(
            env.lookup_collation(&cname)?,
            Collation::Builtin {
                kind: BuiltinCollation::Binary,
                encoding: TextEncoding::Utf8
            }
        ) {
            return Ok(None);
        }

        let lwidth = lrel_meta.columns.len();
        let rwidth = rrel_meta.columns.len();
        let mut combined_cols = Vec::with_capacity(lwidth + rwidth);
        combined_cols.extend(lrel_meta.columns);
        combined_cols.extend(rrel_meta.columns);
        let combined_using_hidden = vec![false; lwidth + rwidth];
        let mut combined_sids = vec![0u32; lwidth + rwidth];
        for sid in &mut combined_sids[lwidth..] {
            *sid = 1;
        }

        #[derive(Clone, Copy)]
        enum JoinProjSrc {
            Left(usize),
            RightProj(usize),
        }
        let mut r_proj_cols: Vec<usize> = Vec::new();
        let mut proj_srcs = Vec::with_capacity(core.columns.len());
        let mut out_cols = Vec::with_capacity(core.columns.len());
        let mut out_colls = Vec::with_capacity(core.columns.len());
        let mut explicit_aliases = Vec::with_capacity(core.columns.len());
        for rc in &core.columns {
            let ResultColumn::Expr(re) = rc else {
                return Ok(None);
            };
            let Expr::Column(c) = &re.expr else {
                return Ok(None);
            };
            let Some(didx) =
                resolve_direct_from_col(c, &combined_cols, &combined_using_hidden, &combined_sids)
            else {
                return Ok(None);
            };
            proj_srcs.push(if didx < lwidth {
                JoinProjSrc::Left(didx)
            } else {
                let rcid = didx - lwidth;
                let rslot = match r_proj_cols.iter().position(|&c| c == rcid) {
                    Some(p) => p,
                    None => {
                        let p = r_proj_cols.len();
                        r_proj_cols.push(rcid);
                        p
                    }
                };
                JoinProjSrc::RightProj(rslot)
            });
            let col = &combined_cols[didx];
            let output_name = match &re.alias {
                Some(a) => a.name.clone(),
                None => derive_column_name(&re.expr, &re.text),
            };
            let norm_coll = if col.collation.is_empty() {
                "BINARY".to_string()
            } else {
                col.collation.clone()
            };
            out_cols.push(ScopeColumn {
                table: if re.alias.is_none() {
                    col.table.clone()
                } else {
                    None
                },
                name: output_name,
                affinity: col.affinity,
                collation: norm_coll.clone(),
                is_rowid_alias: false,
            });
            out_colls.push(ExprCollation::implicit(norm_coll));
            explicit_aliases.push(re.alias.is_some());
        }

        let is_left = (operator.flags & join_flag::LEFT) != 0;
        let r_needed: Option<Vec<usize>> = table_needed_names.map(|names| {
            rmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });
        let coerce_numeric = |v: &SqlValue| -> SqlValue {
            if matches!(aff, Affinity::Integer | Affinity::Numeric | Affinity::Real) {
                if let SqlValue::Text(b) = v {
                    if let Some(num) = apply_numeric_affinity(b, false) {
                        return num;
                    }
                }
            }
            v.clone()
        };

        let mut mem = self.make_mem_guard();
        let has_meter = self.options.meter.is_some();
        // Flat tagged lookup table for small integer keys (0..4096):
        //   0            => empty
        //   1..0x80000000 => unique single right-row index `slot - 1` (zero heap Vec!)
        //   >=0x80000000 => index `(slot & 0x7fff_ffff)` into `multi_buckets`
        const MULTI_TAG: u32 = 0x8000_0000;
        let mut small_int_slots: Vec<u32> = Vec::with_capacity(64);
        let mut multi_buckets: Vec<Vec<usize>> = Vec::new();
        let mut buckets: HashMap<HashKey, Vec<usize>> = HashMap::new();
        let r_proj_width = r_proj_cols.len();
        let mut r_proj: Vec<SqlValue> = Vec::with_capacity(64 * r_proj_width.max(1));
        let mut r_row_count = 0usize;

        let mut insert_small_int = |idx: usize, ridx: usize, mem: &mut MemoryGuard<'_>| -> SqlResult<()> {
            if idx >= small_int_slots.len() {
                let grow = idx + 1 - small_int_slots.len();
                if has_meter {
                    mem.reserve((grow as u64).saturating_mul(4))?;
                }
                small_int_slots.resize(idx + 1, 0);
            }
            let cur = small_int_slots[idx];
            if cur == 0 && ridx < (MULTI_TAG as usize - 1) {
                small_int_slots[idx] = (ridx + 1) as u32;
            } else if cur >= MULTI_TAG {
                multi_buckets[(cur & !MULTI_TAG) as usize].push(ridx);
            } else {
                let bidx = multi_buckets.len() as u32;
                let mut v = Vec::with_capacity(2);
                if cur != 0 {
                    v.push((cur - 1) as usize);
                }
                v.push(ridx);
                multi_buckets.push(v);
                small_int_slots[idx] = MULTI_TAG | bidx;
            }
            Ok(())
        };

        // Stream the RHS build side directly into the hash table + flat `r_proj` buffer
        // (eliminating intermediate `Vec<ScannedRow>` and per-row `Vec<SqlValue>` allocations!).
        self.catalog.visit_table(
            rschema,
            &rt.name.name,
            &TableScanHint::FullScan,
            r_needed.as_deref(),
            self.options.meter.as_deref(),
            &mut |_rid, rvals| {
                let ridx = r_row_count;
                r_row_count += 1;
                if has_meter {
                    self.charge_steps(1)?;
                    mem.reserve(48)?;
                }
                if r_proj_width > 0 {
                    for &rcid in &r_proj_cols {
                        let v = rvals[rcid].clone();
                        if has_meter {
                            mem.reserve(estimate_value_bytes(&v))?;
                        }
                        r_proj.push(v);
                    }
                }
                if let SqlValue::Int(k) = rvals[rk] {
                    if (0..4096).contains(&k) {
                        insert_small_int(k as usize, ridx, &mut mem)?;
                        return Ok(true);
                    }
                    buckets.entry(HashKey::Num(k)).or_default().push(ridx);
                    return Ok(true);
                }
                let rv = coerce_numeric(&rvals[rk]);
                if let Some(key) = HashKey::from_sql(&rv) {
                    if let HashKey::Num(k) = key {
                        if (0..4096).contains(&k) {
                            insert_small_int(k as usize, ridx, &mut mem)?;
                            return Ok(true);
                        }
                    }
                    if has_meter {
                        if let HashKey::Text(ref b) | HashKey::Blob(ref b) = key {
                            mem.reserve(b.len() as u64)?;
                        }
                    }
                    buckets.entry(key).or_default().push(ridx);
                }
                Ok(true)
            },
        )?;
        if !is_left && r_row_count == 0 {
            let order_ctx = OrderEvalContext {
                from_columns: Vec::new(),
                from_using_hidden: Vec::new(),
                from_source_ids: Vec::new(),
                from_rows: Vec::new(),
                precomputed: Vec::new(),
                explicit_aliases,
            };
            return Ok(Some((out_cols, out_colls, Vec::new(), order_ctx)));
        }

        let l_needed: Option<Vec<usize>> = table_needed_names.map(|names| {
            lmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });
        let init_cap = if has_meter {
            r_row_count.max(64)
        } else {
            self.catalog
                .count_table_rows(lschema, &lt.name.name, None)
                .ok()
                .flatten()
                .map(|c| (c as usize).min(1_000_000))
                .unwrap_or_else(|| r_row_count.max(1024))
        };
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(init_cap);
        let proj_len = proj_srcs.len();
        let proj_3: Option<[JoinProjSrc; 3]> = if proj_len == 3 {
            Some([proj_srcs[0], proj_srcs[1], proj_srcs[2]])
        } else {
            None
        };

        // Zero-branch specialized probe for the common inner FK equi-join:
        // unique small-int dimension table (`0..4096`), single projected RHS column, unmetered,
        // `SELECT l.c0, l.c1, r.c2 FROM fact l JOIN dim r ON l.lk = r.rk`.
        let fast_inner_l2r1 = if !has_meter
            && !is_left
            && multi_buckets.is_empty()
            && buckets.is_empty()
            && r_proj_width == 1
        {
            match proj_3 {
                Some([JoinProjSrc::Left(c0), JoinProjSrc::Left(c1), JoinProjSrc::RightProj(0)]) => {
                    Some((c0, c1))
                }
                _ => None,
            }
        } else {
            None
        };

        if let Some((c0, c1)) = fast_inner_l2r1 {
            let slots_len = small_int_slots.len();
            self.catalog.visit_table(
                lschema,
                &lt.name.name,
                &TableScanHint::FullScan,
                l_needed.as_deref(),
                None,
                &mut |_rid, lvals| {
                    if let SqlValue::Int(k) = lvals[lk] {
                        let uk = k as usize;
                        if k >= 0 && uk < slots_len {
                            let slot = small_int_slots[uk];
                            if slot != 0 {
                                out_rows.push(vec![
                                    lvals[c0].clone(),
                                    lvals[c1].clone(),
                                    r_proj[(slot - 1) as usize].clone(),
                                ]);
                            }
                        }
                        return Ok(true);
                    }
                    if !lvals[lk].is_null() {
                        let lv = coerce_numeric(&lvals[lk]);
                        if let Some(HashKey::Num(k)) = HashKey::from_sql(&lv) {
                            let uk = k as usize;
                            if k >= 0 && uk < slots_len {
                                let slot = small_int_slots[uk];
                                if slot != 0 {
                                    out_rows.push(vec![
                                        lvals[c0].clone(),
                                        lvals[c1].clone(),
                                        r_proj[(slot - 1) as usize].clone(),
                                    ]);
                                }
                            }
                        }
                    }
                    Ok(true)
                },
            )?;
        } else {
            self.catalog.visit_table(
                lschema,
                &lt.name.name,
                &TableScanHint::FullScan,
                l_needed.as_deref(),
                self.options.meter.as_deref(),
                &mut |_rid, lvals| {
                    if has_meter {
                        self.charge_steps(1)?;
                    }
                    // Fast unique small-int hit (zero slice indirection, single L1 lookup!)
                    if let SqlValue::Int(k) = lvals[lk] {
                        if (0..4096).contains(&k) {
                            let slot = small_int_slots.get(k as usize).copied().unwrap_or(0);
                            if slot > 0 && slot < MULTI_TAG {
                                let rbase = ((slot - 1) as usize) * r_proj_width;
                                let out_row = if let Some([s0, s1, s2]) = proj_3 {
                                    let v0 = match s0 {
                                        JoinProjSrc::Left(c) => lvals[c].clone(),
                                        JoinProjSrc::RightProj(p) => r_proj[rbase + p].clone(),
                                    };
                                    let v1 = match s1 {
                                        JoinProjSrc::Left(c) => lvals[c].clone(),
                                        JoinProjSrc::RightProj(p) => r_proj[rbase + p].clone(),
                                    };
                                    let v2 = match s2 {
                                        JoinProjSrc::Left(c) => lvals[c].clone(),
                                        JoinProjSrc::RightProj(p) => r_proj[rbase + p].clone(),
                                    };
                                    vec![v0, v1, v2]
                                } else {
                                    let mut row = Vec::with_capacity(proj_len);
                                    for &src in &proj_srcs {
                                        match src {
                                            JoinProjSrc::Left(c) => row.push(lvals[c].clone()),
                                            JoinProjSrc::RightProj(p) => {
                                                row.push(r_proj[rbase + p].clone());
                                            }
                                        }
                                    }
                                    row
                                };
                                if has_meter {
                                    mem.reserve(estimate_row_bytes(&out_row))?;
                                }
                                out_rows.push(out_row);
                                return Ok(true);
                            }
                        }
                    }
                    let mut single_buf = [0usize; 1];
                    let matched_slice: Option<&[usize]> = match &lvals[lk] {
                        SqlValue::Int(k) if (0..4096).contains(k) => {
                            let slot = small_int_slots.get(*k as usize).copied().unwrap_or(0);
                            if slot == 0 {
                                None
                            } else if slot < MULTI_TAG {
                                single_buf[0] = (slot - 1) as usize;
                                Some(&single_buf[..])
                            } else {
                                Some(multi_buckets[(slot & !MULTI_TAG) as usize].as_slice())
                            }
                        }
                        SqlValue::Int(k) => buckets.get(&HashKey::Num(*k)).map(Vec::as_slice),
                        SqlValue::Null => None,
                        other => {
                            let lv = coerce_numeric(other);
                            if let Some(key) = HashKey::from_sql(&lv) {
                                if let HashKey::Num(k) = key {
                                    if (0..4096).contains(&k) {
                                        let slot =
                                            small_int_slots.get(k as usize).copied().unwrap_or(0);
                                        if slot == 0 {
                                            None
                                        } else if slot < MULTI_TAG {
                                            single_buf[0] = (slot - 1) as usize;
                                            Some(&single_buf[..])
                                        } else {
                                            Some(
                                                multi_buckets[(slot & !MULTI_TAG) as usize]
                                                    .as_slice(),
                                            )
                                        }
                                    } else {
                                        buckets.get(&key).map(Vec::as_slice)
                                    }
                                } else {
                                    buckets.get(&key).map(Vec::as_slice)
                                }
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(indices) = matched_slice {
                        for &ridx in indices {
                            if has_meter {
                                self.charge_steps(1)?;
                            }
                            let rbase = ridx * r_proj_width;
                            let mut out_row = Vec::with_capacity(proj_len);
                            for &src in &proj_srcs {
                                match src {
                                    JoinProjSrc::Left(c) => out_row.push(lvals[c].clone()),
                                    JoinProjSrc::RightProj(p) => {
                                        out_row.push(r_proj[rbase + p].clone());
                                    }
                                }
                            }
                            if has_meter {
                                mem.reserve(estimate_row_bytes(&out_row))?;
                            }
                            out_rows.push(out_row);
                        }
                    } else if is_left {
                        let mut out_row = Vec::with_capacity(proj_len);
                        for &src in &proj_srcs {
                            match src {
                                JoinProjSrc::Left(c) => out_row.push(lvals[c].clone()),
                                JoinProjSrc::RightProj(_) => out_row.push(SqlValue::Null),
                            }
                        }
                        if has_meter {
                            mem.reserve(estimate_row_bytes(&out_row))?;
                        }
                        out_rows.push(out_row);
                    }
                    Ok(true)
                },
            )?;
        }

        let order_ctx = OrderEvalContext {
            from_columns: Vec::new(),
            from_using_hidden: Vec::new(),
            from_source_ids: Vec::new(),
            from_rows: Vec::new(),
            precomputed: Vec::new(),
            explicit_aliases,
        };
        Ok(Some((out_cols, out_colls, out_rows, order_ctx)))
    }

    #[allow(clippy::type_complexity)]
    fn try_streaming_group_by(
        &self,
        core: &SelectCore,
        outer_order_by: Option<&[OrderingTerm]>,
        table_needed_names: Option<&[&str]>,
        env: &EvalEnv<'_>,
    ) -> SqlResult<Option<StreamedGroupBy>> {
        if core.having.is_some()
            || core.windows.is_some()
            || core.group_by_all
            || core.from_first
        {
            return Ok(None);
        }
        let (Some(FromSource::Table(ft)), Some(gb)) = (&core.from, core.group_by.as_deref()) else {
            return Ok(None);
        };
        if gb.len() != 1 || integer_ordinal_with_collation(&gb[0]).is_some() {
            return Ok(None);
        }
        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
        if (schema.is_none() && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(schema)
        {
            return Ok(None);
        }
        let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? else {
            return Ok(None);
        };
        if let Some(ref idx_name) = ft.indexed_by {
            if !tmeta
                .indexes
                .iter()
                .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name))
            {
                return Err(SqlError::runtime(format!("no such index: {}", idx_name.name)));
            }
        }
        if Self::synth_rowid_referenced(&tmeta, table_needed_names) {
            return Ok(None);
        }
        if core
            .columns
            .iter()
            .any(|rc| matches!(rc, ResultColumn::Star(_)))
        {
            return Ok(None);
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
        let rel_meta = Self::build_table_rel_meta(&tmeta, alias);
        let expanded = expand_result_columns(core, &rel_meta)?;
        if expanded
            .iter()
            .any(|p| p.direct_col.is_none() && expr_contains_window(&p.expr))
            || outer_order_by.is_some_and(|ob| ob.iter().any(|t| expr_contains_window(&t.expr)))
        {
            return Ok(None);
        }
        // Resolve single GROUP BY column + collation (must be direct table column, not alias)
        let mut g_expr = &gb[0];
        let mut explicit_coll: Option<&str> = None;
        while let Expr::Collate {
            operand, collation, ..
        } = g_expr
        {
            let _ = env.lookup_collation(&collation.name)?;
            if explicit_coll.is_none() {
                explicit_coll = Some(collation.name.as_str());
            }
            g_expr = operand;
        }
        let Expr::Column(gc) = g_expr else {
            return Ok(None);
        };
        let Some(gcol) = resolve_direct_from_col(
            gc,
            &rel_meta.columns,
            &rel_meta.using_hidden,
            &rel_meta.source_ids,
        ) else {
            return Ok(None);
        };
        let gcoll_name = explicit_coll.unwrap_or(rel_meta.columns[gcol].collation.as_str());
        let gcoll_obj = env.lookup_collation(gcoll_name)?;
        if !is_hashable_collation(gcoll_obj) {
            return Ok(None);
        }
        // Check if GROUP BY matches any non-aggregate explicit SELECT alias (let eval_aggregation handle)
        if gc.table.is_none()
            && expanded.iter().any(|p| {
                p.explicit_alias.as_ref().is_some_and(|a| {
                    a.eq_ignore_ascii_case(&gc.column.name)
                        && !expr_contains_aggregate(&p.expr, self.options)
                })
            })
        {
            return Ok(None);
        }
        // Compile optional WHERE predicate
        let mut pred = if let Some(ref where_expr) = core.where_clause {
            let Some(p) = CompiledRowPred::try_compile(
                where_expr,
                &rel_meta.columns,
                &rel_meta.using_hidden,
                &rel_meta.source_ids,
                env,
            )?
            else {
                return Ok(None);
            };
            if p.has_subquery() {
                let mut sc = EvalScope::simple(&rel_meta.columns, &[]);
                sc.hidden_unqualified = Some(&rel_meta.using_hidden);
                sc.source_ids = Some(&rel_meta.source_ids);
                self.validate_expr_and_subqueries(where_expr, &[sc])?;
            }
            Some(p)
        } else {
            None
        };
        // Validate non-direct SELECT / ORDER BY expressions (columns & subqueries) up front so
        // compile-time errors fire uniformly on both empty and non-empty tables (while allowing
        // `ORDER BY` to reference `SELECT` output aliases such as `ORDER BY cnt DESC`!).
        {
            let mut sc = EvalScope::simple(&rel_meta.columns, &[]);
            sc.hidden_unqualified = Some(&rel_meta.using_hidden);
            sc.source_ids = Some(&rel_meta.source_ids);
            let scopes = [sc];
            for p in &expanded {
                if p.direct_col.is_none() {
                    self.validate_expr_and_subqueries(&p.expr, &scopes)?;
                }
            }
            if let Some(ob) = outer_order_by {
                let ob_aliases: Vec<(String, SqlValue, Affinity, String)> = expanded
                    .iter()
                    .map(|p| {
                        (
                            p.output_name.clone(),
                            SqlValue::Null,
                            Affinity::None,
                            "BINARY".to_string(),
                        )
                    })
                    .collect();
                let mut ob_sc = EvalScope::simple(&rel_meta.columns, &[]);
                ob_sc.hidden_unqualified = Some(&rel_meta.using_hidden);
                ob_sc.source_ids = Some(&rel_meta.source_ids);
                ob_sc.aliases = Some(&ob_aliases);
                let ob_scopes = [ob_sc];
                for t in ob {
                    self.validate_expr_and_subqueries(&t.expr, &ob_scopes)?;
                }
            }
        }
        // Collect aggregate calls in exact evaluation order and verify online plan
        let mut agg_calls: Vec<&FunctionExpr> = Vec::new();
        for p in &expanded {
            if p.direct_col.is_none() {
                collect_aggregates(&p.expr, self.options, &mut agg_calls);
            }
        }
        if let Some(ob) = outer_order_by {
            for t in ob {
                collect_aggregates(&t.expr, self.options, &mut agg_calls);
            }
        }
        let Some(raw_plan) = build_online_agg_plan(&agg_calls, &rel_meta, self.options) else {
            return Ok(None);
        };
        // Deduplicate shared accumulators per group so e.g. `sum(val)` + `avg(val)` + `total(val)`
        // (and repeated `count(*)` / `count(col)`) update ONE accumulator per row!
        let mut plan: Vec<OnlineAggKind> = Vec::with_capacity(raw_plan.len());
        let mut agg_slots: Vec<(usize, Option<SumKind>)> = Vec::with_capacity(raw_plan.len());
        for kind in raw_plan {
            match kind {
                OnlineAggKind::CountStar => {
                    let slot = plan
                        .iter()
                        .position(|k| matches!(k, OnlineAggKind::CountStar))
                        .unwrap_or_else(|| {
                            let s = plan.len();
                            plan.push(OnlineAggKind::CountStar);
                            s
                        });
                    agg_slots.push((slot, None));
                }
                OnlineAggKind::CountCol(cid) => {
                    let slot = plan
                        .iter()
                        .position(|k| matches!(k, OnlineAggKind::CountCol(c) if *c == cid))
                        .unwrap_or_else(|| {
                            let s = plan.len();
                            plan.push(OnlineAggKind::CountCol(cid));
                            s
                        });
                    agg_slots.push((slot, None));
                }
                OnlineAggKind::Sum(cid, sk) => {
                    let slot = plan
                        .iter()
                        .position(|k| matches!(k, OnlineAggKind::Sum(c, _) if *c == cid))
                        .unwrap_or_else(|| {
                            let s = plan.len();
                            plan.push(OnlineAggKind::Sum(cid, sk));
                            s
                        });
                    agg_slots.push((slot, Some(sk)));
                }
            }
        }

        let hint = match core.where_clause.as_ref() {
            Some(we) if !ft.not_indexed => self.extract_scan_hint(&tmeta, alias, we, &[], env),
            _ => TableScanHint::FullScan,
        };
        let needed_indices: Option<Vec<usize>> = table_needed_names.map(|names| {
            tmeta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                        .then_some(i)
                })
                .collect()
        });

        enum GbDirect {
            Col(usize),
            Agg(usize, Option<SumKind>),
        }
        let direct_plan: Option<Vec<GbDirect>> =
            if outer_order_by.is_none() && core.distinct.is_none() {
                let mut dp = Vec::with_capacity(expanded.len());
                let mut all_direct = true;
                let mut agg_cursor = 0usize;
                for p in &expanded {
                    if let Some(didx) = p.direct_col {
                        dp.push(GbDirect::Col(didx));
                    } else if let Expr::Function(f) = &p.expr {
                        if agg_cursor < agg_calls.len()
                            && agg_calls[agg_cursor].span.start == f.span.start
                            && agg_calls[agg_cursor].span.end == f.span.end
                        {
                            let (slot, sk) = agg_slots[agg_cursor];
                            agg_cursor += 1;
                            dp.push(GbDirect::Agg(slot, sk));
                        } else {
                            all_direct = false;
                            break;
                        }
                    } else {
                        all_direct = false;
                        break;
                    }
                }
                all_direct.then_some(dp)
            } else {
                None
            };
        let only_gcol_projected = direct_plan.as_ref().is_some_and(|dp| {
            dp.iter()
                .all(|item| !matches!(item, GbDirect::Col(c) if *c != gcol))
        });

        let stride = plan.len();
        let fast_cs_sum: Option<(usize, SumKind)> = if stride == 2 {
            match (&plan[0], &plan[1]) {
                (OnlineAggKind::CountStar, OnlineAggKind::Sum(sum_cid, sk)) => {
                    Some((*sum_cid, *sk))
                }
                _ => None,
            }
        } else {
            None
        };
        let mut small_int_slots: Vec<u32> = Vec::with_capacity(64);
        let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
        let mut group_reps: Vec<Vec<SqlValue>> = Vec::new();
        let mut group_keys: Vec<SqlValue> = Vec::new();
        let mut group_accs: Vec<OnlineAggAcc> = Vec::new();
        let mut cs_accs: Vec<(i64, crate::functions::aggregates::SumAcc)> = Vec::new();
        let mut mem = self.make_mem_guard();
        let has_meter = self.options.meter.is_some();

        self.catalog.visit_table(
            schema,
            &ft.name.name,
            &hint,
            needed_indices.as_deref(),
            self.options.meter.as_deref(),
            &mut |_rid, vals| {
                if has_meter {
                    self.charge_steps(1)?;
                }
                if let Some(ref mut p) = pred {
                    if !p.eval_row(vals, env)? {
                        return Ok(true);
                    }
                }
                let gid = match &vals[gcol] {
                    SqlValue::Int(k) if (0..4096).contains(k) => {
                        let idx = *k as usize;
                        if idx < small_int_slots.len() && small_int_slots[idx] != 0 {
                            (small_int_slots[idx] - 1) as usize
                        } else {
                            if idx >= small_int_slots.len() {
                                let grow = idx + 1 - small_int_slots.len();
                                if has_meter {
                                    mem.reserve((grow as u64).saturating_mul(4))?;
                                }
                                small_int_slots.resize(idx + 1, 0);
                            }
                            if has_meter {
                                let acc_bytes = (stride as u64)
                                    .saturating_mul(std::mem::size_of::<OnlineAggAcc>() as u64);
                                let rep_bytes = if only_gcol_projected {
                                    estimate_value_bytes(&vals[gcol]).saturating_add(24)
                                } else {
                                    estimate_row_bytes(vals)
                                };
                                mem.reserve(
                                    rep_bytes.saturating_add(acc_bytes).saturating_add(64),
                                )?;
                            }
                            let new_gid = if only_gcol_projected {
                                let g = group_keys.len();
                                group_keys.push(SqlValue::Int(*k));
                                g
                            } else {
                                let g = group_reps.len();
                                group_reps.push(vals.to_vec());
                                g
                            };
                            small_int_slots[idx] = (new_gid + 1) as u32;
                            if let Some((_, sk)) = fast_cs_sum {
                                cs_accs.push((0, crate::functions::aggregates::SumAcc::new(sk)));
                            } else {
                                push_online_accs(&plan, &mut group_accs);
                            }
                            new_gid
                        }
                    }
                    other => {
                        let hkey = GroupKey::from_sql_with_coll(other, gcoll_name);
                        if let GroupKey::Num(k) = hkey {
                            if (0..4096).contains(&k) {
                                let idx = k as usize;
                                if idx < small_int_slots.len() && small_int_slots[idx] != 0 {
                                    (small_int_slots[idx] - 1) as usize
                                } else {
                                    if idx >= small_int_slots.len() {
                                        let grow = idx + 1 - small_int_slots.len();
                                        if has_meter {
                                            mem.reserve((grow as u64).saturating_mul(4))?;
                                        }
                                        small_int_slots.resize(idx + 1, 0);
                                    }
                                    if has_meter {
                                        let acc_bytes = (stride as u64).saturating_mul(
                                            std::mem::size_of::<OnlineAggAcc>() as u64,
                                        );
                                        let rep_bytes = if only_gcol_projected {
                                            estimate_value_bytes(&vals[gcol]).saturating_add(24)
                                        } else {
                                            estimate_row_bytes(vals)
                                        };
                                        mem.reserve(
                                            rep_bytes.saturating_add(acc_bytes).saturating_add(64),
                                        )?;
                                    }
                                    let new_gid = if only_gcol_projected {
                                        let g = group_keys.len();
                                        group_keys.push(vals[gcol].clone());
                                        g
                                    } else {
                                        let g = group_reps.len();
                                        group_reps.push(vals.to_vec());
                                        g
                                    };
                                    small_int_slots[idx] = (new_gid + 1) as u32;
                                    if let Some((_, sk)) = fast_cs_sum {
                                        cs_accs.push((
                                            0,
                                            crate::functions::aggregates::SumAcc::new(sk),
                                        ));
                                    } else {
                                        push_online_accs(&plan, &mut group_accs);
                                    }
                                    new_gid
                                }
                            } else if let Some(&existing) = group_index.get(&hkey) {
                                existing
                            } else {
                                if has_meter {
                                    let acc_bytes = (stride as u64)
                                        .saturating_mul(std::mem::size_of::<OnlineAggAcc>() as u64);
                                    let rep_bytes = if only_gcol_projected {
                                        estimate_value_bytes(&vals[gcol]).saturating_add(24)
                                    } else {
                                        estimate_row_bytes(vals)
                                    };
                                    mem.reserve(
                                        rep_bytes.saturating_add(acc_bytes).saturating_add(64),
                                    )?;
                                }
                                let new_gid = if only_gcol_projected {
                                    let g = group_keys.len();
                                    group_keys.push(vals[gcol].clone());
                                    g
                                } else {
                                    let g = group_reps.len();
                                    group_reps.push(vals.to_vec());
                                    g
                                };
                                group_index.insert(hkey, new_gid);
                                if let Some((_, sk)) = fast_cs_sum {
                                    cs_accs.push((
                                        0,
                                        crate::functions::aggregates::SumAcc::new(sk),
                                    ));
                                } else {
                                    push_online_accs(&plan, &mut group_accs);
                                }
                                new_gid
                            }
                        } else if let Some(&existing) = group_index.get(&hkey) {
                            existing
                        } else {
                            if has_meter {
                                let acc_bytes = (stride as u64)
                                    .saturating_mul(std::mem::size_of::<OnlineAggAcc>() as u64);
                                let key_extra = match &hkey {
                                    GroupKey::Text(b) | GroupKey::Blob(b) => b.len() as u64,
                                    _ => 0,
                                };
                                let rep_bytes = if only_gcol_projected {
                                    estimate_value_bytes(&vals[gcol]).saturating_add(24)
                                } else {
                                    estimate_row_bytes(vals)
                                };
                                mem.reserve(
                                    rep_bytes
                                        .saturating_add(acc_bytes)
                                        .saturating_add(key_extra)
                                        .saturating_add(64),
                                )?;
                            }
                            let new_gid = if only_gcol_projected {
                                let g = group_keys.len();
                                group_keys.push(vals[gcol].clone());
                                g
                            } else {
                                let g = group_reps.len();
                                group_reps.push(vals.to_vec());
                                g
                            };
                            group_index.insert(hkey, new_gid);
                            if let Some((_, sk)) = fast_cs_sum {
                                cs_accs.push((0, crate::functions::aggregates::SumAcc::new(sk)));
                            } else {
                                push_online_accs(&plan, &mut group_accs);
                            }
                            new_gid
                        }
                    }
                };
                if let Some((sum_cid, _)) = fast_cs_sum {
                    let g = &mut cs_accs[gid];
                    g.0 += 1;
                    g.1.step_value(&vals[sum_cid]);
                } else if stride > 0 {
                    let base = gid * stride;
                    for acc in &mut group_accs[base..base + stride] {
                        match acc {
                            OnlineAggAcc::CountStar(c) => *c += 1,
                            OnlineAggAcc::CountCol(cid, c) => {
                                if !vals[*cid].is_null() {
                                    *c += 1;
                                }
                            }
                            OnlineAggAcc::Sum(cid, sa) => sa.step_value(&vals[*cid]),
                        }
                    }
                }
                Ok(true)
            },
        )?;

        let n_groups = if only_gcol_projected {
            group_keys.len()
        } else {
            group_reps.len()
        };
        if n_groups == 0 {
            return Ok(Some(StreamedGroupBy::Rows((
                rel_meta,
                expanded,
                Vec::new(),
                Vec::new(),
            ))));
        }

        // Direct output emission when `outer_order_by.is_none() && core.distinct.is_none()` and
        // every projected item is either a direct column or a top-level aggregate in `agg_calls`:
        // skips building `PrecomputedRow`s, secondary `EvalScope` projection, AND `cs_accs -> group_accs`!
        if let Some(direct_plan) = direct_plan {
            let mut out_cols = Vec::with_capacity(expanded.len());
            let mut out_colls = Vec::with_capacity(expanded.len());
            let mut explicit_aliases = Vec::with_capacity(expanded.len());
            for p in &expanded {
                if let Some(didx) = p.direct_col {
                    let rc = &rel_meta.columns[didx];
                    let coll = if rc.collation.is_empty() {
                        "BINARY".to_string()
                    } else {
                        rc.collation.clone()
                    };
                    out_cols.push(ScopeColumn {
                        table: if p.explicit_alias.is_none() {
                            rc.table.clone()
                        } else {
                            None
                        },
                        name: p.output_name.clone(),
                        affinity: rc.affinity,
                        collation: coll.clone(),
                        is_rowid_alias: false,
                    });
                    out_colls.push(ExprCollation::implicit(coll));
                } else {
                    out_cols.push(ScopeColumn {
                        table: None,
                        name: p.output_name.clone(),
                        affinity: Affinity::None,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    });
                    out_colls.push(ExprCollation::none());
                }
                explicit_aliases.push(p.explicit_alias.is_some());
            }
            let mut out_rows = Vec::with_capacity(n_groups);
            if fast_cs_sum.is_some() {
                for (g, (cnt, sa)) in cs_accs.iter().enumerate() {
                    if has_meter {
                        self.charge_steps(1)?;
                    }
                    let mut out_row = Vec::with_capacity(direct_plan.len());
                    for item in &direct_plan {
                        match *item {
                            GbDirect::Col(c) => {
                                if only_gcol_projected {
                                    out_row.push(group_keys[g].clone());
                                } else {
                                    out_row.push(group_reps[g][c].clone());
                                }
                            }
                            GbDirect::Agg(0, _) => out_row.push(SqlValue::Int(*cnt)),
                            GbDirect::Agg(_, sk_opt) => {
                                out_row.push(sa.value_as(sk_opt.expect("sum kind"))?);
                            }
                        }
                    }
                    if has_meter {
                        mem.reserve(estimate_row_bytes(&out_row))?;
                    }
                    out_rows.push(out_row);
                }
            } else {
                for g in 0..n_groups {
                    if has_meter {
                        self.charge_steps(1)?;
                    }
                    let base = g * stride;
                    let accs = &group_accs[base..base + stride];
                    let mut out_row = Vec::with_capacity(direct_plan.len());
                    for item in &direct_plan {
                        match *item {
                            GbDirect::Col(c) => {
                                if only_gcol_projected {
                                    out_row.push(group_keys[g].clone());
                                } else {
                                    out_row.push(group_reps[g][c].clone());
                                }
                            }
                            GbDirect::Agg(slot, sk_opt) => {
                                let val = match &accs[slot] {
                                    OnlineAggAcc::CountStar(c)
                                    | OnlineAggAcc::CountCol(_, c) => SqlValue::Int(*c),
                                    OnlineAggAcc::Sum(_, sa) => {
                                        sa.value_as(sk_opt.expect("sum kind"))?
                                    }
                                };
                                out_row.push(val);
                            }
                        }
                    }
                    if has_meter {
                        mem.reserve(estimate_row_bytes(&out_row))?;
                    }
                    out_rows.push(out_row);
                }
            }
            let order_ctx = OrderEvalContext {
                from_columns: Vec::new(),
                from_using_hidden: Vec::new(),
                from_source_ids: Vec::new(),
                from_rows: Vec::new(),
                precomputed: Vec::new(),
                explicit_aliases,
            };
            return Ok(Some(StreamedGroupBy::Direct((
                out_cols, out_colls, out_rows, order_ctx,
            ))));
        }

        if let Some((sum_cid, _)) = fast_cs_sum {
            group_accs.reserve(cs_accs.len() * 2);
            for (cnt, sa) in cs_accs {
                group_accs.push(OnlineAggAcc::CountStar(cnt));
                group_accs.push(OnlineAggAcc::Sum(sum_cid, sa));
            }
        }

        let mut out_pre = Vec::with_capacity(n_groups);
        for g in 0..n_groups {
            if has_meter {
                self.charge_steps(1)?;
            }
            let mut pre = PrecomputedRow::with_capacity(agg_calls.len());
            let base = g * stride;
            let accs = &group_accs[base..base + stride];
            let mut pre_bytes = 32u64;
            for (fcall, &(slot, sk_opt)) in agg_calls.iter().zip(&agg_slots) {
                let val = match &accs[slot] {
                    OnlineAggAcc::CountStar(c) | OnlineAggAcc::CountCol(_, c) => SqlValue::Int(*c),
                    OnlineAggAcc::Sum(_, sa) => sa.value_as(sk_opt.expect("sum kind"))?,
                };
                if has_meter {
                    pre_bytes = pre_bytes.saturating_add(estimate_value_bytes(&val).saturating_add(16));
                }
                pre.push(fcall.span.start, fcall.span.end, val, false);
            }
            if has_meter {
                mem.reserve(pre_bytes)?;
            }
            out_pre.push(pre);
        }
        Ok(Some(StreamedGroupBy::Rows((
            rel_meta, expanded, group_reps, out_pre,
        ))))
    }

    // ───────────────────────────── FROM / JOINS ─────────────────────────────

    fn eval_from_depth(
        &self,
        source: &FromSource,
        where_hint_expr: Option<&Expr>,
        order_limit_ctx: Option<(&SelectCore, Option<&[OrderingTerm]>, usize)>,
        needed_col_names: Option<&[&str]>,
        outer_scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<Relation> {
        self.check_depth(depth)?;
        match source {
            FromSource::Table(ft) => {
                let schema = ft.schema.as_ref().map(|n| n.name.as_str());
                let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();

                // 1. Check CTEs first (only when unqualified)
                if schema.is_none() {
                    if let Some(cte) = self.ctes.get(&ft.name.name.to_ascii_lowercase()) {
                        let columns: Vec<ScopeColumn> = cte
                            .columns
                            .iter()
                            .map(|c| ScopeColumn {
                                table: Some(alias.clone()),
                                name: c.name.clone(),
                                affinity: c.affinity,
                                collation: c.collation.clone(),
                                is_rowid_alias: false,
                            })
                            .collect();
                        let star_hidden = vec![false; columns.len()];
                        let using_hidden = vec![false; columns.len()];
                        let source_ids = vec![0u32; columns.len()];
                        let rows = if let Some(m) = self.options.meter.as_deref() {
                            m.charge_steps((cte.rows.len() as u64).max(1))?;
                            let bytes: u64 = cte.rows.iter().map(|r| estimate_row_bytes(r)).sum();
                            m.reserve_memory(bytes)?;
                            let cloned = cte.rows.clone();
                            m.release_memory(bytes);
                            cloned
                        } else {
                            cte.rows.clone()
                        };
                        return Ok(Relation {
                            columns,
                            star_hidden,
                            using_hidden,
                            source_ids,
                            rows,
                        });
                    }
                }

                // 2. Check catalog tables
                if !is_valid_schema_name(schema) {
                    return Err(SqlError::runtime(format!(
                        "no such table: {}.{}",
                        schema.unwrap_or(""),
                        ft.name.name
                    )));
                }
                if let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? {
                    if let Some(ref idx_name) = ft.indexed_by {
                        let exists = tmeta
                            .indexes
                            .iter()
                            .any(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name));
                        if !exists {
                            return Err(SqlError::runtime(format!(
                                "no such index: {}",
                                idx_name.name
                            )));
                        }
                    }
                    let hint = if let Some(we) = where_hint_expr {
                        if ft.not_indexed {
                            TableScanHint::FullScan
                        } else {
                            let hint_env = self.make_env();
                            self.extract_scan_hint(&tmeta, &alias, we, outer_scopes, &hint_env)
                        }
                    } else if let Some((core, ob, limit_k)) = order_limit_ctx {
                        self.extract_order_limit_hint(
                            &tmeta,
                            &alias,
                            ft.not_indexed,
                            ft.indexed_by.as_ref().map(|n| n.name.as_str()),
                            core,
                            ob,
                            limit_k,
                        )
                    } else {
                        TableScanHint::FullScan
                    };
                    let needed_indices: Option<Vec<usize>> = needed_col_names.map(|names| {
                        let mut idxs = Vec::new();
                        for (i, col) in tmeta.columns.iter().enumerate() {
                            if names.iter().any(|n| n.eq_ignore_ascii_case(&col.name)) {
                                idxs.push(i);
                            }
                        }
                        idxs
                    });
                    let scanned = self.catalog.scan_table(
                        schema,
                        &ft.name.name,
                        &hint,
                        needed_indices.as_deref(),
                        self.options.meter.as_deref(),
                    )?;
                    let synth_rowid_needed = tmeta.has_rowid
                        && needed_col_names.is_none_or(|names| {
                            names.iter().any(|n| {
                                n.eq_ignore_ascii_case("rowid")
                                    || n.eq_ignore_ascii_case("_rowid_")
                                    || n.eq_ignore_ascii_case("oid")
                            })
                        });
                    let mut columns = Vec::with_capacity(
                        tmeta.columns.len() + if synth_rowid_needed { 3 } else { 0 },
                    );
                    let mut star_hidden = Vec::with_capacity(columns.capacity());
                    for col in &tmeta.columns {
                        columns.push(ScopeColumn {
                            table: Some(alias.clone()),
                            name: col.name.clone(),
                            affinity: col.affinity,
                            collation: col.collation.clone(),
                            is_rowid_alias: false,
                        });
                        star_hidden.push(col.is_hidden);
                    }
                    // Add synthetic rowid aliases (`rowid`, `_rowid_`, `oid`) for rowid tables
                    // unless a real column already has that name or the query never references them.
                    let mut synth_rowid_slots = 0usize;
                    if synth_rowid_needed {
                        for rname in ["rowid", "_rowid_", "oid"] {
                            let exists = tmeta
                                .columns
                                .iter()
                                .any(|c| c.name.eq_ignore_ascii_case(rname));
                            if !exists {
                                columns.push(ScopeColumn {
                                    table: Some(alias.clone()),
                                    name: rname.to_string(),
                                    affinity: Affinity::Integer,
                                    collation: "BINARY".to_string(),
                                    is_rowid_alias: true,
                                });
                                star_hidden.push(true);
                                synth_rowid_slots += 1;
                            }
                        }
                    }

                    let rows: Vec<Vec<SqlValue>> = if synth_rowid_slots == 0 {
                        scanned.into_iter().map(|s| s.values).collect()
                    } else {
                        let mut out = Vec::with_capacity(scanned.len());
                        for srow in scanned {
                            let mut vals = srow.values;
                            let rid_val = srow.rowid.map_or(SqlValue::Null, SqlValue::Int);
                            vals.reserve(synth_rowid_slots);
                            for _ in 0..synth_rowid_slots {
                                vals.push(rid_val.clone());
                            }
                            out.push(vals);
                        }
                        out
                    };
                    let using_hidden = vec![false; columns.len()];
                    let source_ids = vec![0u32; columns.len()];
                    return Ok(Relation {
                        columns,
                        star_hidden,
                        using_hidden,
                        source_ids,
                        rows,
                    });
                }

                // 3. Check catalog views
                if let Some(vmeta) = self.catalog.get_view(schema, &ft.name.name)? {
                    if self.view_depth >= 64 {
                        return Err(SqlError::runtime(format!(
                            "view {} is circularly defined",
                            vmeta.name
                        )));
                    }
                    let mut view_runner = self.child_with_ctes(HashMap::new());
                    view_runner.view_depth = self.view_depth + 1;
                    let parse_opts = ParseOptions {
                        dialect: self.options.dialect,
                        ..ParseOptions::default()
                    };
                    let stmt = match self.options.meter.as_deref() {
                        Some(m) => parse_statement_metered(&vmeta.select_sql, &parse_opts, m)?,
                        None => xdb_parser::parse_statement(&vmeta.select_sql, &parse_opts)?,
                    };
                    let view_select = match stmt {
                        Statement::Select(s) => s,
                        Statement::CreateView(cv) => cv.select,
                        _ => {
                            return Err(SqlError::runtime(format!(
                                "invalid view definition for {}",
                                vmeta.name
                            )));
                        }
                    };
                    let (mut vcols, vrows) = view_runner.execute_select_stmt(&view_select, &[])?;
                    if let Some(ref explicit_names) = vmeta.columns {
                        if explicit_names.len() != vcols.len() {
                            return Err(SqlError::runtime(format!(
                                "expected {} columns for {} but got {}",
                                explicit_names.len(),
                                vmeta.name,
                                vcols.len()
                            )));
                        }
                        for (c, ename) in vcols.iter_mut().zip(explicit_names.iter()) {
                            c.name = ename.clone();
                        }
                    }
                    for c in &mut vcols {
                        c.table = Some(alias.clone());
                    }
                    let star_hidden = vec![false; vcols.len()];
                    let using_hidden = vec![false; vcols.len()];
                    let source_ids = vec![0u32; vcols.len()];
                    return Ok(Relation {
                        columns: vcols,
                        star_hidden,
                        using_hidden,
                        source_ids,
                        rows: vrows,
                    });
                }

                // 4. Eponymous 0-arg TVF fallback (e.g. `SELECT * FROM pragma_database_list`, `FROM json_each`)
                if let Some(tvf) = self.catalog.call_tvf(
                    schema,
                    &ft.name.name,
                    &[],
                    self.options.meter.as_deref(),
                )? {
                    return Ok(relation_from_tvf(&alias, tvf));
                }
                if schema.is_none() {
                    let lower = ft.name.name.to_ascii_lowercase();
                    if matches!(lower.as_str(), "json_each" | "json_tree") {
                        let env = self.make_env();
                        if let Some(tvf) = call_builtin_tvf(&lower, &[], &env.fn_ctx)? {
                            return Ok(relation_from_tvf(&alias, tvf));
                        }
                    }
                }

                Err(SqlError::runtime(format!(
                    "no such table: {}",
                    ft.name.name
                )))
            }
            FromSource::Function(ff) => {
                let env = self.make_env();
                let mut arg_vals = Vec::with_capacity(ff.args.len());
                for a in &ff.args {
                    arg_vals.push(eval_expr(a, outer_scopes, &env)?);
                }
                let schema = ff.schema.as_ref().map(|n| n.name.as_str());
                let alias = ff.alias.as_ref().unwrap_or(&ff.name).name.clone();
                if let Some(tvf) = self.catalog.call_tvf(
                    schema,
                    &ff.name.name,
                    &arg_vals,
                    self.options.meter.as_deref(),
                )? {
                    return Ok(relation_from_tvf(&alias, tvf));
                }
                let lower = ff.name.name.to_ascii_lowercase();
                let row_cap = order_limit_ctx.and_then(|(_, ob, k)| ob.is_none().then_some(k));
                if let Some(tvf) =
                    call_builtin_tvf_with_limit(&lower, &arg_vals, &env.fn_ctx, row_cap)?
                {
                    return Ok(relation_from_tvf(&alias, tvf));
                }
                Err(SqlError::runtime(format!(
                    "no such table: {}",
                    ff.name.name
                )))
            }
            FromSource::Subquery(fs) => {
                let (mut cols, rows) = self.execute_select_stmt(&fs.select, outer_scopes)?;
                let alias = fs.alias.as_ref().map(|n| n.name.clone());
                for c in &mut cols {
                    c.table = alias.clone();
                }
                let star_hidden = vec![false; cols.len()];
                let using_hidden = vec![false; cols.len()];
                let source_ids = vec![0u32; cols.len()];
                Ok(Relation {
                    columns: cols,
                    star_hidden,
                    using_hidden,
                    source_ids,
                    rows,
                })
            }
            FromSource::Group(fg) => {
                let mut rel = self.eval_from_depth(
                    &fg.source,
                    None,
                    None,
                    needed_col_names,
                    outer_scopes,
                    depth + 1,
                )?;
                if let Some(ref alias) = fg.alias {
                    for c in &mut rel.columns {
                        c.table = Some(alias.name.clone());
                    }
                }
                Ok(rel)
            }
            FromSource::Join {
                left,
                operator,
                right,
                on,
                using,
                ..
            } => {
                // Support correlated table-valued function on RHS (e.g. `FROM t, json_each(t.j)` or
                // `LEFT JOIN json_each(t.j) ON ...`)
                if let FromSource::Function(ff) = right.as_ref() {
                    if using.is_none()
                        && (operator.flags & join_flag::RIGHT) == 0
                        && (operator.flags & join_flag::NATURAL) == 0
                        && ff.args.iter().any(expr_has_column_ref)
                    {
                        let lrel = self.eval_from_depth(
                            left,
                            None,
                            None,
                            needed_col_names,
                            outer_scopes,
                            depth + 1,
                        )?;
                        let is_left = (operator.flags & join_flag::LEFT) != 0;
                        return self.eval_correlated_tvf_join(
                            lrel,
                            ff,
                            is_left,
                            on.as_deref(),
                            outer_scopes,
                        );
                    }
                }

                let lrel = self.eval_from_depth(
                    left,
                    None,
                    None,
                    needed_col_names,
                    outer_scopes,
                    depth + 1,
                )?;
                let rrel = self.eval_from_depth(
                    right,
                    None,
                    None,
                    needed_col_names,
                    outer_scopes,
                    depth + 1,
                )?;
                self.eval_join(lrel, rrel, *operator, on.as_deref(), using.as_deref(), outer_scopes)
            }
        }
    }

    fn eval_correlated_tvf_join(
        &self,
        lrel: Relation,
        ff: &xdb_parser::FromFunction,
        is_left: bool,
        on: Option<&Expr>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Relation> {
        let env = self.make_env();
        let schema = ff.schema.as_ref().map(|n| n.name.as_str());
        let alias = ff.alias.as_ref().unwrap_or(&ff.name).name.clone();
        let lower = ff.name.name.to_ascii_lowercase();

        let mut tvf_cols: Option<Vec<String>> = None;
        let mut combined_cols_cache: Option<Vec<ScopeColumn>> = None;
        let mut combined_using_hidden_cache: Vec<bool> = Vec::new();
        let mut combined_source_ids_cache: Vec<u32> = Vec::new();
        let mut checked_on = false;
        let rhs_sid = lrel.source_ids.iter().copied().max().map_or(0, |m| m + 1);
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut mem = self.make_mem_guard();
        let lwidth = lrel.columns.len();

        for lrow in &lrel.rows {
            self.charge_steps(1)?;
            let mut scopes = outer_scopes.to_vec();
            let mut sc = EvalScope::simple(&lrel.columns, lrow);
            sc.hidden_unqualified = Some(&lrel.using_hidden);
            sc.source_ids = Some(&lrel.source_ids);
            scopes.push(sc);
            let mut arg_vals = Vec::with_capacity(ff.args.len());
            for a in &ff.args {
                arg_vals.push(eval_expr(a, &scopes, &env)?);
            }
            let tvf = if let Some(r) = self.catalog.call_tvf(
                schema,
                &ff.name.name,
                &arg_vals,
                self.options.meter.as_deref(),
            )? {
                r
            } else if let Some(r) = call_builtin_tvf(&lower, &arg_vals, &env.fn_ctx)? {
                r
            } else {
                return Err(SqlError::runtime(format!("no such table: {}", ff.name.name)));
            };
            let rwidth = tvf.columns.len();
            if tvf_cols.is_none() {
                let mut cc = lrel.columns.clone();
                let mut uh = lrel.using_hidden.clone();
                let mut sids = lrel.source_ids.clone();
                for cname in &tvf.columns {
                    cc.push(ScopeColumn {
                        table: Some(alias.clone()),
                        name: cname.clone(),
                        affinity: Affinity::None,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    });
                    uh.push(false);
                    sids.push(rhs_sid);
                }
                combined_cols_cache = Some(cc);
                combined_using_hidden_cache = uh;
                combined_source_ids_cache = sids;
                tvf_cols = Some(tvf.columns);
            }
            let mut matched_any = false;
            for rrow in tvf.rows {
                self.charge_steps(1)?;
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.extend(rrow);
                if let Some(on_expr) = on {
                    checked_on = true;
                    let cc = combined_cols_cache.as_ref().expect("tvf cols built");
                    let mut on_scopes = outer_scopes.to_vec();
                    let mut on_sc = EvalScope::simple(cc, &combined);
                    on_sc.hidden_unqualified = Some(&combined_using_hidden_cache);
                    on_sc.source_ids = Some(&combined_source_ids_cache);
                    on_scopes.push(on_sc);
                    let cond = eval_expr(on_expr, &on_scopes, &env)?;
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }
                matched_any = true;
                mem.reserve(estimate_row_bytes(&combined))?;
                out_rows.push(combined);
            }
            if is_left && !matched_any {
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.resize(lwidth + rwidth, SqlValue::Null);
                mem.reserve(estimate_row_bytes(&combined))?;
                out_rows.push(combined);
            }
        }

        // If left side was empty, obtain TVF column names with NULL args.
        let r_col_names = match tvf_cols {
            Some(c) => c,
            None => {
                let nulls = vec![SqlValue::Null; ff.args.len()];
                if let Some(r) = self.catalog.call_tvf(schema, &ff.name.name, &nulls, None)? {
                    r.columns
                } else if let Some(r) = call_builtin_tvf(&lower, &nulls, &env.fn_ctx)? {
                    r.columns
                } else {
                    return Err(SqlError::runtime(format!("no such table: {}", ff.name.name)));
                }
            }
        };

        let mut columns = lrel.columns;
        let mut star_hidden = lrel.star_hidden;
        let mut using_hidden = lrel.using_hidden;
        let mut source_ids = lrel.source_ids;
        for c in r_col_names {
            columns.push(ScopeColumn {
                table: Some(alias.clone()),
                name: c,
                affinity: Affinity::Blob,
                collation: "BINARY".to_string(),
                is_rowid_alias: false,
            });
            star_hidden.push(false);
            using_hidden.push(false);
            source_ids.push(rhs_sid);
        }
        if let Some(on_expr) = on {
            if !checked_on {
                let mut on_scopes = outer_scopes.to_vec();
                let mut on_sc = EvalScope::simple(&columns, &[]);
                on_sc.hidden_unqualified = Some(&using_hidden);
                on_sc.source_ids = Some(&source_ids);
                on_scopes.push(on_sc);
                self.validate_expr_and_subqueries(on_expr, &on_scopes)?;
            }
        }
        Ok(Relation {
            columns,
            star_hidden,
            using_hidden,
            source_ids,
            rows: out_rows,
        })
    }

    fn eval_join(
        &self,
        lrel: Relation,
        rrel: Relation,
        operator: xdb_parser::JoinOperator,
        on: Option<&Expr>,
        using: Option<&[xdb_parser::Name]>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Relation> {
        let env = self.make_env();
        let is_left = (operator.flags & join_flag::LEFT) != 0;
        let is_right = (operator.flags & join_flag::RIGHT) != 0;
        let is_natural = (operator.flags & join_flag::NATURAL) != 0;

        // Build USING / NATURAL matching column index pairs `(l_idx, r_idx)`.
        let mut using_pairs: Vec<(usize, usize)> = Vec::new();
        let mut hidden_rhs: HashSet<usize> = HashSet::new();

        if is_natural {
            for (li, lc) in lrel.columns.iter().enumerate() {
                if lc.is_rowid_alias || lrel.star_hidden[li] || lrel.using_hidden[li] {
                    continue;
                }
                if let Some((ri, _)) = rrel.columns.iter().enumerate().find(|(ri, rc)| {
                    !rc.is_rowid_alias
                        && !rrel.star_hidden[*ri]
                        && !rrel.using_hidden[*ri]
                        && rc.name.eq_ignore_ascii_case(&lc.name)
                }) {
                    using_pairs.push((li, ri));
                    hidden_rhs.insert(ri);
                }
            }
        } else if let Some(ucols) = using {
            for uname in ucols {
                let li = lrel
                    .columns
                    .iter()
                    .enumerate()
                    .find(|(i, c)| {
                        !c.is_rowid_alias
                            && !lrel.using_hidden[*i]
                            && c.name.eq_ignore_ascii_case(&uname.name)
                    })
                    .map(|(i, _)| i)
                    .ok_or_else(|| {
                        SqlError::runtime(format!(
                            "cannot join using column {} - column not present in both tables",
                            uname.name
                        ))
                    })?;
                let ri = rrel
                    .columns
                    .iter()
                    .enumerate()
                    .find(|(i, c)| {
                        !c.is_rowid_alias
                            && !rrel.using_hidden[*i]
                            && c.name.eq_ignore_ascii_case(&uname.name)
                    })
                    .map(|(i, _)| i)
                    .ok_or_else(|| {
                        SqlError::runtime(format!(
                            "cannot join using column {} - column not present in both tables",
                            uname.name
                        ))
                    })?;
                using_pairs.push((li, ri));
                hidden_rhs.insert(ri);
            }
        }

        let coalesce_using = is_right && !using_pairs.is_empty();
        let lwidth = lrel.columns.len();
        let rwidth = rrel.columns.len();

        // Build output schema. For RIGHT/FULL OUTER JOIN USING, prepend a coalesced
        // unqualified column right before each left USING column and mark both `t1.a` and
        // `t2.a` as `using_hidden`, so `a`/`*` return `coalesce(t1.a, t2.a)` while `t1.a`
        // remains NULL on unmatched right rows and `t1.*`/`t2.*` stay intact!
        let mut combined_cols = Vec::with_capacity(lwidth + rwidth + using_pairs.len());
        let mut combined_star_hidden = Vec::with_capacity(lwidth + rwidth + using_pairs.len());
        let mut combined_using_hidden = Vec::with_capacity(lwidth + rwidth + using_pairs.len());
        let mut combined_source_ids = Vec::with_capacity(lwidth + rwidth + using_pairs.len());
        let mut using_by_left: HashMap<usize, usize> = HashMap::new();
        for &(li, ri) in &using_pairs {
            using_by_left.insert(li, ri);
        }
        let sid_offset = lrel.source_ids.iter().copied().max().map_or(0, |m| m + 1);

        for (li, lc) in lrel.columns.iter().enumerate() {
            let lsid = lrel.source_ids[li];
            if coalesce_using && using_by_left.contains_key(&li) {
                combined_cols.push(ScopeColumn {
                    table: None,
                    name: lc.name.clone(),
                    affinity: lc.affinity,
                    collation: lc.collation.clone(),
                    is_rowid_alias: false,
                });
                combined_star_hidden.push(false);
                combined_using_hidden.push(false);
                combined_source_ids.push(lsid);
                combined_cols.push(lc.clone());
                combined_star_hidden.push(lrel.star_hidden[li]);
                combined_using_hidden.push(true);
                combined_source_ids.push(lsid);
            } else {
                combined_cols.push(lc.clone());
                combined_star_hidden.push(lrel.star_hidden[li]);
                combined_using_hidden.push(lrel.using_hidden[li]);
                combined_source_ids.push(lsid);
            }
        }
        for (ri, rc) in rrel.columns.iter().enumerate() {
            combined_cols.push(rc.clone());
            combined_star_hidden.push(rrel.star_hidden[ri]);
            combined_using_hidden.push(rrel.using_hidden[ri] || hidden_rhs.contains(&ri));
            combined_source_ids.push(rrel.source_ids[ri].saturating_add(sid_offset));
        }

        let assemble_row = |lrow: Option<&[SqlValue]>, rrow: Option<&[SqlValue]>| -> Vec<SqlValue> {
            let mut row = Vec::with_capacity(combined_cols.len());
            for li in 0..lwidth {
                let lv = lrow.map_or(SqlValue::Null, |r| r[li].clone());
                if coalesce_using {
                    if let Some(&ri) = using_by_left.get(&li) {
                        let rv = rrow.map_or(SqlValue::Null, |r| r[ri].clone());
                        let coalesced = if lv.is_null() { rv } else { lv.clone() };
                        row.push(coalesced);
                    }
                }
                row.push(lv);
            }
            if let Some(rr) = rrow {
                row.extend_from_slice(rr);
            } else {
                row.resize(row.len() + rwidth, SqlValue::Null);
            }
            row
        };

        // Fast hash-join path for simple equi-joins (no outer correlation, single equality key
        // with built-in UTF-8 BINARY collation and compatible affinities, or single USING column).
        if let Some((lk, rk, aff, coll_name)) =
            detect_simple_equijoin(&lrel, &rrel, on, &using_pairs)
        {
            let is_builtin_utf8_binary = matches!(
                env.lookup_collation(&coll_name),
                Ok(Collation::Builtin {
                    kind: BuiltinCollation::Binary,
                    encoding: TextEncoding::Utf8,
                })
            );
            if is_builtin_utf8_binary && rrel.rows.len() >= 16 {
                return self.eval_hash_equijoin(
                    lrel,
                    rrel,
                    combined_cols,
                    combined_star_hidden,
                    combined_using_hidden,
                    combined_source_ids,
                    lk,
                    rk,
                    aff,
                    is_left,
                    is_right,
                    coalesce_using,
                    &using_by_left,
                );
            }
        }

        // General nested-loop join (handles arbitrary ON expressions, collations, FULL OUTER, etc.)
        if let Some(on_expr) = on {
            if lrel.rows.is_empty() || rrel.rows.is_empty() {
                let mut scopes = outer_scopes.to_vec();
                let mut sc = EvalScope::simple(&combined_cols, &[]);
                sc.hidden_unqualified = Some(&combined_using_hidden);
                sc.source_ids = Some(&combined_source_ids);
                scopes.push(sc);
                self.validate_expr_and_subqueries(on_expr, &scopes)?;
            }
        }
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut mem = self.make_mem_guard();
        let mut rhs_matched = if is_right {
            vec![false; rrel.rows.len()]
        } else {
            Vec::new()
        };

        for lrow in &lrel.rows {
            let mut matched_any = false;
            for (ridx, rrow) in rrel.rows.iter().enumerate() {
                self.charge_steps(1)?;
                let mut ok = true;
                for &(li, ri) in &using_pairs {
                    let aff = comparison_affinity(
                        Some(lrel.columns[li].affinity),
                        Some(rrel.columns[ri].affinity),
                    );
                    let cname = comparison_collation_name(
                        Some(OperandCollation {
                            name: Some(&lrel.columns[li].collation),
                            explicit: false,
                        }),
                        Some(OperandCollation {
                            name: Some(&rrel.columns[ri].collation),
                            explicit: false,
                        }),
                    );
                    let coll = env.lookup_opt_collation(cname)?;
                    if compare_op(
                        CompareOp::Eq,
                        lrow[li].clone(),
                        rrow[ri].clone(),
                        Some(aff),
                        coll,
                    ) != Some(true)
                    {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                let combined = assemble_row(Some(lrow), Some(rrow));

                if let Some(on_expr) = on {
                    let mut scopes = outer_scopes.to_vec();
                    let mut sc = EvalScope::simple(&combined_cols, &combined);
                    sc.hidden_unqualified = Some(&combined_using_hidden);
                    sc.source_ids = Some(&combined_source_ids);
                    scopes.push(sc);
                    let cond = eval_expr(on_expr, &scopes, &env)?;
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }

                matched_any = true;
                if is_right {
                    rhs_matched[ridx] = true;
                }
                mem.reserve(estimate_row_bytes(&combined))?;
                out_rows.push(combined);
            }
            if is_left && !matched_any {
                let combined = assemble_row(Some(lrow), None);
                mem.reserve(estimate_row_bytes(&combined))?;
                out_rows.push(combined);
            }
        }

        if is_right {
            for (ridx, rrow) in rrel.rows.iter().enumerate() {
                if !rhs_matched[ridx] {
                    self.charge_steps(1)?;
                    let combined = assemble_row(None, Some(rrow));
                    mem.reserve(estimate_row_bytes(&combined))?;
                    out_rows.push(combined);
                }
            }
        }

        Ok(Relation {
            columns: combined_cols,
            star_hidden: combined_star_hidden,
            using_hidden: combined_using_hidden,
            source_ids: combined_source_ids,
            rows: out_rows,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_hash_equijoin(
        &self,
        lrel: Relation,
        rrel: Relation,
        combined_cols: Vec<ScopeColumn>,
        combined_star_hidden: Vec<bool>,
        combined_using_hidden: Vec<bool>,
        combined_source_ids: Vec<u32>,
        lk: usize,
        rk: usize,
        aff: Affinity,
        is_left: bool,
        is_right: bool,
        coalesce_using: bool,
        using_by_left: &HashMap<usize, usize>,
    ) -> SqlResult<Relation> {
        let lwidth = lrel.columns.len();
        let rwidth = rrel.columns.len();
        let assemble_row = |lrow: Option<&[SqlValue]>, rrow: Option<&[SqlValue]>| -> Vec<SqlValue> {
            let mut row = Vec::with_capacity(combined_cols.len());
            for li in 0..lwidth {
                let lv = lrow.map_or(SqlValue::Null, |r| r[li].clone());
                if coalesce_using {
                    if let Some(&ri) = using_by_left.get(&li) {
                        let rv = rrow.map_or(SqlValue::Null, |r| r[ri].clone());
                        let coalesced = if lv.is_null() { rv } else { lv.clone() };
                        row.push(coalesced);
                    }
                }
                row.push(lv);
            }
            if let Some(rr) = rrow {
                row.extend_from_slice(rr);
            } else {
                row.resize(row.len() + rwidth, SqlValue::Null);
            }
            row
        };

        let mut mem = self.make_mem_guard();
        // Match `apply_comparison_affinity` exactly:
        // - `aff >= Numeric`: convert numeric-looking TEXT to Int/Real without lossy Int->Real cast.
        // - `aff == Text`: TEXT vs Number compares as TEXT (stringified number), while Number vs
        //   Number compares numerically (so `1 == 1.0`). We index RHS numbers in BOTH `buckets`
        //   (for num==num) and `text_num_buckets` (for text==num) with zero duplicate emissions.
        let coerce_numeric = |v: &SqlValue| -> SqlValue {
            if aff >= Affinity::Numeric {
                if let SqlValue::Text(b) = v {
                    return apply_numeric_affinity(b, false).unwrap_or_else(|| v.clone());
                }
            }
            v.clone()
        };
        let stringify_num = |v: &SqlValue| -> Option<Box<[u8]>> {
            match v {
                SqlValue::Int(i) => Some(int64_to_text(*i).into_bytes().into_boxed_slice()),
                SqlValue::Real(r) => Some(
                    real_to_text(*r, DEFAULT_FP_DIGITS)
                        .into_bytes()
                        .into_boxed_slice(),
                ),
                _ => None,
            }
        };

        let mut small_int_buckets: Vec<Vec<usize>> = Vec::new();
        let mut buckets: HashMap<HashKey, Vec<usize>> = HashMap::new();
        let mut text_num_buckets: HashMap<Box<[u8]>, Vec<usize>> = HashMap::new();
        let use_small_ints = aff != Affinity::Text;
        let has_meter = self.options.meter.is_some();
        mem.reserve((rrel.rows.len() as u64).saturating_mul(48))?;
        for (ridx, rrow) in rrel.rows.iter().enumerate() {
            self.charge_steps(1)?;
            let rv = coerce_numeric(&rrow[rk]);
            if let Some(key) = HashKey::from_sql(&rv) {
                if use_small_ints {
                    if let HashKey::Num(k) = key {
                        if (0..4096).contains(&k) {
                            let idx = k as usize;
                            if idx >= small_int_buckets.len() {
                                let grow = idx + 1 - small_int_buckets.len();
                                if has_meter {
                                    mem.reserve(
                                        (grow as u64).saturating_mul(
                                            std::mem::size_of::<Vec<usize>>() as u64,
                                        ),
                                    )?;
                                }
                                small_int_buckets.resize_with(idx + 1, Vec::new);
                            }
                            small_int_buckets[idx].push(ridx);
                            continue;
                        }
                    }
                }
                if let HashKey::Text(ref b) | HashKey::Blob(ref b) = key {
                    mem.reserve(b.len() as u64)?;
                }
                buckets.entry(key).or_default().push(ridx);
                if aff == Affinity::Text {
                    if let Some(tb) = stringify_num(&rv) {
                        mem.reserve((tb.len() as u64).saturating_add(32))?;
                        text_num_buckets.entry(tb).or_default().push(ridx);
                    }
                }
            }
        }

        let mut out_rows = Vec::with_capacity(if is_right {
            lrel.rows.len().max(rrel.rows.len())
        } else {
            lrel.rows.len()
        });
        let mut rhs_matched = if is_right {
            vec![false; rrel.rows.len()]
        } else {
            Vec::new()
        };
        let simple_concat = !coalesce_using;

        for mut lrow in lrel.rows {
            self.charge_steps(1)?;
            let mut matched_any = false;
            if use_small_ints {
                if let SqlValue::Int(k) = lrow[lk] {
                    if (0..4096).contains(&k) {
                        if let Some(indices) = small_int_buckets
                            .get(k as usize)
                            .filter(|v| !v.is_empty())
                        {
                            if simple_concat && indices.len() == 1 {
                                self.charge_steps(1)?;
                                let ridx = indices[0];
                                let rrow = &rrel.rows[ridx];
                                if is_right {
                                    rhs_matched[ridx] = true;
                                }
                                lrow.extend_from_slice(rrow);
                                mem.reserve(estimate_row_bytes(&lrow))?;
                                out_rows.push(lrow);
                                continue;
                            }
                            for &ridx in indices {
                                self.charge_steps(1)?;
                                let rrow = &rrel.rows[ridx];
                                matched_any = true;
                                if is_right {
                                    rhs_matched[ridx] = true;
                                }
                                let combined = if simple_concat {
                                    let mut row = Vec::with_capacity(lwidth + rwidth);
                                    row.extend_from_slice(&lrow);
                                    row.extend_from_slice(rrow);
                                    row
                                } else {
                                    assemble_row(Some(&lrow), Some(rrow))
                                };
                                mem.reserve(estimate_row_bytes(&combined))?;
                                out_rows.push(combined);
                            }
                        }
                        if is_left && !matched_any {
                            let combined = assemble_row(Some(&lrow), None);
                            mem.reserve(estimate_row_bytes(&combined))?;
                            out_rows.push(combined);
                        }
                        continue;
                    }
                }
            }
            let lv = coerce_numeric(&lrow[lk]);
            let mut matched_any = false;
            if let Some(key) = HashKey::from_sql(&lv) {
                let matched_slice: Option<&[usize]> = if use_small_ints {
                    if let HashKey::Num(k) = key {
                        if (0..4096).contains(&k) {
                            small_int_buckets
                                .get(k as usize)
                                .filter(|v| !v.is_empty())
                                .map(Vec::as_slice)
                        } else {
                            buckets.get(&key).map(Vec::as_slice)
                        }
                    } else {
                        buckets.get(&key).map(Vec::as_slice)
                    }
                } else {
                    buckets.get(&key).map(Vec::as_slice)
                };
                if let Some(indices) = matched_slice {
                    if simple_concat && aff != Affinity::Text && indices.len() == 1 {
                        self.charge_steps(1)?;
                        let ridx = indices[0];
                        let rrow = &rrel.rows[ridx];
                        if is_right {
                            rhs_matched[ridx] = true;
                        }
                        lrow.extend_from_slice(rrow);
                        mem.reserve(estimate_row_bytes(&lrow))?;
                        out_rows.push(lrow);
                        continue;
                    }
                    for &ridx in indices {
                        self.charge_steps(1)?;
                        let rrow = &rrel.rows[ridx];
                        if aff == Affinity::Text
                            && compare_op(
                                CompareOp::Eq,
                                lrow[lk].clone(),
                                rrow[rk].clone(),
                                Some(aff),
                                None,
                            ) != Some(true)
                        {
                            continue;
                        }
                        matched_any = true;
                        if is_right {
                            rhs_matched[ridx] = true;
                        }
                        let combined = if simple_concat {
                            let mut row = Vec::with_capacity(lwidth + rwidth);
                            row.extend_from_slice(&lrow);
                            row.extend_from_slice(rrow);
                            row
                        } else {
                            assemble_row(Some(&lrow), Some(rrow))
                        };
                        mem.reserve(estimate_row_bytes(&combined))?;
                        out_rows.push(combined);
                    }
                }
                // Cross-type matches under TEXT comparison affinity:
                if aff == Affinity::Text {
                    match &lv {
                        SqlValue::Text(b) => {
                            if let Some(indices) = text_num_buckets.get(b) {
                                for &ridx in indices {
                                    self.charge_steps(1)?;
                                    let rrow = &rrel.rows[ridx];
                                    if compare_op(
                                        CompareOp::Eq,
                                        lrow[lk].clone(),
                                        rrow[rk].clone(),
                                        Some(aff),
                                        None,
                                    ) == Some(true)
                                    {
                                        matched_any = true;
                                        if is_right {
                                            rhs_matched[ridx] = true;
                                        }
                                        let combined = assemble_row(Some(&lrow), Some(rrow));
                                        mem.reserve(estimate_row_bytes(&combined))?;
                                        out_rows.push(combined);
                                    }
                                }
                            }
                        }
                        SqlValue::Int(_) | SqlValue::Real(_) => {
                            if let Some(tb) = stringify_num(&lv) {
                                if let Some(indices) = buckets.get(&HashKey::Text(tb)) {
                                    for &ridx in indices {
                                        self.charge_steps(1)?;
                                        let rrow = &rrel.rows[ridx];
                                        if compare_op(
                                            CompareOp::Eq,
                                            lrow[lk].clone(),
                                            rrow[rk].clone(),
                                            Some(aff),
                                            None,
                                        ) == Some(true)
                                        {
                                            matched_any = true;
                                            if is_right {
                                                rhs_matched[ridx] = true;
                                            }
                                            let combined = assemble_row(Some(&lrow), Some(rrow));
                                            mem.reserve(estimate_row_bytes(&combined))?;
                                            out_rows.push(combined);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if is_left && !matched_any {
                let combined = assemble_row(Some(&lrow), None);
                mem.reserve(estimate_row_bytes(&combined))?;
                out_rows.push(combined);
            }
        }

        if is_right {
            for (ridx, rrow) in rrel.rows.iter().enumerate() {
                if !rhs_matched[ridx] {
                    self.charge_steps(1)?;
                    let combined = assemble_row(None, Some(rrow));
                    mem.reserve(estimate_row_bytes(&combined))?;
                    out_rows.push(combined);
                }
            }
        }

        Ok(Relation {
            columns: combined_cols,
            star_hidden: combined_star_hidden,
            using_hidden: combined_using_hidden,
            source_ids: combined_source_ids,
            rows: out_rows,
        })
    }

    /// Derive a safe `RowidOrder`, `IndexOrder`, or `TopK` scan hint for single-table queries
    /// with `LIMIT` and no `WHERE` / `GROUP BY` / `HAVING` / `DISTINCT` / aggregates / windows.
    #[allow(clippy::too_many_arguments)]
    fn extract_order_limit_hint(
        &self,
        tmeta: &crate::catalog::TableMeta,
        alias: &str,
        not_indexed: bool,
        indexed_by: Option<&str>,
        core: &SelectCore,
        order_by: Option<&[OrderingTerm]>,
        limit_k: usize,
    ) -> TableScanHint {
        if limit_k == 0 {
            return TableScanHint::RowidOrder {
                desc: false,
                limit: 0,
            };
        }
        let Some(terms) = order_by.filter(|t| !t.is_empty()) else {
            if indexed_by.is_none() {
                return TableScanHint::RowidOrder {
                    desc: false,
                    limit: limit_k,
                };
            }
            return TableScanHint::FullScan;
        };
        if terms.len() != 1 {
            return TableScanHint::FullScan;
        }
        let term = &terms[0];

        // Extract outermost explicit COLLATE override (if any) and validate all stacked collations.
        let mut explicit_coll: Option<&str> = None;
        let mut inner_expr = &term.expr;
        while let Expr::Collate {
            operand, collation, ..
        } = inner_expr
        {
            if self.options.collations.find(&collation.name).is_none() {
                return TableScanHint::FullScan;
            }
            if explicit_coll.is_none() {
                explicit_coll = Some(collation.name.as_str());
            }
            inner_expr = operand;
        }

        // Resolve ordinal (1..=N) or explicit SELECT alias to an underlying expression.
        let mut resolved_expr: &Expr = inner_expr;
        if let Some((ord, _)) = integer_ordinal_with_collation(&term.expr) {
            let has_star = core
                .columns
                .iter()
                .any(|rc| matches!(rc, ResultColumn::Star(_)));
            if has_star || ord < 1 || ord > core.columns.len() {
                return TableScanHint::FullScan;
            }
            if let ResultColumn::Expr(re) = &core.columns[ord - 1] {
                resolved_expr = &re.expr;
            } else {
                return TableScanHint::FullScan;
            }
        } else if let Some((bare_name, _)) = bare_unqualified_col_and_coll(&term.expr) {
            if let Some(expr) = core.columns.iter().find_map(|rc| match rc {
                ResultColumn::Expr(re)
                    if re
                        .alias
                        .as_ref()
                        .is_some_and(|a| a.name.eq_ignore_ascii_case(bare_name)) =>
                {
                    Some(&re.expr)
                }
                ResultColumn::Star(rs) => rs.replace.as_deref().and_then(|reps| {
                    reps.iter()
                        .find(|rep| rep.name.name.eq_ignore_ascii_case(bare_name))
                        .map(|rep| &rep.expr)
                }),
                _ => None,
            }) {
                resolved_expr = expr;
            }
        }

        // Unwrap any COLLATE inside the resolved alias/ordinal expression as well.
        while let Expr::Collate {
            operand, collation, ..
        } = resolved_expr
        {
            if self.options.collations.find(&collation.name).is_none() {
                return TableScanHint::FullScan;
            }
            if explicit_coll.is_none() {
                explicit_coll = Some(collation.name.as_str());
            }
            resolved_expr = operand;
        }

        let want_desc = term.order == Some(SortOrder::Desc);
        let nulls_first = match term.nulls {
            Some(NullsOrder::First) => true,
            Some(NullsOrder::Last) => false,
            None => !want_desc,
        };

        // 1. Rowid / INTEGER PRIMARY KEY ordering -> O(log N + K) reverse or forward table scan!
        if tmeta.has_rowid && indexed_by.is_none() && is_rowid_column_ref(resolved_expr, tmeta, alias)
        {
            return TableScanHint::RowidOrder {
                desc: want_desc,
                limit: limit_k,
            };
        }

        // 2. Direct table column -> secondary index ordered scan or bounded storage Top-K!
        let Some(cid) = match_table_col_ref(resolved_expr, tmeta, alias) else {
            return TableScanHint::FullScan;
        };
        let col_meta = &tmeta.columns[cid];
        let eff_coll_name = explicit_coll.unwrap_or(col_meta.collation.as_str());
        let Ok(eff_coll) = self
            .options
            .collations
            .find(eff_coll_name)
            .ok_or(())
        else {
            return TableScanHint::FullScan;
        };

        // Secondary index order matches SQLite default NULLS order (`NULLS FIRST` for ASC,
        // `NULLS LAST` for DESC) for built-in collations on non-partial indexes.
        if tmeta.has_rowid
            && !not_indexed
            && nulls_first != want_desc
            && matches!(eff_coll, Collation::Builtin { .. })
        {
            for idx in &tmeta.indexes {
                if idx.partial {
                    continue;
                }
                if let Some(want_idx) = indexed_by {
                    if !idx.name.eq_ignore_ascii_case(want_idx) {
                        continue;
                    }
                }
                if idx.key_columns.first() != Some(&Some(cid)) {
                    continue;
                }
                if !idx
                    .key_collations
                    .first()
                    .is_some_and(|c| c.eq_ignore_ascii_case(eff_coll_name))
                {
                    continue;
                }
                let idx_desc = idx.key_desc.first().copied().unwrap_or(false);
                return TableScanHint::IndexOrder {
                    index_name: idx.name.clone(),
                    desc: want_desc ^ idx_desc,
                    limit: limit_k,
                };
            }
        }

        // 3. Unindexed stored column with small LIMIT -> single-pass storage Top-K scan!
        if indexed_by.is_none() && !col_meta.is_generated && limit_k <= 2048 {
            return TableScanHint::TopK {
                col_idx: cid,
                desc: want_desc,
                nulls_first,
                collation: eff_coll_name.to_string(),
                limit: limit_k,
            };
        }

        TableScanHint::FullScan
    }

    /// Inspect top-level `WHERE` conjuncts to produce a safe rowid or index seek hint.
    fn extract_scan_hint(
        &self,
        tmeta: &crate::catalog::TableMeta,
        alias: &str,
        where_expr: &Expr,
        outer_scopes: &[EvalScope<'_>],
        env: &EvalEnv<'_>,
    ) -> TableScanHint {
        let mut conjuncts = Vec::new();
        collect_and_conjuncts(where_expr, &mut conjuncts);

        // 1. Check rowid / IPK range, BETWEEN, point lookup, or IN list
        if tmeta.has_rowid {
            let mut low = Bound::Unbounded;
            let mut high = Bound::Unbounded;
            let mut found_rowid_pred = false;
            let mut found_rowid_eq = false;
            let mut rowid_in: Option<Vec<i64>> = None;
            for conj in &conjuncts {
                match conj {
                    Expr::Binary {
                        op, left, right, ..
                    } => {
                        let is_cmp = matches!(
                            op,
                            BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                        );
                        if !is_cmp {
                            continue;
                        }
                        let (col_is_rid, val_expr, eff_op) =
                            if is_rowid_column_ref(left, tmeta, alias)
                                && expr_is_pushdown_const(right, self.options)
                            {
                                (true, right.as_ref(), *op)
                            } else if is_rowid_column_ref(right, tmeta, alias)
                                && expr_is_pushdown_const(left, self.options)
                            {
                                (true, left.as_ref(), flip_cmp(*op))
                            } else {
                                (false, right.as_ref(), *op)
                            };
                        if col_is_rid {
                            if let Ok(v) = eval_expr(val_expr, outer_scopes, env) {
                                if !v.is_null() {
                                    let cv = coerce_index_seek_value(v, Affinity::Integer);
                                    found_rowid_pred = true;
                                    match eff_op {
                                        BinaryOp::Eq => {
                                            low = tighten_low_bound(
                                                low,
                                                Bound::Included(cv.clone()),
                                                None,
                                            );
                                            high = tighten_high_bound(
                                                high,
                                                Bound::Included(cv),
                                                None,
                                            );
                                            found_rowid_eq = true;
                                        }
                                        BinaryOp::Ge => {
                                            low = tighten_low_bound(low, Bound::Included(cv), None);
                                        }
                                        BinaryOp::Gt => {
                                            low = tighten_low_bound(low, Bound::Excluded(cv), None);
                                        }
                                        BinaryOp::Le => {
                                            high =
                                                tighten_high_bound(high, Bound::Included(cv), None);
                                        }
                                        BinaryOp::Lt => {
                                            high =
                                                tighten_high_bound(high, Bound::Excluded(cv), None);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Expr::Between {
                        not: false,
                        operand,
                        low: lo_e,
                        high: hi_e,
                        ..
                    } if is_rowid_column_ref(operand, tmeta, alias)
                        && expr_is_pushdown_const(lo_e, self.options)
                        && expr_is_pushdown_const(hi_e, self.options) =>
                    {
                        if let (Ok(lv), Ok(hv)) = (
                            eval_expr(lo_e, outer_scopes, env),
                            eval_expr(hi_e, outer_scopes, env),
                        ) {
                            if !lv.is_null() {
                                let clv = coerce_index_seek_value(lv, Affinity::Integer);
                                low = tighten_low_bound(low, Bound::Included(clv), None);
                                found_rowid_pred = true;
                            }
                            if !hv.is_null() {
                                let chv = coerce_index_seek_value(hv, Affinity::Integer);
                                high = tighten_high_bound(high, Bound::Included(chv), None);
                                found_rowid_pred = true;
                            }
                        }
                    }
                    Expr::InList {
                        not: false,
                        operand,
                        items,
                        ..
                    } if rowid_in.is_none()
                        && is_rowid_column_ref(operand, tmeta, alias)
                        && !items.is_empty()
                        && items.iter().all(|i| expr_is_pushdown_const(i, self.options)) =>
                    {
                        let mut vals = Vec::with_capacity(items.len());
                        let mut all_ok = true;
                        for it in items {
                            match eval_expr(it, outer_scopes, env) {
                                Ok(SqlValue::Int(i)) => vals.push(i),
                                Ok(SqlValue::Real(r)) => {
                                    if let Some(i) = real_to_exact_i64(r) {
                                        vals.push(i);
                                    }
                                }
                                Ok(SqlValue::Text(b)) => {
                                    if let Some(num) = apply_numeric_affinity(&b, false) {
                                        match num {
                                            SqlValue::Int(i) => vals.push(i),
                                            SqlValue::Real(r) => {
                                                if let Some(i) = real_to_exact_i64(r) {
                                                    vals.push(i);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Ok(SqlValue::Null | SqlValue::Blob(_)) => {}
                                Err(_) => {
                                    all_ok = false;
                                    break;
                                }
                            }
                        }
                        if all_ok {
                            vals.sort_unstable();
                            vals.dedup();
                            rowid_in = Some(vals);
                        }
                    }
                    _ => {}
                }
            }
            if found_rowid_eq {
                return TableScanHint::RowidRange { low, high };
            }
            if let Some(values) = rowid_in {
                if !found_rowid_pred {
                    return TableScanHint::RowidIn { values };
                }
            }
            if found_rowid_pred {
                return TableScanHint::RowidRange { low, high };
            }
        }

        // Helper verifying effective comparison collation & affinity match an index column.
        let index_col_matches = |idx: &crate::catalog::IndexMeta,
                                 kpos: usize,
                                 cid: usize,
                                 val_aff: Option<Affinity>,
                                 val_coll: &ExprCollation,
                                 col_on_left: bool|
         -> bool {
            let col_meta = &tmeta.columns[cid];
            let col_op_coll = Some(OperandCollation {
                name: Some(&col_meta.collation),
                explicit: false,
            });
            let eff_coll = if col_on_left {
                comparison_collation_name(col_op_coll, val_coll.as_operand())
            } else {
                comparison_collation_name(val_coll.as_operand(), col_op_coll)
            }
            .unwrap_or("BINARY");
            if !idx.key_collations[kpos].eq_ignore_ascii_case(eff_coll) {
                return false;
            }
            let comp_aff = comparison_affinity(Some(col_meta.affinity), val_aff);
            comp_aff == col_meta.affinity
        };

        // 2. Check secondary index equality prefix on non-partial indexes.
        let mut eq_by_col: HashMap<usize, (SqlValue, Option<Affinity>, ExprCollation, bool)> =
            HashMap::new();
        for conj in &conjuncts {
            if let Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
                ..
            } = conj
            {
                let pair = if let Some(cid) = match_table_col_ref(left, tmeta, alias) {
                    if expr_is_pushdown_const(right, self.options) {
                        Some((cid, right.as_ref(), true))
                    } else {
                        None
                    }
                } else if let Some(cid) = match_table_col_ref(right, tmeta, alias) {
                    if expr_is_pushdown_const(left, self.options) {
                        Some((cid, left.as_ref(), false))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some((cid, val_expr, col_on_left)) = pair {
                    let val_aff = expr_affinity(val_expr, outer_scopes);
                    let val_coll = expr_collation(val_expr, outer_scopes);
                    if let Ok(v) = eval_expr(val_expr, outer_scopes, env) {
                        if !v.is_null() {
                            eq_by_col
                                .entry(cid)
                                .or_insert((v, val_aff, val_coll, col_on_left));
                        }
                    }
                }
            }
        }
        if !eq_by_col.is_empty() {
            let mut best: Option<(String, Vec<SqlValue>)> = None;
            for idx in &tmeta.indexes {
                if idx.partial || idx.key_columns.is_empty() {
                    continue;
                }
                let mut prefix_vals = Vec::new();
                for (kpos, maybe_cid) in idx.key_columns.iter().enumerate() {
                    let Some(cid) = maybe_cid else { break };
                    let Some((v, val_aff, val_coll, col_on_left)) = eq_by_col.get(cid) else {
                        break;
                    };
                    if !index_col_matches(idx, kpos, *cid, *val_aff, val_coll, *col_on_left) {
                        break;
                    }
                    let col_meta = &tmeta.columns[*cid];
                    prefix_vals.push(coerce_index_seek_value(v.clone(), col_meta.affinity));
                }
                if !prefix_vals.is_empty()
                    && best
                        .as_ref()
                        .is_none_or(|(_, prev)| prefix_vals.len() > prev.len())
                {
                    best = Some((idx.name.clone(), prefix_vals));
                }
            }
            if let Some((index_name, values)) = best {
                return TableScanHint::IndexEq { index_name, values };
            }
        }

        // 3. Check secondary index IN list or range scan on leading key column.
        for idx in &tmeta.indexes {
            if idx.partial || idx.key_columns.is_empty() {
                continue;
            }
            let Some(lead_cid) = idx.key_columns[0] else {
                continue;
            };
            let Ok(lead_coll) = env.lookup_collation(&idx.key_collations[0]) else {
                continue;
            };
            let col_meta = &tmeta.columns[lead_cid];
            let mut low = Bound::Unbounded;
            let mut high = Bound::Unbounded;
            let mut found_range = false;

            for conj in &conjuncts {
                match conj {
                    Expr::InList {
                        not: false,
                        operand,
                        items,
                        ..
                    } if match_table_col_ref(operand, tmeta, alias) == Some(lead_cid)
                        && !items.is_empty()
                        && items.iter().all(|i| expr_is_pushdown_const(i, self.options)) =>
                    {
                        let mut vals = Vec::with_capacity(items.len());
                        let mut ok = true;
                        for it in items {
                            let vaff = expr_affinity(it, outer_scopes);
                            let vcoll = expr_collation(it, outer_scopes);
                            if !index_col_matches(idx, 0, lead_cid, vaff, &vcoll, true) {
                                ok = false;
                                break;
                            }
                            match eval_expr(it, outer_scopes, env) {
                                Ok(v) if !v.is_null() => {
                                    vals.push(coerce_index_seek_value(v, col_meta.affinity));
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            return TableScanHint::IndexIn {
                                index_name: idx.name.clone(),
                                values: vals,
                            };
                        }
                    }
                    Expr::Between {
                        not: false,
                        operand,
                        low: lo_e,
                        high: hi_e,
                        ..
                    } if match_table_col_ref(operand, tmeta, alias) == Some(lead_cid)
                        && expr_is_pushdown_const(lo_e, self.options)
                        && expr_is_pushdown_const(hi_e, self.options) =>
                    {
                        let laff = expr_affinity(lo_e, outer_scopes);
                        let lcoll = expr_collation(lo_e, outer_scopes);
                        let haff = expr_affinity(hi_e, outer_scopes);
                        let hcoll = expr_collation(hi_e, outer_scopes);
                        if index_col_matches(idx, 0, lead_cid, laff, &lcoll, true)
                            && index_col_matches(idx, 0, lead_cid, haff, &hcoll, true)
                        {
                            if let (Ok(lv), Ok(hv)) = (
                                eval_expr(lo_e, outer_scopes, env),
                                eval_expr(hi_e, outer_scopes, env),
                            ) {
                                if !lv.is_null() {
                                    let clv = coerce_index_seek_value(lv, col_meta.affinity);
                                    low = tighten_low_bound(
                                        low,
                                        Bound::Included(clv),
                                        Some(lead_coll),
                                    );
                                    found_range = true;
                                }
                                if !hv.is_null() {
                                    let chv = coerce_index_seek_value(hv, col_meta.affinity);
                                    high = tighten_high_bound(
                                        high,
                                        Bound::Included(chv),
                                        Some(lead_coll),
                                    );
                                    found_range = true;
                                }
                            }
                        }
                    }
                    Expr::Binary {
                        op, left, right, ..
                    } if matches!(
                        op,
                        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                    ) =>
                    {
                        let pair = if match_table_col_ref(left, tmeta, alias) == Some(lead_cid)
                            && expr_is_pushdown_const(right, self.options)
                        {
                            Some((right.as_ref(), *op, true))
                        } else if match_table_col_ref(right, tmeta, alias) == Some(lead_cid)
                            && expr_is_pushdown_const(left, self.options)
                        {
                            Some((left.as_ref(), flip_cmp(*op), false))
                        } else {
                            None
                        };
                        if let Some((ve, eff_op, col_on_left)) = pair {
                            let vaff = expr_affinity(ve, outer_scopes);
                            let vcoll = expr_collation(ve, outer_scopes);
                            if index_col_matches(idx, 0, lead_cid, vaff, &vcoll, col_on_left) {
                                if let Ok(v) = eval_expr(ve, outer_scopes, env) {
                                    if !v.is_null() {
                                        let cv = coerce_index_seek_value(v, col_meta.affinity);
                                        found_range = true;
                                        match eff_op {
                                            BinaryOp::Ge => {
                                                low = tighten_low_bound(
                                                    low,
                                                    Bound::Included(cv),
                                                    Some(lead_coll),
                                                );
                                            }
                                            BinaryOp::Gt => {
                                                low = tighten_low_bound(
                                                    low,
                                                    Bound::Excluded(cv),
                                                    Some(lead_coll),
                                                );
                                            }
                                            BinaryOp::Le => {
                                                high = tighten_high_bound(
                                                    high,
                                                    Bound::Included(cv),
                                                    Some(lead_coll),
                                                );
                                            }
                                            BinaryOp::Lt => {
                                                high = tighten_high_bound(
                                                    high,
                                                    Bound::Excluded(cv),
                                                    Some(lead_coll),
                                                );
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if found_range {
                return TableScanHint::IndexRange {
                    index_name: idx.name.clone(),
                    low,
                    high,
                };
            }
        }

        TableScanHint::FullScan
    }

    // ───────────────────────────── GROUP BY / AGGREGATION ─────────────────────────────

    fn eval_aggregation(
        &self,
        core: &SelectCore,
        rel: &Relation,
        expanded: &[ProjectedItem],
        outer_order_by: Option<&[OrderingTerm]>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<Vec<SqlValue>>, Vec<PrecomputedRow>)> {
        let env = self.make_env();

        // Collect all aggregate calls from SELECT, HAVING, ORDER BY, and named WINDOW clauses.
        let mut agg_calls: Vec<&FunctionExpr> = Vec::new();
        for p in expanded {
            collect_aggregates(&p.expr, self.options, &mut agg_calls);
        }
        let select_agg_count = agg_calls.len();
        if let Some(ref hav) = core.having {
            collect_aggregates(hav, self.options, &mut agg_calls);
        }
        if let Some(ob) = outer_order_by {
            for t in ob {
                collect_aggregates(&t.expr, self.options, &mut agg_calls);
            }
        }
        let used_win_names = collect_referenced_window_names(
            expanded,
            outer_order_by,
            core.windows.as_deref().unwrap_or(&[]),
        );
        for wd in core.windows.as_deref().unwrap_or(&[]) {
            if !used_win_names.contains(&wd.name.name.to_ascii_lowercase()) {
                continue;
            }
            for e in wd.window.partition_by.as_deref().unwrap_or(&[]) {
                collect_aggregates(e, self.options, &mut agg_calls);
            }
            for o in wd.window.order_by.as_deref().unwrap_or(&[]) {
                collect_aggregates(&o.expr, self.options, &mut agg_calls);
            }
        }
        // Reject nested aggregates or window functions inside aggregate args/filter/ORDER BY upfront
        // (catches `SELECT sum(sum(x)) FROM empty_t` and `FILTER (WHERE 0)` even on 0 rows).
        for fcall in &agg_calls {
            for a in &fcall.args {
                if expr_contains_aggregate(a, self.options) {
                    return Err(SqlError::runtime(format!(
                        "misuse of aggregate function {}()",
                        fcall.name.name
                    )));
                }
                if expr_contains_window(a) {
                    return Err(SqlError::runtime("misuse of window function"));
                }
            }
            if let Some(ref flt) = fcall.filter {
                if expr_contains_aggregate(flt, self.options) {
                    return Err(SqlError::runtime(format!(
                        "misuse of aggregate function {}()",
                        fcall.name.name
                    )));
                }
                if expr_contains_window(flt) {
                    return Err(SqlError::runtime("misuse of window function"));
                }
            }
            if let Some(ref ob) = fcall.order_by {
                for o in ob {
                    if expr_contains_aggregate(&o.expr, self.options) {
                        return Err(SqlError::runtime(format!(
                            "misuse of aggregate function {}()",
                            fcall.name.name
                        )));
                    }
                    if expr_contains_window(&o.expr) {
                        return Err(SqlError::runtime("misuse of window function"));
                    }
                }
            }
        }

        // Resolve GROUP BY expressions (including XDB `GROUP BY ALL`, ordinals + COLLATE, and aliases).
        let mut group_exprs: Vec<Expr> = Vec::new();
        if core.group_by_all {
            for p in expanded {
                if !expr_contains_aggregate(&p.expr, self.options) && !expr_contains_window(&p.expr)
                {
                    group_exprs.push(p.expr.clone());
                }
            }
        } else if let Some(ref gb) = core.group_by {
            for (gidx, g) in gb.iter().enumerate() {
                if expr_contains_aggregate(g, self.options) {
                    return Err(SqlError::runtime("misuse of aggregate function"));
                }
                if expr_contains_window(g) {
                    return Err(SqlError::runtime("misuse of window function"));
                }
                if let Some((ord, ov_coll)) = integer_ordinal_with_collation(g) {
                    let mut cw = g;
                    while let Expr::Collate {
                        operand, collation, ..
                    } = cw
                    {
                        let _ = env.lookup_collation(&collation.name)?;
                        cw = operand;
                    }
                    if ord < 1 || ord > expanded.len() {
                        return Err(SqlError::runtime(format!(
                            "{} GROUP BY term out of range - should be between 1 and {}",
                            ordinal_label(gidx + 1),
                            expanded.len()
                        )));
                    }
                    let base = expanded[ord - 1].expr.clone();
                    if expr_contains_aggregate(&base, self.options) {
                        return Err(SqlError::runtime(
                            "aggregate functions are not allowed in the GROUP BY clause",
                        ));
                    }
                    if expr_contains_window(&base) {
                        return Err(SqlError::runtime("misuse of window function"));
                    }
                    if let Some(cn) = ov_coll {
                        group_exprs.push(Expr::Collate {
                            span: g.span(),
                            operand: Box::new(base),
                            collation: xdb_parser::Name {
                                span: g.span(),
                                name: cn.to_string(),
                                quote: xdb_parser::QuoteStyle::None,
                            },
                        });
                    } else {
                        group_exprs.push(base);
                    }
                } else {
                    group_exprs.push(g.clone());
                }
            }
        }

        // SQLite rejects referencing an aggregate SELECT alias in GROUP BY with a specific error
        // (including through `COLLATE` wrappers such as `GROUP BY cnt COLLATE NOCASE`).
        for g in &group_exprs {
            let mut inner = g;
            while let Expr::Collate { operand, .. } = inner {
                inner = operand;
            }
            if let Expr::Column(c) = inner {
                if c.schema.is_none() && c.table.is_none() {
                    let is_agg_alias = expanded.iter().any(|p| {
                        p.explicit_alias
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(&c.column.name))
                            && expr_contains_aggregate(&p.expr, self.options)
                    });
                    if is_agg_alias {
                        let in_rel = rel.columns.iter().enumerate().any(|(idx, rc)| {
                            !rel.using_hidden.get(idx).copied().unwrap_or(false)
                                && rc.name.eq_ignore_ascii_case(&c.column.name)
                        });
                        if !in_rel {
                            return Err(SqlError::runtime(
                                "aggregate functions are not allowed in the GROUP BY clause",
                            ));
                        }
                    }
                }
            }
        }

        // Partition rows into groups.
        let mut groups: Vec<(Vec<SqlValue>, Vec<usize>)> = Vec::new();
        let sample_aliases: Vec<(String, SqlValue, Affinity, String)> = {
            let mut sample_scope = EvalScope::simple(&rel.columns, &[]);
            sample_scope.hidden_unqualified = Some(&rel.using_hidden);
            sample_scope.source_ids = Some(&rel.source_ids);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            expanded
                .iter()
                .filter_map(|p| {
                    let alias = p.explicit_alias.as_ref()?;
                    if expr_contains_aggregate(&p.expr, self.options) {
                        return None;
                    }
                    let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                    let coll = expr_collation(&p.expr, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string());
                    Some((alias.clone(), SqlValue::Null, aff, coll))
                })
                .collect()
        };
        let group_colls: Vec<String> = {
            let mut sample_scope = EvalScope::simple(&rel.columns, &[]);
            sample_scope.hidden_unqualified = Some(&rel.using_hidden);
            sample_scope.source_ids = Some(&rel.source_ids);
            sample_scope.aliases = Some(&sample_aliases);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            if rel.rows.is_empty() {
                for g in &group_exprs {
                    self.validate_expr_and_subqueries(g, &scopes)?;
                }
                if let Some(ref hav) = core.having {
                    let having_aliases: Vec<(String, SqlValue, Affinity, String)> = expanded
                        .iter()
                        .filter_map(|p| {
                            let alias = p.explicit_alias.as_ref()?;
                            let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                            let coll = expr_collation(&p.expr, &scopes)
                                .name
                                .unwrap_or_else(|| "BINARY".to_string());
                            Some((alias.clone(), SqlValue::Null, aff, coll))
                        })
                        .collect();
                    let mut hav_scope = EvalScope::simple(&rel.columns, &[]);
                    hav_scope.hidden_unqualified = Some(&rel.using_hidden);
                    hav_scope.source_ids = Some(&rel.source_ids);
                    hav_scope.aliases = Some(&having_aliases);
                    let mut hav_scopes = outer_scopes.to_vec();
                    hav_scopes.push(hav_scope);
                    self.validate_expr_and_subqueries(hav, &hav_scopes)?;
                }
                for fcall in &agg_calls {
                    for a in &fcall.args {
                        self.validate_expr_and_subqueries(a, &scopes)?;
                    }
                    if let Some(ref flt) = fcall.filter {
                        self.validate_expr_and_subqueries(flt, &scopes)?;
                    }
                    if let Some(ref ob) = fcall.order_by {
                        for o in ob {
                            self.validate_expr_and_subqueries(&o.expr, &scopes)?;
                        }
                    }
                }
                if let Some(ref wins) = core.windows {
                    for wdef in wins {
                        validate_window_spec_columns(&wdef.window, &scopes, env.dqs)?;
                    }
                }
            }
            group_exprs
                .iter()
                .map(|g| {
                    expr_collation(g, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string())
                })
                .collect()
        };

        // Only evaluate SELECT aliases that are actually referenced in GROUP BY / HAVING
        // so we never swallow runtime errors or double-evaluate non-deterministic expressions.
        let gb_needed_aliases: HashSet<String> = {
            let mut names = HashSet::new();
            for g in &group_exprs {
                collect_unqualified_col_names(g, &mut names);
            }
            names
        };
        let gb_uses_alias = !gb_needed_aliases.is_empty()
            && expanded.iter().any(|p| {
                p.explicit_alias.as_ref().is_some_and(|alias| {
                    gb_needed_aliases.contains(&alias.to_ascii_lowercase())
                        && !expr_contains_aggregate(&p.expr, self.options)
                })
            });
        let hav_needed_aliases: HashSet<String> = {
            let mut names = HashSet::new();
            if let Some(ref hav) = core.having {
                collect_unqualified_col_names(hav, &mut names);
            }
            names
        };

        let mut mem = self.make_mem_guard();
        if group_exprs.is_empty() {
            // Single global group (always 1 group even if 0 input rows, unless HAVING filters it!).
            let indices: Vec<usize> = (0..rel.rows.len()).collect();
            groups.push((Vec::new(), indices));
        } else {
            let group_coll_objs: Vec<&Collation> = group_colls
                .iter()
                .map(|c| env.lookup_collation(c))
                .collect::<SqlResult<Vec<_>>>()?;
            let all_hashable = group_coll_objs.iter().all(|c| is_hashable_collation(c));

            // Fast single-column direct GROUP BY path (zero per-row `Vec<GroupKey>` / `eval_expr`!).
            let fast_single_col: Option<usize> = if group_exprs.len() == 1
                && all_hashable
                && outer_scopes.is_empty()
                && !gb_uses_alias
            {
                let mut w = &group_exprs[0];
                while let Expr::Collate {
                    operand, collation, ..
                } = w
                {
                    let _ = env.lookup_collation(&collation.name)?;
                    w = operand;
                }
                if let Expr::Column(c) = w {
                    resolve_direct_from_col(c, &rel.columns, &rel.using_hidden, &rel.source_ids)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(gcol) = fast_single_col {
                let gcoll = group_colls[0].as_str();
                // Single-pass online aggregation fast path (no `Vec<usize>` per group, no
                // `Box<dyn AggregateAcc>`, flat 1D accumulator storage, and O(1) direct slot
                // table for small integer group keys `0..4096`!).
                let online_plan = if core.having.is_none() {
                    build_online_agg_plan(&agg_calls, rel, self.options)
                } else {
                    None
                };

                if let Some(plan) = online_plan {
                    let stride = plan.len();
                    let acc_bytes_per_group =
                        (stride as u64).saturating_mul(std::mem::size_of::<OnlineAggAcc>() as u64);
                    let mut small_int_slots: Vec<u32> = Vec::new();
                    let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
                    let mut group_reps: Vec<usize> = Vec::new();
                    let mut group_accs: Vec<OnlineAggAcc> = Vec::new();
                    let has_meter = self.options.meter.is_some();

                    for (ridx, row) in rel.rows.iter().enumerate() {
                        self.charge_steps(1)?;
                        let gid = match &row[gcol] {
                            SqlValue::Int(k) if (0..4096).contains(k) => {
                                let idx = *k as usize;
                                if idx < small_int_slots.len() && small_int_slots[idx] != 0 {
                                    (small_int_slots[idx] - 1) as usize
                                } else {
                                    if idx >= small_int_slots.len() {
                                        let grow = idx + 1 - small_int_slots.len();
                                        if has_meter {
                                            mem.reserve((grow as u64).saturating_mul(4))?;
                                        }
                                        small_int_slots.resize(idx + 1, 0);
                                    }
                                    if has_meter {
                                        mem.reserve(
                                            estimate_row_bytes(std::slice::from_ref(&row[gcol]))
                                                .saturating_add(64)
                                                .saturating_add(acc_bytes_per_group),
                                        )?;
                                    }
                                    let new_gid = group_reps.len();
                                    small_int_slots[idx] = (new_gid + 1) as u32;
                                    group_reps.push(ridx);
                                    push_online_accs(&plan, &mut group_accs);
                                    new_gid
                                }
                            }
                            other => {
                                let hkey = GroupKey::from_sql_with_coll(other, gcoll);
                                if let GroupKey::Num(k) = hkey {
                                    if (0..4096).contains(&k) {
                                        let idx = k as usize;
                                        if idx < small_int_slots.len()
                                            && small_int_slots[idx] != 0
                                        {
                                            (small_int_slots[idx] - 1) as usize
                                        } else {
                                            if idx >= small_int_slots.len() {
                                                let grow = idx + 1 - small_int_slots.len();
                                                if has_meter {
                                                    mem.reserve((grow as u64).saturating_mul(4))?;
                                                }
                                                small_int_slots.resize(idx + 1, 0);
                                            }
                                            if has_meter {
                                                mem.reserve(
                                                    estimate_row_bytes(std::slice::from_ref(
                                                        &row[gcol],
                                                    ))
                                                    .saturating_add(64)
                                                    .saturating_add(acc_bytes_per_group),
                                                )?;
                                            }
                                            let new_gid = group_reps.len();
                                            small_int_slots[idx] = (new_gid + 1) as u32;
                                            group_reps.push(ridx);
                                            push_online_accs(&plan, &mut group_accs);
                                            new_gid
                                        }
                                    } else if let Some(&existing) = group_index.get(&hkey) {
                                        existing
                                    } else {
                                        if has_meter {
                                            mem.reserve(
                                                estimate_row_bytes(std::slice::from_ref(
                                                    &row[gcol],
                                                ))
                                                .saturating_add(64)
                                                .saturating_add(acc_bytes_per_group),
                                            )?;
                                        }
                                        let new_gid = group_reps.len();
                                        group_index.insert(hkey, new_gid);
                                        group_reps.push(ridx);
                                        push_online_accs(&plan, &mut group_accs);
                                        new_gid
                                    }
                                } else if let Some(&existing) = group_index.get(&hkey) {
                                    existing
                                } else {
                                    if has_meter {
                                        mem.reserve(
                                            estimate_row_bytes(std::slice::from_ref(&row[gcol]))
                                                .saturating_add(64)
                                                .saturating_add(acc_bytes_per_group),
                                        )?;
                                    }
                                    let new_gid = group_reps.len();
                                    group_index.insert(hkey, new_gid);
                                    group_reps.push(ridx);
                                    push_online_accs(&plan, &mut group_accs);
                                    new_gid
                                }
                            }
                        };

                        if stride > 0 {
                            let base = gid * stride;
                            for acc in &mut group_accs[base..base + stride] {
                                match acc {
                                    OnlineAggAcc::CountStar(c) => *c += 1,
                                    OnlineAggAcc::CountCol(cid, c) => {
                                        if !row[*cid].is_null() {
                                            *c += 1;
                                        }
                                    }
                                    OnlineAggAcc::Sum(cid, sa) => sa.step_value(&row[*cid]),
                                }
                            }
                        }
                    }

                    let final_ctx = env.fn_ctx.with_collation_and_json_mask(None, 0);
                    let mut out_rows = Vec::with_capacity(group_reps.len());
                    let mut out_pre = Vec::with_capacity(group_reps.len());
                    for (g, rep_idx) in group_reps.into_iter().enumerate() {
                        let mut pre = PrecomputedRow::with_capacity(stride);
                        let base = g * stride;
                        for (fcall, acc) in agg_calls.iter().zip(&group_accs[base..base + stride]) {
                            let val = match acc {
                                OnlineAggAcc::CountStar(c) | OnlineAggAcc::CountCol(_, c) => {
                                    SqlValue::Int(*c)
                                }
                                OnlineAggAcc::Sum(_, sa) => sa.value(&final_ctx)?,
                            };
                            pre.push(fcall.span.start, fcall.span.end, val, false);
                        }
                        let rep_row = rel.rows[rep_idx].clone();
                        if has_meter {
                            mem.reserve(estimate_row_bytes(&rep_row))?;
                        }
                        out_rows.push(rep_row);
                        out_pre.push(pre);
                    }
                    return Ok((out_rows, out_pre));
                }

                let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
                for (ridx, row) in rel.rows.iter().enumerate() {
                    self.charge_steps(1)?;
                    let hkey = GroupKey::from_sql_with_coll(&row[gcol], gcoll);
                    if let Some(&gid) = group_index.get(&hkey) {
                        groups[gid].1.push(ridx);
                    } else {
                        mem.reserve(estimate_row_bytes(std::slice::from_ref(&row[gcol])) + 64)?;
                        let gid = groups.len();
                        group_index.insert(hkey, gid);
                        groups.push((Vec::new(), vec![ridx]));
                    }
                }
            } else {
                let mut group_index: HashMap<Vec<GroupKey>, usize> = HashMap::new();

                for (ridx, row) in rel.rows.iter().enumerate() {
                    self.charge_steps(1)?;
                    let mut key_vals = Vec::with_capacity(group_exprs.len());
                    if outer_scopes.is_empty() && !gb_uses_alias {
                        let mut sc = EvalScope::simple(&rel.columns, row);
                        sc.hidden_unqualified = Some(&rel.using_hidden);
                        sc.source_ids = Some(&rel.source_ids);
                        let sslice = [sc];
                        for g in &group_exprs {
                            key_vals.push(eval_expr(g, &sslice, &env)?);
                        }
                    } else {
                        let mut scopes = outer_scopes.to_vec();
                        let mut sc = EvalScope::simple(&rel.columns, row);
                        sc.hidden_unqualified = Some(&rel.using_hidden);
                        sc.source_ids = Some(&rel.source_ids);
                        scopes.push(sc);

                        // Expose referenced SELECT aliases for GROUP BY expressions!
                        let mut alias_vals: Vec<(String, SqlValue, Affinity, String)> = Vec::new();
                        if !gb_needed_aliases.is_empty() {
                            for p in expanded {
                                let Some(alias) = p.explicit_alias.as_ref() else {
                                    continue;
                                };
                                if !gb_needed_aliases.contains(&alias.to_ascii_lowercase())
                                    || expr_contains_aggregate(&p.expr, self.options)
                                {
                                    continue;
                                }
                                let v = eval_expr(&p.expr, &scopes, &env)?;
                                let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                                let coll = expr_collation(&p.expr, &scopes)
                                    .name
                                    .unwrap_or_else(|| "BINARY".to_string());
                                alias_vals.push((alias.clone(), v, aff, coll));
                            }
                            if let Some(last) = scopes.last_mut() {
                                last.aliases = Some(&alias_vals);
                            }
                        }

                        for g in &group_exprs {
                            key_vals.push(eval_expr(g, &scopes, &env)?);
                        }
                    }

                    if all_hashable {
                        let hkey: Vec<GroupKey> = key_vals
                            .iter()
                            .enumerate()
                            .map(|(k, v)| GroupKey::from_sql_with_coll(v, &group_colls[k]))
                            .collect();
                        if let Some(&gid) = group_index.get(&hkey) {
                            groups[gid].1.push(ridx);
                        } else {
                            mem.reserve(estimate_row_bytes(&key_vals) + 64)?;
                            let gid = groups.len();
                            group_index.insert(hkey, gid);
                            groups.push((key_vals, vec![ridx]));
                        }
                    } else {
                        let mut found = None;
                        for (gid, (gvals, _)) in groups.iter().enumerate() {
                            self.charge_steps(1)?;
                            let mut eq = true;
                            for (k, (a, b)) in gvals.iter().zip(key_vals.iter()).enumerate() {
                                let coll = env.lookup_collation(&group_colls[k])?;
                                if mem_compare(a, b, Some(coll)) != Ordering::Equal {
                                    eq = false;
                                    break;
                                }
                            }
                            if eq {
                                found = Some(gid);
                                break;
                            }
                        }
                        if let Some(gid) = found {
                            groups[gid].1.push(ridx);
                        } else {
                            mem.reserve(estimate_row_bytes(&key_vals) + 64)?;
                            groups.push((key_vals, vec![ridx]));
                        }
                    }
                }
            }
        }

        // Check SQLite single min()/max() bare-column rule:
        // If there is exactly 1 aggregate call in SELECT and it is 1-arg `min` or `max`
        // (even if HAVING/ORDER BY repeats that same aggregate!), non-aggregated bare columns
        // in SELECT/HAVING come from the row that produced the min/max!
        let single_minmax = select_agg_count == 1
            && (agg_calls[0].name.name.eq_ignore_ascii_case("min")
                || agg_calls[0].name.name.eq_ignore_ascii_case("max"))
            && agg_calls[0].args.len() == 1
            && !agg_calls[0].star;

        let mut out_rows = Vec::with_capacity(groups.len());
        let mut out_pre = Vec::with_capacity(groups.len());

        for (_, row_indices) in groups {
            self.charge_steps(1)?;
            let mut pre = PrecomputedRow::with_capacity(agg_calls.len());
            let mut chosen_row_idx: Option<usize> = row_indices.first().copied();

            for (aidx, fcall) in agg_calls.iter().enumerate() {
                let (val, winner) = self.eval_single_aggregate(
                    fcall,
                    &rel.columns,
                    &rel.using_hidden,
                    &rel.source_ids,
                    &rel.rows,
                    &row_indices,
                    outer_scopes,
                    &env,
                )?;
                if single_minmax && aidx == 0 && winner.is_some() {
                    chosen_row_idx = winner;
                }
                let is_j = is_json_aggregate_name(&fcall.name.name);
                pre.push(fcall.span.start, fcall.span.end, val, is_j);
            }

            let rep_row = match chosen_row_idx {
                Some(idx) => rel.rows[idx].clone(),
                None => vec![SqlValue::Null; rel.columns.len()],
            };

            // Evaluate HAVING if present
            if let Some(ref hav) = core.having {
                let mut scopes = outer_scopes.to_vec();
                scopes.push(EvalScope {
                    columns: &rel.columns,
                    values: &rep_row,
                    precomputed: Some(&pre.slots),
                    precomputed_json: Some(&pre.is_json),
                    hidden_unqualified: Some(&rel.using_hidden),
                    source_ids: Some(&rel.source_ids),
                    aliases: None,
                    no_rowid_aliases: false,
                });
                let mut alias_vals: Vec<(String, SqlValue, Affinity, String)> = Vec::new();
                if !hav_needed_aliases.is_empty() {
                    for p in expanded {
                        let Some(alias) = p.explicit_alias.as_ref() else {
                            continue;
                        };
                        if !hav_needed_aliases.contains(&alias.to_ascii_lowercase()) {
                            continue;
                        }
                        let v = eval_expr(&p.expr, &scopes, &env)?;
                        let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                        let coll = expr_collation(&p.expr, &scopes)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        alias_vals.push((alias.clone(), v, aff, coll));
                    }
                    if let Some(last) = scopes.last_mut() {
                        last.aliases = Some(&alias_vals);
                    }
                }
                let cond = eval_expr(hav, &scopes, &env)?;
                if !boolean_value(&cond, false) {
                    continue;
                }
            }

            let pre_bytes: u64 = pre
                .slots
                .iter()
                .map(|(_, _, v)| estimate_value_bytes(v).saturating_add(16))
                .sum();
            mem.reserve(estimate_row_bytes(&rep_row).saturating_add(pre_bytes))?;
            out_rows.push(rep_row);
            out_pre.push(pre);
        }

        Ok((out_rows, out_pre))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_single_aggregate(
        &self,
        fcall: &FunctionExpr,
        columns: &[ScopeColumn],
        using_hidden: &[bool],
        source_ids: &[u32],
        all_rows: &[Vec<SqlValue>],
        row_indices: &[usize],
        outer_scopes: &[EvalScope<'_>],
        env: &EvalEnv<'_>,
    ) -> SqlResult<(SqlValue, Option<usize>)> {
        let mut acc = self
            .options
            .functions
            .create_aggregate(&fcall.name.name, fcall.args.len(), fcall.star)?;

        // Determine collation from first argument if any.
        let mut agg_coll_name = "BINARY".to_string();
        let agg_coll: Option<&Collation> = if let Some(first_arg) = fcall.args.first() {
            let mut sample_scope = EvalScope::simple(columns, &[]);
            sample_scope.hidden_unqualified = Some(using_hidden);
            sample_scope.source_ids = Some(source_ids);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            let oc = expr_collation(first_arg, &scopes);
            match oc.name {
                Some(ref cname) => {
                    agg_coll_name = cname.clone();
                    Some(env.lookup_collation(cname)?)
                }
                None => None,
            }
        } else {
            None
        };

        let ob_terms = fcall.order_by.as_deref().unwrap_or(&[]);
        let is_distinct = fcall.quantifier == Some(DistinctKind::Distinct);

        // Fast O(1) path for unfiltered `count(*)` / `count()` without ORDER BY or DISTINCT.
        if ob_terms.is_empty()
            && !is_distinct
            && fcall.filter.is_none()
            && fcall.name.name.eq_ignore_ascii_case("count")
            && (fcall.star || fcall.args.is_empty())
        {
            if !row_indices.is_empty() {
                self.charge_steps(row_indices.len() as u64)?;
            }
            return Ok((SqlValue::Int(row_indices.len() as i64), None));
        }

        // Fast streaming path when no aggregate ORDER BY and no DISTINCT: steps accumulator
        // directly without allocating `surviving` or per-row `arg_vals` Vecs!
        if ob_terms.is_empty() && !is_distinct {
            let mut winner: Option<usize> = None;
            // Direct single-column streaming fast path (e.g. `sum(val)`, `avg(val)`, `min(val)`, `max(val)`).
            if fcall.args.len() == 1 && fcall.filter.is_none() && outer_scopes.is_empty() {
                if let Expr::Column(c) = &fcall.args[0] {
                    if let Some(cid) = resolve_direct_from_col(c, columns, using_hidden, source_ids) {
                        let mut acc_mem = self.make_mem_guard();
                        let has_meter = self.options.meter.is_some();
                        let call_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
                        for &ridx in row_indices {
                            self.charge_steps(1)?;
                            let prev_bytes = if has_meter { acc.heap_bytes() } else { 0 };
                            acc.step(std::slice::from_ref(&all_rows[ridx][cid]), &call_ctx)?;
                            if has_meter {
                                let cur_bytes = acc.heap_bytes();
                                if cur_bytes > prev_bytes {
                                    acc_mem.reserve(cur_bytes - prev_bytes)?;
                                }
                            }
                            if !acc.skip_bare_column_load() {
                                winner = Some(ridx);
                            }
                        }
                        let final_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
                        let val = acc.finalize(&final_ctx)?;
                        return Ok((val, winner));
                    }
                }
            }
            let mut acc_mem = self.make_mem_guard();
            let mut scopes_buf = outer_scopes.to_vec();
            let mut arg_vals = Vec::with_capacity(fcall.args.len());
            for &ridx in row_indices {
                self.charge_steps(1)?;
                let row = &all_rows[ridx];
                let mut sc = EvalScope::simple(columns, row);
                sc.hidden_unqualified = Some(using_hidden);
                sc.source_ids = Some(source_ids);
                scopes_buf.push(sc);

                if let Some(ref flt) = fcall.filter {
                    let cond = eval_expr(flt, &scopes_buf, env)?;
                    if !boolean_value(&cond, false) {
                        scopes_buf.pop();
                        continue;
                    }
                }
                arg_vals.clear();
                let mut jmask = 0u64;
                for (i, a) in fcall.args.iter().enumerate() {
                    let (v, is_j) = crate::eval::eval_expr_tagged(a, &scopes_buf, env)?;
                    if is_j && i < 64 {
                        jmask |= 1u64 << i;
                    }
                    arg_vals.push(v);
                }
                scopes_buf.pop();
                let prev_bytes = if self.options.meter.is_some() {
                    acc.heap_bytes()
                } else {
                    0
                };
                let call_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, jmask);
                acc.step(&arg_vals, &call_ctx)?;
                if self.options.meter.is_some() {
                    let cur_bytes = acc.heap_bytes();
                    if cur_bytes > prev_bytes {
                        acc_mem.reserve(cur_bytes - prev_bytes)?;
                    }
                }
                if !acc.skip_bare_column_load() {
                    winner = Some(ridx);
                }
            }
            let final_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
            let val = acc.finalize(&final_ctx)?;
            return Ok((val, winner));
        }

        // 1. Collect surviving `(row_idx, arg_vals, arg_json_mask, sort_keys)` after FILTER.
        let mut mem = self.make_mem_guard();
        let has_meter = self.options.meter.is_some();
        let mut surviving: Vec<(usize, Vec<SqlValue>, u64, Vec<SqlValue>)> =
            Vec::with_capacity(row_indices.len());
        let mut scopes_buf = outer_scopes.to_vec();

        for &ridx in row_indices {
            self.charge_steps(1)?;
            let row = &all_rows[ridx];
            let mut sc = EvalScope::simple(columns, row);
            sc.hidden_unqualified = Some(using_hidden);
            sc.source_ids = Some(source_ids);
            scopes_buf.push(sc);

            if let Some(ref flt) = fcall.filter {
                let cond = eval_expr(flt, &scopes_buf, env)?;
                if !boolean_value(&cond, false) {
                    scopes_buf.pop();
                    continue;
                }
            }
            let mut arg_vals = Vec::with_capacity(fcall.args.len());
            let mut jmask = 0u64;
            for (i, a) in fcall.args.iter().enumerate() {
                let (v, is_j) = crate::eval::eval_expr_tagged(a, &scopes_buf, env)?;
                if is_j && i < 64 {
                    jmask |= 1u64 << i;
                }
                arg_vals.push(v);
            }
            let mut skeys = Vec::with_capacity(ob_terms.len());
            for ot in ob_terms {
                skeys.push(eval_expr(&ot.expr, &scopes_buf, env)?);
            }
            scopes_buf.pop();
            if has_meter {
                mem.reserve(
                    estimate_row_bytes(&arg_vals)
                        .saturating_add(estimate_row_bytes(&skeys))
                        .saturating_add(64),
                )?;
            }
            surviving.push((ridx, arg_vals, jmask, skeys));
        }

        // 2. Apply aggregate ORDER BY if present
        if !ob_terms.is_empty() {
            if has_meter && surviving.len() > 1 {
                let n = surviving.len() as u64;
                let lg = (u64::BITS - n.leading_zeros()) as u64;
                self.charge_steps(n.saturating_mul(lg))?;
            }
            let ob_colls: Vec<&Collation> = {
                let mut sample_scope = EvalScope::simple(columns, &[]);
                sample_scope.hidden_unqualified = Some(using_hidden);
                sample_scope.source_ids = Some(source_ids);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample_scope);
                let mut v = Vec::with_capacity(ob_terms.len());
                for ot in ob_terms {
                    let cname = expr_collation(&ot.expr, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string());
                    v.push(env.lookup_collation(&cname)?);
                }
                v
            };
            surviving.sort_by(|a, b| compare_sort_keys(&a.3, &b.3, ob_terms, &ob_colls));
        }

        // 3. Apply DISTINCT if present
        if fcall.quantifier == Some(DistinctKind::Distinct) {
            if fcall.args.len() != 1 {
                return Err(SqlError::runtime(
                    "DISTINCT aggregates must have exactly one argument",
                ));
            }
            let keep_null = fcall.name.name.eq_ignore_ascii_case("json_group_array");
            let mut deduped = Vec::with_capacity(surviving.len());
            let eff_agg_coll = match agg_coll {
                Some(c) => c,
                None => env.lookup_collation(&agg_coll_name)?,
            };
            if is_hashable_collation(eff_agg_coll) {
                let mut seen: HashSet<GroupKey> = HashSet::new();
                for item in surviving {
                    self.charge_steps(1)?;
                    let v = &item.1[0];
                    if v.is_null() && !keep_null {
                        continue;
                    }
                    let gk = GroupKey::from_sql_with_coll(v, &agg_coll_name);
                    if seen.insert(gk) {
                        if has_meter {
                            mem.reserve(estimate_value_bytes(v).saturating_add(32))?;
                        }
                        deduped.push(item);
                    }
                }
            } else {
                let mut seen_vals: Vec<SqlValue> = Vec::new();
                for item in surviving {
                    self.charge_steps(1)?;
                    let v = &item.1[0];
                    if v.is_null() && !keep_null {
                        continue;
                    }
                    let mut dup = false;
                    for s in &seen_vals {
                        self.charge_steps(1)?;
                        if mem_compare(s, v, agg_coll) == Ordering::Equal {
                            dup = true;
                            break;
                        }
                    }
                    if !dup {
                        if has_meter {
                            mem.reserve(estimate_value_bytes(v).saturating_add(32))?;
                        }
                        seen_vals.push(v.clone());
                        deduped.push(item);
                    }
                }
            }
            surviving = deduped;
        }

        // 4. Step accumulator
        let mut winner: Option<usize> = None;
        let mut acc_reserved = 0u64;
        for (ridx, arg_vals, jmask, _) in surviving {
            self.charge_steps(1)?;
            let call_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, jmask);
            acc.step(&arg_vals, &call_ctx)?;
            if has_meter {
                let cur = acc.heap_bytes();
                if cur > acc_reserved {
                    mem.reserve(cur - acc_reserved)?;
                    acc_reserved = cur;
                }
            }
            if !acc.skip_bare_column_load() {
                winner = Some(ridx);
            }
        }

        let final_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
        let val = acc.finalize(&final_ctx)?;
        Ok((val, winner))
    }

    // ───────────────────────────── WINDOW FUNCTIONS ─────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn eval_windows(
        &self,
        core: &SelectCore,
        columns: &[ScopeColumn],
        using_hidden: &[bool],
        source_ids: &[u32],
        rows: &[Vec<SqlValue>],
        sink: &mut WindowSink<'_>,
        expanded: &[ProjectedItem],
        outer_order_by: Option<&[OrderingTerm]>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<()> {
        let mut win_calls: Vec<&FunctionExpr> = Vec::new();
        for p in expanded {
            if p.direct_col.is_none() {
                collect_windows(&p.expr, &mut win_calls);
            }
        }
        if let Some(ob) = outer_order_by {
            for t in ob {
                collect_windows(&t.expr, &mut win_calls);
            }
        }

        let named_windows = core.windows.as_deref().unwrap_or(&[]);

        for wcall in win_calls {
            let Some(ref wref) = wcall.over else {
                continue;
            };
            if wcall.order_by.is_some() {
                return Err(SqlError::runtime(format!(
                    "ORDER BY may not be used with non-aggregate {}()",
                    wcall.name.name
                )));
            }
            if wcall.quantifier == Some(DistinctKind::Distinct) {
                return Err(SqlError::runtime(
                    "DISTINCT is not supported for window functions",
                ));
            }
            let lower_name = wcall.name.name.to_ascii_lowercase();
            let is_win_only = is_builtin_window_only(&lower_name);
            if is_win_only {
                let arity_ok = !wcall.star
                    && match lower_name.as_str() {
                        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
                            wcall.args.is_empty()
                        }
                        "ntile" | "first_value" | "last_value" => wcall.args.len() == 1,
                        "nth_value" => wcall.args.len() == 2,
                        "lag" | "lead" => (1..=3).contains(&wcall.args.len()),
                        _ => false,
                    };
                if !arity_ok {
                    return Err(SqlError::runtime(format!(
                        "wrong number of arguments to function {}()",
                        wcall.name.name
                    )));
                }
            } else if !self
                .options
                .functions
                .is_aggregate(&wcall.name.name, wcall.args.len(), wcall.star)
            {
                return Err(SqlError::runtime(format!(
                    "{}() may not be used as a window function",
                    wcall.name.name
                )));
            }
            if is_win_only && wcall.filter.is_some() {
                return Err(SqlError::runtime(format!(
                    "FILTER clause may only be used with aggregate window functions: {lower_name}()"
                )));
            }
            let spec = resolve_window_ref(wref, named_windows)?;
            // Create `env` AFTER `spec` so any subqueries inside a cloned/merged `WindowSpec`
            // strictly outlive `env`'s pointer-keyed subquery cache (preventing heap ABA reuse).
            let env = self.make_env();
            let part_exprs = spec.partition_by.as_deref().unwrap_or(&[]);
            let ord_terms = spec.order_by.as_deref().unwrap_or(&[]);
            // Reject nested window functions upfront (even on 0-row inputs, matching SQLite).
            if wcall.args.iter().any(expr_contains_window)
                || wcall.filter.as_deref().is_some_and(expr_contains_window)
                || part_exprs.iter().any(expr_contains_window)
                || ord_terms.iter().any(|ot| expr_contains_window(&ot.expr))
            {
                return Err(SqlError::runtime("misuse of window function"));
            }

            if let Some(ref fspec) = spec.frame {
                let needs_range_order = fspec.units == xdb_parser::FrameUnits::Range
                    && (matches!(
                        fspec.from.kind,
                        xdb_parser::FrameBoundType::Preceding
                            | xdb_parser::FrameBoundType::Following
                    ) || fspec.to.as_ref().is_some_and(|t| {
                        matches!(
                            t.kind,
                            xdb_parser::FrameBoundType::Preceding
                                | xdb_parser::FrameBoundType::Following
                        )
                    }));
                if needs_range_order && ord_terms.len() != 1 {
                    return Err(SqlError::runtime(
                        "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression",
                    ));
                }
            }

            let part_coll_names: Vec<String> = {
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
                sample.source_ids = Some(source_ids);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample);
                part_exprs
                    .iter()
                    .map(|pe| {
                        expr_collation(pe, &scopes)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string())
                    })
                    .collect()
            };
            let part_colls: Vec<&Collation> = part_coll_names
                .iter()
                .map(|c| env.lookup_collation(c))
                .collect::<SqlResult<Vec<_>>>()?;

            let ord_colls: Vec<&Collation> = {
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
                sample.source_ids = Some(source_ids);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample);
                let mut v = Vec::with_capacity(ord_terms.len());
                for ot in ord_terms {
                    let cname = expr_collation(&ot.expr, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string());
                    v.push(env.lookup_collation(&cname)?);
                }
                v
            };

            // Resolve and validate frame spec upfront (even when 0 rows survive, matching SQLite).
            let resolved_frame = match &spec.frame {
                Some(fspec) => ResolvedFrame::from_ast(fspec, |b| match &b.expr {
                    Some(e) => Ok(Some(eval_expr(e, outer_scopes, &env)?)),
                    None => Ok(None),
                })?,
                None => ResolvedFrame::default_for(!ord_terms.is_empty()),
            };

            if rows.is_empty() {
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
                sample.source_ids = Some(source_ids);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample);
                for a in &wcall.args {
                    self.validate_expr_and_subqueries(a, &scopes)?;
                }
                if let Some(ref flt) = wcall.filter {
                    self.validate_expr_and_subqueries(flt, &scopes)?;
                }
                for pe in part_exprs {
                    self.validate_expr_and_subqueries(pe, &scopes)?;
                }
                for ot in ord_terms {
                    self.validate_expr_and_subqueries(&ot.expr, &scopes)?;
                }
                continue;
            }

            // Evaluate partition keys and order keys for every row (skipping allocations when
            // PARTITION BY is empty or ORDER BY is a single direct column!).
            let direct_ord_col: Option<usize> = if ord_terms.len() == 1 && outer_scopes.is_empty() {
                if let Expr::Column(c) = &ord_terms[0].expr {
                    resolve_direct_from_col(c, columns, using_hidden, source_ids)
                } else {
                    None
                }
            } else {
                None
            };
            let first_ord_dir = ord_terms
                .first()
                .and_then(|o| o.order)
                .unwrap_or(SortOrder::Asc);
            let first_ord_nulls = ord_terms.first().and_then(|o| o.nulls);
            let first_ord_coll = ord_colls.first().copied();
            let is_json_win = !is_win_only && is_json_aggregate_name(&wcall.name.name);
            let direct_sliding_agg: Option<OnlineAggKind> = if !is_win_only
                && resolved_frame.exclude == FrameExclude::NoOthers
                && wcall.filter.is_none()
                && outer_scopes.is_empty()
                && self
                    .options
                    .functions
                    .is_aggregate(&wcall.name.name, wcall.args.len(), wcall.star)
            {
                classify_online_agg(wcall, columns, using_hidden, source_ids, self.options)
            } else {
                None
            };
            let is_cumulative_frame = resolved_frame.start_kind == FrameBoundType::UnboundedPreceding
                && resolved_frame.end_kind == FrameBoundType::CurrentRow;
            let mut win_mem = self.make_mem_guard();
            let has_meter = self.options.meter.is_some();

            // Zero-sort, zero-peer-table, zero-partition-vec fast path for single-partition
            // cumulative online aggregates (`sum(val) OVER (ORDER BY id)`) when input rows are
            // already strictly monotonic along `direct_ord_col`!
            if part_exprs.is_empty() && is_cumulative_frame {
                if let (Some(ocol), Some(kind)) = (direct_ord_col, direct_sliding_agg) {
                    let ocoll = ord_colls[0];
                    let mut strictly_sorted = true;
                    for i in 1..rows.len() {
                        let a = &rows[i - 1][ocol];
                        let b = &rows[i][ocol];
                        let ok = match (a, b) {
                            (SqlValue::Int(x), SqlValue::Int(y)) => {
                                if first_ord_dir == SortOrder::Desc {
                                    x > y
                                } else {
                                    x < y
                                }
                            }
                            _ => {
                                compare_ordered_val(a, b, first_ord_dir, first_ord_nulls, ocoll)
                                    == Ordering::Less
                            }
                        };
                        if !ok {
                            strictly_sorted = false;
                            break;
                        }
                    }
                    if strictly_sorted {
                        let fctx = env.fn_ctx.with_collation_and_json_mask(None, 0);
                        let mut acc = make_online_acc(kind);
                        match sink {
                            WindowSink::Columnar(cols) => {
                                let mut vals = Vec::with_capacity(rows.len());
                                for row in rows {
                                    self.charge_steps(1)?;
                                    step_online_acc(&mut acc, row);
                                    let v = value_online_acc(&acc, &fctx)?;
                                    if has_meter {
                                        win_mem
                                            .reserve(estimate_value_bytes(&v).saturating_add(16))?;
                                    }
                                    vals.push(v);
                                }
                                cols.push(WindowCol {
                                    span_start: wcall.span.start,
                                    span_end: wcall.span.end,
                                    values: vals,
                                });
                            }
                            WindowSink::PerRow(precomputed) => {
                                for (ridx, row) in rows.iter().enumerate() {
                                    self.charge_steps(1)?;
                                    step_online_acc(&mut acc, row);
                                    let v = value_online_acc(&acc, &fctx)?;
                                    if has_meter {
                                        win_mem
                                            .reserve(estimate_value_bytes(&v).saturating_add(16))?;
                                    }
                                    precomputed[ridx].push(
                                        wcall.span.start,
                                        wcall.span.end,
                                        v,
                                        is_json_win,
                                    );
                                }
                            }
                        }
                        continue;
                    }
                }
            }

            let mut row_pkeys: Vec<Vec<SqlValue>> = if part_exprs.is_empty() {
                Vec::new()
            } else {
                Vec::with_capacity(rows.len())
            };
            let mut row_okeys: Vec<Vec<SqlValue>> =
                if ord_terms.is_empty() || direct_ord_col.is_some() {
                    Vec::new()
                } else {
                    Vec::with_capacity(rows.len())
                };
            if !part_exprs.is_empty() || (!ord_terms.is_empty() && direct_ord_col.is_none()) {
                let eval_ord = !ord_terms.is_empty() && direct_ord_col.is_none();
                for (ridx, row) in rows.iter().enumerate() {
                    self.charge_steps(1)?;
                    let (pre_slots, pre_json) = match &*sink {
                        WindowSink::PerRow(pre) => {
                            (Some(pre[ridx].slots.as_slice()), Some(pre[ridx].is_json.as_slice()))
                        }
                        WindowSink::Columnar(_) => (None, None),
                    };
                    let sc = EvalScope {
                        columns,
                        values: row,
                        precomputed: pre_slots,
                        precomputed_json: pre_json,
                        hidden_unqualified: Some(using_hidden),
                        source_ids: Some(source_ids),
                        aliases: None,
                        no_rowid_aliases: false,
                    };
                    let mut pk = if part_exprs.is_empty() {
                        Vec::new()
                    } else {
                        Vec::with_capacity(part_exprs.len())
                    };
                    let mut ok = if eval_ord {
                        Vec::with_capacity(ord_terms.len())
                    } else {
                        Vec::new()
                    };
                    if outer_scopes.is_empty() {
                        let sslice = [sc];
                        for pe in part_exprs {
                            pk.push(eval_expr(pe, &sslice, &env)?);
                        }
                        if eval_ord {
                            for ot in ord_terms {
                                ok.push(eval_expr(&ot.expr, &sslice, &env)?);
                            }
                        }
                    } else {
                        let mut scopes = outer_scopes.to_vec();
                        scopes.push(sc);
                        for pe in part_exprs {
                            pk.push(eval_expr(pe, &scopes, &env)?);
                        }
                        if eval_ord {
                            for ot in ord_terms {
                                ok.push(eval_expr(&ot.expr, &scopes, &env)?);
                            }
                        }
                    }
                    if has_meter {
                        if !pk.is_empty() {
                            win_mem.reserve(estimate_row_bytes(&pk))?;
                        }
                        if !ok.is_empty() {
                            win_mem.reserve(estimate_row_bytes(&ok))?;
                        }
                    }
                    if !part_exprs.is_empty() {
                        row_pkeys.push(pk);
                    }
                    if eval_ord {
                        row_okeys.push(ok);
                    }
                }
            }

            // Group row indices into partitions (hash-based for built-in collations, metered otherwise).
            let mut partitions: Vec<Vec<usize>> = Vec::new();
            if part_exprs.is_empty() {
                partitions.push((0..rows.len()).collect());
            } else if part_colls.iter().all(|c| is_hashable_collation(c)) {
                let mut pmap: HashMap<Vec<GroupKey>, usize> = HashMap::new();
                for (ridx, pkey) in row_pkeys.iter().enumerate() {
                    self.charge_steps(1)?;
                    let hkey: Vec<GroupKey> = pkey
                        .iter()
                        .enumerate()
                        .map(|(k, v)| GroupKey::from_sql_with_coll(v, &part_coll_names[k]))
                        .collect();
                    if let Some(&pid) = pmap.get(&hkey) {
                        partitions[pid].push(ridx);
                    } else {
                        if has_meter {
                            win_mem.reserve(estimate_row_bytes(pkey).saturating_add(48))?;
                        }
                        let pid = partitions.len();
                        pmap.insert(hkey, pid);
                        partitions.push(vec![ridx]);
                    }
                }
            } else {
                let mut p_rep_keys: Vec<usize> = Vec::new();
                for ridx in 0..rows.len() {
                    self.charge_steps(1)?;
                    let mut found = None;
                    for (pid, &rep) in p_rep_keys.iter().enumerate() {
                        self.charge_steps(1)?;
                        let same = row_pkeys[rep]
                            .iter()
                            .zip(row_pkeys[ridx].iter())
                            .enumerate()
                            .all(|(k, (a, b))| {
                                mem_compare(a, b, Some(part_colls[k])) == Ordering::Equal
                            });
                        if same {
                            found = Some(pid);
                            break;
                        }
                    }
                    if let Some(pid) = found {
                        partitions[pid].push(ridx);
                    } else {
                        p_rep_keys.push(ridx);
                        partitions.push(vec![ridx]);
                    }
                }
            }

            let needs_range_order_vals = resolved_frame.units == FrameUnits::Range
                && (matches!(
                    resolved_frame.start_kind,
                    FrameBoundType::Preceding | FrameBoundType::Following
                ) || matches!(
                    resolved_frame.end_kind,
                    FrameBoundType::Preceding | FrameBoundType::Following
                ));
            let mut col_buf: Option<Vec<SqlValue>> = if matches!(sink, WindowSink::Columnar(_)) {
                Some(vec![SqlValue::Null; rows.len()])
            } else {
                None
            };

            // Evaluate window function on each partition
            for mut part in partitions {
                let plen = part.len();
                if plen > 1 && (direct_ord_col.is_some() || !ord_terms.is_empty()) {
                    let sort_steps =
                        (plen as u64).saturating_mul((usize::BITS - plen.leading_zeros()) as u64);
                    self.charge_steps((sort_steps / 16).max(1))?;
                }
                let mut already_sorted = false;
                let mut strictly_distinct = false;
                if let Some(ocol) = direct_ord_col {
                    let ocoll = ord_colls[0];
                    let mut is_sorted = true;
                    let mut is_distinct = true;
                    for i in 1..plen {
                        let a = &rows[part[i - 1]][ocol];
                        let b = &rows[part[i]][ocol];
                        let ord = match (a, b) {
                            (SqlValue::Int(x), SqlValue::Int(y)) => {
                                if first_ord_dir == SortOrder::Desc {
                                    y.cmp(x)
                                } else {
                                    x.cmp(y)
                                }
                            }
                            _ => compare_ordered_val(a, b, first_ord_dir, first_ord_nulls, ocoll),
                        };
                        match ord {
                            Ordering::Less => {}
                            Ordering::Equal => {
                                is_distinct = false;
                            }
                            Ordering::Greater => {
                                is_sorted = false;
                                is_distinct = false;
                                break;
                            }
                        }
                    }
                    already_sorted = is_sorted;
                    strictly_distinct = is_sorted && is_distinct;
                    if !already_sorted {
                        part.sort_by(|&a, &b| {
                            compare_ordered_val(
                                &rows[a][ocol],
                                &rows[b][ocol],
                                first_ord_dir,
                                first_ord_nulls,
                                ocoll,
                            )
                        });
                    }
                } else if !ord_terms.is_empty() {
                    part.sort_by(|&a, &b| {
                        compare_sort_keys(&row_okeys[a], &row_okeys[b], ord_terms, &ord_colls)
                    });
                }
                self.charge_steps(plen as u64)?;
                // Fast cumulative path for strictly-distinct partitioned windows (`PARTITION BY p ORDER BY id`)
                if strictly_distinct && is_cumulative_frame {
                    if let Some(kind) = direct_sliding_agg {
                        let fctx = env.fn_ctx.with_collation_and_json_mask(None, 0);
                        let mut acc = make_online_acc(kind);
                        for &ridx in &part {
                            self.charge_steps(1)?;
                            step_online_acc(&mut acc, &rows[ridx]);
                            let v = value_online_acc(&acc, &fctx)?;
                            if has_meter {
                                win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                            }
                            if let Some(ref mut cb) = col_buf {
                                cb[ridx] = v;
                            } else if let WindowSink::PerRow(precomputed) = sink {
                                precomputed[ridx].push(
                                    wcall.span.start,
                                    wcall.span.end,
                                    v,
                                    is_json_win,
                                );
                            }
                        }
                        continue;
                    }
                }
                let _ = already_sorted;
                let peers = if let Some(ocol) = direct_ord_col {
                    let ocoll = ord_colls[0];
                    PartitionPeers::build(plen, |i, j| {
                        let va = &rows[part[i]][ocol];
                        let vb = &rows[part[j]][ocol];
                        match (va, vb) {
                            (SqlValue::Int(x), SqlValue::Int(y)) => x == y,
                            _ => mem_compare(va, vb, Some(ocoll)) == Ordering::Equal,
                        }
                    })
                } else {
                    PartitionPeers::build(plen, |i, j| {
                        if ord_terms.is_empty() {
                            return true;
                        }
                        let ra = part[i];
                        let rb = part[j];
                        row_okeys[ra]
                            .iter()
                            .zip(row_okeys[rb].iter())
                            .enumerate()
                            .all(|(k, (a, b))| {
                                mem_compare(a, b, Some(ord_colls[k])) == Ordering::Equal
                            })
                    })
                };
                let first_order_vals: Vec<SqlValue> = if needs_range_order_vals && !ord_terms.is_empty() {
                    if let Some(ocol) = direct_ord_col {
                        part.iter().map(|&r| rows[r][ocol].clone()).collect()
                    } else {
                        part.iter().map(|&r| row_okeys[r][0].clone()).collect()
                    }
                } else {
                    Vec::new()
                };

                // Direct-column stack sliding-window fast path for `sum`/`total`/`avg`/`count`:
                // zero `part_args`, zero `Box<dyn AggregateAcc>`, zero `eval_expr_tagged`!
                if let Some(kind) = direct_sliding_agg {
                    let fctx = env.fn_ctx.with_collation_and_json_mask(None, 0);
                    let mut vals = Vec::with_capacity(plen);
                    let mut acc = make_online_acc(kind);
                    let mut cur_start = 0usize;
                    let mut cur_end = 0usize;
                    for i in 0..plen {
                        self.charge_steps(1)?;
                        let (new_start, new_end) = compute_frame_bounds(
                            &resolved_frame,
                            i,
                            plen,
                            &peers,
                            &first_order_vals,
                            first_ord_dir,
                            first_ord_nulls,
                            first_ord_coll,
                        )?;
                        if new_start != cur_start || new_end != cur_end {
                            if new_start >= new_end {
                                for &ridx in &part[cur_start..cur_end] {
                                    self.charge_steps(1)?;
                                    inverse_online_acc(&mut acc, &rows[ridx]);
                                }
                                cur_start = 0;
                                cur_end = 0;
                            } else if cur_start == cur_end {
                                for &ridx in &part[new_start..new_end] {
                                    self.charge_steps(1)?;
                                    step_online_acc(&mut acc, &rows[ridx]);
                                }
                                cur_start = new_start;
                                cur_end = new_end;
                            } else if new_start >= cur_start && new_end >= cur_end {
                                let evict_end = cur_end.min(new_start);
                                for &ridx in &part[cur_start..evict_end] {
                                    self.charge_steps(1)?;
                                    inverse_online_acc(&mut acc, &rows[ridx]);
                                }
                                let enter_start = cur_end.max(new_start);
                                for &ridx in &part[enter_start..new_end] {
                                    self.charge_steps(1)?;
                                    step_online_acc(&mut acc, &rows[ridx]);
                                }
                                cur_start = new_start;
                                cur_end = new_end;
                            } else {
                                acc = make_online_acc(kind);
                                for &ridx in &part[new_start..new_end] {
                                    self.charge_steps(1)?;
                                    step_online_acc(&mut acc, &rows[ridx]);
                                }
                                cur_start = new_start;
                                cur_end = new_end;
                            }
                        }
                        let v = value_online_acc(&acc, &fctx)?;
                        if has_meter {
                            win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                        }
                        vals.push(v);
                    }
                    for (ppos, val) in vals.into_iter().enumerate() {
                        let ridx = part[ppos];
                        if let Some(ref mut cb) = col_buf {
                            cb[ridx] = val;
                        } else if let WindowSink::PerRow(precomputed) = sink {
                            precomputed[ridx].push(
                                wcall.span.start,
                                wcall.span.end,
                                val,
                                is_json_win,
                            );
                        }
                    }
                    continue;
                }

                // Evaluate function args & FILTER for each row in partition.
                let mut part_args: Vec<Vec<SqlValue>> = Vec::with_capacity(plen);
                let mut part_jmasks: Vec<u64> = Vec::with_capacity(plen);
                let mut part_filter: Vec<bool> = Vec::with_capacity(plen);
                for &ridx in &part {
                    self.charge_steps(1)?;
                    let (pre_slots, pre_json) = match &*sink {
                        WindowSink::PerRow(pre) => {
                            (Some(pre[ridx].slots.as_slice()), Some(pre[ridx].is_json.as_slice()))
                        }
                        WindowSink::Columnar(_) => (None, None),
                    };
                    let sc = EvalScope {
                        columns,
                        values: &rows[ridx],
                        precomputed: pre_slots,
                        precomputed_json: pre_json,
                        hidden_unqualified: Some(using_hidden),
                        source_ids: Some(source_ids),
                        aliases: None,
                        no_rowid_aliases: false,
                    };
                    let mut avals = Vec::with_capacity(wcall.args.len());
                    let mut jmask = 0u64;
                    let keep = if outer_scopes.is_empty() {
                        let sslice = [sc];
                        let k = match &wcall.filter {
                            Some(flt) => boolean_value(&eval_expr(flt, &sslice, &env)?, false),
                            None => true,
                        };
                        for (ai, ae) in wcall.args.iter().enumerate() {
                            let (v, is_j) = crate::eval::eval_expr_tagged(ae, &sslice, &env)?;
                            if is_j && ai < 64 {
                                jmask |= 1u64 << ai;
                            }
                            avals.push(v);
                        }
                        k
                    } else {
                        let mut scopes = outer_scopes.to_vec();
                        scopes.push(sc);
                        let k = match &wcall.filter {
                            Some(flt) => boolean_value(&eval_expr(flt, &scopes, &env)?, false),
                            None => true,
                        };
                        for (ai, ae) in wcall.args.iter().enumerate() {
                            let (v, is_j) = crate::eval::eval_expr_tagged(ae, &scopes, &env)?;
                            if is_j && ai < 64 {
                                jmask |= 1u64 << ai;
                            }
                            avals.push(v);
                        }
                        k
                    };
                    if !avals.is_empty() {
                        win_mem.reserve(estimate_row_bytes(&avals))?;
                    }
                    part_filter.push(keep);
                    part_args.push(avals);
                    part_jmasks.push(jmask);
                }

                let results: Vec<SqlValue> = if is_win_only {
                    eval_window_only_partition(&lower_name, &part_args, &peers, |i| {
                        let frows = compute_frame_rows(
                            &resolved_frame,
                            i,
                            plen,
                            &peers,
                            &first_order_vals,
                            first_ord_dir,
                            first_ord_nulls,
                            first_ord_coll,
                        )?;
                        self.charge_steps((frows.len() as u64 / 32) + 1)?;
                        Ok(frows)
                    })?
                } else if self
                    .options
                    .functions
                    .is_aggregate(&wcall.name.name, wcall.args.len(), wcall.star)
                {
                    let agg_coll: Option<&Collation> = if let Some(first_arg) = wcall.args.first() {
                        let mut sample = EvalScope::simple(columns, &[]);
                        sample.hidden_unqualified = Some(using_hidden);
                        sample.source_ids = Some(source_ids);
                        let mut scopes = outer_scopes.to_vec();
                        scopes.push(sample);
                        match expr_collation(first_arg, &scopes).name {
                            Some(ref cname) => Some(env.lookup_collation(cname)?),
                            None => None,
                        }
                    } else {
                        None
                    };

                    let mut vals = Vec::with_capacity(plen);
                    if resolved_frame.exclude == FrameExclude::NoOthers {
                        // O(N) incremental sliding-window evaluation for monotonic frames
                        // (handles running totals for ALL aggregates, and moving windows for
                        // any aggregate supporting `inverse()`).
                        let mut acc = self.options.functions.create_aggregate(
                            &wcall.name.name,
                            wcall.args.len(),
                            wcall.star,
                        )?;
                        let can_slide = acc.supports_inverse();
                        let mut cur_start = 0usize;
                        let mut cur_end = 0usize;
                        let fctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);

                        for i in 0..plen {
                            self.charge_steps(1)?;
                            let (new_start, new_end) = compute_frame_bounds(
                                &resolved_frame,
                                i,
                                plen,
                                &peers,
                                &first_order_vals,
                                first_ord_dir,
                                first_ord_nulls,
                                first_ord_coll,
                            )?;
                            if new_start == cur_start && new_end == cur_end {
                                let v = acc.value(&fctx)?;
                                win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                                vals.push(v);
                                continue;
                            }
                            if new_start >= new_end {
                                if cur_start < cur_end {
                                    if can_slide {
                                        for r in cur_start..cur_end {
                                            self.charge_steps(1)?;
                                            if !part_filter[r] {
                                                continue;
                                            }
                                            let cctx = env.fn_ctx.with_collation_and_json_mask(
                                                agg_coll,
                                                part_jmasks[r],
                                            );
                                            acc.inverse(&part_args[r], &cctx)?;
                                        }
                                    } else {
                                        acc = self.options.functions.create_aggregate(
                                            &wcall.name.name,
                                            wcall.args.len(),
                                            wcall.star,
                                        )?;
                                    }
                                }
                                cur_start = 0;
                                cur_end = 0;
                                let v = acc.value(&fctx)?;
                                win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                                vals.push(v);
                                continue;
                            }
                            let incremental_ok = (cur_start == cur_end)
                                || (new_start >= cur_start
                                    && new_end >= cur_end
                                    && ((new_start == cur_start && new_start < cur_end)
                                        || can_slide));
                            if incremental_ok {
                                let mut peak_acc_heap = acc.heap_bytes();
                                if cur_start == cur_end {
                                    for r in new_start..new_end {
                                        self.charge_steps(1)?;
                                        if !part_filter[r] {
                                            continue;
                                        }
                                        let cctx = env.fn_ctx.with_collation_and_json_mask(
                                            agg_coll,
                                            part_jmasks[r],
                                        );
                                        acc.step(&part_args[r], &cctx)?;
                                        let h = acc.heap_bytes();
                                        if h > peak_acc_heap {
                                            win_mem.reserve(h - peak_acc_heap)?;
                                            peak_acc_heap = h;
                                        }
                                    }
                                } else {
                                    // Evict leaving rows BEFORE stepping entering rows so
                                    // sliding aggregates (e.g. `sum()`, `json_group_object()`)
                                    // never observe transient duplicates/overflows or evict
                                    // keys that were just overwritten by entering rows.
                                    let evict_end = cur_end.min(new_start);
                                    for r in cur_start..evict_end {
                                        self.charge_steps(1)?;
                                        if !part_filter[r] {
                                            continue;
                                        }
                                        let cctx = env.fn_ctx.with_collation_and_json_mask(
                                            agg_coll,
                                            part_jmasks[r],
                                        );
                                        acc.inverse(&part_args[r], &cctx)?;
                                    }
                                    let enter_start = cur_end.max(new_start);
                                    for r in enter_start..new_end {
                                        self.charge_steps(1)?;
                                        if !part_filter[r] {
                                            continue;
                                        }
                                        let cctx = env.fn_ctx.with_collation_and_json_mask(
                                            agg_coll,
                                            part_jmasks[r],
                                        );
                                        acc.step(&part_args[r], &cctx)?;
                                        let h = acc.heap_bytes();
                                        if h > peak_acc_heap {
                                            win_mem.reserve(h - peak_acc_heap)?;
                                            peak_acc_heap = h;
                                        }
                                    }
                                }
                                cur_start = new_start;
                                cur_end = new_end;
                            } else {
                                acc = self.options.functions.create_aggregate(
                                    &wcall.name.name,
                                    wcall.args.len(),
                                    wcall.star,
                                )?;
                                let mut acc_mem = self.make_mem_guard();
                                let mut peak_acc_heap = acc.heap_bytes();
                                for r in new_start..new_end {
                                    self.charge_steps(1)?;
                                    if !part_filter[r] {
                                        continue;
                                    }
                                    let cctx = env
                                        .fn_ctx
                                        .with_collation_and_json_mask(agg_coll, part_jmasks[r]);
                                    acc.step(&part_args[r], &cctx)?;
                                    let h = acc.heap_bytes();
                                    if h > peak_acc_heap {
                                        acc_mem.reserve(h - peak_acc_heap)?;
                                        peak_acc_heap = h;
                                    }
                                }
                                cur_start = new_start;
                                cur_end = new_end;
                            }
                            let v = acc.value(&fctx)?;
                            win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                            vals.push(v);
                        }
                    } else {
                        for i in 0..plen {
                            self.charge_steps(1)?;
                            let frows = compute_frame_rows(
                                &resolved_frame,
                                i,
                                plen,
                                &peers,
                                &first_order_vals,
                                first_ord_dir,
                                first_ord_nulls,
                                first_ord_coll,
                            )?;
                            let mut acc = self.options.functions.create_aggregate(
                                &wcall.name.name,
                                wcall.args.len(),
                                wcall.star,
                            )?;
                            let mut acc_mem = self.make_mem_guard();
                            let mut peak_acc_heap = acc.heap_bytes();
                            for r in frows {
                                self.charge_steps(1)?;
                                if !part_filter[r] {
                                    continue;
                                }
                                let cctx = env
                                    .fn_ctx
                                    .with_collation_and_json_mask(agg_coll, part_jmasks[r]);
                                acc.step(&part_args[r], &cctx)?;
                                let h = acc.heap_bytes();
                                if h > peak_acc_heap {
                                    acc_mem.reserve(h - peak_acc_heap)?;
                                    peak_acc_heap = h;
                                }
                            }
                            let fctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
                            let v = acc.finalize(&fctx)?;
                            win_mem.reserve(estimate_value_bytes(&v).saturating_add(16))?;
                            vals.push(v);
                        }
                    }
                    vals
                } else {
                    return Err(SqlError::runtime(format!(
                        "{}() may not be used as a window function",
                        wcall.name.name
                    )));
                };

                for (ppos, val) in results.into_iter().enumerate() {
                    let ridx = part[ppos];
                    if is_win_only {
                        win_mem.reserve(estimate_value_bytes(&val).saturating_add(16))?;
                    }
                    if let Some(ref mut cb) = col_buf {
                        cb[ridx] = val;
                    } else if let WindowSink::PerRow(precomputed) = sink {
                        precomputed[ridx].push(
                            wcall.span.start,
                            wcall.span.end,
                            val,
                            is_json_win,
                        );
                    }
                }
            }
            if let Some(vals) = col_buf {
                if let WindowSink::Columnar(cols) = sink {
                    cols.push(WindowCol {
                        span_start: wcall.span.start,
                        span_end: wcall.span.end,
                        values: vals,
                    });
                }
            }
        }

        Ok(())
    }

    // ───────────────────────────── ORDER BY / LIMIT ─────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn apply_order_and_limit(
        &self,
        out_cols: &[ScopeColumn],
        mut rows: Vec<Vec<SqlValue>>,
        order_ctx: Option<&OrderEvalContext>,
        compound_col_aliases: Option<&[Vec<String>]>,
        order_by: Option<&[OrderingTerm]>,
        order_by_all: Option<&OrderByAll>,
        limit: Option<&LimitClause>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        let env = self.make_env();

        // Pre-validate ORDER BY terms & collations before checking LIMIT (SQLite validates
        // ORDER BY at compile time even when LIMIT 0 or rows is empty).
        if let Some(oba) = order_by_all {
            let dir = oba.order.unwrap_or(SortOrder::Asc);
            let nulls = oba.nulls;
            let colls: Vec<&Collation> = out_cols
                .iter()
                .map(|c| env.lookup_collation(&c.collation))
                .collect::<SqlResult<Vec<_>>>()?;
            if rows.len() > 1 {
                let n = rows.len() as u64;
                let log_n = (usize::BITS - rows.len().leading_zeros()) as u64;
                self.charge_steps((n.saturating_mul(log_n) / 16).max(1))?;
                rows.sort_by(|a, b| {
                    for i in 0..out_cols.len() {
                        let ord = compare_ordered_val(&a[i], &b[i], dir, nulls, colls[i]);
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    Ordering::Equal
                });
            }
        } else if let Some(terms) = order_by {
            if !terms.is_empty() {
                enum OrderTermPlan {
                    OutCol(usize),
                    FromCol(usize),
                    Expr { needs_aliases: bool },
                }

                let mut term_colls: Vec<&Collation> = Vec::with_capacity(terms.len());
                let mut term_plans: Vec<OrderTermPlan> = Vec::with_capacity(terms.len());
                let sample_aliases: Vec<(String, SqlValue, Affinity, String)> = out_cols
                    .iter()
                    .map(|c| (c.name.clone(), SqlValue::Null, c.affinity, c.collation.clone()))
                    .collect();

                // Evaluate LIMIT/OFFSET early so we know whether the sort will be skipped
                // (we still validate ORDER BY terms & collations first!).
                let lim_eval = if let Some(lim) = limit {
                    let count = eval_limit_int(&lim.count, outer_scopes, &env)?;
                    let offset = match &lim.offset {
                        Some(off_expr) => {
                            let o = eval_limit_int(off_expr, outer_scopes, &env)?.max(0);
                            usize::try_from(o).unwrap_or(usize::MAX)
                        }
                        None => 0,
                    };
                    Some((count, offset))
                } else {
                    None
                };
                let will_skip_sort = matches!(lim_eval, Some((count, offset)) if count == 0 || offset >= rows.len());

                for (tidx, term) in terms.iter().enumerate() {
                    // Validate every collation in stacked `COLLATE` chains (even on ordinals/aliases).
                    let mut coll_walk = &term.expr;
                    while let Expr::Collate {
                        operand, collation, ..
                    } = coll_walk
                    {
                        let _ = env.lookup_collation(&collation.name)?;
                        coll_walk = operand;
                    }
                    if let Some((ord, ov_coll)) = integer_ordinal_with_collation(&term.expr) {
                        if ord < 1 || ord > out_cols.len() {
                            return Err(SqlError::runtime(format!(
                                "{} ORDER BY term out of range - should be between 1 and {}",
                                ordinal_label(tidx + 1),
                                out_cols.len()
                            )));
                        }
                        let cname = ov_coll.unwrap_or(&out_cols[ord - 1].collation);
                        term_colls.push(env.lookup_collation(cname)?);
                        term_plans.push(OrderTermPlan::OutCol(ord - 1));
                    } else if let Some(c_aliases) = compound_col_aliases {
                        // SQLite compound SELECT `ORDER BY`: each non-ordinal term MUST match
                        // an output column name (from any branch of the compound) with optional
                        // `COLLATE`; arbitrary expressions like `ORDER BY x + 1` are rejected.
                        let (col_name, ov_coll) = if let Expr::Column(c) = coll_walk {
                            let ov = if let Expr::Collate { collation, .. } = &term.expr {
                                Some(collation.name.as_str())
                            } else {
                                None
                            };
                            (c.column.name.as_str(), ov)
                        } else {
                            return Err(SqlError::runtime(format!(
                                "{} ORDER BY term does not match any column in the result set",
                                ordinal_label(tidx + 1)
                            )));
                        };
                        let pos = out_cols
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                            .or_else(|| {
                                c_aliases.iter().position(|names| {
                                    names.iter().any(|n| n.eq_ignore_ascii_case(col_name))
                                })
                            });
                        let Some(pos) = pos else {
                            return Err(SqlError::runtime(format!(
                                "{} ORDER BY term does not match any column in the result set",
                                ordinal_label(tidx + 1)
                            )));
                        };
                        let cname = ov_coll.unwrap_or(&out_cols[pos].collation);
                        term_colls.push(env.lookup_collation(cname)?);
                        term_plans.push(OrderTermPlan::OutCol(pos));
                    } else {
                        let mut cname_opt = None;
                        let mut plan: Option<OrderTermPlan> = None;

                        if let Some((col_name, ov_coll)) = bare_unqualified_col_and_coll(&term.expr)
                        {
                            if let Some(ctx) = order_ctx {
                                if let Some(pos) = out_cols.iter().enumerate().position(|(i, c)| {
                                    ctx.explicit_aliases.get(i).copied().unwrap_or(false)
                                        && c.name.eq_ignore_ascii_case(col_name)
                                }) {
                                    cname_opt = Some(
                                        ov_coll
                                            .map(str::to_string)
                                            .unwrap_or_else(|| out_cols[pos].collation.clone()),
                                    );
                                    plan = Some(OrderTermPlan::OutCol(pos));
                                }
                            } else if let Some(pos) = out_cols
                                .iter()
                                .position(|c| c.name.eq_ignore_ascii_case(col_name))
                            {
                                cname_opt = Some(
                                    ov_coll
                                        .map(str::to_string)
                                        .unwrap_or_else(|| out_cols[pos].collation.clone()),
                                );
                                plan = Some(OrderTermPlan::OutCol(pos));
                            }
                        }

                        if cname_opt.is_none() {
                            if let Some(ctx) = order_ctx {
                                let mut scopes = outer_scopes.to_vec();
                                scopes.push(EvalScope {
                                    columns: &ctx.from_columns,
                                    values: &[],
                                    precomputed: None,
                                    precomputed_json: None,
                                    hidden_unqualified: Some(&ctx.from_using_hidden),
                                    source_ids: Some(&ctx.from_source_ids),
                                    aliases: Some(&sample_aliases),
                                    no_rowid_aliases: false,
                                });
                                cname_opt = expr_collation(&term.expr, &scopes).name;
                            }
                        }
                        if cname_opt.is_none() {
                            let mut scopes = outer_scopes.to_vec();
                            scopes.push(EvalScope::simple(out_cols, &[]));
                            cname_opt = expr_collation(&term.expr, &scopes).name;
                        }
                        let cname = cname_opt.unwrap_or_else(|| "BINARY".to_string());
                        term_colls.push(env.lookup_collation(&cname)?);

                        // Fast-path resolution for direct column references in ORDER BY.
                        if plan.is_none() {
                            if let (Expr::Column(c), Some(ctx)) = (coll_walk, order_ctx) {
                                if let Some(fidx) = resolve_direct_from_col(
                                    c,
                                    &ctx.from_columns,
                                    &ctx.from_using_hidden,
                                    &ctx.from_source_ids,
                                ) {
                                    plan = Some(OrderTermPlan::FromCol(fidx));
                                }
                            }
                        }
                        let resolved_plan = plan.unwrap_or_else(|| {
                            let mut names = HashSet::new();
                            collect_unqualified_col_names(&term.expr, &mut names);
                            let needs_aliases = !names.is_empty()
                                && out_cols
                                    .iter()
                                    .any(|c| names.contains(&c.name.to_ascii_lowercase()));
                            OrderTermPlan::Expr { needs_aliases }
                        });
                        term_plans.push(resolved_plan);

                        // Validate column resolution on 0/1-row results AND when LIMIT 0 /
                        // large OFFSET will skip evaluating ORDER BY expressions over rows!
                        if rows.len() == 1 && !will_skip_sort {
                            let _ = self.eval_order_term(
                                &term.expr,
                                out_cols,
                                &rows[0],
                                order_ctx,
                                0,
                                outer_scopes,
                                &env,
                                true,
                            )?;
                        } else if rows.is_empty() || will_skip_sort {
                            let dummy_row = vec![SqlValue::Null; out_cols.len()];
                            self.eval_order_term_validate(
                                &term.expr,
                                out_cols,
                                &dummy_row,
                                order_ctx,
                                outer_scopes,
                                &env,
                            )?;
                        }
                    }
                }

                if will_skip_sort {
                    rows.clear();
                    return Ok(rows);
                }

                let top_k: Option<usize> = lim_eval.and_then(|(count, offset)| {
                    if count > 0 {
                        let k = offset.saturating_add(usize::try_from(count).unwrap_or(usize::MAX));
                        if k < rows.len() {
                            Some(k)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                if rows.len() > 1 {
                    let mut mem = self.make_mem_guard();
                    if terms.len() == 1 {
                        // Fast single-key ORDER BY path: no inner `Vec<SqlValue>` per row.
                        let term = &terms[0];
                        let tdir = term.order.unwrap_or(SortOrder::Asc);
                        let tnulls = term.nulls;
                        let tcoll = term_colls[0];
                        let mut key_rows: Vec<(usize, SqlValue)> = Vec::with_capacity(rows.len());
                        let has_meter = self.options.meter.is_some();
                        match &term_plans[0] {
                            OrderTermPlan::OutCol(pos) => {
                                for (ridx, row) in rows.iter().enumerate() {
                                    self.charge_steps(1)?;
                                    let val = row[*pos].clone();
                                    if has_meter {
                                        mem.reserve(estimate_value_bytes(&val).saturating_add(16))?;
                                    }
                                    key_rows.push((ridx, val));
                                }
                            }
                            OrderTermPlan::FromCol(fidx) => {
                                let ctx = order_ctx.expect("FromCol requires order_ctx");
                                for ridx in 0..rows.len() {
                                    self.charge_steps(1)?;
                                    let val = ctx.from_rows[ridx][*fidx].clone();
                                    if has_meter {
                                        mem.reserve(estimate_value_bytes(&val).saturating_add(16))?;
                                    }
                                    key_rows.push((ridx, val));
                                }
                            }
                            OrderTermPlan::Expr { needs_aliases } => {
                                for (ridx, row) in rows.iter().enumerate() {
                                    self.charge_steps(1)?;
                                    let val = self.eval_order_term(
                                        &term.expr,
                                        out_cols,
                                        row,
                                        order_ctx,
                                        ridx,
                                        outer_scopes,
                                        &env,
                                        *needs_aliases,
                                    )?;
                                    if has_meter {
                                        mem.reserve(estimate_value_bytes(&val).saturating_add(16))?;
                                    }
                                    key_rows.push((ridx, val));
                                }
                            }
                        }
                        let cmp = |(ra, ka): &(usize, SqlValue), (rb, kb): &(usize, SqlValue)| {
                            compare_ordered_val(ka, kb, tdir, tnulls, tcoll)
                                .then_with(|| ra.cmp(rb))
                        };
                        if self.options.meter.is_some() && key_rows.len() > 1 {
                            let n = key_rows.len() as u64;
                            let lg = (u64::BITS - n.leading_zeros()) as u64;
                            self.charge_steps((n.saturating_mul(lg) / 4).max(1))?;
                        }
                        if let Some(k) = top_k {
                            if k > 0 && k < key_rows.len() / 2 {
                                key_rows.select_nth_unstable_by(k - 1, cmp);
                                key_rows.truncate(k);
                                key_rows.sort_by(cmp);
                            } else {
                                key_rows.sort_by(cmp);
                                key_rows.truncate(k);
                            }
                        } else {
                            key_rows.sort_by(cmp);
                        }
                        let mut sorted = Vec::with_capacity(key_rows.len());
                        let mut slots: Vec<Option<Vec<SqlValue>>> =
                            rows.into_iter().map(Some).collect();
                        for (ridx, _) in key_rows {
                            if let Some(r) = slots[ridx].take() {
                                sorted.push(r);
                            }
                        }
                        rows = sorted;
                    } else {
                        let mut key_rows: Vec<(usize, Vec<SqlValue>)> =
                            Vec::with_capacity(rows.len());
                        for (ridx, row) in rows.iter().enumerate() {
                            self.charge_steps(1)?;
                            let mut keys = Vec::with_capacity(terms.len());
                            for (tidx, term) in terms.iter().enumerate() {
                                let val = match &term_plans[tidx] {
                                    OrderTermPlan::OutCol(pos) => row[*pos].clone(),
                                    OrderTermPlan::FromCol(fidx) => {
                                        order_ctx.expect("FromCol requires order_ctx").from_rows
                                            [ridx][*fidx]
                                            .clone()
                                    }
                                    OrderTermPlan::Expr { needs_aliases } => self.eval_order_term(
                                        &term.expr,
                                        out_cols,
                                        row,
                                        order_ctx,
                                        ridx,
                                        outer_scopes,
                                        &env,
                                        *needs_aliases,
                                    )?,
                                };
                                keys.push(val);
                            }
                            mem.reserve(estimate_row_bytes(&keys) + 16)?;
                            key_rows.push((ridx, keys));
                        }

                        let cmp = |(ra, ka): &(usize, Vec<SqlValue>),
                                   (rb, kb): &(usize, Vec<SqlValue>)| {
                            compare_sort_keys(ka, kb, terms, &term_colls).then_with(|| ra.cmp(rb))
                        };
                        if self.options.meter.is_some() && key_rows.len() > 1 {
                            let n = key_rows.len() as u64;
                            let lg = (u64::BITS - n.leading_zeros()) as u64;
                            self.charge_steps((n.saturating_mul(lg) / 4).max(1))?;
                        }
                        if let Some(k) = top_k {
                            if k > 0 && k < key_rows.len() / 2 {
                                key_rows.select_nth_unstable_by(k - 1, cmp);
                                key_rows.truncate(k);
                                key_rows.sort_by(cmp);
                            } else {
                                key_rows.sort_by(cmp);
                                key_rows.truncate(k);
                            }
                        } else {
                            key_rows.sort_by(cmp);
                        }

                        let mut sorted = Vec::with_capacity(key_rows.len());
                        let mut slots: Vec<Option<Vec<SqlValue>>> =
                            rows.into_iter().map(Some).collect();
                        for (ridx, _) in key_rows {
                            if let Some(r) = slots[ridx].take() {
                                sorted.push(r);
                            }
                        }
                        rows = sorted;
                    }
                }

                // Apply OFFSET/LIMIT slice (we already truncated to `offset + count` if `top_k` ran).
                if let Some((count, offset)) = lim_eval {
                    if offset >= rows.len() {
                        rows.clear();
                    } else {
                        if offset > 0 {
                            rows.drain(..offset);
                        }
                        if count >= 0 {
                            let lim_n = usize::try_from(count).unwrap_or(usize::MAX);
                            if lim_n < rows.len() {
                                rows.truncate(lim_n);
                            }
                        }
                    }
                }
                return Ok(rows);
            }
        }

        if let Some(lim) = limit {
            let count = eval_limit_int(&lim.count, outer_scopes, &env)?;
            let offset = match &lim.offset {
                Some(off_expr) => {
                    let o = eval_limit_int(off_expr, outer_scopes, &env)?.max(0);
                    usize::try_from(o).unwrap_or(usize::MAX)
                }
                None => 0,
            };
            if offset >= rows.len() {
                rows.clear();
            } else {
                if offset > 0 {
                    rows.drain(..offset);
                }
                if count >= 0 {
                    let lim_n = usize::try_from(count).unwrap_or(usize::MAX);
                    if lim_n < rows.len() {
                        rows.truncate(lim_n);
                    }
                }
            }
        }

        Ok(rows)
    }

    fn eval_order_term_validate(
        &self,
        expr: &Expr,
        out_cols: &[ScopeColumn],
        dummy_out_row: &[SqlValue],
        order_ctx: Option<&OrderEvalContext>,
        outer_scopes: &[EvalScope<'_>],
        _env: &EvalEnv<'_>,
    ) -> SqlResult<()> {
        // Validate ALL column references inside `expr` on 0-row results without evaluating runtime errors.
        let aliases: Vec<(String, SqlValue, Affinity, String)> = out_cols
            .iter()
            .zip(dummy_out_row.iter())
            .map(|(c, v)| (c.name.clone(), v.clone(), c.affinity, c.collation.clone()))
            .collect();
        if let Some(ctx) = order_ctx {
            let dummy_from = vec![SqlValue::Null; ctx.from_columns.len()];
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope {
                columns: &ctx.from_columns,
                values: &dummy_from,
                precomputed: None,
                precomputed_json: None,
                hidden_unqualified: Some(&ctx.from_using_hidden),
                source_ids: Some(&ctx.from_source_ids),
                aliases: Some(&aliases),
                no_rowid_aliases: false,
            });
            if self.validate_expr_and_subqueries(expr, &scopes).is_ok() {
                return Ok(());
            }
        }
        let mut scopes = outer_scopes.to_vec();
        scopes.push(EvalScope::simple(out_cols, dummy_out_row));
        self.validate_expr_and_subqueries(expr, &scopes)
    }

    fn validate_expr_and_subqueries(
        &self,
        e: &Expr,
        scopes: &[EvalScope<'_>],
    ) -> SqlResult<()> {
        validate_expr_columns(e, scopes, self.options.dqs)?;
        self.validate_expr_subqueries_depth(e, scopes, 0)
    }

    fn validate_expr_subqueries_depth(
        &self,
        e: &Expr,
        scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<()> {
        if depth >= MAX_QUERY_DEPTH {
            return Ok(());
        }
        match e {
            Expr::InSelect {
                operand, select, ..
            } => {
                let expected = match operand.as_ref() {
                    Expr::Vector { items, .. } => {
                        for it in items {
                            self.validate_expr_subqueries_depth(it, scopes, depth)?;
                        }
                        Some(items.len())
                    }
                    Expr::Subquery { select: lsel, .. } => {
                        self.validate_subquery_schema(lsel, scopes, depth + 1)?
                    }
                    _ => {
                        self.validate_expr_subqueries_depth(operand, scopes, depth)?;
                        Some(1)
                    }
                };
                if let (Some(exp), Some(got)) = (
                    expected,
                    self.validate_subquery_schema(select, scopes, depth + 1)?,
                ) {
                    if got != exp {
                        return Err(SqlError::runtime(format!(
                            "sub-select returns {got} columns - expected {exp}"
                        )));
                    }
                }
                Ok(())
            }
            Expr::Subquery { select, .. } => {
                if let Some(got) = self.validate_subquery_schema(select, scopes, depth + 1)? {
                    if got != 1 {
                        return Err(SqlError::runtime(format!(
                            "sub-select returns {got} columns - expected 1"
                        )));
                    }
                }
                Ok(())
            }
            Expr::Exists { select, .. } => {
                let _ = self.validate_subquery_schema(select, scopes, depth + 1)?;
                Ok(())
            }
            Expr::Unary { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. } => {
                self.validate_expr_subqueries_depth(operand, scopes, depth)
            }
            Expr::Binary {
                op, left, right, ..
            } => {
                let is_cmp = matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::Ne
                        | BinaryOp::Lt
                        | BinaryOp::Le
                        | BinaryOp::Gt
                        | BinaryOp::Ge
                        | BinaryOp::Is
                        | BinaryOp::IsNot
                );
                if is_cmp
                    && (matches!(left.as_ref(), Expr::Subquery { .. } | Expr::Vector { .. })
                        || matches!(right.as_ref(), Expr::Subquery { .. } | Expr::Vector { .. }))
                {
                    let lw = self.validate_row_value_width(left, scopes, depth)?;
                    let rw = self.validate_row_value_width(right, scopes, depth)?;
                    if let (Some(l), Some(r)) = (lw, rw) {
                        if l != r {
                            return Err(SqlError::runtime("row value misused"));
                        }
                    }
                    return Ok(());
                }
                self.validate_expr_subqueries_depth(left, scopes, depth)?;
                self.validate_expr_subqueries_depth(right, scopes, depth)
            }
            Expr::Like {
                left,
                right,
                escape,
                ..
            } => {
                self.validate_expr_subqueries_depth(left, scopes, depth)?;
                self.validate_expr_subqueries_depth(right, scopes, depth)?;
                if let Some(esc) = escape {
                    self.validate_expr_subqueries_depth(esc, scopes, depth)?;
                }
                Ok(())
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                if matches!(
                    operand.as_ref(),
                    Expr::Subquery { .. } | Expr::Vector { .. }
                ) || matches!(low.as_ref(), Expr::Subquery { .. } | Expr::Vector { .. })
                    || matches!(high.as_ref(), Expr::Subquery { .. } | Expr::Vector { .. })
                {
                    let ow = self.validate_row_value_width(operand, scopes, depth)?;
                    let lw = self.validate_row_value_width(low, scopes, depth)?;
                    let hw = self.validate_row_value_width(high, scopes, depth)?;
                    if let (Some(o), Some(l)) = (ow, lw) {
                        if o != l {
                            return Err(SqlError::runtime("row value misused"));
                        }
                    }
                    if let (Some(o), Some(h)) = (ow, hw) {
                        if o != h {
                            return Err(SqlError::runtime("row value misused"));
                        }
                    }
                    return Ok(());
                }
                self.validate_expr_subqueries_depth(operand, scopes, depth)?;
                self.validate_expr_subqueries_depth(low, scopes, depth)?;
                self.validate_expr_subqueries_depth(high, scopes, depth)
            }
            Expr::InList { operand, items, .. } => {
                if matches!(
                    operand.as_ref(),
                    Expr::Subquery { .. } | Expr::Vector { .. }
                ) || items
                    .iter()
                    .any(|i| matches!(i, Expr::Subquery { .. } | Expr::Vector { .. }))
                {
                    let ow = self.validate_row_value_width(operand, scopes, depth)?;
                    for it in items {
                        let iw = self.validate_row_value_width(it, scopes, depth)?;
                        if let (Some(o), Some(got)) = (ow, iw) {
                            if o != got {
                                if o == 1 {
                                    return Err(SqlError::runtime("row value misused"));
                                }
                                let noun = if got == 1 { "term" } else { "terms" };
                                return Err(SqlError::runtime(format!(
                                    "IN(...) element has {got} {noun} - expected {o}"
                                )));
                            }
                        }
                    }
                    return Ok(());
                }
                self.validate_expr_subqueries_depth(operand, scopes, depth)?;
                for i in items {
                    self.validate_expr_subqueries_depth(i, scopes, depth)?;
                }
                Ok(())
            }
            Expr::InTable { operand, args, .. } => {
                self.validate_expr_subqueries_depth(operand, scopes, depth)?;
                if let Some(list) = args {
                    for a in list {
                        self.validate_expr_subqueries_depth(a, scopes, depth)?;
                    }
                }
                Ok(())
            }
            Expr::Case {
                operand,
                whens,
                else_expr,
                ..
            } => {
                if let Some(o) = operand {
                    let has_vec_or_subq =
                        matches!(o.as_ref(), Expr::Subquery { .. } | Expr::Vector { .. })
                            || whens.iter().any(|w| {
                                matches!(w.when, Expr::Subquery { .. } | Expr::Vector { .. })
                            });
                    if has_vec_or_subq {
                        let ow = self.validate_row_value_width(o, scopes, depth)?;
                        for w in whens {
                            let ww = self.validate_row_value_width(&w.when, scopes, depth)?;
                            if let (Some(a), Some(b)) = (ow, ww) {
                                if a != b {
                                    return Err(SqlError::runtime("row value misused"));
                                }
                            }
                            self.validate_expr_subqueries_depth(&w.then, scopes, depth)?;
                        }
                        if let Some(el) = else_expr {
                            self.validate_expr_subqueries_depth(el, scopes, depth)?;
                        }
                        return Ok(());
                    }
                    self.validate_expr_subqueries_depth(o, scopes, depth)?;
                }
                for w in whens {
                    self.validate_expr_subqueries_depth(&w.when, scopes, depth)?;
                    self.validate_expr_subqueries_depth(&w.then, scopes, depth)?;
                }
                if let Some(el) = else_expr {
                    self.validate_expr_subqueries_depth(el, scopes, depth)?;
                }
                Ok(())
            }
            Expr::Function(f) => {
                for a in &f.args {
                    self.validate_expr_subqueries_depth(a, scopes, depth)?;
                }
                if let Some(ref flt) = f.filter {
                    self.validate_expr_subqueries_depth(flt, scopes, depth)?;
                }
                if let Some(ref ob) = f.order_by {
                    for o in ob {
                        self.validate_expr_subqueries_depth(&o.expr, scopes, depth)?;
                    }
                }
                if let Some(WindowRef::Spec(ref ws)) = f.over {
                    if let Some(ref pb) = ws.partition_by {
                        for pe in pb {
                            self.validate_expr_subqueries_depth(pe, scopes, depth)?;
                        }
                    }
                    if let Some(ref ob) = ws.order_by {
                        for o in ob {
                            self.validate_expr_subqueries_depth(&o.expr, scopes, depth)?;
                        }
                    }
                }
                Ok(())
            }
            Expr::Vector { items, .. } => {
                if items.len() > 1 {
                    return Err(SqlError::runtime("row value misused"));
                }
                for i in items {
                    self.validate_expr_subqueries_depth(i, scopes, depth)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn validate_row_value_width(
        &self,
        e: &Expr,
        scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<Option<usize>> {
        match e {
            Expr::Vector { items, .. } => {
                for it in items {
                    self.validate_expr_subqueries_depth(it, scopes, depth)?;
                }
                Ok(Some(items.len()))
            }
            Expr::Subquery { select, .. } => {
                self.validate_subquery_schema(select, scopes, depth + 1)
            }
            _ => {
                self.validate_expr_subqueries_depth(e, scopes, depth)?;
                Ok(Some(1))
            }
        }
    }

    fn build_from_schema_only(&self, from: &FromSource) -> SqlResult<Option<Relation>> {
        match from {
            FromSource::Table(ft) => {
                let schema = ft.schema.as_ref().map(|s| s.name.as_str());
                let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
                if schema.is_none() {
                    if let Some(cte) = self.ctes.get(&ft.name.name.to_ascii_lowercase()) {
                        let cols: Vec<ScopeColumn> = cte
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
                        let n = cols.len();
                        return Ok(Some(Relation {
                            columns: cols,
                            star_hidden: vec![false; n],
                            using_hidden: vec![false; n],
                            source_ids: vec![0; n],
                            rows: Vec::new(),
                        }));
                    }
                }
                if let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? {
                    let mut rel = Self::build_table_rel_meta(&tmeta, alias);
                    if tmeta.has_rowid {
                        for rname in ["rowid", "_rowid_", "oid"] {
                            if !rel
                                .columns
                                .iter()
                                .any(|c| c.name.eq_ignore_ascii_case(rname))
                            {
                                rel.columns.push(ScopeColumn {
                                    table: Some(alias.to_string()),
                                    name: rname.to_string(),
                                    affinity: Affinity::Integer,
                                    collation: "BINARY".to_string(),
                                    is_rowid_alias: true,
                                });
                                rel.star_hidden.push(true);
                                rel.using_hidden.push(false);
                                rel.source_ids.push(0);
                            }
                        }
                    }
                    return Ok(Some(rel));
                }
                if self.catalog.get_view(schema, &ft.name.name)?.is_some() {
                    return Ok(None);
                }
                if self
                    .catalog
                    .call_tvf(schema, &ft.name.name, &[], self.options.meter.as_deref())?
                    .is_some()
                {
                    return Ok(None);
                }
                let full = match schema {
                    Some(s) => format!("{s}.{}", ft.name.name),
                    None => ft.name.name.clone(),
                };
                Err(SqlError::runtime(format!("no such table: {full}")))
            }
            _ => Ok(None),
        }
    }

    fn validate_subquery_schema(
        &self,
        sel: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<Option<usize>> {
        if depth >= MAX_QUERY_DEPTH || sel.with.is_some() {
            return Ok(None);
        }
        self.validate_subquery_body(&sel.body, outer_scopes, depth)
    }

    fn validate_subquery_body(
        &self,
        body: &SelectBody,
        outer_scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<Option<usize>> {
        if depth >= MAX_QUERY_DEPTH {
            return Ok(None);
        }
        match body {
            SelectBody::Values(v) => {
                for row in &v.rows {
                    for item in &row.items {
                        validate_expr_columns(
                            item,
                            outer_scopes,
                            self.options.dqs,
                        )?;
                        self.validate_expr_subqueries_depth(item, outer_scopes, depth)?;
                    }
                }
                Ok(v.rows.first().map(|r| r.items.len()))
            }
            SelectBody::Compound {
                op, left, right, ..
            } => {
                let lc = self.validate_subquery_body(left, outer_scopes, depth + 1)?;
                let rc = self.validate_subquery_body(right, outer_scopes, depth + 1)?;
                if let (Some(l), Some(r)) = (lc, rc) {
                    if l != r {
                        return Err(SqlError::runtime(format!(
                            "SELECTs to the left and right of {} do not have the same number of result columns",
                            op.as_str()
                        )));
                    }
                }
                Ok(lc.or(rc))
            }
            SelectBody::Core(core) => {
                let rel_opt = match &core.from {
                    None => Some(Relation::single_empty_row()),
                    Some(from_src) => self.build_from_schema_only(from_src)?,
                };
                if let Some(rel) = rel_opt {
                    let expanded = expand_result_columns(core, &rel)?;
                    let mut scopes = outer_scopes.to_vec();
                    let mut sc = EvalScope::simple(&rel.columns, &[]);
                    sc.hidden_unqualified = Some(&rel.using_hidden);
                    sc.source_ids = Some(&rel.source_ids);
                    scopes.push(sc);
                    if let Some(ref we) = core.where_clause {
                        self.validate_expr_and_subqueries(we, &scopes)?;
                    }
                    for p in &expanded {
                        if p.direct_col.is_none() {
                            self.validate_expr_and_subqueries(&p.expr, &scopes)?;
                        }
                    }
                    Ok(Some(expanded.len()))
                } else if !core.columns.is_empty()
                    && core
                        .columns
                        .iter()
                        .all(|c| matches!(c, ResultColumn::Expr(_)))
                {
                    Ok(Some(core.columns.len()))
                } else {
                    Ok(None)
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_order_term(
        &self,
        expr: &Expr,
        out_cols: &[ScopeColumn],
        out_row: &[SqlValue],
        order_ctx: Option<&OrderEvalContext>,
        ridx: usize,
        outer_scopes: &[EvalScope<'_>],
        env: &EvalEnv<'_>,
        needs_aliases: bool,
    ) -> SqlResult<SqlValue> {
        // SQLite ORDER BY resolution precedence (`resolve.c:resolveAsName`):
        // - For a bare unqualified column name (`ORDER BY b` or `ORDER BY b COLLATE ...`),
        //   an explicit `AS b` alias in the SELECT list wins first (left-to-right, no ambiguity).
        // - Otherwise check FROM columns (with SELECT aliases as fallback).
        // - Finally check `out_cols` directly (for compound SELECTs).
        if let Some((col_name, _)) = bare_unqualified_col_and_coll(expr) {
            if let Some(ctx) = order_ctx {
                if let Some(pos) = out_cols.iter().enumerate().position(|(i, c)| {
                    ctx.explicit_aliases.get(i).copied().unwrap_or(false)
                        && c.name.eq_ignore_ascii_case(col_name)
                }) {
                    return Ok(out_row[pos].clone());
                }
            } else if let Some(pos) = out_cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
            {
                return Ok(out_row[pos].clone());
            }
        }

        if let Some(ctx) = order_ctx {
            let aliases: Vec<(String, SqlValue, Affinity, String)> = if needs_aliases {
                out_cols
                    .iter()
                    .zip(out_row.iter())
                    .map(|(c, v)| (c.name.clone(), v.clone(), c.affinity, c.collation.clone()))
                    .collect()
            } else {
                Vec::new()
            };
            let pre_slots = ctx.precomputed.get(ridx).map(|p| p.slots.as_slice());
            let pre_json = ctx.precomputed.get(ridx).map(|p| p.is_json.as_slice());
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope {
                columns: &ctx.from_columns,
                values: &ctx.from_rows[ridx],
                precomputed: pre_slots,
                precomputed_json: pre_json,
                hidden_unqualified: Some(&ctx.from_using_hidden),
                source_ids: Some(&ctx.from_source_ids),
                aliases: if needs_aliases { Some(&aliases) } else { None },
                no_rowid_aliases: false,
            });
            match eval_expr(expr, &scopes, env) {
                Ok(v) => return Ok(v),
                // Only fall back to output scope when a column name wasn't found in FROM;
                // never swallow real runtime errors (like BudgetMeter exhaustion or bad JSON)!
                Err(SqlError::Runtime(ref msg)) if msg.starts_with("no such column:") => {}
                Err(e) => return Err(e),
            }
        }

        // Output columns directly (for compound SELECT or alias-only expressions).
        let mut scopes = outer_scopes.to_vec();
        scopes.push(EvalScope::simple(out_cols, out_row));
        eval_expr(expr, &scopes, env)
    }
}

impl SubqueryRunner for QueryExec<'_> {
    fn run_select(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Vec<SqlValue>>)> {
        let (cols, rows) = self.execute_select_stmt(stmt, outer_scopes)?;
        Ok((cols.into_iter().map(|c| c.name).collect(), rows))
    }

    fn run_select_with_affinities(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Option<Affinity>>, Vec<Vec<SqlValue>>)> {
        let (names, affs, _, rows) = self.run_select_with_meta(stmt, outer_scopes)?;
        Ok((names, affs, rows))
    }

    fn run_select_with_meta(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectWithMeta> {
        let (cols, colls, rows) = self.execute_select_stmt_meta(stmt, outer_scopes)?;
        let names = cols.iter().map(|c| c.name.clone()).collect();
        let affs = cols
            .iter()
            .map(|c| {
                if c.affinity == Affinity::None {
                    None
                } else {
                    Some(c.affinity)
                }
            })
            .collect();
        Ok((names, affs, colls, rows))
    }

    fn is_select_uncorrelated(&self, stmt: &SelectStmt, outer_scopes: &[EvalScope<'_>]) -> bool {
        select_stmt_is_uncorrelated(stmt, outer_scopes, self.catalog, &self.ctes, self.options)
    }

    fn is_in_table_cacheable(&self) -> bool {
        true
    }

    fn is_in_table_target_cacheable(&self, schema: Option<&str>, table: &str) -> bool {
        if schema.is_none() && self.ctes.contains_key(&table.to_ascii_lowercase()) {
            return true;
        }
        if matches!(self.catalog.get_table_arc(schema, table), Ok(Some(_))) {
            return true;
        }
        if let Ok(Some(vmeta)) = self.catalog.get_view(schema, table) {
            return view_is_deterministic(&vmeta.select_sql, self.catalog, self.options, 0);
        }
        false
    }

    fn try_scan_1col_in_select(
        &self,
        stmt: &SelectStmt,
    ) -> SqlResult<Option<crate::eval::OneColInScan>> {
        if stmt.with.is_some()
            || stmt.order_by.is_some()
            || stmt.order_by_all.is_some()
            || stmt.limit.is_some()
        {
            return Ok(None);
        }
        let SelectBody::Core(core) = &stmt.body else {
            return Ok(None);
        };
        if core.where_clause.is_some()
            || core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.windows.is_some()
            || core.distinct.is_some()
            || core.from_first
            || core.columns.len() != 1
        {
            return Ok(None);
        }
        let Some(FromSource::Table(ft)) = &core.from else {
            return Ok(None);
        };
        if ft.indexed_by.is_some() {
            return Ok(None);
        }
        let schema = ft.schema.as_ref().map(|n| n.name.as_str());
        if (schema.is_none() && self.ctes.contains_key(&ft.name.name.to_ascii_lowercase()))
            || !is_valid_schema_name(schema)
        {
            return Ok(None);
        }
        let Some(tmeta) = self.catalog.get_table_arc(schema, &ft.name.name)? else {
            return Ok(None);
        };
        let ResultColumn::Expr(re) = &core.columns[0] else {
            return Ok(None);
        };
        let Expr::Column(c) = &re.expr else {
            return Ok(None);
        };
        if let Some(ref sc) = c.schema {
            if !is_valid_schema_name(Some(sc.name.as_str())) {
                return Ok(None);
            }
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();
        if let Some(ref tq) = c.table {
            if !tq.name.eq_ignore_ascii_case(alias) {
                return Ok(None);
            }
        }
        let Some(cid) = tmeta
            .columns
            .iter()
            .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
        else {
            return Ok(None);
        };
        let col = &tmeta.columns[cid];
        let aff = if col.affinity == Affinity::None {
            None
        } else {
            Some(col.affinity)
        };
        let coll = ExprCollation::implicit(col.collation.clone());
        let has_meter = self.options.meter.is_some();
        let mut mem = self.make_mem_guard();
        let mut out = Vec::with_capacity(64);
        let needed = [cid];
        self.catalog.visit_table(
            schema,
            &ft.name.name,
            &TableScanHint::FullScan,
            Some(&needed),
            self.options.meter.as_deref(),
            &mut |_rid, vals| {
                if has_meter {
                    self.charge_steps(1)?;
                    mem.reserve(estimate_value_bytes(&vals[cid]))?;
                }
                out.push(vals[cid].clone());
                Ok(true)
            },
        )?;
        Ok(Some((aff, coll, out)))
    }

    fn run_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        Ok(self.run_in_table_with_affinities(schema, table, args)?.1)
    }

    fn run_in_table_with_affinities(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<(Vec<Option<Affinity>>, Vec<Vec<SqlValue>>)> {
        let (affs, _, rows) = self.run_in_table_with_meta(schema, table, args)?;
        Ok((affs, rows))
    }

    fn run_in_table_with_meta(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<TableRowsWithMeta> {
        if let Some(arg_vals) = args {
            let env = self.make_env();
            if let Some(tvf) = self.catalog.call_tvf(
                schema,
                table,
                arg_vals,
                self.options.meter.as_deref(),
            )? {
                let n = tvf.columns.len();
                return Ok((vec![None; n], vec![ExprCollation::none(); n], tvf.rows));
            }
            if let Some(tvf) =
                call_builtin_tvf(&table.to_ascii_lowercase(), arg_vals, &env.fn_ctx)?
            {
                let n = tvf.columns.len();
                return Ok((vec![None; n], vec![ExprCollation::none(); n], tvf.rows));
            }
            return Err(SqlError::runtime(format!("no such table: {table}")));
        }
        if schema.is_none() {
            if let Some(cte) = self.ctes.get(&table.to_ascii_lowercase()) {
                let affs = cte
                    .columns
                    .iter()
                    .map(|c| {
                        if c.affinity == Affinity::None {
                            None
                        } else {
                            Some(c.affinity)
                        }
                    })
                    .collect();
                let colls = cte
                    .columns
                    .iter()
                    .map(|c| {
                        if c.collation.eq_ignore_ascii_case("BINARY") {
                            ExprCollation::none()
                        } else {
                            ExprCollation::implicit(c.collation.clone())
                        }
                    })
                    .collect();
                let rows = if let Some(m) = self.options.meter.as_deref() {
                    m.charge_steps((cte.rows.len() as u64).max(1))?;
                    let bytes: u64 = cte.rows.iter().map(|r| estimate_row_bytes(r)).sum();
                    m.reserve_memory(bytes)?;
                    let cloned = cte.rows.clone();
                    m.release_memory(bytes);
                    cloned
                } else {
                    cte.rows.clone()
                };
                return Ok((affs, colls, rows));
            }
        }
        let tmeta = self.catalog.get_table_arc(schema, table)?;
        if tmeta.is_none() {
            if let Some(vmeta) = self.catalog.get_view(schema, table)? {
                if self.view_depth >= 64 {
                    return Err(SqlError::runtime(format!(
                        "view {} is circularly defined",
                        vmeta.name
                    )));
                }
                let mut view_runner = self.child_with_ctes(HashMap::new());
                view_runner.view_depth = self.view_depth + 1;
                let parse_opts = ParseOptions {
                    dialect: self.options.dialect,
                    ..ParseOptions::default()
                };
                let stmt = match self.options.meter.as_deref() {
                    Some(m) => parse_statement_metered(&vmeta.select_sql, &parse_opts, m)?,
                    None => xdb_parser::parse_statement(&vmeta.select_sql, &parse_opts)?,
                };
                let view_select = match stmt {
                    Statement::Select(s) => s,
                    Statement::CreateView(cv) => cv.select,
                    _ => {
                        return Err(SqlError::runtime(format!(
                            "invalid view definition for {}",
                            vmeta.name
                        )));
                    }
                };
                let (vcols, vcolls, vrows) = view_runner.execute_select_stmt_meta(&view_select, &[])?;
                let affs = vcols
                    .iter()
                    .map(|c| {
                        if c.affinity == Affinity::None {
                            None
                        } else {
                            Some(c.affinity)
                        }
                    })
                    .collect();
                return Ok((affs, vcolls, vrows));
            }
        }
        let affs: Vec<Option<Affinity>> = tmeta
            .as_ref()
            .map(|m| m.columns.iter().map(|c| Some(c.affinity)).collect())
            .unwrap_or_default();
        let colls: Vec<ExprCollation> = tmeta
            .as_ref()
            .map(|m| {
                m.columns
                    .iter()
                    .map(|c| {
                        if c.collation.eq_ignore_ascii_case("BINARY") {
                            ExprCollation::none()
                        } else {
                            ExprCollation::implicit(c.collation.clone())
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let scanned = self.catalog.scan_table(
            schema,
            table,
            &TableScanHint::FullScan,
            None,
            self.options.meter.as_deref(),
        )?;
        Ok((affs, colls, scanned.into_iter().map(|r| r.values).collect()))
    }
}

// ───────────────────────────── helpers ─────────────────────────────

struct OrderEvalContext {
    from_columns: Vec<ScopeColumn>,
    from_using_hidden: Vec<bool>,
    from_source_ids: Vec<u32>,
    from_rows: Vec<Vec<SqlValue>>,
    precomputed: Vec<PrecomputedRow>,
    explicit_aliases: Vec<bool>,
}

struct ProjectedItem {
    expr: Expr,
    output_name: String,
    explicit_alias: Option<String>,
    /// When `Some(idx)`, projects `rel.rows[r][idx]` directly by position (from `*` / `t.*`),
    /// avoiding name-lookup ambiguity on duplicate column names or self-joins.
    direct_col: Option<usize>,
}

fn expand_result_columns(core: &SelectCore, rel: &Relation) -> SqlResult<Vec<ProjectedItem>> {
    // XDB dialect FROM-first query with omitted SELECT list implies `SELECT *`.
    if core.columns.is_empty() && core.from_first {
        return expand_star(None, None, None, rel);
    }
    let mut out = Vec::with_capacity(core.columns.len());
    for rc in &core.columns {
        match rc {
            ResultColumn::Star(rs) => {
                if core.from.is_none() {
                    return Err(SqlError::runtime("no tables specified"));
                }
                let items = expand_star(
                    rs.table.as_ref().map(|n| n.name.as_str()),
                    rs.exclude.as_deref(),
                    rs.replace.as_deref(),
                    rel,
                )?;
                out.extend(items);
            }
            ResultColumn::Expr(re) => {
                let output_name = match &re.alias {
                    Some(a) => a.name.clone(),
                    None => derive_column_name(&re.expr, &re.text),
                };
                let direct_col = match &re.expr {
                    Expr::Column(c) => resolve_direct_from_col(
                        c,
                        &rel.columns,
                        &rel.using_hidden,
                        &rel.source_ids,
                    ),
                    _ => None,
                };
                out.push(ProjectedItem {
                    expr: re.expr.clone(),
                    output_name,
                    explicit_alias: re.alias.as_ref().map(|a| a.name.clone()),
                    direct_col,
                });
            }
        }
    }
    Ok(out)
}

fn expand_star(
    table_filter: Option<&str>,
    exclude: Option<&[xdb_parser::Name]>,
    replace: Option<&[xdb_parser::StarReplacement]>,
    rel: &Relation,
) -> SqlResult<Vec<ProjectedItem>> {
    let mut out = Vec::new();
    let mut matched_table = table_filter.is_none();

    for (idx, col) in rel.columns.iter().enumerate() {
        if rel.star_hidden[idx] || col.is_rowid_alias {
            continue;
        }
        // Duplicate `USING`/`NATURAL` columns are hidden only for unqualified `*`, not `t.*`.
        if table_filter.is_none() && rel.using_hidden[idx] {
            continue;
        }
        if let Some(tf) = table_filter {
            match &col.table {
                Some(ct) if ct.eq_ignore_ascii_case(tf) => matched_table = true,
                _ => continue,
            }
        }
        if let Some(excl) = exclude {
            if excl.iter().any(|n| n.name.eq_ignore_ascii_case(&col.name)) {
                continue;
            }
        }
        if let Some(repls) = replace {
            if let Some(rep) = repls
                .iter()
                .find(|r| r.name.name.eq_ignore_ascii_case(&col.name))
            {
                out.push(ProjectedItem {
                    expr: rep.expr.clone(),
                    output_name: rep.name.name.clone(),
                    explicit_alias: Some(rep.name.name.clone()),
                    direct_col: None,
                });
                continue;
            }
        }
        let col_ref = Expr::Column(xdb_parser::ColumnRef {
            span: xdb_parser::Span::default(),
            schema: None,
            table: col.table.as_ref().map(|t| xdb_parser::Name {
                span: xdb_parser::Span::default(),
                name: t.clone(),
                quote: xdb_parser::QuoteStyle::None,
            }),
            column: xdb_parser::Name {
                span: xdb_parser::Span::default(),
                name: col.name.clone(),
                quote: xdb_parser::QuoteStyle::None,
            },
        });
        out.push(ProjectedItem {
            expr: col_ref,
            output_name: col.name.clone(),
            explicit_alias: None,
            direct_col: Some(idx),
        });
    }

    if !matched_table {
        return Err(SqlError::runtime(format!(
            "no such table: {}",
            table_filter.unwrap_or("")
        )));
    }
    Ok(out)
}

fn derive_column_name(expr: &Expr, raw_text: &str) -> String {
    match expr {
        Expr::Column(c) => c.column.name.clone(),
        Expr::Collate { operand, .. } => derive_column_name(operand, raw_text),
        _ => raw_text.trim().to_string(),
    }
}

fn apply_cte_column_names(
    cte: &CommonTableExpr,
    cols: &[ScopeColumn],
) -> SqlResult<Vec<ScopeColumn>> {
    if let Some(ref explicit) = cte.columns {
        if explicit.len() != cols.len() {
            return Err(SqlError::runtime(format!(
                "table {} has {} values for {} columns",
                cte.name.name,
                cols.len(),
                explicit.len()
            )));
        }
        Ok(cols
            .iter()
            .zip(explicit.iter())
            .map(|(c, i)| ScopeColumn {
                table: None,
                name: i.name.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_rowid_alias: false,
            })
            .collect())
    } else {
        Ok(cols
            .iter()
            .map(|c| ScopeColumn {
                table: None,
                name: c.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_rowid_alias: false,
            })
            .collect())
    }
}

fn relation_from_tvf(alias: &str, tvf: crate::functions::TableValuedResult) -> Relation {
    let columns = tvf
        .columns
        .into_iter()
        .map(|name| ScopeColumn {
            table: Some(alias.to_string()),
            name,
            affinity: Affinity::Blob,
            collation: "BINARY".to_string(),
            is_rowid_alias: false,
        })
        .collect::<Vec<_>>();
    let star_hidden = vec![false; columns.len()];
    let using_hidden = vec![false; columns.len()];
    let source_ids = vec![0u32; columns.len()];
    Relation {
        columns,
        star_hidden,
        using_hidden,
        source_ids,
        rows: tvf.rows,
    }
}

/// Parse an integer ordinal term (`1`, `2`, `1 COLLATE NOCASE`).
/// Overflowing integer literals return `Some((usize::MAX, coll))` so the caller emits
/// SQLite's exact `"Nth ORDER BY/GROUP BY term out of range"` error instead of evaluating a constant!
fn integer_ordinal_with_collation(expr: &Expr) -> Option<(usize, Option<&str>)> {
    let mut cur = expr;
    let mut outer_coll: Option<&str> = None;
    while let Expr::Collate {
        operand, collation, ..
    } = cur
    {
        if outer_coll.is_none() {
            outer_coll = Some(collation.name.as_str());
        }
        cur = operand;
    }
    if let Expr::Number(n) = cur {
        if !n.is_float && !n.is_hex {
            let ord = n.text.parse::<usize>().unwrap_or(usize::MAX);
            return Some((ord, outer_coll));
        }
    }
    None
}

fn bare_unqualified_col_and_coll(expr: &Expr) -> Option<(&str, Option<&str>)> {
    match expr {
        Expr::Column(c) if c.schema.is_none() && c.table.is_none() => {
            Some((c.column.name.as_str(), None))
        }
        Expr::Collate {
            operand, collation, ..
        } => bare_unqualified_col_and_coll(operand)
            .map(|(name, _)| (name, Some(collation.name.as_str()))),
        _ => None,
    }
}

/// Evaluate a `LIMIT` or `OFFSET` expression strictly matching SQLite's `OP_MustBeInt`.
fn eval_limit_int(
    expr: &Expr,
    outer_scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<i64> {
    let v = eval_expr(expr, outer_scopes, env)?;
    must_be_int(&v).ok_or_else(|| SqlError::runtime("datatype mismatch"))
}

fn flatten_compound_branches<'a>(
    body: &'a SelectBody,
    out: &mut Vec<(Option<CompoundOperator>, &'a SelectBody)>,
) {
    match body {
        SelectBody::Compound {
            op, left, right, ..
        } => {
            flatten_compound_branches(left, out);
            out.push((Some(*op), right));
        }
        other => {
            out.push((None, other));
        }
    }
}

fn collect_unqualified_col_names(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Column(c) => {
            if c.table.is_none() && c.schema.is_none() {
                out.insert(c.column.name.to_ascii_lowercase());
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => collect_unqualified_col_names(operand, out),
        Expr::Binary { left, right, .. } => {
            collect_unqualified_col_names(left, out);
            collect_unqualified_col_names(right, out);
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_unqualified_col_names(left, out);
            collect_unqualified_col_names(right, out);
            if let Some(e) = escape {
                collect_unqualified_col_names(e, out);
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_unqualified_col_names(operand, out);
            collect_unqualified_col_names(low, out);
            collect_unqualified_col_names(high, out);
        }
        Expr::InList { operand, items, .. } => {
            collect_unqualified_col_names(operand, out);
            for i in items {
                collect_unqualified_col_names(i, out);
            }
        }
        Expr::InSelect {
            operand, select, ..
        } => {
            collect_unqualified_col_names(operand, out);
            collect_unqualified_col_names_select(select, out);
        }
        Expr::InTable { operand, args, .. } => {
            collect_unqualified_col_names(operand, out);
            if let Some(list) = args {
                for a in list {
                    collect_unqualified_col_names(a, out);
                }
            }
        }
        Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
            collect_unqualified_col_names_select(select, out);
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(o) = operand {
                collect_unqualified_col_names(o, out);
            }
            for w in whens {
                collect_unqualified_col_names(&w.when, out);
                collect_unqualified_col_names(&w.then, out);
            }
            if let Some(e) = else_expr {
                collect_unqualified_col_names(e, out);
            }
        }
        Expr::Function(f) => {
            for a in &f.args {
                collect_unqualified_col_names(a, out);
            }
            if let Some(ref flt) = f.filter {
                collect_unqualified_col_names(flt, out);
            }
            if let Some(ref ob) = f.order_by {
                for o in ob {
                    collect_unqualified_col_names(&o.expr, out);
                }
            }
            if let Some(WindowRef::Spec(ws)) = &f.over {
                collect_unqualified_col_names_window(ws, out);
            }
        }
        Expr::Vector { items, .. } => {
            for i in items {
                collect_unqualified_col_names(i, out);
            }
        }
        _ => {}
    }
}

fn collect_unqualified_col_names_window(ws: &WindowSpec, out: &mut HashSet<String>) {
    for e in ws.partition_by.as_deref().unwrap_or(&[]) {
        collect_unqualified_col_names(e, out);
    }
    for o in ws.order_by.as_deref().unwrap_or(&[]) {
        collect_unqualified_col_names(&o.expr, out);
    }
    if let Some(ref fr) = ws.frame {
        if let Some(ref e) = fr.from.expr {
            collect_unqualified_col_names(e, out);
        }
        if let Some(ref to) = fr.to {
            if let Some(ref e) = to.expr {
                collect_unqualified_col_names(e, out);
            }
        }
    }
}

fn collect_unqualified_col_names_select(stmt: &SelectStmt, out: &mut HashSet<String>) {
    if let Some(ref with) = stmt.with {
        for cte in &with.ctes {
            collect_unqualified_col_names_select(&cte.select, out);
        }
    }
    collect_unqualified_col_names_body(&stmt.body, out);
    for o in stmt.order_by.as_deref().unwrap_or(&[]) {
        collect_unqualified_col_names(&o.expr, out);
    }
    if let Some(ref lim) = stmt.limit {
        collect_unqualified_col_names(&lim.count, out);
        if let Some(ref off) = lim.offset {
            collect_unqualified_col_names(off, out);
        }
    }
}

fn collect_unqualified_col_names_body(body: &SelectBody, out: &mut HashSet<String>) {
    match body {
        SelectBody::Core(core) => {
            for rc in &core.columns {
                match rc {
                    ResultColumn::Expr(re) => collect_unqualified_col_names(&re.expr, out),
                    ResultColumn::Star(rs) => {
                        for r in rs.replace.as_deref().unwrap_or(&[]) {
                            collect_unqualified_col_names(&r.expr, out);
                        }
                    }
                }
            }
            if let Some(ref from) = core.from {
                collect_unqualified_col_names_from(from, out);
            }
            if let Some(ref w) = core.where_clause {
                collect_unqualified_col_names(w, out);
            }
            for g in core.group_by.as_deref().unwrap_or(&[]) {
                collect_unqualified_col_names(g, out);
            }
            if let Some(ref h) = core.having {
                collect_unqualified_col_names(h, out);
            }
            for wd in core.windows.as_deref().unwrap_or(&[]) {
                collect_unqualified_col_names_window(&wd.window, out);
            }
        }
        SelectBody::Values(vals) => {
            for r in &vals.rows {
                for e in &r.items {
                    collect_unqualified_col_names(e, out);
                }
            }
        }
        SelectBody::Compound { left, right, .. } => {
            collect_unqualified_col_names_body(left, out);
            collect_unqualified_col_names_body(right, out);
        }
    }
}

fn collect_unqualified_col_names_from(src: &FromSource, out: &mut HashSet<String>) {
    match src {
        FromSource::Table(_) => {}
        FromSource::Function(ff) => {
            for a in &ff.args {
                collect_unqualified_col_names(a, out);
            }
        }
        FromSource::Subquery(fs) => collect_unqualified_col_names_select(&fs.select, out),
        FromSource::Group(fg) => collect_unqualified_col_names_from(&fg.source, out),
        FromSource::Join {
            left, right, on, ..
        } => {
            collect_unqualified_col_names_from(left, out);
            collect_unqualified_col_names_from(right, out);
            if let Some(e) = on {
                collect_unqualified_col_names(e, out);
            }
        }
    }
}

/// Dry-run validation of all column references inside `expr` for 0-row queries so
/// `SELECT nonexistent FROM empty_t`, `WHERE nonexistent = 1`, `GROUP BY nonexistent`, etc.
/// still raise SQLite's exact `"no such column"` / `"ambiguous column name"` errors.
fn validate_expr_columns(expr: &Expr, scopes: &[EvalScope<'_>], dqs: bool) -> SqlResult<()> {
    match expr {
        Expr::Column(c) => {
            let _ = resolve_column(c, scopes, dqs)?;
            Ok(())
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => validate_expr_columns(operand, scopes, dqs),
        Expr::Binary { left, right, .. } => {
            validate_expr_columns(left, scopes, dqs)?;
            validate_expr_columns(right, scopes, dqs)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            validate_expr_columns(left, scopes, dqs)?;
            validate_expr_columns(right, scopes, dqs)?;
            if let Some(e) = escape {
                validate_expr_columns(e, scopes, dqs)?;
            }
            Ok(())
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            validate_expr_columns(operand, scopes, dqs)?;
            validate_expr_columns(low, scopes, dqs)?;
            validate_expr_columns(high, scopes, dqs)
        }
        Expr::InList { operand, items, .. } => {
            validate_expr_columns(operand, scopes, dqs)?;
            for i in items {
                validate_expr_columns(i, scopes, dqs)?;
            }
            Ok(())
        }
        Expr::InSelect { operand, .. } => validate_expr_columns(operand, scopes, dqs),
        Expr::InTable { operand, args, .. } => {
            validate_expr_columns(operand, scopes, dqs)?;
            if let Some(list) = args {
                for a in list {
                    validate_expr_columns(a, scopes, dqs)?;
                }
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(o) = operand {
                validate_expr_columns(o, scopes, dqs)?;
            }
            for w in whens {
                validate_expr_columns(&w.when, scopes, dqs)?;
                validate_expr_columns(&w.then, scopes, dqs)?;
            }
            if let Some(e) = else_expr {
                validate_expr_columns(e, scopes, dqs)?;
            }
            Ok(())
        }
        Expr::Function(f) => {
            for a in &f.args {
                validate_expr_columns(a, scopes, dqs)?;
            }
            if let Some(ref flt) = f.filter {
                validate_expr_columns(flt, scopes, dqs)?;
            }
            if let Some(ref ob) = f.order_by {
                for o in ob {
                    validate_expr_columns(&o.expr, scopes, dqs)?;
                }
            }
            if let Some(WindowRef::Spec(ref ws)) = f.over {
                validate_window_spec_columns(ws, scopes, dqs)?;
            }
            Ok(())
        }
        Expr::Vector { items, .. } => {
            for i in items {
                validate_expr_columns(i, scopes, dqs)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_window_spec_columns(
    ws: &WindowSpec,
    scopes: &[EvalScope<'_>],
    dqs: bool,
) -> SqlResult<()> {
    if let Some(ref pb) = ws.partition_by {
        for e in pb {
            validate_expr_columns(e, scopes, dqs)?;
        }
    }
    if let Some(ref ob) = ws.order_by {
        for o in ob {
            validate_expr_columns(&o.expr, scopes, dqs)?;
        }
    }
    if let Some(ref fr) = ws.frame {
        if let Some(ref e) = fr.from.expr {
            validate_expr_columns(e, scopes, dqs)?;
        }
        if let Some(ref to) = fr.to {
            if let Some(ref e) = to.expr {
                validate_expr_columns(e, scopes, dqs)?;
            }
        }
    }
    Ok(())
}

fn ordinal_label(n: usize) -> String {
    match n % 100 {
        11..=13 => format!("{n}th"),
        _ => match n % 10 {
            1 => format!("{n}st"),
            2 => format!("{n}nd"),
            3 => format!("{n}rd"),
            _ => format!("{n}th"),
        },
    }
}

fn is_json_aggregate_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("json_group_array")
        || name.eq_ignore_ascii_case("json_group_object")
}

/// Coerce a comparison RHS value for an index or rowid seek matching SQLite's
/// `apply_comparison_affinity` rules without lossy `i64 -> f64` conversion on `REAL` columns
/// (preserving exact `int_float_compare` in B-tree `SearchKey`).
fn coerce_index_seek_value(v: SqlValue, col_aff: Affinity) -> SqlValue {
    if col_aff >= Affinity::Numeric {
        match v {
            SqlValue::Text(ref b) => apply_numeric_affinity(b, false).unwrap_or(v),
            other => other,
        }
    } else if col_aff == Affinity::Text {
        apply_affinity(v, Affinity::Text)
    } else {
        v
    }
}

/// Intersect two lower range bounds, returning the tighter (greater) bound.
fn tighten_low_bound(cur: Bound, cand: Bound, coll: Option<&Collation>) -> Bound {
    match (&cur, &cand) {
        (Bound::Unbounded, _) => cand,
        (_, Bound::Unbounded) => cur,
        (Bound::Included(a), Bound::Excluded(b)) => {
            if mem_compare(b, a, coll) != Ordering::Less {
                cand
            } else {
                cur
            }
        }
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            if mem_compare(b, a, coll) == Ordering::Greater {
                cand
            } else {
                cur
            }
        }
    }
}

/// Intersect two upper range bounds, returning the tighter (lesser) bound.
fn tighten_high_bound(cur: Bound, cand: Bound, coll: Option<&Collation>) -> Bound {
    match (&cur, &cand) {
        (Bound::Unbounded, _) => cand,
        (_, Bound::Unbounded) => cur,
        (Bound::Included(a), Bound::Excluded(b)) => {
            if mem_compare(b, a, coll) != Ordering::Greater {
                cand
            } else {
                cur
            }
        }
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            if mem_compare(b, a, coll) == Ordering::Less {
                cand
            } else {
                cur
            }
        }
    }
}

fn is_hashable_collation(coll: &Collation) -> bool {
    matches!(
        coll,
        Collation::Builtin {
            encoding: TextEncoding::Utf8,
            ..
        }
    )
}

fn estimate_row_bytes(row: &[SqlValue]) -> u64 {
    let mut total = (row.len() as u64).saturating_mul(24);
    for v in row {
        match v {
            SqlValue::Text(b) | SqlValue::Blob(b) => {
                total = total.saturating_add(b.len() as u64);
            }
            _ => {}
        }
    }
    total
}

/// Collation-aware row deduplication set: O(1) hashed for built-in UTF-8 BINARY/NOCASE/RTRIM,
/// metered linear comparison for custom user collations or UTF-16 BINARY.
struct RowDedup<'a> {
    coll_names: Vec<&'a str>,
    colls: Vec<&'a Collation>,
    hashable: bool,
    hashed: HashSet<Vec<GroupKey>>,
    linear: Vec<Vec<SqlValue>>,
    mem: MemoryGuard<'a>,
}

impl<'a> RowDedup<'a> {
    fn new(
        coll_names: &[&'a str],
        colls: &[&'a Collation],
        meter: Option<&'a dyn xdb_core::Meter>,
    ) -> Self {
        let hashable = colls.iter().all(|c| is_hashable_collation(c));
        Self {
            coll_names: coll_names.to_vec(),
            colls: colls.to_vec(),
            hashable,
            hashed: HashSet::new(),
            linear: Vec::new(),
            mem: MemoryGuard::new(meter),
        }
    }

    fn key_for(&self, row: &[SqlValue]) -> Vec<GroupKey> {
        row.iter()
            .enumerate()
            .map(|(i, v)| {
                let cn = self.coll_names.get(i).copied().unwrap_or("BINARY");
                GroupKey::from_sql_with_coll(v, cn)
            })
            .collect()
    }

    fn contains(&self, row: &[SqlValue], exec: &QueryExec<'_>) -> SqlResult<bool> {
        if self.hashable {
            return Ok(self.hashed.contains(&self.key_for(row)));
        }
        for prev in &self.linear {
            exec.charge_steps(1)?;
            if prev.len() == row.len()
                && prev
                    .iter()
                    .zip(row.iter())
                    .enumerate()
                    .all(|(i, (a, b))| mem_compare(a, b, self.colls.get(i).copied()) == Ordering::Equal)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Returns `true` if `row` was newly inserted (not already present).
    fn insert(&mut self, row: &[SqlValue], exec: &QueryExec<'_>) -> SqlResult<bool> {
        if self.hashable {
            let key = self.key_for(row);
            if self.hashed.contains(&key) {
                return Ok(false);
            }
            self.mem.reserve(estimate_row_bytes(row).saturating_add(32))?;
            self.hashed.insert(key);
            return Ok(true);
        }
        if self.contains(row, exec)? {
            return Ok(false);
        }
        self.mem.reserve(estimate_row_bytes(row).saturating_add(32))?;
        self.linear.push(row.to_vec());
        Ok(true)
    }
}

fn record_compound_branch_names(branch_names: &mut Vec<Vec<String>>, cols: &[ScopeColumn]) {
    if branch_names.len() < cols.len() {
        branch_names.resize_with(cols.len(), Vec::new);
    }
    for (i, c) in cols.iter().enumerate() {
        if !branch_names[i]
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&c.name))
        {
            branch_names[i].push(c.name.clone());
        }
    }
}

fn collect_referenced_window_names(
    expanded: &[ProjectedItem],
    outer_order_by: Option<&[OrderingTerm]>,
    defs: &[WindowDefinition],
) -> HashSet<String> {
    let mut names = HashSet::new();
    if defs.is_empty() {
        return names;
    }
    let mut win_calls = Vec::new();
    for p in expanded {
        if p.direct_col.is_none() {
            collect_windows(&p.expr, &mut win_calls);
        }
    }
    if let Some(ob) = outer_order_by {
        for t in ob {
            collect_windows(&t.expr, &mut win_calls);
        }
    }
    let mut work: Vec<String> = Vec::new();
    for wc in win_calls {
        match &wc.over {
            Some(WindowRef::Name(n)) => work.push(n.name.to_ascii_lowercase()),
            Some(WindowRef::Spec(ws)) => {
                if let Some(ref b) = ws.base {
                    work.push(b.name.to_ascii_lowercase());
                }
            }
            None => {}
        }
    }
    while let Some(wname) = work.pop() {
        if names.insert(wname.clone()) {
            if let Some(def) = defs
                .iter()
                .find(|d| d.name.name.eq_ignore_ascii_case(&wname))
            {
                if let Some(ref b) = def.window.base {
                    work.push(b.name.to_ascii_lowercase());
                }
            }
        }
    }
    names
}

fn select_stmt_references_table(stmt: &SelectStmt, name: &str) -> bool {
    if let Some(ref w) = stmt.with {
        for cte in &w.ctes {
            if select_stmt_references_table(&cte.select, name) {
                return true;
            }
            if cte.name.name.eq_ignore_ascii_case(name) {
                // Inner WITH defines a shadowing CTE with the same name; subsequent CTEs and the
                // body refer to the inner CTE, not the outer table/CTE.
                return false;
            }
        }
    }
    select_body_references_table(&stmt.body, name)
        || stmt
            .order_by
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|o| expr_references_table(&o.expr, name))
        || stmt.limit.as_ref().is_some_and(|l| {
            expr_references_table(&l.count, name)
                || l.offset
                    .as_deref()
                    .is_some_and(|o| expr_references_table(o, name))
        })
}

fn select_body_references_table_in_subquery(body: &SelectBody, name: &str) -> bool {
    match body {
        SelectBody::Core(core) => {
            core.from
                .as_ref()
                .is_some_and(|f| from_source_references_table_in_subquery(f, name))
                || core
                    .where_clause
                    .as_ref()
                    .is_some_and(|e| expr_references_table(e, name))
                || core
                    .having
                    .as_ref()
                    .is_some_and(|e| expr_references_table(e, name))
                || core.columns.iter().any(|rc| match rc {
                    ResultColumn::Expr(re) => expr_references_table(&re.expr, name),
                    ResultColumn::Star(_) => false,
                })
        }
        SelectBody::Values(v) => v
            .rows
            .iter()
            .any(|r| r.items.iter().any(|e| expr_references_table(e, name))),
        SelectBody::Compound { left, right, .. } => {
            select_body_references_table_in_subquery(left, name)
                || select_body_references_table_in_subquery(right, name)
        }
    }
}

fn from_source_references_table_in_subquery(src: &FromSource, name: &str) -> bool {
    match src {
        FromSource::Table(_) => false,
        FromSource::Function(ff) => ff.args.iter().any(|a| expr_references_table(a, name)),
        FromSource::Subquery(fs) => select_stmt_references_table(&fs.select, name),
        FromSource::Group(fg) => from_source_references_table_in_subquery(&fg.source, name),
        FromSource::Join {
            left, right, on, ..
        } => {
            from_source_references_table_in_subquery(left, name)
                || from_source_references_table_in_subquery(right, name)
                || on.as_deref().is_some_and(|e| expr_references_table(e, name))
        }
    }
}

fn select_body_references_table(body: &SelectBody, name: &str) -> bool {
    match body {
        SelectBody::Core(core) => {
            core.from
                .as_ref()
                .is_some_and(|f| from_source_references_table(f, name))
                || core
                    .where_clause
                    .as_ref()
                    .is_some_and(|e| expr_references_table(e, name))
                || core
                    .having
                    .as_ref()
                    .is_some_and(|e| expr_references_table(e, name))
                || core.columns.iter().any(|rc| match rc {
                    ResultColumn::Expr(re) => expr_references_table(&re.expr, name),
                    ResultColumn::Star(_) => false,
                })
        }
        SelectBody::Values(v) => v
            .rows
            .iter()
            .any(|r| r.items.iter().any(|e| expr_references_table(e, name))),
        SelectBody::Compound { left, right, .. } => {
            select_body_references_table(left, name) || select_body_references_table(right, name)
        }
    }
}

fn from_source_references_table(src: &FromSource, name: &str) -> bool {
    match src {
        FromSource::Table(ft) => ft.schema.is_none() && ft.name.name.eq_ignore_ascii_case(name),
        FromSource::Function(ff) => ff.args.iter().any(|a| expr_references_table(a, name)),
        FromSource::Subquery(fs) => select_stmt_references_table(&fs.select, name),
        FromSource::Group(fg) => from_source_references_table(&fg.source, name),
        FromSource::Join {
            left, right, on, ..
        } => {
            from_source_references_table(left, name)
                || from_source_references_table(right, name)
                || on.as_deref().is_some_and(|e| expr_references_table(e, name))
        }
    }
}

fn expr_references_table(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
            select_stmt_references_table(select, name)
        }
        Expr::InSelect { operand, select, .. } => {
            expr_references_table(operand, name) || select_stmt_references_table(select, name)
        }
        Expr::InTable {
            operand,
            schema,
            table,
            ..
        } => {
            expr_references_table(operand, name)
                || (schema.is_none() && table.name.eq_ignore_ascii_case(name))
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_references_table(operand, name),
        Expr::Binary { left, right, .. } => {
            expr_references_table(left, name) || expr_references_table(right, name)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_references_table(left, name)
                || expr_references_table(right, name)
                || escape
                    .as_deref()
                    .is_some_and(|e| expr_references_table(e, name))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_references_table(operand, name)
                || expr_references_table(low, name)
                || expr_references_table(high, name)
        }
        Expr::InList { operand, items, .. } => {
            expr_references_table(operand, name)
                || items.iter().any(|e| expr_references_table(e, name))
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_some_and(|e| expr_references_table(e, name))
                || whens.iter().any(|w| {
                    expr_references_table(&w.when, name) || expr_references_table(&w.then, name)
                })
                || else_expr
                    .as_deref()
                    .is_some_and(|e| expr_references_table(e, name))
        }
        Expr::Function(f) => f.args.iter().any(|a| expr_references_table(a, name)),
        Expr::Vector { items, .. } => items.iter().any(|e| expr_references_table(e, name)),
        _ => false,
    }
}

fn expr_is_pushdown_const(expr: &Expr, options: &QueryOptions) -> bool {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::Variable(_) => true,
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_is_pushdown_const(operand, options),
        Expr::Binary { left, right, .. } => {
            expr_is_pushdown_const(left, options) && expr_is_pushdown_const(right, options)
        }
        Expr::Function(f) => {
            let lower = f.name.name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "random"
                    | "randomblob"
                    | "changes"
                    | "total_changes"
                    | "last_insert_rowid"
                    | "sqlite_offset"
            ) || options.functions.has_custom_scalar(&lower, f.args.len())
            {
                return false;
            }
            f.over.is_none()
                && !options
                    .functions
                    .is_aggregate(&f.name.name, f.args.len(), f.star)
                && f.args.iter().all(|a| expr_is_pushdown_const(a, options))
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_none_or(|e| expr_is_pushdown_const(e, options))
                && whens.iter().all(|w| {
                    expr_is_pushdown_const(&w.when, options)
                        && expr_is_pushdown_const(&w.then, options)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|e| expr_is_pushdown_const(e, options))
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum OnlineAggKind {
    CountStar,
    CountCol(usize),
    Sum(usize, SumKind),
}

enum OnlineAggAcc {
    CountStar(i64),
    CountCol(usize, i64),
    Sum(usize, SumAcc),
}

#[inline]
fn make_online_acc(kind: OnlineAggKind) -> OnlineAggAcc {
    match kind {
        OnlineAggKind::CountStar => OnlineAggAcc::CountStar(0),
        OnlineAggKind::CountCol(cid) => OnlineAggAcc::CountCol(cid, 0),
        OnlineAggKind::Sum(cid, sk) => OnlineAggAcc::Sum(cid, SumAcc::new(sk)),
    }
}

#[inline]
fn push_online_accs(plan: &[OnlineAggKind], out: &mut Vec<OnlineAggAcc>) {
    for &k in plan {
        out.push(make_online_acc(k));
    }
}

#[inline]
fn step_online_acc(acc: &mut OnlineAggAcc, row: &[SqlValue]) {
    match acc {
        OnlineAggAcc::CountStar(c) => *c += 1,
        OnlineAggAcc::CountCol(cid, c) => {
            if !row[*cid].is_null() {
                *c += 1;
            }
        }
        OnlineAggAcc::Sum(cid, sa) => sa.step_value(&row[*cid]),
    }
}

#[inline]
fn inverse_online_acc(acc: &mut OnlineAggAcc, row: &[SqlValue]) {
    match acc {
        OnlineAggAcc::CountStar(c) => *c -= 1,
        OnlineAggAcc::CountCol(cid, c) => {
            if !row[*cid].is_null() {
                *c -= 1;
            }
        }
        OnlineAggAcc::Sum(cid, sa) => sa.inverse_value(&row[*cid]),
    }
}

#[inline]
fn value_online_acc(
    acc: &OnlineAggAcc,
    ctx: &crate::functions::FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    match acc {
        OnlineAggAcc::CountStar(c) | OnlineAggAcc::CountCol(_, c) => Ok(SqlValue::Int(*c)),
        OnlineAggAcc::Sum(_, sa) => sa.value(ctx),
    }
}

fn classify_online_agg(
    fcall: &FunctionExpr,
    columns: &[ScopeColumn],
    using_hidden: &[bool],
    source_ids: &[u32],
    options: &QueryOptions,
) -> Option<OnlineAggKind> {
    if fcall.filter.is_some()
        || fcall.order_by.is_some()
        || fcall.quantifier == Some(DistinctKind::Distinct)
        || !options
            .functions
            .is_aggregate(&fcall.name.name, fcall.args.len(), fcall.star)
    {
        return None;
    }
    if fcall.name.name.eq_ignore_ascii_case("count") {
        if fcall.star || fcall.args.is_empty() {
            return Some(OnlineAggKind::CountStar);
        }
        if fcall.args.len() == 1 {
            if let Expr::Column(c) = &fcall.args[0] {
                if let Some(cid) = resolve_direct_from_col(c, columns, using_hidden, source_ids) {
                    return Some(OnlineAggKind::CountCol(cid));
                }
            }
        }
        return None;
    }
    if fcall.args.len() == 1 && !fcall.star {
        let sk = if fcall.name.name.eq_ignore_ascii_case("sum") {
            Some(SumKind::Sum)
        } else if fcall.name.name.eq_ignore_ascii_case("total") {
            Some(SumKind::Total)
        } else if fcall.name.name.eq_ignore_ascii_case("avg") {
            Some(SumKind::Avg)
        } else {
            None
        };
        if let (Some(kind), Expr::Column(c)) = (sk, &fcall.args[0]) {
            if let Some(cid) = resolve_direct_from_col(c, columns, using_hidden, source_ids) {
                return Some(OnlineAggKind::Sum(cid, kind));
            }
        }
    }
    None
}

fn build_online_agg_plan(
    agg_calls: &[&FunctionExpr],
    rel: &Relation,
    options: &QueryOptions,
) -> Option<Vec<OnlineAggKind>> {
    let mut plan = Vec::with_capacity(agg_calls.len());
    for fcall in agg_calls {
        let kind = classify_online_agg(
            fcall,
            &rel.columns,
            &rel.using_hidden,
            &rel.source_ids,
            options,
        )?;
        plan.push(kind);
    }
    Some(plan)
}

#[inline]
fn compare_ordered_val(
    a: &SqlValue,
    b: &SqlValue,
    dir: SortOrder,
    nulls: Option<NullsOrder>,
    coll: &Collation,
) -> Ordering {
    if let (SqlValue::Int(x), SqlValue::Int(y)) = (a, b) {
        return if dir == SortOrder::Desc {
            y.cmp(x)
        } else {
            x.cmp(y)
        };
    }
    let a_null = a.is_null();
    let b_null = b.is_null();
    if a_null || b_null {
        if a_null && b_null {
            return Ordering::Equal;
        }
        let null_first = match nulls {
            Some(NullsOrder::First) => true,
            Some(NullsOrder::Last) => false,
            None => dir == SortOrder::Asc,
        };
        return match (a_null, null_first) {
            (true, true) | (false, false) => Ordering::Less,
            (true, false) | (false, true) => Ordering::Greater,
        };
    }
    let ord = mem_compare(a, b, Some(coll));
    if dir == SortOrder::Desc {
        ord.reverse()
    } else {
        ord
    }
}

fn compare_sort_keys(
    a: &[SqlValue],
    b: &[SqlValue],
    terms: &[OrderingTerm],
    colls: &[&Collation],
) -> Ordering {
    for (i, term) in terms.iter().enumerate() {
        let dir = term.order.unwrap_or(SortOrder::Asc);
        let ord = compare_ordered_val(&a[i], &b[i], dir, term.nulls, colls[i]);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn resolve_window_ref(wref: &WindowRef, defs: &[WindowDefinition]) -> SqlResult<WindowSpec> {
    match wref {
        WindowRef::Name(n) => {
            let def = defs
                .iter()
                .find(|d| d.name.name.eq_ignore_ascii_case(&n.name))
                .ok_or_else(|| SqlError::runtime(format!("no such window: {}", n.name)))?;
            resolve_window_chain(&def.window, defs, 0)
        }
        WindowRef::Spec(ws) => resolve_window_chain(ws, defs, 0),
    }
}

fn resolve_window_chain(
    spec: &WindowSpec,
    defs: &[WindowDefinition],
    depth: usize,
) -> SqlResult<WindowSpec> {
    if depth > 32 {
        return Err(SqlError::runtime("window chaining cycle detected"));
    }
    let Some(ref base_name) = spec.base else {
        return Ok(spec.clone());
    };
    let base_def = defs
        .iter()
        .find(|d| d.name.name.eq_ignore_ascii_case(&base_name.name))
        .ok_or_else(|| SqlError::runtime(format!("no such window: {}", base_name.name)))?;
    let mut merged = resolve_window_chain(&base_def.window, defs, depth + 1)?;
    // SQLite window inheritance validation (`window.c:sqlite3WindowChain`):
    // 1. Derived window may NEVER specify a PARTITION BY clause.
    // 2. Derived window may NOT specify ORDER BY if base already has ORDER BY.
    // 3. Base window may NOT have an explicit frame specification.
    if spec.partition_by.is_some() {
        return Err(SqlError::runtime(format!(
            "cannot override PARTITION clause of window: {}",
            base_name.name
        )));
    }
    if merged.order_by.is_some() && spec.order_by.is_some() {
        return Err(SqlError::runtime(format!(
            "cannot override ORDER BY clause of window: {}",
            base_name.name
        )));
    }
    if merged.frame.is_some() {
        return Err(SqlError::runtime(format!(
            "cannot override frame specification of window: {}",
            base_name.name
        )));
    }
    if spec.order_by.is_some() {
        merged.order_by = spec.order_by.clone();
    }
    if spec.frame.is_some() {
        merged.frame = spec.frame.clone();
    }
    merged.base = None;
    Ok(merged)
}

fn collect_and_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
        ..
    } = expr
    {
        collect_and_conjuncts(left, out);
        collect_and_conjuncts(right, out);
    } else {
        out.push(expr);
    }
}

fn flip_cmp(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Le => BinaryOp::Ge,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Ge => BinaryOp::Le,
        other => other,
    }
}

fn is_rowid_column_ref(expr: &Expr, tmeta: &crate::catalog::TableMeta, alias: &str) -> bool {
    let Expr::Column(c) = expr else {
        return false;
    };
    if let Some(ref t) = c.table {
        if !t.name.eq_ignore_ascii_case(alias) {
            return false;
        }
    }
    if let Some(ipk) = tmeta.ipk_column {
        if tmeta.columns[ipk].name.eq_ignore_ascii_case(&c.column.name) {
            return true;
        }
    }
    if matches!(
        c.column.name.to_ascii_lowercase().as_str(),
        "rowid" | "_rowid_" | "oid"
    ) && !tmeta
        .columns
        .iter()
        .any(|col| col.name.eq_ignore_ascii_case(&c.column.name))
    {
        return true;
    }
    false
}

fn match_table_col_ref(
    expr: &Expr,
    tmeta: &crate::catalog::TableMeta,
    alias: &str,
) -> Option<usize> {
    let Expr::Column(c) = expr else {
        return None;
    };
    if let Some(ref t) = c.table {
        if !t.name.eq_ignore_ascii_case(alias) {
            return None;
        }
    }
    tmeta
        .columns
        .iter()
        .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
}

fn expr_has_column_ref(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) => true,
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_has_column_ref(operand),
        Expr::Binary { left, right, .. } => expr_has_column_ref(left) || expr_has_column_ref(right),
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_has_column_ref(left)
                || expr_has_column_ref(right)
                || escape.as_deref().is_some_and(expr_has_column_ref)
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_has_column_ref(operand)
                || expr_has_column_ref(low)
                || expr_has_column_ref(high)
        }
        Expr::InList { operand, items, .. } => {
            expr_has_column_ref(operand) || items.iter().any(expr_has_column_ref)
        }
        Expr::Function(f) => f.args.iter().any(expr_has_column_ref),
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand.as_deref().is_some_and(expr_has_column_ref)
                || whens
                    .iter()
                    .any(|w| expr_has_column_ref(&w.when) || expr_has_column_ref(&w.then))
                || else_expr.as_deref().is_some_and(expr_has_column_ref)
        }
        Expr::Vector { items, .. } => items.iter().any(expr_has_column_ref),
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. } => true,
        _ => false,
    }
}

fn detect_simple_equijoin(
    lrel: &Relation,
    rrel: &Relation,
    on: Option<&Expr>,
    using_pairs: &[(usize, usize)],
) -> Option<(usize, usize, Affinity, String)> {
    if using_pairs.len() == 1 && on.is_none() {
        let (li, ri) = using_pairs[0];
        let aff = comparison_affinity(
            Some(lrel.columns[li].affinity),
            Some(rrel.columns[ri].affinity),
        );
        let cname = comparison_collation_name(
            Some(OperandCollation {
                name: Some(&lrel.columns[li].collation),
                explicit: false,
            }),
            Some(OperandCollation {
                name: Some(&rrel.columns[ri].collation),
                explicit: false,
            }),
        )
        .unwrap_or("BINARY");
        return Some((li, ri, aff, cname.to_string()));
    }
    if !using_pairs.is_empty() {
        return None;
    }
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
        ..
    } = on?
    else {
        return None;
    };
    let (Expr::Column(lc), Expr::Column(rc)) = (left.as_ref(), right.as_ref()) else {
        return None;
    };
    let find_in = |rel: &Relation, c: &xdb_parser::ColumnRef| -> Option<usize> {
        if let Some(ref s) = c.schema {
            if !is_valid_schema_name(Some(s.name.as_str())) {
                return None;
            }
        }
        let wt = c.table.as_ref().map(|n| n.name.as_str());
        let mut found: Option<usize> = None;
        for (i, sc) in rel.columns.iter().enumerate() {
            if wt.is_none() && rel.using_hidden.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Some(t) = wt {
                if sc.table.as_deref().is_none_or(|st| !st.eq_ignore_ascii_case(t)) {
                    continue;
                }
            }
            if sc.name.eq_ignore_ascii_case(&c.column.name) {
                if let Some(prev) = found {
                    if rel.columns[prev].is_rowid_alias && !sc.is_rowid_alias {
                        found = Some(i);
                    } else if !rel.columns[prev].is_rowid_alias && sc.is_rowid_alias {
                        // keep real column
                    } else {
                        return None;
                    }
                } else {
                    found = Some(i);
                }
            }
        }
        found
    };
    let (li, ri, swapped) = match (
        find_in(lrel, lc),
        find_in(rrel, rc),
        find_in(rrel, lc),
        find_in(lrel, rc),
    ) {
        (Some(li), Some(ri), None, None) => (li, ri, false),
        (None, None, Some(ri), Some(li)) => (li, ri, true),
        _ => return None,
    };
    let aff = comparison_affinity(
        Some(lrel.columns[li].affinity),
        Some(rrel.columns[ri].affinity),
    );
    let l_op = Some(OperandCollation {
        name: Some(&lrel.columns[li].collation),
        explicit: false,
    });
    let r_op = Some(OperandCollation {
        name: Some(&rrel.columns[ri].collation),
        explicit: false,
    });
    // Preserve ON clause operand order: `ON r.k = l.k` uses `r.k`'s collation first!
    let cname = if swapped {
        comparison_collation_name(r_op, l_op)
    } else {
        comparison_collation_name(l_op, r_op)
    }
    .unwrap_or("BINARY");
    Some((li, ri, aff, cname.to_string()))
}

/// Hash key for BINARY equi-joins (normalizing integer/real numeric equality across `[-2^63, 2^63)`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum HashKey {
    Num(i64),
    FloatBits(u64),
    Text(Box<[u8]>),
    Blob(Box<[u8]>),
}

impl HashKey {
    fn from_sql(v: &SqlValue) -> Option<Self> {
        match v {
            SqlValue::Null => None,
            SqlValue::Int(i) => Some(HashKey::Num(*i)),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    None
                } else if let Some(iv) = real_to_exact_i64(*r) {
                    Some(HashKey::Num(iv))
                } else {
                    let norm = if *r == 0.0 { 0.0f64 } else { *r };
                    Some(HashKey::FloatBits(norm.to_bits()))
                }
            }
            SqlValue::Text(b) => Some(HashKey::Text(b.clone())),
            SqlValue::Blob(b) => Some(HashKey::Blob(b.clone())),
        }
    }
}

/// Hash key for `GROUP BY` / `DISTINCT` / set ops (includes `Null`, supports BINARY/NOCASE/RTRIM).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GroupKey {
    Null,
    Num(i64),
    FloatBits(u64),
    Text(Box<[u8]>),
    Blob(Box<[u8]>),
}

impl GroupKey {
    fn from_sql_with_coll(v: &SqlValue, coll_name: &str) -> Self {
        match v {
            SqlValue::Null => GroupKey::Null,
            SqlValue::Int(i) => GroupKey::Num(*i),
            SqlValue::Real(r) => {
                if r.is_nan() {
                    GroupKey::Null
                } else if let Some(iv) = real_to_exact_i64(*r) {
                    GroupKey::Num(iv)
                } else {
                    let norm = if *r == 0.0 { 0.0f64 } else { *r };
                    GroupKey::FloatBits(norm.to_bits())
                }
            }
            SqlValue::Text(b) => {
                if coll_name.eq_ignore_ascii_case("NOCASE") {
                    if let Some(nul) = b.iter().position(|&x| x == 0) {
                        let mut norm = Vec::with_capacity(nul + 9);
                        norm.extend(b[..nul].iter().map(|&c| c.to_ascii_lowercase()));
                        norm.push(0);
                        norm.extend_from_slice(&(b.len() as u64).to_le_bytes());
                        GroupKey::Text(norm.into_boxed_slice())
                    } else {
                        let norm: Vec<u8> = b.iter().map(|&c| c.to_ascii_lowercase()).collect();
                        GroupKey::Text(norm.into_boxed_slice())
                    }
                } else if coll_name.eq_ignore_ascii_case("RTRIM") {
                    let mut end = b.len();
                    while end > 0 && b[end - 1] == b' ' {
                        end -= 1;
                    }
                    GroupKey::Text(b[..end].to_vec().into_boxed_slice())
                } else {
                    GroupKey::Text(b.clone())
                }
            }
            SqlValue::Blob(b) => GroupKey::Blob(b.clone()),
        }
    }
}

fn resolve_direct_from_col(
    c: &xdb_parser::ColumnRef,
    columns: &[ScopeColumn],
    using_hidden: &[bool],
    source_ids: &[u32],
) -> Option<usize> {
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

/// Collect all column names referenced in a `SelectCore` + `ORDER BY` for projection pruning
/// without heap-allocating lowercased `String`s. Returns `None` if `*` or `FROM`-first implicit
/// `*` is present (falling back safely to decoding all columns).
fn collect_core_referenced_col_names<'a>(
    core: &'a SelectCore,
    outer_order_by: Option<&'a [OrderingTerm]>,
) -> Option<Vec<&'a str>> {
    if core.from_first {
        return None;
    }
    let mut names = Vec::with_capacity(8);
    for rc in &core.columns {
        match rc {
            ResultColumn::Star(_) => return None,
            ResultColumn::Expr(re) => {
                if !collect_expr_col_names_no_subquery(&re.expr, &mut names) {
                    return None;
                }
            }
        }
    }
    if let Some(ref from_src) = core.from {
        if !collect_from_col_names_for_pruning(from_src, &mut names) {
            return None;
        }
    }
    if let Some(ref w) = core.where_clause {
        if !collect_expr_col_names_no_subquery(w, &mut names) {
            return None;
        }
    }
    if let Some(ref gb) = core.group_by {
        for g in gb {
            if !collect_expr_col_names_no_subquery(g, &mut names) {
                return None;
            }
        }
    }
    if let Some(ref hav) = core.having {
        if !collect_expr_col_names_no_subquery(hav, &mut names) {
            return None;
        }
    }
    if let Some(ref wins) = core.windows {
        for wd in wins {
            for e in wd.window.partition_by.as_deref().unwrap_or(&[]) {
                if !collect_expr_col_names_no_subquery(e, &mut names) {
                    return None;
                }
            }
            for o in wd.window.order_by.as_deref().unwrap_or(&[]) {
                if !collect_expr_col_names_no_subquery(&o.expr, &mut names) {
                    return None;
                }
            }
            if let Some(ref frame) = wd.window.frame {
                if !collect_frame_bound_col_names(&frame.from, &mut names)
                    || frame
                        .to
                        .as_ref()
                        .is_some_and(|e| !collect_frame_bound_col_names(e, &mut names))
                {
                    return None;
                }
            }
        }
    }
    if let Some(ob) = outer_order_by {
        for o in ob {
            if !collect_expr_col_names_no_subquery(&o.expr, &mut names) {
                return None;
            }
        }
    }
    Some(names)
}

#[inline]
fn push_col_name_unique<'a>(out: &mut Vec<&'a str>, name: &'a str) {
    if !out.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
        out.push(name);
    }
}

fn collect_from_col_names_for_pruning<'a>(src: &'a FromSource, out: &mut Vec<&'a str>) -> bool {
    match src {
        FromSource::Table(_) | FromSource::Subquery(_) => true,
        FromSource::Function(ff) => ff
            .args
            .iter()
            .all(|a| collect_expr_col_names_no_subquery(a, out)),
        FromSource::Group(fg) => collect_from_col_names_for_pruning(&fg.source, out),
        FromSource::Join {
            left,
            operator,
            right,
            on,
            using,
            ..
        } => {
            if (operator.flags & join_flag::NATURAL) != 0 {
                return false;
            }
            if !collect_from_col_names_for_pruning(left, out)
                || !collect_from_col_names_for_pruning(right, out)
            {
                return false;
            }
            if let Some(ref on_expr) = on {
                if !collect_expr_col_names_no_subquery(on_expr, out) {
                    return false;
                }
            }
            if let Some(ref ucols) = using {
                for u in ucols {
                    push_col_name_unique(out, &u.name);
                }
            }
            true
        }
    }
}

fn collect_expr_col_names_no_subquery<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) -> bool {
    match expr {
        Expr::Column(c) => {
            push_col_name_unique(out, &c.column.name);
            true
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => collect_expr_col_names_no_subquery(operand, out),
        Expr::Binary { left, right, .. } => {
            collect_expr_col_names_no_subquery(left, out)
                && collect_expr_col_names_no_subquery(right, out)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_expr_col_names_no_subquery(left, out)
                && collect_expr_col_names_no_subquery(right, out)
                && escape
                    .as_deref()
                    .is_none_or(|e| collect_expr_col_names_no_subquery(e, out))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_expr_col_names_no_subquery(operand, out)
                && collect_expr_col_names_no_subquery(low, out)
                && collect_expr_col_names_no_subquery(high, out)
        }
        Expr::InList { operand, items, .. } => {
            collect_expr_col_names_no_subquery(operand, out)
                && items
                    .iter()
                    .all(|i| collect_expr_col_names_no_subquery(i, out))
        }
        Expr::InTable { operand, args, .. } => {
            collect_expr_col_names_no_subquery(operand, out)
                && args.as_deref().unwrap_or(&[]).iter().all(|a| {
                    collect_expr_col_names_no_subquery(a, out)
                })
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_none_or(|e| collect_expr_col_names_no_subquery(e, out))
                && whens.iter().all(|w| {
                    collect_expr_col_names_no_subquery(&w.when, out)
                        && collect_expr_col_names_no_subquery(&w.then, out)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|e| collect_expr_col_names_no_subquery(e, out))
        }
        Expr::Function(f) => {
            let args_ok = f
                .args
                .iter()
                .all(|a| collect_expr_col_names_no_subquery(a, out));
            let flt_ok = f
                .filter
                .as_deref()
                .is_none_or(|e| collect_expr_col_names_no_subquery(e, out));
            let ob_ok = f
                .order_by
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .all(|o| collect_expr_col_names_no_subquery(&o.expr, out));
            let over_ok = match &f.over {
                Some(WindowRef::Spec(ws)) => {
                    ws.partition_by
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .all(|e| collect_expr_col_names_no_subquery(e, out))
                        && ws
                            .order_by
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .all(|o| collect_expr_col_names_no_subquery(&o.expr, out))
                        && ws.frame.as_ref().is_none_or(|fr| {
                            collect_frame_bound_col_names(&fr.from, out)
                                && fr
                                    .to
                                    .as_ref()
                                    .is_none_or(|e| collect_frame_bound_col_names(e, out))
                        })
                }
                _ => true,
            };
            args_ok && flt_ok && ob_ok && over_ok
        }
        Expr::Vector { items, .. } => items
            .iter()
            .all(|i| collect_expr_col_names_no_subquery(i, out)),
        Expr::Raise { message, .. } => message
            .as_deref()
            .is_none_or(|m| collect_expr_col_names_no_subquery(m, out)),
        Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
            collect_subquery_col_names(select, out)
        }
        Expr::InSelect {
            operand, select, ..
        } => {
            collect_expr_col_names_no_subquery(operand, out)
                && collect_subquery_col_names(select, out)
        }
        _ => true,
    }
}

fn collect_frame_bound_col_names<'a>(
    bound: &'a xdb_parser::FrameBound,
    out: &mut Vec<&'a str>,
) -> bool {
    bound
        .expr
        .as_deref()
        .is_none_or(|e| collect_expr_col_names_no_subquery(e, out))
}

fn collect_subquery_col_names<'a>(stmt: &'a SelectStmt, out: &mut Vec<&'a str>) -> bool {
    if let Some(ref w) = stmt.with {
        for cte in &w.ctes {
            if !collect_subquery_col_names(&cte.select, out) {
                return false;
            }
        }
    }
    if !collect_subquery_body_col_names(&stmt.body, out) {
        return false;
    }
    if let Some(ref ob) = stmt.order_by {
        for o in ob {
            if !collect_expr_col_names_no_subquery(&o.expr, out) {
                return false;
            }
        }
    }
    if let Some(ref lim) = stmt.limit {
        if !collect_expr_col_names_no_subquery(&lim.count, out) {
            return false;
        }
        if let Some(ref off) = lim.offset {
            if !collect_expr_col_names_no_subquery(off, out) {
                return false;
            }
        }
    }
    true
}

fn collect_subquery_body_col_names<'a>(body: &'a SelectBody, out: &mut Vec<&'a str>) -> bool {
    match body {
        SelectBody::Core(c) => {
            if c.from_first {
                return false;
            }
            for rc in &c.columns {
                match rc {
                    ResultColumn::Star(_) => return false,
                    ResultColumn::Expr(re) => {
                        if !collect_expr_col_names_no_subquery(&re.expr, out) {
                            return false;
                        }
                    }
                }
            }
            if let Some(ref f) = c.from {
                if !collect_subquery_from_col_names(f, out) {
                    return false;
                }
            }
            if let Some(ref w) = c.where_clause {
                if !collect_expr_col_names_no_subquery(w, out) {
                    return false;
                }
            }
            if let Some(ref gb) = c.group_by {
                for g in gb {
                    if !collect_expr_col_names_no_subquery(g, out) {
                        return false;
                    }
                }
            }
            if let Some(ref hav) = c.having {
                if !collect_expr_col_names_no_subquery(hav, out) {
                    return false;
                }
            }
            if let Some(ref wins) = c.windows {
                for wd in wins {
                    for e in wd.window.partition_by.as_deref().unwrap_or(&[]) {
                        if !collect_expr_col_names_no_subquery(e, out) {
                            return false;
                        }
                    }
                    for o in wd.window.order_by.as_deref().unwrap_or(&[]) {
                        if !collect_expr_col_names_no_subquery(&o.expr, out) {
                            return false;
                        }
                    }
                    if let Some(ref fr) = wd.window.frame {
                        if !collect_frame_bound_col_names(&fr.from, out)
                            || fr
                                .to
                                .as_ref()
                                .is_none_or(|e| collect_frame_bound_col_names(e, out))
                        {
                            return false;
                        }
                    }
                }
            }
            true
        }
        SelectBody::Values(v) => v.rows.iter().all(|r| {
            r.items
                .iter()
                .all(|e| collect_expr_col_names_no_subquery(e, out))
        }),
        SelectBody::Compound { left, right, .. } => {
            collect_subquery_body_col_names(left, out)
                && collect_subquery_body_col_names(right, out)
        }
    }
}

fn collect_subquery_from_col_names<'a>(src: &'a FromSource, out: &mut Vec<&'a str>) -> bool {
    match src {
        FromSource::Table(_) => true,
        FromSource::Function(ff) => ff
            .args
            .iter()
            .all(|a| collect_expr_col_names_no_subquery(a, out)),
        FromSource::Subquery(fs) => collect_subquery_col_names(&fs.select, out),
        FromSource::Group(fg) => collect_subquery_from_col_names(&fg.source, out),
        FromSource::Join {
            left, right, on, ..
        } => {
            collect_subquery_from_col_names(left, out)
                && collect_subquery_from_col_names(right, out)
                && on
                    .as_deref()
                    .is_none_or(|e| collect_expr_col_names_no_subquery(e, out))
        }
    }
}

/// Sound static check verifying `stmt` is deterministic and cannot reference any column
/// in `outer_scopes` (enabling safe per-query caching of uncorrelated subqueries).
fn select_stmt_is_uncorrelated(
    stmt: &SelectStmt,
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
) -> bool {
    select_stmt_is_uncorrelated_depth(stmt, outer_scopes, catalog, ctes, options, 0)
}

fn select_stmt_is_uncorrelated_depth(
    stmt: &SelectStmt,
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
    view_depth: usize,
) -> bool {
    if stmt.with.is_some() {
        return false;
    }
    if !select_body_is_uncorrelated_depth(
        &stmt.body,
        outer_scopes,
        catalog,
        ctes,
        options,
        view_depth,
    ) {
        return false;
    }
    let mut top_inner: Vec<(String, HashSet<String>)> = Vec::new();
    if let SelectBody::Core(core) = &stmt.body {
        if let Some(ref from) = core.from {
            collect_inner_table_cols(from, catalog, ctes, &mut top_inner);
        }
    }
    if let Some(ref ob) = stmt.order_by {
        for o in ob {
            if !expr_is_uncorrelated_depth(
                &o.expr,
                &top_inner,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) {
                return false;
            }
        }
    }
    if let Some(ref lim) = stmt.limit {
        if !expr_is_uncorrelated_depth(
            &lim.count,
            &[],
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        ) {
            return false;
        }
        if let Some(ref off) = lim.offset {
            if !expr_is_uncorrelated_depth(
                off,
                &[],
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) {
                return false;
            }
        }
    }
    true
}

fn select_body_is_uncorrelated_depth(
    body: &SelectBody,
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
    view_depth: usize,
) -> bool {
    match body {
        SelectBody::Core(core) => {
            let mut inner_tables: Vec<(String, HashSet<String>)> = Vec::new();
            if let Some(ref from) = core.from {
                if !from_source_is_uncorrelated(
                    from,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
                collect_inner_table_cols(from, catalog, ctes, &mut inner_tables);
            }
            for rc in &core.columns {
                if let ResultColumn::Expr(re) = rc {
                    if !expr_is_uncorrelated_depth(
                        &re.expr,
                        &inner_tables,
                        outer_scopes,
                        catalog,
                        ctes,
                        options,
                        view_depth,
                    ) {
                        return false;
                    }
                } else if let ResultColumn::Star(rs) = rc {
                    for rep in rs.replace.as_deref().unwrap_or(&[]) {
                        if !expr_is_uncorrelated_depth(
                            &rep.expr,
                            &inner_tables,
                            outer_scopes,
                            catalog,
                            ctes,
                            options,
                            view_depth,
                        ) {
                            return false;
                        }
                    }
                }
            }
            if let Some(ref w) = core.where_clause {
                if !expr_is_uncorrelated_depth(
                    w,
                    &inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
            }
            for g in core.group_by.as_deref().unwrap_or(&[]) {
                if !expr_is_uncorrelated_depth(
                    g,
                    &inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
            }
            if let Some(ref h) = core.having {
                if !expr_is_uncorrelated_depth(
                    h,
                    &inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
            }
            for wd in core.windows.as_deref().unwrap_or(&[]) {
                if !window_spec_is_uncorrelated_depth(
                    &wd.window,
                    &inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
            }
            true
        }
        SelectBody::Values(vals) => vals.rows.iter().all(|r| {
            r.items.iter().all(|e| {
                expr_is_uncorrelated_depth(e, &[], outer_scopes, catalog, ctes, options, view_depth)
            })
        }),
        SelectBody::Compound { left, right, .. } => {
            select_body_is_uncorrelated_depth(
                left,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && select_body_is_uncorrelated_depth(
                right,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }
    }
}

fn window_spec_is_uncorrelated_depth(
    ws: &WindowSpec,
    inner_tables: &[(String, HashSet<String>)],
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
    view_depth: usize,
) -> bool {
    let part_ok = ws.partition_by.as_deref().unwrap_or(&[]).iter().all(|e| {
        expr_is_uncorrelated_depth(
            e,
            inner_tables,
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        )
    });
    let ord_ok = ws.order_by.as_deref().unwrap_or(&[]).iter().all(|o| {
        expr_is_uncorrelated_depth(
            &o.expr,
            inner_tables,
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        )
    });
    let frame_ok = ws.frame.as_ref().is_none_or(|fr| {
        let from_ok = fr.from.expr.as_deref().is_none_or(|e| {
            expr_is_uncorrelated_depth(
                e,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        });
        let to_ok = fr.to.as_ref().and_then(|t| t.expr.as_deref()).is_none_or(|e| {
            expr_is_uncorrelated_depth(
                e,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        });
        from_ok && to_ok
    });
    part_ok && ord_ok && frame_ok
}

fn view_is_deterministic(
    select_sql: &str,
    catalog: &dyn SqlCatalog,
    options: &QueryOptions,
    depth: usize,
) -> bool {
    if depth >= 8 {
        return false;
    }
    let parse_opts = ParseOptions {
        dialect: options.dialect,
        ..ParseOptions::default()
    };
    let Ok(stmt) = xdb_parser::parse_statement(select_sql, &parse_opts) else {
        return false;
    };
    let sel = match stmt {
        Statement::Select(s) => s,
        Statement::CreateView(cv) => cv.select,
        _ => return false,
    };
    select_stmt_is_uncorrelated_depth(&sel, &[], catalog, &HashMap::new(), options, depth + 1)
}

fn collect_inner_table_cols(
    src: &FromSource,
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    out: &mut Vec<(String, HashSet<String>)>,
) {
    match src {
        FromSource::Table(ft) => {
            let schema = ft.schema.as_ref().map(|n| n.name.as_str());
            let alias = ft
                .alias
                .as_ref()
                .unwrap_or(&ft.name)
                .name
                .to_ascii_lowercase();
            if schema.is_none() {
                if let Some(cte) = ctes.get(&ft.name.name.to_ascii_lowercase()) {
                    let cols = cte
                        .columns
                        .iter()
                        .map(|c| c.name.to_ascii_lowercase())
                        .collect();
                    out.push((alias, cols));
                    return;
                }
            }
            if let Ok(Some(tmeta)) = catalog.get_table_arc(schema, &ft.name.name) {
                let mut cols: HashSet<String> = tmeta
                    .columns
                    .iter()
                    .map(|c| c.name.to_ascii_lowercase())
                    .collect();
                if tmeta.has_rowid {
                    cols.insert("rowid".to_string());
                    cols.insert("_rowid_".to_string());
                    cols.insert("oid".to_string());
                }
                out.push((alias, cols));
            }
        }
        FromSource::Group(fg) => {
            if let Some(ref alias) = fg.alias {
                let mut sub = Vec::new();
                collect_inner_table_cols(&fg.source, catalog, ctes, &mut sub);
                let mut merged = HashSet::new();
                for (_, cols) in sub {
                    merged.extend(cols);
                }
                out.push((alias.name.to_ascii_lowercase(), merged));
            } else {
                collect_inner_table_cols(&fg.source, catalog, ctes, out);
            }
        }
        FromSource::Join { left, right, .. } => {
            collect_inner_table_cols(left, catalog, ctes, out);
            collect_inner_table_cols(right, catalog, ctes, out);
        }
        FromSource::Function(_) | FromSource::Subquery(_) => {}
    }
}

fn from_source_is_uncorrelated(
    src: &FromSource,
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
    view_depth: usize,
) -> bool {
    match src {
        FromSource::Table(ft) => {
            let schema = ft.schema.as_ref().map(|n| n.name.as_str());
            if schema.is_none() && ctes.contains_key(&ft.name.name.to_ascii_lowercase()) {
                return true;
            }
            if matches!(catalog.get_table_arc(schema, &ft.name.name), Ok(Some(_))) {
                return true;
            }
            if let Ok(Some(vmeta)) = catalog.get_view(schema, &ft.name.name) {
                return view_is_deterministic(&vmeta.select_sql, catalog, options, view_depth);
            }
            false
        }
        FromSource::Function(ff) => ff.args.iter().all(|a| {
            expr_is_uncorrelated_depth(a, &[], outer_scopes, catalog, ctes, options, view_depth)
        }),
        FromSource::Subquery(fs) => select_stmt_is_uncorrelated_depth(
            &fs.select,
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        ),
        FromSource::Group(fg) => from_source_is_uncorrelated(
            &fg.source,
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        ),
        FromSource::Join {
            left, right, on, ..
        } => {
            if !from_source_is_uncorrelated(left, outer_scopes, catalog, ctes, options, view_depth)
                || !from_source_is_uncorrelated(
                    right,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            {
                return false;
            }
            if let Some(on_e) = on {
                let mut inner = Vec::new();
                collect_inner_table_cols(left, catalog, ctes, &mut inner);
                collect_inner_table_cols(right, catalog, ctes, &mut inner);
                if !expr_is_uncorrelated_depth(
                    on_e,
                    &inner,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) {
                    return false;
                }
            }
            true
        }
    }
}

fn col_matches_outer_scopes(c: &xdb_parser::ColumnRef, outer_scopes: &[EvalScope<'_>]) -> bool {
    let wt = c.table.as_ref().map(|n| n.name.as_str());
    let wc = c.column.name.as_str();
    for scope in outer_scopes {
        for col in scope.columns {
            if let Some(t) = wt {
                if col.table.as_deref().is_none_or(|ct| !ct.eq_ignore_ascii_case(t)) {
                    continue;
                }
            }
            if col.name.eq_ignore_ascii_case(wc) {
                return true;
            }
        }
        if wt.is_none() {
            if let Some(aliases) = scope.aliases {
                if aliases.iter().any(|(a, _, _, _)| a.eq_ignore_ascii_case(wc)) {
                    return true;
                }
            }
        }
    }
    false
}

fn col_shadowed_by_inner(
    c: &xdb_parser::ColumnRef,
    inner_tables: &[(String, HashSet<String>)],
) -> bool {
    let wc = c.column.name.to_ascii_lowercase();
    match &c.table {
        Some(t) => {
            let tl = t.name.to_ascii_lowercase();
            inner_tables
                .iter()
                .any(|(alias, cols)| *alias == tl && cols.contains(&wc))
        }
        None => inner_tables.iter().any(|(_, cols)| cols.contains(&wc)),
    }
}

fn expr_is_uncorrelated_depth(
    expr: &Expr,
    inner_tables: &[(String, HashSet<String>)],
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn SqlCatalog,
    ctes: &HashMap<String, Arc<CteTable>>,
    options: &QueryOptions,
    view_depth: usize,
) -> bool {
    match expr {
        Expr::Column(c) => {
            !col_matches_outer_scopes(c, outer_scopes) || col_shadowed_by_inner(c, inner_tables)
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_is_uncorrelated_depth(
            operand,
            inner_tables,
            outer_scopes,
            catalog,
            ctes,
            options,
            view_depth,
        ),
        Expr::Binary { left, right, .. } => {
            expr_is_uncorrelated_depth(
                left,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && expr_is_uncorrelated_depth(
                right,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }
        Expr::Like {
            op,
            left,
            right,
            escape,
            ..
        } => {
            if matches!(op, LikeOperator::Regexp | LikeOperator::Match) {
                return false;
            }
            expr_is_uncorrelated_depth(
                left,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && expr_is_uncorrelated_depth(
                right,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && escape.as_deref().is_none_or(|e| {
                expr_is_uncorrelated_depth(
                    e,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            })
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_is_uncorrelated_depth(
                operand,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && expr_is_uncorrelated_depth(
                low,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && expr_is_uncorrelated_depth(
                high,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }
        Expr::InList { operand, items, .. } => {
            expr_is_uncorrelated_depth(
                operand,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && items.iter().all(|i| {
                expr_is_uncorrelated_depth(
                    i,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            })
        }
        Expr::InSelect {
            operand, select, ..
        } => {
            expr_is_uncorrelated_depth(
                operand,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            ) && select_stmt_is_uncorrelated_depth(
                select,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }
        Expr::InTable {
            operand,
            schema,
            table,
            args,
            ..
        } => {
            let target_ok = if args.is_some() {
                true
            } else {
                let s = schema.as_ref().map(|n| n.name.as_str());
                if (s.is_none() && ctes.contains_key(&table.name.to_ascii_lowercase()))
                    || matches!(catalog.get_table_arc(s, &table.name), Ok(Some(_)))
                {
                    true
                } else if let Ok(Some(vmeta)) = catalog.get_view(s, &table.name) {
                    view_is_deterministic(&vmeta.select_sql, catalog, options, view_depth)
                } else {
                    false
                }
            };
            target_ok
                && expr_is_uncorrelated_depth(
                    operand,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
                && args.as_deref().unwrap_or(&[]).iter().all(|a| {
                    expr_is_uncorrelated_depth(
                        a,
                        inner_tables,
                        outer_scopes,
                        catalog,
                        ctes,
                        options,
                        view_depth,
                    )
                })
        }
        Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
            select_stmt_is_uncorrelated_depth(
                select,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand.as_deref().is_none_or(|e| {
                expr_is_uncorrelated_depth(
                    e,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            }) && whens.iter().all(|w| {
                expr_is_uncorrelated_depth(
                    &w.when,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ) && expr_is_uncorrelated_depth(
                    &w.then,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            }) && else_expr.as_deref().is_none_or(|e| {
                expr_is_uncorrelated_depth(
                    e,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            })
        }
        Expr::Function(f) => {
            let fname = f.name.name.as_str();
            if fname.eq_ignore_ascii_case("random")
                || fname.eq_ignore_ascii_case("randomblob")
                || fname.eq_ignore_ascii_case("changes")
                || fname.eq_ignore_ascii_case("total_changes")
                || fname.eq_ignore_ascii_case("last_insert_rowid")
                || fname.eq_ignore_ascii_case("sqlite_offset")
                || options.functions.has_custom_scalar(fname, f.args.len())
            {
                return false;
            }
            let args_ok = f.args.iter().all(|a| {
                expr_is_uncorrelated_depth(
                    a,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            });
            let flt_ok = f.filter.as_deref().is_none_or(|e| {
                expr_is_uncorrelated_depth(
                    e,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            });
            let ob_ok = f.order_by.as_deref().unwrap_or(&[]).iter().all(|o| {
                expr_is_uncorrelated_depth(
                    &o.expr,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                )
            });
            let over_ok = match &f.over {
                Some(WindowRef::Spec(ws)) => window_spec_is_uncorrelated_depth(
                    ws,
                    inner_tables,
                    outer_scopes,
                    catalog,
                    ctes,
                    options,
                    view_depth,
                ),
                _ => true,
            };
            args_ok && flt_ok && ob_ok && over_ok
        }
        Expr::Vector { items, .. } => items.iter().all(|i| {
            expr_is_uncorrelated_depth(
                i,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }),
        Expr::Raise { message, .. } => message.as_deref().is_none_or(|m| {
            expr_is_uncorrelated_depth(
                m,
                inner_tables,
                outer_scopes,
                catalog,
                ctes,
                options,
                view_depth,
            )
        }),
        _ => true,
    }
}

// ───────────────────────────── Public entry points ─────────────────────────────

/// Execute a parsed [`SelectStmt`] against `catalog`, returning full column metadata
/// (table qualifier, name, affinity, collation) alongside the result rows.
pub fn execute_select_stmt_detailed(
    catalog: &dyn SqlCatalog,
    stmt: &SelectStmt,
    params: &QueryParams,
    options: &QueryOptions,
) -> SqlResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
    let exec = QueryExec::new(catalog, options, params);
    exec.execute_select_stmt(stmt, &[])
}

/// Execute a parsed [`SelectStmt`] against `catalog`.
pub fn execute_select_stmt(
    catalog: &dyn SqlCatalog,
    stmt: &SelectStmt,
    params: &QueryParams,
    options: &QueryOptions,
) -> SqlResult<QueryResult> {
    let (cols, rows) = execute_select_stmt_detailed(catalog, stmt, params, options)?;
    Ok(QueryResult {
        columns: cols.into_iter().map(|c| c.name).collect(),
        rows,
    })
}

/// Parse and execute a SQL `SELECT` (or `VALUES` / `WITH ... SELECT`) query string against `catalog`.
pub fn execute_query(
    catalog: &dyn SqlCatalog,
    sql: &str,
    params: &QueryParams,
    options: &QueryOptions,
) -> SqlResult<QueryResult> {
    let parse_opts = ParseOptions {
        dialect: options.dialect,
        ..ParseOptions::default()
    };
    let stmt = match options.meter.as_deref() {
        Some(m) => parse_statement_metered(sql, &parse_opts, m)?,
        None => xdb_parser::parse_statement(sql, &parse_opts)?,
    };
    match stmt {
        Statement::Select(sel) => execute_select_stmt(catalog, &sel, params, options),
        Statement::Explain { .. } => Err(SqlError::unsupported(
            "EXPLAIN is not yet supported in Phase 2 read-only engine",
        )),
        _ => Err(SqlError::unsupported(
            "only SELECT / VALUES queries are supported in read-only execution",
        )),
    }
}
