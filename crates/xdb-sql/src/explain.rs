//! SQLite 3.24+ `EXPLAIN QUERY PLAN` output generator returning
//! `(id, parent, notused, detail)` rows matching SQLite's query-plan tree format.

use xdb_core::{Meter, SqlValue};
use xdb_parser::{
    join_flag, BinaryOp, CompoundOperator, DistinctKind, Expr, FromSource, OrderingTerm,
    ParseOptions, ResultColumn, SelectBody, SelectCore, SelectStmt, SortOrder, Statement,
    WindowDefinition, WindowRef, WindowSpec, WithClause,
};

use crate::catalog::{IndexMeta, QueryOptions, QueryParams, QueryResult, SqlCatalog, TableMeta};
use crate::errors::{SqlError, SqlResult};

const MAX_EQP_VIEW_DEPTH: usize = 64;
const MAX_EQP_EXPR_DEPTH: usize = 1000;
const MAX_EQP_COMPOUND_TERMS: usize = 500;

struct EqpMemGuard<'a> {
    meter: Option<&'a dyn Meter>,
    reserved: u64,
}

impl<'a> EqpMemGuard<'a> {
    fn new(meter: Option<&'a dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    fn reserve(&mut self, bytes: u64) -> SqlResult<()> {
        if bytes > 0 {
            if let Some(m) = self.meter {
                m.reserve_memory(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
            }
        }
        Ok(())
    }
}

impl Drop for EqpMemGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(m) = self.meter {
                m.release_memory(self.reserved);
            }
            self.reserved = 0;
        }
    }
}

fn is_builtin_tvf(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "json_each" | "json_tree" | "generate_series"
    )
}

#[derive(Debug, Clone)]
enum ChosenAccess {
    Scan,
    Rowid,
    Index {
        key_columns: Vec<Option<usize>>,
        key_collations: Vec<String>,
        key_desc: Vec<bool>,
        eq_prefix_len: usize,
    },
}

struct EqpBuilder<'a> {
    catalog: &'a dyn SqlCatalog,
    options: &'a QueryOptions,
    next_id: i64,
    rows: Vec<Vec<SqlValue>>,
    ctes: Vec<String>,
    expanding_views: Vec<String>,
    depth: usize,
    mem: EqpMemGuard<'a>,
}

impl<'a> EqpBuilder<'a> {
    fn new(catalog: &'a dyn SqlCatalog, options: &'a QueryOptions) -> Self {
        Self {
            catalog,
            options,
            next_id: 2,
            rows: Vec::new(),
            ctes: Vec::new(),
            expanding_views: Vec::new(),
            depth: 0,
            mem: EqpMemGuard::new(options.meter.as_deref()),
        }
    }

    fn charge_step(&self) -> SqlResult<()> {
        if let Some(ref m) = self.options.meter {
            m.charge_steps(1)?;
        }
        Ok(())
    }

    fn emit(&mut self, parent: i64, detail: impl Into<String>) -> SqlResult<i64> {
        self.charge_step()?;
        let detail_str = detail.into();
        self.mem
            .reserve(128u64.saturating_add(detail_str.len() as u64))?;
        let id = self.next_id;
        self.next_id += 1;
        self.rows.push(vec![
            SqlValue::Int(id),
            SqlValue::Int(parent),
            SqlValue::Int(0),
            SqlValue::text(detail_str),
        ]);
        Ok(id)
    }

    fn push_with(&mut self, with: &WithClause, parent: i64) -> SqlResult<usize> {
        let prev = self.ctes.len();
        for cte in &with.ctes {
            let node_id = self.emit(parent, format!("MATERIALIZE {}", cte.name.name))?;
            self.ctes.push(cte.name.name.clone());
            self.explain_select(&cte.select, node_id)?;
        }
        Ok(prev)
    }

    fn explain_returning(&mut self, ret: Option<&[ResultColumn]>, parent: i64) -> SqlResult<()> {
        if let Some(cols) = ret {
            for rc in cols {
                if let ResultColumn::Expr(re) = rc {
                    self.explain_expr_subqueries(&re.expr, parent, 0)?;
                }
            }
        }
        Ok(())
    }

    fn explain_stmt(&mut self, stmt: &Statement, parent: i64) -> SqlResult<()> {
        self.charge_step()?;
        if self.depth >= MAX_EQP_EXPR_DEPTH {
            return Err(SqlError::runtime("query nesting too deep"));
        }
        self.depth += 1;
        let res = match stmt {
            Statement::Select(sel) => self.explain_select(sel, parent),
            Statement::Insert(ins) => {
                let schema = ins.table.schema.as_ref().map(|s| s.name.as_str());
                if self
                    .catalog
                    .get_table_arc(schema, &ins.table.name.name)?
                    .is_none()
                    && self.catalog.get_view(schema, &ins.table.name.name)?.is_none()
                {
                    return Err(SqlError::runtime(format!(
                        "no such table: {}",
                        ins.table.name.name
                    )));
                }
                let prev_ctes = if let Some(ref with) = ins.with {
                    Some(self.push_with(with, parent)?)
                } else {
                    None
                };
                let r = (|| -> SqlResult<()> {
                    if let Some(ref sel) = ins.select {
                        self.explain_select(sel, parent)?;
                    } else {
                        self.emit(parent, "SCAN CONSTANT ROW")?;
                    }
                    for up in &ins.upsert {
                        if let Some(ref tw) = up.target_where {
                            self.explain_expr_subqueries(tw, parent, 0)?;
                        }
                        if let Some(ref sets) = up.set {
                            for sc in sets {
                                self.explain_expr_subqueries(&sc.expr, parent, 0)?;
                            }
                        }
                        if let Some(ref we) = up.where_clause {
                            self.explain_expr_subqueries(we, parent, 0)?;
                        }
                    }
                    self.explain_returning(ins.returning.as_deref(), parent)?;
                    Ok(())
                })();
                if let Some(prev) = prev_ctes {
                    self.ctes.truncate(prev);
                }
                r
            }
            Statement::Update(upd) => {
                let prev_ctes = if let Some(ref with) = upd.with {
                    Some(self.push_with(with, parent)?)
                } else {
                    None
                };
                let alias = upd.table.alias.as_ref().map(|a| a.name.as_str());
                let mut preds = Vec::new();
                if let Some(ref w) = upd.where_clause {
                    collect_conjuncts(w, &mut preds);
                }
                let r = (|| -> SqlResult<()> {
                    let mut from_tables: Vec<(String, std::sync::Arc<TableMeta>)> = Vec::new();
                    if let Some(ref from) = upd.from {
                        self.collect_from_tables(from, &mut from_tables, 0)?;
                    }
                    let _ = self.explain_table_access(
                        upd.table.schema.as_ref().map(|s| s.name.as_str()),
                        &upd.table.name.name,
                        alias,
                        upd.table.indexed_by.as_ref().map(|n| n.name.as_str()),
                        upd.table.not_indexed,
                        &preds,
                        &[],
                        None,
                        false,
                        false,
                        &from_tables,
                        parent,
                    )?;
                    if let Some(ref from) = upd.from {
                        let target_schema = upd.table.schema.as_ref().map(|s| s.name.as_str());
                        if let Ok(Some(tmeta)) =
                            self.catalog.get_table_arc(target_schema, &upd.table.name.name)
                        {
                            let t_alias = alias.unwrap_or(&upd.table.name.name).to_string();
                            from_tables.push((t_alias, tmeta));
                        }
                        self.explain_from(from, &preds, &[], None, false, &from_tables, parent)?;
                    }
                    for sc in &upd.set {
                        self.explain_expr_subqueries(&sc.expr, parent, 0)?;
                    }
                    if let Some(ref w) = upd.where_clause {
                        self.explain_expr_subqueries(w, parent, 0)?;
                    }
                    self.explain_returning(upd.returning.as_deref(), parent)?;
                    Ok(())
                })();
                if let Some(prev) = prev_ctes {
                    self.ctes.truncate(prev);
                }
                r
            }
            Statement::Delete(del) => {
                let prev_ctes = if let Some(ref with) = del.with {
                    Some(self.push_with(with, parent)?)
                } else {
                    None
                };
                let alias = del.table.alias.as_ref().map(|a| a.name.as_str());
                let mut preds = Vec::new();
                if let Some(ref w) = del.where_clause {
                    collect_conjuncts(w, &mut preds);
                }
                let r = (|| -> SqlResult<()> {
                    let _ = self.explain_table_access(
                        del.table.schema.as_ref().map(|s| s.name.as_str()),
                        &del.table.name.name,
                        alias,
                        del.table.indexed_by.as_ref().map(|n| n.name.as_str()),
                        del.table.not_indexed,
                        &preds,
                        &[],
                        None,
                        false,
                        false,
                        &[],
                        parent,
                    )?;
                    if let Some(ref w) = del.where_clause {
                        self.explain_expr_subqueries(w, parent, 0)?;
                    }
                    self.explain_returning(del.returning.as_deref(), parent)?;
                    Ok(())
                })();
                if let Some(prev) = prev_ctes {
                    self.ctes.truncate(prev);
                }
                r
            }
            Statement::Explain { statement, .. } => self.explain_stmt(statement, parent),
            _ => Ok(()),
        };
        self.depth -= 1;
        res
    }

