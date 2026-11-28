//! SQLite 3.54.1 trigger compilation, row-scope construction (`OLD.*` / `NEW.*`),
//! and `ALTER TABLE` trigger DDL rewriting.

use std::sync::Arc;

use xdb_core::{Affinity, SqlValue};
use xdb_parser::{
    expr_children, keyword_code, parse_statement, tk, Expr, FromSource, ParseOptions, SelectBody,
    SelectStmt, Span, Statement, TriggerEventKind, TriggerStep, TriggerTime,
};
use xdb_sql::{EvalScope, ScopeColumn};

use crate::schema::{SchemaCatalog, TableInfo};

/// Maximum nested trigger recursion depth (`SQLITE_MAX_TRIGGER_DEPTH`).
pub(crate) const MAX_TRIGGER_DEPTH: usize = 1000;

/// Pre-parsed trigger ready for execution against a table or view.
#[derive(Debug, Clone)]
pub(crate) struct CompiledTrigger {
    pub name: String,
    pub time: TriggerTime,
    pub event: TriggerEventKind,
    pub update_of: Option<Vec<String>>,
    pub when: Option<Expr>,
    pub body: Vec<TriggerStep>,
}

impl CompiledTrigger {
    /// Returns true if this `UPDATE` trigger matches the set of columns named in `SET`.
    #[inline]
    pub fn matches_update(&self, updated_cols: &[String]) -> bool {
        let Some(ref cols) = self.update_of else {
            return true;
        };
        cols.iter()
            .any(|tc| updated_cols.iter().any(|uc| uc.eq_ignore_ascii_case(tc)))
    }
}

/// Outcome of executing a trigger or trigger list on a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerOutcome {
    /// Normal completion — proceed with the DML row action and remaining triggers.
    Continue,
    /// `RAISE(IGNORE)` fired — skip remaining triggers and skip the DML action on this row.
    Ignore,
}

/// Outer `OLD.*` / `NEW.*` row scope visible inside a trigger `WHEN` clause and body.
///
/// All columns are marked `hidden_unqualified = true` so only qualified `OLD.<col>` / `NEW.<col>`
/// references resolve to this scope, matching SQLite's trigger scoping rules.
#[derive(Debug, Clone)]
pub(crate) struct TriggerRowCtx {
    pub cols: Vec<ScopeColumn>,
    pub vals: Vec<SqlValue>,
    pub hidden_unqualified: Vec<bool>,
}

impl TriggerRowCtx {
    /// Build a trigger scope for a real table with optional `OLD` and/or `NEW` rows.
    pub fn for_table(
        table: &TableInfo,
        old: Option<(Option<i64>, &[SqlValue])>,
        new: Option<(Option<i64>, &[SqlValue])>,
    ) -> Self {
        let cap = (table.columns.len() + 3) * (usize::from(old.is_some()) + usize::from(new.is_some()));
        let mut cols = Vec::with_capacity(cap);
        let mut vals = Vec::with_capacity(cap);
        if let Some((old_rid, old_row)) = old {
            push_table_qualifier(table, "old", old_rid, old_row, &mut cols, &mut vals);
        }
        if let Some((new_rid, new_row)) = new {
            push_table_qualifier(table, "new", new_rid, new_row, &mut cols, &mut vals);
        }
        let hidden_unqualified = vec![true; cols.len()];
        Self {
            cols,
            vals,
            hidden_unqualified,
        }
    }

    /// Build a trigger scope for an `INSTEAD OF` trigger on a view.
    pub fn for_view(
        view_cols: &[ScopeColumn],
        old: Option<&[SqlValue]>,
        new: Option<&[SqlValue]>,
    ) -> Self {
        let cap = view_cols.len() * (usize::from(old.is_some()) + usize::from(new.is_some()));
        let mut cols = Vec::with_capacity(cap);
        let mut vals = Vec::with_capacity(cap);
        if let Some(old_row) = old {
            push_view_qualifier(view_cols, "old", old_row, &mut cols, &mut vals);
        }
        if let Some(new_row) = new {
            push_view_qualifier(view_cols, "new", new_row, &mut cols, &mut vals);
        }
        let hidden_unqualified = vec![true; cols.len()];
        Self {
            cols,
            vals,
            hidden_unqualified,
        }
    }

    #[inline]
    pub fn as_eval_scope(&self) -> EvalScope<'_> {
        let mut sc = EvalScope::simple(&self.cols, &self.vals);
        sc.hidden_unqualified = Some(&self.hidden_unqualified);
        sc
    }
}

fn push_table_qualifier(
    table: &TableInfo,
    qual: &str,
    rowid: Option<i64>,
    row: &[SqlValue],
    cols: &mut Vec<ScopeColumn>,
    vals: &mut Vec<SqlValue>,
) {
    for (cid, col) in table.columns.iter().enumerate() {
        cols.push(ScopeColumn {
            table: Some(qual.to_string()),
            name: col.name.clone(),
            affinity: col.affinity,
            collation: col.collation.clone(),
            is_rowid_alias: false,
        });
        vals.push(row.get(cid).cloned().unwrap_or(SqlValue::Null));
    }
    if !table.without_rowid {
        let rid_val = rowid.map_or(SqlValue::Null, SqlValue::Int);
        for alias in ["rowid", "_rowid_", "oid"] {
            if !table
                .columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(alias))
            {
                cols.push(ScopeColumn {
                    table: Some(qual.to_string()),
                    name: alias.to_string(),
                    affinity: Affinity::Integer,
                    collation: "BINARY".to_string(),
                    is_rowid_alias: false,
                });
                vals.push(rid_val.clone());
            }
        }
    }
}

fn push_view_qualifier(
    view_cols: &[ScopeColumn],
    qual: &str,
    row: &[SqlValue],
    cols: &mut Vec<ScopeColumn>,
    vals: &mut Vec<SqlValue>,
) {
    for (idx, col) in view_cols.iter().enumerate() {
        cols.push(ScopeColumn {
            table: Some(qual.to_string()),
            name: col.name.clone(),
            affinity: col.affinity,
            collation: col.collation.clone(),
            is_rowid_alias: false,
        });
        vals.push(row.get(idx).cloned().unwrap_or(SqlValue::Null));
    }
}

/// Compile all triggers attached to `target_name` (table or view) in SQLite's reverse creation order.
pub(crate) fn compile_triggers_for_target(
    schema: &SchemaCatalog,
    target_name: &str,
) -> Vec<Arc<CompiledTrigger>> {
    let mut out = Vec::new();
    let opts = ParseOptions {
        dialect: xdb_parser::Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    for tr in schema.triggers.iter().rev() {
        if !tr.table_name.eq_ignore_ascii_case(target_name) {
            continue;
        }
        if let Ok(Statement::CreateTrigger(ct)) = parse_statement(&tr.sql, &opts) {
            out.push(Arc::new(CompiledTrigger {
                name: tr.name.clone(),
                time: ct.time.unwrap_or(TriggerTime::Before),
                event: ct.event,
                update_of: ct
                    .update_of
                    .map(|list| list.into_iter().map(|n| n.name).collect()),
                when: ct.when,
                body: ct.body,
            }));
        }
    }
    out
}

// ───────────────────────────── ALTER TABLE trigger DDL rewriters ─────────────────────────────

fn quote_ident_if_needed(name: &str) -> String {
    let simple = !name.is_empty()
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_alphabetic() || b == b'_'
            } else {
                b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
            }
        })
        && keyword_code(name.as_bytes()) == tk::ID;
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

