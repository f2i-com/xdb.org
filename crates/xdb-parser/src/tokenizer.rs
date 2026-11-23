//! The SQL tokenizer: an exact port of `sqlite3GetToken` (`tokenize.c:273`) over
//! UTF-8 bytes, plus the context rules `sqlite3RunParser` applies to `WINDOW`,
//! `OVER`, and `FILTER` (`tokenize.c:196-246`), XDB's `ILIKE` / `::` rules, and
//! a statement splitter modelled on `sqlite3_complete` (`complete.c`).
//!
//! Because Rust `&str` is UTF-8 and every byte `>= 0x80` is an identifier character
//! (`IdChar`) except the 3-byte UTF-8 BOM (`0xEF 0xBB 0xBF`) at token start
//! (`CC_BOM`), every emitted `[start, end)` byte span lies on a valid UTF-8
//! character boundary and matches SQLite's C tokenizer byte-for-byte.

use crate::tokens::{falls_back_to_id, keyword_code, tk};

// Character classes (tokenize.c:30-58).
const CC_X: u8 = 0;
const CC_KYWD0: u8 = 1;
const CC_KYWD: u8 = 2;
const CC_DIGIT: u8 = 3;
const CC_DOLLAR: u8 = 4;
const CC_VARALPHA: u8 = 5;
const CC_VARNUM: u8 = 6;
const CC_SPACE: u8 = 7;
const CC_QUOTE: u8 = 8;
const CC_QUOTE2: u8 = 9;
const CC_PIPE: u8 = 10;
const CC_MINUS: u8 = 11;
const CC_LT: u8 = 12;
const CC_GT: u8 = 13;
const CC_EQ: u8 = 14;
const CC_BANG: u8 = 15;
const CC_SLASH: u8 = 16;
const CC_LP: u8 = 17;
const CC_RP: u8 = 18;
const CC_SEMI: u8 = 19;
const CC_PLUS: u8 = 20;
const CC_STAR: u8 = 21;
const CC_PERCENT: u8 = 22;
const CC_COMMA: u8 = 23;
const CC_AND: u8 = 24;
const CC_TILDA: u8 = 25;
const CC_DOT: u8 = 26;
const CC_ID: u8 = 27;
// CC_ILLEGAL = 28 is the switch's default case.
const CC_NUL: u8 = 29;
const CC_BOM: u8 = 30;

// aiClass[] for 0x00..=0x7f (tokenize.c:60, SQLITE_ASCII).
#[rustfmt::skip]
const AI_CLASS: [u8; 128] = [
    29, 28, 28, 28, 28, 28, 28, 28, 28,  7,  7, 28,  7,  7, 28, 28,
    28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
     7, 15,  8,  5,  4, 22, 24,  8, 17, 18, 21, 20, 23, 11, 26, 16,
     3,  3,  3,  3,  3,  3,  3,  3,  3,  3,  5, 19, 12, 14, 13,  6,
     5,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,
     1,  1,  1,  1,  1,  1,  1,  1,  0,  2,  2,  9, 28, 28, 28,  2,
     8,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,  1,
     1,  1,  1,  1,  1,  1,  1,  1,  0,  2,  2, 28, 10, 28, 25, 28,
];

// sqlite3CtypeMap bits (global.c:118) for 0x00..=0x7f:
// 0x01 space, 0x02 alpha, 0x04 digit, 0x08 xdigit, 0x20 lower, 0x40 '$'/'_', 0x80 quote.
const CT_SPACE: u8 = 0x01;
const CT_DIGIT: u8 = 0x04;
const CT_XDIGIT: u8 = 0x08;
const CT_QUOTE: u8 = 0x80;

