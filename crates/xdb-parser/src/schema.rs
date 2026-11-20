//! The `build.c` table and index model: from a parsed `CREATE TABLE` /
//! `CREATE INDEX` to the definitions SQLite keeps in memory (`struct Table` /
//! `Column` / `Index`), ported from SQLite 3.54.1 `build.c`.

use std::collections::HashSet;

use xdb_core::value::affinity_type;
use xdb_core::Affinity;

use crate::ast::*;
use crate::errors::{ParseResult, SqlParseError, SqlParseErrorKind};
use crate::text::{dequote, same_name, stored_type, StandardType};

/// `pragma table_xinfo` `"hidden"`: 0 normal, 1 hidden (vtab), 2 VIRTUAL generated, 3 STORED generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColumnHidden {
    Normal = 0,
    Hidden = 1,
    Virtual = 2,
    Stored = 3,
}

/// `OE_*` conflict resolution as stored on constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnError {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
    Default,
    None,
}

impl OnError {
    fn from_conflict(c: Option<ConflictAction>) -> Self {
        match c {
            None => OnError::Default,
            Some(ConflictAction::Rollback) => OnError::Rollback,
            Some(ConflictAction::Abort) => OnError::Abort,
            Some(ConflictAction::Fail) => OnError::Fail,
            Some(ConflictAction::Ignore) => OnError::Ignore,
            Some(ConflictAction::Replace) => OnError::Replace,
        }
    }
}

/// `pragma index_list` `"origin"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexOrigin {
    /// `'c'`: `CREATE INDEX`
    CreateIndex,
    /// `'u'`: `UNIQUE` constraint
    Unique,
    /// `'pk'`: `PRIMARY KEY` constraint
    PrimaryKey,
}

impl IndexOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            IndexOrigin::CreateIndex => "c",
            IndexOrigin::Unique => "u",
            IndexOrigin::PrimaryKey => "pk",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub cid: usize,
    pub name: String,
    pub decl_type: String,
    pub standard_type: Option<StandardType>,
    pub affinity: Affinity,
    pub not_null: bool,
    pub not_null_on_error: OnError,
    pub default_text: Option<String>,
    pub default_expr: Option<Expr>,
    pub collation: Option<String>,
    pub hidden: ColumnHidden,
    pub generated: Option<Expr>,
    pub primary_key: bool,
    /// `pragma table_info` `"pk"`: 1-based position in primary key, or 0.
    pub pk: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnDefinition {
    /// Table column index (`>= 0`), `-1` for rowid, `-2` for an expression.
    pub cid: i32,
    pub name: Option<String>,
    pub expr: Option<Expr>,
    pub desc: bool,
    pub collation: String,
    pub key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinition {
    pub name: String,
    pub table: String,
    pub origin: IndexOrigin,
    pub on_error: OnError,
    pub unique: bool,
    pub partial: bool,
    pub where_clause: Option<Expr>,
    pub columns: Vec<IndexColumnDefinition>,
    pub key_count: usize,
    pub auto: bool,
    pub without_rowid_primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDefinition {
    pub name: String,
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDefinition {
    pub from: Vec<usize>,
    pub table: String,
    pub to: Option<Vec<String>>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
    pub deferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryKeyColumn {
    pub cid: i32,
    pub desc: bool,
    pub collation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDefinition {
    pub name: String,
    pub schema: Option<String>,
    pub temp: bool,
    pub columns: Vec<ColumnDefinition>,
    /// The `INTEGER PRIMARY KEY` (rowid alias) column index, or `None` (`Table.iPKey = -1`).
    pub ipk_column: Option<usize>,
    pub ipk_on_error: OnError,
    pub primary_key: Vec<PrimaryKeyColumn>,
    pub autoincrement: bool,
    pub without_rowid: bool,
    pub strict: bool,
    pub has_virtual_columns: bool,
    pub has_stored_columns: bool,
    /// Table B-tree record layout: the column `cid` stored in each record field.
    pub record_columns: Vec<usize>,
    pub indexes: Vec<IndexDefinition>,
    pub checks: Vec<CheckDefinition>,
    pub foreign_keys: Vec<ForeignKeyDefinition>,
}

/// Options for [`build_table`] and [`build_index`].
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Building from `sqlite_schema` text (`db->init.busy`).
    pub schema: bool,
    /// Schema format number (database header offset 44). Default 4.
    pub file_format: u32,
    /// `SQLITE_LIMIT_COLUMN` (default 2000).
    pub max_columns: usize,
    /// `SQLITE_DBCONFIG_DQS_DDL`; defaults to `None` (which inherits `schema`).
    pub dqs: Option<bool>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            schema: false,
            file_format: 4,
            max_columns: 2000,
            dqs: None,
        }
    }
}

fn def_err(message: impl Into<String>, span: Option<Span>) -> SqlParseError {
    SqlParseError::new(
        message,
        SqlParseErrorKind::Definition,
        span.map(|s| s.start),
        span.map(|s| s.end),
    )
}

fn std_type_affinity(std_ty: StandardType) -> Affinity {
    match std_ty {
        StandardType::Any => Affinity::Numeric,
        StandardType::Blob => Affinity::Blob,
        StandardType::Int | StandardType::Integer => Affinity::Integer,
        StandardType::Real => Affinity::Real,
        StandardType::Text => Affinity::Text,
    }
}

/// Output of [`declared_type`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredType {
    pub stored: Option<String>,
    pub standard_type: Option<StandardType>,
    pub affinity: Affinity,
}

/// The declared type as `sqlite3AddColumn` stores it and the column affinity it implies
/// (before `STRICT` adjustments).
pub fn declared_type(raw_type: Option<&str>) -> DeclaredType {
    let t = stored_type(raw_type);
    if let Some(std_ty) = t.standard_type {
        return DeclaredType {
            stored: None,
            standard_type: Some(std_ty),
            affinity: std_type_affinity(std_ty),
        };
    }
    match t.stored {
        None => DeclaredType {
            stored: None,
            standard_type: None,
            affinity: Affinity::Blob,
        },
        Some(stored) => {
            let aff = affinity_type(stored.as_bytes());
            DeclaredType {
                stored: Some(stored),
                standard_type: None,
                affinity: aff,
            }
        }
    }
}

struct BuildState {
    table_name: String,
    cols: Vec<ColumnDefinition>,
    col_expr_set: Vec<bool>,
    unique: Vec<bool>,
    has_primary_key: bool,
    ipk: Option<usize>,
    ipk_on_error: OnError,
    ipk_sort_desc: bool,
    autoincrement: bool,
    indexes: Vec<IndexDefinition>,
    checks: Vec<CheckDefinition>,
    fks: Vec<ForeignKeyDefinition>,
    sort_mask: bool,
    dqs: bool,
}

fn column_index(cols: &[ColumnDefinition], name: &str) -> Option<usize> {
    cols.iter().position(|c| same_name(&c.name, name))
}

fn skip_collate(mut e: &Expr) -> &Expr {
    while let Expr::Collate { operand, .. } = e {
        e = operand;
    }
    e
}

#[derive(Clone, Copy)]
enum SelfRefContext {
    Index,
    Where,
    Check,
    Generated,
}

impl SelfRefContext {
    const fn text(self) -> &'static str {
        match self {
            SelfRefContext::Index => "index expressions",
            SelfRefContext::Where => "partial index WHERE clauses",
            SelfRefContext::Check => "CHECK constraints",
            SelfRefContext::Generated => "generated columns",
        }
    }
}

