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
#[inline]
pub fn keyword_code(bytes: &[u8]) -> u16 {
    let n = bytes.len();
    if !(2..=17).contains(&n) {
        return tk::ID;
    }
    match (n, bytes[0] | 0x20) {
        (2, b'a') if bytes[1] | 0x20 == b's' => tk::AS,
        (2, b'b') if bytes[1] | 0x20 == b'y' => tk::BY,
        (2, b'd') if bytes[1] | 0x20 == b'o' => tk::DO,
        (2, b'i') => match bytes[1] | 0x20 {
            b'f' => tk::IF,
            b'n' => tk::IN,
            b's' => tk::IS,
            _ => tk::ID,
        },
        (2, b'n') if bytes[1] | 0x20 == b'o' => tk::NO,
        (2, b'o') => match bytes[1] | 0x20 {
            b'f' => tk::OF,
            b'n' => tk::ON,
            b'r' => tk::OR,
            _ => tk::ID,
        },
        (2, b't') if bytes[1] | 0x20 == b'o' => tk::TO,

        (3, b'a') => {
            if bytes.eq_ignore_ascii_case(b"and") {
                tk::AND
            } else if bytes.eq_ignore_ascii_case(b"all") {
                tk::ALL
            } else if bytes.eq_ignore_ascii_case(b"asc") {
                tk::ASC
            } else if bytes.eq_ignore_ascii_case(b"add") {
                tk::ADD
            } else {
                tk::ID
            }
        }
        (3, b'e') if bytes.eq_ignore_ascii_case(b"end") => tk::END,
        (3, b'f') if bytes.eq_ignore_ascii_case(b"for") => tk::FOR,
        (3, b'k') if bytes.eq_ignore_ascii_case(b"key") => tk::KEY,
        (3, b'n') if bytes.eq_ignore_ascii_case(b"not") => tk::NOT,
        (3, b'r') if bytes.eq_ignore_ascii_case(b"row") => tk::ROW,
        (3, b's') if bytes.eq_ignore_ascii_case(b"set") => tk::SET,

        (4, b'c') => {
            if bytes.eq_ignore_ascii_case(b"case") {
                tk::CASE
            } else if bytes.eq_ignore_ascii_case(b"cast") {
                tk::CAST
            } else {
                tk::ID
            }
        }
        (4, b'd') => {
            if bytes.eq_ignore_ascii_case(b"desc") {
                tk::DESC
            } else if bytes.eq_ignore_ascii_case(b"drop") {
                tk::DROP
            } else {
                tk::ID
            }
        }
        (4, b'e') => {
            if bytes.eq_ignore_ascii_case(b"else") {
                tk::ELSE
            } else if bytes.eq_ignore_ascii_case(b"each") {
                tk::EACH
            } else {
                tk::ID
            }
        }
        (4, b'f') => {
            if bytes.eq_ignore_ascii_case(b"from") {
                tk::FROM
            } else if bytes.eq_ignore_ascii_case(b"full") {
                tk::JOIN_KW
            } else if bytes.eq_ignore_ascii_case(b"fail") {
                tk::FAIL
            } else {
                tk::ID
            }
        }
        (4, b'g') if bytes.eq_ignore_ascii_case(b"glob") => tk::LIKE_KW,
        (4, b'i') if bytes.eq_ignore_ascii_case(b"into") => tk::INTO,
        (4, b'j') if bytes.eq_ignore_ascii_case(b"join") => tk::JOIN,
        (4, b'l') => {
            if bytes.eq_ignore_ascii_case(b"left") {
                tk::JOIN_KW
            } else if bytes.eq_ignore_ascii_case(b"like") {
                tk::LIKE_KW
            } else if bytes.eq_ignore_ascii_case(b"last") {
                tk::LAST
            } else {
                tk::ID
            }
        }
        (4, b'n') if bytes.eq_ignore_ascii_case(b"null") => tk::NULL,
        (4, b'o') if bytes.eq_ignore_ascii_case(b"over") => tk::OVER,
        (4, b'p') if bytes.eq_ignore_ascii_case(b"plan") => tk::PLAN,
        (4, b'r') if bytes.eq_ignore_ascii_case(b"rows") => tk::ROWS,
        (4, b't') => {
            if bytes.eq_ignore_ascii_case(b"then") {
                tk::THEN
            } else if bytes.eq_ignore_ascii_case(b"temp") {
                tk::TEMP
            } else if bytes.eq_ignore_ascii_case(b"ties") {
                tk::TIES
            } else {
                tk::ID
            }
        }
        (4, b'v') if bytes.eq_ignore_ascii_case(b"view") => tk::VIEW,
        (4, b'w') => {
            if bytes.eq_ignore_ascii_case(b"with") {
                tk::WITH
            } else if bytes.eq_ignore_ascii_case(b"when") {
                tk::WHEN
            } else {
                tk::ID
            }
        }

        (5, b'a') => {
            if bytes.eq_ignore_ascii_case(b"alter") {
                tk::ALTER
            } else if bytes.eq_ignore_ascii_case(b"after") {
                tk::AFTER
            } else if bytes.eq_ignore_ascii_case(b"abort") {
                tk::ABORT
            } else {
                tk::ID
            }
        }
        (5, b'b') if bytes.eq_ignore_ascii_case(b"begin") => tk::BEGIN,
        (5, b'c') => {
            if bytes.eq_ignore_ascii_case(b"check") {
                tk::CHECK
            } else if bytes.eq_ignore_ascii_case(b"cross") {
                tk::JOIN_KW
            } else {
                tk::ID
            }
        }
        (5, b'f') if bytes.eq_ignore_ascii_case(b"first") => tk::FIRST,
        (5, b'g') if bytes.eq_ignore_ascii_case(b"group") => tk::GROUP,
        (5, b'i') => {
            if bytes.eq_ignore_ascii_case(b"index") {
                tk::INDEX
            } else if bytes.eq_ignore_ascii_case(b"inner") {
                tk::JOIN_KW
            } else {
                tk::ID
            }
        }
        (5, b'l') if bytes.eq_ignore_ascii_case(b"limit") => tk::LIMIT,
        (5, b'm') if bytes.eq_ignore_ascii_case(b"match") => tk::MATCH,
        (5, b'n') if bytes.eq_ignore_ascii_case(b"nulls") => tk::NULLS,
        (5, b'o') => {
            if bytes.eq_ignore_ascii_case(b"order") {
                tk::ORDER
            } else if bytes.eq_ignore_ascii_case(b"outer") {
                tk::JOIN_KW
            } else {
                tk::ID
            }
        }
        (5, b'q') if bytes.eq_ignore_ascii_case(b"query") => tk::QUERY,
        (5, b'r') => {
            if bytes.eq_ignore_ascii_case(b"right") {
                tk::JOIN_KW
            } else if bytes.eq_ignore_ascii_case(b"range") {
                tk::RANGE
            } else if bytes.eq_ignore_ascii_case(b"raise") {
                tk::RAISE
            } else {
                tk::ID
            }
        }
        (5, b't') if bytes.eq_ignore_ascii_case(b"table") => tk::TABLE,
        (5, b'u') => {
            if bytes.eq_ignore_ascii_case(b"union") {
                tk::UNION
            } else if bytes.eq_ignore_ascii_case(b"using") {
                tk::USING
            } else {
                tk::ID
            }
        }
        (5, b'w') if bytes.eq_ignore_ascii_case(b"where") => tk::WHERE,

        (6, b'a') => {
            if bytes.eq_ignore_ascii_case(b"action") {
                tk::ACTION
            } else if bytes.eq_ignore_ascii_case(b"always") {
                tk::ALWAYS
            } else if bytes.eq_ignore_ascii_case(b"attach") {
                tk::ATTACH
            } else {
                tk::ID
            }
        }
        (6, b'b') if bytes.eq_ignore_ascii_case(b"before") => tk::BEFORE,
        (6, b'c') => {
            if bytes.eq_ignore_ascii_case(b"create") {
                tk::CREATE
            } else if bytes.eq_ignore_ascii_case(b"commit") {
                tk::COMMIT
            } else if bytes.eq_ignore_ascii_case(b"column") {
                tk::COLUMNKW
            } else {
                tk::ID
            }
        }
        (6, b'd') => {
            if bytes.eq_ignore_ascii_case(b"delete") {
                tk::DELETE
            } else if bytes.eq_ignore_ascii_case(b"detach") {
                tk::DETACH
            } else {
                tk::ID
            }
        }
        (6, b'e') => {
            if bytes.eq_ignore_ascii_case(b"exists") {
                tk::EXISTS
            } else if bytes.eq_ignore_ascii_case(b"except") {
                tk::EXCEPT
            } else if bytes.eq_ignore_ascii_case(b"escape") {
                tk::ESCAPE
            } else {
                tk::ID
            }
        }
        (6, b'f') if bytes.eq_ignore_ascii_case(b"filter") => tk::FILTER,
        (6, b'g') if bytes.eq_ignore_ascii_case(b"groups") => tk::GROUPS,
        (6, b'h') if bytes.eq_ignore_ascii_case(b"having") => tk::HAVING,
        (6, b'i') => {
            if bytes.eq_ignore_ascii_case(b"insert") {
                tk::INSERT
            } else if bytes.eq_ignore_ascii_case(b"ignore") {
                tk::IGNORE
            } else if bytes.eq_ignore_ascii_case(b"isnull") {
                tk::ISNULL
            } else {
                tk::ID
            }
        }
        (6, b'o') => {
            if bytes.eq_ignore_ascii_case(b"offset") {
                tk::OFFSET
            } else if bytes.eq_ignore_ascii_case(b"others") {
                tk::OTHERS
            } else {
                tk::ID
            }
        }
        (6, b'p') if bytes.eq_ignore_ascii_case(b"pragma") => tk::PRAGMA,
        (6, b'r') => {
            if bytes.eq_ignore_ascii_case(b"rename") {
                tk::RENAME
            } else if bytes.eq_ignore_ascii_case(b"regexp") {
                tk::LIKE_KW
            } else {
                tk::ID
            }
        }
        (6, b's') if bytes.eq_ignore_ascii_case(b"select") => tk::SELECT,
        (6, b'u') => {
            if bytes.eq_ignore_ascii_case(b"update") {
                tk::UPDATE
            } else if bytes.eq_ignore_ascii_case(b"unique") {
                tk::UNIQUE
            } else {
                tk::ID
            }
        }
        (6, b'v') => {
            if bytes.eq_ignore_ascii_case(b"values") {
                tk::VALUES
            } else if bytes.eq_ignore_ascii_case(b"vacuum") {
                tk::VACUUM
            } else {
                tk::ID
            }
        }
        (6, b'w') if bytes.eq_ignore_ascii_case(b"window") => tk::WINDOW,

        (7, b'a') if bytes.eq_ignore_ascii_case(b"analyze") => tk::ANALYZE,
        (7, b'b') if bytes.eq_ignore_ascii_case(b"between") => tk::BETWEEN,
        (7, b'c') => {
            if bytes.eq_ignore_ascii_case(b"collate") {
                tk::COLLATE
            } else if bytes.eq_ignore_ascii_case(b"current") {
                tk::CURRENT
            } else if bytes.eq_ignore_ascii_case(b"cascade") {
                tk::CASCADE
            } else {
                tk::ID
            }
        }
        (7, b'd') if bytes.eq_ignore_ascii_case(b"default") => tk::DEFAULT,
        (7, b'e') => {
            if bytes.eq_ignore_ascii_case(b"explain") {
                tk::EXPLAIN
            } else if bytes.eq_ignore_ascii_case(b"exclude") {
                tk::EXCLUDE
            } else {
                tk::ID
            }
        }
        (7, b'f') if bytes.eq_ignore_ascii_case(b"foreign") => tk::FOREIGN,
        (7, b'i') => {
            if bytes.eq_ignore_ascii_case(b"indexed") {
                tk::INDEXED
            } else if bytes.eq_ignore_ascii_case(b"instead") {
                tk::INSTEAD
            } else {
                tk::ID
            }
        }
        (7, b'n') => {
            if bytes.eq_ignore_ascii_case(b"notnull") {
                tk::NOTNULL
            } else if bytes.eq_ignore_ascii_case(b"nothing") {
                tk::NOTHING
            } else if bytes.eq_ignore_ascii_case(b"natural") {
                tk::JOIN_KW
            } else {
                tk::ID
            }
        }
        (7, b'p') if bytes.eq_ignore_ascii_case(b"primary") => tk::PRIMARY,
        (7, b'r') => {
            if bytes.eq_ignore_ascii_case(b"replace") {
                tk::REPLACE
            } else if bytes.eq_ignore_ascii_case(b"release") {
                tk::RELEASE
            } else if bytes.eq_ignore_ascii_case(b"reindex") {
                tk::REINDEX
            } else {
                tk::ID
            }
        }
        (7, b't') if bytes.eq_ignore_ascii_case(b"trigger") => tk::TRIGGER,
        (7, b'v') if bytes.eq_ignore_ascii_case(b"virtual") => tk::VIRTUAL,
        (7, b'w') if bytes.eq_ignore_ascii_case(b"without") => tk::WITHOUT,

        (8, b'c') if bytes.eq_ignore_ascii_case(b"conflict") => tk::CONFLICT,
        (8, b'd') => {
            if bytes.eq_ignore_ascii_case(b"distinct") {
                tk::DISTINCT
            } else if bytes.eq_ignore_ascii_case(b"database") {
                tk::DATABASE
            } else if bytes.eq_ignore_ascii_case(b"deferred") {
                tk::DEFERRED
            } else {
                tk::ID
            }
        }
        (8, b'r') => {
            if bytes.eq_ignore_ascii_case(b"rollback") {
                tk::ROLLBACK
            } else if bytes.eq_ignore_ascii_case(b"restrict") {
                tk::RESTRICT
            } else {
                tk::ID
            }
        }

        (9, b'e') if bytes.eq_ignore_ascii_case(b"exclusive") => tk::EXCLUSIVE,
        (9, b'f') if bytes.eq_ignore_ascii_case(b"following") => tk::FOLLOWING,
        (9, b'g') if bytes.eq_ignore_ascii_case(b"generated") => tk::GENERATED,
        (9, b'i') => {
            if bytes.eq_ignore_ascii_case(b"intersect") {
                tk::INTERSECT
            } else if bytes.eq_ignore_ascii_case(b"immediate") {
                tk::IMMEDIATE
            } else if bytes.eq_ignore_ascii_case(b"initially") {
                tk::INITIALLY
            } else {
                tk::ID
            }
        }
        (9, b'p') => {
            if bytes.eq_ignore_ascii_case(b"partition") {
                tk::PARTITION
            } else if bytes.eq_ignore_ascii_case(b"preceding") {
                tk::PRECEDING
            } else {
                tk::ID
            }
        }
        (9, b'r') => {
            if bytes.eq_ignore_ascii_case(b"returning") {
                tk::RETURNING
            } else if bytes.eq_ignore_ascii_case(b"recursive") {
                tk::RECURSIVE
            } else {
                tk::ID
            }
        }
        (9, b's') if bytes.eq_ignore_ascii_case(b"savepoint") => tk::SAVEPOINT,
        (9, b't') if bytes.eq_ignore_ascii_case(b"temporary") => tk::TEMP,
        (9, b'u') if bytes.eq_ignore_ascii_case(b"unbounded") => tk::UNBOUNDED,

        (10, b'c') if bytes.eq_ignore_ascii_case(b"constraint") => tk::CONSTRAINT,
        (10, b'd') if bytes.eq_ignore_ascii_case(b"deferrable") => tk::DEFERRABLE,
        (10, b'r') if bytes.eq_ignore_ascii_case(b"references") => tk::REFERENCES,

        (11, b't') if bytes.eq_ignore_ascii_case(b"transaction") => tk::TRANSACTION,

        (12, b'c')
            if bytes.eq_ignore_ascii_case(b"current_date")
                || bytes.eq_ignore_ascii_case(b"current_time") =>
        {
            tk::CTIME_KW
        }
        (12, b'm') if bytes.eq_ignore_ascii_case(b"materialized") => tk::MATERIALIZED,

        (13, b'a') if bytes.eq_ignore_ascii_case(b"autoincrement") => tk::AUTOINCR,

        (17, b'c') if bytes.eq_ignore_ascii_case(b"current_timestamp") => tk::CTIME_KW,

        _ => tk::ID,
    }
}