    fn explain_select(&mut self, sel: &SelectStmt, parent: i64) -> SqlResult<()> {
        self.charge_step()?;
        if self.depth >= MAX_EQP_EXPR_DEPTH {
            return Err(SqlError::runtime("query nesting too deep"));
        }
        self.depth += 1;
        let prev_cte_len = self.ctes.len();
        let res = (|| -> SqlResult<()> {
            if let Some(ref with) = sel.with {
                let _ = self.push_with(with, parent)?;
            }
            let has_order_by = sel.order_by.is_some() || sel.order_by_all.is_some();
            self.explain_body(&sel.body, parent, has_order_by, sel.order_by.as_deref())?;
            if let Some(ref obs) = sel.order_by {
                for ot in obs {
                    self.explain_expr_subqueries(&ot.expr, parent, 0)?;
                }
            }
            if let Some(ref lim) = sel.limit {
                self.explain_expr_subqueries(&lim.count, parent, 0)?;
                if let Some(ref off) = lim.offset {
                    self.explain_expr_subqueries(off, parent, 0)?;
                }
            }
            Ok(())
        })();
        self.ctes.truncate(prev_cte_len);
        self.depth -= 1;
        res
    }

    fn explain_body(
        &mut self,
        body: &SelectBody,
        parent: i64,
        has_order_by: bool,
        order_by: Option<&[OrderingTerm]>,
    ) -> SqlResult<()> {
        self.charge_step()?;
        match body {
            SelectBody::Core(core) => {
                let single_plan = self.explain_core(core, order_by, parent)?;
                let order_satisfied = match (&single_plan, order_by) {
                    (Some((tbl, alias, access)), Some(obs)) if !obs.is_empty() => {
                        order_by_satisfied_by_single_table(core, tbl, alias, access, obs)
                    }
                    _ => false,
                };
                if has_order_by && !order_satisfied {
                    self.emit(parent, "USE TEMP B-TREE FOR ORDER BY")?;
                }
                Ok(())
            }
            SelectBody::Values(vals) => {
                let n = vals.rows.len();
                if n == 1 {
                    self.emit(parent, "SCAN CONSTANT ROW")?;
                } else {
                    self.emit(parent, format!("SCAN {n} CONSTANT ROWS"))?;
                }
                for row in &vals.rows {
                    for item in &row.items {
                        self.explain_expr_subqueries(item, parent, 0)?;
                    }
                }
                if has_order_by {
                    self.emit(parent, "USE TEMP B-TREE FOR ORDER BY")?;
                }
                Ok(())
            }
            SelectBody::Compound { .. } => {
                // Flatten left-associative compound chain iteratively (supports up to
                // `MAX_EQP_COMPOUND_TERMS = 500` independently of subquery nesting depth).
                let mut terms: Vec<(Option<CompoundOperator>, &SelectBody)> = Vec::new();
                let mut cur = body;
                while let SelectBody::Compound {
                    op, left, right, ..
                } = cur
                {
                    if terms.len() >= MAX_EQP_COMPOUND_TERMS {
                        return Err(SqlError::runtime("too many terms in compound SELECT"));
                    }
                    terms.push((Some(*op), right.as_ref()));
                    cur = left.as_ref();
                }
                terms.push((None, cur));
                if terms.len() > MAX_EQP_COMPOUND_TERMS {
                    return Err(SqlError::runtime("too many terms in compound SELECT"));
                }
                terms.reverse();
                let cmp_id = self.emit(parent, "COMPOUND QUERY")?;
                let left_id = self.emit(cmp_id, "LEFT-MOST SUBQUERY")?;
                self.explain_body(terms[0].1, left_id, false, None)?;
                for (op_opt, branch) in &terms[1..] {
                    let op_label = match op_opt.expect("compound op") {
                        CompoundOperator::Union => "UNION USING TEMP B-TREE",
                        CompoundOperator::UnionAll => "UNION ALL",
                        CompoundOperator::Intersect => "INTERSECT USING TEMP B-TREE",
                        CompoundOperator::Except => "EXCEPT USING TEMP B-TREE",
                    };
                    let right_id = self.emit(cmp_id, op_label)?;
                    self.explain_body(branch, right_id, false, None)?;
                }
                if has_order_by {
                    self.emit(cmp_id, "USE TEMP B-TREE FOR ORDER BY")?;
                }
                Ok(())
            }
        }
    }

    fn explain_core(
        &mut self,
        core: &SelectCore,
        order_by: Option<&[OrderingTerm]>,
        parent: i64,
    ) -> SqlResult<Option<(std::sync::Arc<TableMeta>, String, ChosenAccess)>> {
        let extra_refs = ExtraRefs {
            where_clause: core.where_clause.as_ref(),
            group_by: core.group_by.as_deref(),
            having: core.having.as_ref(),
            order_by,
            windows: core.windows.as_deref(),
        };
        let mut preds = Vec::new();
        if let Some(ref w) = core.where_clause {
            collect_conjuncts(w, &mut preds);
        }
        let is_single_from = matches!(core.from, Some(FromSource::Table(_)));
        let single_plan = if let Some(ref from) = core.from {
            let mut from_tables: Vec<(String, std::sync::Arc<TableMeta>)> = Vec::new();
            self.collect_from_tables(from, &mut from_tables, 0)?;
            self.explain_from(
                from,
                &preds,
                &[],
                Some((&core.columns, &extra_refs)),
                is_single_from,
                &from_tables,
                parent,
            )?
        } else {
            self.emit(parent, "SCAN CONSTANT ROW")?;
            None
        };
        // Explain any subqueries in WHERE, SELECT, GROUP BY, HAVING, WINDOW.
        if let Some(ref w) = core.where_clause {
            self.explain_expr_subqueries(w, parent, 0)?;
        }
        for rc in &core.columns {
            if let ResultColumn::Expr(re) = rc {
                self.explain_expr_subqueries(&re.expr, parent, 0)?;
            }
        }
        if let Some(ref gb) = core.group_by {
            for e in gb {
                self.explain_expr_subqueries(e, parent, 0)?;
            }
        }
        if let Some(ref h) = core.having {
            self.explain_expr_subqueries(h, parent, 0)?;
        }
        if let Some(ref wins) = core.windows {
            for wd in wins {
                self.explain_window_spec_subqueries(&wd.window, parent, 0)?;
            }
        }
        if core.group_by.is_some() || core.group_by_all {
            self.emit(parent, "USE TEMP B-TREE FOR GROUP BY")?;
        }
        if core.distinct == Some(DistinctKind::Distinct) {
            self.emit(parent, "USE TEMP B-TREE FOR DISTINCT")?;
        }
        Ok(single_plan)
    }