struct SelfRefTable<'a> {
    name: &'a str,
    cols: &'a [ColumnDefinition],
    without_rowid: bool,
    dqs: bool,
}

fn is_rowid_name(n: &str) -> bool {
    same_name(n, "rowid") || same_name(n, "oid") || same_name(n, "_rowid_")
}

/// Returns `Ok(Some(cid))` (`cid >= 0` for column, `-1` for rowid) or `Ok(None)` for TRUE/FALSE/DQS.
fn resolve_column_ref(
    x: &ColumnRef,
    ctx: SelfRefContext,
    t: &SelfRefTable<'_>,
) -> ParseResult<Option<i32>> {
    let mut pending: Option<SqlParseError> = None;
    if x.table.is_some()
        && matches!(ctx, SelfRefContext::Index | SelfRefContext::Generated)
    {
        pending = Some(def_err(
            format!("the \".\" operator prohibited in {}", ctx.text()),
            Some(x.span),
        ));
    }
    let n = &x.column.name;
    let table_ok = x.table.as_ref().is_none_or(|tb| same_name(&tb.name, t.name))
        && x.schema
            .as_ref()
            .is_none_or(|sc| same_name(&sc.name, "main"));
    if table_ok {
        if let Some(i) = column_index(t.cols, n) {
            if let Some(err) = pending {
                return Err(err);
            }
            return Ok(Some(i as i32));
        }
        if !t.without_rowid
            && matches!(ctx, SelfRefContext::Where | SelfRefContext::Check)
            && is_rowid_name(n)
        {
            if let Some(err) = pending {
                return Err(err);
            }
            return Ok(Some(-1));
        }
    }
    if x.table.is_none()
        && x.column.quote == QuoteStyle::None
        && (same_name(n, "true") || same_name(n, "false"))
    {
        if let Some(err) = pending {
            return Err(err);
        }
        return Ok(None);
    }
    if x.table.is_none() && x.column.quote == QuoteStyle::Double && t.dqs {
        return Ok(None);
    }
    let q = match (&x.schema, &x.table) {
        (Some(sc), Some(tb)) => format!("{}.{}.", sc.name, tb.name),
        (None, Some(tb)) => format!("{}.", tb.name),
        _ => String::new(),
    };
    Err(def_err(format!("no such column: {q}{n}"), Some(x.span)))
}

fn resolve_self_ref(e: &Expr, ctx: SelfRefContext, t: &SelfRefTable<'_>) -> ParseResult<()> {
    match e {
        Expr::InList { operand, items, .. } => {
            if items.is_empty() && !fold_expr(operand, false).has_func {
                return Ok(());
            }
            for c in expr_children(e) {
                resolve_self_ref(c, ctx, t)?;
            }
            Ok(())
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            if *op == BinaryOp::And {
                let l = fold_expr(left, false);
                let r = fold_expr(right, false);
                if (l.is_false || r.is_false) && !l.has_func && !r.has_func {
                    return Ok(());
                }
            }
            resolve_self_ref(left, ctx, t)?;
            resolve_self_ref(right, ctx, t)
        }
        Expr::Column(c) => {
            resolve_column_ref(c, ctx, t)?;
            Ok(())
        }
        Expr::Variable(v) => Err(def_err(
            format!("parameters prohibited in {}", ctx.text()),
            Some(v.span),
        )),
        Expr::Subquery { span, .. } | Expr::Exists { span, .. } => Err(def_err(
            format!("subqueries prohibited in {}", ctx.text()),
            Some(*span),
        )),
        Expr::InSelect { span, operand, .. } | Expr::InTable { span, operand, .. } => {
            resolve_self_ref(operand, ctx, t)?;
            Err(def_err(
                format!("subqueries prohibited in {}", ctx.text()),
                Some(*span),
            ))
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            resolve_self_ref(right, ctx, t)?;
            resolve_self_ref(left, ctx, t)?;
            if let Some(esc) = escape {
                resolve_self_ref(esc, ctx, t)?;
            }
            Ok(())
        }
        _ => {
            for c in expr_children(e) {
                resolve_self_ref(c, ctx, t)?;
            }
            Ok(())
        }
    }
}

