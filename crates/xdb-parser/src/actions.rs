//! Grammar rule actions for SQLite 3.54.1 (`src/parse.y`) and XDB dialect
//! additions, dispatched by [`ActionId`].

use std::collections::HashMap;

use crate::ast::*;
use crate::errors::SqlParseErrorKind;
use crate::lalr_tables::ActionId;
use crate::parser::{expr_flag, Parser};
use crate::text::{dequote, same_name, stored_type};
use crate::tokenizer::is_quote_char;
use crate::tokens::tk;

const FUNC: u8 = expr_flag::HAS_FUNC;
const FALSE: u8 = expr_flag::IS_FALSE;
const UPLUS: u8 = expr_flag::UPLUS;

// ─────────────────────────── semantic stack values ───────────────────────────

#[derive(Debug)]
pub(crate) struct CreateTableHead {
    pub temp: bool,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub start: usize,
}

#[derive(Debug)]
pub(crate) struct TableArgs {
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    pub options: Vec<TableOption>,
    pub as_select: Option<SelectStmt>,
}

#[derive(Debug)]
pub(crate) struct OneSelect {
    pub body: SelectBody,
    pub order_by: Option<Vec<OrderingTerm>>,
    pub order_by_all: Option<OrderByAll>,
    pub limit: Option<LimitClause>,
    pub chain_heights: Vec<u32>,
    pub coroutine: bool,
    pub last_row_no_affinity: bool,
    pub values: bool,
}

#[derive(Debug)]
pub(crate) struct CompoundAcc {
    pub body: SelectBody,
    pub terms: Vec<OneSelect>,
    pub ops: Vec<CompoundOperator>,
    pub chain: usize,
    pub last_values: bool,
}

#[derive(Debug)]
pub(crate) struct WindowSpecMeta {
    pub base: Option<String>,
    pub implicit_frame: bool,
}

#[derive(Debug)]
pub(crate) struct WindowDefMeta {
    pub raw_name: String,
    pub base: Option<String>,
    pub has_partition: bool,
    pub has_order_by: bool,
    pub implicit_frame: bool,
}

#[derive(Debug)]
pub(crate) struct WindowList {
    pub defs: Vec<WindowDefinition>,
    pub metas: Vec<WindowDefMeta>,
}

#[derive(Debug)]
pub(crate) struct TriggerDeclData {
    pub temp: bool,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub time: Option<TriggerTime>,
    pub event: TriggerEventKind,
    pub update_of: Option<Vec<Name>>,
    pub table_schema: Option<Name>,
    pub table: Name,
    pub for_each_row: bool,
    pub when: Option<Expr>,
}

#[derive(Debug)]
pub(crate) struct AlterAddData {
    pub schema: Option<Name>,
    pub table: Name,
    pub name: Name,
    pub type_name: Option<TypeName>,
    pub start: usize,
    pub action_start: usize,
}

#[derive(Debug)]
pub(crate) struct VtabHeadData {
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub module: Name,
    pub start: usize,
}

#[derive(Debug, Default)]
pub(crate) enum SemVal {
    #[default]
    Empty,
    Tok(u16),
    Bool(bool),
    Usize(usize),
    Str(String),
    Name(Name),
    Names(Vec<Name>),
    FullName {
        schema: Option<Name>,
        name: Name,
    },
    Expr(Expr),
    Exprs(Vec<Expr>),
    GroupByAll,
    OrderingTerms(Vec<OrderingTerm>),
    OrderByAll(OrderByAll),
    SortOrder(SortOrder),
    NullsOrder(NullsOrder),
    Limit(LimitClause),
    TypeName(TypeName),
    Conflict(ConflictAction),
    RefAction(ForeignKeyAction),
    RefArgs {
        on_delete: Option<ForeignKeyAction>,
        on_update: Option<ForeignKeyAction>,
        match_name: Option<Name>,
    },
    Deferrable(DeferrableClause),
    InitDeferred(InitiallyDeferred),
    Generated {
        expr: Expr,
        storage_token: Option<Name>,
    },
    ColConstraint(ColumnConstraint),
    ColConstraints(Vec<ColumnConstraint>),
    ColName {
        name: Name,
        type_name: Option<TypeName>,
    },
    ColDefs(Vec<ColumnDef>),
    TableConstraints(Vec<TableConstraint>),
    TableOptions(Vec<TableOption>),
    CreateTableHead(Box<CreateTableHead>),
    TableArgs(Box<TableArgs>),
    IdListItems(Vec<IdListItem>),
    TransMode(TransactionMode),
    Distinct(DistinctKind),
    CompoundOp(CompoundOperator),
    CompoundAcc(Box<CompoundAcc>),
    Select(Box<SelectStmt>),
    ResultCols(Vec<ResultColumn>),
    StarMods {
        exclude: Option<Vec<Name>>,
        replace: Option<Vec<StarReplacement>>,
    },
    StarReplacements(Vec<StarReplacement>),
    FromSource(Box<FromSource>),
    StlPrefix {
        left: Box<FromSource>,
        op: JoinOperator,
    },
    FromTable(Box<FromTable>),
    JoinOp(JoinOperator),
    OnUsing {
        on: Option<Expr>,
        using: Option<Vec<Name>>,
    },
    IndexedBy {
        indexed_by: Option<Name>,
        not_indexed: bool,
    },
    WhereReturning {
        where_clause: Option<Expr>,
        returning: Option<Vec<ResultColumn>>,
    },
    SetClauses(Vec<SetClause>),
    InsertCmd {
        or_conflict: Option<ConflictAction>,
        replace_into: bool,
    },
    Upsert {
        clauses: Vec<UpsertClause>,
        returning: Option<Vec<ResultColumn>>,
    },
    CaseWhens(Vec<CaseWhen>),
    LikeOp {
        op: LikeOperator,
        not: bool,
        ilike: bool,
    },
    WithClause(WithClause),
    Cte(Box<CommonTableExpr>),
    Ctes(Vec<CommonTableExpr>),
    WindowList(WindowList),
    WindowDefs(Vec<WindowDefinition>),
    WindowSpec(Box<WindowSpec>, WindowSpecMeta),
    FrameUnits(FrameUnits),
    FrameBound(FrameBound),
    FrameExclude(FrameExclude),
    FilterOver {
        filter: Option<Box<Expr>>,
        over: Option<WindowRef>,
    },
    WindowRef(WindowRef),
    TriggerDecl(Box<TriggerDeclData>),
    TriggerTime(TriggerTime),
    TriggerEvent {
        event: TriggerEventKind,
        update_of: Option<Vec<Name>>,
    },
    TriggerSteps(Vec<TriggerStep>),
    TriggerStep(TriggerStep),
    AlterAdd(Box<AlterAddData>),
    VtabHead(Box<VtabHeadData>),
    VtabArgs(Vec<VtabArg>),
    Stmt(Box<Statement>),
}

// ─────────────────────────── helpers ───────────────────────────

#[inline]
fn is_space(c: u8) -> bool {
    c == b' ' || (0x09..=0x0d).contains(&c)
}

fn trimmed(sql: &str, mut a: usize, mut b: usize) -> (usize, usize) {
    let bytes = sql.as_bytes();
    while a < b && is_space(bytes[a]) {
        a += 1;
    }
    while b > a && is_space(bytes[b - 1]) {
        b -= 1;
    }
    (a, b)
}

#[inline]
fn text<'a>(p: &Parser<'a>, k: usize) -> &'a str {
    let s = p.st[k];
    let e = p.en[k];
    if s < 0 || e < s {
        ""
    } else {
        &p.sql[s as usize..e as usize]
    }
}

#[inline]
fn tok_name(p: &Parser<'_>, k: usize) -> Name {
    let raw = text(p, k);
    Name {
        span: Span::new(p.st[k].max(0) as usize, p.en[k].max(0) as usize),
        name: dequote(raw),
        quote: QuoteStyle::from_raw(raw),
    }
}

#[inline]
fn raw_name<'a>(p: &Parser<'a>, n: &Name) -> &'a str {
    &p.sql[n.span.start..n.span.end]
}

fn span_slots(p: &Parser<'_>, from: usize, to: usize) -> Span {
    let mut s = -1isize;
    let mut e = -1isize;
    for k in from..=to {
        if p.st[k] >= 0 {
            if s < 0 {
                s = p.st[k];
            }
            e = p.en[k];
        }
    }
    if s < 0 {
        Span::new(0, 0)
    } else {
        Span::new(s as usize, e as usize)
    }
}

#[inline]
fn nm_dbnm(nm: Name, dbnm: Option<Name>) -> (Option<Name>, Name) {
    match dbnm {
        None => (None, nm),
        Some(n) => (Some(nm), n),
    }
}

#[inline]
fn hex_value(c: u8) -> u8 {
    if c <= b'9' {
        c - b'0'
    } else {
        (c | 0x20) - b'a' + 10
    }
}

/// Output of [`dequote_number`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DequotedNumber {
    pub text: String,
    pub is_float: bool,
    pub is_hex: bool,
    pub error: Option<String>,
}

/// `sqlite3DequoteNumber` (`util.c:339`): remove `_` separators from a `QNUMBER`.
pub fn dequote_number(token: &str) -> DequotedNumber {
    let mut buf = token.as_bytes().to_vec();
    buf.push(0);
    let is_hex = buf.len() >= 2 && buf[0] == b'0' && (buf[1] == b'x' || buf[1] == b'X');
    let mut is_float = false;
    let mut error: Option<String> = None;
    let mut out = 0usize;
    let mut i = 0usize;
    loop {
        let c = buf[i];
        if c != b'_' {
            buf[out] = c;
            out += 1;
            if c == b'e' || c == b'E' || c == b'.' {
                is_float = true;
            }
        } else {
            let prev = if i > 0 { buf[i - 1] } else { 0 };
            let nxt = buf[i + 1];
            let ok = if is_hex {
                prev.is_ascii_hexdigit() && nxt.is_ascii_hexdigit()
            } else {
                prev.is_ascii_digit() && nxt.is_ascii_digit()
            };
            if !ok {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                error = Some(String::from_utf8_lossy(&buf[..end]).into_owned());
            }
        }
        if c == 0 {
            break;
        }
        i += 1;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    DequotedNumber {
        text: String::from_utf8_lossy(&buf[..len]).into_owned(),
        is_float: if is_hex { false } else { is_float },
        is_hex,
        error,
    }
}

fn is_zero_integer(z: &str) -> bool {
    let b = z.as_bytes();
    let start = if b.len() > 2 && b[0] == b'0' && (b[1] | 0x20) == b'x' {
        2
    } else {
        0
    };
    b[start..].iter().all(|&c| c == b'0')
}

fn set_pexpr(p: &mut Parser<'_>, b: usize, h: u32, flags: u8) {
    p.h[b] = h;
    p.f[b] = flags & FUNC;
    let rs = p.rs;
    let re = p.re;
    p.check_height(h, rs, re);
}

fn is_constant(e: &Expr) -> bool {
    match e {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::Variable(_)
        | Expr::CurrentTime { .. } => true,
        Expr::Column(c) => {
            c.table.is_none()
                && c.column.quote == QuoteStyle::None
                && (same_name(&c.column.name, "true") || same_name(&c.column.name, "false"))
        }
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. }
        | Expr::Raise { .. }
        | Expr::Function(_)
        | Expr::Like { .. } => false,
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => is_constant(operand),
        Expr::Binary { left, right, .. } => is_constant(left) && is_constant(right),
        Expr::Between {
            operand, low, high, ..
        } => is_constant(operand) && is_constant(low) && is_constant(high),
        Expr::InList { operand, items, .. } => is_constant(operand) && items.iter().all(is_constant),
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand.as_deref().is_none_or(is_constant)
                && whens
                    .iter()
                    .all(|w| is_constant(&w.when) && is_constant(&w.then))
                && else_expr.as_deref().is_none_or(is_constant)
        }
        Expr::Vector { items, .. } => items.iter().all(is_constant),
    }
}

fn has_affinity(mut e: &Expr) -> bool {
    while let Expr::Collate { operand, .. } = e {
        e = operand;
    }
    matches!(e, Expr::Cast { .. })
}

// ─────────────────────────── stack getters ───────────────────────────

fn take(p: &mut Parser<'_>, k: usize) -> SemVal {
    std::mem::take(&mut p.v[k])
}

fn take_tok(p: &mut Parser<'_>, k: usize) -> u16 {
    match take(p, k) {
        SemVal::Tok(t) => t,
        _ => 0,
    }
}

fn take_bool(p: &mut Parser<'_>, k: usize) -> bool {
    match take(p, k) {
        SemVal::Bool(b) => b,
        _ => false,
    }
}

fn take_opt_bool(p: &mut Parser<'_>, k: usize) -> Option<bool> {
    match take(p, k) {
        SemVal::Bool(b) => Some(b),
        _ => None,
    }
}

fn take_usize(p: &mut Parser<'_>, k: usize) -> usize {
    match take(p, k) {
        SemVal::Usize(n) => n,
        _ => 0,
    }
}

fn take_str(p: &mut Parser<'_>, k: usize) -> String {
    match take(p, k) {
        SemVal::Str(s) => s,
        _ => String::new(),
    }
}

fn take_name(p: &mut Parser<'_>, k: usize) -> Name {
    match take(p, k) {
        SemVal::Name(n) => n,
        _ => Name {
            span: Span::default(),
            name: String::new(),
            quote: QuoteStyle::None,
        },
    }
}

fn take_opt_name(p: &mut Parser<'_>, k: usize) -> Option<Name> {
    match take(p, k) {
        SemVal::Name(n) => Some(n),
        _ => None,
    }
}

fn take_names(p: &mut Parser<'_>, k: usize) -> Vec<Name> {
    match take(p, k) {
        SemVal::Names(v) => v,
        _ => Vec::new(),
    }
}

fn take_opt_names(p: &mut Parser<'_>, k: usize) -> Option<Vec<Name>> {
    match take(p, k) {
        SemVal::Names(v) => Some(v),
        _ => None,
    }
}

fn take_fullname(p: &mut Parser<'_>, k: usize) -> (Option<Name>, Name) {
    match take(p, k) {
        SemVal::FullName { schema, name } => (schema, name),
        _ => (
            None,
            Name {
                span: Span::default(),
                name: String::new(),
                quote: QuoteStyle::None,
            },
        ),
    }
}

fn take_expr(p: &mut Parser<'_>, k: usize) -> Expr {
    match take(p, k) {
        SemVal::Expr(e) => e,
        _ => Expr::Null {
            span: Span::default(),
        },
    }
}

fn take_opt_expr(p: &mut Parser<'_>, k: usize) -> Option<Expr> {
    match take(p, k) {
        SemVal::Expr(e) => Some(e),
        _ => None,
    }
}

fn take_exprs(p: &mut Parser<'_>, k: usize) -> Vec<Expr> {
    match take(p, k) {
        SemVal::Exprs(v) => v,
        _ => Vec::new(),
    }
}

fn take_opt_exprs(p: &mut Parser<'_>, k: usize) -> Option<Vec<Expr>> {
    match take(p, k) {
        SemVal::Exprs(v) => Some(v),
        _ => None,
    }
}

fn take_ordering_terms(p: &mut Parser<'_>, k: usize) -> Vec<OrderingTerm> {
    match take(p, k) {
        SemVal::OrderingTerms(v) => v,
        _ => Vec::new(),
    }
}

fn take_opt_sort_order(p: &mut Parser<'_>, k: usize) -> Option<SortOrder> {
    match take(p, k) {
        SemVal::SortOrder(s) => Some(s),
        _ => None,
    }
}

fn take_opt_nulls(p: &mut Parser<'_>, k: usize) -> Option<NullsOrder> {
    match take(p, k) {
        SemVal::NullsOrder(n) => Some(n),
        _ => None,
    }
}

fn take_opt_limit(p: &mut Parser<'_>, k: usize) -> Option<LimitClause> {
    match take(p, k) {
        SemVal::Limit(l) => Some(l),
        _ => None,
    }
}

fn take_opt_type_name(p: &mut Parser<'_>, k: usize) -> Option<TypeName> {
    match take(p, k) {
        SemVal::TypeName(t) => Some(t),
        _ => None,
    }
}

fn take_opt_conflict(p: &mut Parser<'_>, k: usize) -> Option<ConflictAction> {
    match take(p, k) {
        SemVal::Conflict(c) => Some(c),
        _ => None,
    }
}

fn take_conflict(p: &mut Parser<'_>, k: usize) -> ConflictAction {
    match take(p, k) {
        SemVal::Conflict(c) => c,
        _ => ConflictAction::Abort,
    }
}

fn take_ref_action(p: &mut Parser<'_>, k: usize) -> ForeignKeyAction {
    match take(p, k) {
        SemVal::RefAction(a) => a,
        _ => ForeignKeyAction::NoAction,
    }
}

fn take_opt_init_deferred(p: &mut Parser<'_>, k: usize) -> Option<InitiallyDeferred> {
    match take(p, k) {
        SemVal::InitDeferred(d) => Some(d),
        _ => None,
    }
}

fn take_opt_deferrable(p: &mut Parser<'_>, k: usize) -> Option<DeferrableClause> {
    match take(p, k) {
        SemVal::Deferrable(d) => Some(d),
        _ => None,
    }
}

