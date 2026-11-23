//! Full SQLite 3.54.1 + XDB dialect `SELECT` query planner and executor.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use xdb_core::value::{
    apply_affinity, boolean_value, compare_op, comparison_affinity, comparison_collation_name,
    mem_compare, must_be_int, real_to_exact_i64, Collation, CompareOp, OperandCollation,
};
use xdb_core::{Affinity, SqlValue};
use xdb_parser::{
    join_flag, parse_statement_metered, BinaryOp, CommonTableExpr, CompoundOperator, DistinctKind,
    Expr, FromSource, FunctionExpr, LimitClause, NullsOrder, OrderByAll, OrderingTerm,
    ParseOptions, ResultColumn, SelectBody, SelectCore, SelectStmt, SortOrder, Statement,
    WindowDefinition, WindowRef, WindowSpec, WithClause,
};

use crate::catalog::{Bound, QueryOptions, QueryParams, QueryResult, SqlCatalog, TableScanHint};
use crate::errors::{SqlError, SqlResult};
use crate::eval::{
    collect_aggregates, collect_windows, eval_expr, expr_affinity, expr_collation,
    expr_contains_aggregate, expr_contains_window, is_valid_schema_name, resolve_column, EvalEnv,
    EvalScope, ExprCollation, ScopeColumn, SelectWithMeta, SubqueryRunner, TableRowsWithMeta,
};
use crate::functions::{
    call_builtin_tvf, compute_frame_rows, eval_window_only_partition, is_builtin_window_only,
    PartitionPeers, ResolvedFrame,
};
use crate::utf8::MemoryGuard;

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
    rows: Vec<Vec<SqlValue>>,
}

#[derive(Debug, Clone)]
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
            rows: vec![Vec::new()],
        }
    }
}

/// Materialized CTE entry visible in the current query scope.
#[derive(Debug, Clone)]
struct CteTable {
    columns: Vec<ScopeColumn>,
    rows: Vec<Vec<SqlValue>>,
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
}

type SelectRowsMeta = (Vec<ScopeColumn>, Vec<bool>, Vec<Vec<SqlValue>>);
type SelectCoreResult = (Vec<ScopeColumn>, Vec<bool>, Vec<Vec<SqlValue>>, OrderEvalContext);