/// Resolve an index / PRIMARY KEY term: `Some(cid)` (`>= 0` or `-1` rowid), or `None` for an expression.
fn resolve_index_term(
    e: &Expr,
    t: &SelfRefTable<'_>,
    primary_key_list: bool,
) -> ParseResult<Option<i32>> {
    let p = skip_collate(e);
    let one_level_collate = match e {
        Expr::Collate { operand, .. } => std::ptr::eq(operand.as_ref(), p),
        _ => std::ptr::eq(e, p),
    };
    if let Expr::String(s) = p {
        if primary_key_list || one_level_collate {
            if let Some(i) = column_index(t.cols, &s.value) {
                return Ok(Some(i as i32));
            }
            return Err(def_err(
                format!("no such column: {}", s.value),
                Some(s.span),
            ));
        }
    }
    if let Expr::Column(c) = p {
        return resolve_column_ref(c, SelfRefContext::Index, t);
    }
    resolve_self_ref(p, SelfRefContext::Index, t)?;
    Ok(None)
}

/// Direct sub-expressions of `e` (not descending into subqueries).
pub fn expr_children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => vec![operand.as_ref()],
        Expr::Binary { left, right, .. } => vec![left.as_ref(), right.as_ref()],
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            let mut out = vec![left.as_ref(), right.as_ref()];
            if let Some(esc) = escape {
                out.push(esc.as_ref());
            }
            out
        }
        Expr::Between {
            operand, low, high, ..
        } => vec![operand.as_ref(), low.as_ref(), high.as_ref()],
        Expr::InList { operand, items, .. } => {
            let mut out = Vec::with_capacity(1 + items.len());
            out.push(operand.as_ref());
            for item in items {
                out.push(item);
            }
            out
        }
        Expr::InSelect { operand, .. } => vec![operand.as_ref()],
        Expr::InTable { operand, args, .. } => {
            let mut out = Vec::with_capacity(1 + args.as_ref().map_or(0, Vec::len));
            out.push(operand.as_ref());
            if let Some(args) = args {
                for a in args {
                    out.push(a);
                }
            }
            out
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            let mut out = Vec::new();
            if let Some(op) = operand {
                out.push(op.as_ref());
            }
            for w in whens {
                out.push(&w.when);
                out.push(&w.then);
            }
            if let Some(el) = else_expr {
                out.push(el.as_ref());
            }
            out
        }
        Expr::Function(f) => {
            let mut out: Vec<&Expr> = f.args.iter().collect();
            if let Some(ref ob) = f.order_by {
                for o in ob {
                    out.push(&o.expr);
                }
            }
            if let Some(ref flt) = f.filter {
                out.push(flt.as_ref());
            }
            out
        }
        Expr::Vector { items, .. } => items.iter().collect(),
        Expr::Raise { message, .. } => message.as_deref().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `sqlite3GetInt32` (`util.c:1310`) on an unsigned `INTEGER` token: its value when it fits
/// in a non-negative `i32` (`0..=2147483647`), else `None`.
pub fn get_int32_token(text: &str) -> Option<i32> {
    let b = text.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut i = 0usize;
    if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        i = 2;
        while i < b.len() && b[i] == b'0' {
            i += 1;
        }
        let mut u: u64 = 0;
        let mut k = 0usize;
        while k < 8 && i + k < b.len() && b[i + k].is_ascii_hexdigit() {
            let d = if b[i + k] <= b'9' {
                b[i + k] - b'0'
            } else {
                (b[i + k] | 0x20) - b'a' + 10
            };
            u = (u << 4) | u64::from(d);
            k += 1;
        }
        if u >= 0x8000_0000 || (i + k < b.len() && b[i + k].is_ascii_hexdigit()) {
            return None;
        }
        return Some(u as i32);
    }
    while i < b.len() && b[i] == b'0' {
        i += 1;
    }
    let mut v: u64 = 0;
    let mut k = 0usize;
    while k < 11 && i + k < b.len() {
        let c = b[i + k];
        if !c.is_ascii_digit() {
            break;
        }
        v = v * 10 + u64::from(c - b'0');
        k += 1;
    }
    if k > 10 || v > 2_147_483_647 {
        return None;
    }
    Some(v as i32)
}

#[derive(Clone, Copy)]
struct Folded {
    constant: bool,
    has_func: bool,
    is_false: bool,
    literal: bool,
}

const NOT_CONST: Folded = Folded {
    constant: false,
    has_func: false,
    is_false: false,
    literal: false,
};

