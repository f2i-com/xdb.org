//! AST for every statement of SQLite 3.54.1's grammar (`src/parse.y`), plus the
//! XDB dialect's additions.
//!
//! Every node carries its UTF-8 byte [`Span`] (`[start, end)`), so
//! `&sql[span.start..span.end]` is the node's exact source text.

/// Source byte span `[start, end)` in UTF-8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[inline]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// How an identifier was quoted in SQL source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuoteStyle {
    None,
    Double,
    Single,
    Bracket,
    Backtick,
}

impl QuoteStyle {
    #[inline]
    pub fn from_raw(raw: &str) -> Self {
        match raw.as_bytes().first().copied() {
            Some(b'"') => QuoteStyle::Double,
            Some(b'\'') => QuoteStyle::Single,
            Some(b'[') => QuoteStyle::Bracket,
            Some(b'`') => QuoteStyle::Backtick,
            _ => QuoteStyle::None,
        }
    }
}

/// An identifier (`parse.y` `nm` / `id` / `ids` / `idj`), dequoted with `sqlite3Dequote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub span: Span,
    pub name: String,
    pub quote: QuoteStyle,
}

// ───────────────────────────── expressions ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberLiteral {
    pub span: Span,
    pub text: String,
    pub is_float: bool,
    pub is_hex: bool,
    /// True when written with `_` digit separators (`TK_QNUMBER`).
    pub separators: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteral {
    pub span: Span,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobLiteral {
    pub span: Span,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CurrentTimeKeyword {
    CurrentTime,
    CurrentDate,
    CurrentTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableExpr {
    pub span: Span,
    pub text: String,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub span: Span,
    pub schema: Option<Name>,
    pub table: Option<Name>,
    pub column: Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Not,
    Neg,
    Pos,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
    BitAnd,
    BitOr,
    LShift,
    RShift,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
    Ptr,
    Ptr2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LikeOperator {
    Like,
    Glob,
    Regexp,
    Match,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseWhen {
    pub span: Span,
    pub when: Expr,
    pub then: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DistinctKind {
    Distinct,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NullsOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderingTerm {
    pub span: Span,
    pub expr: Expr,
    pub order: Option<SortOrder>,
    pub nulls: Option<NullsOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameUnits {
    Range,
    Rows,
    Groups,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameBoundType {
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRow,
    Preceding,
    Following,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBound {
    pub span: Span,
    pub kind: FrameBoundType,
    pub expr: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameExclude {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSpec {
    pub span: Span,
    pub units: FrameUnits,
    pub from: FrameBound,
    pub to: Option<FrameBound>,
    pub exclude: Option<FrameExclude>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSpec {
    pub span: Span,
    pub base: Option<Name>,
    pub partition_by: Option<Vec<Expr>>,
    pub order_by: Option<Vec<OrderingTerm>>,
    pub frame: Option<FrameSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRef {
    Spec(Box<WindowSpec>),
    Name(Name),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionExpr {
    pub span: Span,
    pub name: Name,
    pub quantifier: Option<DistinctKind>,
    pub star: bool,
    pub args: Vec<Expr>,
    pub order_by: Option<Vec<OrderingTerm>>,
    pub filter: Option<Box<Expr>>,
    pub over: Option<WindowRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RaiseAction {
    Ignore,
    Rollback,
    Abort,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Number(NumberLiteral),
    String(StringLiteral),
    Blob(BlobLiteral),
    Null {
        span: Span,
    },
    Boolean {
        span: Span,
        value: bool,
    },
    CurrentTime {
        span: Span,
        keyword: CurrentTimeKeyword,
    },
    Variable(VariableExpr),
    Column(ColumnRef),
    Unary {
        span: Span,
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        span: Span,
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        span: Span,
        not: bool,
        operand: Box<Expr>,
    },
    Like {
        span: Span,
        op: LikeOperator,
        not: bool,
        left: Box<Expr>,
        right: Box<Expr>,
        escape: Option<Box<Expr>>,
        /// XDB dialect: written `ILIKE`.
        ilike: bool,
    },
    Between {
        span: Span,
        not: bool,
        operand: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
    },
    InList {
        span: Span,
        not: bool,
        operand: Box<Expr>,
        items: Vec<Expr>,
    },
    InSelect {
        span: Span,
        not: bool,
        operand: Box<Expr>,
        select: Box<SelectStmt>,
    },
    InTable {
        span: Span,
        not: bool,
        operand: Box<Expr>,
        schema: Option<Name>,
        table: Name,
        args: Option<Vec<Expr>>,
    },
    Exists {
        span: Span,
        select: Box<SelectStmt>,
    },
    Subquery {
        span: Span,
        select: Box<SelectStmt>,
    },
    Cast {
        span: Span,
        operand: Box<Expr>,
        type_name: Option<TypeName>,
    },
    Collate {
        span: Span,
        operand: Box<Expr>,
        collation: Name,
    },
    Case {
        span: Span,
        operand: Option<Box<Expr>>,
        whens: Vec<CaseWhen>,
        else_expr: Option<Box<Expr>>,
    },
    Function(Box<FunctionExpr>),
    Vector {
        span: Span,
        items: Vec<Expr>,
        /// Max item expression height (used during parse-time height checks).
        item_height: u32,
    },
    Raise {
        span: Span,
        action: RaiseAction,
        message: Option<Box<Expr>>,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Number(n) => n.span,
            Expr::String(s) => s.span,
            Expr::Blob(b) => b.span,
            Expr::Null { span }
            | Expr::Boolean { span, .. }
            | Expr::CurrentTime { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::Like { span, .. }
            | Expr::Between { span, .. }
            | Expr::InList { span, .. }
            | Expr::InSelect { span, .. }
            | Expr::InTable { span, .. }
            | Expr::Exists { span, .. }
            | Expr::Subquery { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Collate { span, .. }
            | Expr::Case { span, .. }
            | Expr::Vector { span, .. }
            | Expr::Raise { span, .. } => *span,
            Expr::Variable(v) => v.span,
            Expr::Column(c) => c.span,
            Expr::Function(f) => f.span,
        }
    }
}

// ───────────────────────────── SELECT ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStmt {
    pub span: Span,
    pub with: Option<WithClause>,
    pub body: SelectBody,
    pub order_by: Option<Vec<OrderingTerm>>,
    /// XDB dialect: `ORDER BY ALL [ASC|DESC] [NULLS FIRST|LAST]`.
    pub order_by_all: Option<OrderByAll>,
    pub limit: Option<LimitClause>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompoundOperator {
    Union,
    UnionAll,
    Intersect,
    Except,
}

impl CompoundOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            CompoundOperator::Union => "UNION",
            CompoundOperator::UnionAll => "UNION ALL",
            CompoundOperator::Intersect => "INTERSECT",
            CompoundOperator::Except => "EXCEPT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectBody {
    Core(Box<SelectCore>),
    Values(ValuesClause),
    Compound {
        span: Span,
        op: CompoundOperator,
        left: Box<SelectBody>,
        right: Box<SelectBody>,
    },
}

impl SelectBody {
    pub fn span(&self) -> Span {
        match self {
            SelectBody::Core(c) => c.span,
            SelectBody::Values(v) => v.span,
            SelectBody::Compound { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectCore {
    pub span: Span,
    pub distinct: Option<DistinctKind>,
    pub columns: Vec<ResultColumn>,
    pub from: Option<FromSource>,
    pub where_clause: Option<Expr>,
    pub group_by: Option<Vec<Expr>>,
    /// XDB dialect: `GROUP BY ALL`.
    pub group_by_all: bool,
    pub having: Option<Expr>,
    pub windows: Option<Vec<WindowDefinition>>,
    /// XDB dialect: FROM-first query.
    pub from_first: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuesClause {
    pub span: Span,
    pub rows: Vec<ValuesRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuesRow {
    pub span: Span,
    pub items: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitClause {
    pub span: Span,
    pub count: Box<Expr>,
    pub offset: Option<Box<Expr>>,
    pub comma_form: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderByAll {
    pub span: Span,
    pub order: Option<SortOrder>,
    pub nulls: Option<NullsOrder>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultColumn {
    Expr(ResultExpr),
    Star(ResultStar),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultExpr {
    pub span: Span,
    pub expr: Expr,
    pub alias: Option<Name>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultStar {
    pub span: Span,
    pub table: Option<Name>,
    pub exclude: Option<Vec<Name>>,
    pub replace: Option<Vec<StarReplacement>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarReplacement {
    pub span: Span,
    pub expr: Expr,
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithClause {
    pub span: Span,
    pub recursive: bool,
    pub ctes: Vec<CommonTableExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonTableExpr {
    pub span: Span,
    pub name: Name,
    pub columns: Option<Vec<IdListItem>>,
    pub materialized: Option<bool>,
    pub select: SelectStmt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowDefinition {
    pub span: Span,
    pub name: Name,
    pub window: WindowSpec,
}

// ── FROM ──

/// `JT_*` bits of `sqlite3JoinType` (`sqliteInt.h`).
pub mod join_flag {
    pub const INNER: u8 = 0x01;
    pub const CROSS: u8 = 0x02;
    pub const NATURAL: u8 = 0x04;
    pub const LEFT: u8 = 0x08;
    pub const RIGHT: u8 = 0x10;
    pub const OUTER: u8 = 0x20;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinOperator {
    pub span: Span,
    pub comma: bool,
    pub flags: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromTable {
    pub span: Span,
    pub schema: Option<Name>,
    pub name: Name,
    pub alias: Option<Name>,
    pub indexed_by: Option<Name>,
    pub not_indexed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromFunction {
    pub span: Span,
    pub schema: Option<Name>,
    pub name: Name,
    pub args: Vec<Expr>,
    pub alias: Option<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromSubquery {
    pub span: Span,
    pub select: Box<SelectStmt>,
    pub alias: Option<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromGroup {
    pub span: Span,
    pub source: Box<FromSource>,
    pub alias: Option<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromSource {
    Table(FromTable),
    Function(FromFunction),
    Subquery(FromSubquery),
    Group(FromGroup),
    Join {
        span: Span,
        left: Box<FromSource>,
        operator: JoinOperator,
        right: Box<FromSource>,
        on: Option<Box<Expr>>,
        using: Option<Vec<Name>>,
    },
}

impl FromSource {
    pub fn span(&self) -> Span {
        match self {
            FromSource::Table(t) => t.span,
            FromSource::Function(f) => f.span,
            FromSource::Subquery(s) => s.span,
            FromSource::Group(g) => g.span,
            FromSource::Join { span, .. } => *span,
        }
    }
}

// ───────────────────────────── DDL ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeName {
    pub span: Span,
    pub text: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictAction {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForeignKeyAction {
    SetNull,
    SetDefault,
    Cascade,
    Restrict,
    NoAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdListItem {
    pub span: Span,
    pub name: Name,
    pub collate: Option<Name>,
    pub order: Option<SortOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InitiallyDeferred {
    Deferred,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferrableClause {
    pub span: Span,
    pub not: bool,
    pub initially: Option<InitiallyDeferred>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyClause {
    pub span: Span,
    pub table: Name,
    pub columns: Option<Vec<IdListItem>>,
    pub on_delete: Option<ForeignKeyAction>,
    pub on_update: Option<ForeignKeyAction>,
    pub match_name: Option<Name>,
    pub deferrable: Option<DeferrableClause>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefaultForm {
    Literal,
    Signed,
    Paren,
    Identifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratedStorage {
    Virtual,
    Stored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConstraint {
    pub span: Span,
    pub name: Option<Name>,
    pub expr: Expr,
    pub text: String,
    pub on_conflict: Option<ConflictAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnConstraint {
    PrimaryKey {
        span: Span,
        name: Option<Name>,
        order: Option<SortOrder>,
        on_conflict: Option<ConflictAction>,
        autoincrement: bool,
    },
    NotNull {
        span: Span,
        name: Option<Name>,
        on_conflict: Option<ConflictAction>,
    },
    Null {
        span: Span,
        name: Option<Name>,
        on_conflict: Option<ConflictAction>,
    },
    Unique {
        span: Span,
        name: Option<Name>,
        on_conflict: Option<ConflictAction>,
    },
    Check(CheckConstraint),
    Default {
        span: Span,
        name: Option<Name>,
        form: DefaultForm,
        expr: Expr,
        text: String,
        text_span: Span,
    },
    Collate {
        span: Span,
        name: Option<Name>,
        collation: Name,
    },
    References {
        span: Span,
        name: Option<Name>,
        clause: ForeignKeyClause,
    },
    Deferrable {
        span: Span,
        name: Option<Name>,
        clause: DeferrableClause,
    },
    Generated {
        span: Span,
        name: Option<Name>,
        expr: Expr,
        storage_token: Option<Name>,
        storage: Option<GeneratedStorage>,
        always: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub span: Span,
    pub name: Name,
    pub type_name: Option<TypeName>,
    pub declared_type: String,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableConstraint {
    PrimaryKey {
        span: Span,
        name: Option<Name>,
        columns: Vec<OrderingTerm>,
        autoincrement: bool,
        on_conflict: Option<ConflictAction>,
    },
    Unique {
        span: Span,
        name: Option<Name>,
        columns: Vec<OrderingTerm>,
        on_conflict: Option<ConflictAction>,
    },
    Check(CheckConstraint),
    ForeignKey {
        span: Span,
        name: Option<Name>,
        columns: Vec<IdListItem>,
        clause: ForeignKeyClause,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableOptionKind {
    WithoutRowid,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableOption {
    pub span: Span,
    pub option: TableOptionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStmt {
    pub span: Span,
    pub temp: bool,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    pub options: Vec<TableOption>,
    pub without_rowid: bool,
    pub strict: bool,
    pub as_select: Option<Box<SelectStmt>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexStmt {
    pub span: Span,
    pub unique: bool,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub table: Name,
    pub columns: Vec<OrderingTerm>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateViewStmt {
    pub span: Span,
    pub temp: bool,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub columns: Option<Vec<IdListItem>>,
    pub select: Box<SelectStmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerTime {
    Before,
    After,
    InsteadOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerEventKind {
    Delete,
    Insert,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerStep {
    Insert(Box<InsertStmt>),
    Update(Box<UpdateStmt>),
    Delete(Box<DeleteStmt>),
    Select(Box<SelectStmt>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTriggerStmt {
    pub span: Span,
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
    pub body: Vec<TriggerStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtabArg {
    pub span: Span,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateVirtualTableStmt {
    pub span: Span,
    pub if_not_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
    pub module: Name,
    pub args: Option<Vec<VtabArg>>,
}

// ───────────────────────────── DML ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStmt {
    pub span: Span,
    pub with: Option<WithClause>,
    pub or_conflict: Option<ConflictAction>,
    pub replace_into: bool,
    pub table: FromTable,
    pub columns: Option<Vec<Name>>,
    pub select: Option<Box<SelectStmt>>,
    pub upsert: Vec<UpsertClause>,
    pub returning: Option<Vec<ResultColumn>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpsertAction {
    Nothing,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertClause {
    pub span: Span,
    pub target: Option<Vec<OrderingTerm>>,
    pub target_where: Option<Expr>,
    pub action: UpsertAction,
    pub set: Option<Vec<SetClause>>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetClause {
    pub span: Span,
    pub columns: Vec<Name>,
    pub parenthesized: bool,
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStmt {
    pub span: Span,
    pub with: Option<WithClause>,
    pub or_conflict: Option<ConflictAction>,
    pub table: FromTable,
    pub set: Vec<SetClause>,
    pub from: Option<FromSource>,
    pub where_clause: Option<Expr>,
    pub returning: Option<Vec<ResultColumn>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStmt {
    pub span: Span,
    pub with: Option<WithClause>,
    pub table: FromTable,
    pub where_clause: Option<Expr>,
    pub returning: Option<Vec<ResultColumn>>,
}

// ───────────────────────────── other statements ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropObjectType {
    Table,
    View,
    Index,
    Trigger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropStmt {
    pub span: Span,
    pub object_type: DropObjectType,
    pub if_exists: bool,
    pub schema: Option<Name>,
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterTableStmt {
    pub span: Span,
    pub schema: Option<Name>,
    pub table: Name,
    pub action: AlterTableAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    RenameTable {
        span: Span,
        new_name: Name,
    },
    RenameColumn {
        span: Span,
        column: Name,
        new_name: Name,
    },
    AddColumn {
        span: Span,
        column: ColumnDef,
    },
    DropColumn {
        span: Span,
        column: Name,
    },
    DropConstraint {
        span: Span,
        name: Name,
    },
    DropNotNull {
        span: Span,
        column: Name,
    },
    SetNotNull {
        span: Span,
        column: Name,
        on_conflict: Option<ConflictAction>,
    },
    AddCheck {
        span: Span,
        name: Option<Name>,
        expr: Expr,
        text: String,
        on_conflict: Option<ConflictAction>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionMode {
    Deferred,
    Immediate,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PragmaValueForm {
    Eq,
    Call,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaValue {
    pub span: Span,
    pub text: String,
    pub form: PragmaValueForm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaStmt {
    pub span: Span,
    pub schema: Option<Name>,
    pub name: Name,
    pub value: Option<PragmaValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    CreateTable(Box<CreateTableStmt>),
    CreateIndex(Box<CreateIndexStmt>),
    CreateView(Box<CreateViewStmt>),
    CreateTrigger(Box<CreateTriggerStmt>),
    CreateVirtualTable(Box<CreateVirtualTableStmt>),
    Select(Box<SelectStmt>),
    Insert(Box<InsertStmt>),
    Update(Box<UpdateStmt>),
    Delete(Box<DeleteStmt>),
    Drop(DropStmt),
    AlterTable(Box<AlterTableStmt>),
    Begin {
        span: Span,
        mode: Option<TransactionMode>,
    },
    Commit {
        span: Span,
    },
    Rollback {
        span: Span,
        savepoint: Option<Name>,
    },
    Savepoint {
        span: Span,
        name: Name,
    },
    Release {
        span: Span,
        name: Name,
    },
    Pragma(PragmaStmt),
    Attach {
        span: Span,
        file: Box<Expr>,
        schema: Box<Expr>,
        key: Option<Box<Expr>>,
    },
    Detach {
        span: Span,
        schema: Box<Expr>,
    },
    Analyze {
        span: Span,
        schema: Option<Name>,
        name: Option<Name>,
    },
    Reindex {
        span: Span,
        schema: Option<Name>,
        name: Option<Name>,
    },
    Vacuum {
        span: Span,
        schema: Option<Name>,
        into: Option<Box<Expr>>,
    },
    Explain {
        span: Span,
        query_plan: bool,
        statement: Box<Statement>,
    },
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::CreateTable(s) => s.span,
            Statement::CreateIndex(s) => s.span,
            Statement::CreateView(s) => s.span,
            Statement::CreateTrigger(s) => s.span,
            Statement::CreateVirtualTable(s) => s.span,
            Statement::Select(s) => s.span,
            Statement::Insert(s) => s.span,
            Statement::Update(s) => s.span,
            Statement::Delete(s) => s.span,
            Statement::Drop(s) => s.span,
            Statement::AlterTable(s) => s.span,
            Statement::Begin { span, .. }
            | Statement::Commit { span }
            | Statement::Rollback { span, .. }
            | Statement::Savepoint { span, .. }
            | Statement::Release { span, .. }
            | Statement::Attach { span, .. }
            | Statement::Detach { span, .. }
            | Statement::Analyze { span, .. }
            | Statement::Reindex { span, .. }
            | Statement::Vacuum { span, .. }
            | Statement::Explain { span, .. } => *span,
            Statement::Pragma(p) => p.span,
        }
    }
}