/// Execution context holding catalog, options, parameters, and active CTE bindings.
struct QueryExec<'a> {
    catalog: &'a dyn SqlCatalog,
    options: &'a QueryOptions,
    params: &'a QueryParams,
    ctes: HashMap<String, CteTable>,
    view_depth: usize,
    query_depth: usize,
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
        }
    }

    fn child_with_ctes(&self, ctes: HashMap<String, CteTable>) -> Self {
        Self {
            catalog: self.catalog,
            options: self.options,
            params: self.params,
            ctes,
            view_depth: self.view_depth,
            query_depth: self.query_depth + 1,
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
        EvalEnv::new(self.options, self.params, self)
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
        for cte in &with.ctes {
            let cte_table = if with.recursive {
                self.eval_recursive_cte(cte, &map, outer_scopes)?
            } else {
                let runner = self.child_with_ctes(map.clone());
                let (cols, rows) = runner.execute_select_stmt(&cte.select, outer_scopes)?;
                let cte_cols = apply_cte_column_names(cte, &cols)?;
                CteTable {
                    columns: cte_cols,
                    rows,
                }
            };
            map.insert(cte.name.name.to_ascii_lowercase(), cte_table);
        }
        Ok(self.child_with_ctes(map))
    }

    fn eval_recursive_cte(
        &self,
        cte: &CommonTableExpr,
        existing: &HashMap<String, CteTable>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<CteTable> {
        // Flatten left-associative compound branches so we handle BOTH single and multiple
        // recursive terms (`base UNION ALL rec1 UNION ALL rec2`), while treating non-self-referencing
        // compound CTEs as normal non-recursive queries!
        let mut flat: Vec<(Option<CompoundOperator>, &SelectBody)> = Vec::new();
        flatten_compound_branches(&cte.select.body, &mut flat);

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

            let base_runner = self.child_with_ctes(existing.clone());
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
            let mut dedup = RowDedup::new(&coll_names, &colls);
            let mut mem = self.make_mem_guard();

            let mut all_rows: Vec<Vec<SqlValue>> = Vec::new();
            let mut frontier: Vec<Vec<SqlValue>> = Vec::new();

            let early_stop_rows = if let Some(ref lim) = cte.select.limit {
                let count = eval_limit_int(&lim.count, outer_scopes, &env)?;
                let offset = match &lim.offset {
                    Some(o) => eval_limit_int(o, outer_scopes, &env)?.max(0),
                    None => 0,
                };
                if count >= 0 {
                    Some(usize::try_from(count.saturating_add(offset)).unwrap_or(usize::MAX))
                } else {
                    None
                }
            } else {
                None
            };

            let key = cte.name.name.to_ascii_lowercase();

            if let Some(ref ord_terms) = cte.select.order_by {
                // SQLite recursive CTE `ORDER BY`: priority-queue traversal popping the single
                // minimum frontier row per step (enabling bounded `ORDER BY ... LIMIT` on infinite
                // recursive CTEs and matching SQLite's exact priority-queue output order!).
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
                        let mut sc_vec = outer_scopes.to_vec();
                        sc_vec.push(EvalScope::simple(&cte_cols, &[]));
                        let cname = expr_collation(&term.expr, &sc_vec)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        term_colls.push(env.lookup_collation(&cname)?);
                    }
                }
                let compute_keys = |row: &[SqlValue]| -> SqlResult<Vec<SqlValue>> {
                    let mut ks = Vec::with_capacity(ord_terms.len());
                    let mut sc_vec = outer_scopes.to_vec();
                    sc_vec.push(EvalScope::simple(&cte_cols, row));
                    for term in ord_terms {
                        if let Some((ord, _)) = integer_ordinal_with_collation(&term.expr) {
                            ks.push(row[ord - 1].clone());
                        } else {
                            ks.push(eval_expr(&term.expr, &sc_vec, &env)?);
                        }
                    }
                    Ok(ks)
                };

                let mut pq: Vec<(Vec<SqlValue>, Vec<SqlValue>)> = Vec::new();
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
                    pq.push((ks, row));
                }

                let mut iter = 0usize;
                while !pq.is_empty() {
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
                    let mut best_i = 0usize;
                    for i in 1..pq.len() {
                        if compare_sort_keys(&pq[i].0, &pq[best_i].0, ord_terms, &term_colls)
                            == Ordering::Less
                        {
                            best_i = i;
                        }
                    }
                    let (_, popped) = pq.swap_remove(best_i);
                    all_rows.push(popped.clone());
                    if let Some(cap) = early_stop_rows {
                        if all_rows.len() >= cap {
                            break;
                        }
                    }
                    let mut step_map = existing.clone();
                    step_map.insert(
                        key.clone(),
                        CteTable {
                            columns: cte_cols.clone(),
                            rows: vec![popped],
                        },
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
                            if *is_dist && !dedup.insert(&row, self)? {
                                continue;
                            }
                            mem.reserve(estimate_row_bytes(&row))?;
                            let ks = compute_keys(&row)?;
                            pq.push((ks, row));
                        }
                    }
                }
                let final_rows = self.apply_order_and_limit(
                    &cte_cols,
                    all_rows,
                    None,
                    None,
                    None,
                    cte.select.limit.as_ref(),
                    outer_scopes,
                )?;
                return Ok(CteTable {
                    columns: cte_cols,
                    rows: final_rows,
                });
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

                let mut step_map = existing.clone();
                step_map.insert(
                    key.clone(),
                    CteTable {
                        columns: cte_cols.clone(),
                        rows: std::mem::take(&mut frontier),
                    },
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
                        if *is_dist && !dedup.insert(&row, self)? {
                            continue;
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

            let final_rows = self.apply_order_and_limit(
                &cte_cols,
                all_rows,
                None,
                None,
                cte.select.order_by_all.as_ref(),
                cte.select.limit.as_ref(),
                outer_scopes,
            )?;
            return Ok(CteTable {
                columns: cte_cols,
                rows: final_rows,
            });
        }

        // Non-recursive CTE under WITH RECURSIVE: execute normally.
        let runner = self.child_with_ctes(existing.clone());
        let (cols, rows) = runner.execute_select_stmt(&cte.select, outer_scopes)?;
        let cte_cols = apply_cte_column_names(cte, &cols)?;
        Ok(CteTable {
            columns: cte_cols,
            rows,
        })
    }

    fn execute_select_no_with(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<SelectRowsMeta> {
        match &stmt.body {
            SelectBody::Core(core) => {
                // Keep FROM-scope info available for ORDER BY resolution in single-core queries.
                let (out_cols, out_expl, out_rows, order_ctx) =
                    self.execute_select_core(core, outer_scopes, stmt.order_by.as_deref())?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    Some(&order_ctx),
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                    outer_scopes,
                )?;
                Ok((out_cols, out_expl, final_rows))
            }
            SelectBody::Values(_) | SelectBody::Compound { .. } => {
                let (out_cols, out_expl, out_rows) =
                    self.execute_select_body(&stmt.body, outer_scopes)?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    None,
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
        self.execute_select_body_depth(body, outer_scopes, self.query_depth)
    }

    fn execute_select_body_depth(
        &self,
        body: &SelectBody,
        outer_scopes: &[EvalScope<'_>],
        depth: usize,
    ) -> SqlResult<SelectRowsMeta> {
        self.check_depth(depth)?;
        match body {
            SelectBody::Core(core) => {
                let (cols, expl, rows, _) = self.execute_select_core(core, outer_scopes, None)?;
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
                let cols = (1..=w)
                    .map(|i| ScopeColumn {
                        table: None,
                        name: format!("column{i}"),
                        affinity: Affinity::None,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    })
                    .collect();
                Ok((cols, vec![false; w], rows))
            }
            SelectBody::Compound {
                op, left, right, ..
            } => {
                let (mut lcols, mut lexpl, lrows) =
                    self.execute_select_body_depth(left, outer_scopes, depth + 1)?;
                let (rcols, rexpl, rrows) =
                    self.execute_select_body_depth(right, outer_scopes, depth + 1)?;
                if lcols.len() != rcols.len() {
                    return Err(SqlError::runtime(format!(
                        "SELECTs to the left and right of {} do not have the same number of result columns",
                        op.as_str()
                    )));
                }
                // Merge collations across compound branches (explicit COLLATE on RHS overrides
                // implicit collation on LHS, matching SQLite `SELECT 'a' UNION SELECT 'A' COLLATE NOCASE`).
                for i in 0..lcols.len() {
                    let lc = OperandCollation {
                        name: Some(&lcols[i].collation),
                        explicit: lexpl[i],
                    };
                    let rc = OperandCollation {
                        name: Some(&rcols[i].collation),
                        explicit: rexpl[i],
                    };
                    if let Some(eff) = comparison_collation_name(Some(lc), Some(rc)) {
                        lcols[i].collation = eff.to_string();
                    }
                    lexpl[i] = lexpl[i] || rexpl[i];
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
                        let mut dedup = RowDedup::new(&coll_names, &colls);
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
                        let mut rset = RowDedup::new(&coll_names, &colls);
                        for r in &rrows {
                            self.charge_steps(1)?;
                            let _ = rset.insert(r, self)?;
                        }
                        let mut out_set = RowDedup::new(&coll_names, &colls);
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
                        let mut rset = RowDedup::new(&coll_names, &colls);
                        for r in &rrows {
                            self.charge_steps(1)?;
                            let _ = rset.insert(r, self)?;
                        }
                        let mut out_set = RowDedup::new(&coll_names, &colls);
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
                Ok((lcols, lexpl, out_rows))
            }
        }
    }

    fn execute_select_core(
        &self,
        core: &SelectCore,
        outer_scopes: &[EvalScope<'_>],
        outer_order_by: Option<&[OrderingTerm]>,
    ) -> SqlResult<SelectCoreResult> {
        let env = self.make_env();

        // 1. Evaluate FROM clause (with optional single-table WHERE pushdown)
        let mut rel = match &core.from {
            None => Relation::single_empty_row(),
            Some(from_src) => self.eval_from_depth(
                from_src,
                core.where_clause.as_ref(),
                outer_scopes,
                self.query_depth,
            )?,
        };

        // 2. Apply WHERE filter (and validate WHERE column references even on 0-row tables!)
        if let Some(ref where_expr) = core.where_clause {
            if rel.rows.is_empty() {
                let mut scopes = outer_scopes.to_vec();
                let mut sc = EvalScope::simple(&rel.columns, &[]);
                sc.hidden_unqualified = Some(&rel.using_hidden);
                scopes.push(sc);
                validate_expr_columns(where_expr, &scopes, env.dqs)?;
            } else {
                let mut filtered = Vec::with_capacity(rel.rows.len());
                for row in rel.rows {
                    self.charge_steps(1)?;
                    let mut scopes = outer_scopes.to_vec();
                    let mut sc = EvalScope::simple(&rel.columns, &row);
                    sc.hidden_unqualified = Some(&rel.using_hidden);
                    scopes.push(sc);
                    let cond = eval_expr(where_expr, &scopes, &env)?;
                    if boolean_value(&cond, false) {
                        filtered.push(row);
                    }
                }
                rel.rows = filtered;
            }
        }

        // 3. Expand SELECT list (*, t.*, * EXCLUDE/REPLACE, FROM-first empty columns -> *)
        let expanded = expand_result_columns(core, &rel)?;

        // Validate SELECT expression column references even when 0 rows survive WHERE!
        if rel.rows.is_empty() {
            let mut scopes = outer_scopes.to_vec();
            let mut sc = EvalScope::simple(&rel.columns, &[]);
            sc.hidden_unqualified = Some(&rel.using_hidden);
            scopes.push(sc);
            for p in &expanded {
                validate_expr_columns(&p.expr, &scopes, env.dqs)?;
            }
        }

        // 4. Determine if this is an aggregate query (including aggregates inside named WINDOWs!)
        let has_agg_in_select = expanded
            .iter()
            .any(|p| expr_contains_aggregate(&p.expr, self.options));
        let has_agg_in_order = outer_order_by.is_some_and(|ob| {
            ob.iter()
                .any(|t| expr_contains_aggregate(&t.expr, self.options))
        });
        let has_agg_in_named_windows = core.windows.as_deref().unwrap_or(&[]).iter().any(|wd| {
            wd.window
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
                    .any(|o| expr_contains_aggregate(&o.expr, self.options))
        });
        let has_agg_in_having = core
            .having
            .as_ref()
            .is_some_and(|h| expr_contains_aggregate(h, self.options));
        let is_aggregate = core.group_by.is_some()
            || core.group_by_all
            || has_agg_in_select
            || has_agg_in_order
            || has_agg_in_named_windows
            || has_agg_in_having;

        if core.having.is_some() && !is_aggregate {
            return Err(SqlError::runtime(
                "HAVING clause on a non-aggregate query",
            ));
        }

        // 5. Grouping & Aggregation (produces working rows + per-row precomputed aggregate slots)
        let (work_rows, mut precomputed_per_row) = if is_aggregate {
            self.eval_aggregation(core, &rel, &expanded, outer_order_by, outer_scopes)?
        } else {
            let empty_pre = vec![PrecomputedRow::new(); rel.rows.len()];
            (rel.rows, empty_pre)
        };

        // 6. Window Functions (evaluated over post-GROUP BY / HAVING rows; validated even on 0 rows!)
        let has_windows = expanded.iter().any(|p| expr_contains_window(&p.expr))
            || outer_order_by.is_some_and(|ob| ob.iter().any(|t| expr_contains_window(&t.expr)));
        if has_windows {
            self.eval_windows(
                core,
                &rel.columns,
                &rel.using_hidden,
                &work_rows,
                &mut precomputed_per_row,
                &expanded,
                outer_order_by,
                outer_scopes,
            )?;
        }

        // 7. Evaluate SELECT projections (preserving source table qualifier + explicit COLLATE flag)
        let mut out_cols = Vec::with_capacity(expanded.len());
        let mut out_expl = Vec::with_capacity(expanded.len());
        for p in &expanded {
            let mut sample_scope = EvalScope::simple(&rel.columns, &[]);
            sample_scope.hidden_unqualified = Some(&rel.using_hidden);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
            let ec = expr_collation(&p.expr, &scopes);
            let coll = ec.name.unwrap_or_else(|| "BINARY".to_string());
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
            out_expl.push(ec.explicit);
        }

        let mut projected_rows = Vec::with_capacity(work_rows.len());
        let mut kept_work_rows = Vec::with_capacity(work_rows.len());
        let mut kept_pre = Vec::with_capacity(work_rows.len());
        let is_distinct = core.distinct == Some(DistinctKind::Distinct);
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
        let mut dist_dedup = RowDedup::new(&dist_coll_names, &dist_colls);
        let mut mem = self.make_mem_guard();

        for (ridx, row) in work_rows.into_iter().enumerate() {
            self.charge_steps(1)?;
            let pre = &precomputed_per_row[ridx];
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope {
                columns: &rel.columns,
                values: &row,
                precomputed: Some(&pre.slots),
                precomputed_json: Some(&pre.is_json),
                hidden_unqualified: Some(&rel.using_hidden),
                aliases: None,
                no_rowid_aliases: false,
            });
            let mut out_row = Vec::with_capacity(expanded.len());
            for p in &expanded {
                out_row.push(eval_expr(&p.expr, &scopes, &env)?);
            }
            if is_distinct && !dist_dedup.insert(&out_row, self)? {
                continue;
            }
            mem.reserve(estimate_row_bytes(&out_row))?;
            projected_rows.push(out_row);
            kept_work_rows.push(row);
            kept_pre.push(pre.clone());
        }

        let order_ctx = OrderEvalContext {
            from_columns: rel.columns,
            from_using_hidden: rel.using_hidden,
            from_rows: kept_work_rows,
            precomputed: kept_pre,
            explicit_aliases: expanded.iter().map(|p| p.explicit_alias.is_some()).collect(),
        };
        Ok((out_cols, out_expl, projected_rows, order_ctx))
    }

    // ───────────────────────────── FROM / JOINS ─────────────────────────────

    fn eval_from_depth(
        &self,
        source: &FromSource,
        where_hint_expr: Option<&Expr>,
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
                        return Ok(Relation {
                            columns,
                            star_hidden,
                            using_hidden,
                            rows: cte.rows.clone(),
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
                if let Some(tmeta) = self.catalog.get_table(schema, &ft.name.name)? {
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
                    let hint = if ft.not_indexed {
                        TableScanHint::FullScan
                    } else if let Some(we) = where_hint_expr {
                        self.extract_scan_hint(&tmeta, &alias, we, outer_scopes)
                    } else {
                        TableScanHint::FullScan
                    };
                    let scanned = self.catalog.scan_table(
                        schema,
                        &ft.name.name,
                        &hint,
                        None,
                        self.options.meter.as_deref(),
                    )?;
                    let mut columns = Vec::with_capacity(tmeta.columns.len() + 3);
                    let mut star_hidden = Vec::with_capacity(tmeta.columns.len() + 3);
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
                    // unless a real column already has that name.
                    let mut synth_rowid_slots = 0usize;
                    if tmeta.has_rowid {
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

                    let mut rows = Vec::with_capacity(scanned.len());
                    for srow in scanned {
                        let mut vals = srow.values;
                        if synth_rowid_slots > 0 {
                            let rid_val = srow.rowid.map_or(SqlValue::Null, SqlValue::Int);
                            for _ in 0..synth_rowid_slots {
                                vals.push(rid_val.clone());
                            }
                        }
                        rows.push(vals);
                    }
                    let using_hidden = vec![false; columns.len()];
                    return Ok(Relation {
                        columns,
                        star_hidden,
                        using_hidden,
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
                    return Ok(Relation {
                        columns: vcols,
                        star_hidden,
                        using_hidden,
                        rows: vrows,
                    });
                }

                // 4. Eponymous 0-arg TVF fallback (e.g. `SELECT * FROM pragma_database_list`)
                if let Some(tvf) = self.catalog.call_tvf(
                    schema,
                    &ft.name.name,
                    &[],
                    self.options.meter.as_deref(),
                )? {
                    return Ok(relation_from_tvf(&alias, tvf));
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
                if let Some(tvf) = call_builtin_tvf(&lower, &arg_vals, &env.fn_ctx)? {
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
                Ok(Relation {
                    columns: cols,
                    star_hidden,
                    using_hidden,
                    rows,
                })
            }
            FromSource::Group(fg) => {
                let mut rel = self.eval_from_depth(&fg.source, None, outer_scopes, depth + 1)?;
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
                        let lrel = self.eval_from_depth(left, None, outer_scopes, depth + 1)?;
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

                let lrel = self.eval_from_depth(left, None, outer_scopes, depth + 1)?;
                let rrel = self.eval_from_depth(right, None, outer_scopes, depth + 1)?;
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
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut mem = self.make_mem_guard();
        let lwidth = lrel.columns.len();

        for lrow in &lrel.rows {
            self.charge_steps(1)?;
            let mut scopes = outer_scopes.to_vec();
            let mut sc = EvalScope::simple(&lrel.columns, lrow);
            sc.hidden_unqualified = Some(&lrel.using_hidden);
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
                for cname in &tvf.columns {
                    cc.push(ScopeColumn {
                        table: Some(alias.clone()),
                        name: cname.clone(),
                        affinity: Affinity::None,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    });
                    uh.push(false);
                }
                combined_cols_cache = Some(cc);
                combined_using_hidden_cache = uh;
                tvf_cols = Some(tvf.columns);
            }
            let mut matched_any = false;
            for rrow in tvf.rows {
                self.charge_steps(1)?;
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.extend(rrow);
                if let Some(on_expr) = on {
                    let cc = combined_cols_cache.as_ref().expect("tvf cols built");
                    let mut on_scopes = outer_scopes.to_vec();
                    let mut on_sc = EvalScope::simple(cc, &combined);
                    on_sc.hidden_unqualified = Some(&combined_using_hidden_cache);
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
        }
        Ok(Relation {
            columns,
            star_hidden,
            using_hidden,
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
        let mut using_by_left: HashMap<usize, usize> = HashMap::new();
        for &(li, ri) in &using_pairs {
            using_by_left.insert(li, ri);
        }

        for (li, lc) in lrel.columns.iter().enumerate() {
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
                combined_cols.push(lc.clone());
                combined_star_hidden.push(lrel.star_hidden[li]);
                combined_using_hidden.push(true);
            } else {
                combined_cols.push(lc.clone());
                combined_star_hidden.push(lrel.star_hidden[li]);
                combined_using_hidden.push(lrel.using_hidden[li]);
            }
        }
        for (ri, rc) in rrel.columns.iter().enumerate() {
            combined_cols.push(rc.clone());
            combined_star_hidden.push(rrel.star_hidden[ri]);
            combined_using_hidden.push(rrel.using_hidden[ri] || hidden_rhs.contains(&ri));
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
        // with BINARY collation and compatible affinities, or single USING column).
        if let Some((lk, rk, aff, coll_name)) =
            detect_simple_equijoin(&lrel, &rrel, on, &using_pairs)
        {
            if coll_name.eq_ignore_ascii_case("BINARY") && rrel.rows.len() >= 16 {
                return self.eval_hash_equijoin(
                    lrel,
                    rrel,
                    combined_cols,
                    combined_star_hidden,
                    combined_using_hidden,
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

        let mut buckets: HashMap<HashKey, Vec<usize>> = HashMap::with_capacity(rrel.rows.len());
        for (ridx, rrow) in rrel.rows.iter().enumerate() {
            self.charge_steps(1)?;
            let rv = apply_affinity(rrow[rk].clone(), aff);
            if let Some(key) = HashKey::from_sql(&rv) {
                buckets.entry(key).or_default().push(ridx);
            }
        }

        let mut out_rows = Vec::new();
        let mut mem = self.make_mem_guard();
        let mut rhs_matched = if is_right {
            vec![false; rrel.rows.len()]
        } else {
            Vec::new()
        };

        for lrow in &lrel.rows {
            self.charge_steps(1)?;
            let lv = apply_affinity(lrow[lk].clone(), aff);
            let mut matched_any = false;
            if let Some(key) = HashKey::from_sql(&lv) {
                if let Some(indices) = buckets.get(&key) {
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
                            let combined = assemble_row(Some(lrow), Some(rrow));
                            mem.reserve(estimate_row_bytes(&combined))?;
                            out_rows.push(combined);
                        }
                    }
                }
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
            rows: out_rows,
        })
    }

    /// Inspect top-level `WHERE` conjuncts to produce a safe rowid or index seek hint.
    fn extract_scan_hint(
        &self,
        tmeta: &crate::catalog::TableMeta,
        alias: &str,
        where_expr: &Expr,
        outer_scopes: &[EvalScope<'_>],
    ) -> TableScanHint {
        let mut conjuncts = Vec::new();
        collect_and_conjuncts(where_expr, &mut conjuncts);
        let env = self.make_env();

        // 1. Check rowid / IPK range or point lookup
        if tmeta.has_rowid {
            let mut low = Bound::Unbounded;
            let mut high = Bound::Unbounded;
            let mut found_rowid_pred = false;
            for conj in &conjuncts {
                if let Expr::Binary {
                    op, left, right, ..
                } = conj
                {
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
                        if let Ok(v) = eval_expr(val_expr, outer_scopes, &env) {
                            if !v.is_null() {
                                found_rowid_pred = true;
                                match eff_op {
                                    BinaryOp::Eq => {
                                        low = Bound::Included(v.clone());
                                        high = Bound::Included(v);
                                        break;
                                    }
                                    BinaryOp::Ge => low = Bound::Included(v),
                                    BinaryOp::Gt => low = Bound::Excluded(v),
                                    BinaryOp::Le => high = Bound::Included(v),
                                    BinaryOp::Lt => high = Bound::Excluded(v),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            if found_rowid_pred {
                return TableScanHint::RowidRange { low, high };
            }
        }

        // 2. Check secondary index equality prefix on non-partial indexes.
        // Store `(value, val_affinity, val_collation, col_on_left)` so we verify both
        // `comparison_affinity` and `comparison_collation_name` match the index!
        let mut eq_by_col: HashMap<usize, (SqlValue, Option<Affinity>, crate::eval::ExprCollation, bool)> =
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
                    if let Ok(v) = eval_expr(val_expr, outer_scopes, &env) {
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
                    let col_meta = &tmeta.columns[*cid];
                    // Verify effective comparison collation matches the index key collation!
                    let col_op_coll = Some(OperandCollation {
                        name: Some(&col_meta.collation),
                        explicit: false,
                    });
                    let eff_coll = if *col_on_left {
                        comparison_collation_name(col_op_coll, val_coll.as_operand())
                    } else {
                        comparison_collation_name(val_coll.as_operand(), col_op_coll)
                    }
                    .unwrap_or("BINARY");
                    if !idx.key_collations[kpos].eq_ignore_ascii_case(eff_coll) {
                        break;
                    }
                    // Verify comparison_affinity does not override the column's index affinity!
                    // (e.g., `TEXT` col compared with `CAST(1 AS REAL)` uses NUMERIC affinity,
                    // which matches `'01'` and `'1'` and cannot be answered by a single TEXT seek).
                    let comp_aff = comparison_affinity(Some(col_meta.affinity), *val_aff);
                    if comp_aff != col_meta.affinity {
                        break;
                    }
                    prefix_vals.push(apply_affinity(v.clone(), col_meta.affinity));
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
        for wd in core.windows.as_deref().unwrap_or(&[]) {
            for e in wd.window.partition_by.as_deref().unwrap_or(&[]) {
                collect_aggregates(e, self.options, &mut agg_calls);
            }
            for o in wd.window.order_by.as_deref().unwrap_or(&[]) {
                collect_aggregates(&o.expr, self.options, &mut agg_calls);
            }
        }

        // Resolve GROUP BY expressions (including XDB `GROUP BY ALL`, ordinals + COLLATE, and aliases).
        let mut group_exprs: Vec<Expr> = Vec::new();
        if core.group_by_all {
            for p in expanded {
                if !expr_contains_aggregate(&p.expr, self.options) {
                    group_exprs.push(p.expr.clone());
                }
            }
        } else if let Some(ref gb) = core.group_by {
            for (gidx, g) in gb.iter().enumerate() {
                if let Some((ord, ov_coll)) = integer_ordinal_with_collation(g) {
                    if ord < 1 || ord > expanded.len() {
                        return Err(SqlError::runtime(format!(
                            "{} GROUP BY term out of range - should be between 1 and {}",
                            ordinal_label(gidx + 1),
                            expanded.len()
                        )));
                    }
                    let base = expanded[ord - 1].expr.clone();
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

        // Partition rows into groups.
        let mut groups: Vec<(Vec<SqlValue>, Vec<usize>)> = Vec::new();
        let sample_aliases: Vec<(String, SqlValue, Affinity, String)> = {
            let mut sample_scope = EvalScope::simple(&rel.columns, &[]);
            sample_scope.hidden_unqualified = Some(&rel.using_hidden);
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
            sample_scope.aliases = Some(&sample_aliases);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            if rel.rows.is_empty() {
                for g in &group_exprs {
                    validate_expr_columns(g, &scopes, env.dqs)?;
                }
                if let Some(ref hav) = core.having {
                    validate_expr_columns(hav, &scopes, env.dqs)?;
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
            let all_hashable = group_colls.iter().all(|c| is_hashable_collation(c));
            let mut group_index: HashMap<Vec<GroupKey>, usize> = HashMap::new();

            for (ridx, row) in rel.rows.iter().enumerate() {
                self.charge_steps(1)?;
                let mut scopes = outer_scopes.to_vec();
                let mut sc = EvalScope::simple(&rel.columns, row);
                sc.hidden_unqualified = Some(&rel.using_hidden);
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

                let mut key_vals = Vec::with_capacity(group_exprs.len());
                for g in &group_exprs {
                    key_vals.push(eval_expr(g, &scopes, &env)?);
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
                    &rel.rows,
                    &row_indices,
                    outer_scopes,
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

            mem.reserve(estimate_row_bytes(&rep_row))?;
            out_rows.push(rep_row);
            out_pre.push(pre);
        }

        Ok((out_rows, out_pre))
    }

    fn eval_single_aggregate(
        &self,
        fcall: &FunctionExpr,
        columns: &[ScopeColumn],
        using_hidden: &[bool],
        all_rows: &[Vec<SqlValue>],
        row_indices: &[usize],
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(SqlValue, Option<usize>)> {
        let env = self.make_env();
        let mut acc = self
            .options
            .functions
            .create_aggregate(&fcall.name.name, fcall.args.len(), fcall.star)?;

        // Determine collation from first argument if any.
        let mut agg_coll_name = "BINARY".to_string();
        let agg_coll: Option<&Collation> = if let Some(first_arg) = fcall.args.first() {
            let mut sample_scope = EvalScope::simple(columns, &[]);
            sample_scope.hidden_unqualified = Some(using_hidden);
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

        // 1. Collect surviving `(row_idx, arg_vals, arg_json_mask, sort_keys)` after FILTER.
        let mut surviving: Vec<(usize, Vec<SqlValue>, u64, Vec<SqlValue>)> =
            Vec::with_capacity(row_indices.len());
        let ob_terms = fcall.order_by.as_deref().unwrap_or(&[]);

        for &ridx in row_indices {
            self.charge_steps(1)?;
            let row = &all_rows[ridx];
            let mut scopes = outer_scopes.to_vec();
            let mut sc = EvalScope::simple(columns, row);
            sc.hidden_unqualified = Some(using_hidden);
            scopes.push(sc);

            if let Some(ref flt) = fcall.filter {
                let cond = eval_expr(flt, &scopes, &env)?;
                if !boolean_value(&cond, false) {
                    continue;
                }
            }
            let mut arg_vals = Vec::with_capacity(fcall.args.len());
            let mut jmask = 0u64;
            for (i, a) in fcall.args.iter().enumerate() {
                let (v, is_j) = crate::eval::eval_expr_tagged(a, &scopes, &env)?;
                if is_j && i < 64 {
                    jmask |= 1u64 << i;
                }
                arg_vals.push(v);
            }
            let mut skeys = Vec::with_capacity(ob_terms.len());
            for ot in ob_terms {
                skeys.push(eval_expr(&ot.expr, &scopes, &env)?);
            }
            surviving.push((ridx, arg_vals, jmask, skeys));
        }

        // 2. Apply aggregate ORDER BY if present
        if !ob_terms.is_empty() {
            let ob_colls: Vec<&Collation> = {
                let mut sample_scope = EvalScope::simple(columns, &[]);
                sample_scope.hidden_unqualified = Some(using_hidden);
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
            if is_hashable_collation(&agg_coll_name) {
                let mut seen: HashSet<GroupKey> = HashSet::new();
                for item in surviving {
                    self.charge_steps(1)?;
                    let v = &item.1[0];
                    if v.is_null() && !keep_null {
                        continue;
                    }
                    let gk = GroupKey::from_sql_with_coll(v, &agg_coll_name);
                    if seen.insert(gk) {
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
                        seen_vals.push(v.clone());
                        deduped.push(item);
                    }
                }
            }
            surviving = deduped;
        }

        // 4. Step accumulator
        let mut winner: Option<usize> = None;
        for (ridx, arg_vals, jmask, _) in surviving {
            self.charge_steps(1)?;
            let call_ctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, jmask);
            acc.step(&arg_vals, &call_ctx)?;
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
        rows: &[Vec<SqlValue>],
        precomputed: &mut [PrecomputedRow],
        expanded: &[ProjectedItem],
        outer_order_by: Option<&[OrderingTerm]>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<()> {
        let env = self.make_env();

        let mut win_calls: Vec<&FunctionExpr> = Vec::new();
        for p in expanded {
            collect_windows(&p.expr, &mut win_calls);
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
            if wcall.quantifier == Some(DistinctKind::Distinct) {
                return Err(SqlError::runtime(
                    "DISTINCT is not supported for window functions",
                ));
            }
            let lower_name = wcall.name.name.to_ascii_lowercase();
            let is_win_only = is_builtin_window_only(&lower_name);
            if is_win_only && wcall.filter.is_some() {
                return Err(SqlError::runtime(format!(
                    "FILTER clause may only be used with aggregate window functions: {lower_name}()"
                )));
            }
            let spec = resolve_window_ref(wref, named_windows)?;
            let part_exprs = spec.partition_by.as_deref().unwrap_or(&[]);
            let ord_terms = spec.order_by.as_deref().unwrap_or(&[]);

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

            if rows.is_empty() {
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample);
                for pe in part_exprs {
                    validate_expr_columns(pe, &scopes, env.dqs)?;
                }
                for ot in ord_terms {
                    validate_expr_columns(&ot.expr, &scopes, env.dqs)?;
                }
                continue;
            }

            let part_coll_names: Vec<String> = {
                let mut sample = EvalScope::simple(columns, &[]);
                sample.hidden_unqualified = Some(using_hidden);
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

            // Evaluate partition keys and order keys for every row.
            let mut row_pkeys: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
            let mut row_okeys: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
            for (ridx, row) in rows.iter().enumerate() {
                self.charge_steps(1)?;
                let mut scopes = outer_scopes.to_vec();
                scopes.push(EvalScope {
                    columns,
                    values: row,
                    precomputed: Some(&precomputed[ridx].slots),
                    precomputed_json: Some(&precomputed[ridx].is_json),
                    hidden_unqualified: Some(using_hidden),
                    aliases: None,
                    no_rowid_aliases: false,
                });
                let mut pk = Vec::with_capacity(part_exprs.len());
                for pe in part_exprs {
                    pk.push(eval_expr(pe, &scopes, &env)?);
                }
                let mut ok = Vec::with_capacity(ord_terms.len());
                for ot in ord_terms {
                    ok.push(eval_expr(&ot.expr, &scopes, &env)?);
                }
                row_pkeys.push(pk);
                row_okeys.push(ok);
            }

            // Group row indices into partitions (hash-based for built-in collations, metered otherwise).
            let mut partitions: Vec<Vec<usize>> = Vec::new();
            if part_exprs.is_empty() {
                partitions.push((0..rows.len()).collect());
            } else if part_coll_names.iter().all(|c| is_hashable_collation(c)) {
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

            // Resolve frame spec (evaluate constant bound offset expressions once).
            let resolved_frame = match &spec.frame {
                Some(fspec) => ResolvedFrame::from_ast(fspec, |b| match &b.expr {
                    Some(e) => Ok(Some(eval_expr(e, outer_scopes, &env)?)),
                    None => Ok(None),
                })?,
                None => ResolvedFrame::default_for(!ord_terms.is_empty()),
            };

            let first_ord_dir = ord_terms
                .first()
                .and_then(|o| o.order)
                .unwrap_or(SortOrder::Asc);
            let first_ord_coll = ord_colls.first().copied();
            let is_json_win = !is_win_only && is_json_aggregate_name(&wcall.name.name);

            // Evaluate window function on each partition
            for mut part in partitions {
                if !ord_terms.is_empty() {
                    part.sort_by(|&a, &b| {
                        compare_sort_keys(&row_okeys[a], &row_okeys[b], ord_terms, &ord_colls)
                    });
                }
                let plen = part.len();
                let peers = PartitionPeers::build(plen, |i, j| {
                    if ord_terms.is_empty() {
                        return true;
                    }
                    let ra = part[i];
                    let rb = part[j];
                    row_okeys[ra]
                        .iter()
                        .zip(row_okeys[rb].iter())
                        .enumerate()
                        .all(|(k, (a, b))| mem_compare(a, b, Some(ord_colls[k])) == Ordering::Equal)
                });
                let first_order_vals: Vec<SqlValue> = if !ord_terms.is_empty() {
                    part.iter().map(|&r| row_okeys[r][0].clone()).collect()
                } else {
                    Vec::new()
                };

                // Evaluate function args & FILTER for each row in partition.
                let mut part_args: Vec<Vec<SqlValue>> = Vec::with_capacity(plen);
                let mut part_jmasks: Vec<u64> = Vec::with_capacity(plen);
                let mut part_filter: Vec<bool> = Vec::with_capacity(plen);
                for &ridx in &part {
                    self.charge_steps(1)?;
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(EvalScope {
                        columns,
                        values: &rows[ridx],
                        precomputed: Some(&precomputed[ridx].slots),
                        precomputed_json: Some(&precomputed[ridx].is_json),
                        hidden_unqualified: Some(using_hidden),
                        aliases: None,
                        no_rowid_aliases: false,
                    });
                    let keep = match &wcall.filter {
                        Some(flt) => boolean_value(&eval_expr(flt, &scopes, &env)?, false),
                        None => true,
                    };
                    part_filter.push(keep);
                    let mut avals = Vec::with_capacity(wcall.args.len());
                    let mut jmask = 0u64;
                    for (ai, ae) in wcall.args.iter().enumerate() {
                        let (v, is_j) = crate::eval::eval_expr_tagged(ae, &scopes, &env)?;
                        if is_j && ai < 64 {
                            jmask |= 1u64 << ai;
                        }
                        avals.push(v);
                    }
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
                    for i in 0..plen {
                        self.charge_steps(1)?;
                        let frows = compute_frame_rows(
                            &resolved_frame,
                            i,
                            plen,
                            &peers,
                            &first_order_vals,
                            first_ord_dir,
                            first_ord_coll,
                        )?;
                        let mut acc = self.options.functions.create_aggregate(
                            &wcall.name.name,
                            wcall.args.len(),
                            wcall.star,
                        )?;
                        for r in frows {
                            self.charge_steps(1)?;
                            if !part_filter[r] {
                                continue;
                            }
                            let cctx = env
                                .fn_ctx
                                .with_collation_and_json_mask(agg_coll, part_jmasks[r]);
                            acc.step(&part_args[r], &cctx)?;
                        }
                        let fctx = env.fn_ctx.with_collation_and_json_mask(agg_coll, 0);
                        vals.push(acc.finalize(&fctx)?);
                    }
                    vals
                } else {
                    return Err(SqlError::runtime(format!(
                        "{}() may not be used as a window function",
                        wcall.name.name
                    )));
                };

                for (ppos, &ridx) in part.iter().enumerate() {
                    precomputed[ridx].push(
                        wcall.span.start,
                        wcall.span.end,
                        results[ppos].clone(),
                        is_json_win,
                    );
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
        order_by: Option<&[OrderingTerm]>,
        order_by_all: Option<&OrderByAll>,
        limit: Option<&LimitClause>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        let env = self.make_env();

        if let Some(oba) = order_by_all {
            let dir = oba.order.unwrap_or(SortOrder::Asc);
            let nulls = oba.nulls;
            let colls: Vec<&Collation> = out_cols
                .iter()
                .map(|c| env.lookup_collation(&c.collation))
                .collect::<SqlResult<Vec<_>>>()?;
            rows.sort_by(|a, b| {
                for i in 0..out_cols.len() {
                    let ord = compare_ordered_val(&a[i], &b[i], dir, nulls, colls[i]);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        } else if let Some(terms) = order_by {
            if !terms.is_empty() {
                let mut term_colls: Vec<&Collation> = Vec::with_capacity(terms.len());

                // Always validate ORDER BY ordinals & expressions even when `rows.len() <= 1`!
                let sample_aliases: Vec<(String, SqlValue, Affinity, String)> = out_cols
                    .iter()
                    .map(|c| (c.name.clone(), SqlValue::Null, c.affinity, c.collation.clone()))
                    .collect();
                for (tidx, term) in terms.iter().enumerate() {
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
                    } else {
                        // Use SQLite's exact `resolveAsName` precedence:
                        // 1. Bare unqualified name matching an EXPLICIT `AS` alias in SELECT wins first.
                        // 2. Otherwise FROM scope (with output aliases as fallback), then output scope.
                        let mut cname_opt = None;
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
                                }
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
                        // Validate column resolution on 0/1-row results too!
                        if rows.len() == 1 {
                            let _ = self.eval_order_term(
                                &term.expr,
                                out_cols,
                                &rows[0],
                                order_ctx,
                                0,
                                outer_scopes,
                                &env,
                            )?;
                        } else if rows.is_empty() {
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

                if rows.len() > 1 {
                    let mut mem = self.make_mem_guard();
                    let mut key_rows: Vec<(usize, Vec<SqlValue>)> = Vec::with_capacity(rows.len());
                    for (ridx, row) in rows.iter().enumerate() {
                        self.charge_steps(1)?;
                        let mut keys = Vec::with_capacity(terms.len());
                        for term in terms {
                            if let Some((ord, _)) = integer_ordinal_with_collation(&term.expr) {
                                keys.push(row[ord - 1].clone());
                            } else {
                                let val = self.eval_order_term(
                                    &term.expr,
                                    out_cols,
                                    row,
                                    order_ctx,
                                    ridx,
                                    outer_scopes,
                                    &env,
                                )?;
                                keys.push(val);
                            }
                        }
                        mem.reserve(estimate_row_bytes(&keys) + 16)?;
                        key_rows.push((ridx, keys));
                    }

                    key_rows.sort_by(|(_, ka), (_, kb)| {
                        compare_sort_keys(ka, kb, terms, &term_colls)
                    });

                    let mut sorted = Vec::with_capacity(rows.len());
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
        env: &EvalEnv<'_>,
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
                aliases: Some(&aliases),
                no_rowid_aliases: false,
            });
            if validate_expr_columns(expr, &scopes, env.dqs).is_ok() {
                return Ok(());
            }
        }
        let mut scopes = outer_scopes.to_vec();
        scopes.push(EvalScope::simple(out_cols, dummy_out_row));
        validate_expr_columns(expr, &scopes, env.dqs)
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
            let aliases: Vec<(String, SqlValue, Affinity, String)> = out_cols
                .iter()
                .zip(out_row.iter())
                .map(|(c, v)| (c.name.clone(), v.clone(), c.affinity, c.collation.clone()))
                .collect();
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope {
                columns: &ctx.from_columns,
                values: &ctx.from_rows[ridx],
                precomputed: Some(&ctx.precomputed[ridx].slots),
                precomputed_json: Some(&ctx.precomputed[ridx].is_json),
                hidden_unqualified: Some(&ctx.from_using_hidden),
                aliases: Some(&aliases),
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
        let (cols, expl, rows) = self.execute_select_stmt_meta(stmt, outer_scopes)?;
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
        let colls = cols
            .iter()
            .zip(expl.iter())
            .map(|(c, &is_explicit)| {
                if is_explicit {
                    ExprCollation::explicit(c.collation.clone())
                } else if !c.collation.eq_ignore_ascii_case("BINARY") {
                    ExprCollation::implicit(c.collation.clone())
                } else {
                    ExprCollation::none()
                }
            })
            .collect();
        Ok((names, affs, colls, rows))
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
                return Ok((affs, colls, cte.rows.clone()));
            }
        }
        let tmeta = self.catalog.get_table(schema, table)?;
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
    from_rows: Vec<Vec<SqlValue>>,
    precomputed: Vec<PrecomputedRow>,
    explicit_aliases: Vec<bool>,
}

struct ProjectedItem {
    expr: Expr,
    output_name: String,
    explicit_alias: Option<String>,
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
                out.push(ProjectedItem {
                    expr: re.expr.clone(),
                    output_name,
                    explicit_alias: re.alias.as_ref().map(|a| a.name.clone()),
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
    Relation {
        columns,
        star_hidden,
        using_hidden,
        rows: tvf.rows,
    }
}

/// Parse an integer ordinal term (`1`, `2`, `1 COLLATE NOCASE`).
/// Overflowing integer literals return `Some((usize::MAX, coll))` so the caller emits
/// SQLite's exact `"Nth ORDER BY/GROUP BY term out of range"` error instead of evaluating a constant!
fn integer_ordinal_with_collation(expr: &Expr) -> Option<(usize, Option<&str>)> {
    match expr {
        Expr::Number(n) if !n.is_float && !n.is_hex => {
            let ord = n.text.parse::<usize>().unwrap_or(usize::MAX);
            Some((ord, None))
        }
        Expr::Collate {
            operand, collation, ..
        } => {
            if let Expr::Number(n) = operand.as_ref() {
                if !n.is_float && !n.is_hex {
                    let ord = n.text.parse::<usize>().unwrap_or(usize::MAX);
                    return Some((ord, Some(collation.name.as_str())));
                }
            }
            None
        }
        _ => None,
    }
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
        Expr::InSelect { operand, .. } | Expr::InTable { operand, .. } => {
            collect_unqualified_col_names(operand, out);
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
        }
        Expr::Vector { items, .. } => {
            for i in items {
                collect_unqualified_col_names(i, out);
            }
        }
        _ => {}
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

fn is_hashable_collation(name: &str) -> bool {
    name.eq_ignore_ascii_case("BINARY")
        || name.eq_ignore_ascii_case("NOCASE")
        || name.eq_ignore_ascii_case("RTRIM")
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

/// Collation-aware row deduplication set: O(1) hashed for BINARY/NOCASE/RTRIM,
/// metered linear comparison for custom user collations.
struct RowDedup<'a> {
    coll_names: Vec<&'a str>,
    colls: Vec<&'a Collation>,
    hashable: bool,
    hashed: HashSet<Vec<GroupKey>>,
    linear: Vec<Vec<SqlValue>>,
}

impl<'a> RowDedup<'a> {
    fn new(coll_names: &[&'a str], colls: &[&'a Collation]) -> Self {
        let hashable = coll_names.iter().all(|c| is_hashable_collation(c));
        Self {
            coll_names: coll_names.to_vec(),
            colls: colls.to_vec(),
            hashable,
            hashed: HashSet::new(),
            linear: Vec::new(),
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
            return Ok(self.hashed.insert(self.key_for(row)));
        }
        if self.contains(row, exec)? {
            return Ok(false);
        }
        self.linear.push(row.to_vec());
        Ok(true)
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
        FromSource::Subquery(fs) => select_body_references_table(&fs.select.body, name),
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
            select_body_references_table(&select.body, name)
        }
        Expr::InSelect { operand, select, .. } => {
            expr_references_table(operand, name)
                || select_body_references_table(&select.body, name)
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

fn compare_ordered_val(
    a: &SqlValue,
    b: &SqlValue,
    dir: SortOrder,
    nulls: Option<NullsOrder>,
    coll: &Collation,
) -> Ordering {
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
        let wt = c.table.as_ref().map(|n| n.name.as_str());
        let mut found = None;
        for (i, sc) in rel.columns.iter().enumerate() {
            if let Some(t) = wt {
                if sc.table.as_deref().is_none_or(|st| !st.eq_ignore_ascii_case(t)) {
                    continue;
                }
            }
            if sc.name.eq_ignore_ascii_case(&c.column.name) {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
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
                if let Some(iv) = real_to_exact_i64(*r) {
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
                if let Some(iv) = real_to_exact_i64(*r) {
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