fn fold_expr(e: &Expr, schema_mode: bool) -> Folded {
    match e {
        Expr::Number(n) => {
            let v = if !n.is_float && !n.separators {
                get_int32_token(&n.text)
            } else {
                None
            };
            Folded {
                constant: true,
                has_func: false,
                is_false: v == Some(0),
                literal: true,
            }
        }
        Expr::String(_) | Expr::Blob(_) => Folded {
            constant: true,
            has_func: false,
            is_false: false,
            literal: true,
        },
        Expr::Null { .. } => Folded {
            constant: true,
            has_func: false,
            is_false: false,
            literal: false,
        },
        Expr::Boolean { value, .. } => Folded {
            constant: true,
            has_func: false,
            is_false: !*value,
            literal: false,
        },
        Expr::CurrentTime { .. } => Folded {
            constant: true,
            has_func: true,
            is_false: false,
            literal: false,
        },
        Expr::Column(c) => {
            if c.table.is_none()
                && c.column.quote == QuoteStyle::None
                && (same_name(&c.column.name, "true") || same_name(&c.column.name, "false"))
            {
                Folded {
                    constant: true,
                    has_func: false,
                    is_false: false,
                    literal: false,
                }
            } else {
                NOT_CONST
            }
        }
        Expr::Variable(_) => {
            if schema_mode {
                Folded {
                    constant: true,
                    has_func: false,
                    is_false: false,
                    literal: false,
                }
            } else {
                NOT_CONST
            }
        }
        Expr::Raise { .. }
        | Expr::Exists { .. }
        | Expr::Subquery { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. } => NOT_CONST,
        Expr::Binary {
            op, left, right, ..
        } => {
            let l = fold_expr(left, schema_mode);
            let r = fold_expr(right, schema_mode);
            if *op == BinaryOp::And && (l.is_false || r.is_false) && !l.has_func && !r.has_func {
                return Folded {
                    constant: true,
                    has_func: false,
                    is_false: true,
                    literal: true,
                };
            }
            let fn_op = matches!(op, BinaryOp::Ptr | BinaryOp::Ptr2);
            Folded {
                constant: l.constant && r.constant,
                has_func: fn_op || l.has_func || r.has_func,
                is_false: false,
                literal: false,
            }
        }
        Expr::IsNull { not, operand, .. } => {
            let inner = fold_expr(operand, schema_mode);
            let mut p = operand.as_ref();
            while let Expr::Unary {
                op: UnaryOp::Pos | UnaryOp::Neg,
                operand: u,
                ..
            } = p
            {
                p = u.as_ref();
            }
            let pf = if std::ptr::eq(p, operand.as_ref()) {
                inner
            } else {
                fold_expr(p, schema_mode)
            };
            if pf.literal {
                return Folded {
                    constant: true,
                    has_func: false,
                    is_false: !*not,
                    literal: true,
                };
            }
            Folded {
                constant: inner.constant,
                has_func: inner.has_func,
                is_false: false,
                literal: false,
            }
        }
        Expr::InList {
            not,
            operand,
            items,
            ..
        } => {
            let op = fold_expr(operand, schema_mode);
            if items.is_empty() {
                if !op.has_func {
                    return Folded {
                        constant: true,
                        has_func: false,
                        is_false: !*not,
                        literal: false,
                    };
                }
                return Folded {
                    constant: op.constant,
                    has_func: true,
                    is_false: false,
                    literal: false,
                };
            }
            let mut constant = op.constant;
            let mut has_func = op.has_func;
            for x in items {
                let f = fold_expr(x, schema_mode);
                constant = constant && f.constant;
                has_func = has_func || f.has_func;
            }
            Folded {
                constant,
                has_func,
                is_false: false,
                literal: false,
            }
        }
        Expr::Function(f) => {
            let mut constant = f.over.is_none() && f.filter.is_none();
            for x in expr_children(e) {
                constant = constant && fold_expr(x, schema_mode).constant;
            }
            Folded {
                constant,
                has_func: true,
                is_false: false,
                literal: false,
            }
        }
        Expr::Like { .. } => {
            let mut constant = true;
            for x in expr_children(e) {
                constant = constant && fold_expr(x, schema_mode).constant;
            }
            Folded {
                constant,
                has_func: true,
                is_false: false,
                literal: false,
            }
        }
        _ => {
            let mut constant = true;
            let mut has_func = false;
            for x in expr_children(e) {
                let f = fold_expr(x, schema_mode);
                constant = constant && f.constant;
                has_func = has_func || f.has_func;
            }
            Folded {
                constant,
                has_func,
                is_false: false,
                literal: false,
            }
        }
    }
}

/// `sqlite3ExprIsConstantOrFunction` (`expr.c:2920`), the `DEFAULT`-value rule.
pub fn is_constant_or_function(e: &Expr, schema_mode: bool) -> bool {
    fold_expr(e, schema_mode).constant
}

fn unsupported_nulls(terms: Option<&[OrderingTerm]>) -> ParseResult<()> {
    let Some(terms) = terms else {
        return Ok(());
    };
    for t in terms {
        if let Some(nulls) = t.nulls {
            let desc = t.order == Some(SortOrder::Desc);
            let nulls_asc = nulls == NullsOrder::First;
            let sf = (u8::from(desc)) | (if desc == nulls_asc { 2 } else { 0 });
            let word = if sf == 0 || sf == 3 { "FIRST" } else { "LAST" };
            return Err(def_err(
                format!("unsupported use of NULLS {word}"),
                Some(t.span),
            ));
        }
    }
    Ok(())
}

fn reorder_replace(list: &mut [IndexDefinition]) {
    for i in 0..list.len() {
        if list[i].on_error != OnError::Replace {
            continue;
        }
        let mut j = i;
        while j + 1 < list.len() && list[j + 1].on_error != OnError::Replace {
            list.swap(j, j + 1);
            j += 1;
        }
        return;
    }
}

fn create_auto_index(
    st: &mut BuildState,
    terms: Option<&[OrderingTerm]>,
    on_error: OnError,
    sort_desc: bool,
    origin: IndexOrigin,
) -> ParseResult<()> {
    unsupported_nulls(terms)?;
    let name = format!("sqlite_autoindex_{}_{}", st.table_name, st.indexes.len() + 1);
    let mut keys: Vec<(i32, String, bool)> = Vec::new();
    match terms {
        None => {
            let i = st.cols.len() - 1;
            st.unique[i] = true;
            let coll = st.cols[i]
                .collation
                .clone()
                .unwrap_or_else(|| "BINARY".to_string());
            keys.push((i as i32, coll, sort_desc && st.sort_mask));
        }
        Some(terms) => {
            let self_tbl = SelfRefTable {
                name: &st.table_name,
                cols: &st.cols,
                without_rowid: false,
                dqs: st.dqs,
            };
            for term in terms {
                let j0 =
                    resolve_index_term(&term.expr, &self_tbl, origin == IndexOrigin::PrimaryKey)?;
                let Some(j0) = j0 else {
                    return Err(def_err(
                        "expressions prohibited in PRIMARY KEY and UNIQUE constraints",
                        Some(term.span),
                    ));
                };
                let j = if j0 < 0 {
                    st.ipk.map_or(-1, |i| i as i32)
                } else {
                    j0
                };
                let coll = if let Expr::Collate { collation, .. } = &term.expr {
                    collation.name.clone()
                } else if j >= 0 {
                    st.cols[j as usize]
                        .collation
                        .clone()
                        .unwrap_or_else(|| "BINARY".to_string())
                } else {
                    "BINARY".to_string()
                };
                keys.push((j, coll, term.order == Some(SortOrder::Desc) && st.sort_mask));
            }
        }
    }

    let key_count = keys.len();
    let mut columns = Vec::with_capacity(key_count + 1);
    for (cid, collation, desc) in keys {
        columns.push(IndexColumnDefinition {
            cid,
            name: if cid >= 0 {
                Some(st.cols[cid as usize].name.clone())
            } else {
                None
            },
            expr: None,
            desc,
            collation,
            key: true,
        });
    }
    columns.push(IndexColumnDefinition {
        cid: -1,
        name: None,
        expr: None,
        desc: false,
        collation: "BINARY".to_string(),
        key: false,
    });

    // Equivalent automatic index merge (build.c:4410).
    for other in &mut st.indexes {
        if other.key_count != key_count {
            continue;
        }
        let same = (0..key_count).all(|k| {
            let a = &other.columns[k];
            let b = &columns[k];
            a.cid == b.cid && same_name(&a.collation, &b.collation)
        });
        if same {
            if other.on_error != on_error {
                if other.on_error != OnError::Default && on_error != OnError::Default {
                    return Err(def_err("conflicting ON CONFLICT clauses specified", None));
                }
                if other.on_error == OnError::Default {
                    other.on_error = on_error;
                }
            }
            if origin == IndexOrigin::PrimaryKey {
                other.origin = IndexOrigin::PrimaryKey;
            }
            reorder_replace(&mut st.indexes);
            return Ok(());
        }
    }

    st.indexes.insert(
        0,
        IndexDefinition {
            name,
            table: st.table_name.clone(),
            origin,
            on_error,
            unique: on_error != OnError::None,
            partial: false,
            where_clause: None,
            columns,
            key_count,
            auto: true,
            without_rowid_primary_key: false,
        },
    );
    reorder_replace(&mut st.indexes);
    Ok(())
}

