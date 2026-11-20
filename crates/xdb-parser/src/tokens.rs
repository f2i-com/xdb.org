//! Token codes and the keyword table, ported from SQLite 3.54.1
//! (`tool/mkkeywordhash.c`, `src/parse.y` `%token` / `%fallback`).
//!
//! The numeric values are Lemon's terminal codes for `parse.y`, so tokens index
//! the static LALR(1) parser tables directly. `DCOLON` exists only in the XDB
//! dialect's grammar.

/// Token codes (`TK_*` in SQLite's generated `parse.h`).
#[allow(non_snake_case)]
pub mod tk {
    pub const EOF: u16 = 0;
    pub const SEMI: u16 = 1;
    pub const EXPLAIN: u16 = 2;
    pub const QUERY: u16 = 3;
    pub const PLAN: u16 = 4;
    pub const BEGIN: u16 = 5;
    pub const TRANSACTION: u16 = 6;
    pub const DEFERRED: u16 = 7;
    pub const IMMEDIATE: u16 = 8;
    pub const EXCLUSIVE: u16 = 9;
    pub const COMMIT: u16 = 10;
    pub const END: u16 = 11;
    pub const ROLLBACK: u16 = 12;
    pub const SAVEPOINT: u16 = 13;
    pub const RELEASE: u16 = 14;
    pub const TO: u16 = 15;
    pub const TABLE: u16 = 16;
    pub const CREATE: u16 = 17;
    pub const IF: u16 = 18;
    pub const NOT: u16 = 19;
    pub const EXISTS: u16 = 20;
    pub const TEMP: u16 = 21;
    pub const LP: u16 = 22;
    pub const RP: u16 = 23;
    pub const AS: u16 = 24;
    pub const COMMA: u16 = 25;
    pub const WITHOUT: u16 = 26;
    pub const ABORT: u16 = 27;
    pub const ACTION: u16 = 28;
    pub const AFTER: u16 = 29;
    pub const ANALYZE: u16 = 30;
    pub const ASC: u16 = 31;
    pub const ATTACH: u16 = 32;
    pub const BEFORE: u16 = 33;
    pub const BY: u16 = 34;
    pub const CASCADE: u16 = 35;
    pub const CAST: u16 = 36;
    pub const CONFLICT: u16 = 37;
    pub const DATABASE: u16 = 38;
    pub const DESC: u16 = 39;
    pub const DETACH: u16 = 40;
    pub const EACH: u16 = 41;
    pub const FAIL: u16 = 42;
    pub const OR: u16 = 43;
    pub const AND: u16 = 44;
    pub const IS: u16 = 45;
    pub const ISNOT: u16 = 46;
    pub const MATCH: u16 = 47;
    pub const LIKE_KW: u16 = 48;
    pub const BETWEEN: u16 = 49;
    pub const IN: u16 = 50;
    pub const ISNULL: u16 = 51;
    pub const NOTNULL: u16 = 52;
    pub const NE: u16 = 53;
    pub const EQ: u16 = 54;
    pub const GT: u16 = 55;
    pub const LE: u16 = 56;
    pub const LT: u16 = 57;
    pub const GE: u16 = 58;
    pub const ESCAPE: u16 = 59;
    pub const ID: u16 = 60;
    pub const COLUMNKW: u16 = 61;
    pub const DO: u16 = 62;
    pub const FOR: u16 = 63;
    pub const IGNORE: u16 = 64;
    pub const INITIALLY: u16 = 65;
    pub const INSTEAD: u16 = 66;
    pub const NO: u16 = 67;
    pub const KEY: u16 = 68;
    pub const OF: u16 = 69;
    pub const OFFSET: u16 = 70;
    pub const PRAGMA: u16 = 71;
    pub const RAISE: u16 = 72;
    pub const RECURSIVE: u16 = 73;
    pub const REPLACE: u16 = 74;
    pub const RESTRICT: u16 = 75;
    pub const ROW: u16 = 76;
    pub const ROWS: u16 = 77;
    pub const TRIGGER: u16 = 78;
    pub const VACUUM: u16 = 79;
    pub const VIEW: u16 = 80;
    pub const VIRTUAL: u16 = 81;
    pub const WITH: u16 = 82;
    pub const NULLS: u16 = 83;
    pub const FIRST: u16 = 84;
    pub const LAST: u16 = 85;
    pub const CURRENT: u16 = 86;
    pub const FOLLOWING: u16 = 87;
    pub const PARTITION: u16 = 88;
    pub const PRECEDING: u16 = 89;
    pub const RANGE: u16 = 90;
    pub const UNBOUNDED: u16 = 91;
    pub const EXCLUDE: u16 = 92;
    pub const GROUPS: u16 = 93;
    pub const OTHERS: u16 = 94;
    pub const TIES: u16 = 95;
    pub const GENERATED: u16 = 96;
    pub const ALWAYS: u16 = 97;
    pub const MATERIALIZED: u16 = 98;
    pub const REINDEX: u16 = 99;
    pub const RENAME: u16 = 100;
    pub const CTIME_KW: u16 = 101;
    pub const ANY: u16 = 102;
    pub const BITAND: u16 = 103;
    pub const BITOR: u16 = 104;
    pub const LSHIFT: u16 = 105;
    pub const RSHIFT: u16 = 106;
    pub const PLUS: u16 = 107;
    pub const MINUS: u16 = 108;
    pub const STAR: u16 = 109;
    pub const SLASH: u16 = 110;
    pub const REM: u16 = 111;
    pub const CONCAT: u16 = 112;
    pub const PTR: u16 = 113;
    pub const COLLATE: u16 = 114;
    pub const BITNOT: u16 = 115;
    pub const ON: u16 = 116;
    pub const INDEXED: u16 = 117;
    pub const STRING: u16 = 118;
    pub const JOIN_KW: u16 = 119;
    pub const CONSTRAINT: u16 = 120;
    pub const DEFAULT: u16 = 121;
    pub const NULL: u16 = 122;
    pub const PRIMARY: u16 = 123;
    pub const UNIQUE: u16 = 124;
    pub const CHECK: u16 = 125;
    pub const REFERENCES: u16 = 126;
    pub const AUTOINCR: u16 = 127;
    pub const INSERT: u16 = 128;
    pub const DELETE: u16 = 129;
    pub const UPDATE: u16 = 130;
    pub const SET: u16 = 131;
    pub const DEFERRABLE: u16 = 132;
    pub const FOREIGN: u16 = 133;
    pub const DROP: u16 = 134;
    pub const UNION: u16 = 135;
    pub const ALL: u16 = 136;
    pub const EXCEPT: u16 = 137;
    pub const INTERSECT: u16 = 138;
    pub const SELECT: u16 = 139;
    pub const VALUES: u16 = 140;
    pub const DISTINCT: u16 = 141;
    pub const DOT: u16 = 142;
    pub const FROM: u16 = 143;
    pub const JOIN: u16 = 144;
    pub const USING: u16 = 145;
    pub const ORDER: u16 = 146;
    pub const GROUP: u16 = 147;
    pub const HAVING: u16 = 148;
    pub const LIMIT: u16 = 149;
    pub const WHERE: u16 = 150;
    pub const RETURNING: u16 = 151;
    pub const INTO: u16 = 152;
    pub const NOTHING: u16 = 153;
    pub const FLOAT: u16 = 154;
    pub const BLOB: u16 = 155;
    pub const INTEGER: u16 = 156;
    pub const VARIABLE: u16 = 157;
    pub const CASE: u16 = 158;
    pub const WHEN: u16 = 159;
    pub const THEN: u16 = 160;
    pub const ELSE: u16 = 161;
    pub const INDEX: u16 = 162;
    pub const ALTER: u16 = 163;
    pub const ADD: u16 = 164;
    pub const WINDOW: u16 = 165;
    pub const OVER: u16 = 166;
    pub const FILTER: u16 = 167;
    pub const COLUMN: u16 = 168;
    pub const AGG_FUNCTION: u16 = 169;
    pub const AGG_COLUMN: u16 = 170;
    pub const TRUEFALSE: u16 = 171;
    pub const FUNCTION: u16 = 172;
    pub const UPLUS: u16 = 173;
    pub const UMINUS: u16 = 174;
    pub const TRUTH: u16 = 175;
    pub const REGISTER: u16 = 176;
    pub const VECTOR: u16 = 177;
    pub const SELECT_COLUMN: u16 = 178;
    pub const IF_NULL_ROW: u16 = 179;
    pub const ASTERISK: u16 = 180;
    pub const SPAN: u16 = 181;
    pub const ERROR: u16 = 182;
    pub const QNUMBER: u16 = 183;
    pub const SPACE: u16 = 184;
    pub const COMMENT: u16 = 185;
    pub const ILLEGAL: u16 = 186;
    /// XDB dialect only: the `::` cast operator.
    pub const DCOLON: u16 = 187;
}

