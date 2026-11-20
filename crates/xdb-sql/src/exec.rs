//! Full SQLite 3.54.1 + XDB dialect `SELECT` query planner and executor.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use xdb_core::value::{
    apply_affinity, boolean_value, compare_op, comparison_affinity, comparison_collation_name,
    int_value, mem_compare, Collation, CompareOp, OperandCollation,
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
    expr_contains_aggregate, expr_contains_window, is_valid_schema_name, EvalEnv, EvalScope,
    ScopeColumn, SubqueryRunner,
};
use crate::functions::{
    call_builtin_tvf, compute_frame_rows, eval_window_only_partition, is_builtin_window_only,
    PartitionPeers, ResolvedFrame,
};

/// Materialized intermediate relation during query execution.
#[derive(Debug, Clone)]
struct Relation {
    columns: Vec<ScopeColumn>,
    /// Columns hidden from `SELECT *` (such as hidden table columns or right-hand `USING`/`NATURAL` duplicates).
    star_hidden: Vec<bool>,
    rows: Vec<Vec<SqlValue>>,
}

type PrecomputedSlots = Vec<(usize, usize, SqlValue)>;

impl Relation {
    fn single_empty_row() -> Self {
        Self {
            columns: Vec::new(),
            star_hidden: Vec::new(),
            rows: vec![Vec::new()],
        }
    }
}

/// Materialized CTE entry visible in the current query scope.
#[derive(Debug, Clone)]
struct CteTable {
    columns: Vec<String>,
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
}

impl SubqueryRunner for CatalogSubqueryRunner<'_> {
    fn run_select(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<String>, Vec<Vec<SqlValue>>)> {
        self.exec.run_select(stmt, outer_scopes)
    }

    fn run_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        self.exec.run_in_table(schema, table, args)
    }
}