fn mark_primary_key_column(st: &mut BuildState, i: usize) -> ParseResult<()> {
    let c = &mut st.cols[i];
    c.primary_key = true;
    if matches!(c.hidden, ColumnHidden::Virtual | ColumnHidden::Stored) {
        return Err(def_err(
            "generated columns cannot be part of the PRIMARY KEY",
            None,
        ));
    }
    Ok(())
}

fn add_primary_key(
    st: &mut BuildState,
    terms: Option<&[OrderingTerm]>,
    on_error: OnError,
    auto_inc: bool,
    sort_desc: bool,
) -> ParseResult<()> {
    if st.has_primary_key {
        return Err(def_err(
            format!("table \"{}\" has more than one primary key", st.table_name),
            None,
        ));
    }
    st.has_primary_key = true;
    let mut i_col: Option<usize> = None;
    let n_term: usize;
    match terms {
        None => {
            let idx = st.cols.len() - 1;
            i_col = Some(idx);
            mark_primary_key_column(st, idx)?;
            n_term = 1;
        }
        Some(terms) => {
            n_term = terms.len();
            for term in terms {
                let p = skip_collate(&term.expr);
                let nm = match p {
                    Expr::String(s) => Some(s.value.as_str()),
                    Expr::Column(c) if c.table.is_none() => Some(c.column.name.as_str()),
                    _ => None,
                };
                if let Some(nm) = nm {
                    if let Some(j) = column_index(&st.cols, nm) {
                        i_col = Some(j);
                        mark_primary_key_column(st, j)?;
                    }
                }
            }
        }
    }
    if n_term == 1
        && i_col.is_some_and(|i| st.cols[i].standard_type == Some(StandardType::Integer))
        && !sort_desc
    {
        st.ipk = i_col;
        st.ipk_on_error = on_error;
        if auto_inc {
            st.autoincrement = true;
        }
        if let Some(terms) = terms {
            st.ipk_sort_desc = terms[0].order == Some(SortOrder::Desc);
            unsupported_nulls(Some(terms))?;
        }
    } else if auto_inc {
        return Err(def_err(
            "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY",
            None,
        ));
    } else {
        create_auto_index(st, terms, on_error, sort_desc, IndexOrigin::PrimaryKey)?;
    }
    Ok(())
}

fn is_dup_column(
    idx: &[IndexColumnDefinition],
    n_key: usize,
    pk: &IndexColumnDefinition,
) -> bool {
    idx.iter()
        .take(n_key)
        .any(|c| c.cid == pk.cid && same_name(&c.collation, &pk.collation))
}

fn convert_to_without_rowid(st: &mut BuildState) -> ParseResult<()> {
    for col in &mut st.cols {
        if col.primary_key && !col.not_null {
            col.not_null = true;
            col.not_null_on_error = OnError::Abort;
        }
    }
    if let Some(ipk) = st.ipk.take() {
        let name = st.cols[ipk].name.clone();
        let term = OrderingTerm {
            span: Span::default(),
            expr: Expr::Column(ColumnRef {
                span: Span::default(),
                schema: None,
                table: None,
                column: Name {
                    span: Span::default(),
                    name,
                    quote: QuoteStyle::None,
                },
            }),
            order: if st.ipk_sort_desc {
                Some(SortOrder::Desc)
            } else {
                None
            },
            nulls: None,
        };
        let on_err = st.ipk_on_error;
        create_auto_index(st, Some(&[term]), on_err, false, IndexOrigin::PrimaryKey)?;
    }

    let pk_idx = st
        .indexes
        .iter()
        .position(|x| x.origin == IndexOrigin::PrimaryKey)
        .expect("WITHOUT ROWID table has a PRIMARY KEY index");

    // Remove redundant columns from the PRIMARY KEY
    {
        let pk = &mut st.indexes[pk_idx];
        let mut kept: Vec<IndexColumnDefinition> = Vec::new();
        for c in pk.columns.iter().take(pk.key_count) {
            if kept.is_empty() || !is_dup_column(&kept, kept.len(), c) {
                kept.push(c.clone());
            }
        }
        pk.key_count = kept.len();
        pk.columns = kept;
        pk.without_rowid_primary_key = true;
    }

    let pk_keys: Vec<IndexColumnDefinition> = st.indexes[pk_idx].columns.clone();
    let n_pk = pk_keys.len();

    // Replace the rowid tail of the other automatic indexes with the primary-key columns
    for (i, idx) in st.indexes.iter_mut().enumerate() {
        if i == pk_idx {
            continue;
        }
        let mut new_cols: Vec<IndexColumnDefinition> =
            idx.columns.iter().take(idx.key_count).cloned().collect();
        for p in pk_keys.iter().take(n_pk) {
            if !is_dup_column(&new_cols, idx.key_count, p) {
                new_cols.push(IndexColumnDefinition {
                    cid: p.cid,
                    name: p.name.clone(),
                    expr: None,
                    desc: false, // bAscKeyBug
                    collation: p.collation.clone(),
                    key: false,
                });
            }
        }
        idx.columns = new_cols;
    }

    // Add every other non-VIRTUAL table column to the PRIMARY KEY index
    let pk = &mut st.indexes[pk_idx];
    let mut in_pk: HashSet<i32> = pk.columns.iter().map(|c| c.cid).collect();
    for (i, col) in st.cols.iter().enumerate() {
        let cid = i as i32;
        if in_pk.contains(&cid) || col.hidden == ColumnHidden::Virtual {
            continue;
        }
        pk.columns.push(IndexColumnDefinition {
            cid,
            name: Some(col.name.clone()),
            expr: None,
            desc: false,
            collation: col
                .collation
                .clone()
                .unwrap_or_else(|| "BINARY".to_string()),
            key: false,
        });
        in_pk.insert(cid);
    }
    Ok(())
}