fn take_opt_id_list_items(p: &mut Parser<'_>, k: usize) -> Option<Vec<IdListItem>> {
    match take(p, k) {
        SemVal::IdListItems(v) => Some(v),
        _ => None,
    }
}

fn take_id_list_items(p: &mut Parser<'_>, k: usize) -> Vec<IdListItem> {
    match take(p, k) {
        SemVal::IdListItems(v) => v,
        _ => Vec::new(),
    }
}

fn take_select(p: &mut Parser<'_>, k: usize) -> SelectStmt {
    match take(p, k) {
        SemVal::Select(s) => *s,
        _ => SelectStmt {
            span: Span::default(),
            with: None,
            body: SelectBody::Values(ValuesClause {
                span: Span::default(),
                rows: Vec::new(),
            }),
            order_by: None,
            order_by_all: None,
            limit: None,
        },
    }
}

fn take_compound_acc(p: &mut Parser<'_>, k: usize) -> Box<CompoundAcc> {
    match take(p, k) {
        SemVal::CompoundAcc(acc) => acc,
        _ => Box::new(CompoundAcc {
            body: SelectBody::Values(ValuesClause {
                span: Span::default(),
                rows: Vec::new(),
            }),
            terms: Vec::new(),
            ops: Vec::new(),
            chain: 0,
            last_values: false,
        }),
    }
}

fn take_result_cols(p: &mut Parser<'_>, k: usize) -> Vec<ResultColumn> {
    match take(p, k) {
        SemVal::ResultCols(v) => v,
        _ => Vec::new(),
    }
}

fn take_opt_result_cols(p: &mut Parser<'_>, k: usize) -> Option<Vec<ResultColumn>> {
    match take(p, k) {
        SemVal::ResultCols(v) => Some(v),
        _ => None,
    }
}

fn take_from_source(p: &mut Parser<'_>, k: usize) -> FromSource {
    match take(p, k) {
        SemVal::FromSource(f) => *f,
        _ => FromSource::Table(FromTable {
            span: Span::default(),
            schema: None,
            name: Name {
                span: Span::default(),
                name: String::new(),
                quote: QuoteStyle::None,
            },
            alias: None,
            indexed_by: None,
            not_indexed: false,
        }),
    }
}

fn take_opt_from_source(p: &mut Parser<'_>, k: usize) -> Option<FromSource> {
    match take(p, k) {
        SemVal::FromSource(f) => Some(*f),
        _ => None,
    }
}

fn take_from_table(p: &mut Parser<'_>, k: usize) -> FromTable {
    match take(p, k) {
        SemVal::FromTable(t) => *t,
        _ => FromTable {
            span: Span::default(),
            schema: None,
            name: Name {
                span: Span::default(),
                name: String::new(),
                quote: QuoteStyle::None,
            },
            alias: None,
            indexed_by: None,
            not_indexed: false,
        },
    }
}

fn take_opt_with(p: &mut Parser<'_>, k: usize) -> Option<WithClause> {
    match take(p, k) {
        SemVal::WithClause(w) => Some(w),
        _ => None,
    }
}

fn take_set_clauses(p: &mut Parser<'_>, k: usize) -> Vec<SetClause> {
    match take(p, k) {
        SemVal::SetClauses(v) => v,
        _ => Vec::new(),
    }
}

fn take_window_spec(p: &mut Parser<'_>, k: usize) -> (WindowSpec, WindowSpecMeta) {
    match take(p, k) {
        SemVal::WindowSpec(w, m) => (*w, m),
        _ => (
            WindowSpec {
                span: Span::default(),
                base: None,
                partition_by: None,
                order_by: None,
                frame: None,
            },
            WindowSpecMeta {
                base: None,
                implicit_frame: true,
            },
        ),
    }
}

fn take_window_ref(p: &mut Parser<'_>, k: usize) -> WindowRef {
    match take(p, k) {
        SemVal::WindowRef(r) => r,
        SemVal::Name(n) => WindowRef::Name(n),
        SemVal::WindowSpec(w, _) => WindowRef::Spec(w),
        _ => WindowRef::Name(Name {
            span: Span::default(),
            name: String::new(),
            quote: QuoteStyle::None,
        }),
    }
}

fn take_frame_of(p: &mut Parser<'_>, k: usize) -> Option<FrameSpec> {
    take_window_spec(p, k).0.frame
}

fn take_window_order_by(p: &mut Parser<'_>, k: usize) -> Option<Vec<OrderingTerm>> {
    match take(p, k) {
        SemVal::Empty => None,
        SemVal::OrderingTerms(v) => Some(v),
        SemVal::OrderByAll(ob) => {
            p.error(
                "near \"ALL\": syntax error",
                ob.span.start as isize,
                ob.span.end as isize,
                SqlParseErrorKind::Syntax,
            );
            None
        }
        _ => None,
    }
}

fn take_trigger_step(p: &mut Parser<'_>, k: usize) -> TriggerStep {
    match take(p, k) {
        SemVal::TriggerStep(s) => s,
        SemVal::Select(s) => TriggerStep::Select(s),
        _ => TriggerStep::Select(Box::new(SelectStmt {
            span: Span::default(),
            with: None,
            body: SelectBody::Values(ValuesClause {
                span: Span::default(),
                rows: Vec::new(),
            }),
            order_by: None,
            order_by_all: None,
            limit: None,
        })),
    }
}

fn take_stmt(p: &mut Parser<'_>, k: usize) -> Statement {
    match take(p, k) {
        SemVal::Stmt(s) => *s,
        SemVal::Select(s) => Statement::Select(s),
        _ => Statement::Commit {
            span: Span::default(),
        },
    }
}

fn take_pragma_text(p: &mut Parser<'_>, k: usize) -> String {
    match take(p, k) {
        SemVal::Str(s) => s,
        SemVal::Name(n) => n.name,
        _ => String::new(),
    }
}

// ─────────────────────────── domain helpers ───────────────────────────

fn select_height(one: &OneSelect) -> u32 {
    one.chain_heights.iter().copied().max().unwrap_or(0)
}