#[inline]
fn is_ident_boundary_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn apply_spans(sql: &str, mut spans: Vec<Span>, replacement: &str) -> String {
    if spans.is_empty() {
        return sql.to_string();
    }
    spans.sort_by_key(|s| (s.start, s.end));
    spans.dedup();
    let already_quoted = replacement.starts_with('"');
    let forced_quoted = if already_quoted {
        None
    } else {
        Some(format!("\"{}\"", replacement.replace('"', "\"\"")))
    };
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + spans.len() * (replacement.len() + 2));
    let mut last = 0usize;
    for sp in spans {
        if sp.start < last || sp.end > sql.len() || sp.start > sp.end {
            continue;
        }
        out.push_str(&sql[last..sp.start]);
        let need_quote = !already_quoted
            && (matches!(bytes.get(sp.start), Some(b'"' | b'`' | b'[' | b'\''))
                || (sp.start > 0 && is_ident_boundary_byte(bytes[sp.start - 1]))
                || (sp.end < bytes.len() && is_ident_boundary_byte(bytes[sp.end])));
        if need_quote {
            out.push_str(forced_quoted.as_deref().unwrap_or(replacement));
        } else {
            out.push_str(replacement);
        }
        last = sp.end;
    }
    out.push_str(&sql[last..]);
    out
}

fn is_shadowed(name: &str, ctes: &[String], aliases: &[String]) -> bool {
    ctes.iter().any(|c| c.eq_ignore_ascii_case(name))
        || aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
}

fn collect_from_aliases(src: &FromSource, aliases: &mut Vec<String>) {
    match src {
        FromSource::Table(ft) => {
            if let Some(ref a) = ft.alias {
                // Any explicit alias (even `FROM t AS t`) shadows the raw table name qualifier.
                aliases.push(a.name.clone());
            }
        }
        FromSource::Function(ff) => {
            if let Some(ref a) = ff.alias {
                aliases.push(a.name.clone());
            }
        }
        FromSource::Subquery(sub) => {
            if let Some(ref a) = sub.alias {
                aliases.push(a.name.clone());
            }
        }
        FromSource::Group(g) => collect_from_aliases(&g.source, aliases),
        FromSource::Join { left, right, .. } => {
            collect_from_aliases(left, aliases);
            collect_from_aliases(right, aliases);
        }
    }
}

fn from_has_unaliased_table(src: &FromSource, old_name: &str, ctes: &[String]) -> bool {
    match src {
        FromSource::Table(ft) => {
            ft.alias.is_none()
                && ft.name.name.eq_ignore_ascii_case(old_name)
                && !ctes.iter().any(|c| c.eq_ignore_ascii_case(old_name))
        }
        FromSource::Group(g) => from_has_unaliased_table(&g.source, old_name, ctes),
        FromSource::Join { left, right, .. } => {
            from_has_unaliased_table(left, old_name, ctes)
                || from_has_unaliased_table(right, old_name, ctes)
        }
        _ => false,
    }
}

fn collect_table_name_spans_in_window_spec(
    ws: &xdb_parser::WindowSpec,
    old_name: &str,
    rewrite_qual: bool,
    ctes: &[String],
    out: &mut Vec<Span>,
) {
    if let Some(ref pbs) = ws.partition_by {
        for e in pbs {
            collect_table_name_spans_in_expr(e, old_name, rewrite_qual, ctes, out);
        }
    }
    if let Some(ref obs) = ws.order_by {
        for ot in obs {
            collect_table_name_spans_in_expr(&ot.expr, old_name, rewrite_qual, ctes, out);
        }
    }
}

fn collect_table_name_spans_in_expr(
    expr: &Expr,
    old_name: &str,
    rewrite_qual: bool,
    ctes: &[String],
    out: &mut Vec<Span>,
) {
    match expr {
        Expr::Column(c) => {
            if rewrite_qual {
                if let Some(ref t) = c.table {
                    if t.name.eq_ignore_ascii_case(old_name) {
                        out.push(t.span);
                    }
                }
            }
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            collect_table_name_spans_in_select(select, old_name, rewrite_qual, ctes, out);
        }
        Expr::InTable { table, .. } => {
            if table.name.eq_ignore_ascii_case(old_name)
                && !ctes.iter().any(|c| c.eq_ignore_ascii_case(old_name))
            {
                out.push(table.span);
            }
        }
        Expr::Function(f) => {
            if let Some(xdb_parser::WindowRef::Spec(ref ws)) = f.over {
                collect_table_name_spans_in_window_spec(ws, old_name, rewrite_qual, ctes, out);
            }
        }
        _ => {}
    }
    for ch in expr_children(expr) {
        collect_table_name_spans_in_expr(ch, old_name, rewrite_qual, ctes, out);
    }
}

fn collect_table_name_spans_in_from(
    src: &FromSource,
    old_name: &str,
    rewrite_qual: bool,
    ctes: &[String],
    out: &mut Vec<Span>,
) {
    match src {
        FromSource::Table(ft) => {
            if ft.name.name.eq_ignore_ascii_case(old_name)
                && !ctes.iter().any(|c| c.eq_ignore_ascii_case(old_name))
            {
                out.push(ft.name.span);
            }
        }
        FromSource::Function(ff) => {
            for a in &ff.args {
                collect_table_name_spans_in_expr(a, old_name, rewrite_qual, ctes, out);
            }
        }
        FromSource::Subquery(sub) => {
            collect_table_name_spans_in_select(&sub.select, old_name, false, ctes, out);
        }
        FromSource::Group(g) => {
            collect_table_name_spans_in_from(&g.source, old_name, rewrite_qual, ctes, out);
        }
        FromSource::Join {
            left, right, on, ..
        } => {
            collect_table_name_spans_in_from(left, old_name, rewrite_qual, ctes, out);
            collect_table_name_spans_in_from(right, old_name, rewrite_qual, ctes, out);
            if let Some(ref on_expr) = on {
                collect_table_name_spans_in_expr(on_expr, old_name, rewrite_qual, ctes, out);
            }
        }
    }
}

fn collect_table_name_spans_in_with(
    w: &xdb_parser::WithClause,
    old_name: &str,
    outer_rewrite_qual: bool,
    ctes: &mut Vec<String>,
    out: &mut Vec<Span>,
) {
    for cte in &w.ctes {
        if w.recursive {
            ctes.push(cte.name.name.clone());
        }
        collect_table_name_spans_in_select(&cte.select, old_name, outer_rewrite_qual, ctes, out);
        if !w.recursive {
            ctes.push(cte.name.name.clone());
        }
    }
}