/// Execution context holding catalog, options, parameters, and active CTE bindings.
struct QueryExec<'a> {
    catalog: &'a dyn SqlCatalog,
    options: &'a QueryOptions,
    params: &'a QueryParams,
    ctes: HashMap<String, CteTable>,
    view_depth: usize,
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
        }
    }

    fn child_with_ctes(&self, ctes: HashMap<String, CteTable>) -> Self {
        Self {
            catalog: self.catalog,
            options: self.options,
            params: self.params,
            ctes,
            view_depth: self.view_depth,
        }
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
                let col_names = apply_cte_column_names(cte, &cols)?;
                CteTable {
                    columns: col_names,
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
        // Check if body is `base UNION [ALL] recursive_step`.
        if let SelectBody::Compound {
            op, left, right, ..
        } = &cte.select.body
        {
            if matches!(op, CompoundOperator::Union | CompoundOperator::UnionAll) {
                let is_distinct = *op == CompoundOperator::Union;
                let base_stmt = SelectStmt {
                    span: left.span(),
                    with: None,
                    body: (**left).clone(),
                    order_by: None,
                    order_by_all: None,
                    limit: None,
                };
                let step_stmt = SelectStmt {
                    span: right.span(),
                    with: None,
                    body: (**right).clone(),
                    order_by: None,
                    order_by_all: None,
                    limit: None,
                };
                let base_runner = self.child_with_ctes(existing.clone());
                let (base_cols, base_rows) =
                    base_runner.execute_select_stmt(&base_stmt, outer_scopes)?;
                let col_names = apply_cte_column_names(cte, &base_cols)?;
                let width = col_names.len();

                let mut all_rows: Vec<Vec<SqlValue>> = Vec::new();
                let mut seen: Vec<Vec<SqlValue>> = Vec::new();
                let mut frontier: Vec<Vec<SqlValue>> = Vec::new();

                for row in base_rows {
                    if row.len() != width {
                        return Err(SqlError::runtime(
                            "SELECTs to the left and right of UNION do not have the same number of result columns",
                        ));
                    }
                    if is_distinct && contains_row_binary(&seen, &row) {
                        continue;
                    }
                    if is_distinct {
                        seen.push(row.clone());
                    }
                    frontier.push(row.clone());
                    all_rows.push(row);
                }

                // Determine optional LIMIT/OFFSET cap on the recursive CTE itself when no ORDER BY.
                let early_stop_rows = if cte.select.order_by.is_none()
                    && cte.select.order_by_all.is_none()
                {
                    if let Some(ref lim) = cte.select.limit {
                        let env = self.make_env();
                        let count = int_value(&eval_expr(&lim.count, outer_scopes, &env)?);
                        let offset = match &lim.offset {
                            Some(o) => int_value(&eval_expr(o, outer_scopes, &env)?).max(0),
                            None => 0,
                        };
                        if count >= 0 {
                            Some((count.saturating_add(offset)) as usize)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let key = cte.name.name.to_ascii_lowercase();
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
                            columns: col_names.clone(),
                            rows: std::mem::take(&mut frontier),
                        },
                    );
                    let step_runner = self.child_with_ctes(step_map);
                    let (_, new_rows) =
                        step_runner.execute_select_stmt(&step_stmt, outer_scopes)?;

                    for row in new_rows {
                        self.charge_steps(1)?;
                        if row.len() != width {
                            return Err(SqlError::runtime(
                                "SELECTs to the left and right of UNION do not have the same number of result columns",
                            ));
                        }
                        if is_distinct && contains_row_binary(&seen, &row) {
                            continue;
                        }
                        if is_distinct {
                            seen.push(row.clone());
                        }
                        frontier.push(row.clone());
                        all_rows.push(row);
                        if let Some(cap) = early_stop_rows {
                            if all_rows.len() >= cap {
                                break;
                            }
                        }
                    }
                }

                // Apply CTE's ORDER BY / LIMIT if present.
                let scope_cols: Vec<ScopeColumn> = col_names
                    .iter()
                    .map(|n| ScopeColumn {
                        table: None,
                        name: n.clone(),
                        affinity: Affinity::Blob,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    })
                    .collect();
                let final_rows = self.apply_order_and_limit(
                    &scope_cols,
                    all_rows,
                    None,
                    cte.select.order_by.as_deref(),
                    cte.select.order_by_all.as_ref(),
                    cte.select.limit.as_ref(),
                    outer_scopes,
                )?;
                return Ok(CteTable {
                    columns: col_names,
                    rows: final_rows,
                });
            }
        }

        // Non-compound CTE under WITH RECURSIVE: execute normally.
        let runner = self.child_with_ctes(existing.clone());
        let (cols, rows) = runner.execute_select_stmt(&cte.select, outer_scopes)?;
        let col_names = apply_cte_column_names(cte, &cols)?;
        Ok(CteTable {
            columns: col_names,
            rows,
        })
    }

    fn execute_select_no_with(
        &self,
        stmt: &SelectStmt,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        match &stmt.body {
            SelectBody::Core(core) => {
                // Keep FROM-scope info available for ORDER BY resolution in single-core queries.
                let (out_cols, out_rows, order_ctx) =
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
                Ok((out_cols, final_rows))
            }
            SelectBody::Values(_) | SelectBody::Compound { .. } => {
                let (out_cols, out_rows) = self.execute_select_body(&stmt.body, outer_scopes)?;
                let final_rows = self.apply_order_and_limit(
                    &out_cols,
                    out_rows,
                    None,
                    stmt.order_by.as_deref(),
                    stmt.order_by_all.as_ref(),
                    stmt.limit.as_ref(),
                    outer_scopes,
                )?;
                Ok((out_cols, final_rows))
            }
        }
    }

    fn execute_select_body(
        &self,
        body: &SelectBody,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        match body {
            SelectBody::Core(core) => {
                let (cols, rows, _) = self.execute_select_core(core, outer_scopes, None)?;
                Ok((cols, rows))
            }
            SelectBody::Values(values) => {
                let env = self.make_env();
                let mut rows = Vec::with_capacity(values.rows.len());
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
                    rows.push(vals);
                }
                let w = width.unwrap_or(0);
                let cols = (1..=w)
                    .map(|i| ScopeColumn {
                        table: None,
                        name: format!("column{i}"),
                        affinity: Affinity::Blob,
                        collation: "BINARY".to_string(),
                        is_rowid_alias: false,
                    })
                    .collect();
                Ok((cols, rows))
            }
            SelectBody::Compound {
                op, left, right, ..
            } => {
                let (lcols, lrows) = self.execute_select_body(left, outer_scopes)?;
                let (rcols, rrows) = self.execute_select_body(right, outer_scopes)?;
                if lcols.len() != rcols.len() {
                    return Err(SqlError::runtime(format!(
                        "SELECTs to the left and right of {} do not have the same number of result columns",
                        op.as_str()
                    )));
                }
                let out_rows = match op {
                    CompoundOperator::UnionAll => {
                        let mut out = lrows;
                        out.extend(rrows);
                        out
                    }
                    CompoundOperator::Union => {
                        let mut out = Vec::new();
                        for r in lrows.into_iter().chain(rrows) {
                            self.charge_steps(1)?;
                            if !contains_row_binary(&out, &r) {
                                out.push(r);
                            }
                        }
                        out
                    }
                    CompoundOperator::Intersect => {
                        let mut out = Vec::new();
                        for r in lrows {
                            self.charge_steps(1)?;
                            if contains_row_binary(&rrows, &r) && !contains_row_binary(&out, &r) {
                                out.push(r);
                            }
                        }
                        out
                    }
                    CompoundOperator::Except => {
                        let mut out = Vec::new();
                        for r in lrows {
                            self.charge_steps(1)?;
                            if !contains_row_binary(&rrows, &r) && !contains_row_binary(&out, &r) {
                                out.push(r);
                            }
                        }
                        out
                    }
                };
                Ok((lcols, out_rows))
            }
        }
    }

    fn execute_select_core(
        &self,
        core: &SelectCore,
        outer_scopes: &[EvalScope<'_>],
        outer_order_by: Option<&[OrderingTerm]>,
    ) -> SqlResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>, OrderEvalContext)> {
        let env = self.make_env();

        // 1. Evaluate FROM clause (with optional single-table WHERE pushdown)
        let mut rel = match &core.from {
            None => Relation::single_empty_row(),
            Some(from_src) => {
                self.eval_from(from_src, core.where_clause.as_ref(), outer_scopes)?
            }
        };

        // 2. Apply WHERE filter
        if let Some(ref where_expr) = core.where_clause {
            let mut filtered = Vec::with_capacity(rel.rows.len());
            for row in rel.rows {
                self.charge_steps(1)?;
                let mut scopes = outer_scopes.to_vec();
                scopes.push(EvalScope::simple(&rel.columns, &row));
                let cond = eval_expr(where_expr, &scopes, &env)?;
                if boolean_value(&cond, false) {
                    filtered.push(row);
                }
            }
            rel.rows = filtered;
        }

        // 3. Expand SELECT list (*, t.*, * EXCLUDE/REPLACE, FROM-first empty columns -> *)
        let expanded = expand_result_columns(core, &rel)?;

        // 4. Determine if this is an aggregate query
        let has_agg_in_select = expanded
            .iter()
            .any(|p| expr_contains_aggregate(&p.expr, self.options));
        let has_agg_in_order = outer_order_by.is_some_and(|ob| {
            ob.iter()
                .any(|t| expr_contains_aggregate(&t.expr, self.options))
        });
        let is_aggregate = core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || has_agg_in_select
            || has_agg_in_order;

        // 5. Grouping & Aggregation (produces working rows + per-row precomputed aggregate slots)
        let (work_rows, mut precomputed_per_row) = if is_aggregate {
            self.eval_aggregation(core, &rel, &expanded, outer_order_by, outer_scopes)?
        } else {
            let empty_pre = vec![Vec::new(); rel.rows.len()];
            (rel.rows, empty_pre)
        };

        // 6. Window Functions (evaluated over post-GROUP BY / HAVING rows!)
        let has_windows = expanded.iter().any(|p| expr_contains_window(&p.expr))
            || outer_order_by.is_some_and(|ob| ob.iter().any(|t| expr_contains_window(&t.expr)));
        if has_windows {
            self.eval_windows(
                core,
                &rel.columns,
                &work_rows,
                &mut precomputed_per_row,
                &expanded,
                outer_order_by,
                outer_scopes,
            )?;
        }

        // 7. Evaluate SELECT projections
        let out_cols: Vec<ScopeColumn> = expanded
            .iter()
            .map(|p| {
                let sample_scope = EvalScope::simple(&rel.columns, &[]);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample_scope);
                let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                let coll = expr_collation(&p.expr, &scopes)
                    .name
                    .unwrap_or_else(|| "BINARY".to_string());
                ScopeColumn {
                    table: None,
                    name: p.output_name.clone(),
                    affinity: aff,
                    collation: coll,
                    is_rowid_alias: false,
                }
            })
            .collect();

        let mut projected_rows = Vec::with_capacity(work_rows.len());
        let mut kept_work_rows = Vec::with_capacity(work_rows.len());
        let mut kept_pre = Vec::with_capacity(work_rows.len());
        let is_distinct = core.distinct == Some(DistinctKind::Distinct);

        for (ridx, row) in work_rows.into_iter().enumerate() {
            self.charge_steps(1)?;
            let pre = &precomputed_per_row[ridx];
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope {
                columns: &rel.columns,
                values: &row,
                precomputed: Some(pre),
                aliases: None,
                no_rowid_aliases: false,
            });
            let mut out_row = Vec::with_capacity(expanded.len());
            for p in &expanded {
                out_row.push(eval_expr(&p.expr, &scopes, &env)?);
            }
            if is_distinct && contains_row_binary(&projected_rows, &out_row) {
                continue;
            }
            projected_rows.push(out_row);
            kept_work_rows.push(row);
            kept_pre.push(pre.clone());
        }

        let order_ctx = OrderEvalContext {
            from_columns: rel.columns,
            from_rows: kept_work_rows,
            precomputed: kept_pre,
        };
        Ok((out_cols, projected_rows, order_ctx))
    }

    // ───────────────────────────── FROM / JOINS ─────────────────────────────

    fn eval_from(
        &self,
        source: &FromSource,
        where_hint_expr: Option<&Expr>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Relation> {
        match source {
            FromSource::Table(ft) => {
                let schema = ft.schema.as_ref().map(|n| n.name.as_str());
                let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();

                // 1. Check CTEs first (only when unqualified)
                if schema.is_none() {
                    if let Some(cte) = self.ctes.get(&ft.name.name.to_ascii_lowercase()) {
                        let columns = cte
                            .columns
                            .iter()
                            .map(|c| ScopeColumn {
                                table: Some(alias.clone()),
                                name: c.clone(),
                                affinity: Affinity::Blob,
                                collation: "BINARY".to_string(),
                                is_rowid_alias: false,
                            })
                            .collect();
                        let star_hidden = vec![false; cte.columns.len()];
                        return Ok(Relation {
                            columns,
                            star_hidden,
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
                    return Ok(Relation {
                        columns,
                        star_hidden,
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
                    return Ok(Relation {
                        columns: vcols,
                        star_hidden,
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
                Ok(Relation {
                    columns: cols,
                    star_hidden,
                    rows,
                })
            }
            FromSource::Group(fg) => {
                let mut rel = self.eval_from(&fg.source, None, outer_scopes)?;
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
                // Support correlated table-valued function on RHS (e.g. `FROM t, json_each(t.j)` or `LEFT JOIN json_each(t.j)`)
                if let FromSource::Function(ff) = right.as_ref() {
                    if on.is_none()
                        && using.is_none()
                        && (operator.flags & join_flag::RIGHT) == 0
                        && (operator.flags & join_flag::NATURAL) == 0
                        && ff.args.iter().any(expr_has_column_ref)
                    {
                        let lrel = self.eval_from(left, None, outer_scopes)?;
                        let is_left = (operator.flags & join_flag::LEFT) != 0;
                        return self.eval_correlated_tvf_join(lrel, ff, is_left, outer_scopes);
                    }
                }

                let lrel = self.eval_from(left, None, outer_scopes)?;
                let rrel = self.eval_from(right, None, outer_scopes)?;
                self.eval_join(lrel, rrel, *operator, on.as_deref(), using.as_deref(), outer_scopes)
            }
        }
    }

    fn eval_correlated_tvf_join(
        &self,
        lrel: Relation,
        ff: &xdb_parser::FromFunction,
        is_left: bool,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<Relation> {
        let env = self.make_env();
        let schema = ff.schema.as_ref().map(|n| n.name.as_str());
        let alias = ff.alias.as_ref().unwrap_or(&ff.name).name.clone();
        let lower = ff.name.name.to_ascii_lowercase();

        // Determine TVF output schema by evaluating a dummy or first call.
        let mut tvf_cols: Option<Vec<String>> = None;
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::new();
        let lwidth = lrel.columns.len();

        for lrow in &lrel.rows {
            self.charge_steps(1)?;
            let mut scopes = outer_scopes.to_vec();
            scopes.push(EvalScope::simple(&lrel.columns, lrow));
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
                tvf_cols = Some(tvf.columns);
            }
            if tvf.rows.is_empty() {
                if is_left {
                    let mut combined = Vec::with_capacity(lwidth + rwidth);
                    combined.extend_from_slice(lrow);
                    combined.resize(lwidth + rwidth, SqlValue::Null);
                    out_rows.push(combined);
                }
            } else {
                for rrow in tvf.rows {
                    let mut combined = Vec::with_capacity(lwidth + rwidth);
                    combined.extend_from_slice(lrow);
                    combined.extend(rrow);
                    out_rows.push(combined);
                }
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
        for c in r_col_names {
            columns.push(ScopeColumn {
                table: Some(alias.clone()),
                name: c,
                affinity: Affinity::Blob,
                collation: "BINARY".to_string(),
                is_rowid_alias: false,
            });
            star_hidden.push(false);
        }
        Ok(Relation {
            columns,
            star_hidden,
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
                if lc.is_rowid_alias || lrel.star_hidden[li] {
                    continue;
                }
                if let Some((ri, _)) = rrel.columns.iter().enumerate().find(|(ri, rc)| {
                    !rc.is_rowid_alias
                        && !rrel.star_hidden[*ri]
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
                    .find(|(_, c)| !c.is_rowid_alias && c.name.eq_ignore_ascii_case(&uname.name))
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
                    .find(|(_, c)| !c.is_rowid_alias && c.name.eq_ignore_ascii_case(&uname.name))
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

        let lwidth = lrel.columns.len();
        let rwidth = rrel.columns.len();
        let mut combined_cols = Vec::with_capacity(lwidth + rwidth);
        combined_cols.extend_from_slice(&lrel.columns);
        combined_cols.extend_from_slice(&rrel.columns);

        let mut combined_hidden = Vec::with_capacity(lwidth + rwidth);
        combined_hidden.extend_from_slice(&lrel.star_hidden);
        for (ri, &h) in rrel.star_hidden.iter().enumerate() {
            combined_hidden.push(h || hidden_rhs.contains(&ri));
        }

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
                    combined_hidden,
                    lk,
                    rk,
                    aff,
                    is_left,
                    is_right,
                    &using_pairs,
                );
            }
        }

        // General nested-loop join (handles arbitrary ON expressions, collations, FULL OUTER, etc.)
        let mut out_rows: Vec<Vec<SqlValue>> = Vec::new();
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
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.extend_from_slice(rrow);

                if let Some(on_expr) = on {
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(EvalScope::simple(&combined_cols, &combined));
                    let cond = eval_expr(on_expr, &scopes, &env)?;
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }

                matched_any = true;
                if is_right {
                    rhs_matched[ridx] = true;
                }
                out_rows.push(combined);
            }
            if is_left && !matched_any {
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.resize(lwidth + rwidth, SqlValue::Null);
                out_rows.push(combined);
            }
        }

        if is_right {
            for (ridx, rrow) in rrel.rows.iter().enumerate() {
                if !rhs_matched[ridx] {
                    self.charge_steps(1)?;
                    let mut combined = vec![SqlValue::Null; lwidth + rwidth];
                    combined[lwidth..].clone_from_slice(rrow);
                    // For RIGHT/FULL OUTER JOIN USING, populate the coalesced left USING column!
                    for &(li, ri) in &using_pairs {
                        combined[li] = rrow[ri].clone();
                    }
                    out_rows.push(combined);
                }
            }
        }

        Ok(Relation {
            columns: combined_cols,
            star_hidden: combined_hidden,
            rows: out_rows,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_hash_equijoin(
        &self,
        lrel: Relation,
        rrel: Relation,
        combined_cols: Vec<ScopeColumn>,
        combined_hidden: Vec<bool>,
        lk: usize,
        rk: usize,
        aff: Affinity,
        is_left: bool,
        is_right: bool,
        using_pairs: &[(usize, usize)],
    ) -> SqlResult<Relation> {
        let lwidth = lrel.columns.len();
        let rwidth = rrel.columns.len();
        let mut buckets: HashMap<HashKey, Vec<usize>> = HashMap::with_capacity(rrel.rows.len());
        for (ridx, rrow) in rrel.rows.iter().enumerate() {
            self.charge_steps(1)?;
            let rv = apply_affinity(rrow[rk].clone(), aff);
            if let Some(key) = HashKey::from_sql(&rv) {
                buckets.entry(key).or_default().push(ridx);
            }
        }

        let mut out_rows = Vec::new();
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
                            let mut combined = Vec::with_capacity(lwidth + rwidth);
                            combined.extend_from_slice(lrow);
                            combined.extend_from_slice(rrow);
                            out_rows.push(combined);
                        }
                    }
                }
            }
            if is_left && !matched_any {
                let mut combined = Vec::with_capacity(lwidth + rwidth);
                combined.extend_from_slice(lrow);
                combined.resize(lwidth + rwidth, SqlValue::Null);
                out_rows.push(combined);
            }
        }

        if is_right {
            for (ridx, rrow) in rrel.rows.iter().enumerate() {
                if !rhs_matched[ridx] {
                    let mut combined = vec![SqlValue::Null; lwidth + rwidth];
                    combined[lwidth..].clone_from_slice(rrow);
                    for &(li, ri) in using_pairs {
                        combined[li] = rrow[ri].clone();
                    }
                    out_rows.push(combined);
                }
            }
        }

        Ok(Relation {
            columns: combined_cols,
            star_hidden: combined_hidden,
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
                        if is_rowid_column_ref(left, tmeta, alias) && !expr_has_column_ref(right) {
                            (true, right.as_ref(), *op)
                        } else if is_rowid_column_ref(right, tmeta, alias)
                            && !expr_has_column_ref(left)
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

        // 2. Check secondary index equality prefix on non-partial indexes
        let mut eq_by_col: HashMap<usize, SqlValue> = HashMap::new();
        for conj in &conjuncts {
            if let Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
                ..
            } = conj
            {
                let pair = if let Some(cid) = match_table_col_ref(left, tmeta, alias) {
                    if !expr_has_column_ref(right) {
                        Some((cid, right.as_ref()))
                    } else {
                        None
                    }
                } else if let Some(cid) = match_table_col_ref(right, tmeta, alias) {
                    if !expr_has_column_ref(left) {
                        Some((cid, left.as_ref()))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some((cid, val_expr)) = pair {
                    if let Ok(v) = eval_expr(val_expr, outer_scopes, &env) {
                        if !v.is_null() {
                            eq_by_col.entry(cid).or_insert(v);
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
                    // Only push down when index key collation matches column collation
                    if !idx.key_collations[kpos]
                        .eq_ignore_ascii_case(&tmeta.columns[*cid].collation)
                    {
                        break;
                    }
                    if let Some(v) = eq_by_col.get(cid) {
                        prefix_vals.push(apply_affinity(v.clone(), tmeta.columns[*cid].affinity));
                    } else {
                        break;
                    }
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
    ) -> SqlResult<(Vec<Vec<SqlValue>>, Vec<PrecomputedSlots>)> {
        let env = self.make_env();

        // Collect all aggregate calls from SELECT, HAVING, and ORDER BY.
        let mut agg_calls: Vec<&FunctionExpr> = Vec::new();
        for p in expanded {
            collect_aggregates(&p.expr, self.options, &mut agg_calls);
        }
        if let Some(ref hav) = core.having {
            collect_aggregates(hav, self.options, &mut agg_calls);
        }
        if let Some(ob) = outer_order_by {
            for t in ob {
                collect_aggregates(&t.expr, self.options, &mut agg_calls);
            }
        }

        // Resolve GROUP BY expressions (including XDB `GROUP BY ALL`, ordinals, and aliases).
        let mut group_exprs: Vec<Expr> = Vec::new();
        if core.group_by_all {
            for p in expanded {
                if !expr_contains_aggregate(&p.expr, self.options) {
                    group_exprs.push(p.expr.clone());
                }
            }
        } else if let Some(ref gb) = core.group_by {
            for g in gb {
                if let Some(ord) = integer_ordinal(g) {
                    if ord < 1 || ord > expanded.len() {
                        return Err(SqlError::runtime(format!(
                            "1st GROUP BY term out of range - should be between 1 and {}",
                            expanded.len()
                        )));
                    }
                    group_exprs.push(expanded[ord - 1].expr.clone());
                } else {
                    group_exprs.push(g.clone());
                }
            }
        }

        // Partition rows into groups.
        let mut groups: Vec<(Vec<SqlValue>, Vec<usize>)> = Vec::new();
        let group_colls: Vec<String> = {
            let sample_scope = EvalScope::simple(&rel.columns, &[]);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            group_exprs
                .iter()
                .map(|g| {
                    expr_collation(g, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string())
                })
                .collect()
        };

        if group_exprs.is_empty() {
            // Single global group (always 1 group even if 0 input rows, unless HAVING filters it!).
            let indices: Vec<usize> = (0..rel.rows.len()).collect();
            groups.push((Vec::new(), indices));
        } else {
            let all_binary = group_colls
                .iter()
                .all(|c| c.eq_ignore_ascii_case("BINARY"));
            let mut group_index: HashMap<Vec<GroupKey>, usize> = HashMap::new();

            for (ridx, row) in rel.rows.iter().enumerate() {
                self.charge_steps(1)?;
                let mut scopes = outer_scopes.to_vec();
                scopes.push(EvalScope::simple(&rel.columns, row));

                // Also expose SELECT aliases for GROUP BY expressions that reference an alias!
                let alias_vals: Vec<(String, SqlValue, Affinity, String)> = expanded
                    .iter()
                    .filter_map(|p| {
                        let alias = p.explicit_alias.as_ref()?;
                        if expr_contains_aggregate(&p.expr, self.options) {
                            return None;
                        }
                        let v = eval_expr(&p.expr, &scopes, &env).ok()?;
                        let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                        let coll = expr_collation(&p.expr, &scopes)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        Some((alias.clone(), v, aff, coll))
                    })
                    .collect();
                if let Some(last) = scopes.last_mut() {
                    last.aliases = Some(&alias_vals);
                }

                let mut key_vals = Vec::with_capacity(group_exprs.len());
                for g in &group_exprs {
                    key_vals.push(eval_expr(g, &scopes, &env)?);
                }

                if all_binary {
                    let hkey: Vec<GroupKey> = key_vals.iter().map(GroupKey::from_sql).collect();
                    if let Some(&gid) = group_index.get(&hkey) {
                        groups[gid].1.push(ridx);
                    } else {
                        let gid = groups.len();
                        group_index.insert(hkey, gid);
                        groups.push((key_vals, vec![ridx]));
                    }
                } else {
                    let mut found = None;
                    for (gid, (gvals, _)) in groups.iter().enumerate() {
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
                        groups.push((key_vals, vec![ridx]));
                    }
                }
            }
        }

        // Check SQLite single min()/max() bare-column rule:
        // If there is exactly 1 aggregate call and it is 1-arg `min` or `max`, non-aggregated
        // bare columns in SELECT/HAVING come from the row that produced the min/max!
        let single_minmax = agg_calls.len() == 1
            && (agg_calls[0].name.name.eq_ignore_ascii_case("min")
                || agg_calls[0].name.name.eq_ignore_ascii_case("max"))
            && agg_calls[0].args.len() == 1
            && !agg_calls[0].star;

        let mut out_rows = Vec::with_capacity(groups.len());
        let mut out_pre = Vec::with_capacity(groups.len());

        for (_, row_indices) in groups {
            self.charge_steps(1)?;
            let mut pre: Vec<(usize, usize, SqlValue)> = Vec::with_capacity(agg_calls.len());
            let mut chosen_row_idx: Option<usize> = row_indices.first().copied();

            for (aidx, fcall) in agg_calls.iter().enumerate() {
                let (val, winner) = self.eval_single_aggregate(
                    fcall,
                    &rel.columns,
                    &rel.rows,
                    &row_indices,
                    outer_scopes,
                )?;
                if single_minmax && aidx == 0 && winner.is_some() {
                    chosen_row_idx = winner;
                }
                pre.push((fcall.span.start, fcall.span.end, val));
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
                    precomputed: Some(&pre),
                    aliases: None,
                    no_rowid_aliases: false,
                });
                let alias_vals: Vec<(String, SqlValue, Affinity, String)> = expanded
                    .iter()
                    .filter_map(|p| {
                        let alias = p.explicit_alias.as_ref()?;
                        let v = eval_expr(&p.expr, &scopes, &env).ok()?;
                        let aff = expr_affinity(&p.expr, &scopes).unwrap_or(Affinity::None);
                        let coll = expr_collation(&p.expr, &scopes)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        Some((alias.clone(), v, aff, coll))
                    })
                    .collect();
                if let Some(last) = scopes.last_mut() {
                    last.aliases = Some(&alias_vals);
                }
                let cond = eval_expr(hav, &scopes, &env)?;
                if !boolean_value(&cond, false) {
                    continue;
                }
            }

            out_rows.push(rep_row);
            out_pre.push(pre);
        }

        Ok((out_rows, out_pre))
    }

    fn eval_single_aggregate(
        &self,
        fcall: &FunctionExpr,
        columns: &[ScopeColumn],
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
        let agg_coll: Option<&Collation> = if let Some(first_arg) = fcall.args.first() {
            let sample_scope = EvalScope::simple(columns, &[]);
            let mut scopes = outer_scopes.to_vec();
            scopes.push(sample_scope);
            let oc = expr_collation(first_arg, &scopes);
            match oc.name {
                Some(ref cname) => Some(env.lookup_collation(cname)?),
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
            scopes.push(EvalScope::simple(columns, row));

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
                let sample_scope = EvalScope::simple(columns, &[]);
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
            let mut deduped = Vec::with_capacity(surviving.len());
            let mut seen_vals: Vec<SqlValue> = Vec::new();
            for item in surviving {
                let v = &item.1[0];
                if v.is_null() {
                    continue;
                }
                let dup = seen_vals
                    .iter()
                    .any(|s| mem_compare(s, v, agg_coll) == Ordering::Equal);
                if !dup {
                    seen_vals.push(v.clone());
                    deduped.push(item);
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
        rows: &[Vec<SqlValue>],
        precomputed: &mut [Vec<(usize, usize, SqlValue)>],
        expanded: &[ProjectedItem],
        outer_order_by: Option<&[OrderingTerm]>,
        outer_scopes: &[EvalScope<'_>],
    ) -> SqlResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
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
            let spec = resolve_window_ref(wref, named_windows)?;
            let part_exprs = spec.partition_by.as_deref().unwrap_or(&[]);
            let ord_terms = spec.order_by.as_deref().unwrap_or(&[]);

            let part_colls: Vec<&Collation> = {
                let sample = EvalScope::simple(columns, &[]);
                let mut scopes = outer_scopes.to_vec();
                scopes.push(sample);
                let mut v = Vec::with_capacity(part_exprs.len());
                for pe in part_exprs {
                    let cname = expr_collation(pe, &scopes)
                        .name
                        .unwrap_or_else(|| "BINARY".to_string());
                    v.push(env.lookup_collation(&cname)?);
                }
                v
            };

            let ord_colls: Vec<&Collation> = {
                let sample = EvalScope::simple(columns, &[]);
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
                    precomputed: Some(&precomputed[ridx]),
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

            // Group row indices into partitions.
            let mut partitions: Vec<Vec<usize>> = Vec::new();
            if part_exprs.is_empty() {
                partitions.push((0..rows.len()).collect());
            } else {
                let mut p_rep_keys: Vec<usize> = Vec::new();
                for ridx in 0..rows.len() {
                    self.charge_steps(1)?;
                    let mut found = None;
                    for (pid, &rep) in p_rep_keys.iter().enumerate() {
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

            let lower_name = wcall.name.name.to_ascii_lowercase();
            let first_ord_dir = ord_terms
                .first()
                .and_then(|o| o.order)
                .unwrap_or(SortOrder::Asc);
            let first_ord_coll = ord_colls.first().copied();

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
                        precomputed: Some(&precomputed[ridx]),
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

                let results: Vec<SqlValue> = if is_builtin_window_only(&lower_name) {
                    eval_window_only_partition(&lower_name, &part_args, &peers, |i| {
                        compute_frame_rows(
                            &resolved_frame,
                            i,
                            plen,
                            &peers,
                            &first_order_vals,
                            first_ord_dir,
                            first_ord_coll,
                        )
                    })?
                } else if self
                    .options
                    .functions
                    .is_aggregate(&wcall.name.name, wcall.args.len(), wcall.star)
                {
                    let agg_coll: Option<&Collation> = if let Some(first_arg) = wcall.args.first() {
                        let sample = EvalScope::simple(columns, &[]);
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
                    precomputed[ridx].push((
                        wcall.span.start,
                        wcall.span.end,
                        results[ppos].clone(),
                    ));
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
            if !terms.is_empty() && rows.len() > 1 {
                let mut key_rows: Vec<(usize, Vec<SqlValue>)> = Vec::with_capacity(rows.len());
                let mut term_colls: Vec<&Collation> = Vec::with_capacity(terms.len());

                // Pre-resolve collations for each ORDER BY term
                for term in terms {
                    if let Some(ord) = integer_ordinal(&term.expr) {
                        if ord < 1 || ord > out_cols.len() {
                            return Err(SqlError::runtime(format!(
                                "1st ORDER BY term out of range - should be between 1 and {}",
                                out_cols.len()
                            )));
                        }
                        term_colls.push(env.lookup_collation(&out_cols[ord - 1].collation)?);
                    } else {
                        let sample_out = EvalScope::simple(out_cols, &[]);
                        let mut scopes = outer_scopes.to_vec();
                        if let Some(ctx) = order_ctx {
                            scopes.push(EvalScope::simple(&ctx.from_columns, &[]));
                        }
                        scopes.push(sample_out);
                        let cname = expr_collation(&term.expr, &scopes)
                            .name
                            .unwrap_or_else(|| "BINARY".to_string());
                        term_colls.push(env.lookup_collation(&cname)?);
                    }
                }

                for (ridx, row) in rows.iter().enumerate() {
                    self.charge_steps(1)?;
                    let mut keys = Vec::with_capacity(terms.len());
                    for term in terms {
                        if let Some(ord) = integer_ordinal(&term.expr) {
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
                    key_rows.push((ridx, keys));
                }

                key_rows.sort_by(|(_, ka), (_, kb)| {
                    compare_sort_keys(ka, kb, terms, &term_colls)
                });

                let mut sorted = Vec::with_capacity(rows.len());
                let mut slots: Vec<Option<Vec<SqlValue>>> = rows.into_iter().map(Some).collect();
                for (ridx, _) in key_rows {
                    if let Some(r) = slots[ridx].take() {
                        sorted.push(r);
                    }
                }
                rows = sorted;
            }
        }

        if let Some(lim) = limit {
            let count = int_value(&eval_expr(&lim.count, outer_scopes, &env)?);
            let offset = match &lim.offset {
                Some(off_expr) => int_value(&eval_expr(off_expr, outer_scopes, &env)?).max(0) as usize,
                None => 0,
            };
            if offset >= rows.len() {
                rows.clear();
            } else {
                if offset > 0 {
                    rows.drain(..offset);
                }
                if count >= 0 && (count as usize) < rows.len() {
                    rows.truncate(count as usize);
                }
            }
        }

        Ok(rows)
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
        // SQLite ORDER BY name resolution:
        // 1. Try FROM-scope with output aliases as fallback (matches SQLite's rule where
        //    unqualified bare aliases resolve to SELECT alias, while table.col resolves in FROM).
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
                precomputed: Some(&ctx.precomputed[ridx]),
                aliases: Some(&aliases),
                no_rowid_aliases: false,
            });
            if let Ok(v) = eval_expr(expr, &scopes, env) {
                return Ok(v);
            }
        }
        // 2. Output columns directly (for compound SELECT or alias-only expressions).
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

    fn run_in_table(
        &self,
        schema: Option<&str>,
        table: &str,
        args: Option<&[SqlValue]>,
    ) -> SqlResult<Vec<Vec<SqlValue>>> {
        if let Some(arg_vals) = args {
            let env = self.make_env();
            if let Some(tvf) = self.catalog.call_tvf(
                schema,
                table,
                arg_vals,
                self.options.meter.as_deref(),
            )? {
                return Ok(tvf.rows);
            }
            if let Some(tvf) =
                call_builtin_tvf(&table.to_ascii_lowercase(), arg_vals, &env.fn_ctx)?
            {
                return Ok(tvf.rows);
            }
            return Err(SqlError::runtime(format!("no such table: {table}")));
        }
        if schema.is_none() {
            if let Some(cte) = self.ctes.get(&table.to_ascii_lowercase()) {
                return Ok(cte.rows.clone());
            }
        }
        let scanned = self.catalog.scan_table(
            schema,
            table,
            &TableScanHint::FullScan,
            None,
            self.options.meter.as_deref(),
        )?;
        Ok(scanned.into_iter().map(|r| r.values).collect())
    }
}

// ───────────────────────────── helpers ─────────────────────────────

struct OrderEvalContext {
    from_columns: Vec<ScopeColumn>,
    from_rows: Vec<Vec<SqlValue>>,
    precomputed: Vec<Vec<(usize, usize, SqlValue)>>,
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

fn apply_cte_column_names(cte: &CommonTableExpr, cols: &[ScopeColumn]) -> SqlResult<Vec<String>> {
    if let Some(ref explicit) = cte.columns {
        if explicit.len() != cols.len() {
            return Err(SqlError::runtime(format!(
                "table {} has {} values for {} columns",
                cte.name.name,
                cols.len(),
                explicit.len()
            )));
        }
        Ok(explicit.iter().map(|i| i.name.name.clone()).collect())
    } else {
        Ok(cols.iter().map(|c| c.name.clone()).collect())
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
    Relation {
        columns,
        star_hidden,
        rows: tvf.rows,
    }
}

fn integer_ordinal(expr: &Expr) -> Option<usize> {
    if let Expr::Number(n) = expr {
        if !n.is_float && !n.is_hex {
            return n.text.parse::<usize>().ok();
        }
    }
    None
}

fn contains_row_binary(haystack: &[Vec<SqlValue>], needle: &[SqlValue]) -> bool {
    haystack.iter().any(|row| {
        row.len() == needle.len()
            && row
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| mem_compare(a, b, None) == Ordering::Equal)
    })
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
    if spec.partition_by.is_some() {
        merged.partition_by = spec.partition_by.clone();
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
    let pair = match (
        find_in(lrel, lc),
        find_in(rrel, rc),
        find_in(rrel, lc),
        find_in(lrel, rc),
    ) {
        (Some(li), Some(ri), None, None) => (li, ri),
        (None, None, Some(ri), Some(li)) => (li, ri),
        _ => return None,
    };
    let (li, ri) = pair;
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
    Some((li, ri, aff, cname.to_string()))
}

/// Hash key for BINARY equi-joins (normalizing integer/real numeric equality where exact).
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
                let iv = xdb_core::value::real_to_i64(*r);
                if xdb_core::value::real_same_as_int(*r, iv) {
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

/// Hash key for `GROUP BY` (distinct from `HashKey`: includes `Null` since two NULLs group together).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GroupKey {
    Null,
    Num(i64),
    FloatBits(u64),
    Text(Box<[u8]>),
    Blob(Box<[u8]>),
}

impl GroupKey {
    fn from_sql(v: &SqlValue) -> Self {
        match v {
            SqlValue::Null => GroupKey::Null,
            SqlValue::Int(i) => GroupKey::Num(*i),
            SqlValue::Real(r) => {
                let iv = xdb_core::value::real_to_i64(*r);
                if xdb_core::value::real_same_as_int(*r, iv) {
                    GroupKey::Num(iv)
                } else {
                    let norm = if *r == 0.0 { 0.0f64 } else { *r };
                    GroupKey::FloatBits(norm.to_bits())
                }
            }
            SqlValue::Text(b) => GroupKey::Text(b.clone()),
            SqlValue::Blob(b) => GroupKey::Blob(b.clone()),
        }
    }
}

// ───────────────────────────── Public entry points ─────────────────────────────

/// Execute a parsed [`SelectStmt`] against `catalog`.
pub fn execute_select_stmt(
    catalog: &dyn SqlCatalog,
    stmt: &SelectStmt,
    params: &QueryParams,
    options: &QueryOptions,
) -> SqlResult<QueryResult> {
    let exec = QueryExec::new(catalog, options, params);
    let (cols, rows) = exec.execute_select_stmt(stmt, &[])?;
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