/// Number of token codes (one past the largest).
pub const TK_COUNT: usize = 188;

/// `parse.y` `%fallback ID ...`: keywords that fall back to `TK_ID`.
pub const FALLBACK_TO_ID: &[u16] = &[
    tk::ABORT,
    tk::ACTION,
    tk::AFTER,
    tk::ANALYZE,
    tk::ASC,
    tk::ATTACH,
    tk::BEFORE,
    tk::BEGIN,
    tk::BY,
    tk::CASCADE,
    tk::CAST,
    tk::COLUMNKW,
    tk::CONFLICT,
    tk::DATABASE,
    tk::DEFERRED,
    tk::DESC,
    tk::DETACH,
    tk::DO,
    tk::EACH,
    tk::END,
    tk::EXCLUSIVE,
    tk::EXPLAIN,
    tk::FAIL,
    tk::FOR,
    tk::IGNORE,
    tk::IMMEDIATE,
    tk::INITIALLY,
    tk::INSTEAD,
    tk::LIKE_KW,
    tk::MATCH,
    tk::NO,
    tk::PLAN,
    tk::QUERY,
    tk::KEY,
    tk::OF,
    tk::OFFSET,
    tk::PRAGMA,
    tk::RAISE,
    tk::RECURSIVE,
    tk::RELEASE,
    tk::REPLACE,
    tk::RESTRICT,
    tk::ROW,
    tk::ROWS,
    tk::ROLLBACK,
    tk::SAVEPOINT,
    tk::TEMP,
    tk::TRIGGER,
    tk::VACUUM,
    tk::VIEW,
    tk::VIRTUAL,
    tk::WITH,
    tk::WITHOUT,
    tk::NULLS,
    tk::FIRST,
    tk::LAST,
    tk::CURRENT,
    tk::FOLLOWING,
    tk::PARTITION,
    tk::PRECEDING,
    tk::RANGE,
    tk::UNBOUNDED,
    tk::EXCLUDE,
    tk::GROUPS,
    tk::OTHERS,
    tk::TIES,
    tk::GENERATED,
    tk::ALWAYS,
    tk::MATERIALIZED,
    tk::REINDEX,
    tk::RENAME,
    tk::CTIME_KW,
    tk::IF,
];