const CTYPE: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x09] = CT_SPACE;
    t[0x0a] = CT_SPACE;
    t[0x0b] = CT_SPACE;
    t[0x0c] = CT_SPACE;
    t[0x0d] = CT_SPACE;
    t[0x20] = CT_SPACE;
    let mut c = 0x30;
    while c <= 0x39 {
        t[c] = 0x0c;
        c += 1;
    }
    let mut c = 0x41;
    while c <= 0x5a {
        t[c] = if c <= 0x46 { 0x0a } else { 0x02 };
        c += 1;
    }
    let mut c = 0x61;
    while c <= 0x7a {
        t[c] = if c <= 0x66 { 0x2a } else { 0x22 };
        c += 1;
    }
    t[0x24] = 0x40; // $
    t[0x5f] = 0x40; // _
    t[0x22] = CT_QUOTE; // "
    t[0x27] = CT_QUOTE; // '
    t[0x5b] = CT_QUOTE; // [
    t[0x60] = CT_QUOTE; // `
    t
};

/// `sqlite3Isspace` on a byte (`false` for `>= 0x80`).
#[inline]
pub const fn is_sql_space(c: u8) -> bool {
    c < 0x80 && (CTYPE[c as usize] & CT_SPACE) != 0
}

#[inline]
const fn is_digit(c: u8) -> bool {
    c < 0x80 && (CTYPE[c as usize] & CT_DIGIT) != 0
}

#[inline]
const fn is_xdigit(c: u8) -> bool {
    c < 0x80 && (CTYPE[c as usize] & CT_XDIGIT) != 0
}

/// `sqlite3Isquote`: one of `"`, `'`, `[`, `` ` ``.
#[inline]
pub const fn is_quote_char(c: u8) -> bool {
    c < 0x80 && (CTYPE[c as usize] & CT_QUOTE) != 0
}

/// `IdChar` (`tokenize.c:168`): letters, digits, `_`, `$`, and every byte `>= 0x80`.
#[inline]
pub const fn is_id_char(c: u8) -> bool {
    c >= 0x80 || (CTYPE[c as usize] & 0x46) != 0
}

#[inline]
const fn char_class(c: u8) -> u8 {
    if c < 0x80 {
        AI_CLASS[c as usize]
    } else if c == 0xef {
        CC_BOM
    } else {
        CC_ID
    }
}

/// True if `bytes[i..i+5]` is ASCII case-insensitively `"ilike"`.
#[inline]
pub fn is_ilike(bytes: &[u8], i: usize) -> bool {
    i.checked_add(5)
        .and_then(|end| bytes.get(i..end))
        .is_some_and(|s| s.eq_ignore_ascii_case(b"ilike"))
}

/// Effective end of SQL input as SQLite sees it: index of the first NUL byte, or `sql.len()`.
#[inline]
pub fn sql_end(sql: &str) -> usize {
    sql.as_bytes()
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(sql.len())
}

/// SQL dialect: `Sqlite` matches SQLite 3.54.1 strictly; `Xdb` adds DuckDB-inspired conveniences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    #[default]
    Sqlite,
    Xdb,
}

/// A scanned token with its UTF-8 byte span `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: u16,
    pub start: usize,
    pub end: usize,
}