/// Build the SQLite `build.c` table definition from a parsed `CREATE TABLE` statement.
pub fn build_table(
    stmt: &CreateTableStmt,
    sql: &str,
    options: &BuildOptions,
) -> ParseResult<TableDefinition> {
    if stmt.as_select.is_some() {
        return Err(SqlParseError::new(
            "CREATE TABLE ... AS SELECT is not supported",
            SqlParseErrorKind::Unsupported,
            Some(stmt.span.start),
            Some(stmt.span.end),
        ));
    }
    let schema_mode = options.schema;
    let dqs = options.dqs.unwrap_or(schema_mode);
    let mut st = BuildState {
        table_name: stmt.name.name.clone(),
        cols: Vec::with_capacity(stmt.columns.len()),
        col_expr_set: Vec::with_capacity(stmt.columns.len()),
        unique: Vec::with_capacity(stmt.columns.len()),
        has_primary_key: false,
        ipk: None,
        ipk_on_error: OnError::Default,
        ipk_sort_desc: false,
        autoincrement: false,
        indexes: Vec::new(),
        checks: Vec::new(),
        fks: Vec::new(),
        sort_mask: options.file_format >= 4,
        dqs,
    };
    let mut has_type: Vec<bool> = Vec::with_capacity(stmt.columns.len());

    for def in &stmt.columns {
        if st.cols.len() + 1 > options.max_columns {
            return Err(def_err(
                format!("too many columns on {}", st.table_name),
                Some(def.span),
            ));
        }
        let name = def.name.name.clone();
        if !st.cols.is_empty() && column_index(&st.cols, &name).is_some() {
            return Err(def_err(
                format!("duplicate column name: {name}"),
                Some(def.name.span),
            ));
        }
        let dt = declared_type(def.type_name.as_ref().map(|t| t.text.as_str()));
        has_type.push(dt.stored.is_some());
        let decl_type = dt
            .stored
            .or_else(|| dt.standard_type.map(|s| s.as_str().to_string()))
            .unwrap_or_default();
        let i_col = st.cols.len();
        st.cols.push(ColumnDefinition {
            cid: i_col,
            name: name.clone(),
            decl_type,
            standard_type: dt.standard_type,
            affinity: dt.affinity,
            not_null: false,
            not_null_on_error: OnError::None,
            default_text: None,
            default_expr: None,
            collation: None,
            hidden: ColumnHidden::Normal,
            generated: None,
            primary_key: false,
            pk: 0,
        });
        st.col_expr_set.push(false);
        st.unique.push(false);

        for c in &def.constraints {
            match c {
                ColumnConstraint::Default {
                    span, expr, text, ..
                } => {
                    if !is_constant_or_function(expr, schema_mode) {
                        return Err(def_err(
                            format!("default value of column [{name}] is not constant"),
                            Some(*span),
                        ));
                    }
                    if matches!(
                        st.cols[i_col].hidden,
                        ColumnHidden::Virtual | ColumnHidden::Stored
                    ) {
                        return Err(def_err(
                            "cannot use DEFAULT on a generated column",
                            Some(*span),
                        ));
                    }
                    st.cols[i_col].default_text = Some(text.clone());
                    st.cols[i_col].default_expr = Some(expr.clone());
                    st.col_expr_set[i_col] = true;
                }
                ColumnConstraint::Null { .. } => {}
                ColumnConstraint::NotNull { on_conflict, .. } => {
                    st.cols[i_col].not_null = true;
                    st.cols[i_col].not_null_on_error = OnError::from_conflict(*on_conflict);
                }
                ColumnConstraint::PrimaryKey {
                    order,
                    on_conflict,
                    autoincrement,
                    ..
                } => {
                    add_primary_key(
                        &mut st,
                        None,
                        OnError::from_conflict(*on_conflict),
                        *autoincrement,
                        *order == Some(SortOrder::Desc),
                    )?;
                }
                ColumnConstraint::Unique { on_conflict, .. } => {
                    create_auto_index(
                        &mut st,
                        None,
                        OnError::from_conflict(*on_conflict),
                        false,
                        IndexOrigin::Unique,
                    )?;
                }
                ColumnConstraint::Check(chk) => {
                    let chk_name = chk
                        .name
                        .as_ref()
                        .map(|n| n.name.clone())
                        .unwrap_or_else(|| dequote(&chk.text));
                    st.checks.push(CheckDefinition {
                        name: chk_name,
                        expr: chk.expr.clone(),
                    });
                }
                ColumnConstraint::References { span, clause, .. } => {
                    if let Some(ref to) = clause.columns {
                        if to.len() != 1 {
                            let raw_tbl = &sql[clause.table.span.start..clause.table.span.end];
                            return Err(def_err(
                                format!(
                                    "foreign key on {name} should reference only one column of table {raw_tbl}"
                                ),
                                Some(*span),
                            ));
                        }
                    }
                    st.fks.push(ForeignKeyDefinition {
                        from: vec![i_col],
                        table: clause.table.name.clone(),
                        to: clause
                            .columns
                            .as_ref()
                            .map(|cols| cols.iter().map(|x| x.name.name.clone()).collect()),
                        on_delete: clause.on_delete.unwrap_or(ForeignKeyAction::NoAction),
                        on_update: clause.on_update.unwrap_or(ForeignKeyAction::NoAction),
                        deferred: false,
                    });
                }
                ColumnConstraint::Deferrable { clause, .. } => {
                    if let Some(fk) = st.fks.last_mut() {
                        fk.deferred =
                            !clause.not && clause.initially == Some(InitiallyDeferred::Deferred);
                    }
                }
                ColumnConstraint::Collate { collation, .. } => {
                    st.cols[i_col].collation = Some(collation.name.clone());
                    for idx in &mut st.indexes {
                        if let Some(first) = idx.columns.first_mut() {
                            if first.cid == i_col as i32 {
                                first.collation = collation.name.clone();
                            }
                        }
                    }
                }
                ColumnConstraint::Generated {
                    span,
                    expr,
                    storage_token,
                    storage,
                    ..
                } => {
                    if st.col_expr_set[i_col] {
                        return Err(def_err(
                            format!("error in generated column \"{name}\""),
                            Some(*span),
                        ));
                    }
                    let hidden = match (storage_token, storage) {
                        (None, _) => ColumnHidden::Virtual,
                        (Some(_), Some(GeneratedStorage::Stored)) => ColumnHidden::Stored,
                        (Some(_), Some(GeneratedStorage::Virtual)) => ColumnHidden::Virtual,
                        (Some(_), None) => {
                            return Err(def_err(
                                format!("error in generated column \"{name}\""),
                                Some(*span),
                            ));
                        }
                    };
                    st.cols[i_col].hidden = hidden;
                    if st.cols[i_col].primary_key {
                        mark_primary_key_column(&mut st, i_col)?;
                    }
                    st.cols[i_col].generated = Some(expr.clone());
                    st.col_expr_set[i_col] = true;
                }
            }
        }
    }

    for c in &stmt.constraints {
        match c {
            TableConstraint::PrimaryKey {
                columns,
                autoincrement,
                on_conflict,
                ..
            } => {
                add_primary_key(
                    &mut st,
                    Some(columns),
                    OnError::from_conflict(*on_conflict),
                    *autoincrement,
                    false,
                )?;
            }
            TableConstraint::Unique {
                columns,
                on_conflict,
                ..
            } => {
                create_auto_index(
                    &mut st,
                    Some(columns),
                    OnError::from_conflict(*on_conflict),
                    false,
                    IndexOrigin::Unique,
                )?;
            }
            TableConstraint::Check(chk) => {
                let chk_name = chk
                    .name
                    .as_ref()
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| dequote(&chk.text));
                st.checks.push(CheckDefinition {
                    name: chk_name,
                    expr: chk.expr.clone(),
                });
            }
            TableConstraint::ForeignKey {
                span,
                columns,
                clause,
                ..
            } => {
                if let Some(ref to) = clause.columns {
                    if to.len() != columns.len() {
                        return Err(def_err(
                            "number of columns in foreign key does not match the number of columns in the referenced table",
                            Some(*span),
                        ));
                    }
                }
                let mut from = Vec::with_capacity(columns.len());
                for item in columns {
                    let Some(j) = column_index(&st.cols, &item.name.name) else {
                        return Err(def_err(
                            format!("unknown column \"{}\" in foreign key definition", item.name.name),
                            Some(item.span),
                        ));
                    };
                    from.push(j);
                }
                let deferred = clause
                    .deferrable
                    .is_some_and(|d| !d.not && d.initially == Some(InitiallyDeferred::Deferred));
                st.fks.push(ForeignKeyDefinition {
                    from,
                    table: clause.table.name.clone(),
                    to: clause
                        .columns
                        .as_ref()
                        .map(|cols| cols.iter().map(|x| x.name.name.clone()).collect()),
                    on_delete: clause.on_delete.unwrap_or(ForeignKeyAction::NoAction),
                    on_update: clause.on_update.unwrap_or(ForeignKeyAction::NoAction),
                    deferred,
                });
            }
        }
    }

    // ── sqlite3EndTable ──
    if stmt.strict {
        for col in &mut st.cols {
            let Some(std_ty) = col.standard_type else {
                if has_type[col.cid] {
                    return Err(def_err(
                        format!(
                            "unknown datatype for {}.{}: \"{}\"",
                            st.table_name, col.name, col.decl_type
                        ),
                        None,
                    ));
                }
                return Err(def_err(
                    format!("missing datatype for {}.{}", st.table_name, col.name),
                    None,
                ));
            };
            if std_ty == StandardType::Any {
                col.affinity = Affinity::Blob;
            }
            if col.primary_key && st.ipk != Some(col.cid) && !col.not_null {
                col.not_null = true;
                col.not_null_on_error = OnError::Abort;
            }
        }
    }

    if stmt.without_rowid {
        if st.autoincrement {
            return Err(def_err(
                "AUTOINCREMENT not allowed on WITHOUT ROWID tables",
                None,
            ));
        }
        if !st.has_primary_key {
            return Err(def_err(
                format!("PRIMARY KEY missing on table {}", st.table_name),
                None,
            ));
        }
        convert_to_without_rowid(&mut st)?;
    }

    // Resolve CHECK constraints and generated column expressions
    {
        let self_tbl = SelfRefTable {
            name: &st.table_name,
            cols: &st.cols,
            without_rowid: stmt.without_rowid,
            dqs,
        };
        for c in &st.checks {
            resolve_self_ref(&c.expr, SelfRefContext::Check, &self_tbl)?;
        }
        for col in &st.cols {
            if let Some(ref gen_expr) = col.generated {
                resolve_self_ref(gen_expr, SelfRefContext::Generated, &self_tbl)?;
            }
        }
    }

    let mut non_generated = 0usize;
    let mut has_virtual = false;
    let mut has_stored = false;
    for col in &st.cols {
        match col.hidden {
            ColumnHidden::Virtual => has_virtual = true,
            ColumnHidden::Stored => has_stored = true,
            _ => non_generated += 1,
        }
    }
    if (has_virtual || has_stored) && non_generated == 0 {
        return Err(def_err("must have at least one non-generated column", None));
    }

    // pragma table_info pk
    let pk_idx = st
        .indexes
        .iter()
        .position(|x| x.origin == IndexOrigin::PrimaryKey);
    let n_cols = st.cols.len();
    for col in &mut st.cols {
        if !col.primary_key {
            col.pk = 0;
        } else if let Some(pi) = pk_idx {
            let pk_cols = &st.indexes[pi].columns;
            let mut k = 1usize;
            while k <= n_cols && (k > pk_cols.len() || pk_cols[k - 1].cid != col.cid as i32) {
                k += 1;
            }
            col.pk = k;
        } else {
            col.pk = 1;
        }
    }

    let primary_key = if let Some(ipk) = st.ipk {
        vec![PrimaryKeyColumn {
            cid: ipk as i32,
            desc: st.ipk_sort_desc,
            collation: st.cols[ipk]
                .collation
                .clone()
                .unwrap_or_else(|| "BINARY".to_string()),
        }]
    } else if let Some(pi) = pk_idx {
        let pk = &st.indexes[pi];
        pk.columns
            .iter()
            .take(pk.key_count)
            .map(|c| PrimaryKeyColumn {
                cid: c.cid,
                desc: c.desc,
                collation: c.collation.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let record_columns = if stmt.without_rowid {
        if let Some(pi) = pk_idx {
            st.indexes[pi]
                .columns
                .iter()
                .map(|c| c.cid as usize)
                .collect()
        } else {
            Vec::new()
        }
    } else {
        st.cols
            .iter()
            .filter(|c| c.hidden != ColumnHidden::Virtual)
            .map(|c| c.cid)
            .collect()
    };

    Ok(TableDefinition {
        name: st.table_name,
        schema: stmt.schema.as_ref().map(|s| s.name.clone()),
        temp: stmt.temp && !schema_mode,
        columns: st.cols,
        ipk_column: st.ipk,
        ipk_on_error: st.ipk_on_error,
        primary_key,
        autoincrement: st.autoincrement,
        without_rowid: stmt.without_rowid,
        strict: stmt.strict,
        has_virtual_columns: has_virtual,
        has_stored_columns: has_stored,
        record_columns,
        indexes: st.indexes,
        checks: st.checks,
        foreign_keys: st.fks,
    })
}

/// `sqlite3CreateIndex` for `CREATE INDEX` on an existing table (`build.c:4021`).
pub fn build_index(
    stmt: &CreateIndexStmt,
    table: &TableDefinition,
    options: &BuildOptions,
) -> ParseResult<IndexDefinition> {
    let schema_mode = options.schema;
    let sort_mask = options.file_format >= 4;
    if !schema_mode
        && table.name.len() >= 7
        && same_name(&table.name[..7], "sqlite_")
    {
        return Err(def_err(
            format!("table {} may not be indexed", table.name),
            Some(stmt.table.span),
        ));
    }
    unsupported_nulls(Some(&stmt.columns))?;
    let cols = &table.columns;
    let pk = if table.without_rowid {
        table.indexes.iter().find(|x| x.without_rowid_primary_key)
    } else {
        None
    };
    let self_tbl = SelfRefTable {
        name: &table.name,
        cols,
        without_rowid: table.without_rowid,
        dqs: options.dqs.unwrap_or(schema_mode),
    };
    if let Some(ref wh) = stmt.where_clause {
        resolve_self_ref(wh, SelfRefContext::Where, &self_tbl)?;
    }

    let mut columns = Vec::with_capacity(stmt.columns.len() + 1);
    for term in &stmt.columns {
        let j0 = resolve_index_term(&term.expr, &self_tbl, false)?;
        let (cid, expr) = match j0 {
            None => (-2, Some(term.expr.clone())),
            Some(j) if j < 0 => (table.ipk_column.map_or(-1, |i| i as i32), None),
            Some(j) => (j, None),
        };
        let collation = if let Expr::Collate { collation, .. } = &term.expr {
            collation.name.clone()
        } else if cid >= 0 {
            cols[cid as usize]
                .collation
                .clone()
                .unwrap_or_else(|| "BINARY".to_string())
        } else {
            "BINARY".to_string()
        };
        columns.push(IndexColumnDefinition {
            cid,
            name: if cid >= 0 {
                Some(cols[cid as usize].name.clone())
            } else {
                None
            },
            expr,
            desc: term.order == Some(SortOrder::Desc) && sort_mask,
            collation,
            key: true,
        });
    }
    let key_count = columns.len();
    if let Some(pk) = pk {
        for p in pk.columns.iter().take(pk.key_count) {
            if !is_dup_column(&columns, key_count, p) {
                columns.push(IndexColumnDefinition {
                    cid: p.cid,
                    name: p.name.clone(),
                    expr: None,
                    desc: p.desc,
                    collation: p.collation.clone(),
                    key: false,
                });
            }
        }
    } else {
        columns.push(IndexColumnDefinition {
            cid: -1,
            name: None,
            expr: None,
            desc: false,
            collation: "BINARY".to_string(),
            key: false,
        });
    }
    let on_error = if stmt.unique {
        OnError::Abort
    } else {
        OnError::None
    };
    Ok(IndexDefinition {
        name: stmt.name.name.clone(),
        table: table.name.clone(),
        origin: IndexOrigin::CreateIndex,
        on_error,
        unique: stmt.unique,
        partial: stmt.where_clause.is_some(),
        where_clause: stmt.where_clause.clone(),
        columns,
        key_count,
        auto: false,
        without_rowid_primary_key: false,
    })
}