const FALLBACK_MASK: [bool; TK_COUNT] = {
    let mut mask = [false; TK_COUNT];
    let mut i = 0;
    while i < FALLBACK_TO_ID.len() {
        mask[FALLBACK_TO_ID[i] as usize] = true;
        i += 1;
    }
    mask
};

/// `sqlite3ParserFallback(t) == TK_ID`.
#[inline]
pub const fn falls_back_to_id(t: u16) -> bool {
    let idx = t as usize;
    idx < TK_COUNT && FALLBACK_MASK[idx]
}

/// `keywordCode` (`keywordhash.h`): the keyword token for `bytes`, or `tk::ID`.
/// Matching is ASCII case-insensitive.
pub fn keyword_code(bytes: &[u8]) -> u16 {
    let n = bytes.len();
    if !(2..=17).contains(&n) {
        return tk::ID;
    }
    let mut lower = [0u8; 17];
    let mut i = 0;
    while i < n {
        lower[i] = bytes[i].to_ascii_lowercase();
        i += 1;
    }
    match &lower[..n] {
        b"abort" => tk::ABORT,
        b"action" => tk::ACTION,
        b"add" => tk::ADD,
        b"after" => tk::AFTER,
        b"all" => tk::ALL,
        b"alter" => tk::ALTER,
        b"always" => tk::ALWAYS,
        b"analyze" => tk::ANALYZE,
        b"and" => tk::AND,
        b"as" => tk::AS,
        b"asc" => tk::ASC,
        b"attach" => tk::ATTACH,
        b"autoincrement" => tk::AUTOINCR,
        b"before" => tk::BEFORE,
        b"begin" => tk::BEGIN,
        b"between" => tk::BETWEEN,
        b"by" => tk::BY,
        b"cascade" => tk::CASCADE,
        b"case" => tk::CASE,
        b"cast" => tk::CAST,
        b"check" => tk::CHECK,
        b"collate" => tk::COLLATE,
        b"column" => tk::COLUMNKW,
        b"commit" => tk::COMMIT,
        b"conflict" => tk::CONFLICT,
        b"constraint" => tk::CONSTRAINT,
        b"create" => tk::CREATE,
        b"cross" => tk::JOIN_KW,
        b"current" => tk::CURRENT,
        b"current_date" => tk::CTIME_KW,
        b"current_time" => tk::CTIME_KW,
        b"current_timestamp" => tk::CTIME_KW,
        b"database" => tk::DATABASE,
        b"default" => tk::DEFAULT,
        b"deferred" => tk::DEFERRED,
        b"deferrable" => tk::DEFERRABLE,
        b"delete" => tk::DELETE,
        b"desc" => tk::DESC,
        b"detach" => tk::DETACH,
        b"distinct" => tk::DISTINCT,
        b"do" => tk::DO,
        b"drop" => tk::DROP,
        b"end" => tk::END,
        b"each" => tk::EACH,
        b"else" => tk::ELSE,
        b"escape" => tk::ESCAPE,
        b"except" => tk::EXCEPT,
        b"exclusive" => tk::EXCLUSIVE,
        b"exclude" => tk::EXCLUDE,
        b"exists" => tk::EXISTS,
        b"explain" => tk::EXPLAIN,
        b"fail" => tk::FAIL,
        b"filter" => tk::FILTER,
        b"first" => tk::FIRST,
        b"following" => tk::FOLLOWING,
        b"for" => tk::FOR,
        b"foreign" => tk::FOREIGN,
        b"from" => tk::FROM,
        b"full" => tk::JOIN_KW,
        b"generated" => tk::GENERATED,
        b"glob" => tk::LIKE_KW,
        b"group" => tk::GROUP,
        b"groups" => tk::GROUPS,
        b"having" => tk::HAVING,
        b"if" => tk::IF,
        b"ignore" => tk::IGNORE,
        b"immediate" => tk::IMMEDIATE,
        b"in" => tk::IN,
        b"index" => tk::INDEX,
        b"indexed" => tk::INDEXED,
        b"initially" => tk::INITIALLY,
        b"inner" => tk::JOIN_KW,
        b"insert" => tk::INSERT,
        b"instead" => tk::INSTEAD,
        b"intersect" => tk::INTERSECT,
        b"into" => tk::INTO,
        b"is" => tk::IS,
        b"isnull" => tk::ISNULL,
        b"join" => tk::JOIN,
        b"key" => tk::KEY,
        b"last" => tk::LAST,
        b"left" => tk::JOIN_KW,
        b"like" => tk::LIKE_KW,
        b"limit" => tk::LIMIT,
        b"match" => tk::MATCH,
        b"materialized" => tk::MATERIALIZED,
        b"natural" => tk::JOIN_KW,
        b"no" => tk::NO,
        b"not" => tk::NOT,
        b"nothing" => tk::NOTHING,
        b"notnull" => tk::NOTNULL,
        b"null" => tk::NULL,
        b"nulls" => tk::NULLS,
        b"of" => tk::OF,
        b"offset" => tk::OFFSET,
        b"on" => tk::ON,
        b"or" => tk::OR,
        b"order" => tk::ORDER,
        b"others" => tk::OTHERS,
        b"outer" => tk::JOIN_KW,
        b"over" => tk::OVER,
        b"partition" => tk::PARTITION,
        b"plan" => tk::PLAN,
        b"pragma" => tk::PRAGMA,
        b"preceding" => tk::PRECEDING,
        b"primary" => tk::PRIMARY,
        b"query" => tk::QUERY,
        b"raise" => tk::RAISE,
        b"range" => tk::RANGE,
        b"recursive" => tk::RECURSIVE,
        b"references" => tk::REFERENCES,
        b"regexp" => tk::LIKE_KW,
        b"reindex" => tk::REINDEX,
        b"release" => tk::RELEASE,
        b"rename" => tk::RENAME,
        b"replace" => tk::REPLACE,
        b"restrict" => tk::RESTRICT,
        b"returning" => tk::RETURNING,
        b"right" => tk::JOIN_KW,
        b"rollback" => tk::ROLLBACK,
        b"row" => tk::ROW,
        b"rows" => tk::ROWS,
        b"savepoint" => tk::SAVEPOINT,
        b"select" => tk::SELECT,
        b"set" => tk::SET,
        b"table" => tk::TABLE,
        b"temp" | b"temporary" => tk::TEMP,
        b"then" => tk::THEN,
        b"ties" => tk::TIES,
        b"to" => tk::TO,
        b"transaction" => tk::TRANSACTION,
        b"trigger" => tk::TRIGGER,
        b"unbounded" => tk::UNBOUNDED,
        b"union" => tk::UNION,
        b"unique" => tk::UNIQUE,
        b"update" => tk::UPDATE,
        b"using" => tk::USING,
        b"vacuum" => tk::VACUUM,
        b"values" => tk::VALUES,
        b"view" => tk::VIEW,
        b"virtual" => tk::VIRTUAL,
        b"when" => tk::WHEN,
        b"where" => tk::WHERE,
        b"window" => tk::WINDOW,
        b"with" => tk::WITH,
        b"without" => tk::WITHOUT,
        _ => tk::ID,
    }
}