/// Allocation-free SQL scanner (`sqlite3GetToken`).
#[derive(Debug, Clone)]
pub struct Scanner<'a> {
    bytes: &'a [u8],
    pub limit: usize,
    pub dialect: Dialect,
    pub kind: u16,
    pub end: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(sql: &'a str, dialect: Dialect) -> Self {
        let limit = sql_end(sql);
        Self {
            bytes: sql.as_bytes(),
            limit,
            dialect,
            kind: tk::EOF,
            end: 0,
        }
    }

    #[inline]
    fn c(&self, i: usize) -> u8 {
        if i < self.limit {
            self.bytes[i]
        } else {
            0
        }
    }

    #[inline]
    fn set(&mut self, kind: u16, end: usize) {
        self.kind = kind;
        self.end = end;
    }

    /// `sqlite3GetToken`: scan the token starting at byte offset `i`.
    pub fn scan(&mut self, i: usize) {
        let n = self.limit;
        if i >= n {
            self.set(tk::EOF, i);
            return;
        }
        let c0 = self.bytes[i];
        let mut j: usize;
        match char_class(c0) {
            CC_SPACE => {
                j = i + 1;
                while j < n && is_sql_space(self.bytes[j]) {
                    j += 1;
                }
                self.set(tk::SPACE, j);
                return;
            }
            CC_MINUS => {
                let c1 = self.c(i + 1);
                if c1 == b'-' {
                    j = i + 2;
                    while j < n && self.bytes[j] != b'\n' {
                        j += 1;
                    }
                    self.set(tk::COMMENT, j);
                    return;
                }
                if c1 == b'>' {
                    let extra = usize::from(self.c(i + 2) == b'>');
                    self.set(tk::PTR, i + 2 + extra);
                    return;
                }
                self.set(tk::MINUS, i + 1);
                return;
            }
            CC_LP => {
                self.set(tk::LP, i + 1);
                return;
            }
            CC_RP => {
                self.set(tk::RP, i + 1);
                return;
            }
            CC_SEMI => {
                self.set(tk::SEMI, i + 1);
                return;
            }
            CC_PLUS => {
                self.set(tk::PLUS, i + 1);
                return;
            }
            CC_STAR => {
                self.set(tk::STAR, i + 1);
                return;
            }
            CC_SLASH => {
                if self.c(i + 1) != b'*' || self.c(i + 2) == 0 {
                    self.set(tk::SLASH, i + 1);
                    return;
                }
                j = i + 3;
                let mut c = self.c(i + 2);
                while c != b'*' || self.c(j) != b'/' {
                    c = self.c(j);
                    if c == 0 {
                        break;
                    }
                    j += 1;
                }
                if c != 0 {
                    j += 1;
                }
                self.set(tk::COMMENT, j);
                return;
            }
            CC_PERCENT => {
                self.set(tk::REM, i + 1);
                return;
            }
            CC_EQ => {
                let extra = usize::from(self.c(i + 1) == b'=');
                self.set(tk::EQ, i + 1 + extra);
                return;
            }
            CC_LT => {
                let c = self.c(i + 1);
                if c == b'=' {
                    self.set(tk::LE, i + 2);
                } else if c == b'>' {
                    self.set(tk::NE, i + 2);
                } else if c == b'<' {
                    self.set(tk::LSHIFT, i + 2);
                } else {
                    self.set(tk::LT, i + 1);
                }
                return;
            }
            CC_GT => {
                let c = self.c(i + 1);
                if c == b'=' {
                    self.set(tk::GE, i + 2);
                } else if c == b'>' {
                    self.set(tk::RSHIFT, i + 2);
                } else {
                    self.set(tk::GT, i + 1);
                }
                return;
            }
            CC_BANG => {
                if self.c(i + 1) != b'=' {
                    self.set(tk::ILLEGAL, i + 1);
                } else {
                    self.set(tk::NE, i + 2);
                }
                return;
            }
            CC_PIPE => {
                if self.c(i + 1) != b'|' {
                    self.set(tk::BITOR, i + 1);
                } else {
                    self.set(tk::CONCAT, i + 2);
                }
                return;
            }
            CC_COMMA => {
                self.set(tk::COMMA, i + 1);
                return;
            }
            CC_AND => {
                self.set(tk::BITAND, i + 1);
                return;
            }
            CC_TILDA => {
                self.set(tk::BITNOT, i + 1);
                return;
            }
            CC_QUOTE => {
                let delim = c0;
                let mut c = 0u8;
                j = i + 1;
                while j < n {
                    c = self.bytes[j];
                    if c == delim {
                        if self.c(j + 1) == delim {
                            j += 2;
                            continue;
                        } else {
                            break;
                        }
                    }
                    j += 1;
                }
                if j >= n {
                    c = 0;
                }
                if c == b'\'' {
                    self.set(tk::STRING, j + 1);
                } else if c != 0 {
                    self.set(tk::ID, j + 1);
                } else {
                    self.set(tk::ILLEGAL, j);
                }
                return;
            }
            CC_DOT => {
                if !is_digit(self.c(i + 1)) {
                    self.set(tk::DOT, i + 1);
                } else {
                    self.scan_number(i);
                }
                return;
            }
            CC_DIGIT => {
                self.scan_number(i);
                return;
            }
            CC_QUOTE2 => {
                j = i + 1;
                let mut c = c0;
                while c != b']' {
                    c = self.c(j);
                    if c == 0 {
                        break;
                    }
                    j += 1;
                }
                self.set(if c == b']' { tk::ID } else { tk::ILLEGAL }, j);
                return;
            }
            CC_VARNUM => {
                j = i + 1;
                while is_digit(self.c(j)) {
                    j += 1;
                }
                self.set(tk::VARIABLE, j);
                return;
            }
            CC_DOLLAR | CC_VARALPHA => {
                let mut kind = tk::VARIABLE;
                let mut count = 0usize;
                j = i + 1;
                loop {
                    let c = self.c(j);
                    if c == 0 {
                        break;
                    }
                    if is_id_char(c) {
                        count += 1;
                        j += 1;
                    } else if c == b'(' && count > 0 {
                        // TCL-style array suffix: $a(...) up to whitespace or ')'
                        loop {
                            j += 1;
                            let cc = self.c(j);
                            if cc == 0 || is_sql_space(cc) || cc == b')' {
                                if cc == b')' {
                                    j += 1;
                                } else {
                                    kind = tk::ILLEGAL;
                                }
                                break;
                            }
                        }
                        break;
                    } else if c == b':' && self.c(j + 1) == b':' {
                        j += 2;
                    } else {
                        break;
                    }
                }
                if count == 0 {
                    kind = tk::ILLEGAL;
                }
                if kind == tk::ILLEGAL
                    && self.dialect == Dialect::Xdb
                    && c0 == b':'
                    && self.c(i + 1) == b':'
                {
                    self.set(tk::DCOLON, i + 2);
                    return;
                }
                self.set(kind, j);
                return;
            }
            CC_KYWD0 => {
                if char_class(self.c(i + 1)) > CC_KYWD {
                    j = i + 1;
                } else {
                    j = i + 2;
                    while char_class(self.c(j)) <= CC_KYWD {
                        j += 1;
                    }
                    if is_id_char(self.c(j)) {
                        j += 1;
                    } else {
                        self.set(keyword_code(&self.bytes[i..j]), j);
                        return;
                    }
                }
            }
            CC_X => {
                if self.c(i + 1) == b'\'' {
                    let mut kind = tk::BLOB;
                    j = i + 2;
                    while is_xdigit(self.c(j)) {
                        j += 1;
                    }
                    if self.c(j) != b'\'' || !(j - i).is_multiple_of(2) {
                        kind = tk::ILLEGAL;
                        loop {
                            let c = self.c(j);
                            if c == 0 || c == b'\'' {
                                break;
                            }
                            j += 1;
                        }
                    }
                    if self.c(j) != 0 {
                        j += 1;
                    }
                    self.set(kind, j);
                    return;
                }
                j = i + 1;
            }
            CC_KYWD | CC_ID => {
                j = i + 1;
            }
            CC_BOM => {
                // UTF-8 BOM EF BB BF at start of token is SPACE; any other 0xEF... is an ID.
                if self.c(i + 1) == 0xbb && self.c(i + 2) == 0xbf {
                    self.set(tk::SPACE, i + 3);
                    return;
                }
                j = i + 1;
            }
            CC_NUL => {
                self.set(tk::EOF, i);
                return;
            }
            _ => {
                self.set(tk::ILLEGAL, i + 1);
                return;
            }
        }
        while is_id_char(self.c(j)) {
            j += 1;
        }
        self.set(tk::ID, j);
    }

    fn scan_number(&mut self, i: usize) {
        let mut kind = tk::INTEGER;
        let mut j: usize;
        let c1 = self.c(i + 1);
        if self.bytes[i] == b'0' && (c1 == b'x' || c1 == b'X') && is_xdigit(self.c(i + 2)) {
            j = i + 3;
            loop {
                let c = self.c(j);
                if !is_xdigit(c) {
                    if c == b'_' {
                        kind = tk::QNUMBER;
                    } else {
                        break;
                    }
                }
                j += 1;
            }
        } else {
            j = i;
            loop {
                let c = self.c(j);
                if !is_digit(c) {
                    if c == b'_' {
                        kind = tk::QNUMBER;
                    } else {
                        break;
                    }
                }
                j += 1;
            }
            if self.c(j) == b'.' {
                if kind == tk::INTEGER {
                    kind = tk::FLOAT;
                }
                j += 1;
                loop {
                    let c = self.c(j);
                    if !is_digit(c) {
                        if c == b'_' {
                            kind = tk::QNUMBER;
                        } else {
                            break;
                        }
                    }
                    j += 1;
                }
            }
            let ce = self.c(j);
            if (ce == b'e' || ce == b'E')
                && (is_digit(self.c(j + 1))
                    || ((self.c(j + 1) == b'+' || self.c(j + 1) == b'-')
                        && is_digit(self.c(j + 2))))
            {
                if kind == tk::INTEGER {
                    kind = tk::FLOAT;
                }
                j += 2;
                loop {
                    let c = self.c(j);
                    if !is_digit(c) {
                        if c == b'_' {
                            kind = tk::QNUMBER;
                        } else {
                            break;
                        }
                    }
                    j += 1;
                }
            }
        }
        while is_id_char(self.c(j)) {
            kind = tk::ILLEGAL;
            j += 1;
        }
        self.set(kind, j);
    }
}