fn collect_table_name_spans_in_select(
    sel: &SelectStmt,
    old_name: &str,
    outer_rewrite_qual: bool,
    outer_ctes: &[String],
    out: &mut Vec<Span>,
) {
    let mut ctes = outer_ctes.to_vec();
    if let Some(ref w) = sel.with {
        collect_table_name_spans_in_with(w, old_name, outer_rewrite_qual, &mut ctes, out);
    }
    let body_rewrites_qual = collect_table_name_spans_in_body(
        &sel.body,
        old_name,
        outer_rewrite_qual,
        &ctes,
        out,
    );
    if let Some(ref ob) = sel.order_by {
        for ot in ob {
            collect_table_name_spans_in_expr(&ot.expr, old_name, body_rewrites_qual, &ctes, out);
        }
    }
    if let Some(ref lim) = sel.limit {
        collect_table_name_spans_in_expr(&lim.count, old_name, outer_rewrite_qual, &ctes, out);
        if let Some(ref off) = lim.offset {
            collect_table_name_spans_in_expr(off, old_name, outer_rewrite_qual, &ctes, out);
        }
    }
}

fn collect_table_name_spans_in_body(
    body: &SelectBody,
    old_name: &str,
    outer_rewrite_qual: bool,
    ctes: &[String],
    out: &mut Vec<Span>,
) -> bool {
    match body {
        SelectBody::Core(core) => {
            let mut aliases = Vec::new();
            let has_unaliased = if let Some(ref f) = core.from {
                collect_from_aliases(f, &mut aliases);
                from_has_unaliased_table(f, old_name, ctes)
            } else {
                false
            };
            // Rewrite `old_name.col` when `old_name` is an active unaliased table in `FROM`
            // (or inherited from an outer correlated scope) and not shadowed by a CTE or alias.
            let shadowed = is_shadowed(old_name, ctes, &aliases);
            let rewrite_qual = (has_unaliased || outer_rewrite_qual) && !shadowed;
            if let Some(ref f) = core.from {
                collect_table_name_spans_in_from(f, old_name, rewrite_qual, ctes, out);
            }
            for rc in &core.columns {
                match rc {
                    xdb_parser::ResultColumn::Star(st) => {
                        if rewrite_qual {
                            if let Some(ref t) = st.table {
                                if t.name.eq_ignore_ascii_case(old_name) {
                                    out.push(t.span);
                                }
                            }
                        }
                        if let Some(ref reps) = st.replace {
                            for rep in reps {
                                collect_table_name_spans_in_expr(
                                    &rep.expr,
                                    old_name,
                                    rewrite_qual,
                                    ctes,
                                    out,
                                );
                            }
                        }
                    }
                    xdb_parser::ResultColumn::Expr(re) => {
                        collect_table_name_spans_in_expr(&re.expr, old_name, rewrite_qual, ctes, out);
                    }
                }
            }
            if let Some(ref we) = core.where_clause {
                collect_table_name_spans_in_expr(we, old_name, rewrite_qual, ctes, out);
            }
            if let Some(ref gb) = core.group_by {
                for e in gb {
                    collect_table_name_spans_in_expr(e, old_name, rewrite_qual, ctes, out);
                }
            }
            if let Some(ref h) = core.having {
                collect_table_name_spans_in_expr(h, old_name, rewrite_qual, ctes, out);
            }
            if let Some(ref wins) = core.windows {
                for wd in wins {
                    collect_table_name_spans_in_window_spec(
                        &wd.window,
                        old_name,
                        rewrite_qual,
                        ctes,
                        out,
                    );
                }
            }
            rewrite_qual
        }
        SelectBody::Values(v) => {
            for row in &v.rows {
                for e in &row.items {
                    collect_table_name_spans_in_expr(e, old_name, outer_rewrite_qual, ctes, out);
                }
            }
            outer_rewrite_qual
        }
        SelectBody::Compound { left, right, .. } => {
            let l = collect_table_name_spans_in_body(left, old_name, outer_rewrite_qual, ctes, out);
            let _ =
                collect_table_name_spans_in_body(right, old_name, outer_rewrite_qual, ctes, out);
            l
        }
    }
}