fn double_link(p: &mut Parser<'_>, acc: &CompoundAcc) {
    let n = acc.terms.len();
    if n >= 2 {
        for k in (0..=(n - 2)).rev() {
            let t = &acc.terms[k];
            if t.order_by.is_some() || t.order_by_all.is_some() || t.limit.is_some() {
                let clause = if t.order_by.is_some() || t.order_by_all.is_some() {
                    "ORDER BY"
                } else {
                    "LIMIT"
                };
                let rs = p.rs;
                let re = p.re;
                p.error(
                    format!(
                        "{clause} clause should come after {} not before",
                        acc.ops[k].as_str()
                    ),
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
                break;
            }
        }
    }
    if !acc.last_values && acc.chain > 1 && acc.chain > p.max_compound {
        let rs = p.rs;
        let re = p.re;
        p.error(
            "too many terms in compound SELECT",
            rs,
            re,
            SqlParseErrorKind::Syntax,
        );
    }
}

fn finish_select(
    p: &mut Parser<'_>,
    b: usize,
    acc_slot: usize,
    with_clause: Option<WithClause>,
) -> SemVal {
    let mut acc = take_compound_acc(p, acc_slot);
    double_link(p, &acc);
    let last = acc.terms.pop();
    p.h[b] = p.h[acc_slot];
    p.f[b] = 0;
    let (order_by, order_by_all, limit) = match last {
        Some(l) => (l.order_by, l.order_by_all, l.limit),
        None => (None, None, None),
    };
    SemVal::Select(Box::new(SelectStmt {
        span: p.rule_span(),
        with: with_clause,
        body: acc.body,
        order_by,
        order_by_all,
        limit,
    }))
}

fn single_acc(p: &mut Parser<'_>, b: usize, one: OneSelect) -> SemVal {
    p.h[b] = select_height(&one);
    p.f[b] = 0;
    let body = one.body.clone();
    let chain = one.chain_heights.len();
    let last_values = one.values;
    SemVal::CompoundAcc(Box::new(CompoundAcc {
        body,
        terms: vec![one],
        ops: Vec::new(),
        chain,
        last_values,
    }))
}

struct CoreSlots {
    distinct: Option<usize>,
    columns: Option<usize>,
    from: usize,
    where_slot: usize,
    group_by: Option<usize>,
    having: usize,
    window: Option<usize>,
    order_by: usize,
    limit: usize,
}

fn one_select(
    p: &mut Parser<'_>,
    b: usize,
    s: CoreSlots,
    from_first: bool,
    implicit_star: bool,
) -> SemVal {
    let (group_by, group_by_all) = match s.group_by {
        None => (None, false),
        Some(k) => match take(p, k) {
            SemVal::GroupByAll => (None, true),
            SemVal::Exprs(v) => (Some(v), false),
            _ => (None, false),
        },
    };
    let (order_by, order_by_all) = match take(p, s.order_by) {
        SemVal::OrderingTerms(v) => (Some(v), None),
        SemVal::OrderByAll(ob) => (None, Some(ob)),
        _ => (None, None),
    };
    let last_core = s.window.unwrap_or(s.having);
    let core_span = span_slots(p, b, last_core);
    let columns = if implicit_star {
        vec![ResultColumn::Star(ResultStar {
            span: Span::new(core_span.start, core_span.start),
            table: None,
            exclude: None,
            replace: None,
        })]
    } else {
        take_result_cols(p, s.columns.unwrap())
    };
    let distinct = s.distinct.and_then(|k| match take(p, k) {
        SemVal::Distinct(d) => Some(d),
        _ => None,
    });
    let from = take_opt_from_source(p, s.from);
    let where_clause = take_opt_expr(p, s.where_slot);
    let having = take_opt_expr(p, s.having);
    let windows = s.window.and_then(|k| match take(p, k) {
        SemVal::WindowDefs(w) => Some(w),
        _ => None,
    });
    let limit = take_opt_limit(p, s.limit);

    let mut h = if implicit_star {
        1
    } else {
        p.h[s.columns.unwrap()]
    };
    for k in [
        Some(s.where_slot),
        s.group_by,
        Some(s.having),
        Some(s.order_by),
        Some(s.limit),
    ]
    .into_iter()
    .flatten()
    {
        if p.h[k] > h {
            h = p.h[k];
        }
    }

    let core = SelectCore {
        span: core_span,
        distinct,
        columns,
        from,
        where_clause,
        group_by,
        group_by_all,
        having,
        windows,
        from_first,
    };
    single_acc(
        p,
        b,
        OneSelect {
            body: SelectBody::Core(Box::new(core)),
            order_by,
            order_by_all,
            limit,
            chain_heights: vec![h],
            coroutine: false,
            last_row_no_affinity: false,
            values: false,
        },
    )
}

fn add_values_row(
    p: &mut Parser<'_>,
    b: usize,
    mut acc: Box<CompoundAcc>,
    row_slot: usize,
    lp: usize,
    rp: usize,
) -> SemVal {
    let items = take_exprs(p, row_slot);
    let row_h = p.h[row_slot];
    let constant = items.iter().all(is_constant);
    let row_no_aff = constant && !items.iter().any(has_affinity);
    let row = ValuesRow {
        span: Span::new(p.st[lp].max(0) as usize, p.en[rp].max(0) as usize),
        items,
    };
    let row_end = row.span.end;
    let one = &mut acc.terms[0];
    if let SelectBody::Values(ref mut values) = one.body {
        values.rows.push(row);
        values.span.end = row_end;
    }
    let union_all = p.has_with
        || p.schema_mode
        || !constant
        || (!one.coroutine && !one.last_row_no_affinity);
    if union_all {
        one.chain_heights.push(row_h);
        one.coroutine = false;
        one.last_row_no_affinity = row_no_aff;
    } else if !one.coroutine {
        if let Some(last) = one.chain_heights.last_mut() {
            *last = 1;
        }
        one.coroutine = true;
    }
    one.values = !one.coroutine || one.chain_heights.len() > 1;
    acc.chain = one.chain_heights.len();
    acc.last_values = one.values;
    acc.body = one.body.clone();
    p.h[b] = select_height(one);
    SemVal::CompoundAcc(acc)
}

fn append_from_term(
    p: &mut Parser<'_>,
    prefix_slot: usize,
    term: FromSource,
    on_using_slot: usize,
) -> SemVal {
    let prefix = match take(p, prefix_slot) {
        SemVal::StlPrefix { left, op } => Some((left, op)),
        _ => None,
    };
    let (on, using) = match take(p, on_using_slot) {
        SemVal::OnUsing { on, using } => (on, using),
        _ => (None, None),
    };
    match prefix {
        None => {
            if on.is_some() || using.is_some() {
                let kw = if on.is_some() { "ON" } else { "USING" };
                let rs = p.rs;
                let re = p.re;
                p.error(
                    format!("a JOIN clause is required before {kw}"),
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            }
            SemVal::FromSource(Box::new(term))
        }
        Some((left, operator)) => {
            let start = left.span().start;
            let end = p.re.max(0) as usize;
            SemVal::FromSource(Box::new(FromSource::Join {
                span: Span::new(start, end),
                left,
                operator,
                right: Box::new(term),
                on: on.map(Box::new),
                using,
            }))
        }
    }
}

fn join_type(p: &mut Parser<'_>, words: &[&str]) -> u8 {
    const JT_ERROR: u8 = 0x80;
    let mut jt = 0u8;
    for &w in words {
        let bits = if same_name(w, "natural") {
            join_flag::NATURAL
        } else if same_name(w, "left") {
            join_flag::LEFT | join_flag::OUTER
        } else if same_name(w, "outer") {
            join_flag::OUTER
        } else if same_name(w, "right") {
            join_flag::RIGHT | join_flag::OUTER
        } else if same_name(w, "full") {
            join_flag::LEFT | join_flag::RIGHT | join_flag::OUTER
        } else if same_name(w, "inner") {
            join_flag::INNER
        } else if same_name(w, "cross") {
            join_flag::INNER | join_flag::CROSS
        } else {
            JT_ERROR
        };
        if bits == JT_ERROR {
            jt |= JT_ERROR;
            break;
        }
        jt |= bits;
    }
    let bad = (jt & (join_flag::INNER | join_flag::OUTER))
        == (join_flag::INNER | join_flag::OUTER)
        || (jt & JT_ERROR) != 0
        || (jt & (join_flag::OUTER | join_flag::LEFT | join_flag::RIGHT)) == join_flag::OUTER;
    if bad {
        let rs = p.rs;
        let re = p.re;
        p.error(
            format!("unknown join type: {}", words.join(" ")),
            rs,
            re,
            SqlParseErrorKind::Syntax,
        );
        return join_flag::INNER;
    }
    jt
}

fn vector_size(e: &Expr) -> Option<usize> {
    match e {
        Expr::Subquery { .. } => None,
        Expr::Vector { items, .. } => Some(items.len()),
        _ => Some(1),
    }
}

fn id_list_item(
    p: &mut Parser<'_>,
    nm_slot: usize,
    collate: Option<Name>,
    order: Option<SortOrder>,
) -> IdListItem {
    let name = take_name(p, nm_slot);
    if (collate.is_some() || order.is_some()) && !p.schema_mode {
        let raw = raw_name(p, &name).to_string();
        let s = name.span.start as isize;
        let e = name.span.end as isize;
        p.error(
            format!("syntax error after column name \"{raw}\""),
            s,
            e,
            SqlParseErrorKind::Syntax,
        );
    }
    let span = span_slots(p, nm_slot, nm_slot + 2);
    IdListItem {
        span,
        name,
        collate,
        order,
    }
}

fn build_column_def(p: &mut Parser<'_>, name_slot: usize, carg_slot: usize) -> ColumnDef {
    let (name, type_name) = match take(p, name_slot) {
        SemVal::ColName { name, type_name } => (name, type_name),
        _ => (
            Name {
                span: Span::default(),
                name: String::new(),
                quote: QuoteStyle::None,
            },
            None,
        ),
    };
    let constraints = match take(p, carg_slot) {
        SemVal::ColConstraints(c) => c,
        _ => Vec::new(),
    };
    let span = span_slots(p, name_slot, carg_slot);
    let declared_type = stored_type(type_name.as_ref().map(|t| t.text.as_str())).pragma_type;
    ColumnDef {
        span,
        name,
        type_name,
        declared_type,
        constraints,
    }
}

fn check_text(p: &Parser<'_>, lp_slot: usize, rp_slot: usize) -> String {
    let (a, z) = trimmed(
        p.sql,
        p.en[lp_slot].max(0) as usize,
        p.st[rp_slot].max(0) as usize,
    );
    p.sql[a..z].to_string()
}

fn default_constraint(
    p: &Parser<'_>,
    form: DefaultForm,
    expr: Expr,
    text_start: usize,
    text_end: usize,
) -> SemVal {
    let (a, z) = trimmed(p.sql, text_start, text_end);
    SemVal::ColConstraint(ColumnConstraint::Default {
        span: p.rule_span(),
        name: p.constraint_name.clone(),
        form,
        expr,
        text: p.sql[a..z].to_string(),
        text_span: Span::new(a, z),
    })
}

fn build_generated_constraint(
    p: &Parser<'_>,
    expr: Expr,
    storage_token: Option<Name>,
    always: bool,
) -> SemVal {
    let storage = storage_token.as_ref().and_then(|tok| {
        let raw = raw_name(p, tok);
        if raw.len() == 7 && same_name(raw, "virtual") {
            Some(GeneratedStorage::Virtual)
        } else if raw.len() == 6 && same_name(raw, "stored") {
            Some(GeneratedStorage::Stored)
        } else {
            None
        }
    });
    SemVal::ColConstraint(ColumnConstraint::Generated {
        span: p.rule_span(),
        name: p.constraint_name.clone(),
        expr,
        storage_token,
        storage,
        always,
    })
}

fn variable_number(p: &mut Parser<'_>, z: &str, s: usize, e: usize) -> usize {
    let zb = z.as_bytes();
    let x = if zb.len() == 1 {
        p.n_var += 1;
        p.n_var
    } else if zb[0] == b'?' {
        let mut v = 0usize;
        let mut ok = true;
        for &c in &zb[1..] {
            v = v.saturating_mul(10).saturating_add(usize::from(c - b'0'));
            if v > p.max_var {
                ok = false;
                break;
            }
        }
        if !ok || v < 1 {
            let max_var = p.max_var;
            p.error(
                format!("variable number must be between ?1 and ?{max_var}"),
                s as isize,
                e as isize,
                SqlParseErrorKind::Limit,
            );
            return 0;
        }
        if v > p.n_var {
            p.n_var = v;
        }
        v
    } else {
        let names = p.var_names.get_or_insert_with(HashMap::new);
        if let Some(&prior) = names.get(z) {
            prior
        } else {
            p.n_var += 1;
            let idx = p.n_var;
            names.insert(z.to_string(), idx);
            idx
        }
    };
    if x > p.max_var {
        p.error(
            "too many SQL variables",
            s as isize,
            e as isize,
            SqlParseErrorKind::Limit,
        );
    }
    x
}

fn function_call(
    p: &mut Parser<'_>,
    b: usize,
    quantifier: Option<DistinctKind>,
    args_slot: Option<usize>,
    star: bool,
    order_by_slot: Option<usize>,
    filter_over_slot: Option<usize>,
) -> SemVal {
    let name = tok_name(p, b);
    let args = args_slot.map(|k| take_exprs(p, k)).unwrap_or_default();
    let order_by = order_by_slot.map(|k| take_ordering_terms(p, k));
    let (filter, over) = match filter_over_slot {
        Some(k) => match take(p, k) {
            SemVal::FilterOver { filter, over } => (filter, over),
            _ => (None, None),
        },
        None => (None, None),
    };
    if args.len() > p.max_args {
        let raw = text(p, b).to_string();
        let st = p.st[b];
        let en = p.en[b];
        p.error(
            format!("too many arguments on function {raw}"),
            st,
            en,
            SqlParseErrorKind::Limit,
        );
    }
    let ha = args_slot.map(|k| p.h[k]).unwrap_or(0);
    let fa = args_slot.map(|k| p.f[k]).unwrap_or(0);
    let rs = p.rs;
    let re = p.re;
    if p.n_err == 0 {
        p.check_height(ha + 1, rs, re);
    }
    if over.is_some() && quantifier == Some(DistinctKind::Distinct) {
        p.error(
            "DISTINCT is not supported for window functions",
            rs,
            re,
            SqlParseErrorKind::Syntax,
        );
    }
    if let Some(ref ob) = order_by {
        if !args.is_empty() {
            if over.is_some() {
                p.error(
                    format!("ORDER BY may not be used with non-aggregate {}()", name.name),
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            } else if ob.len() > p.max_columns {
                p.error(
                    "too many terms in ORDER BY clause",
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            }
        }
    }
    p.h[b] = ha + 1;
    p.f[b] = FUNC | (fa & FUNC);
    SemVal::Expr(Expr::Function(Box::new(FunctionExpr {
        span: p.rule_span(),
        name,
        quantifier,
        star,
        args,
        order_by,
        filter,
        over,
    })))
}

fn is_null_node(p: &mut Parser<'_>, b: usize, operand: Expr, not: bool) -> SemVal {
    let mut x = &operand;
    while let Expr::Unary {
        op: UnaryOp::Pos | UnaryOp::Neg,
        operand: inner,
        ..
    } = x
    {
        x = inner;
    }
    if matches!(x, Expr::Number(_) | Expr::String(_) | Expr::Blob(_)) {
        p.h[b] = 1;
        p.f[b] = if not { 0 } else { FALSE };
    } else {
        let h = p.h[b] + 1;
        let f = p.f[b];
        set_pexpr(p, b, h, f);
    }
    SemVal::Expr(Expr::IsNull {
        span: p.rule_span(),
        not,
        operand: Box::new(operand),
    })
}

fn is_node(p: &mut Parser<'_>, b: usize, right_slot: usize, op: BinaryOp) -> SemVal {
    let left = take_expr(p, b);
    let right = take_expr(p, right_slot);
    if matches!(right, Expr::Null { .. }) {
        return is_null_node(p, b, left, op == BinaryOp::IsNot);
    }
    let h = p.h[b].max(p.h[right_slot]) + 1;
    let f = p.f[b] | p.f[right_slot];
    set_pexpr(p, b, h, f);
    SemVal::Expr(Expr::Binary {
        span: p.rule_span(),
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn binary_expr(p: &mut Parser<'_>, b: usize, op: BinaryOp) -> SemVal {
    let left = take_expr(p, b);
    let right = take_expr(p, b + 2);
    let hl = p.h[b];
    let hr = p.h[b + 2];
    let fl = p.f[b];
    let fr = p.f[b + 2];
    if op == BinaryOp::And && ((fl | fr) & (FALSE | FUNC)) == FALSE {
        p.h[b] = 1;
        p.f[b] = FALSE;
    } else {
        set_pexpr(p, b, hl.max(hr) + 1, fl | fr);
    }
    SemVal::Expr(Expr::Binary {
        span: p.rule_span(),
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn in_list_action(p: &mut Parser<'_>, b: usize) -> SemVal {
    let operand = take_expr(p, b);
    let not = take_bool(p, b + 1);
    let mut items = take_exprs(p, b + 3);
    let ha = p.h[b];
    let fa = p.f[b];
    let hy = p.h[b + 3];
    let fy = p.f[b + 3];
    let rs = p.rs;
    let re = p.re;
    let span = p.rule_span();
    if items.is_empty() {
        if (fa & FUNC) == 0 {
            p.h[b] = 1;
            p.f[b] = if not { 0 } else { FALSE };
        } else {
            set_pexpr(p, b, ha.max(1) + 1, fa);
        }
        return SemVal::Expr(Expr::InList {
            span,
            not,
            operand: Box::new(operand),
            items,
        });
    }
    let mut h: u32;
    let f: u8;
    let result: Expr;
    if items.len() == 1
        && is_constant(&items[0])
        && !matches!(items[0], Expr::Collate { .. })
        && !matches!(operand, Expr::Vector { .. })
    {
        p.check_height(hy + 1, rs, re);
        h = ha.max(hy + 1) + 1;
        p.check_height(h, rs, re);
        f = (fa | fy) & FUNC;
        result = Expr::InList {
            span,
            not,
            operand: Box::new(operand),
            items,
        };
    } else if items.len() == 1 && matches!(items[0], Expr::Subquery { .. }) {
        p.check_height(ha + 1, rs, re);
        h = ha.max(hy.saturating_sub(1)) + 1;
        if p.n_err == 0 {
            p.check_height(h, rs, re);
        }
        f = fa & FUNC;
        let select = match items.pop().unwrap() {
            Expr::Subquery { select, .. } => select,
            _ => unreachable!(),
        };
        result = Expr::InSelect {
            span,
            not,
            operand: Box::new(operand),
            select,
        };
    } else {
        p.check_height(ha + 1, rs, re);
        if let Expr::Vector {
            items: ref op_items,
            ..
        } = operand
        {
            let n = op_items.len();
            let mut hv = 0u32;
            for el in &items {
                let (k, ih) = match el {
                    Expr::Vector {
                        items: v_items,
                        item_height,
                        ..
                    } => (v_items.len(), *item_height),
                    _ => (1, 1),
                };
                if k != n {
                    let s = el.span();
                    p.error(
                        format!(
                            "IN(...) element has {k} term{} - expected {n}",
                            if k > 1 { "s" } else { "" }
                        ),
                        s.start as isize,
                        s.end as isize,
                        SqlParseErrorKind::Syntax,
                    );
                    break;
                }
                if ih > hv {
                    hv = ih;
                }
            }
            h = ha.max(hv) + 1;
            f = fa & FUNC;
        } else {
            h = ha.max(hy) + 1;
            f = (fa | fy) & FUNC;
        }
        if p.n_err == 0 {
            p.check_height(h, rs, re);
        }
        result = Expr::InList {
            span,
            not,
            operand: Box::new(operand),
            items,
        };
    }
    if not {
        h += 1;
        p.check_height(h, rs, re);
    }
    p.h[b] = h;
    p.f[b] = f;
    SemVal::Expr(result)
}

fn build_frame_spec(
    p: &mut Parser<'_>,
    units: FrameUnits,
    from: FrameBound,
    to: Option<FrameBound>,
    exclude: Option<FrameExclude>,
) -> FrameSpec {
    let st = from.kind;
    let et = to
        .as_ref()
        .map(|b| b.kind)
        .unwrap_or(FrameBoundType::CurrentRow);
    if (st == FrameBoundType::CurrentRow && et == FrameBoundType::Preceding)
        || (st == FrameBoundType::Following
            && (et == FrameBoundType::Preceding || et == FrameBoundType::CurrentRow))
    {
        let rs = p.rs;
        let re = p.re;
        p.error(
            "unsupported frame specification",
            rs,
            re,
            SqlParseErrorKind::Syntax,
        );
    }
    FrameSpec {
        span: p.rule_span(),
        units,
        from,
        to,
        exclude,
    }
}

fn build_window_spec(
    p: &Parser<'_>,
    base: Option<Name>,
    partition_by: Option<Vec<Expr>>,
    order_by: Option<Vec<OrderingTerm>>,
    frame: Option<FrameSpec>,
) -> SemVal {
    let meta = WindowSpecMeta {
        base: base.as_ref().map(|n| raw_name(p, n).to_string()),
        implicit_frame: frame.is_none(),
    };
    SemVal::WindowSpec(
        Box::new(WindowSpec {
            span: p.rule_span(),
            base,
            partition_by,
            order_by,
            frame,
        }),
        meta,
    )
}

fn from_table(
    p: &mut Parser<'_>,
    nm_slot: usize,
    last_slot: usize,
    indexed: Option<(Option<Name>, bool)>,
) -> FromSource {
    let (schema, name) = nm_dbnm(take_name(p, nm_slot), take_opt_name(p, nm_slot + 1));
    let alias = take_opt_name(p, nm_slot + 2);
    let span = span_slots(p, nm_slot, last_slot);
    let (indexed_by, not_indexed) = indexed.unwrap_or((None, false));
    FromSource::Table(FromTable {
        span,
        schema,
        name,
        alias,
        indexed_by,
        not_indexed,
    })
}

fn xfullname_action(
    p: &mut Parser<'_>,
    schema_slot: Option<usize>,
    name_slot: usize,
    alias_slot: Option<usize>,
) -> SemVal {
    SemVal::FromTable(Box::new(FromTable {
        span: p.rule_span(),
        schema: schema_slot.map(|k| take_name(p, k)),
        name: take_name(p, name_slot),
        alias: alias_slot.map(|k| take_name(p, k)),
        indexed_by: None,
        not_indexed: false,
    }))
}

fn ordering_term(p: &mut Parser<'_>, expr_slot: usize) -> OrderingTerm {
    let expr = take_expr(p, expr_slot);
    let order = take_opt_sort_order(p, expr_slot + 1);
    let nulls = take_opt_nulls(p, expr_slot + 2);
    let end = span_slots(p, expr_slot, expr_slot + 2).end;
    let start = expr.span().start;
    OrderingTerm {
        span: Span::new(start, end),
        expr,
        order,
        nulls,
    }
}

fn set_clause_single(p: &mut Parser<'_>, nm_slot: usize, expr_slot: usize) -> SetClause {
    let span = span_slots(p, nm_slot, expr_slot);
    SetClause {
        span,
        columns: vec![take_name(p, nm_slot)],
        parenthesized: false,
        expr: take_expr(p, expr_slot),
    }
}

fn set_clause_vector(p: &mut Parser<'_>, lp_slot: usize) -> SetClause {
    let columns = take_names(p, lp_slot + 1);
    let expr = take_expr(p, lp_slot + 4);
    if let Some(n) = vector_size(&expr) {
        if n != columns.len() {
            let rs = p.rs;
            let re = p.re;
            p.error(
                format!("{} columns assigned {n} values", columns.len()),
                rs,
                re,
                SqlParseErrorKind::Syntax,
            );
        }
    }
    let span = span_slots(p, lp_slot, lp_slot + 4);
    SetClause {
        span,
        columns,
        parenthesized: true,
        expr,
    }
}

fn with_indexed(p: &mut Parser<'_>, mut t: FromTable, idx_slot: usize) -> FromTable {
    if let SemVal::IndexedBy {
        indexed_by,
        not_indexed,
    } = take(p, idx_slot)
    {
        t.indexed_by = indexed_by;
        t.not_indexed = not_indexed;
        t.span.end = p.en[idx_slot].max(0) as usize;
    }
    t
}

fn drop_cmd(p: &mut Parser<'_>, b: usize, object_type: DropObjectType) -> SemVal {
    let if_exists = take_bool(p, b + 2);
    let (schema, name) = take_fullname(p, b + 3);
    SemVal::Stmt(Box::new(Statement::Drop(DropStmt {
        span: p.rule_span(),
        object_type,
        if_exists,
        schema,
        name,
    })))
}

fn like_op_action(p: &mut Parser<'_>, k: usize, not: bool) -> SemVal {
    let tok = take_tok(p, k);
    let (op, ilike) = if tok == tk::MATCH {
        (LikeOperator::Match, false)
    } else {
        let raw = text(p, k);
        if same_name(raw, "glob") {
            (LikeOperator::Glob, false)
        } else if same_name(raw, "regexp") {
            (LikeOperator::Regexp, false)
        } else {
            (LikeOperator::Like, same_name(raw, "ilike"))
        }
    };
    SemVal::LikeOp { op, not, ilike }
}

fn like_expr_action(p: &mut Parser<'_>, b: usize, escape_slot: Option<usize>) -> SemVal {
    let (op, not, ilike) = match take(p, b + 1) {
        SemVal::LikeOp { op, not, ilike } => (op, not, ilike),
        _ => (LikeOperator::Like, false, false),
    };
    let mut h = p.h[b].max(p.h[b + 2]);
    let mut f = p.f[b] | p.f[b + 2];
    if let Some(es) = escape_slot {
        h = h.max(p.h[es]);
        f |= p.f[es];
    }
    h += 1;
    let rs = p.rs;
    let re = p.re;
    if p.n_err == 0 {
        p.check_height(h, rs, re);
    }
    if not {
        h += 1;
        p.check_height(h, rs, re);
    }
    p.h[b] = h;
    p.f[b] = FUNC | (f & FUNC);
    SemVal::Expr(Expr::Like {
        span: p.rule_span(),
        op,
        not,
        left: Box::new(take_expr(p, b)),
        right: Box::new(take_expr(p, b + 2)),
        escape: escape_slot.map(|k| Box::new(take_expr(p, k))),
        ilike,
    })
}

fn pragma_action(p: &mut Parser<'_>, b: usize, form: Option<PragmaValueForm>) -> SemVal {
    let (schema, name) = nm_dbnm(take_name(p, b + 1), take_opt_name(p, b + 2));
    let value = form.map(|f| PragmaValue {
        span: Span::new(p.st[b + 4].max(0) as usize, p.en[b + 4].max(0) as usize),
        text: take_pragma_text(p, b + 4),
        form: f,
    });
    SemVal::Stmt(Box::new(Statement::Pragma(PragmaStmt {
        span: p.rule_span(),
        schema,
        name,
        value,
    })))
}

fn alter_action(p: &mut Parser<'_>, b: usize, action: AlterTableAction) -> SemVal {
    let (schema, table) = take_fullname(p, b + 2);
    SemVal::Stmt(Box::new(Statement::AlterTable(Box::new(AlterTableStmt {
        span: p.rule_span(),
        schema,
        table,
        action,
    }))))
}

// ─────────────────────────── main dispatch ───────────────────────────

pub(crate) fn run_action(action: ActionId, p: &mut Parser<'_>, b: usize) -> SemVal {
    match action {
        ActionId::PassThrough | ActionId::Pass0 => take(p, b),
        ActionId::Nul => SemVal::Empty,
        ActionId::Yes => SemVal::Bool(true),
        ActionId::No => SemVal::Bool(false),
        ActionId::EmptyTableOptions => SemVal::TableOptions(Vec::new()),
        ActionId::EmptyColConstraints => SemVal::ColConstraints(Vec::new()),
        ActionId::EmptyTableConstraints => SemVal::TableConstraints(Vec::new()),
        ActionId::EmptyResultCols => SemVal::ResultCols(Vec::new()),
        ActionId::EmptyExprs => SemVal::Exprs(Vec::new()),
        ActionId::EmptyVtabArgs => SemVal::VtabArgs(Vec::new()),
        ActionId::Pass1 => {
            p.h[b] = p.h[b + 1];
            p.f[b] = p.f[b + 1];
            take(p, b + 1)
        }
        ActionId::Pass2 => {
            p.h[b] = p.h[b + 2];
            p.f[b] = p.f[b + 2];
            take(p, b + 2)
        }

        // ── command list ──
        ActionId::Explain => {
            p.explain = 1;
            p.explain_start = p.st[b].max(0) as usize;
            SemVal::Empty
        }
        ActionId::ExplainQueryPlan => {
            p.explain = 2;
            p.explain_start = p.st[b].max(0) as usize;
            SemVal::Empty
        }
        ActionId::Cmdx => {
            let mut s = take_stmt(p, b);
            if p.explain != 0 && !matches!(s, Statement::Explain { .. }) {
                let end = s.span().end;
                s = Statement::Explain {
                    span: Span::new(p.explain_start, end),
                    query_plan: p.explain == 2,
                    statement: Box::new(s),
                };
            }
            if p.n_err == 0 {
                p.done = true;
                p.result = Some(s);
            }
            SemVal::Empty
        }

        // ── transactions ──
        ActionId::CmdBegin => {
            let mode = match take(p, b + 1) {
                SemVal::TransMode(m) => Some(m),
                _ => None,
            };
            SemVal::Stmt(Box::new(Statement::Begin {
                span: p.rule_span(),
                mode,
            }))
        }
        ActionId::TransDeferred => SemVal::TransMode(TransactionMode::Deferred),
        ActionId::TransImmediate => SemVal::TransMode(TransactionMode::Immediate),
        ActionId::TransExclusive => SemVal::TransMode(TransactionMode::Exclusive),
        ActionId::CmdCommit => SemVal::Stmt(Box::new(Statement::Commit {
            span: p.rule_span(),
        })),
        ActionId::CmdRollback => SemVal::Stmt(Box::new(Statement::Rollback {
            span: p.rule_span(),
            savepoint: None,
        })),
        ActionId::CmdSavepoint => SemVal::Stmt(Box::new(Statement::Savepoint {
            span: p.rule_span(),
            name: take_name(p, b + 1),
        })),
        ActionId::CmdRelease => SemVal::Stmt(Box::new(Statement::Release {
            span: p.rule_span(),
            name: take_name(p, b + 2),
        })),
        ActionId::CmdRollbackTo => SemVal::Stmt(Box::new(Statement::Rollback {
            span: p.rule_span(),
            savepoint: Some(take_name(p, b + 4)),
        })),

        // ── CREATE TABLE ──
        ActionId::CreateTableHead => {
            let start = take_usize(p, b);
            let temp = take_bool(p, b + 1);
            let if_not_exists = take_bool(p, b + 3);
            let (schema, name) = nm_dbnm(take_name(p, b + 4), take_opt_name(p, b + 5));
            SemVal::CreateTableHead(Box::new(CreateTableHead {
                temp,
                if_not_exists,
                schema,
                name,
                start,
            }))
        }
        ActionId::CreateKw => {
            p.constraint_name = None;
            SemVal::Usize(p.st[b].max(0) as usize)
        }
        ActionId::CmdCreateTable => {
            let h = match take(p, b) {
                SemVal::CreateTableHead(h) => h,
                _ => unreachable!(),
            };
            let a = match take(p, b + 1) {
                SemVal::TableArgs(a) => a,
                _ => unreachable!(),
            };
            let without_rowid = a
                .options
                .iter()
                .any(|o| o.option == TableOptionKind::WithoutRowid);
            let strict = a
                .options
                .iter()
                .any(|o| o.option == TableOptionKind::Strict);
            SemVal::Stmt(Box::new(Statement::CreateTable(Box::new(CreateTableStmt {
                span: Span::new(h.start, p.re.max(0) as usize),
                temp: h.temp,
                if_not_exists: h.if_not_exists,
                schema: h.schema,
                name: h.name,
                columns: a.columns,
                constraints: a.constraints,
                options: a.options,
                without_rowid,
                strict,
                as_select: a.as_select.map(Box::new),
            }))))
        }
        ActionId::CreateTableArgsCols => {
            let columns = match take(p, b + 1) {
                SemVal::ColDefs(c) => c,
                _ => Vec::new(),
            };
            let constraints = match take(p, b + 2) {
                SemVal::TableConstraints(c) => c,
                _ => Vec::new(),
            };
            let options = match take(p, b + 4) {
                SemVal::TableOptions(o) => o,
                _ => Vec::new(),
            };
            SemVal::TableArgs(Box::new(TableArgs {
                columns,
                constraints,
                options,
                as_select: None,
            }))
        }
        ActionId::CreateTableArgsAsSelect => SemVal::TableArgs(Box::new(TableArgs {
            columns: Vec::new(),
            constraints: Vec::new(),
            options: Vec::new(),
            as_select: Some(take_select(p, b + 1)),
        })),
        ActionId::TableOptionSetAppend => {
            let mut list = match take(p, b) {
                SemVal::TableOptions(v) => v,
                _ => Vec::new(),
            };
            if let SemVal::TableOptions(mut more) = take(p, b + 2) {
                list.append(&mut more);
            }
            SemVal::TableOptions(list)
        }
        ActionId::TableOptionWithout => {
            let nm = take_name(p, b + 1);
            let raw = raw_name(p, &nm).to_string();
            if !(raw.len() == 5 && same_name(&raw, "rowid")) {
                let s = p.st[b + 1];
                let e = p.en[b + 1];
                p.error(
                    format!("unknown table option: {raw}"),
                    s,
                    e,
                    SqlParseErrorKind::Syntax,
                );
            }
            SemVal::TableOptions(vec![TableOption {
                span: p.rule_span(),
                option: TableOptionKind::WithoutRowid,
            }])
        }
        ActionId::TableOptionNm => {
            let nm = take_name(p, b);
            let raw = raw_name(p, &nm).to_string();
            if !(raw.len() == 6 && same_name(&raw, "strict")) {
                let rs = p.rs;
                let re = p.re;
                p.error(
                    format!("unknown table option: {raw}"),
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            }
            SemVal::TableOptions(vec![TableOption {
                span: p.rule_span(),
                option: TableOptionKind::Strict,
            }])
        }
        ActionId::ColumnListAppend => {
            let mut list = match take(p, b) {
                SemVal::ColDefs(v) => v,
                _ => Vec::new(),
            };
            list.push(build_column_def(p, b + 2, b + 3));
            SemVal::ColDefs(list)
        }
        ActionId::ColumnListSingle => SemVal::ColDefs(vec![build_column_def(p, b, b + 1)]),
        ActionId::ColumnName => {
            p.constraint_name = None;
            SemVal::ColName {
                name: take_name(p, b),
                type_name: take_opt_type_name(p, b + 1),
            }
        }

        // ── typetoken / signed ──
        ActionId::TypeNamePlain => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: Vec::new(),
            })
        }
        ActionId::TypeToken1 => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: vec![text(p, b + 2).to_string()],
            })
        }
        ActionId::TypeToken2 => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: vec![text(p, b + 2).to_string(), text(p, b + 4).to_string()],
            })
        }
        ActionId::PlusNumSigned => SemVal::Str(text(p, b + 1).to_string()),
        ActionId::PlusNumPlain => SemVal::Str(text(p, b).to_string()),
        ActionId::MinusNum => SemVal::Str(format!("-{}", text(p, b + 1))),

        // ── scanpt / scantok ──
        ActionId::ScanPt => SemVal::Usize(p.look_start),

        // ── column constraints ──
        ActionId::CargListAppend => {
            let mut list = match take(p, b) {
                SemVal::ColConstraints(v) => v,
                _ => Vec::new(),
            };
            if let SemVal::ColConstraint(c) = take(p, b + 1) {
                list.push(c);
            }
            SemVal::ColConstraints(list)
        }
        ActionId::CconsConstraintName => {
            p.constraint_name = Some(take_name(p, b + 1));
            SemVal::Empty
        }
        ActionId::CconsDefaultTerm => {
            let expr = take_expr(p, b + 2);
            default_constraint(
                p,
                DefaultForm::Literal,
                expr,
                p.st[b + 2].max(0) as usize,
                p.en[b + 2].max(0) as usize,
            )
        }
        ActionId::CconsDefaultParen => {
            let expr = take_expr(p, b + 2);
            default_constraint(
                p,
                DefaultForm::Paren,
                expr,
                p.en[b + 1].max(0) as usize,
                p.st[b + 3].max(0) as usize,
            )
        }
        ActionId::CconsDefaultPlusTerm => {
            let expr = take_expr(p, b + 3);
            default_constraint(
                p,
                DefaultForm::Signed,
                expr,
                p.st[b + 1].max(0) as usize,
                p.en[b + 3].max(0) as usize,
            )
        }
        ActionId::CconsDefaultMinusTerm => {
            let term = take_expr(p, b + 3);
            let end = term.span().end;
            let start = p.st[b + 1].max(0) as usize;
            let expr = Expr::Unary {
                span: Span::new(start, end),
                op: UnaryOp::Neg,
                operand: Box::new(term),
            };
            default_constraint(p, DefaultForm::Signed, expr, start, p.en[b + 3].max(0) as usize)
        }
        ActionId::CconsDefaultId => {
            let s = p.st[b + 2].max(0) as usize;
            let e = p.en[b + 2].max(0) as usize;
            let raw = &p.sql[s..e];
            let quoted = raw.as_bytes().first().copied().is_some_and(is_quote_char);
            let expr = if !quoted && same_name(raw, "true") {
                Expr::Boolean {
                    span: Span::new(s, e),
                    value: true,
                }
            } else if !quoted && same_name(raw, "false") {
                Expr::Boolean {
                    span: Span::new(s, e),
                    value: false,
                }
            } else {
                Expr::String(StringLiteral {
                    span: Span::new(s, e),
                    value: dequote(raw),
                })
            };
            default_constraint(p, DefaultForm::Identifier, expr, s, e)
        }
        ActionId::CconsNull => SemVal::ColConstraint(ColumnConstraint::Null {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            on_conflict: take_opt_conflict(p, b + 1),
        }),
        ActionId::CconsNotNull => SemVal::ColConstraint(ColumnConstraint::NotNull {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            on_conflict: take_opt_conflict(p, b + 2),
        }),
        ActionId::CconsPrimaryKey => SemVal::ColConstraint(ColumnConstraint::PrimaryKey {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            order: take_opt_sort_order(p, b + 2),
            on_conflict: take_opt_conflict(p, b + 3),
            autoincrement: take_bool(p, b + 4),
        }),
        ActionId::CconsUnique => SemVal::ColConstraint(ColumnConstraint::Unique {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            on_conflict: take_opt_conflict(p, b + 1),
        }),
        ActionId::CconsCheck => SemVal::ColConstraint(ColumnConstraint::Check(CheckConstraint {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            expr: take_expr(p, b + 2),
            text: check_text(p, b + 1, b + 3),
            on_conflict: None,
        })),
        ActionId::CconsReferences => {
            let (on_delete, on_update, match_name) = match take(p, b + 3) {
                SemVal::RefArgs {
                    on_delete,
                    on_update,
                    match_name,
                } => (on_delete, on_update, match_name),
                _ => (None, None, None),
            };
            let span = p.rule_span();
            SemVal::ColConstraint(ColumnConstraint::References {
                span,
                name: p.constraint_name.clone(),
                clause: ForeignKeyClause {
                    span,
                    table: take_name(p, b + 1),
                    columns: take_opt_id_list_items(p, b + 2),
                    on_delete,
                    on_update,
                    match_name,
                    deferrable: None,
                },
            })
        }
        ActionId::CconsDeferrable => {
            let clause = take_opt_deferrable(p, b).unwrap();
            SemVal::ColConstraint(ColumnConstraint::Deferrable {
                span: p.rule_span(),
                name: p.constraint_name.clone(),
                clause,
            })
        }
        ActionId::CconsCollate => SemVal::ColConstraint(ColumnConstraint::Collate {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            collation: tok_name(p, b + 1),
        }),
        ActionId::CconsGeneratedAlways => match take(p, b + 3) {
            SemVal::Generated {
                expr,
                storage_token,
            } => build_generated_constraint(p, expr, storage_token, true),
            _ => unreachable!(),
        },
        ActionId::CconsGeneratedShort => match take(p, b + 1) {
            SemVal::Generated {
                expr,
                storage_token,
            } => build_generated_constraint(p, expr, storage_token, false),
            _ => unreachable!(),
        },
        ActionId::GeneratedNoStorage => SemVal::Generated {
            expr: take_expr(p, b + 1),
            storage_token: None,
        },
        ActionId::GeneratedWithStorage => SemVal::Generated {
            expr: take_expr(p, b + 1),
            storage_token: Some(tok_name(p, b + 3)),
        },
        ActionId::RefArgsEmpty => SemVal::RefArgs {
            on_delete: None,
            on_update: None,
            match_name: None,
        },
        ActionId::RefArgsAppend => {
            let (mut on_delete, mut on_update, mut match_name) = match take(p, b) {
                SemVal::RefArgs {
                    on_delete,
                    on_update,
                    match_name,
                } => (on_delete, on_update, match_name),
                _ => (None, None, None),
            };
            if let SemVal::RefArgs {
                on_delete: d,
                on_update: u,
                match_name: m,
            } = take(p, b + 1)
            {
                if d.is_some() {
                    on_delete = d;
                }
                if u.is_some() {
                    on_update = u;
                }
                if m.is_some() {
                    match_name = m;
                }
            }
            SemVal::RefArgs {
                on_delete,
                on_update,
                match_name,
            }
        }
        ActionId::RefArgMatch => SemVal::RefArgs {
            on_delete: None,
            on_update: None,
            match_name: Some(take_name(p, b + 1)),
        },
        ActionId::RefArgInsert => SemVal::RefArgs {
            on_delete: None,
            on_update: None,
            match_name: None,
        },
        ActionId::RefArgDelete => SemVal::RefArgs {
            on_delete: Some(take_ref_action(p, b + 2)),
            on_update: None,
            match_name: None,
        },
        ActionId::RefArgUpdate => SemVal::RefArgs {
            on_delete: None,
            on_update: Some(take_ref_action(p, b + 2)),
            match_name: None,
        },
        ActionId::RefActSetNull => SemVal::RefAction(ForeignKeyAction::SetNull),
        ActionId::RefActSetDefault => SemVal::RefAction(ForeignKeyAction::SetDefault),
        ActionId::RefActCascade => SemVal::RefAction(ForeignKeyAction::Cascade),
        ActionId::RefActRestrict => SemVal::RefAction(ForeignKeyAction::Restrict),
        ActionId::RefActNoAction => SemVal::RefAction(ForeignKeyAction::NoAction),
        ActionId::DeferSubclauseNot => SemVal::Deferrable(DeferrableClause {
            span: p.rule_span(),
            not: true,
            initially: take_opt_init_deferred(p, b + 2),
        }),
        ActionId::DeferSubclause => SemVal::Deferrable(DeferrableClause {
            span: p.rule_span(),
            not: false,
            initially: take_opt_init_deferred(p, b + 1),
        }),
        ActionId::InitDeferred => SemVal::InitDeferred(InitiallyDeferred::Deferred),
        ActionId::InitImmediate => SemVal::InitDeferred(InitiallyDeferred::Immediate),

        // ── table constraints ──
        ActionId::ConsListAppend => {
            let mut list = match take(p, b) {
                SemVal::TableConstraints(v) => v,
                _ => Vec::new(),
            };
            if let SemVal::TableConstraints(mut more) = take(p, b + 2) {
                list.append(&mut more);
            }
            SemVal::TableConstraints(list)
        }
        ActionId::TconsComma => {
            p.constraint_name = None;
            SemVal::Empty
        }
        ActionId::TconsConstraintName => {
            p.constraint_name = Some(take_name(p, b + 1));
            SemVal::TableConstraints(Vec::new())
        }
        ActionId::TconsPrimaryKey => {
            SemVal::TableConstraints(vec![TableConstraint::PrimaryKey {
                span: p.rule_span(),
                name: p.constraint_name.clone(),
                columns: take_ordering_terms(p, b + 3),
                autoincrement: take_bool(p, b + 4),
                on_conflict: take_opt_conflict(p, b + 6),
            }])
        }
        ActionId::TconsUnique => SemVal::TableConstraints(vec![TableConstraint::Unique {
            span: p.rule_span(),
            name: p.constraint_name.clone(),
            columns: take_ordering_terms(p, b + 2),
            on_conflict: take_opt_conflict(p, b + 4),
        }]),
        ActionId::TconsCheck => {
            SemVal::TableConstraints(vec![TableConstraint::Check(CheckConstraint {
                span: p.rule_span(),
                name: p.constraint_name.clone(),
                expr: take_expr(p, b + 2),
                text: check_text(p, b + 1, b + 3),
                on_conflict: take_opt_conflict(p, b + 4),
            })])
        }
        ActionId::TconsForeignKey => {
            let (on_delete, on_update, match_name) = match take(p, b + 8) {
                SemVal::RefArgs {
                    on_delete,
                    on_update,
                    match_name,
                } => (on_delete, on_update, match_name),
                _ => (None, None, None),
            };
            SemVal::TableConstraints(vec![TableConstraint::ForeignKey {
                span: p.rule_span(),
                name: p.constraint_name.clone(),
                columns: take_id_list_items(p, b + 3),
                clause: ForeignKeyClause {
                    span: Span::new(p.st[b + 5].max(0) as usize, p.re.max(0) as usize),
                    table: take_name(p, b + 6),
                    columns: take_opt_id_list_items(p, b + 7),
                    on_delete,
                    on_update,
                    match_name,
                    deferrable: take_opt_deferrable(p, b + 9),
                },
            }])
        }
        ActionId::ResolveIgnore => SemVal::Conflict(ConflictAction::Ignore),
        ActionId::ResolveReplace => SemVal::Conflict(ConflictAction::Replace),
        ActionId::RaiseRollback => SemVal::Conflict(ConflictAction::Rollback),
        ActionId::RaiseAbort => SemVal::Conflict(ConflictAction::Abort),
        ActionId::RaiseFail => SemVal::Conflict(ConflictAction::Fail),

        // ── DROP ──
        ActionId::CmdDropTable => drop_cmd(p, b, DropObjectType::Table),
        ActionId::CmdDropView => drop_cmd(p, b, DropObjectType::View),
        ActionId::CmdDropIndex => drop_cmd(p, b, DropObjectType::Index),
        ActionId::CmdDropTrigger => drop_cmd(p, b, DropObjectType::Trigger),
        ActionId::FullName1 => SemVal::FullName {
            schema: None,
            name: take_name(p, b),
        },
        ActionId::FullName2 => SemVal::FullName {
            schema: Some(take_name(p, b)),
            name: take_name(p, b + 2),
        },

        // ── CREATE VIEW ──
        ActionId::CmdCreateView => {
            if p.n_var > 0 {
                let rs = p.rs;
                let re = p.re;
                p.error(
                    "parameters are not allowed in views",
                    rs,
                    re,
                    SqlParseErrorKind::Definition,
                );
            }
            let start = take_usize(p, b);
            let temp = take_bool(p, b + 1);
            let if_not_exists = take_bool(p, b + 3);
            let (schema, name) = nm_dbnm(take_name(p, b + 4), take_opt_name(p, b + 5));
            let columns = take_opt_id_list_items(p, b + 6);
            let select = Box::new(take_select(p, b + 8));
            SemVal::Stmt(Box::new(Statement::CreateView(Box::new(CreateViewStmt {
                span: Span::new(start, p.re.max(0) as usize),
                temp,
                if_not_exists,
                schema,
                name,
                columns,
                select,
            }))))
        }
        ActionId::EidListAppend => {
            let mut list = take_id_list_items(p, b);
            let col = take_opt_name(p, b + 3);
            let ord = take_opt_sort_order(p, b + 4);
            list.push(id_list_item(p, b + 2, col, ord));
            SemVal::IdListItems(list)
        }
        ActionId::EidListSingle => {
            let col = take_opt_name(p, b + 1);
            let ord = take_opt_sort_order(p, b + 2);
            SemVal::IdListItems(vec![id_list_item(p, b, col, ord)])
        }
        ActionId::CollateName => SemVal::Name(tok_name(p, b + 1)),

        // ── SELECT ──
        ActionId::SelectWith => {
            let ctes = match take(p, b + 1) {
                SemVal::Ctes(v) => v,
                _ => Vec::new(),
            };
            let w = WithClause {
                span: Span::new(p.st[b].max(0) as usize, p.en[b + 1].max(0) as usize),
                recursive: false,
                ctes,
            };
            finish_select(p, b, b + 2, Some(w))
        }
        ActionId::SelectWithRecursive => {
            let ctes = match take(p, b + 2) {
                SemVal::Ctes(v) => v,
                _ => Vec::new(),
            };
            let w = WithClause {
                span: Span::new(p.st[b].max(0) as usize, p.en[b + 2].max(0) as usize),
                recursive: true,
                ctes,
            };
            finish_select(p, b, b + 3, Some(w))
        }
        ActionId::SelectNoWith => finish_select(p, b, b, None),
        ActionId::SelectCompound => {
            let mut acc = take_compound_acc(p, b);
            let op = match take(p, b + 1) {
                SemVal::CompoundOp(o) => o,
                _ => CompoundOperator::Union,
            };
            let mut rhs_acc = take_compound_acc(p, b + 2);
            let one = rhs_acc.terms.remove(0);
            let mut h = select_height(&one);
            let mut values = one.values;
            if one.chain_heights.len() > 1 {
                h = 1;
                values = false;
            }
            let left_start = acc.body.span().start;
            let right_end = one.body.span().end;
            let left = std::mem::replace(
                &mut acc.body,
                SelectBody::Values(ValuesClause {
                    span: Span::default(),
                    rows: Vec::new(),
                }),
            );
            acc.body = SelectBody::Compound {
                span: Span::new(left_start, right_end),
                op,
                left: Box::new(left),
                right: Box::new(one.body.clone()),
            };
            acc.terms.push(one);
            acc.ops.push(op);
            acc.chain += 1;
            acc.last_values = values;
            if h > p.h[b] {
                p.h[b] = h;
            }
            SemVal::CompoundAcc(acc)
        }
        ActionId::MultiOpUnion => SemVal::CompoundOp(CompoundOperator::Union),
        ActionId::MultiOpUnionAll => SemVal::CompoundOp(CompoundOperator::UnionAll),
        ActionId::MultiOpExceptOrIntersect => {
            let op = if take_tok(p, b) == tk::EXCEPT {
                CompoundOperator::Except
            } else {
                CompoundOperator::Intersect
            };
            SemVal::CompoundOp(op)
        }
        ActionId::OneSelectNoWin => one_select(
            p,
            b,
            CoreSlots {
                distinct: Some(b + 1),
                columns: Some(b + 2),
                from: b + 3,
                where_slot: b + 4,
                group_by: Some(b + 5),
                having: b + 6,
                window: None,
                order_by: b + 7,
                limit: b + 8,
            },
            false,
            false,
        ),
        ActionId::OneSelectWithWin => one_select(
            p,
            b,
            CoreSlots {
                distinct: Some(b + 1),
                columns: Some(b + 2),
                from: b + 3,
                where_slot: b + 4,
                group_by: Some(b + 5),
                having: b + 6,
                window: Some(b + 7),
                order_by: b + 8,
                limit: b + 9,
            },
            false,
            false,
        ),
        ActionId::ValuesSingle => {
            let items = take_exprs(p, b + 2);
            let h = p.h[b + 2];
            let constant = items.iter().all(is_constant);
            let no_aff = constant && !items.iter().any(has_affinity);
            let row = ValuesRow {
                span: Span::new(p.st[b + 1].max(0) as usize, p.en[b + 3].max(0) as usize),
                items,
            };
            let values = ValuesClause {
                span: p.rule_span(),
                rows: vec![row],
            };
            single_acc(
                p,
                b,
                OneSelect {
                    body: SelectBody::Values(values),
                    order_by: None,
                    order_by_all: None,
                    limit: None,
                    chain_heights: vec![h],
                    coroutine: false,
                    last_row_no_affinity: no_aff,
                    values: true,
                },
            )
        }
        ActionId::MvaluesAppend => {
            let acc = take_compound_acc(p, b);
            add_values_row(p, b, acc, b + 3, b + 2, b + 4)
        }
        ActionId::Distinct => SemVal::Distinct(DistinctKind::Distinct),
        ActionId::DistinctAll => SemVal::Distinct(DistinctKind::All),
        ActionId::SelColExpr => {
            let mut list = take_result_cols(p, b);
            let s0 = take_usize(p, b + 1);
            let expr = take_expr(p, b + 2);
            let s1 = take_usize(p, b + 3);
            let alias = take_opt_name(p, b + 4);
            let (a, z) = trimmed(p.sql, s0, s1);
            let start = expr.span().start;
            let end = if alias.is_some() {
                p.en[b + 4].max(0) as usize
            } else {
                expr.span().end
            };
            list.push(ResultColumn::Expr(ResultExpr {
                span: Span::new(start, end),
                expr,
                alias,
                text: p.sql[a..z].to_string(),
            }));
            let h = p.h[b + 2];
            if h > p.h[b] {
                p.h[b] = h;
            }
            p.f[b] = 0;
            SemVal::ResultCols(list)
        }
        ActionId::SelColStar => {
            let mut list = take_result_cols(p, b);
            list.push(ResultColumn::Star(ResultStar {
                span: Span::new(p.st[b + 2].max(0) as usize, p.en[b + 2].max(0) as usize),
                table: None,
                exclude: None,
                replace: None,
            }));
            if p.h[b] < 1 {
                p.h[b] = 1;
            }
            SemVal::ResultCols(list)
        }
        ActionId::SelColTableStar => {
            let mut list = take_result_cols(p, b);
            list.push(ResultColumn::Star(ResultStar {
                span: Span::new(p.st[b + 2].max(0) as usize, p.en[b + 4].max(0) as usize),
                table: Some(take_name(p, b + 2)),
                exclude: None,
                replace: None,
            }));
            if p.h[b] < 2 {
                p.h[b] = 2;
            }
            SemVal::ResultCols(list)
        }
        ActionId::TokName0 => SemVal::Name(tok_name(p, b)),

        // ── FROM ──
        ActionId::StlPrefix => {
            let left = Box::new(take_from_source(p, b));
            let op = match take(p, b + 1) {
                SemVal::JoinOp(j) => j,
                _ => JoinOperator {
                    span: Span::default(),
                    comma: false,
                    flags: join_flag::INNER,
                },
            };
            SemVal::StlPrefix { left, op }
        }
        ActionId::SelTabTable => {
            let term = from_table(p, b + 1, b + 3, None);
            append_from_term(p, b, term, b + 4)
        }
        ActionId::SelTabTableIndexed => {
            let indexed = match take(p, b + 4) {
                SemVal::IndexedBy {
                    indexed_by,
                    not_indexed,
                } => Some((indexed_by, not_indexed)),
                _ => None,
            };
            let term = from_table(p, b + 1, b + 4, indexed);
            append_from_term(p, b, term, b + 5)
        }
        ActionId::SelTabFunction => {
            let (schema, name) = nm_dbnm(take_name(p, b + 1), take_opt_name(p, b + 2));
            let args = take_exprs(p, b + 4);
            let alias = take_opt_name(p, b + 6);
            let span = span_slots(p, b + 1, b + 6);
            let term = FromSource::Function(FromFunction {
                span,
                schema,
                name,
                args,
                alias,
            });
            append_from_term(p, b, term, b + 7)
        }
        ActionId::SelTabSubquery => {
            let span = span_slots(p, b + 1, b + 4);
            let term = FromSource::Subquery(FromSubquery {
                span,
                select: Box::new(take_select(p, b + 2)),
                alias: take_opt_name(p, b + 4),
            });
            append_from_term(p, b, term, b + 5)
        }
        ActionId::SelTabGroup => {
            let span = span_slots(p, b + 1, b + 4);
            let term = FromSource::Group(FromGroup {
                span,
                source: Box::new(take_from_source(p, b + 2)),
                alias: take_opt_name(p, b + 4),
            });
            append_from_term(p, b, term, b + 5)
        }
        ActionId::XFullName1 => xfullname_action(p, None, b, None),
        ActionId::XFullName2 => xfullname_action(p, Some(b), b + 2, None),
        ActionId::XFullName1Alias => xfullname_action(p, None, b, Some(b + 2)),
        ActionId::XFullName2Alias => xfullname_action(p, Some(b), b + 2, Some(b + 4)),
        ActionId::JoinOpCommaOrJoin => SemVal::JoinOp(JoinOperator {
            span: p.rule_span(),
            comma: take_tok(p, b) == tk::COMMA,
            flags: join_flag::INNER,
        }),
        ActionId::JoinOp1 => {
            let w0 = text(p, b).to_string();
            let flags = join_type(p, &[w0.as_str()]);
            SemVal::JoinOp(JoinOperator {
                span: p.rule_span(),
                comma: false,
                flags,
            })
        }
        ActionId::JoinOp2 => {
            let w0 = text(p, b).to_string();
            let n1 = take_name(p, b + 1);
            let w1 = raw_name(p, &n1).to_string();
            let flags = join_type(p, &[w0.as_str(), w1.as_str()]);
            SemVal::JoinOp(JoinOperator {
                span: p.rule_span(),
                comma: false,
                flags,
            })
        }
        ActionId::JoinOp3 => {
            let w0 = text(p, b).to_string();
            let n1 = take_name(p, b + 1);
            let n2 = take_name(p, b + 2);
            let w1 = raw_name(p, &n1).to_string();
            let w2 = raw_name(p, &n2).to_string();
            let flags = join_type(p, &[w0.as_str(), w1.as_str(), w2.as_str()]);
            SemVal::JoinOp(JoinOperator {
                span: p.rule_span(),
                comma: false,
                flags,
            })
        }
        ActionId::OnUsingOn => SemVal::OnUsing {
            on: Some(take_expr(p, b + 1)),
            using: None,
        },
        ActionId::OnUsingUsing => SemVal::OnUsing {
            on: None,
            using: Some(take_names(p, b + 2)),
        },
        ActionId::IndexedBy => SemVal::IndexedBy {
            indexed_by: Some(take_name(p, b + 2)),
            not_indexed: false,
        },
        ActionId::NotIndexed => SemVal::IndexedBy {
            indexed_by: None,
            not_indexed: true,
        },

        // ── ORDER BY / GROUP BY / HAVING / LIMIT / WHERE ──
        ActionId::SortListAppend => {
            let mut list = take_ordering_terms(p, b);
            list.push(ordering_term(p, b + 2));
            if p.h[b + 2] > p.h[b] {
                p.h[b] = p.h[b + 2];
            }
            p.f[b] |= p.f[b + 2] & FUNC;
            SemVal::OrderingTerms(list)
        }
        ActionId::SortListSingle => {
            p.f[b] &= FUNC;
            SemVal::OrderingTerms(vec![ordering_term(p, b)])
        }
        ActionId::SortAsc => SemVal::SortOrder(SortOrder::Asc),
        ActionId::SortDesc => SemVal::SortOrder(SortOrder::Desc),
        ActionId::NullsFirst => SemVal::NullsOrder(NullsOrder::First),
        ActionId::NullsLast => SemVal::NullsOrder(NullsOrder::Last),
        ActionId::LimitOnly => {
            let h = p.h[b + 1] + 1;
            set_pexpr(p, b, h, 0);
            SemVal::Limit(LimitClause {
                span: p.rule_span(),
                count: Box::new(take_expr(p, b + 1)),
                offset: None,
                comma_form: false,
            })
        }
        ActionId::LimitOffset => {
            let h = p.h[b + 1].max(p.h[b + 3]) + 1;
            set_pexpr(p, b, h, 0);
            SemVal::Limit(LimitClause {
                span: p.rule_span(),
                count: Box::new(take_expr(p, b + 1)),
                offset: Some(Box::new(take_expr(p, b + 3))),
                comma_form: false,
            })
        }
        ActionId::LimitComma => {
            let h = p.h[b + 1].max(p.h[b + 3]) + 1;
            set_pexpr(p, b, h, 0);
            SemVal::Limit(LimitClause {
                span: p.rule_span(),
                count: Box::new(take_expr(p, b + 3)),
                offset: Some(Box::new(take_expr(p, b + 1))),
                comma_form: true,
            })
        }

        // ── DELETE / UPDATE / INSERT ──
        ActionId::WhereRetEmpty => SemVal::WhereReturning {
            where_clause: None,
            returning: None,
        },
        ActionId::WhereRetWhere => SemVal::WhereReturning {
            where_clause: Some(take_expr(p, b + 1)),
            returning: None,
        },
        ActionId::WhereRetReturning => SemVal::WhereReturning {
            where_clause: None,
            returning: Some(take_result_cols(p, b + 1)),
        },
        ActionId::WhereRetBoth => SemVal::WhereReturning {
            where_clause: Some(take_expr(p, b + 1)),
            returning: Some(take_result_cols(p, b + 3)),
        },
        ActionId::CmdDelete => {
            let with = take_opt_with(p, b);
            let table = take_from_table(p, b + 3);
            let table = with_indexed(p, table, b + 4);
            let (where_clause, returning) = match take(p, b + 5) {
                SemVal::WhereReturning {
                    where_clause,
                    returning,
                } => (where_clause, returning),
                _ => (None, None),
            };
            SemVal::Stmt(Box::new(Statement::Delete(Box::new(DeleteStmt {
                span: p.rule_span(),
                with,
                table,
                where_clause,
                returning,
            }))))
        }
        ActionId::CmdUpdate => {
            let with = take_opt_with(p, b);
            let or_conflict = take_opt_conflict(p, b + 2);
            let table = take_from_table(p, b + 3);
            let table = with_indexed(p, table, b + 4);
            let set = take_set_clauses(p, b + 6);
            let set_cols: usize = set.iter().map(|s| s.columns.len()).sum();
            if set_cols > p.max_columns {
                let rs = p.rs;
                let re = p.re;
                p.error(
                    "too many columns in set list",
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            }
            let from = take_opt_from_source(p, b + 7);
            let (where_clause, returning) = match take(p, b + 8) {
                SemVal::WhereReturning {
                    where_clause,
                    returning,
                } => (where_clause, returning),
                _ => (None, None),
            };
            SemVal::Stmt(Box::new(Statement::Update(Box::new(UpdateStmt {
                span: p.rule_span(),
                with,
                or_conflict,
                table,
                set,
                from,
                where_clause,
                returning,
            }))))
        }
        ActionId::SetListAppendSingle => {
            let mut list = take_set_clauses(p, b);
            list.push(set_clause_single(p, b + 2, b + 4));
            SemVal::SetClauses(list)
        }
        ActionId::SetListAppendVector => {
            let mut list = take_set_clauses(p, b);
            list.push(set_clause_vector(p, b + 2));
            SemVal::SetClauses(list)
        }
        ActionId::SetListSingle => SemVal::SetClauses(vec![set_clause_single(p, b, b + 2)]),
        ActionId::SetListVector => SemVal::SetClauses(vec![set_clause_vector(p, b)]),
        ActionId::CmdInsertSelect => {
            let with = take_opt_with(p, b);
            let (or_conflict, replace_into) = match take(p, b + 1) {
                SemVal::InsertCmd {
                    or_conflict,
                    replace_into,
                } => (or_conflict, replace_into),
                _ => (None, false),
            };
            let table = take_from_table(p, b + 3);
            let columns = take_opt_names(p, b + 4);
            let select = Some(Box::new(take_select(p, b + 5)));
            let (upsert, returning) = match take(p, b + 6) {
                SemVal::Upsert { clauses, returning } => (clauses, returning),
                _ => (Vec::new(), None),
            };
            SemVal::Stmt(Box::new(Statement::Insert(Box::new(InsertStmt {
                span: p.rule_span(),
                with,
                or_conflict,
                replace_into,
                table,
                columns,
                select,
                upsert,
                returning,
            }))))
        }
        ActionId::CmdInsertDefault => {
            let with = take_opt_with(p, b);
            let (or_conflict, replace_into) = match take(p, b + 1) {
                SemVal::InsertCmd {
                    or_conflict,
                    replace_into,
                } => (or_conflict, replace_into),
                _ => (None, false),
            };
            let table = take_from_table(p, b + 3);
            let columns = take_opt_names(p, b + 4);
            let returning = take_opt_result_cols(p, b + 7);
            SemVal::Stmt(Box::new(Statement::Insert(Box::new(InsertStmt {
                span: p.rule_span(),
                with,
                or_conflict,
                replace_into,
                table,
                columns,
                select: None,
                upsert: Vec::new(),
                returning,
            }))))
        }
        ActionId::UpsertEmpty => SemVal::Upsert {
            clauses: Vec::new(),
            returning: None,
        },
        ActionId::UpsertReturning => SemVal::Upsert {
            clauses: Vec::new(),
            returning: Some(take_result_cols(p, b + 1)),
        },
        ActionId::UpsertTargetUpdate => {
            let c = UpsertClause {
                span: span_slots(p, b, b + 10),
                target: Some(take_ordering_terms(p, b + 3)),
                target_where: take_opt_expr(p, b + 5),
                action: UpsertAction::Update,
                set: Some(take_set_clauses(p, b + 9)),
                where_clause: take_opt_expr(p, b + 10),
            };
            let (mut rest, returning) = match take(p, b + 11) {
                SemVal::Upsert { clauses, returning } => (clauses, returning),
                _ => (Vec::new(), None),
            };
            let mut clauses = Vec::with_capacity(rest.len() + 1);
            clauses.push(c);
            clauses.append(&mut rest);
            SemVal::Upsert { clauses, returning }
        }
        ActionId::UpsertTargetNothing => {
            let c = UpsertClause {
                span: span_slots(p, b, b + 7),
                target: Some(take_ordering_terms(p, b + 3)),
                target_where: take_opt_expr(p, b + 5),
                action: UpsertAction::Nothing,
                set: None,
                where_clause: None,
            };
            let (mut rest, returning) = match take(p, b + 8) {
                SemVal::Upsert { clauses, returning } => (clauses, returning),
                _ => (Vec::new(), None),
            };
            let mut clauses = Vec::with_capacity(rest.len() + 1);
            clauses.push(c);
            clauses.append(&mut rest);
            SemVal::Upsert { clauses, returning }
        }
        ActionId::UpsertCatchAllNothing => SemVal::Upsert {
            clauses: vec![UpsertClause {
                span: span_slots(p, b, b + 3),
                target: None,
                target_where: None,
                action: UpsertAction::Nothing,
                set: None,
                where_clause: None,
            }],
            returning: take_opt_result_cols(p, b + 4),
        },
        ActionId::UpsertCatchAllUpdate => SemVal::Upsert {
            clauses: vec![UpsertClause {
                span: span_slots(p, b, b + 6),
                target: None,
                target_where: None,
                action: UpsertAction::Update,
                set: Some(take_set_clauses(p, b + 5)),
                where_clause: take_opt_expr(p, b + 6),
            }],
            returning: take_opt_result_cols(p, b + 7),
        },
        ActionId::Returning => SemVal::ResultCols(take_result_cols(p, b + 1)),
        ActionId::InsertCmdInsert => SemVal::InsertCmd {
            or_conflict: take_opt_conflict(p, b + 1),
            replace_into: false,
        },
        ActionId::InsertCmdReplace => SemVal::InsertCmd {
            or_conflict: Some(ConflictAction::Replace),
            replace_into: true,
        },
        ActionId::IdListAppend => {
            let mut list = take_names(p, b);
            list.push(take_name(p, b + 2));
            SemVal::Names(list)
        }
        ActionId::IdListSingle => SemVal::Names(vec![take_name(p, b)]),

        // ── expressions ──
        ActionId::ExprColumn1 => {
            p.h[b] = 1;
            SemVal::Expr(Expr::Column(ColumnRef {
                span: p.rule_span(),
                schema: None,
                table: None,
                column: tok_name(p, b),
            }))
        }
        ActionId::ExprColumn2 => {
            set_pexpr(p, b, 2, 0);
            SemVal::Expr(Expr::Column(ColumnRef {
                span: p.rule_span(),
                schema: None,
                table: Some(take_name(p, b)),
                column: take_name(p, b + 2),
            }))
        }
        ActionId::ExprColumn3 => {
            let rs = p.rs;
            let re = p.re;
            p.check_height(2, rs, re);
            set_pexpr(p, b, 3, 0);
            SemVal::Expr(Expr::Column(ColumnRef {
                span: p.rule_span(),
                schema: Some(take_name(p, b)),
                table: Some(take_name(p, b + 2)),
                column: take_name(p, b + 4),
            }))
        }
        ActionId::TermNullFloatBlob => {
            let t = take_tok(p, b);
            let s = p.st[b].max(0) as usize;
            let e = p.en[b].max(0) as usize;
            p.h[b] = 1;
            if t == tk::NULL {
                SemVal::Expr(Expr::Null {
                    span: Span::new(s, e),
                })
            } else if t == tk::FLOAT {
                SemVal::Expr(Expr::Number(NumberLiteral {
                    span: Span::new(s, e),
                    text: p.sql[s..e].to_string(),
                    is_float: true,
                    is_hex: false,
                    separators: false,
                }))
            } else {
                let hex = &p.sql.as_bytes()[s + 2..e - 1];
                let mut value = Vec::with_capacity(hex.len() >> 1);
                let mut i = 0;
                while i + 1 < hex.len() {
                    value.push((hex_value(hex[i]) << 4) | hex_value(hex[i + 1]));
                    i += 2;
                }
                SemVal::Expr(Expr::Blob(BlobLiteral {
                    span: Span::new(s, e),
                    value,
                }))
            }
        }
        ActionId::TermString => {
            p.h[b] = 1;
            SemVal::Expr(Expr::String(StringLiteral {
                span: p.rule_span(),
                value: dequote(text(p, b)),
            }))
        }
        ActionId::TermInteger => {
            let z = text(p, b);
            let zb = z.as_bytes();
            let is_hex = zb.len() > 1 && zb[0] == b'0' && (zb[1] | 0x20) == b'x';
            p.h[b] = 1;
            p.f[b] = if is_zero_integer(z) { FALSE } else { 0 };
            SemVal::Expr(Expr::Number(NumberLiteral {
                span: p.rule_span(),
                text: z.to_string(),
                is_float: false,
                is_hex,
                separators: false,
            }))
        }
        ActionId::TermQnumber => {
            let r = dequote_number(text(p, b));
            if let Some(err_tok) = r.error {
                let rs = p.rs;
                let re = p.re;
                p.error(
                    format!("unrecognized token: \"{err_tok}\""),
                    rs,
                    re,
                    SqlParseErrorKind::Syntax,
                );
            }
            p.h[b] = 1;
            SemVal::Expr(Expr::Number(NumberLiteral {
                span: p.rule_span(),
                text: r.text,
                is_float: r.is_float,
                is_hex: r.is_hex,
                separators: true,
            }))
        }
        ActionId::TermCtime => {
            let raw = text(p, b);
            p.h[b] = 1;
            p.f[b] = FUNC;
            let keyword = if same_name(raw, "current_time") {
                CurrentTimeKeyword::CurrentTime
            } else if same_name(raw, "current_date") {
                CurrentTimeKeyword::CurrentDate
            } else {
                CurrentTimeKeyword::CurrentTimestamp
            };
            SemVal::Expr(Expr::CurrentTime {
                span: p.rule_span(),
                keyword,
            })
        }
        ActionId::ExprVariable => {
            let s = p.st[b].max(0) as usize;
            let e = p.en[b].max(0) as usize;
            let z = p.sql[s..e].to_string();
            let zb = z.as_bytes();
            p.h[b] = 1;
            if zb.len() > 1 && zb[0] == b'#' && zb[1].is_ascii_digit() {
                p.syntax_error_at(s, e);
                return SemVal::Expr(Expr::Null {
                    span: Span::new(s, e),
                });
            }
            let index = variable_number(p, &z, s, e);
            SemVal::Expr(Expr::Variable(VariableExpr {
                span: Span::new(s, e),
                text: z,
                index,
            }))
        }
        ActionId::ExprCollate => {
            let operand = take_expr(p, b);
            let h = p.h[b] + 1;
            let rs = p.rs;
            let re = p.re;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            p.h[b] = h;
            p.f[b] = 0;
            SemVal::Expr(Expr::Collate {
                span: p.rule_span(),
                operand: Box::new(operand),
                collation: tok_name(p, b + 2),
            })
        }
        ActionId::ExprCast => {
            p.h[b] = p.h[b + 2] + 1;
            p.f[b] = p.f[b + 2] & FUNC;
            SemVal::Expr(Expr::Cast {
                span: p.rule_span(),
                operand: Box::new(take_expr(p, b + 2)),
                type_name: take_opt_type_name(p, b + 4),
            })
        }
        ActionId::ExprFuncArgs => {
            let q = match take(p, b + 2) {
                SemVal::Distinct(d) => Some(d),
                _ => None,
            };
            function_call(p, b, q, Some(b + 3), false, None, None)
        }
        ActionId::ExprFuncArgsOrderBy => {
            let q = match take(p, b + 2) {
                SemVal::Distinct(d) => Some(d),
                _ => None,
            };
            function_call(p, b, q, Some(b + 3), false, Some(b + 6), None)
        }
        ActionId::ExprFuncStar => function_call(p, b, None, None, true, None, None),
        ActionId::ExprFuncArgsFilterOver => {
            let q = match take(p, b + 2) {
                SemVal::Distinct(d) => Some(d),
                _ => None,
            };
            function_call(p, b, q, Some(b + 3), false, None, Some(b + 5))
        }
        ActionId::ExprFuncArgsOrderByFilterOver => {
            let q = match take(p, b + 2) {
                SemVal::Distinct(d) => Some(d),
                _ => None,
            };
            function_call(p, b, q, Some(b + 3), false, Some(b + 6), Some(b + 8))
        }
        ActionId::ExprFuncStarFilterOver => {
            function_call(p, b, None, None, true, None, Some(b + 4))
        }
        ActionId::ExprVector => {
            let mut items = take_exprs(p, b + 1);
            items.push(take_expr(p, b + 3));
            let item_height = p.h[b + 1].max(p.h[b + 3]);
            p.h[b] = 1;
            p.f[b] = (p.f[b + 1] | p.f[b + 3]) & FUNC;
            SemVal::Expr(Expr::Vector {
                span: p.rule_span(),
                items,
                item_height,
            })
        }
        ActionId::ExprBinary => {
            let op = match take_tok(p, b + 1) {
                tk::OR => BinaryOp::Or,
                tk::AND => BinaryOp::And,
                tk::EQ => BinaryOp::Eq,
                tk::NE => BinaryOp::Ne,
                tk::LT => BinaryOp::Lt,
                tk::LE => BinaryOp::Le,
                tk::GT => BinaryOp::Gt,
                tk::GE => BinaryOp::Ge,
                tk::BITAND => BinaryOp::BitAnd,
                tk::BITOR => BinaryOp::BitOr,
                tk::LSHIFT => BinaryOp::LShift,
                tk::RSHIFT => BinaryOp::RShift,
                tk::PLUS => BinaryOp::Add,
                tk::MINUS => BinaryOp::Sub,
                tk::STAR => BinaryOp::Mul,
                tk::SLASH => BinaryOp::Div,
                tk::REM => BinaryOp::Rem,
                tk::CONCAT => BinaryOp::Concat,
                _ => BinaryOp::Eq,
            };
            binary_expr(p, b, op)
        }
        ActionId::LikeOpPos => like_op_action(p, b, false),
        ActionId::LikeOpNeg => like_op_action(p, b + 1, true),
        ActionId::ExprLike => like_expr_action(p, b, None),
        ActionId::ExprLikeEscape => like_expr_action(p, b, Some(b + 4)),
        ActionId::ExprIsNullOrNotNull => {
            let operand = take_expr(p, b);
            let not = take_tok(p, b + 1) == tk::NOTNULL;
            is_null_node(p, b, operand, not)
        }
        ActionId::ExprNotNull => {
            let operand = take_expr(p, b);
            is_null_node(p, b, operand, true)
        }
        ActionId::ExprIs => is_node(p, b, b + 2, BinaryOp::Is),
        ActionId::ExprIsNot => is_node(p, b, b + 3, BinaryOp::IsNot),
        ActionId::ExprIsNotDistinctFrom => is_node(p, b, b + 5, BinaryOp::Is),
        ActionId::ExprIsDistinctFrom => is_node(p, b, b + 4, BinaryOp::IsNot),
        ActionId::ExprUnaryNot => {
            let h = p.h[b + 1] + 1;
            let f = p.f[b + 1];
            set_pexpr(p, b, h, f);
            SemVal::Expr(Expr::Unary {
                span: p.rule_span(),
                op: UnaryOp::Not,
                operand: Box::new(take_expr(p, b + 1)),
            })
        }
        ActionId::ExprUnaryBitNot => {
            let h = p.h[b + 1] + 1;
            let f = p.f[b + 1];
            set_pexpr(p, b, h, f);
            SemVal::Expr(Expr::Unary {
                span: p.rule_span(),
                op: UnaryOp::BitNot,
                operand: Box::new(take_expr(p, b + 1)),
            })
        }
        ActionId::ExprUnaryPlusMinus => {
            let plus = take_tok(p, b) == tk::PLUS;
            let ho = p.h[b + 1];
            let fo = p.f[b + 1];
            if (fo & UPLUS) != 0 {
                p.h[b] = ho;
                p.f[b] = (fo & FUNC) | if plus { UPLUS } else { 0 };
            } else {
                set_pexpr(p, b, ho + 1, fo);
                if plus {
                    p.f[b] |= UPLUS;
                }
            }
            SemVal::Expr(Expr::Unary {
                span: p.rule_span(),
                op: if plus { UnaryOp::Pos } else { UnaryOp::Neg },
                operand: Box::new(take_expr(p, b + 1)),
            })
        }
        ActionId::ExprPtr => {
            let h = p.h[b].max(p.h[b + 2]) + 1;
            let rs = p.rs;
            let re = p.re;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            let f = FUNC | ((p.f[b] | p.f[b + 2]) & FUNC);
            let op = if p.en[b + 1] - p.st[b + 1] == 3 {
                BinaryOp::Ptr2
            } else {
                BinaryOp::Ptr
            };
            p.h[b] = h;
            p.f[b] = f;
            SemVal::Expr(Expr::Binary {
                span: p.rule_span(),
                op,
                left: Box::new(take_expr(p, b)),
                right: Box::new(take_expr(p, b + 2)),
            })
        }
        ActionId::ExprBetween => {
            let not = take_bool(p, b + 1);
            let rs = p.rs;
            let re = p.re;
            let hb = p.h[b];
            p.check_height(hb + 1, rs, re);
            let mut h = hb.max(p.h[b + 2]).max(p.h[b + 4]) + 1;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            if not {
                h += 1;
                p.check_height(h, rs, re);
            }
            let f = (p.f[b] | p.f[b + 2] | p.f[b + 4]) & FUNC;
            p.h[b] = h;
            p.f[b] = f;
            SemVal::Expr(Expr::Between {
                span: p.rule_span(),
                not,
                operand: Box::new(take_expr(p, b)),
                low: Box::new(take_expr(p, b + 2)),
                high: Box::new(take_expr(p, b + 4)),
            })
        }
        ActionId::ExprInList => in_list_action(p, b),
        ActionId::ExprSubquery => {
            let h = p.h[b + 1] + 1;
            let rs = p.rs;
            let re = p.re;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            p.h[b] = h;
            p.f[b] = 0;
            SemVal::Expr(Expr::Subquery {
                span: p.rule_span(),
                select: Box::new(take_select(p, b + 1)),
            })
        }
        ActionId::ExprInSelect => {
            let not = take_bool(p, b + 1);
            let rs = p.rs;
            let re = p.re;
            let hb = p.h[b];
            p.check_height(hb + 1, rs, re);
            let mut h = hb.max(p.h[b + 3]) + 1;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            if not {
                h += 1;
                p.check_height(h, rs, re);
            }
            p.h[b] = h;
            p.f[b] &= FUNC;
            SemVal::Expr(Expr::InSelect {
                span: p.rule_span(),
                not,
                operand: Box::new(take_expr(p, b)),
                select: Box::new(take_select(p, b + 3)),
            })
        }
        ActionId::ExprInTable => {
            let not = take_bool(p, b + 1);
            let (schema, table) = nm_dbnm(take_name(p, b + 2), take_opt_name(p, b + 3));
            let rs = p.rs;
            let re = p.re;
            let mut h = p.h[b] + 1;
            p.check_height(h, rs, re);
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            if not {
                h += 1;
                p.check_height(h, rs, re);
            }
            p.h[b] = h;
            p.f[b] &= FUNC;
            SemVal::Expr(Expr::InTable {
                span: p.rule_span(),
                not,
                operand: Box::new(take_expr(p, b)),
                schema,
                table,
                args: take_opt_exprs(p, b + 4),
            })
        }
        ActionId::ExprExists => {
            let h = p.h[b + 2] + 1;
            let rs = p.rs;
            let re = p.re;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            p.h[b] = h;
            p.f[b] = 0;
            SemVal::Expr(Expr::Exists {
                span: p.rule_span(),
                select: Box::new(take_select(p, b + 2)),
            })
        }
        ActionId::ExprCase => {
            let operand = take_opt_expr(p, b + 1);
            let whens = match take(p, b + 2) {
                SemVal::CaseWhens(w) => w,
                _ => Vec::new(),
            };
            let else_expr = take_opt_expr(p, b + 3);
            let ho = if operand.is_some() { p.h[b + 1] } else { 0 };
            let he = if else_expr.is_some() { p.h[b + 3] } else { 0 };
            let rs = p.rs;
            let re = p.re;
            p.check_height(ho + 1, rs, re);
            let h = ho.max(p.h[b + 2]).max(he) + 1;
            if p.n_err == 0 {
                p.check_height(h, rs, re);
            }
            let fo = if operand.is_some() { p.f[b + 1] } else { 0 };
            let fe = if else_expr.is_some() { p.f[b + 3] } else { 0 };
            p.h[b] = h;
            p.f[b] = (fo | p.f[b + 2] | fe) & FUNC;
            SemVal::Expr(Expr::Case {
                span: p.rule_span(),
                operand: operand.map(Box::new),
                whens,
                else_expr: else_expr.map(Box::new),
            })
        }
        ActionId::CaseWhenAppend => {
            let mut list = match take(p, b) {
                SemVal::CaseWhens(w) => w,
                _ => Vec::new(),
            };
            list.push(CaseWhen {
                span: Span::new(p.st[b + 1].max(0) as usize, p.en[b + 4].max(0) as usize),
                when: take_expr(p, b + 2),
                then: take_expr(p, b + 4),
            });
            p.h[b] = p.h[b].max(p.h[b + 2]).max(p.h[b + 4]);
            p.f[b] = (p.f[b] | p.f[b + 2] | p.f[b + 4]) & FUNC;
            SemVal::CaseWhens(list)
        }
        ActionId::CaseWhenSingle => {
            p.h[b] = p.h[b + 1].max(p.h[b + 3]);
            p.f[b] = (p.f[b + 1] | p.f[b + 3]) & FUNC;
            SemVal::CaseWhens(vec![CaseWhen {
                span: p.rule_span(),
                when: take_expr(p, b + 1),
                then: take_expr(p, b + 3),
            }])
        }
        ActionId::NExprListAppend => {
            let mut list = take_exprs(p, b);
            list.push(take_expr(p, b + 2));
            if p.h[b + 2] > p.h[b] {
                p.h[b] = p.h[b + 2];
            }
            p.f[b] = (p.f[b] | p.f[b + 2]) & FUNC;
            SemVal::Exprs(list)
        }
        ActionId::NExprListSingle => {
            p.f[b] &= FUNC;
            SemVal::Exprs(vec![take_expr(p, b)])
        }
        ActionId::ParenExprList => SemVal::Exprs(take_exprs(p, b + 1)),

        // ── CREATE INDEX ──
        ActionId::CmdCreateIndex => {
            let start = take_usize(p, b);
            let unique = take_bool(p, b + 1);
            let if_not_exists = take_bool(p, b + 3);
            let (schema, name) = nm_dbnm(take_name(p, b + 4), take_opt_name(p, b + 5));
            let table = take_name(p, b + 7);
            let columns = take_ordering_terms(p, b + 9);
            let where_clause = take_opt_expr(p, b + 11);
            SemVal::Stmt(Box::new(Statement::CreateIndex(Box::new(CreateIndexStmt {
                span: Span::new(start, p.re.max(0) as usize),
                unique,
                if_not_exists,
                schema,
                name,
                table,
                columns,
                where_clause,
            }))))
        }

        // ── VACUUM / PRAGMA ──
        ActionId::CmdVacuum => SemVal::Stmt(Box::new(Statement::Vacuum {
            span: p.rule_span(),
            schema: None,
            into: take_opt_expr(p, b + 1).map(Box::new),
        })),
        ActionId::CmdVacuumSchema => SemVal::Stmt(Box::new(Statement::Vacuum {
            span: p.rule_span(),
            schema: Some(take_name(p, b + 1)),
            into: take_opt_expr(p, b + 2).map(Box::new),
        })),
        ActionId::CmdPragmaGet => pragma_action(p, b, None),
        ActionId::CmdPragmaEq => pragma_action(p, b, Some(PragmaValueForm::Eq)),
        ActionId::CmdPragmaCall => pragma_action(p, b, Some(PragmaValueForm::Call)),
        ActionId::NmNumText => SemVal::Str(text(p, b).to_string()),

        // ── CREATE TRIGGER ──
        ActionId::CmdCreateTrigger => {
            let start = take_usize(p, b);
            let d = match take(p, b + 1) {
                SemVal::TriggerDecl(d) => d,
                _ => unreachable!(),
            };
            let body = match take(p, b + 3) {
                SemVal::TriggerSteps(s) => s,
                _ => Vec::new(),
            };
            p.in_trigger = false;
            SemVal::Stmt(Box::new(Statement::CreateTrigger(Box::new(
                CreateTriggerStmt {
                    span: Span::new(start, p.re.max(0) as usize),
                    temp: d.temp,
                    if_not_exists: d.if_not_exists,
                    schema: d.schema,
                    name: d.name,
                    time: d.time,
                    event: d.event,
                    update_of: d.update_of,
                    table_schema: d.table_schema,
                    table: d.table,
                    for_each_row: d.for_each_row,
                    when: d.when,
                    body,
                },
            ))))
        }
        ActionId::TriggerDecl => {
            let temp = take_bool(p, b);
            let if_not_exists = take_bool(p, b + 2);
            let (schema, name) = nm_dbnm(take_name(p, b + 3), take_opt_name(p, b + 4));
            let time = match take(p, b + 5) {
                SemVal::TriggerTime(t) => Some(t),
                _ => None,
            };
            let (event, update_of) = match take(p, b + 6) {
                SemVal::TriggerEvent { event, update_of } => (event, update_of),
                _ => (TriggerEventKind::Insert, None),
            };
            let (table_schema, table) = take_fullname(p, b + 8);
            let for_each_row = take_bool(p, b + 9);
            let when = take_opt_expr(p, b + 10);
            p.in_trigger = true;
            SemVal::TriggerDecl(Box::new(TriggerDeclData {
                temp,
                if_not_exists,
                schema,
                name,
                time,
                event,
                update_of,
                table_schema,
                table,
                for_each_row,
                when,
            }))
        }
        ActionId::TriggerTimeBeforeAfter => {
            let t = if take_tok(p, b) == tk::BEFORE {
                TriggerTime::Before
            } else {
                TriggerTime::After
            };
            SemVal::TriggerTime(t)
        }
        ActionId::TriggerTimeInsteadOf => SemVal::TriggerTime(TriggerTime::InsteadOf),
        ActionId::TriggerEventDeleteInsert => {
            let event = if take_tok(p, b) == tk::DELETE {
                TriggerEventKind::Delete
            } else {
                TriggerEventKind::Insert
            };
            SemVal::TriggerEvent {
                event,
                update_of: None,
            }
        }
        ActionId::TriggerEventUpdate => SemVal::TriggerEvent {
            event: TriggerEventKind::Update,
            update_of: None,
        },
        ActionId::TriggerEventUpdateOf => SemVal::TriggerEvent {
            event: TriggerEventKind::Update,
            update_of: Some(take_names(p, b + 2)),
        },
        ActionId::TriggerCmdListAppend => {
            let mut list = match take(p, b) {
                SemVal::TriggerSteps(s) => s,
                _ => Vec::new(),
            };
            list.push(take_trigger_step(p, b + 1));
            SemVal::TriggerSteps(list)
        }
        ActionId::TriggerCmdListSingle => SemVal::TriggerSteps(vec![take_trigger_step(p, b)]),
        ActionId::TrIdxByIndexed => {
            let rs = p.rs;
            let re = p.re;
            p.error(
                "the INDEXED BY clause is not allowed on UPDATE or DELETE statements within triggers",
                rs,
                re,
                SqlParseErrorKind::Syntax,
            );
            SemVal::Empty
        }
        ActionId::TrIdxByNotIndexed => {
            let rs = p.rs;
            let re = p.re;
            p.error(
                "the NOT INDEXED clause is not allowed on UPDATE or DELETE statements within triggers",
                rs,
                re,
                SqlParseErrorKind::Syntax,
            );
            SemVal::Empty
        }
        ActionId::TriggerCmdUpdate => SemVal::TriggerStep(TriggerStep::Update(Box::new(
            UpdateStmt {
                span: p.rule_span(),
                with: None,
                or_conflict: take_opt_conflict(p, b + 1),
                table: take_from_table(p, b + 2),
                set: take_set_clauses(p, b + 5),
                from: take_opt_from_source(p, b + 6),
                where_clause: take_opt_expr(p, b + 7),
                returning: None,
            },
        ))),
        ActionId::TriggerCmdInsert => {
            let (or_conflict, replace_into) = match take(p, b + 1) {
                SemVal::InsertCmd {
                    or_conflict,
                    replace_into,
                } => (or_conflict, replace_into),
                _ => (None, false),
            };
            let (upsert, returning) = match take(p, b + 6) {
                SemVal::Upsert { clauses, returning } => (clauses, returning),
                _ => (Vec::new(), None),
            };
            SemVal::TriggerStep(TriggerStep::Insert(Box::new(InsertStmt {
                span: p.rule_span(),
                with: None,
                or_conflict,
                replace_into,
                table: take_from_table(p, b + 3),
                columns: take_opt_names(p, b + 4),
                select: Some(Box::new(take_select(p, b + 5))),
                upsert,
                returning,
            })))
        }
        ActionId::TriggerCmdDelete => SemVal::TriggerStep(TriggerStep::Delete(Box::new(
            DeleteStmt {
                span: p.rule_span(),
                with: None,
                table: take_from_table(p, b + 2),
                where_clause: take_opt_expr(p, b + 4),
                returning: None,
            },
        ))),
        ActionId::ExprRaiseIgnore => {
            p.h[b] = 1;
            SemVal::Expr(Expr::Raise {
                span: p.rule_span(),
                action: RaiseAction::Ignore,
                message: None,
            })
        }
        ActionId::ExprRaiseMsg => {
            let h = p.h[b + 4] + 1;
            let f = p.f[b + 4];
            set_pexpr(p, b, h, f);
            let action = match take_conflict(p, b + 2) {
                ConflictAction::Rollback => RaiseAction::Rollback,
                ConflictAction::Fail => RaiseAction::Fail,
                _ => RaiseAction::Abort,
            };
            SemVal::Expr(Expr::Raise {
                span: p.rule_span(),
                action,
                message: Some(Box::new(take_expr(p, b + 4))),
            })
        }

        // ── ATTACH / DETACH / REINDEX / ANALYZE ──
        ActionId::CmdAttach => SemVal::Stmt(Box::new(Statement::Attach {
            span: p.rule_span(),
            file: Box::new(take_expr(p, b + 2)),
            schema: Box::new(take_expr(p, b + 4)),
            key: take_opt_expr(p, b + 5).map(Box::new),
        })),
        ActionId::CmdDetach => SemVal::Stmt(Box::new(Statement::Detach {
            span: p.rule_span(),
            schema: Box::new(take_expr(p, b + 2)),
        })),
        ActionId::CmdReindex => SemVal::Stmt(Box::new(Statement::Reindex {
            span: p.rule_span(),
            schema: None,
            name: None,
        })),
        ActionId::CmdReindexName => {
            let (schema, name) = nm_dbnm(take_name(p, b + 1), take_opt_name(p, b + 2));
            SemVal::Stmt(Box::new(Statement::Reindex {
                span: p.rule_span(),
                schema,
                name: Some(name),
            }))
        }
        ActionId::CmdAnalyze => SemVal::Stmt(Box::new(Statement::Analyze {
            span: p.rule_span(),
            schema: None,
            name: None,
        })),
        ActionId::CmdAnalyzeName => {
            let (schema, name) = nm_dbnm(take_name(p, b + 1), take_opt_name(p, b + 2));
            SemVal::Stmt(Box::new(Statement::Analyze {
                span: p.rule_span(),
                schema,
                name: Some(name),
            }))
        }

        // ── ALTER TABLE ──
        ActionId::CmdAlterRenameTable => {
            let span = span_slots(p, b + 3, b + 5);
            let new_name = take_name(p, b + 5);
            alter_action(p, b, AlterTableAction::RenameTable { span, new_name })
        }
        ActionId::CmdAlterAddColumn => {
            let a = match take(p, b) {
                SemVal::AlterAdd(a) => a,
                _ => unreachable!(),
            };
            let e = span_slots(p, b, b + 1).end;
            let ce = if p.en[b + 1] >= 0 {
                p.en[b + 1] as usize
            } else {
                e
            };
            let declared_type =
                stored_type(a.type_name.as_ref().map(|t| t.text.as_str())).pragma_type;
            let constraints = match take(p, b + 1) {
                SemVal::ColConstraints(c) => c,
                _ => Vec::new(),
            };
            let col_start = a.name.span.start;
            let column = ColumnDef {
                span: Span::new(col_start, ce),
                name: a.name,
                type_name: a.type_name,
                declared_type,
                constraints,
            };
            SemVal::Stmt(Box::new(Statement::AlterTable(Box::new(AlterTableStmt {
                span: Span::new(a.start, p.re.max(0) as usize),
                schema: a.schema,
                table: a.table,
                action: AlterTableAction::AddColumn {
                    span: Span::new(a.action_start, ce),
                    column,
                },
            }))))
        }
        ActionId::AlterAddHead => {
            p.constraint_name = None;
            let (schema, table) = take_fullname(p, b + 2);
            SemVal::AlterAdd(Box::new(AlterAddData {
                schema,
                table,
                name: take_name(p, b + 5),
                type_name: take_opt_type_name(p, b + 6),
                start: p.rs.max(0) as usize,
                action_start: p.st[b + 3].max(0) as usize,
            }))
        }
        ActionId::CmdAlterDropColumn => {
            let span = span_slots(p, b + 3, b + 5);
            let column = take_name(p, b + 5);
            alter_action(p, b, AlterTableAction::DropColumn { span, column })
        }
        ActionId::CmdAlterRenameColumn => {
            let span = span_slots(p, b + 3, b + 7);
            let column = take_name(p, b + 5);
            let new_name = take_name(p, b + 7);
            alter_action(
                p,
                b,
                AlterTableAction::RenameColumn {
                    span,
                    column,
                    new_name,
                },
            )
        }
        ActionId::CmdAlterDropConstraint => {
            let span = span_slots(p, b + 3, b + 5);
            let name = take_name(p, b + 5);
            alter_action(p, b, AlterTableAction::DropConstraint { span, name })
        }
        ActionId::CmdAlterDropNotNull => {
            let span = span_slots(p, b + 3, b + 8);
            let column = take_name(p, b + 5);
            alter_action(p, b, AlterTableAction::DropNotNull { span, column })
        }
        ActionId::CmdAlterSetNotNull => {
            let span = span_slots(p, b + 3, b + 9);
            let column = take_name(p, b + 5);
            let on_conflict = take_opt_conflict(p, b + 9);
            alter_action(
                p,
                b,
                AlterTableAction::SetNotNull {
                    span,
                    column,
                    on_conflict,
                },
            )
        }
        ActionId::CmdAlterAddNamedCheck => {
            let span = span_slots(p, b + 3, b + 10);
            let name = Some(take_name(p, b + 5));
            let expr = take_expr(p, b + 8);
            let text = check_text(p, b + 7, b + 9);
            let on_conflict = take_opt_conflict(p, b + 10);
            alter_action(
                p,
                b,
                AlterTableAction::AddCheck {
                    span,
                    name,
                    expr,
                    text,
                    on_conflict,
                },
            )
        }
        ActionId::CmdAlterAddCheck => {
            let span = span_slots(p, b + 3, b + 8);
            let expr = take_expr(p, b + 6);
            let text = check_text(p, b + 5, b + 7);
            let on_conflict = take_opt_conflict(p, b + 8);
            alter_action(
                p,
                b,
                AlterTableAction::AddCheck {
                    span,
                    name: None,
                    expr,
                    text,
                    on_conflict,
                },
            )
        }

        // ── CREATE VIRTUAL TABLE ──
        ActionId::CmdCreateVtab => {
            let h = match take(p, b) {
                SemVal::VtabHead(h) => h,
                _ => unreachable!(),
            };
            SemVal::Stmt(Box::new(Statement::CreateVirtualTable(Box::new(
                CreateVirtualTableStmt {
                    span: Span::new(h.start, p.re.max(0) as usize),
                    if_not_exists: h.if_not_exists,
                    schema: h.schema,
                    name: h.name,
                    module: h.module,
                    args: None,
                },
            ))))
        }
        ActionId::CmdCreateVtabArgs => {
            let h = match take(p, b) {
                SemVal::VtabHead(h) => h,
                _ => unreachable!(),
            };
            let args = match take(p, b + 2) {
                SemVal::VtabArgs(a) => a,
                _ => Vec::new(),
            };
            SemVal::Stmt(Box::new(Statement::CreateVirtualTable(Box::new(
                CreateVirtualTableStmt {
                    span: Span::new(h.start, p.re.max(0) as usize),
                    if_not_exists: h.if_not_exists,
                    schema: h.schema,
                    name: h.name,
                    module: h.module,
                    args: Some(args),
                },
            ))))
        }
        ActionId::CreateVtabHead => {
            let start = take_usize(p, b);
            let if_not_exists = take_bool(p, b + 3);
            let (schema, name) = nm_dbnm(take_name(p, b + 4), take_opt_name(p, b + 5));
            let module = take_name(p, b + 7);
            SemVal::VtabHead(Box::new(VtabHeadData {
                if_not_exists,
                schema,
                name,
                module,
                start,
            }))
        }
        ActionId::VtabArgExtend => {
            if p.rs < 0 {
                SemVal::VtabArgs(Vec::new())
            } else {
                let sp = p.rule_span();
                SemVal::VtabArgs(vec![VtabArg {
                    span: sp,
                    text: p.sql[sp.start..sp.end].to_string(),
                }])
            }
        }
        ActionId::VtabArgListAppend => {
            let mut list = match take(p, b) {
                SemVal::VtabArgs(v) => v,
                _ => Vec::new(),
            };
            if let SemVal::VtabArgs(mut more) = take(p, b + 2) {
                list.append(&mut more);
            }
            SemVal::VtabArgs(list)
        }

        // ── WITH ──
        ActionId::WithPlain => {
            let ctes = match take(p, b + 1) {
                SemVal::Ctes(v) => v,
                _ => Vec::new(),
            };
            SemVal::WithClause(WithClause {
                span: p.rule_span(),
                recursive: false,
                ctes,
            })
        }
        ActionId::WithRecursive => {
            let ctes = match take(p, b + 2) {
                SemVal::Ctes(v) => v,
                _ => Vec::new(),
            };
            SemVal::WithClause(WithClause {
                span: p.rule_span(),
                recursive: true,
                ctes,
            })
        }
        ActionId::WqItem => SemVal::Cte(Box::new(CommonTableExpr {
            span: p.rule_span(),
            name: take_name(p, b),
            columns: take_opt_id_list_items(p, b + 1),
            materialized: take_opt_bool(p, b + 2),
            select: take_select(p, b + 4),
        })),
        ActionId::WithNm => {
            p.has_with = true;
            take(p, b)
        }
        ActionId::WqListSingle => match take(p, b) {
            SemVal::Cte(c) => SemVal::Ctes(vec![*c]),
            _ => SemVal::Ctes(Vec::new()),
        },
        ActionId::WqListAppend => {
            let mut list = match take(p, b) {
                SemVal::Ctes(v) => v,
                _ => Vec::new(),
            };
            if let SemVal::Cte(cte) = take(p, b + 2) {
                for c in &list {
                    if same_name(&c.name.name, &cte.name.name) {
                        p.error(
                            format!("duplicate WITH table name: {}", cte.name.name),
                            cte.span.start as isize,
                            cte.span.end as isize,
                            SqlParseErrorKind::Syntax,
                        );
                    }
                }
                list.push(*cte);
            }
            SemVal::Ctes(list)
        }

        // ── windows ──
        ActionId::WindowDefnListAppend => {
            let mut list = match take(p, b) {
                SemVal::WindowList(l) => l,
                _ => WindowList {
                    defs: Vec::new(),
                    metas: Vec::new(),
                },
            };
            if let SemVal::WindowList(mut rhs) = take(p, b + 2) {
                let d = rhs.defs.remove(0);
                let mut meta = rhs.metas.remove(0);
                if let Some(ref base) = meta.base {
                    let exist = list
                        .metas
                        .iter()
                        .rev()
                        .find(|m| same_name(&m.raw_name, base));
                    match exist {
                        None => {
                            p.error(
                                format!("no such window: {base}"),
                                d.span.start as isize,
                                d.span.end as isize,
                                SqlParseErrorKind::Syntax,
                            );
                        }
                        Some(ex) => {
                            let what = if meta.has_partition {
                                Some("PARTITION clause")
                            } else if ex.has_order_by && meta.has_order_by {
                                Some("ORDER BY clause")
                            } else if !ex.implicit_frame {
                                Some("frame specification")
                            } else {
                                None
                            };
                            if let Some(what) = what {
                                p.error(
                                    format!("cannot override {what} of window: {base}"),
                                    d.span.start as isize,
                                    d.span.end as isize,
                                    SqlParseErrorKind::Syntax,
                                );
                            } else {
                                meta.has_partition = ex.has_partition;
                                if ex.has_order_by {
                                    meta.has_order_by = true;
                                }
                            }
                        }
                    }
                }
                list.defs.push(d);
                list.metas.push(meta);
            }
            SemVal::WindowList(list)
        }
        ActionId::WindowDefnSingle => {
            let name = take_name(p, b);
            let (mut w, wmeta) = take_window_spec(p, b + 3);
            w.span = Span::new(p.st[b + 2].max(0) as usize, p.en[b + 4].max(0) as usize);
            let raw = raw_name(p, &name).to_string();
            let meta = WindowDefMeta {
                raw_name: raw,
                base: wmeta.base,
                has_partition: w.partition_by.is_some(),
                has_order_by: w.order_by.is_some(),
                implicit_frame: wmeta.implicit_frame,
            };
            let def = WindowDefinition {
                span: p.rule_span(),
                name,
                window: w,
            };
            SemVal::WindowList(WindowList {
                defs: vec![def],
                metas: vec![meta],
            })
        }
        ActionId::WindowPartition => {
            let part = Some(take_exprs(p, b + 2));
            let ob = take_window_order_by(p, b + 3);
            let frame = take_frame_of(p, b + 4);
            build_window_spec(p, None, part, ob, frame)
        }
        ActionId::WindowBasePartition => {
            let base = Some(take_name(p, b));
            let part = Some(take_exprs(p, b + 3));
            let ob = take_window_order_by(p, b + 4);
            let frame = take_frame_of(p, b + 5);
            build_window_spec(p, base, part, ob, frame)
        }
        ActionId::WindowOrderBy => {
            let ob = Some(take_ordering_terms(p, b + 2));
            let frame = take_frame_of(p, b + 3);
            build_window_spec(p, None, None, ob, frame)
        }
        ActionId::WindowBaseOrderBy => {
            let base = Some(take_name(p, b));
            let ob = Some(take_ordering_terms(p, b + 3));
            let frame = take_frame_of(p, b + 4);
            build_window_spec(p, base, None, ob, frame)
        }
        ActionId::WindowBaseFrame => {
            let base = Some(take_name(p, b));
            let frame = take_frame_of(p, b + 1);
            build_window_spec(p, base, None, None, frame)
        }
        ActionId::FrameOptEmpty => build_window_spec(p, None, None, None, None),
        ActionId::FrameOptSingle => {
            let units = match take(p, b) {
                SemVal::FrameUnits(u) => u,
                _ => FrameUnits::Range,
            };
            let from = match take(p, b + 1) {
                SemVal::FrameBound(fb) => fb,
                _ => unreachable!(),
            };
            let exclude = match take(p, b + 2) {
                SemVal::FrameExclude(ex) => Some(ex),
                _ => None,
            };
            let frame = build_frame_spec(p, units, from, None, exclude);
            build_window_spec(p, None, None, None, Some(frame))
        }
        ActionId::FrameOptBetween => {
            let units = match take(p, b) {
                SemVal::FrameUnits(u) => u,
                _ => FrameUnits::Range,
            };
            let from = match take(p, b + 2) {
                SemVal::FrameBound(fb) => fb,
                _ => unreachable!(),
            };
            let to = match take(p, b + 4) {
                SemVal::FrameBound(fb) => Some(fb),
                _ => None,
            };
            let exclude = match take(p, b + 5) {
                SemVal::FrameExclude(ex) => Some(ex),
                _ => None,
            };
            let frame = build_frame_spec(p, units, from, to, exclude);
            build_window_spec(p, None, None, None, Some(frame))
        }
        ActionId::RangeOrRows => {
            let u = match take_tok(p, b) {
                tk::RANGE => FrameUnits::Range,
                tk::ROWS => FrameUnits::Rows,
                _ => FrameUnits::Groups,
            };
            SemVal::FrameUnits(u)
        }
        ActionId::FrameBoundUnboundedPreceding => SemVal::FrameBound(FrameBound {
            span: p.rule_span(),
            kind: FrameBoundType::UnboundedPreceding,
            expr: None,
        }),
        ActionId::FrameBoundUnboundedFollowing => SemVal::FrameBound(FrameBound {
            span: p.rule_span(),
            kind: FrameBoundType::UnboundedFollowing,
            expr: None,
        }),
        ActionId::FrameBoundExpr => {
            let kind = if take_tok(p, b + 1) == tk::PRECEDING {
                FrameBoundType::Preceding
            } else {
                FrameBoundType::Following
            };
            SemVal::FrameBound(FrameBound {
                span: p.rule_span(),
                kind,
                expr: Some(Box::new(take_expr(p, b))),
            })
        }
        ActionId::FrameBoundCurrentRow => SemVal::FrameBound(FrameBound {
            span: p.rule_span(),
            kind: FrameBoundType::CurrentRow,
            expr: None,
        }),
        ActionId::FrameExcludeNoOthers => SemVal::FrameExclude(FrameExclude::NoOthers),
        ActionId::FrameExcludeCurrentRow => SemVal::FrameExclude(FrameExclude::CurrentRow),
        ActionId::FrameExcludeGroupOrTies => {
            let ex = if take_tok(p, b) == tk::GROUP {
                FrameExclude::Group
            } else {
                FrameExclude::Ties
            };
            SemVal::FrameExclude(ex)
        }
        ActionId::WindowClause => match take(p, b + 1) {
            SemVal::WindowList(l) => SemVal::WindowDefs(l.defs),
            _ => SemVal::WindowDefs(Vec::new()),
        },
        ActionId::FilterOverBoth => SemVal::FilterOver {
            filter: Some(Box::new(take_expr(p, b))),
            over: Some(take_window_ref(p, b + 1)),
        },
        ActionId::FilterOverOnlyOver => SemVal::FilterOver {
            filter: None,
            over: Some(take_window_ref(p, b)),
        },
        ActionId::FilterOverOnlyFilter => SemVal::FilterOver {
            filter: Some(Box::new(take_expr(p, b))),
            over: None,
        },
        ActionId::OverClauseSpec => {
            let (mut w, _) = take_window_spec(p, b + 2);
            w.span = Span::new(p.st[b + 1].max(0) as usize, p.en[b + 3].max(0) as usize);
            SemVal::WindowRef(WindowRef::Spec(Box::new(w)))
        }
        ActionId::FilterClause => take(p, b + 3),

        // ── XDB dialect ──
        ActionId::XdbExprDcolonCast => {
            p.h[b] += 1;
            p.f[b] &= FUNC;
            SemVal::Expr(Expr::Cast {
                span: p.rule_span(),
                operand: Box::new(take_expr(p, b)),
                type_name: take_opt_type_name(p, b + 2),
            })
        }
        ActionId::XdbCastType0 => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: Vec::new(),
            })
        }
        ActionId::XdbCastType1 => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: vec![take_str(p, b + 2)],
            })
        }
        ActionId::XdbCastType2 => {
            let sp = p.rule_span();
            SemVal::TypeName(TypeName {
                span: sp,
                text: p.sql[sp.start..sp.end].to_string(),
                args: vec![take_str(p, b + 2), take_str(p, b + 4)],
            })
        }
        ActionId::XdbGroupByAll => SemVal::GroupByAll,
        ActionId::XdbOrderByAll => SemVal::OrderByAll(OrderByAll {
            span: p.rule_span(),
            order: take_opt_sort_order(p, b + 3),
            nulls: take_opt_nulls(p, b + 4),
        }),
        ActionId::XdbSelColStarMods => {
            let mut list = take_result_cols(p, b);
            let (exclude, replace) = match take(p, b + 3) {
                SemVal::StarMods { exclude, replace } => (exclude, replace),
                _ => (None, None),
            };
            list.push(ResultColumn::Star(ResultStar {
                span: Span::new(p.st[b + 2].max(0) as usize, p.en[b + 3].max(0) as usize),
                table: None,
                exclude,
                replace,
            }));
            if p.h[b] < 1 {
                p.h[b] = 1;
            }
            SemVal::ResultCols(list)
        }
        ActionId::XdbSelColTableStarMods => {
            let mut list = take_result_cols(p, b);
            let (exclude, replace) = match take(p, b + 5) {
                SemVal::StarMods { exclude, replace } => (exclude, replace),
                _ => (None, None),
            };
            list.push(ResultColumn::Star(ResultStar {
                span: Span::new(p.st[b + 2].max(0) as usize, p.en[b + 5].max(0) as usize),
                table: Some(take_name(p, b + 2)),
                exclude,
                replace,
            }));
            if p.h[b] < 2 {
                p.h[b] = 2;
            }
            SemVal::ResultCols(list)
        }
        ActionId::XdbStarModsExcludeList => SemVal::StarMods {
            exclude: Some(take_names(p, b + 2)),
            replace: None,
        },
        ActionId::XdbStarModsExcludeOne => SemVal::StarMods {
            exclude: Some(vec![take_name(p, b + 1)]),
            replace: None,
        },
        ActionId::XdbStarModsReplace => {
            let replace = match take(p, b + 2) {
                SemVal::StarReplacements(r) => Some(r),
                _ => None,
            };
            SemVal::StarMods {
                exclude: None,
                replace,
            }
        }
        ActionId::XdbStarModsExcludeListReplace => {
            let exclude = Some(take_names(p, b + 2));
            let replace = match take(p, b + 6) {
                SemVal::StarReplacements(r) => Some(r),
                _ => None,
            };
            SemVal::StarMods { exclude, replace }
        }
        ActionId::XdbStarModsExcludeOneReplace => {
            let exclude = Some(vec![take_name(p, b + 1)]);
            let replace = match take(p, b + 4) {
                SemVal::StarReplacements(r) => Some(r),
                _ => None,
            };
            SemVal::StarMods { exclude, replace }
        }
        ActionId::XdbRepListAppend => {
            let mut list = match take(p, b) {
                SemVal::StarReplacements(r) => r,
                _ => Vec::new(),
            };
            list.push(StarReplacement {
                span: Span::new(p.st[b + 2].max(0) as usize, p.en[b + 4].max(0) as usize),
                expr: take_expr(p, b + 2),
                name: take_name(p, b + 4),
            });
            SemVal::StarReplacements(list)
        }
        ActionId::XdbRepListSingle => SemVal::StarReplacements(vec![StarReplacement {
            span: p.rule_span(),
            expr: take_expr(p, b),
            name: take_name(p, b + 2),
        }]),
        ActionId::XdbFromFirstBare => one_select(
            p,
            b,
            CoreSlots {
                distinct: None,
                columns: None,
                from: b + 1,
                where_slot: b + 2,
                group_by: Some(b + 3),
                having: b + 4,
                window: None,
                order_by: b + 5,
                limit: b + 6,
            },
            true,
            true,
        ),
        ActionId::XdbFromFirstSelect => one_select(
            p,
            b,
            CoreSlots {
                distinct: Some(b + 3),
                columns: Some(b + 4),
                from: b + 1,
                where_slot: b + 5,
                group_by: Some(b + 6),
                having: b + 7,
                window: None,
                order_by: b + 8,
                limit: b + 9,
            },
            true,
            false,
        ),
        ActionId::XdbFromFirstSelectWin => one_select(
            p,
            b,
            CoreSlots {
                distinct: Some(b + 3),
                columns: Some(b + 4),
                from: b + 1,
                where_slot: b + 5,
                group_by: Some(b + 6),
                having: b + 7,
                window: Some(b + 8),
                order_by: b + 9,
                limit: b + 10,
            },
            true,
            false,
        ),
    }
}