/// Every raw token of `sql`, including `SPACE` and `COMMENT` tokens.
pub fn raw_tokens(sql: &str, dialect: Dialect) -> Vec<Token> {
    let mut s = Scanner::new(sql, dialect);
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        s.scan(i);
        if s.kind == tk::EOF {
            break;
        }
        out.push(Token {
            kind: s.kind,
            start: i,
            end: s.end,
        });
        if s.end == i {
            break;
        }
        i = s.end;
    }
    out
}

fn analyze_next(s: &mut Scanner<'_>, mut i: usize) -> (u16, usize) {
    loop {
        s.scan(i);
        if s.kind != tk::SPACE && s.kind != tk::COMMENT {
            break;
        }
        i = s.end;
    }
    let mut t = s.kind;
    if t == tk::ID
        || t == tk::STRING
        || t == tk::JOIN_KW
        || t == tk::WINDOW
        || t == tk::OVER
        || falls_back_to_id(t)
    {
        t = tk::ID;
    }
    (t, s.end)
}

/// `analyzeWindowKeyword` / `analyzeOverKeyword` / `analyzeFilterKeyword`
/// (`tokenize.c:226-246`).
pub fn classify_context_keyword(
    s: &mut Scanner<'_>,
    kind: u16,
    end: usize,
    last_token: Option<u16>,
) -> u16 {
    if kind == tk::WINDOW {
        let (a_type, a_end) = analyze_next(s, end);
        if a_type != tk::ID {
            return tk::ID;
        }
        let (b_type, _) = analyze_next(s, a_end);
        return if b_type == tk::AS { tk::WINDOW } else { tk::ID };
    }
    if kind == tk::OVER {
        if last_token == Some(tk::RP) {
            let (a_type, _) = analyze_next(s, end);
            if a_type == tk::LP || a_type == tk::ID {
                return tk::OVER;
            }
        }
        return tk::ID;
    }
    if kind == tk::FILTER {
        if last_token == Some(tk::RP) && analyze_next(s, end).0 == tk::LP {
            return tk::FILTER;
        }
        return tk::ID;
    }
    kind
}