    fn explain_window_spec_subqueries(
        &mut self,
        ws: &WindowSpec,
        parent: i64,
        edepth: usize,
    ) -> SqlResult<()> {
        if let Some(ref pbs) = ws.partition_by {
            for e in pbs {
                self.explain_expr_subqueries(e, parent, edepth + 1)?;
            }
        }
        if let Some(ref obs) = ws.order_by {
            for ot in obs {
                self.explain_expr_subqueries(&ot.expr, parent, edepth + 1)?;
            }
        }
        if let Some(ref fr) = ws.frame {
            if let Some(ref e) = fr.from.expr {
                self.explain_expr_subqueries(e, parent, edepth + 1)?;
            }
            if let Some(ref to) = fr.to {
                if let Some(ref e) = to.expr {
                    self.explain_expr_subqueries(e, parent, edepth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn explain_expr_subqueries(&mut self, expr: &Expr, parent: i64, edepth: usize) -> SqlResult<()> {
        if edepth >= MAX_EQP_EXPR_DEPTH {
            return Err(SqlError::runtime("Expression tree is too large"));
        }
        self.charge_step()?;
        match expr {
            Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
                let id = self.emit(parent, "SCALAR SUBQUERY")?;
                self.explain_select(select, id)
            }
            Expr::InSelect { operand, select, .. } => {
                self.explain_expr_subqueries(operand, parent, edepth + 1)?;
                let id = self.emit(parent, "LIST SUBQUERY")?;
                self.explain_select(select, id)
            }
            Expr::Unary { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. } => {
                self.explain_expr_subqueries(operand, parent, edepth + 1)
            }
            Expr::Binary { left, right, .. } => {
                self.explain_expr_subqueries(left, parent, edepth + 1)?;
                self.explain_expr_subqueries(right, parent, edepth + 1)
            }
            Expr::Like {
                left,
                right,
                escape,
                ..
            } => {
                self.explain_expr_subqueries(left, parent, edepth + 1)?;
                self.explain_expr_subqueries(right, parent, edepth + 1)?;
                if let Some(e) = escape {
                    self.explain_expr_subqueries(e, parent, edepth + 1)?;
                }
                Ok(())
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                self.explain_expr_subqueries(operand, parent, edepth + 1)?;
                self.explain_expr_subqueries(low, parent, edepth + 1)?;
                self.explain_expr_subqueries(high, parent, edepth + 1)
            }
            Expr::InList { operand, items, .. } => {
                self.explain_expr_subqueries(operand, parent, edepth + 1)?;
                for i in items {
                    self.explain_expr_subqueries(i, parent, edepth + 1)?;
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
                    self.explain_expr_subqueries(o, parent, edepth + 1)?;
                }
                for w in whens {
                    self.explain_expr_subqueries(&w.when, parent, edepth + 1)?;
                    self.explain_expr_subqueries(&w.then, parent, edepth + 1)?;
                }
                if let Some(e) = else_expr {
                    self.explain_expr_subqueries(e, parent, edepth + 1)?;
                }
                Ok(())
            }
            Expr::Function(f) => {
                for a in &f.args {
                    self.explain_expr_subqueries(a, parent, edepth + 1)?;
                }
                if let Some(ref obs) = f.order_by {
                    for ot in obs {
                        self.explain_expr_subqueries(&ot.expr, parent, edepth + 1)?;
                    }
                }
                if let Some(ref flt) = f.filter {
                    self.explain_expr_subqueries(flt, parent, edepth + 1)?;
                }
                if let Some(WindowRef::Spec(ref ws)) = f.over {
                    self.explain_window_spec_subqueries(ws, parent, edepth)?;
                }
                Ok(())
            }
            Expr::Vector { items, .. } => {
                for i in items {
                    self.explain_expr_subqueries(i, parent, edepth + 1)?;
                }
                Ok(())
            }
            Expr::Raise {
                message: Some(m), ..
            } => self.explain_expr_subqueries(m, parent, edepth + 1),
            _ => Ok(()),
        }
    }

    fn collect_from_col_names(
        &self,
        from: &FromSource,
        out: &mut Vec<String>,
        depth: usize,
    ) -> SqlResult<()> {
        if depth >= MAX_EQP_EXPR_DEPTH {
            return Ok(());
        }
        self.charge_step()?;
        match from {
            FromSource::Table(ft) => {
                let schema = ft.schema.as_ref().map(|s| s.name.as_str());
                if let Some(tbl) = self.catalog.get_table_arc(schema, &ft.name.name)? {
                    for c in &tbl.columns {
                        out.push(c.name.clone());
                    }
                }
            }
            FromSource::Group(fg) => self.collect_from_col_names(&fg.source, out, depth + 1)?,
            FromSource::Join { left, right, .. } => {
                self.collect_from_col_names(left, out, depth + 1)?;
                self.collect_from_col_names(right, out, depth + 1)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn collect_from_tables(
        &mut self,
        from: &FromSource,
        out: &mut Vec<(String, std::sync::Arc<TableMeta>)>,
        depth: usize,
    ) -> SqlResult<()> {
        if depth >= MAX_EQP_EXPR_DEPTH {
            return Ok(());
        }
        self.charge_step()?;
        match from {
            FromSource::Table(ft) => {
                let schema = ft.schema.as_ref().map(|s| s.name.as_str());
                if schema.is_none()
                    && self
                        .ctes
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(&ft.name.name))
                {
                    return Ok(());
                }
                if let Some(tbl) = self.catalog.get_table_arc(schema, &ft.name.name)? {
                    let eff_alias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();
                    out.push((eff_alias, tbl));
                }
            }
            FromSource::Group(fg) => self.collect_from_tables(&fg.source, out, depth + 1)?,
            FromSource::Join { left, right, .. } => {
                self.collect_from_tables(left, out, depth + 1)?;
                self.collect_from_tables(right, out, depth + 1)?;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn explain_from(
        &mut self,
        from: &FromSource,
        preds: &[&Expr],
        join_eq_cols: &[String],
        proj_info: Option<(&[ResultColumn], &ExtraRefs<'_>)>,
        allow_covering: bool,
        from_tables: &[(String, std::sync::Arc<TableMeta>)],
        parent: i64,
    ) -> SqlResult<Option<(std::sync::Arc<TableMeta>, String, ChosenAccess)>> {
        self.charge_step()?;
        match from {
            FromSource::Table(ft) => {
                let alias = ft.alias.as_ref().map(|a| a.name.as_str());
                self.explain_table_access(
                    ft.schema.as_ref().map(|s| s.name.as_str()),
                    &ft.name.name,
                    alias,
                    ft.indexed_by.as_ref().map(|n| n.name.as_str()),
                    ft.not_indexed,
                    preds,
                    join_eq_cols,
                    proj_info,
                    allow_covering,
                    true,
                    from_tables,
                    parent,
                )
            }
            FromSource::Function(ff) => {
                if !self.catalog.has_table_valued_function(&ff.name.name)
                    && !is_builtin_tvf(&ff.name.name)
                {
                    return Err(SqlError::runtime(format!(
                        "no such table: {}",
                        ff.name.name
                    )));
                }
                let detail = if let Some(ref a) = ff.alias {
                    format!("SCAN {} AS {} VIRTUAL TABLE INDEX 0:", ff.name.name, a.name)
                } else {
                    format!("SCAN {} VIRTUAL TABLE INDEX 0:", ff.name.name)
                };
                self.emit(parent, detail)?;
                for a in &ff.args {
                    self.explain_expr_subqueries(a, parent, 0)?;
                }
                Ok(None)
            }
            FromSource::Subquery(fs) => {
                let label = fs
                    .alias
                    .as_ref()
                    .map(|a| a.name.as_str())
                    .unwrap_or("SUBQUERY");
                let sub_id = self.emit(parent, format!("CO-ROUTINE {label}"))?;
                self.explain_select(&fs.select, sub_id)?;
                self.emit(parent, format!("SCAN {label}"))?;
                Ok(None)
            }
            FromSource::Group(fg) => self.explain_from(
                &fg.source,
                preds,
                join_eq_cols,
                proj_info,
                allow_covering,
                from_tables,
                parent,
            ),
            FromSource::Join {
                left,
                right,
                operator,
                on,
                using,
                ..
            } => {
                // Never push WHERE conjuncts into nullable outer-join sides (`LEFT` -> right,
                // `RIGHT` -> left, `FULL` -> both), and never push `ON`/`USING`/`NATURAL` into
                // the preserved RHS of `RIGHT`/`FULL OUTER` joins!
                let is_left_outer = (operator.flags & join_flag::LEFT) != 0;
                let is_right_outer = (operator.flags & join_flag::RIGHT) != 0;
                let left_preds: &[&Expr] = if is_right_outer { &[] } else { preds };
                let _ = self.explain_from(
                    left,
                    left_preds,
                    join_eq_cols,
                    proj_info,
                    false,
                    from_tables,
                    parent,
                )?;
                let mut right_preds: Vec<&Expr> = if is_left_outer || is_right_outer {
                    Vec::new()
                } else {
                    preds.to_vec()
                };
                let mut right_join_eq: Vec<String> = if is_right_outer {
                    Vec::new()
                } else {
                    join_eq_cols.to_vec()
                };
                if !is_right_outer {
                    if let Some(ref on_e) = on {
                        collect_conjuncts(on_e, &mut right_preds);
                    }
                    if let Some(u) = using {
                        for col in u {
                            right_join_eq.push(col.name.clone());
                        }
                    } else if (operator.flags & join_flag::NATURAL) != 0 {
                        let mut lcols = Vec::new();
                        let mut rcols = Vec::new();
                        self.collect_from_col_names(left, &mut lcols, 0)?;
                        self.collect_from_col_names(right, &mut rcols, 0)?;
                        for rc in &rcols {
                            if lcols.iter().any(|lc| lc.eq_ignore_ascii_case(rc)) {
                                right_join_eq.push(rc.clone());
                            }
                        }
                    }
                }
                let _ = self.explain_from(
                    right,
                    &right_preds,
                    &right_join_eq,
                    proj_info,
                    false,
                    from_tables,
                    parent,
                )?;
                if let Some(ref on_e) = on {
                    self.explain_expr_subqueries(on_e, parent, 0)?;
                }
                Ok(None)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn explain_table_access(
        &mut self,
        schema: Option<&str>,
        name: &str,
        alias: Option<&str>,
        indexed_by: Option<&str>,
        not_indexed: bool,
        preds: &[&Expr],
        join_eq_cols: &[String],
        proj_info: Option<(&[ResultColumn], &ExtraRefs<'_>)>,
        allow_covering: bool,
        allow_cte: bool,
        from_tables: &[(String, std::sync::Arc<TableMeta>)],
        parent: i64,
    ) -> SqlResult<Option<(std::sync::Arc<TableMeta>, String, ChosenAccess)>> {
        let label = match alias {
            Some(a) if !a.eq_ignore_ascii_case(name) => format!("{name} AS {a}"),
            _ => name.to_string(),
        };
        let eff_alias = alias.unwrap_or(name);

        // CTEs are only matched when allowed (not DML targets) and no explicit schema qualifier is given.
        if allow_cte
            && schema.is_none()
            && self.ctes.iter().any(|c| c.eq_ignore_ascii_case(name))
        {
            if let Some(idx_name) = indexed_by {
                return Err(SqlError::runtime(format!("no such index: {idx_name}")));
            }
            self.emit(parent, format!("SCAN {label}"))?;
            return Ok(None);
        }

        if let Some(tbl) = self.catalog.get_table_arc(schema, name)? {
            // Validate explicit `INDEXED BY <idx>` against table metadata (allowing partial
            // indexes whenever the query's `WHERE` predicates imply the index's `WHERE` clause).
            if let Some(idx_name) = indexed_by {
                let Some(ix) = tbl
                    .indexes
                    .iter()
                    .find(|ix| ix.name.eq_ignore_ascii_case(idx_name))
                else {
                    return Err(SqlError::runtime(format!("no such index: {idx_name}")));
                };
                if ix.partial
                    && !ix.where_clause.as_ref().is_some_and(|iw| {
                        where_implies_partial_index(preds, iw, &tbl.name, eff_alias)
                    })
                {
                    return Err(SqlError::runtime(format!(
                        "cannot use index: {}",
                        ix.name
                    )));
                }
            }
            let wr_pk = if !tbl.has_rowid {
                self.catalog.get_without_rowid_pk(schema, name)?
            } else {
                None
            };
            let (detail, access) = describe_table_plan(
                &tbl,
                wr_pk.as_ref(),
                &label,
                eff_alias,
                not_indexed,
                indexed_by,
                preds,
                join_eq_cols,
                proj_info,
                allow_covering,
                from_tables,
            );
            self.emit(parent, detail)?;
            return Ok(Some((tbl, eff_alias.to_string(), access)));
        }

        if let Some(view) = self.catalog.get_view(schema, name)? {
            if let Some(idx_name) = indexed_by {
                return Err(SqlError::runtime(format!("no such index: {idx_name}")));
            }
            let lower = view.name.to_ascii_lowercase();
            if self.expanding_views.len() >= MAX_EQP_VIEW_DEPTH
                || self.expanding_views.iter().any(|v| v == &lower)
            {
                return Err(SqlError::runtime(format!(
                    "view {} is circularly defined",
                    view.name
                )));
            }
            self.expanding_views.push(lower);
            // Views execute in their own CTE scope — outer CTEs must not leak inside.
            let saved_ctes = std::mem::take(&mut self.ctes);
            let sub_id = self.emit(parent, format!("CO-ROUTINE {label}"))?;
            let parse_opts = ParseOptions {
                dialect: self.options.dialect,
                ..ParseOptions::default()
            };
            let parsed = match self.options.meter.as_deref() {
                Some(m) => xdb_parser::parse_statement_metered(&view.select_sql, &parse_opts, m),
                None => xdb_parser::parse_statement(&view.select_sql, &parse_opts),
            };
            let expand_res = match parsed {
                Ok(Statement::CreateView(cv)) => self.explain_select(&cv.select, sub_id),
                Ok(Statement::Select(sel)) => self.explain_select(&sel, sub_id),
                Ok(_) => Err(SqlError::runtime(format!(
                    "invalid view definition for {}",
                    view.name
                ))),
                Err(e) => Err(SqlError::from(e)),
            };
            self.ctes = saved_ctes;
            self.expanding_views.pop();
            expand_res?;
            self.emit(parent, format!("SCAN {label}"))?;
            return Ok(None);
        }

        // Bare 0-argument pragma / built-in table-valued functions (`SELECT * FROM pragma_table_list`).
        if schema.is_none()
            && (self.catalog.has_table_valued_function(name) || is_builtin_tvf(name))
        {
            if let Some(idx_name) = indexed_by {
                return Err(SqlError::runtime(format!("no such index: {idx_name}")));
            }
            self.emit(parent, format!("SCAN {label} VIRTUAL TABLE INDEX 0:"))?;
            return Ok(None);
        }

        Err(SqlError::runtime(format!("no such table: {name}")))
    }
}

struct ExtraRefs<'a> {
    where_clause: Option<&'a Expr>,
    group_by: Option<&'a [Expr]>,
    having: Option<&'a Expr>,
    order_by: Option<&'a [OrderingTerm]>,
    windows: Option<&'a [WindowDefinition]>,
}

fn collect_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    let mut stack = vec![expr];
    while let Some(cur) = stack.pop() {
        if let Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } = cur
        {
            stack.push(right);
            stack.push(left);
        } else {
            out.push(cur);
        }
    }
}

fn unwrap_collate(expr: &Expr) -> (&Expr, Option<&str>) {
    if let Expr::Collate {
        operand, collation, ..
    } = expr
    {
        (operand, Some(collation.name.as_str()))
    } else {
        (expr, None)
    }
}

fn is_rowid_ref(expr: &Expr, tbl: &TableMeta, alias: &str) -> bool {
    if !tbl.has_rowid {
        return false;
    }
    let Expr::Column(c) = expr else {
        return false;
    };
    if let Some(ref t) = c.table {
        if !t.name.eq_ignore_ascii_case(alias) {
            return false;
        }
    }
    // Real declared columns take precedence over rowid aliases (`rowid`/`_rowid_`/`oid`).
    if let Some(cid) = tbl
        .columns
        .iter()
        .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
    {
        return tbl.ipk_column == Some(cid);
    }
    matches!(
        c.column.name.to_ascii_lowercase().as_str(),
        "rowid" | "_rowid_" | "oid"
    )
}

fn col_ref_cid(expr: &Expr, tbl: &TableMeta, alias: &str) -> Option<usize> {
    let Expr::Column(c) = expr else {
        return None;
    };
    if let Some(ref t) = c.table {
        if !t.name.eq_ignore_ascii_case(alias) {
            return None;
        }
    }
    tbl.columns
        .iter()
        .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
}

fn window_spec_refs_table_col(ws: &WindowSpec, tbl: &TableMeta, alias: &str) -> bool {
    ws.partition_by
        .as_deref()
        .is_some_and(|es| es.iter().any(|e| expr_refs_table_col(e, tbl, alias)))
        || ws
            .order_by
            .as_deref()
            .is_some_and(|obs| obs.iter().any(|o| expr_refs_table_col(&o.expr, tbl, alias)))
        || ws.frame.as_ref().is_some_and(|fr| {
            fr.from
                .expr
                .as_deref()
                .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
                || fr
                    .to
                    .as_ref()
                    .and_then(|b| b.expr.as_deref())
                    .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
        })
}

fn select_refs_table_col(sel: &SelectStmt, tbl: &TableMeta, alias: &str) -> bool {
    fn body_refs(body: &SelectBody, tbl: &TableMeta, alias: &str) -> bool {
        match body {
            SelectBody::Core(core) => {
                core.columns.iter().any(|rc| match rc {
                    ResultColumn::Expr(re) => expr_refs_table_col(&re.expr, tbl, alias),
                    ResultColumn::Star(_) => false,
                }) || core
                    .where_clause
                    .as_ref()
                    .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
                    || core
                        .group_by
                        .as_deref()
                        .is_some_and(|gs| gs.iter().any(|e| expr_refs_table_col(e, tbl, alias)))
                    || core
                        .having
                        .as_ref()
                        .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
            }
            SelectBody::Values(v) => v
                .rows
                .iter()
                .any(|r| r.items.iter().any(|e| expr_refs_table_col(e, tbl, alias))),
            SelectBody::Compound { left, right, .. } => {
                body_refs(left, tbl, alias) || body_refs(right, tbl, alias)
            }
        }
    }
    body_refs(&sel.body, tbl, alias)
        || sel
            .order_by
            .as_deref()
            .is_some_and(|obs| obs.iter().any(|o| expr_refs_table_col(&o.expr, tbl, alias)))
}

fn expr_refs_table_col(expr: &Expr, tbl: &TableMeta, alias: &str) -> bool {
    match expr {
        Expr::Column(c) => {
            if let Some(ref t) = c.table {
                t.name.eq_ignore_ascii_case(alias)
            } else {
                tbl.columns
                    .iter()
                    .any(|col| col.name.eq_ignore_ascii_case(&c.column.name))
                    || (tbl.has_rowid
                        && matches!(
                            c.column.name.to_ascii_lowercase().as_str(),
                            "rowid" | "_rowid_" | "oid"
                        ))
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => expr_refs_table_col(operand, tbl, alias),
        Expr::Binary { left, right, .. } => {
            expr_refs_table_col(left, tbl, alias) || expr_refs_table_col(right, tbl, alias)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_refs_table_col(left, tbl, alias)
                || expr_refs_table_col(right, tbl, alias)
                || escape
                    .as_deref()
                    .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_refs_table_col(operand, tbl, alias)
                || expr_refs_table_col(low, tbl, alias)
                || expr_refs_table_col(high, tbl, alias)
        }
        Expr::InList { operand, items, .. } => {
            expr_refs_table_col(operand, tbl, alias)
                || items.iter().any(|i| expr_refs_table_col(i, tbl, alias))
        }
        Expr::Subquery { select, .. } | Expr::Exists { select, .. } => {
            select_refs_table_col(select, tbl, alias)
        }
        Expr::InSelect {
            operand, select, ..
        } => expr_refs_table_col(operand, tbl, alias) || select_refs_table_col(select, tbl, alias),
        Expr::InTable { operand, .. } => expr_refs_table_col(operand, tbl, alias),
        Expr::Function(f) => {
            f.args.iter().any(|a| expr_refs_table_col(a, tbl, alias))
                || f.order_by
                    .as_deref()
                    .is_some_and(|obs| obs.iter().any(|o| expr_refs_table_col(&o.expr, tbl, alias)))
                || f.filter
                    .as_deref()
                    .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
                || matches!(
                    f.over,
                    Some(WindowRef::Spec(ref ws)) if window_spec_refs_table_col(ws, tbl, alias)
                )
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
                || whens.iter().any(|w| {
                    expr_refs_table_col(&w.when, tbl, alias)
                        || expr_refs_table_col(&w.then, tbl, alias)
                })
                || else_expr
                    .as_deref()
                    .is_some_and(|e| expr_refs_table_col(e, tbl, alias))
        }
        Expr::Vector { items, .. } => items.iter().any(|i| expr_refs_table_col(i, tbl, alias)),
        Expr::Raise {
            message: Some(m), ..
        } => expr_refs_table_col(m, tbl, alias),
        _ => false,
    }
}

fn window_spec_covered_by_index(
    ws: &WindowSpec,
    tbl: &TableMeta,
    alias: &str,
    key_columns: &[Option<usize>],
) -> bool {
    ws.partition_by
        .as_deref()
        .is_none_or(|es| es.iter().all(|e| expr_covered_by_index(e, tbl, alias, key_columns)))
        && ws.order_by.as_deref().is_none_or(|obs| {
            obs.iter()
                .all(|o| expr_covered_by_index(&o.expr, tbl, alias, key_columns))
        })
        && ws.frame.as_ref().is_none_or(|fr| {
            fr.from
                .expr
                .as_deref()
                .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                && fr
                    .to
                    .as_ref()
                    .and_then(|b| b.expr.as_deref())
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
        })
}

fn expr_covered_by_index(
    expr: &Expr,
    tbl: &TableMeta,
    alias: &str,
    key_columns: &[Option<usize>],
) -> bool {
    match expr {
        Expr::Column(_) => {
            if is_rowid_ref(expr, tbl, alias) {
                true
            } else if let Some(cid) = col_ref_cid(expr, tbl, alias) {
                key_columns.contains(&Some(cid))
            } else {
                false
            }
        }
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::Variable(_)
        | Expr::CurrentTime { .. } => true,
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => {
            expr_covered_by_index(operand, tbl, alias, key_columns)
        }
        Expr::Binary { left, right, .. } => {
            expr_covered_by_index(left, tbl, alias, key_columns)
                && expr_covered_by_index(right, tbl, alias, key_columns)
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            expr_covered_by_index(left, tbl, alias, key_columns)
                && expr_covered_by_index(right, tbl, alias, key_columns)
                && escape
                    .as_deref()
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_covered_by_index(operand, tbl, alias, key_columns)
                && expr_covered_by_index(low, tbl, alias, key_columns)
                && expr_covered_by_index(high, tbl, alias, key_columns)
        }
        Expr::InList { operand, items, .. } => {
            expr_covered_by_index(operand, tbl, alias, key_columns)
                && items
                    .iter()
                    .all(|i| expr_covered_by_index(i, tbl, alias, key_columns))
        }
        Expr::Function(f) => {
            f.args
                .iter()
                .all(|a| expr_covered_by_index(a, tbl, alias, key_columns))
                && f.order_by.as_deref().is_none_or(|obs| {
                    obs.iter()
                        .all(|o| expr_covered_by_index(&o.expr, tbl, alias, key_columns))
                })
                && f.filter
                    .as_deref()
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                && match f.over {
                    Some(WindowRef::Spec(ref ws)) => {
                        window_spec_covered_by_index(ws, tbl, alias, key_columns)
                    }
                    _ => true,
                }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                && whens.iter().all(|w| {
                    expr_covered_by_index(&w.when, tbl, alias, key_columns)
                        && expr_covered_by_index(&w.then, tbl, alias, key_columns)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
        }
        Expr::Vector { items, .. } => items
            .iter()
            .all(|i| expr_covered_by_index(i, tbl, alias, key_columns)),
        _ => false,
    }
}

fn check_index_covering(
    allow_covering: bool,
    proj_info: Option<(&[ResultColumn], &ExtraRefs<'_>)>,
    tbl: &TableMeta,
    alias: &str,
    key_columns: &[Option<usize>],
) -> bool {
    allow_covering
        && proj_info.is_some_and(|(rcs, extra)| {
            !rcs.is_empty()
                && rcs.iter().all(|rc| match rc {
                    ResultColumn::Star(_) => false,
                    ResultColumn::Expr(re) => {
                        expr_covered_by_index(&re.expr, tbl, alias, key_columns)
                    }
                })
                && extra
                    .where_clause
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                && extra.group_by.is_none_or(|gs| {
                    gs.iter()
                        .all(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                })
                && extra
                    .having
                    .is_none_or(|e| expr_covered_by_index(e, tbl, alias, key_columns))
                && extra.order_by.is_none_or(|obs| {
                    obs.iter()
                        .all(|o| expr_covered_by_index(&o.expr, tbl, alias, key_columns))
                })
                && extra.windows.is_none_or(|wins| {
                    wins.iter()
                        .all(|wd| window_spec_covered_by_index(&wd.window, tbl, alias, key_columns))
                })
        })
}

fn lookup_from_col_collation<'a>(
    expr: &Expr,
    from_tables: &'a [(String, std::sync::Arc<TableMeta>)],
) -> Option<&'a str> {
    let Expr::Column(c) = expr else {
        return None;
    };
    if let Some(ref tbl_qual) = c.table {
        for (alias, tmeta) in from_tables {
            if alias.eq_ignore_ascii_case(&tbl_qual.name) {
                if let Some(col) = tmeta
                    .columns
                    .iter()
                    .find(|tc| tc.name.eq_ignore_ascii_case(&c.column.name))
                {
                    return Some(col.collation.as_str());
                }
            }
        }
        return None;
    }
    let mut found: Option<&'a str> = None;
    for (_, tmeta) in from_tables {
        if let Some(col) = tmeta
            .columns
            .iter()
            .find(|tc| tc.name.eq_ignore_ascii_case(&c.column.name))
        {
            if found.is_some() {
                return None;
            }
            found = Some(col.collation.as_str());
        }
    }
    found
}

/// Match conjuncts against an index's leading key prefix (`col1=? [AND col2=? ...] [AND colk>? AND colk<?]`).
fn match_index_prefix(
    ix: &IndexMeta,
    tbl: &TableMeta,
    alias: &str,
    conjs: &[&Expr],
    join_eq_cols: &[String],
    from_tables: &[(String, std::sync::Arc<TableMeta>)],
) -> Option<(String, usize)> {
    let mut parts: Vec<String> = Vec::new();
    let mut eq_prefix_len = 0usize;
    for (k, opt_cid) in ix.key_columns.iter().enumerate() {
        let Some(cid) = *opt_cid else {
            break;
        };
        let col = &tbl.columns[cid];
        let cname = &col.name;
        let ix_coll = ix
            .key_collations
            .get(k)
            .map(|s| s.as_str())
            .unwrap_or("BINARY");
        let mut has_single_eq = join_eq_cols.iter().any(|jc| jc.eq_ignore_ascii_case(cname))
            && col.collation.eq_ignore_ascii_case(ix_coll);
        let mut has_multi_in = false;
        let mut has_lo = false;
        let mut has_hi = false;

        for conj in conjs {
            match conj {
                Expr::Binary {
                    op, left, right, ..
                } => {
                    let (l_inner, l_coll) = unwrap_collate(left);
                    let (r_inner, r_coll) = unwrap_collate(right);
                    let (matched_op, eff_coll) =
                        if col_ref_cid(l_inner, tbl, alias) == Some(cid)
                            && !expr_refs_table_col(right, tbl, alias)
                        {
                            // `tbl.col OP other`: explicit LHS > explicit RHS > implicit LHS (`col.collation`).
                            (Some(*op), l_coll.or(r_coll).unwrap_or(col.collation.as_str()))
                        } else if col_ref_cid(r_inner, tbl, alias) == Some(cid)
                            && !expr_refs_table_col(left, tbl, alias)
                        {
                            let flipped = match op {
                                BinaryOp::Lt => BinaryOp::Gt,
                                BinaryOp::Le => BinaryOp::Ge,
                                BinaryOp::Gt => BinaryOp::Lt,
                                BinaryOp::Ge => BinaryOp::Le,
                                other => *other,
                            };
                            // `other OP tbl.col`: explicit LHS > explicit RHS > implicit LHS (`other`) > implicit RHS (`col.collation`).
                            let l_impl = lookup_from_col_collation(l_inner, from_tables);
                            (
                                Some(flipped),
                                l_coll
                                    .or(r_coll)
                                    .or(l_impl)
                                    .unwrap_or(col.collation.as_str()),
                            )
                        } else {
                            (None, col.collation.as_str())
                        };
                    if let Some(mop) = matched_op {
                        if !eff_coll.eq_ignore_ascii_case(ix_coll) {
                            continue;
                        }
                        match mop {
                            BinaryOp::Eq | BinaryOp::Is => has_single_eq = true,
                            BinaryOp::Ge | BinaryOp::Gt => has_lo = true,
                            BinaryOp::Le | BinaryOp::Lt => has_hi = true,
                            _ => {}
                        }
                    }
                }
                Expr::IsNull {
                    not: false,
                    operand,
                    ..
                } if col_ref_cid(operand, tbl, alias) == Some(cid) => {
                    has_single_eq = true;
                }
                Expr::Between {
                    not: false,
                    operand,
                    low,
                    high,
                    ..
                } => {
                    let (op_inner, op_coll) = unwrap_collate(operand);
                    let eff_coll = op_coll.unwrap_or(col.collation.as_str());
                    if col_ref_cid(op_inner, tbl, alias) == Some(cid)
                        && !expr_refs_table_col(low, tbl, alias)
                        && !expr_refs_table_col(high, tbl, alias)
                        && eff_coll.eq_ignore_ascii_case(ix_coll)
                    {
                        has_lo = true;
                        has_hi = true;
                    }
                }
                Expr::InList {
                    not: false,
                    operand,
                    items,
                    ..
                } => {
                    let (op_inner, op_coll) = unwrap_collate(operand);
                    let eff_coll = op_coll.unwrap_or(col.collation.as_str());
                    if col_ref_cid(op_inner, tbl, alias) == Some(cid)
                        && items.iter().all(|i| !expr_refs_table_col(i, tbl, alias))
                        && eff_coll.eq_ignore_ascii_case(ix_coll)
                    {
                        if items.len() == 1 {
                            has_single_eq = true;
                        } else {
                            has_multi_in = true;
                        }
                    }
                }
                _ => {}
            }
        }

        if has_single_eq {
            parts.push(format!("{cname}=?"));
            if eq_prefix_len == k {
                eq_prefix_len = k + 1;
            }
        } else if has_multi_in {
            // Multi-value IN(...) still uses index lookup, but breaks global sort order on
            // subsequent index columns (`WHERE a IN (1,2) ORDER BY b` needs a temp B-tree).
            parts.push(format!("{cname}=?"));
        } else if has_lo || has_hi {
            match (has_lo, has_hi) {
                (true, true) => parts.push(format!("{cname}>? AND {cname}<?")),
                (true, false) => parts.push(format!("{cname}>?")),
                (false, true) => parts.push(format!("{cname}<?")),
                (false, false) => unreachable!(),
            }
            break;
        } else {
            break;
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some((parts.join(" AND "), eq_prefix_len))
    }
}

#[allow(clippy::too_many_arguments)]
fn describe_table_plan(
    tbl: &TableMeta,
    wr_pk: Option<&IndexMeta>,
    label: &str,
    alias: &str,
    not_indexed: bool,
    forced_index: Option<&str>,
    conjs: &[&Expr],
    join_eq_cols: &[String],
    proj_info: Option<(&[ResultColumn], &ExtraRefs<'_>)>,
    allow_covering: bool,
    from_tables: &[(String, std::sync::Arc<TableMeta>)],
) -> (String, ChosenAccess) {
    // 1. Check rowid / INTEGER PRIMARY KEY lookup or range (unless forced to a specific secondary index).
    if forced_index.is_none() && tbl.has_rowid {
        let ipk_join_eq = tbl.ipk_column.is_some_and(|ipk| {
            let cname = &tbl.columns[ipk].name;
            join_eq_cols.iter().any(|jc| jc.eq_ignore_ascii_case(cname))
        });
        if ipk_join_eq {
            return (
                format!("SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"),
                ChosenAccess::Rowid,
            );
        }
        let mut rid_lo = false;
        let mut rid_hi = false;
        for conj in conjs {
            match conj {
                Expr::Binary {
                    op, left, right, ..
                } => {
                    let matched_op = if is_rowid_ref(left, tbl, alias)
                        && !expr_refs_table_col(right, tbl, alias)
                    {
                        Some(*op)
                    } else if is_rowid_ref(right, tbl, alias)
                        && !expr_refs_table_col(left, tbl, alias)
                    {
                        Some(match op {
                            BinaryOp::Lt => BinaryOp::Gt,
                            BinaryOp::Le => BinaryOp::Ge,
                            BinaryOp::Gt => BinaryOp::Lt,
                            BinaryOp::Ge => BinaryOp::Le,
                            other => *other,
                        })
                    } else {
                        None
                    };
                    match matched_op {
                        Some(BinaryOp::Eq) => {
                            return (
                                format!("SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"),
                                ChosenAccess::Rowid,
                            );
                        }
                        Some(BinaryOp::Ge | BinaryOp::Gt) => rid_lo = true,
                        Some(BinaryOp::Le | BinaryOp::Lt) => rid_hi = true,
                        _ => {}
                    }
                }
                Expr::Between {
                    not: false,
                    operand,
                    low,
                    high,
                    ..
                } if is_rowid_ref(operand, tbl, alias)
                    && !expr_refs_table_col(low, tbl, alias)
                    && !expr_refs_table_col(high, tbl, alias) =>
                {
                    rid_lo = true;
                    rid_hi = true;
                }
                Expr::InList {
                    not: false,
                    operand,
                    items,
                    ..
                } if is_rowid_ref(operand, tbl, alias)
                    && items.iter().all(|i| !expr_refs_table_col(i, tbl, alias)) =>
                {
                    return (
                        format!("SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"),
                        ChosenAccess::Rowid,
                    );
                }
                _ => {}
            }
        }
        if rid_lo || rid_hi {
            let cond = match (rid_lo, rid_hi) {
                (true, true) => "rowid>? AND rowid<?",
                (true, false) => "rowid>?",
                (false, true) => "rowid<?",
                (false, false) => unreachable!(),
            };
            return (
                format!("SEARCH {label} USING INTEGER PRIMARY KEY ({cond})"),
                ChosenAccess::Rowid,
            );
        }
    }

    // 2. For `WITHOUT ROWID` tables, check primary key search first (unless forced to secondary index).
    if forced_index.is_none() && !tbl.has_rowid {
        if let Some(pk) = wr_pk {
            if let Some((cond, eq_prefix_len)) =
                match_index_prefix(pk, tbl, alias, conjs, join_eq_cols, from_tables)
            {
                return (
                    format!("SEARCH {label} USING PRIMARY KEY ({cond})"),
                    ChosenAccess::Index {
                        key_columns: pk.key_columns.clone(),
                        key_collations: pk.key_collations.clone(),
                        key_desc: pk.key_desc.clone(),
                        eq_prefix_len,
                    },
                );
            }
        }
    }

    if not_indexed {
        return (format!("SCAN {label}"), ChosenAccess::Scan);
    }

    // 3. Check secondary indexes (prefer longest matched prefix / forced index).
    let mut best: Option<(usize, String, ChosenAccess)> = None;
    for ix in &tbl.indexes {
        if ix.partial
            && !ix
                .where_clause
                .as_ref()
                .is_some_and(|iw| where_implies_partial_index(conjs, iw, &tbl.name, alias))
        {
            continue;
        }
        if let Some(fi) = forced_index {
            if !ix.name.eq_ignore_ascii_case(fi) {
                continue;
            }
        }
        if let Some((cond, eq_prefix_len)) =
            match_index_prefix(ix, tbl, alias, conjs, join_eq_cols, from_tables)
        {
            let covering = check_index_covering(allow_covering, proj_info, tbl, alias, &ix.key_columns);
            let kind = if covering { "COVERING INDEX" } else { "INDEX" };
            let detail = format!("SEARCH {label} USING {kind} {} ({cond})", ix.name);
            let score = eq_prefix_len * 2 + usize::from(covering) + 1;
            let access = ChosenAccess::Index {
                key_columns: ix.key_columns.clone(),
                key_collations: ix.key_collations.clone(),
                key_desc: ix.key_desc.clone(),
                eq_prefix_len,
            };
            if best.as_ref().is_none_or(|(bs, _, _)| score > *bs) {
                best = Some((score, detail, access));
            }
        } else if let Some(fi) = forced_index {
            if ix.name.eq_ignore_ascii_case(fi) {
                let covering =
                    check_index_covering(allow_covering, proj_info, tbl, alias, &ix.key_columns);
                let kind = if covering { "COVERING INDEX" } else { "INDEX" };
                return (
                    format!("SCAN {label} USING {kind} {}", ix.name),
                    ChosenAccess::Index {
                        key_columns: ix.key_columns.clone(),
                        key_collations: ix.key_collations.clone(),
                        key_desc: ix.key_desc.clone(),
                        eq_prefix_len: 0,
                    },
                );
            }
        }
    }
    if let Some((_, detail, access)) = best {
        return (detail, access);
    }

    // Default scan access (rowid order for normal tables, PK order for WITHOUT ROWID).
    if let Some(pk) = wr_pk {
        return (
            format!("SCAN {label}"),
            ChosenAccess::Index {
                key_columns: pk.key_columns.clone(),
                key_collations: pk.key_collations.clone(),
                key_desc: pk.key_desc.clone(),
                eq_prefix_len: 0,
            },
        );
    }
    (format!("SCAN {label}"), ChosenAccess::Scan)
}

/// Returns true if an unqualified column reference in `ORDER BY` is shadowed by an explicit
/// `SELECT` output alias (e.g. `SELECT b AS id FROM t ORDER BY id`).
fn order_by_unqual_shadowed_by_alias(core: &SelectCore, expr: &Expr) -> bool {
    let Expr::Column(c) = expr else {
        return false;
    };
    if c.table.is_some() {
        return false;
    }
    core.columns.iter().any(|rc| {
        matches!(
            rc,
            ResultColumn::Expr(re)
                if re.alias.as_ref().is_some_and(|a| a.name.eq_ignore_ascii_case(&c.column.name))
        )
    })
}

fn expr_has_window_call(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) if f.over.is_some() => true,
        Expr::Subquery { .. } | Expr::Exists { .. } => false,
        Expr::InSelect { operand, .. } => expr_has_window_call(operand),
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .any(expr_has_window_call),
    }
}

fn core_has_window(core: &SelectCore) -> bool {
    core.windows.is_some()
        || core.columns.iter().any(|rc| match rc {
            ResultColumn::Expr(re) => expr_has_window_call(&re.expr),
            ResultColumn::Star(rs) => rs
                .replace
                .as_deref()
                .is_some_and(|reps| reps.iter().any(|r| expr_has_window_call(&r.expr))),
        })
}

fn order_by_satisfied_by_single_table(
    core: &SelectCore,
    tbl: &TableMeta,
    alias: &str,
    access: &ChosenAccess,
    obs: &[OrderingTerm],
) -> bool {
    if core.group_by.is_some()
        || core.group_by_all
        || core.distinct == Some(DistinctKind::Distinct)
        || core_has_window(core)
        || obs.is_empty()
    {
        return false;
    }
    // Single-term `ORDER BY rowid` / `ORDER BY <ipk>` on a rowid table (Scan or Rowid access).
    if matches!(access, ChosenAccess::Scan | ChosenAccess::Rowid) && tbl.has_rowid && obs.len() == 1
    {
        let (inner, coll) = unwrap_collate(&obs[0].expr);
        if coll.is_none()
            && obs[0].nulls.is_none()
            && !order_by_unqual_shadowed_by_alias(core, inner)
            && is_rowid_ref(inner, tbl, alias)
        {
            return true;
        }
    }
    // Index or WITHOUT ROWID PK scan/search matching ORDER BY suffix after equality prefix.
    if let ChosenAccess::Index {
        key_columns,
        key_collations,
        key_desc,
        eq_prefix_len,
    } = access
    {
        let mut dir_match: Option<bool> = None; // true = forward, false = reverse
        for (i, ot) in obs.iter().enumerate() {
            if ot.nulls.is_some() {
                return false;
            }
            let k = eq_prefix_len + i;
            let Some(Some(kcid)) = key_columns.get(k) else {
                return false;
            };
            let (inner, explicit_coll) = unwrap_collate(&ot.expr);
            if order_by_unqual_shadowed_by_alias(core, inner) {
                return false;
            }
            if col_ref_cid(inner, tbl, alias) != Some(*kcid) {
                return false;
            }
            let want_coll = explicit_coll.unwrap_or(tbl.columns[*kcid].collation.as_str());
            let ix_coll = key_collations
                .get(k)
                .map(|s| s.as_str())
                .unwrap_or("BINARY");
            if !want_coll.eq_ignore_ascii_case(ix_coll) {
                return false;
            }
            let want_desc = ot.order == Some(SortOrder::Desc);
            let ix_is_desc = key_desc.get(k).copied().unwrap_or(false);
            let forward = want_desc == ix_is_desc;
            if let Some(prev) = dir_match {
                if prev != forward {
                    return false;
                }
            } else {
                dir_match = Some(forward);
            }
        }
        return true;
    }
    false
}

/// Returns true if `expr` may carry a column or explicit collation sequence
/// (so commuting `a op b` to `b flip(op) a` could change the comparison collation).
fn expr_may_carry_collation(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) | Expr::Collate { .. } | Expr::Subquery { .. } => true,
        Expr::Unary {
            op: xdb_parser::UnaryOp::Pos,
            operand,
            ..
        } => expr_may_carry_collation(operand),
        Expr::Case {
            whens, else_expr, ..
        } => {
            whens.iter().any(|w| expr_may_carry_collation(&w.then))
                || else_expr.as_deref().is_some_and(expr_may_carry_collation)
        }
        _ => false,
    }
}

/// Structural equality helper for checking whether a query's `WHERE` conjunct implies a
/// partial index `WHERE` predicate (case-insensitive on identifiers/functions/collations,
/// allowing `alias.col` to match unqualified `col`).
#[allow(clippy::only_used_in_recursion)]
pub fn expr_matches_partial_index(
    query_expr: &Expr,
    idx_expr: &Expr,
    table_name: &str,
    alias: &str,
) -> bool {
    match (query_expr, idx_expr) {
        (Expr::Column(qc), Expr::Column(ic)) => {
            if !qc.column.name.eq_ignore_ascii_case(&ic.column.name) {
                return false;
            }
            if let Some(ref qt) = qc.table {
                if !qt.name.eq_ignore_ascii_case(alias) {
                    return false;
                }
            }
            true
        }
        (Expr::Number(a), Expr::Number(b)) => a.text == b.text,
        (Expr::String(a), Expr::String(b)) => a.value == b.value,
        (Expr::Blob(a), Expr::Blob(b)) => a.value == b.value,
        (Expr::Null { .. }, Expr::Null { .. }) => true,
        (Expr::Boolean { value: a, .. }, Expr::Boolean { value: b, .. }) => a == b,
        (
            Expr::Unary {
                op: oa,
                operand: xa,
                ..
            },
            Expr::Unary {
                op: ob,
                operand: xb,
                ..
            },
        ) => oa == ob && expr_matches_partial_index(xa, xb, table_name, alias),
        (
            Expr::IsNull {
                not: na,
                operand: xa,
                ..
            },
            Expr::IsNull {
                not: nb,
                operand: xb,
                ..
            },
        ) => na == nb && expr_matches_partial_index(xa, xb, table_name, alias),
        (
            Expr::Binary {
                op: oa,
                left: la,
                right: ra,
                ..
            },
            Expr::Binary {
                op: ob,
                left: lb,
                right: rb,
                ..
            },
        ) => {
            if oa == ob
                && expr_matches_partial_index(la, lb, table_name, alias)
                && expr_matches_partial_index(ra, rb, table_name, alias)
            {
                return true;
            }
            let flipped = match ob {
                BinaryOp::Eq => Some(BinaryOp::Eq),
                BinaryOp::Ne => Some(BinaryOp::Ne),
                BinaryOp::Is => Some(BinaryOp::Is),
                BinaryOp::IsNot => Some(BinaryOp::IsNot),
                BinaryOp::Lt => Some(BinaryOp::Gt),
                BinaryOp::Le => Some(BinaryOp::Ge),
                BinaryOp::Gt => Some(BinaryOp::Lt),
                BinaryOp::Ge => Some(BinaryOp::Le),
                _ => None,
            };
            // Only commute `a op b` <-> `b flip(op) a` when at least one side cannot carry a
            // column/explicit collation; otherwise flipping reverses LHS collation precedence.
            flipped == Some(*oa)
                && (!expr_may_carry_collation(la) || !expr_may_carry_collation(ra))
                && expr_matches_partial_index(la, rb, table_name, alias)
                && expr_matches_partial_index(ra, lb, table_name, alias)
        }
        (
            Expr::Between {
                not: na,
                operand: oa,
                low: la,
                high: ha,
                ..
            },
            Expr::Between {
                not: nb,
                operand: ob,
                low: lb,
                high: hb,
                ..
            },
        ) => {
            na == nb
                && expr_matches_partial_index(oa, ob, table_name, alias)
                && expr_matches_partial_index(la, lb, table_name, alias)
                && expr_matches_partial_index(ha, hb, table_name, alias)
        }
        (
            Expr::InList {
                not: na,
                operand: oa,
                items: ia,
                ..
            },
            Expr::InList {
                not: nb,
                operand: ob,
                items: ib,
                ..
            },
        ) => {
            na == nb
                && ia.len() == ib.len()
                && expr_matches_partial_index(oa, ob, table_name, alias)
                && ia
                    .iter()
                    .zip(ib)
                    .all(|(x, y)| expr_matches_partial_index(x, y, table_name, alias))
        }
        (
            Expr::Collate {
                operand: oa,
                collation: ca,
                ..
            },
            Expr::Collate {
                operand: ob,
                collation: cb,
                ..
            },
        ) => {
            ca.name.eq_ignore_ascii_case(&cb.name)
                && expr_matches_partial_index(oa, ob, table_name, alias)
        }
        (Expr::Function(fa), Expr::Function(fb)) => {
            fa.name.name.eq_ignore_ascii_case(&fb.name.name)
                && fa.star == fb.star
                && fa.quantifier == fb.quantifier
                && fa.args.len() == fb.args.len()
                && fa
                    .args
                    .iter()
                    .zip(&fb.args)
                    .all(|(x, y)| expr_matches_partial_index(x, y, table_name, alias))
        }
        (
            Expr::Cast {
                operand: oa,
                type_name: ta,
                ..
            },
            Expr::Cast {
                operand: ob,
                type_name: tb,
                ..
            },
        ) => {
            expr_matches_partial_index(oa, ob, table_name, alias)
                && ta.as_ref().map(|t| t.text.to_ascii_uppercase())
                    == tb.as_ref().map(|t| t.text.to_ascii_uppercase())
        }
        _ => false,
    }
}

/// Returns `true` if `where_preds` (flattened `AND` conjuncts from a query's `WHERE` clause)
/// imply every conjunct of a partial index's `WHERE` predicate `ix_where`.
pub fn where_implies_partial_index(
    where_preds: &[&Expr],
    ix_where: &Expr,
    table_name: &str,
    alias: &str,
) -> bool {
    if where_preds.is_empty() {
        return false;
    }
    let mut ix_conjs = Vec::new();
    collect_conjuncts(ix_where, &mut ix_conjs);
    ix_conjs.iter().all(|ic| {
        where_preds
            .iter()
            .any(|qp| expr_matches_partial_index(qp, ic, table_name, alias))
    })
}

fn returning_contains_raise(rcs: &[ResultColumn]) -> bool {
    rcs.iter().any(|rc| match rc {
        ResultColumn::Expr(re) => crate::eval::expr_contains_raise(&re.expr),
        ResultColumn::Star(rs) => rs
            .replace
            .as_deref()
            .is_some_and(|reps| reps.iter().any(|r| crate::eval::expr_contains_raise(&r.expr))),
    })
}

fn stmt_contains_raise(stmt: &Statement) -> bool {
    match stmt {
        Statement::Select(sel) => crate::eval::select_contains_raise(sel),
        Statement::Insert(ins) => {
            ins.with
                .as_ref()
                .is_some_and(|w| w.ctes.iter().any(|c| crate::eval::select_contains_raise(&c.select)))
                || ins
                    .select
                    .as_deref()
                    .is_some_and(crate::eval::select_contains_raise)
                || ins.upsert.iter().any(|u| {
                    u.target_where
                        .as_ref()
                        .is_some_and(crate::eval::expr_contains_raise)
                        || u.where_clause
                            .as_ref()
                            .is_some_and(crate::eval::expr_contains_raise)
                        || u.set.as_deref().is_some_and(|sets| {
                            sets.iter()
                                .any(|sc| crate::eval::expr_contains_raise(&sc.expr))
                        })
                })
                || ins.returning.as_deref().is_some_and(returning_contains_raise)
        }
        Statement::Update(upd) => {
            upd.with
                .as_ref()
                .is_some_and(|w| w.ctes.iter().any(|c| crate::eval::select_contains_raise(&c.select)))
                || upd
                    .from
                    .as_ref()
                    .is_some_and(crate::eval::from_contains_raise)
                || upd
                    .where_clause
                    .as_ref()
                    .is_some_and(crate::eval::expr_contains_raise)
                || upd
                    .set
                    .iter()
                    .any(|sc| crate::eval::expr_contains_raise(&sc.expr))
                || upd.returning.as_deref().is_some_and(returning_contains_raise)
        }
        Statement::Delete(del) => {
            del.with
                .as_ref()
                .is_some_and(|w| w.ctes.iter().any(|c| crate::eval::select_contains_raise(&c.select)))
                || del
                    .where_clause
                    .as_ref()
                    .is_some_and(crate::eval::expr_contains_raise)
                || del.returning.as_deref().is_some_and(returning_contains_raise)
        }
        Statement::Explain { statement, .. } => stmt_contains_raise(statement),
        _ => false,
    }
}

/// Generate a SQLite-compatible `EXPLAIN QUERY PLAN` result (`id, parent, notused, detail`).
pub fn explain_query_plan(
    catalog: &dyn SqlCatalog,
    stmt: &Statement,
    _params: &QueryParams,
    options: &QueryOptions,
) -> SqlResult<QueryResult> {
    if !options.in_trigger && stmt_contains_raise(stmt) {
        return Err(SqlError::runtime(
            "RAISE() may only be used within a trigger-program",
        ));
    }
    let mut b = EqpBuilder::new(catalog, options);
    b.explain_stmt(stmt, 0)?;
    Ok(QueryResult {
        columns: vec![
            "id".to_string(),
            "parent".to_string(),
            "notused".to_string(),
            "detail".to_string(),
        ],
        rows: b.rows,
    })
}
