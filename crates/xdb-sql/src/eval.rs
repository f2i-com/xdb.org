//! SQLite 3.54.1 expression evaluator with exact affinity, collation propagation,
//! row-value vector comparisons, correlated subqueries, and `VIRTUAL` generated column support.

use std::cell::Cell;
use std::cmp::Ordering;

use xdb_core::value::{
    affinity_of_decl_type, apply_affinity, apply_comparison_affinity, arith, bit_not, bitwise,
    boolean_value, cast_to_type, compare_op, comparison_affinity, comparison_collation_name,
    in_affinity, integer_literal_value, mem_compare, negate, real_literal_value, ArithOp, BitOp,
    Collation, CompareOp, OperandCollation,
};
use xdb_core::{Affinity, SqlValue};
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
use crate::utf8::text_bytes_of;

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

/// Shared execution environment for expression evaluation.
pub struct EvalEnv<'a> {
    pub options: &'a QueryOptions,
    pub params: &'a QueryParams,
    pub fn_ctx: FunctionContext<'a>,
    pub subqueries: &'a dyn SubqueryRunner,
    pub dqs: bool,
    eval_depth: Cell<u32>,
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
        let mut fn_ctx = FunctionContext::new(
            options.meter.as_deref(),
            options.effective_now_julian_ms(),
        );
        fn_ctx.limits = options.limits;
        fn_ctx.local_tz_offset_minutes = options.local_tz_offset_minutes;
        fn_ctx.changes = options.changes;
        fn_ctx.total_changes = options.total_changes;
        fn_ctx.last_insert_rowid = options.last_insert_rowid;
        Self {
            options,
            params,
            fn_ctx,
            subqueries,
            dqs: options.dqs,
            eval_depth: Cell::new(0),
        }
    }

    pub fn lookup_collation(&self, name: &str) -> SqlResult<&Collation> {
        self.options
            .collations
            .find(name)
            .ok_or_else(|| SqlError::runtime(format!("no such collation sequence: {name}")))
    }

    pub fn lookup_opt_collation(&self, name: Option<&str>) -> SqlResult<Option<&Collation>> {
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

/// Resolve a column reference across `scopes` (innermost = last in slice).
pub fn resolve_column(
    col_ref: &ColumnRef,
    scopes: &[EvalScope<'_>],
    dqs: bool,
) -> SqlResult<ResolvedCol> {
    let want_schema = col_ref.schema.as_ref().map(|n| n.name.as_str());
    let want_table = col_ref.table.as_ref().map(|n| n.name.as_str());
    let want_col = col_ref.column.name.as_str();

    // Validate schema qualifier if present (only `main` exists in XDB single-DB catalogs).
    if let Some(ws) = want_schema {
        if !ws.eq_ignore_ascii_case("main") {
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
                    } else if want_table.is_none() {
                        ambiguous = true;
                    }
                } else {
                    found_idx = Some(idx);
                }
            }
        }
        if ambiguous {
            return Err(SqlError::runtime(format!(
                "ambiguous column name: {want_col}"
            )));
        }
        if let Some(idx) = found_idx {
            let c = &scope.columns[idx];
            return Ok(ResolvedCol {
                value: scope.values.get(idx).cloned().unwrap_or(SqlValue::Null),
                affinity: c.affinity,
                collation: c.collation.clone(),
            });
        }
        // Check SELECT aliases (only when unqualified)
        if want_table.is_none() {
            if let Some(aliases) = scope.aliases {
                for (aname, aval, aaff, acoll) in aliases {
                    if aname.eq_ignore_ascii_case(want_col) {
                        return Ok(ResolvedCol {
                            value: aval.clone(),
                            affinity: *aaff,
                            collation: acoll.clone(),
                        });
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
            return Ok(ResolvedCol {
                value: SqlValue::Int(1),
                affinity: Affinity::None,
                collation: "BINARY".to_string(),
            });
        }
        if want_col.eq_ignore_ascii_case("false") {
            return Ok(ResolvedCol {
                value: SqlValue::Int(0),
                affinity: Affinity::None,
                collation: "BINARY".to_string(),
            });
        }
    }

    // SQLite DQS fallback: double-quoted identifier becomes a string literal when `dqs` is true.
    if dqs && want_table.is_none() && col_ref.schema.is_none() && col_ref.column.quote == QuoteStyle::Double {
        return Ok(ResolvedCol {
            value: SqlValue::text(want_col),
            affinity: Affinity::Blob,
            collation: "BINARY".to_string(),
        });
    }

    let full = match (want_schema, want_table) {
        (Some(s), Some(t)) => format!("{s}.{t}.{want_col}"),
        (None, Some(t)) => format!("{t}.{want_col}"),
        (Some(s), None) => format!("{s}.{want_col}"),
        (None, None) => want_col.to_string(),
    };
    Err(SqlError::runtime(format!("no such column: {full}")))
}

/// Static affinity of an expression (`sqlite3ExprAffinity`, `expr.c:324`).
/// Returns `None` (`SQLITE_AFF_NONE`) for literals and general expressions with no affinity.
pub fn expr_affinity(expr: &Expr, scopes: &[EvalScope<'_>]) -> Option<Affinity> {
    match expr {
        Expr::Column(col_ref) => resolve_column(col_ref, scopes, false)
            .ok()
            .map(|r| r.affinity),
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

/// Collation of an expression (`sqlite3ExprCollSeq`, `expr.c:180`).
pub fn expr_collation(expr: &Expr, scopes: &[EvalScope<'_>]) -> ExprCollation {
    match expr {
        Expr::Collate { collation, .. } => ExprCollation::explicit(collation.name.clone()),
        Expr::Column(col_ref) => match resolve_column(col_ref, scopes, false) {
            Ok(r) => ExprCollation::implicit(r.collation),
            Err(_) => ExprCollation::none(),
        },
        Expr::Unary {
            op: UnaryOp::Pos,
            operand,
            ..
        } => expr_collation(operand, scopes),
        _ => ExprCollation::none(),
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
            let resolved = resolve_column(c, scopes, env.dqs)?;
            Ok((resolved.value, false))
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
            let out = match op {
                UnaryOp::Pos => v,
                UnaryOp::Neg => negate(&v),
                UnaryOp::BitNot => bit_not(&v),
                UnaryOp::Not => {
                    if v.is_null() {
                        SqlValue::Null
                    } else {
                        SqlValue::Int(i64::from(!boolean_value(&v, false)))
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
                    other => SqlValue::Int(i64::from(!boolean_value(&other, false))),
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
            if is_row_value_expr(operand) || is_row_value_expr(low) || is_row_value_expr(high) {
                let (xvals, xaffs, xcolls) = eval_row_vector(operand, scopes, env)?;
                let (lvals, laffs, lcolls) = eval_row_vector(low, scopes, env)?;
                let (hvals, haffs, hcolls) = eval_row_vector(high, scopes, env)?;
                if xvals.len() != lvals.len() || xvals.len() != hvals.len() {
                    return Err(SqlError::runtime("row value misused"));
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
            let (x, _) = eval_expr_tagged(operand, scopes, env)?;
            let (lo, _) = eval_expr_tagged(low, scopes, env)?;
            let (hi, _) = eval_expr_tagged(high, scopes, env)?;
            let aff_lo = comparison_affinity(
                expr_affinity(operand, scopes),
                expr_affinity(low, scopes),
            );
            let aff_hi = comparison_affinity(
                expr_affinity(operand, scopes),
                expr_affinity(high, scopes),
            );
            let op_coll = expr_collation(operand, scopes);
            let lo_coll = expr_collation(low, scopes);
            let hi_coll = expr_collation(high, scopes);
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
                schema.as_ref().map(|n| n.name.as_str()),
                &table.name,
                eval_args.as_deref(),
            )?;
            eval_in_rows(*not, operand, &rhs_affs, &rhs_colls, &rows, scopes, env)
        }
        Expr::Exists { select, .. } => {
            let (_, rows) = env.subqueries.run_select(select, scopes)?;
            Ok((SqlValue::Int(i64::from(!rows.is_empty())), false))
        }
        Expr::Subquery { select, .. } => {
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
                let (base_val, _) = eval_expr_tagged(base_expr, scopes, env)?;
                let base_aff = expr_affinity(base_expr, scopes);
                let base_coll = expr_collation(base_expr, scopes);
                for w in whens {
                    if !base_val.is_null() {
                        let (wv, _) = eval_expr_tagged(&w.when, scopes, env)?;
                        if !wv.is_null() {
                            let aff = comparison_affinity(base_aff, expr_affinity(&w.when, scopes));
                            let w_coll = expr_collation(&w.when, scopes);
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
                for w in whens {
                    let (cond, _) = eval_expr_tagged(&w.when, scopes, env)?;
                    if boolean_value(&cond, false) {
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
            // Short-circuit built-in conditional functions (SQLite evaluates only taken branches).
            let lower_fn = f.name.name.to_ascii_lowercase();
            if !env
                .options
                .functions
                .has_custom_scalar(&lower_fn, f.args.len())
            {
                match lower_fn.as_str() {
                    "coalesce" => {
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
                    "ifnull" => {
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
                    "iif" | "if" => {
                        if f.args.len() < 2 {
                            return Err(SqlError::runtime(format!(
                                "wrong number of arguments to function {}()",
                                f.name.name
                            )));
                        }
                        let mut idx = 0;
                        while idx + 1 < f.args.len() {
                            let (cond, _) = eval_expr_tagged(&f.args[idx], scopes, env)?;
                            if boolean_value(&cond, false) {
                                return eval_expr_tagged(&f.args[idx + 1], scopes, env);
                            }
                            idx += 2;
                        }
                        if idx < f.args.len() {
                            return eval_expr_tagged(&f.args[idx], scopes, env);
                        }
                        return Ok((SqlValue::Null, false));
                    }
                    _ => {}
                }
            }
            let mut arg_vals = Vec::with_capacity(f.args.len());
            let mut json_mask: u64 = 0;
            // Explicit COLLATE on any arg overrides implicit collation (matching SQLite).
            let mut explicit_coll: Option<&Collation> = None;
            let mut implicit_coll: Option<&Collation> = None;
            for (idx, a) in f.args.iter().enumerate() {
                let oc = expr_collation(a, scopes);
                if let Some(ref cname) = oc.name {
                    if oc.explicit {
                        if explicit_coll.is_none() {
                            explicit_coll = Some(env.lookup_collation(cname)?);
                        }
                    } else if implicit_coll.is_none() {
                        implicit_coll = Some(env.lookup_collation(cname)?);
                    }
                }
                let (v, is_j) = eval_expr_tagged(a, scopes, env)?;
                if is_j && idx < 64 {
                    json_mask |= 1u64 << idx;
                }
                arg_vals.push(v);
            }
            let coll_opt = explicit_coll.or(implicit_coll);
            let call_ctx = env
                .fn_ctx
                .with_collation_and_json_mask(coll_opt, json_mask);
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
        Expr::Raise { message, .. } => {
            let msg = match message {
                Some(m) => {
                    let v = eval_expr(m, scopes, env)?;
                    text_bytes_of(&v)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_else(|| "RAISE".to_string())
                }
                None => "RAISE".to_string(),
            };
            Err(SqlError::runtime(msg))
        }
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
    match op {
        BinaryOp::And => {
            let (lv, _) = eval_expr_tagged(left, scopes, env)?;
            if !lv.is_null() && !boolean_value(&lv, false) {
                return Ok((SqlValue::Int(0), false));
            }
            let (rv, _) = eval_expr_tagged(right, scopes, env)?;
            if !rv.is_null() && !boolean_value(&rv, false) {
                return Ok((SqlValue::Int(0), false));
            }
            if lv.is_null() || rv.is_null() {
                return Ok((SqlValue::Null, false));
            }
            return Ok((SqlValue::Int(1), false));
        }
        BinaryOp::Or => {
            let (lv, _) = eval_expr_tagged(left, scopes, env)?;
            if !lv.is_null() && boolean_value(&lv, false) {
                return Ok((SqlValue::Int(1), false));
            }
            let (rv, _) = eval_expr_tagged(right, scopes, env)?;
            if !rv.is_null() && boolean_value(&rv, false) {
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
            let (lv, _) = eval_expr_tagged(left, scopes, env)?;
            let (rv, _) = eval_expr_tagged(right, scopes, env)?;
            let aff = comparison_affinity(expr_affinity(left, scopes), expr_affinity(right, scopes));
            let lcoll = expr_collation(left, scopes);
            let rcoll = expr_collation(right, scopes);
            let cname = comparison_collation_name(lcoll.as_operand(), rcoll.as_operand());
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
        BinaryOp::Add => arith(ArithOp::Add, &lv, &rv),
        BinaryOp::Sub => arith(ArithOp::Subtract, &lv, &rv),
        BinaryOp::Mul => arith(ArithOp::Multiply, &lv, &rv),
        BinaryOp::Div => arith(ArithOp::Divide, &lv, &rv),
        BinaryOp::Rem => arith(ArithOp::Remainder, &lv, &rv),
        BinaryOp::BitAnd => bitwise(BitOp::And, &lv, &rv),
        BinaryOp::BitOr => bitwise(BitOp::Or, &lv, &rv),
        BinaryOp::LShift => bitwise(BitOp::ShiftLeft, &lv, &rv),
        BinaryOp::RShift => bitwise(BitOp::ShiftRight, &lv, &rv),
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
            let (cols, affs, colls, rows) = env.subqueries.run_select_with_meta(select, scopes)?;
            let width = cols.len();
            let vals = rows
                .into_iter()
                .next()
                .unwrap_or_else(|| vec![SqlValue::Null; width]);
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
    let (lvals, laffs, lcolls) = eval_row_vector(left, scopes, env)?;
    let (rvals, raffs, rcolls) = eval_row_vector(right, scopes, env)?;
    if lvals.len() != rvals.len() {
        return Err(SqlError::runtime(format!(
            "row value misused: {} vs {} columns",
            lvals.len(),
            rvals.len()
        )));
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
    // Row-value IN list: `(a, b) IN ((1, 2), (3, NULL))`
    if is_row_value_expr(operand) {
        let (lvals, laffs, lcolls) = eval_row_vector(operand, scopes, env)?;
        let width = lvals.len();
        let mut saw_null = false;
        for item in items {
            let (rvals, _raffs, rcolls) = eval_row_vector(item, scopes, env)?;
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
                let eff_aff = in_affinity(laffs[i], None);
                let lv = apply_affinity(lvals[i].clone(), eff_aff);
                let rv = apply_affinity(rvals[i].clone(), eff_aff);
                if lv.is_null() || rv.is_null() {
                    row_null = true;
                    continue;
                }
                let cname =
                    comparison_collation_name(lcolls[i].as_operand(), rcolls[i].as_operand());
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

    let lhs_val = eval_expr(operand, scopes, env)?;
    let lhs_aff = expr_affinity(operand, scopes);
    let lhs_coll = expr_collation(operand, scopes);
    let aff = in_affinity(lhs_aff, None);

    if lhs_val.is_null() {
        return Ok((SqlValue::Null, false));
    }
    let lhs_coerced = apply_affinity(lhs_val, aff);
    let mut saw_null = false;
    for item in items {
        let rv = eval_expr(item, scopes, env)?;
        if rv.is_null() {
            saw_null = true;
            continue;
        }
        let rcoll = expr_collation(item, scopes);
        let cname = comparison_collation_name(lhs_coll.as_operand(), rcoll.as_operand());
        let coll = env.lookup_opt_collation(cname)?;
        let rhs_coerced = apply_affinity(rv, aff);
        if mem_compare(&lhs_coerced, &rhs_coerced, coll) == Ordering::Equal {
            return Ok((SqlValue::Int(i64::from(!not)), false));
        }
    }
    if saw_null {
        Ok((SqlValue::Null, false))
    } else {
        Ok((SqlValue::Int(i64::from(not)), false))
    }
}

fn eval_in_select(
    not: bool,
    operand: &Expr,
    select: &SelectStmt,
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqlResult<(SqlValue, bool)> {
    let (cols, rhs_affs, rhs_colls, rows) = env.subqueries.run_select_with_meta(select, scopes)?;
    let width = if let Expr::Vector { items, .. } = operand {
        items.len()
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

/// Ensure `SqlCatalog` trait is imported cleanly if needed by callers.
#[allow(dead_code)]
fn _assert_catalog_object_safe(_: &dyn SqlCatalog) {}