/// XDB dialect: whether the identifier `ILIKE` is the operator (`LIKE_KW`) or stays `ID`.
pub fn classify_ilike(s: &mut Scanner<'_>, mut i: usize) -> u16 {
    loop {
        s.scan(i);
        if s.kind != tk::SPACE && s.kind != tk::COMMENT {
            break;
        }
        i = s.end;
    }
    let t = s.kind;
    let end = s.end;
    if t == tk::WINDOW {
        return if classify_context_keyword(s, tk::WINDOW, end, None) == tk::WINDOW {
            tk::ID
        } else {
            tk::LIKE_KW
        };
    }
    match t {
        tk::ID
        | tk::STRING
        | tk::INDEXED
        | tk::JOIN_KW
        | tk::INTEGER
        | tk::FLOAT
        | tk::BLOB
        | tk::QNUMBER
        | tk::NULL
        | tk::VARIABLE
        | tk::LP
        | tk::NOT
        | tk::BITNOT
        | tk::PLUS
        | tk::MINUS
        | tk::CASE
        | tk::EXISTS
        | tk::OVER
        | tk::FILTER => tk::LIKE_KW,
        _ => {
            if falls_back_to_id(t) {
                tk::LIKE_KW
            } else {
                tk::ID
            }
        }
    }
}