/// Rewrite all references to `old_name` in a `CREATE TRIGGER` DDL statement (both `ON <table>`
/// and DML/SELECT references inside the trigger body).
pub(crate) fn rewrite_trigger_table_name(ddl: &str, old_name: &str, new_name: &str) -> String {
    let opts = ParseOptions {
        dialect: xdb_parser::Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    let Ok(Statement::CreateTrigger(ct)) = parse_statement(ddl, &opts) else {
        return ddl.to_string();
    };
    let mut spans = Vec::new();
    if ct.table.name.eq_ignore_ascii_case(old_name) {
        spans.push(ct.table.span);
    }
    if let Some(ref w) = ct.when {
        // In WHEN, qualifiers are OLD/NEW (never table names) — only rewrite nested subqueries.
        collect_table_name_spans_in_expr(w, old_name, false, &[], &mut spans);
    }
    for step in &ct.body {
        match step {
            TriggerStep::Select(sel) => {
                collect_table_name_spans_in_select(sel, old_name, false, &[], &mut spans);
            }
            TriggerStep::Insert(ins) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = ins.with {
                    collect_table_name_spans_in_with(w, old_name, false, &mut ctes, &mut spans);
                }
                let ins_matches = ins.table.name.name.eq_ignore_ascii_case(old_name);
                if ins_matches {
                    spans.push(ins.table.name.span);
                }
                if let Some(ref sel) = ins.select {
                    collect_table_name_spans_in_select(sel, old_name, false, &ctes, &mut spans);
                }
                let ins_qual = ins_matches && ins.table.alias.is_none();
                for up in &ins.upsert {
                    if let Some(ref tw) = up.target_where {
                        collect_table_name_spans_in_expr(tw, old_name, ins_qual, &ctes, &mut spans);
                    }
                    if let Some(ref sets) = up.set {
                        for sc in sets {
                            collect_table_name_spans_in_expr(
                                &sc.expr,
                                old_name,
                                ins_qual,
                                &ctes,
                                &mut spans,
                            );
                        }
                    }
                    if let Some(ref we) = up.where_clause {
                        collect_table_name_spans_in_expr(we, old_name, ins_qual, &ctes, &mut spans);
                    }
                }
            }
            TriggerStep::Update(upd) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = upd.with {
                    collect_table_name_spans_in_with(w, old_name, false, &mut ctes, &mut spans);
                }
                let upd_matches = upd.table.name.name.eq_ignore_ascii_case(old_name);
                if upd_matches {
                    spans.push(upd.table.name.span);
                }
                let mut aliases = Vec::new();
                let from_unaliased = if let Some(ref f) = upd.from {
                    collect_from_aliases(f, &mut aliases);
                    from_has_unaliased_table(f, old_name, &ctes)
                } else {
                    false
                };
                let upd_qual = (upd_matches
                    && upd.table.alias.is_none()
                    && !aliases.iter().any(|a| a.eq_ignore_ascii_case(old_name)))
                    || (from_unaliased && !is_shadowed(old_name, &ctes, &aliases));
                if let Some(ref f) = upd.from {
                    collect_table_name_spans_in_from(f, old_name, upd_qual, &ctes, &mut spans);
                }
                for sc in &upd.set {
                    collect_table_name_spans_in_expr(&sc.expr, old_name, upd_qual, &ctes, &mut spans);
                }
                if let Some(ref we) = upd.where_clause {
                    collect_table_name_spans_in_expr(we, old_name, upd_qual, &ctes, &mut spans);
                }
            }
            TriggerStep::Delete(del) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = del.with {
                    collect_table_name_spans_in_with(w, old_name, false, &mut ctes, &mut spans);
                }
                let del_matches = del.table.name.name.eq_ignore_ascii_case(old_name);
                if del_matches {
                    spans.push(del.table.name.span);
                }
                let del_qual = del_matches && del.table.alias.is_none();
                if let Some(ref we) = del.where_clause {
                    collect_table_name_spans_in_expr(we, old_name, del_qual, &ctes, &mut spans);
                }
            }
        }
    }
    let quoted = quote_ident_if_needed(new_name);
    apply_spans(ddl, spans, &quoted)
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_window_spec(
    ws: &xdb_parser::WindowSpec,
    qualifiers: &[String],
    allow_unqual: bool,
    ctes: &[String],
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) {
    if let Some(ref pbs) = ws.partition_by {
        for e in pbs {
            collect_col_spans_in_expr(
                e,
                qualifiers,
                allow_unqual,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
    }
    if let Some(ref obs) = ws.order_by {
        for ot in obs {
            collect_col_spans_in_expr(
                &ot.expr,
                qualifiers,
                allow_unqual,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_expr(
    expr: &Expr,
    qualifiers: &[String],
    allow_unqual: bool,
    ctes: &[String],
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) {
    match expr {
        Expr::Column(c) => {
            if c.column.name.eq_ignore_ascii_case(old_col) {
                let matched = match c.table {
                    Some(ref t) => qualifiers.iter().any(|q| t.name.eq_ignore_ascii_case(q)),
                    None => allow_unqual,
                };
                if matched {
                    out.push(c.column.span);
                }
            }
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            collect_col_spans_in_select(
                select,
                qualifiers,
                allow_unqual,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
        Expr::Function(f) => {
            if let Some(xdb_parser::WindowRef::Spec(ref ws)) = f.over {
                collect_col_spans_in_window_spec(
                    ws,
                    qualifiers,
                    allow_unqual,
                    ctes,
                    table_name,
                    old_col,
                    schema,
                    out,
                );
            }
        }
        _ => {}
    }
    for ch in expr_children(expr) {
        collect_col_spans_in_expr(
            ch,
            qualifiers,
            allow_unqual,
            ctes,
            table_name,
            old_col,
            schema,
            out,
        );
    }
}

struct FromColInfo {
    target_quals: Vec<String>,
    non_target_aliases: Vec<String>,
    total_sources: usize,
    target_matches: usize,
    other_has_old_col: bool,
}

fn builtin_tvf_has_col(fn_name: &str, col: &str) -> Option<bool> {
    let cols: &[&str] = match fn_name.to_ascii_lowercase().as_str() {
        "json_each" => &["key", "value", "type", "atom", "id", "parent", "fullkey", "path"],
        "json_tree" => &["key", "value", "type", "atom", "id", "parent", "fullkey", "path"],
        "generate_series" => &["value"],
        "pragma_table_info" => &["cid", "name", "type", "notnull", "dflt_value", "pk"],
        "pragma_table_xinfo" => &["cid", "name", "type", "notnull", "dflt_value", "pk", "hidden"],
        "pragma_table_list" => &["schema", "name", "type", "ncol", "wr", "strict"],
        "pragma_index_list" => &["seq", "name", "unique", "origin", "partial"],
        "pragma_index_info" => &["seqno", "cid", "name"],
        "pragma_index_xinfo" => &["seqno", "cid", "name", "desc", "coll", "key"],
        "pragma_database_list" => &["seq", "name", "file"],
        "pragma_foreign_key_list" => {
            &["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"]
        }
        "pragma_foreign_key_check" => &["table", "rowid", "parent", "fkid"],
        "pragma_collation_list" => &["seq", "name"],
        "pragma_compile_options" => &["compile_options"],
        "pragma_pragma_list" => &["name"],
        "pragma_function_list" => &["name", "builtin", "type", "enc", "narg", "flags"],
        _ => return None,
    };
    Some(cols.iter().any(|c| c.eq_ignore_ascii_case(col)))
}

fn select_output_may_have_col(
    sel: &SelectStmt,
    col: &str,
    schema: Option<&SchemaCatalog>,
) -> bool {
    fn body_may_have(body: &SelectBody, col: &str, schema: Option<&SchemaCatalog>) -> bool {
        match body {
            SelectBody::Core(core) => {
                for rc in &core.columns {
                    match rc {
                        xdb_parser::ResultColumn::Expr(re) => {
                            if let Some(ref a) = re.alias {
                                if a.name.eq_ignore_ascii_case(col) {
                                    return true;
                                }
                            } else if let Expr::Column(c) = &re.expr {
                                if c.column.name.eq_ignore_ascii_case(col) {
                                    return true;
                                }
                            }
                        }
                        xdb_parser::ResultColumn::Star(rs) => {
                            if let Some(ref excl) = rs.exclude {
                                if excl.iter().any(|e| e.name.eq_ignore_ascii_case(col)) {
                                    continue;
                                }
                            }
                            if let Some(ref reps) = rs.replace {
                                if reps.iter().any(|r| r.name.name.eq_ignore_ascii_case(col)) {
                                    return true;
                                }
                            }
                            let Some(ref f) = core.from else {
                                return true;
                            };
                            if from_may_have_col(f, col, schema) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
            SelectBody::Values(_) => false,
            SelectBody::Compound { left, .. } => body_may_have(left, col, schema),
        }
    }
    fn from_may_have_col(src: &FromSource, col: &str, schema: Option<&SchemaCatalog>) -> bool {
        match src {
            FromSource::Table(ft) => {
                if let Some(sc) = schema {
                    if let Some(tbl) = sc.table(&ft.name.name) {
                        return tbl.columns.iter().any(|c| c.name.eq_ignore_ascii_case(col));
                    }
                    if let Some(v) = sc.view(&ft.name.name) {
                        if let Some(ref cn) = v.column_names {
                            return cn.iter().any(|n| n.eq_ignore_ascii_case(col));
                        }
                        if let Ok(Statement::CreateView(cv)) = parse_statement(
                            &v.sql,
                            &ParseOptions {
                                dialect: xdb_parser::Dialect::Xdb,
                                schema: true,
                                ..ParseOptions::default()
                            },
                        ) {
                            return select_output_may_have_col(&cv.select, col, schema);
                        }
                    }
                }
                if let Some(b) = builtin_tvf_has_col(&ft.name.name, col) {
                    return b;
                }
                true
            }
            FromSource::Function(ff) => builtin_tvf_has_col(&ff.name.name, col).unwrap_or(true),
            FromSource::Subquery(sub) => select_output_may_have_col(&sub.select, col, schema),
            FromSource::Group(g) => from_may_have_col(&g.source, col, schema),
            FromSource::Join { left, right, .. } => {
                from_may_have_col(left, col, schema) || from_may_have_col(right, col, schema)
            }
        }
    }
    body_may_have(&sel.body, col, schema)
}

fn select_body_aliases_col(body: &SelectBody, old_col: &str) -> bool {
    match body {
        SelectBody::Core(core) => core.columns.iter().any(|rc| {
            matches!(
                rc,
                xdb_parser::ResultColumn::Expr(re)
                    if re.alias.as_ref().is_some_and(|a| a.name.eq_ignore_ascii_case(old_col))
            )
        }),
        SelectBody::Compound { left, .. } => select_body_aliases_col(left, old_col),
        SelectBody::Values(_) => false,
    }
}

fn inspect_from_for_col_rename(
    src: &FromSource,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: Option<&SchemaCatalog>,
    info: &mut FromColInfo,
) {
    match src {
        FromSource::Table(ft) => {
            info.total_sources += 1;
            let is_cte = ft.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&ft.name.name));
            let eff_alias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();
            if !is_cte && ft.name.name.eq_ignore_ascii_case(table_name) {
                info.target_matches += 1;
                info.target_quals.push(eff_alias);
            } else {
                info.non_target_aliases.push(eff_alias);
                if is_cte {
                    info.other_has_old_col = true;
                } else if let Some(sc) = schema {
                    if let Some(other_tbl) = sc.table(&ft.name.name) {
                        if other_tbl
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(old_col))
                        {
                            info.other_has_old_col = true;
                        }
                    } else if let Some(v) = sc.view(&ft.name.name) {
                        let has = if let Some(ref cn) = v.column_names {
                            cn.iter().any(|n| n.eq_ignore_ascii_case(old_col))
                        } else if let Ok(Statement::CreateView(cv)) = parse_statement(
                            &v.sql,
                            &ParseOptions {
                                dialect: xdb_parser::Dialect::Xdb,
                                schema: true,
                                ..ParseOptions::default()
                            },
                        ) {
                            select_output_may_have_col(&cv.select, old_col, schema)
                        } else {
                            true
                        };
                        if has {
                            info.other_has_old_col = true;
                        }
                    } else if builtin_tvf_has_col(&ft.name.name, old_col).unwrap_or(true) {
                        info.other_has_old_col = true;
                    }
                } else if builtin_tvf_has_col(&ft.name.name, old_col).unwrap_or(true) {
                    info.other_has_old_col = true;
                }
            }
        }
        FromSource::Function(ff) => {
            info.total_sources += 1;
            info.non_target_aliases
                .push(ff.alias.as_ref().unwrap_or(&ff.name).name.clone());
            if builtin_tvf_has_col(&ff.name.name, old_col).unwrap_or(true) {
                info.other_has_old_col = true;
            }
        }
        FromSource::Subquery(sub) => {
            info.total_sources += 1;
            if let Some(ref a) = sub.alias {
                info.non_target_aliases.push(a.name.clone());
            }
            if select_output_may_have_col(&sub.select, old_col, schema) {
                info.other_has_old_col = true;
            }
        }
        FromSource::Group(g) => {
            inspect_from_for_col_rename(&g.source, table_name, old_col, ctes, schema, info);
        }
        FromSource::Join { left, right, .. } => {
            inspect_from_for_col_rename(left, table_name, old_col, ctes, schema, info);
            inspect_from_for_col_rename(right, table_name, old_col, ctes, schema, info);
        }
    }
}

fn from_references_target_table(src: &FromSource, table_name: &str, ctes: &[String]) -> bool {
    match src {
        FromSource::Table(ft) => {
            let is_cte = ft.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&ft.name.name));
            !is_cte && ft.name.name.eq_ignore_ascii_case(table_name)
        }
        FromSource::Group(g) => from_references_target_table(&g.source, table_name, ctes),
        FromSource::Join { left, right, .. } => {
            from_references_target_table(left, table_name, ctes)
                || from_references_target_table(right, table_name, ctes)
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_from_exprs(
    src: &FromSource,
    quals: &[String],
    allow_unqual: bool,
    outer_quals: &[String],
    ctes: &[String],
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) {
    match src {
        FromSource::Table(_) => {}
        FromSource::Function(ff) => {
            for a in &ff.args {
                collect_col_spans_in_expr(
                    a, quals, allow_unqual, ctes, table_name, old_col, schema, out,
                );
            }
        }
        FromSource::Subquery(sub) => {
            collect_col_spans_in_select(
                &sub.select,
                outer_quals,
                false,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
        FromSource::Group(g) => {
            collect_col_spans_in_from_exprs(
                &g.source,
                quals,
                allow_unqual,
                outer_quals,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
        FromSource::Join {
            left,
            right,
            on,
            using,
            ..
        } => {
            collect_col_spans_in_from_exprs(
                left,
                quals,
                allow_unqual,
                outer_quals,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
            collect_col_spans_in_from_exprs(
                right,
                quals,
                allow_unqual,
                outer_quals,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
            if let Some(ref on_expr) = on {
                collect_col_spans_in_expr(
                    on_expr,
                    quals,
                    allow_unqual,
                    ctes,
                    table_name,
                    old_col,
                    schema,
                    out,
                );
            }
            if let Some(ref us) = using {
                if from_references_target_table(left, table_name, ctes)
                    || from_references_target_table(right, table_name, ctes)
                {
                    for u in us {
                        if u.name.eq_ignore_ascii_case(old_col) {
                            out.push(u.span);
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_with(
    w: &xdb_parser::WithClause,
    outer_quals: &[String],
    outer_unqual: bool,
    ctes: &mut Vec<String>,
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) {
    for cte in &w.ctes {
        if w.recursive {
            ctes.push(cte.name.name.clone());
        }
        collect_col_spans_in_select(
            &cte.select,
            outer_quals,
            outer_unqual,
            ctes,
            table_name,
            old_col,
            schema,
            out,
        );
        if !w.recursive {
            ctes.push(cte.name.name.clone());
        }
    }
}

/// Rewrite references to `old_col` on `table_name` inside a `CREATE TRIGGER` DDL statement
/// (`UPDATE OF`, `OLD.<col>`, `NEW.<col>`, and DML/SELECT/CTE/subquery/WINDOW references).
pub(crate) fn rewrite_trigger_column_name(
    ddl: &str,
    table_name: &str,
    old_col: &str,
    new_col: &str,
    schema: Option<&SchemaCatalog>,
) -> String {
    let opts = ParseOptions {
        dialect: xdb_parser::Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    let Ok(Statement::CreateTrigger(ct)) = parse_statement(ddl, &opts) else {
        return ddl.to_string();
    };
    let on_target = ct.table.name.eq_ignore_ascii_case(table_name);
    let mut spans = Vec::new();
    let mut base_quals: Vec<String> = Vec::new();
    if on_target {
        base_quals.push("old".to_string());
        base_quals.push("new".to_string());
        if let Some(ref ucols) = ct.update_of {
            for c in ucols {
                if c.name.eq_ignore_ascii_case(old_col) {
                    spans.push(c.span);
                }
            }
        }
    }
    if let Some(ref w) = ct.when {
        collect_col_spans_in_expr(
            w, &base_quals, false, &[], table_name, old_col, schema, &mut spans,
        );
    }
    for step in &ct.body {
        match step {
            TriggerStep::Select(sel) => {
                collect_col_spans_in_select(
                    sel,
                    &base_quals,
                    false,
                    &[],
                    table_name,
                    old_col,
                    schema,
                    &mut spans,
                );
            }
            TriggerStep::Insert(ins) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = ins.with {
                    collect_col_spans_in_with(
                        w,
                        &base_quals,
                        false,
                        &mut ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                let ins_is_target = ins.table.name.name.eq_ignore_ascii_case(table_name);
                if ins_is_target {
                    if let Some(ref cols) = ins.columns {
                        for c in cols {
                            if c.name.eq_ignore_ascii_case(old_col) {
                                spans.push(c.span);
                            }
                        }
                    }
                }
                if let Some(ref sel) = ins.select {
                    collect_col_spans_in_select(
                        sel,
                        &base_quals,
                        false,
                        &ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                for up in &ins.upsert {
                    let mut quals: Vec<String> = base_quals.clone();
                    if ins_is_target {
                        let eff_q = ins
                            .table
                            .alias
                            .as_ref()
                            .map_or(ins.table.name.name.as_str(), |a| a.name.as_str());
                        quals.push(eff_q.to_string());
                        quals.push("excluded".to_string());
                    } else if let Some(ref a) = ins.table.alias {
                        quals.retain(|q| !q.eq_ignore_ascii_case(&a.name));
                    } else {
                        quals.retain(|q| !q.eq_ignore_ascii_case(&ins.table.name.name));
                    }
                    if let Some(ref targets) = up.target {
                        if ins_is_target {
                            for idx_col in targets {
                                collect_col_spans_in_expr(
                                    &idx_col.expr,
                                    &quals,
                                    true,
                                    &ctes,
                                    table_name,
                                    old_col,
                                    schema,
                                    &mut spans,
                                );
                            }
                        }
                    }
                    if let Some(ref tw) = up.target_where {
                        collect_col_spans_in_expr(
                            tw,
                            &quals,
                            ins_is_target,
                            &ctes,
                            table_name,
                            old_col,
                            schema,
                            &mut spans,
                        );
                    }
                    if let Some(ref sets) = up.set {
                        for sc in sets {
                            if ins_is_target {
                                for c in &sc.columns {
                                    if c.name.eq_ignore_ascii_case(old_col) {
                                        spans.push(c.span);
                                    }
                                }
                            }
                            collect_col_spans_in_expr(
                                &sc.expr,
                                &quals,
                                ins_is_target,
                                &ctes,
                                table_name,
                                old_col,
                                schema,
                                &mut spans,
                            );
                        }
                    }
                    if let Some(ref we) = up.where_clause {
                        collect_col_spans_in_expr(
                            we,
                            &quals,
                            ins_is_target,
                            &ctes,
                            table_name,
                            old_col,
                            schema,
                            &mut spans,
                        );
                    }
                }
            }
            TriggerStep::Update(upd) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = upd.with {
                    collect_col_spans_in_with(
                        w,
                        &base_quals,
                        false,
                        &mut ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                let upd_is_target = upd.table.name.name.eq_ignore_ascii_case(table_name);
                let mut from_info = FromColInfo {
                    target_quals: Vec::new(),
                    non_target_aliases: Vec::new(),
                    total_sources: 0,
                    target_matches: 0,
                    other_has_old_col: false,
                };
                if let Some(ref f) = upd.from {
                    inspect_from_for_col_rename(
                        f,
                        table_name,
                        old_col,
                        &ctes,
                        schema,
                        &mut from_info,
                    );
                }
                let upd_other_has_col = if upd_is_target {
                    false
                } else if let Some(sc) = schema {
                    sc.table(&upd.table.name.name).is_none_or(|ot| {
                        ot.columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(old_col))
                    })
                } else {
                    true
                };
                let mut shadowed = from_info.non_target_aliases.clone();
                if !upd_is_target {
                    shadowed.push(
                        upd.table
                            .alias
                            .as_ref()
                            .map_or(upd.table.name.name.clone(), |a| a.name.clone()),
                    );
                }
                let mut quals: Vec<String> = base_quals
                    .iter()
                    .filter(|q| !shadowed.iter().any(|s| s.eq_ignore_ascii_case(q)))
                    .cloned()
                    .collect();
                if upd_is_target {
                    let eff_q = upd
                        .table
                        .alias
                        .as_ref()
                        .map_or(upd.table.name.name.as_str(), |a| a.name.as_str());
                    quals.push(eff_q.to_string());
                }
                for q in &from_info.target_quals {
                    if !quals.iter().any(|e| e.eq_ignore_ascii_case(q)) {
                        quals.push(q.clone());
                    }
                }
                let target_present = upd_is_target || from_info.target_matches > 0;
                let allow_unqual = target_present && !from_info.other_has_old_col && !upd_other_has_col;
                if let Some(ref f) = upd.from {
                    collect_col_spans_in_from_exprs(
                        f,
                        &quals,
                        allow_unqual,
                        &base_quals,
                        &ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                for sc in &upd.set {
                    if upd_is_target {
                        for c in &sc.columns {
                            if c.name.eq_ignore_ascii_case(old_col) {
                                spans.push(c.span);
                            }
                        }
                    }
                    collect_col_spans_in_expr(
                        &sc.expr,
                        &quals,
                        allow_unqual,
                        &ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                if let Some(ref we) = upd.where_clause {
                    collect_col_spans_in_expr(
                        we,
                        &quals,
                        allow_unqual,
                        &ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
            }
            TriggerStep::Delete(del) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = del.with {
                    collect_col_spans_in_with(
                        w,
                        &base_quals,
                        false,
                        &mut ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
                let del_is_target = del.table.name.name.eq_ignore_ascii_case(table_name);
                let mut quals: Vec<String> = base_quals.clone();
                if del_is_target {
                    let eff_q = del
                        .table
                        .alias
                        .as_ref()
                        .map_or(del.table.name.name.as_str(), |a| a.name.as_str());
                    quals.push(eff_q.to_string());
                } else {
                    let shadow = del
                        .table
                        .alias
                        .as_ref()
                        .map_or(del.table.name.name.as_str(), |a| a.name.as_str());
                    quals.retain(|q| !q.eq_ignore_ascii_case(shadow));
                }
                if let Some(ref we) = del.where_clause {
                    collect_col_spans_in_expr(
                        we,
                        &quals,
                        del_is_target,
                        &ctes,
                        table_name,
                        old_col,
                        schema,
                        &mut spans,
                    );
                }
            }
        }
    }
    let quoted = quote_ident_if_needed(new_col);
    apply_spans(ddl, spans, &quoted)
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_select(
    sel: &SelectStmt,
    outer_quals: &[String],
    outer_unqual: bool,
    outer_ctes: &[String],
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) {
    let mut ctes = outer_ctes.to_vec();
    if let Some(ref w) = sel.with {
        collect_col_spans_in_with(
            w,
            outer_quals,
            outer_unqual,
            &mut ctes,
            table_name,
            old_col,
            schema,
            out,
        );
    }
    let (ob_quals, ob_unqual) = collect_col_spans_in_body(
        &sel.body,
        outer_quals,
        outer_unqual,
        &ctes,
        table_name,
        old_col,
        schema,
        out,
    );
    if let Some(ref ob) = sel.order_by {
        // Explicit `SELECT ... AS <old_col>` aliases shadow the table column in unqualified ORDER BY.
        let eff_ob_unqual = ob_unqual && !select_body_aliases_col(&sel.body, old_col);
        for ot in ob {
            collect_col_spans_in_expr(
                &ot.expr,
                &ob_quals,
                eff_ob_unqual,
                &ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
    }
    if let Some(ref lim) = sel.limit {
        collect_col_spans_in_expr(
            &lim.count,
            outer_quals,
            outer_unqual,
            &ctes,
            table_name,
            old_col,
            schema,
            out,
        );
        if let Some(ref off) = lim.offset {
            collect_col_spans_in_expr(
                off,
                outer_quals,
                outer_unqual,
                &ctes,
                table_name,
                old_col,
                schema,
                out,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_col_spans_in_body(
    body: &SelectBody,
    outer_quals: &[String],
    outer_unqual: bool,
    ctes: &[String],
    table_name: &str,
    old_col: &str,
    schema: Option<&SchemaCatalog>,
    out: &mut Vec<Span>,
) -> (Vec<String>, bool) {
    match body {
        SelectBody::Core(core) => {
            let mut from_info = FromColInfo {
                target_quals: Vec::new(),
                non_target_aliases: Vec::new(),
                total_sources: 0,
                target_matches: 0,
                other_has_old_col: false,
            };
            if let Some(ref f) = core.from {
                inspect_from_for_col_rename(f, table_name, old_col, ctes, schema, &mut from_info);
            }
            let mut quals: Vec<String> = outer_quals
                .iter()
                .filter(|q| {
                    !from_info
                        .non_target_aliases
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case(q))
                })
                .cloned()
                .collect();
            for q in &from_info.target_quals {
                if !quals.iter().any(|e| e.eq_ignore_ascii_case(q)) {
                    quals.push(q.clone());
                }
            }
            let allow_unqual = if from_info.target_matches > 0 {
                !from_info.other_has_old_col
            } else {
                outer_unqual && !from_info.other_has_old_col
            };
            if let Some(ref f) = core.from {
                collect_col_spans_in_from_exprs(
                    f,
                    &quals,
                    allow_unqual,
                    outer_quals,
                    ctes,
                    table_name,
                    old_col,
                    schema,
                    out,
                );
            }
            for rc in &core.columns {
                match rc {
                    xdb_parser::ResultColumn::Expr(re) => {
                        collect_col_spans_in_expr(
                            &re.expr,
                            &quals,
                            allow_unqual,
                            ctes,
                            table_name,
                            old_col,
                            schema,
                            out,
                        );
                    }
                    xdb_parser::ResultColumn::Star(rs) => {
                        let star_targets_table = match &rs.table {
                            Some(t) => quals.iter().any(|q| t.name.eq_ignore_ascii_case(q)),
                            None => allow_unqual,
                        };
                        if let Some(ref excl) = rs.exclude {
                            if star_targets_table {
                                for ex in excl {
                                    if ex.name.eq_ignore_ascii_case(old_col) {
                                        out.push(ex.span);
                                    }
                                }
                            }
                        }
                        if let Some(ref reps) = rs.replace {
                            for rep in reps {
                                collect_col_spans_in_expr(
                                    &rep.expr,
                                    &quals,
                                    allow_unqual,
                                    ctes,
                                    table_name,
                                    old_col,
                                    schema,
                                    out,
                                );
                                if star_targets_table && rep.name.name.eq_ignore_ascii_case(old_col)
                                {
                                    out.push(rep.name.span);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ref we) = core.where_clause {
                collect_col_spans_in_expr(
                    we,
                    &quals,
                    allow_unqual,
                    ctes,
                    table_name,
                    old_col,
                    schema,
                    out,
                );
            }
            if let Some(ref gb) = core.group_by {
                for e in gb {
                    collect_col_spans_in_expr(
                        e,
                        &quals,
                        allow_unqual,
                        ctes,
                        table_name,
                        old_col,
                        schema,
                        out,
                    );
                }
            }
            if let Some(ref h) = core.having {
                collect_col_spans_in_expr(
                    h,
                    &quals,
                    allow_unqual,
                    ctes,
                    table_name,
                    old_col,
                    schema,
                    out,
                );
            }
            if let Some(ref wins) = core.windows {
                for wd in wins {
                    collect_col_spans_in_window_spec(
                        &wd.window,
                        &quals,
                        allow_unqual,
                        ctes,
                        table_name,
                        old_col,
                        schema,
                        out,
                    );
                }
            }
            (quals, allow_unqual)
        }
        SelectBody::Values(v) => {
            for row in &v.rows {
                for e in &row.items {
                    collect_col_spans_in_expr(
                        e,
                        outer_quals,
                        outer_unqual,
                        ctes,
                        table_name,
                        old_col,
                        schema,
                        out,
                    );
                }
            }
            (outer_quals.to_vec(), outer_unqual)
        }
        SelectBody::Compound { left, right, .. } => {
            let l = collect_col_spans_in_body(
                left,
                outer_quals,
                outer_unqual,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
            let _ = collect_col_spans_in_body(
                right,
                outer_quals,
                outer_unqual,
                ctes,
                table_name,
                old_col,
                schema,
                out,
            );
            l
        }
    }
}

/// Returns `true` if a trigger DDL contains a `JOIN ... USING (<old_col>)` where one side
/// references `table_name` and the other side does not (so renaming `old_col` breaks the join,
/// matching SQLite's `cannot join using column ...` rejection).
pub(crate) fn trigger_has_using_join_conflict(
    sql: &str,
    table_name: &str,
    old_col: &str,
) -> bool {
    let opts = ParseOptions {
        dialect: xdb_parser::Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    let Ok(Statement::CreateTrigger(ct)) = parse_statement(sql, &opts) else {
        return false;
    };
    fn expr_has(e: &Expr, table_name: &str, old_col: &str, ctes: &[String]) -> bool {
        match e {
            Expr::Subquery { select, .. }
            | Expr::Exists { select, .. }
            | Expr::InSelect { select, .. } => {
                if let Expr::InSelect { operand, .. } = e {
                    if expr_has(operand, table_name, old_col, ctes) {
                        return true;
                    }
                }
                select_has(select, table_name, old_col, ctes)
            }
            _ => xdb_parser::expr_children(e)
                .into_iter()
                .any(|ch| expr_has(ch, table_name, old_col, ctes)),
        }
    }
    fn from_has(src: &FromSource, table_name: &str, old_col: &str, ctes: &[String]) -> bool {
        match src {
            FromSource::Table(_) => false,
            FromSource::Function(ff) => ff
                .args
                .iter()
                .any(|a| expr_has(a, table_name, old_col, ctes)),
            FromSource::Subquery(sub) => select_has(&sub.select, table_name, old_col, ctes),
            FromSource::Group(g) => from_has(&g.source, table_name, old_col, ctes),
            FromSource::Join {
                left,
                right,
                on,
                using,
                ..
            } => {
                if let Some(ref us) = using {
                    if us.iter().any(|u| u.name.eq_ignore_ascii_case(old_col)) {
                        let lref = from_references_target_table(left, table_name, ctes);
                        let rref = from_references_target_table(right, table_name, ctes);
                        if lref ^ rref {
                            return true;
                        }
                    }
                }
                from_has(left, table_name, old_col, ctes)
                    || from_has(right, table_name, old_col, ctes)
                    || on
                        .as_deref()
                        .is_some_and(|e| expr_has(e, table_name, old_col, ctes))
            }
        }
    }
    fn body_has(body: &SelectBody, table_name: &str, old_col: &str, ctes: &[String]) -> bool {
        match body {
            SelectBody::Core(core) => {
                core.from
                    .as_ref()
                    .is_some_and(|f| from_has(f, table_name, old_col, ctes))
                    || core.columns.iter().any(|rc| match rc {
                        xdb_parser::ResultColumn::Expr(re) => {
                            expr_has(&re.expr, table_name, old_col, ctes)
                        }
                        xdb_parser::ResultColumn::Star(rs) => rs.replace.as_ref().is_some_and(|r| {
                            r.iter()
                                .any(|x| expr_has(&x.expr, table_name, old_col, ctes))
                        }),
                    })
                    || core
                        .where_clause
                        .as_ref()
                        .is_some_and(|e| expr_has(e, table_name, old_col, ctes))
                    || core
                        .group_by
                        .as_ref()
                        .is_some_and(|gb| gb.iter().any(|e| expr_has(e, table_name, old_col, ctes)))
                    || core
                        .having
                        .as_ref()
                        .is_some_and(|e| expr_has(e, table_name, old_col, ctes))
                    || core.windows.as_ref().is_some_and(|wins| {
                        wins.iter().any(|wd| {
                            wd.window.partition_by.as_ref().is_some_and(|pbs| {
                                pbs.iter().any(|e| expr_has(e, table_name, old_col, ctes))
                            }) || wd.window.order_by.as_ref().is_some_and(|obs| {
                                obs.iter()
                                    .any(|o| expr_has(&o.expr, table_name, old_col, ctes))
                            })
                        })
                    })
            }
            SelectBody::Values(v) => v
                .rows
                .iter()
                .any(|r| r.items.iter().any(|e| expr_has(e, table_name, old_col, ctes))),
            SelectBody::Compound { left, right, .. } => {
                body_has(left, table_name, old_col, ctes)
                    || body_has(right, table_name, old_col, ctes)
            }
        }
    }
    fn select_has(sel: &SelectStmt, table_name: &str, old_col: &str, outer_ctes: &[String]) -> bool {
        let mut ctes = outer_ctes.to_vec();
        if let Some(ref w) = sel.with {
            for cte in &w.ctes {
                if w.recursive {
                    ctes.push(cte.name.name.clone());
                }
                if select_has(&cte.select, table_name, old_col, &ctes) {
                    return true;
                }
                if !w.recursive {
                    ctes.push(cte.name.name.clone());
                }
            }
        }
        body_has(&sel.body, table_name, old_col, &ctes)
            || sel
                .order_by
                .as_ref()
                .is_some_and(|ob| ob.iter().any(|o| expr_has(&o.expr, table_name, old_col, &ctes)))
            || sel.limit.as_ref().is_some_and(|l| {
                expr_has(&l.count, table_name, old_col, &ctes)
                    || l.offset
                        .as_deref()
                        .is_some_and(|o| expr_has(o, table_name, old_col, &ctes))
            })
    }
    if ct
        .when
        .as_ref()
        .is_some_and(|e| expr_has(e, table_name, old_col, &[]))
    {
        return true;
    }
    for step in &ct.body {
        let hit = match step {
            TriggerStep::Select(s) => select_has(s, table_name, old_col, &[]),
            TriggerStep::Insert(ins) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = ins.with {
                    for cte in &w.ctes {
                        if w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                        if select_has(&cte.select, table_name, old_col, &ctes) {
                            return true;
                        }
                        if !w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                    }
                }
                ins.select
                    .as_ref()
                    .is_some_and(|s| select_has(s, table_name, old_col, &ctes))
                    || ins.upsert.iter().any(|up| {
                        up.target_where
                            .as_ref()
                            .is_some_and(|e| expr_has(e, table_name, old_col, &ctes))
                            || up.set.as_ref().is_some_and(|sets| {
                                sets.iter()
                                    .any(|sc| expr_has(&sc.expr, table_name, old_col, &ctes))
                            })
                            || up
                                .where_clause
                                .as_ref()
                                .is_some_and(|e| expr_has(e, table_name, old_col, &ctes))
                    })
            }
            TriggerStep::Update(upd) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = upd.with {
                    for cte in &w.ctes {
                        if w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                        if select_has(&cte.select, table_name, old_col, &ctes) {
                            return true;
                        }
                        if !w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                    }
                }
                upd.from
                    .as_ref()
                    .is_some_and(|f| from_has(f, table_name, old_col, &ctes))
                    || upd
                        .where_clause
                        .as_ref()
                        .is_some_and(|e| expr_has(e, table_name, old_col, &ctes))
                    || upd
                        .set
                        .iter()
                        .any(|s| expr_has(&s.expr, table_name, old_col, &ctes))
            }
            TriggerStep::Delete(del) => {
                let mut ctes = Vec::new();
                if let Some(ref w) = del.with {
                    for cte in &w.ctes {
                        if w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                        if select_has(&cte.select, table_name, old_col, &ctes) {
                            return true;
                        }
                        if !w.recursive {
                            ctes.push(cte.name.name.clone());
                        }
                    }
                }
                del.where_clause
                    .as_ref()
                    .is_some_and(|e| expr_has(e, table_name, old_col, &ctes))
            }
        };
        if hit {
            return true;
        }
    }
    false
}