/// Tokens `sqlite3RunParser` feeds to the parser (drops whitespace/comments and
/// classifies `WINDOW`/`OVER`/`FILTER`).
pub fn parser_tokens(sql: &str, dialect: Dialect) -> Vec<Token> {
    let mut s = Scanner::new(sql, dialect);
    let mut probe = Scanner::new(sql, dialect);
    let mut out = Vec::new();
    let mut last: Option<u16> = None;
    let mut i = 0;
    loop {
        s.scan(i);
        let mut t = s.kind;
        let end = s.end;
        if t == tk::EOF {
            break;
        }
        if t != tk::SPACE && t != tk::COMMENT {
            if t == tk::WINDOW || t == tk::OVER || t == tk::FILTER {
                t = classify_context_keyword(&mut probe, t, end, last);
            } else if t == tk::ID
                && dialect == Dialect::Xdb
                && end - i == 5
                && is_ilike(sql.as_bytes(), i)
            {
                t = classify_ilike(&mut probe, end);
            }
            out.push(Token {
                kind: t,
                start: i,
                end,
            });
            last = Some(t);
        }
        if end == i {
            break;
        }
        i = end;
    }
    out
}

/// Split a SQL script into statement `[start, end)` spans the way `sqlite3_complete` sees them.
pub fn split_statements(sql: &str) -> Vec<(usize, usize)> {
    const TRANS: [[usize; 8]; 8] = [
        [1, 0, 2, 3, 4, 2, 2, 2],
        [1, 1, 2, 3, 4, 2, 2, 2],
        [1, 2, 2, 2, 2, 2, 2, 2],
        [1, 3, 3, 2, 4, 2, 2, 2],
        [1, 4, 2, 2, 2, 4, 5, 2],
        [6, 5, 5, 5, 5, 5, 5, 5],
        [6, 6, 5, 5, 5, 5, 5, 7],
        [1, 7, 5, 5, 5, 5, 5, 5],
    ];
    let mut s = Scanner::new(sql, Dialect::Sqlite);
    let mut out = Vec::new();
    let mut state = 0usize;
    let mut stmt_start: Option<usize> = None;
    let mut i = 0usize;
    loop {
        s.scan(i);
        let t = s.kind;
        if t == tk::EOF {
            break;
        }
        let tok = match t {
            tk::SEMI => 0,
            tk::SPACE | tk::COMMENT => 1,
            tk::EXPLAIN => 3,
            tk::CREATE => 4,
            tk::TEMP => 5,
            tk::TRIGGER => 6,
            tk::END => 7,
            _ => 2,
        };
        if tok != 1 && tok != 0 && stmt_start.is_none() {
            stmt_start = Some(i);
        }
        state = TRANS[state][tok];
        if tok == 0 && state == 1 {
            if let Some(start) = stmt_start.take() {
                out.push((start, s.end));
            }
        }
        if s.end == i {
            break;
        }
        i = s.end;
    }
    if let Some(start) = stmt_start {
        out.push((start, s.limit));
    }
    out
}
