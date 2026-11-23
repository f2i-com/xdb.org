//! Lemon LALR(1) parser driver (`tool/lempar.c` `Parse()` with `YYNOERRORRECOVERY`)
//! and the token loop of `sqlite3RunParser` (`tokenize.c`).

use std::collections::HashMap;

use xdb_core::Meter;

use crate::actions::{run_action, SemVal};
use crate::ast::{Name, SelectStmt, Span, Statement};
use crate::errors::{ParseResult, SqlParseError, SqlParseErrorKind};
use crate::lalr_tables::{ActionId, LalrTables, SQLITE_GRAMMAR, XDB_GRAMMAR};
use crate::tokenizer::{classify_context_keyword, classify_ilike, is_ilike, Dialect, Scanner};
use crate::tokens::tk;

/// Expression properties tracked at parse time (`EP_*` subset).
pub(crate) mod expr_flag {
    /// `EP_HasFunc`: contains a function call (propagated).
    pub const HAS_FUNC: u8 = 1;
    /// `EP_IsFalse`: integer literal 0 or folded false constant (not propagated).
    pub const IS_FALSE: u8 = 2;
    /// Node SQLite builds is `TK_UPLUS` (unary `+` rewritable in place).
    pub const UPLUS: u8 = 4;
}

/// Options controlling SQL parsing and SQLite parse-time limits.
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Parse as SQLite does while loading `sqlite_schema` (`db->init.busy = 1`).
    pub schema: bool,
    /// SQL dialect (`Sqlite` or `Xdb`).
    pub dialect: Dialect,
    /// `SQLITE_LIMIT_VARIABLE_NUMBER` (default 32766).
    pub max_variable_number: usize,
    /// `SQLITE_LIMIT_FUNCTION_ARG` (default 1000).
    pub max_function_args: usize,
    /// `SQLITE_LIMIT_EXPR_DEPTH` (default 1000).
    pub max_expr_depth: u32,
    /// `SQLITE_LIMIT_PARSER_DEPTH` (default 2500).
    pub max_parser_depth: usize,
    /// `SQLITE_LIMIT_COLUMN` (default 2000).
    pub max_columns: usize,
    /// `SQLITE_LIMIT_COMPOUND_SELECT` (default 500).
    pub max_compound_select: usize,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            schema: false,
            dialect: Dialect::Sqlite,
            max_variable_number: 32766,
            max_function_args: 1000,
            max_expr_depth: 1000,
            max_parser_depth: 2500,
            max_columns: 2000,
            max_compound_select: 500,
        }
    }
}

/// Output of parsing one statement at an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAt {
    pub statement: Option<Statement>,
    pub end: usize,
}

/// Stateful LALR(1) SQL parser instance.
pub struct Parser<'a> {
    pub(crate) sql: &'a str,
    pub(crate) schema_mode: bool,
    pub(crate) dialect: Dialect,
    pub(crate) max_var: usize,
    pub(crate) max_args: usize,
    pub(crate) max_height: u32,
    pub(crate) max_depth: usize,
    pub(crate) max_columns: usize,
    pub(crate) max_compound: usize,
    pub(crate) limit: usize,
    tables: &'static LalrTables,
    scanner: Scanner<'a>,
    probe: Scanner<'a>,
    meter: Option<&'a dyn Meter>,

    // Parser stack
    pub(crate) state: Vec<u16>,
    pub(crate) v: Vec<SemVal>,
    pub(crate) st: Vec<isize>,
    pub(crate) en: Vec<isize>,
    pub(crate) h: Vec<u32>,
    pub(crate) f: Vec<u8>,
    top: usize,
    stack_end: usize,

    // Rule currently being reduced
    pub(crate) rs: isize,
    pub(crate) re: isize,
    pub(crate) look_type: u16,
    pub(crate) look_start: usize,
    pub(crate) look_end: usize,

    // Per-statement state (Parse)
    pub(crate) n_var: usize,
    pub(crate) var_names: Option<HashMap<String, usize>>,
    pub(crate) constraint_name: Option<Name>,
    pub(crate) has_with: bool,
    pub(crate) in_trigger: bool,
    pub(crate) explain: u8,
    pub(crate) explain_start: usize,
    pub(crate) n_err: usize,
    err: Option<SqlParseError>,
    pub(crate) done: bool,
    pub(crate) result: Option<Statement>,
}

impl std::fmt::Debug for Parser<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("dialect", &self.dialect)
            .field("schema_mode", &self.schema_mode)
            .field("top", &self.top)
            .field("n_err", &self.n_err)
            .field("done", &self.done)
            .finish()
    }
}

impl<'a> Parser<'a> {
    pub fn new(sql: &'a str, options: &ParseOptions, meter: Option<&'a dyn Meter>) -> Self {
        let tables = match options.dialect {
            Dialect::Sqlite => &SQLITE_GRAMMAR,
            Dialect::Xdb => &XDB_GRAMMAR,
        };
        let scanner = Scanner::new(sql, options.dialect);
        let probe = Scanner::new(sql, options.dialect);
        let limit = scanner.limit;
        let init_cap = tables.yy_stack_depth.min(options.max_parser_depth.max(1));
        let mut v = Vec::with_capacity(init_cap);
        v.resize_with(init_cap, || SemVal::Empty);
        Self {
            sql,
            schema_mode: options.schema,
            dialect: options.dialect,
            max_var: options.max_variable_number,
            max_args: options.max_function_args,
            max_height: options.max_expr_depth,
            max_depth: options.max_parser_depth,
            max_columns: options.max_columns,
            max_compound: options.max_compound_select,
            limit,
            tables,
            scanner,
            probe,
            meter,
            state: vec![0; init_cap],
            v,
            st: vec![-1; init_cap],
            en: vec![-1; init_cap],
            h: vec![0; init_cap],
            f: vec![0; init_cap],
            top: 0,
            stack_end: init_cap - 1,
            rs: -1,
            re: -1,
            look_type: 0,
            look_start: 0,
            look_end: 0,
            n_var: 0,
            var_names: None,
            constraint_name: None,
            has_with: false,
            in_trigger: false,
            explain: 0,
            explain_start: 0,
            n_err: 0,
            err: None,
            done: false,
            result: None,
        }
    }

    // ─────────────────────────── errors ───────────────────────────

    pub(crate) fn error(
        &mut self,
        message: impl Into<String>,
        start: isize,
        end: isize,
        kind: SqlParseErrorKind,
    ) {
        self.n_err += 1;
        let s = if start >= 0 {
            Some(start as usize)
        } else {
            None
        };
        let e = if end >= 0 { Some(end as usize) } else { None };
        self.err = Some(SqlParseError::new(message, kind, s, e));
    }

    pub(crate) fn syntax_error_at(&mut self, s: usize, e: usize) {
        if s >= self.limit {
            self.error(
                "incomplete input",
                s as isize,
                e as isize,
                SqlParseErrorKind::Syntax,
            );
        } else {
            let tok = &self.sql[s..e];
            self.error(
                format!("near \"{tok}\": syntax error"),
                s as isize,
                e as isize,
                SqlParseErrorKind::Syntax,
            );
        }
    }

    pub(crate) fn check_height(&mut self, h: u32, s: isize, e: isize) {
        if h > self.max_height {
            self.error(
                format!(
                    "Expression tree is too large (maximum depth {})",
                    self.max_height
                ),
                s,
                e,
                SqlParseErrorKind::Limit,
            );
        }
    }

    #[inline]
    pub(crate) fn rule_span(&self) -> Span {
        let s = if self.rs >= 0 { self.rs as usize } else { 0 };
        let e = if self.re >= 0 { self.re as usize } else { s };
        Span::new(s, e)
    }

    // ─────────────────────────── token loops ───────────────────────────

    /// Parse one statement starting at byte `offset`.
    pub fn parse_at(&mut self, offset: usize) -> ParseResult<ParsedAt> {
        let mut i = offset;
        loop {
            self.scanner.scan(i);
            if self.scanner.kind == tk::SPACE || self.scanner.kind == tk::COMMENT {
                if let Some(m) = self.meter {
                    m.charge_steps(1).map_err(SqlParseError::from_limit)?;
                }
                i = self.scanner.end;
                continue;
            }
            if self.scanner.kind == tk::SEMI {
                if let Some(m) = self.meter {
                    m.charge_steps(1).map_err(SqlParseError::from_limit)?;
                }
                return Ok(ParsedAt {
                    statement: None,
                    end: self.scanner.end,
                });
            }
            if self.scanner.kind == tk::EOF {
                return Ok(ParsedAt {
                    statement: None,
                    end: offset,
                });
            }
            break;
        }
        self.reset_stack();
        let mut last: Option<u16> = None;
        loop {
            self.scanner.scan(i);
            let mut kind = self.scanner.kind;
            let mut end = self.scanner.end;
            if kind == tk::SPACE || kind == tk::COMMENT {
                if let Some(m) = self.meter {
                    m.charge_steps(1).map_err(SqlParseError::from_limit)?;
                }
                i = end;
                continue;
            }
            if kind == tk::EOF {
                if last == Some(tk::SEMI) {
                    kind = tk::EOF;
                } else if last == Some(tk::EOF) {
                    break;
                } else {
                    kind = tk::SEMI;
                }
                end = i;
            } else if kind == tk::WINDOW || kind == tk::OVER || kind == tk::FILTER {
                kind = classify_context_keyword(&mut self.probe, kind, end, last);
            } else if kind == tk::ID
                && self.dialect == Dialect::Xdb
                && end - i == 5
                && is_ilike(self.sql.as_bytes(), i)
            {
                kind = classify_ilike(&mut self.probe, end);
            } else if kind == tk::ILLEGAL {
                return Err(SqlParseError::new(
                    format!("unrecognized token: \"{}\"", &self.sql[i..end]),
                    SqlParseErrorKind::Syntax,
                    Some(i),
                    Some(end),
                ));
            }
            self.feed(kind, i, end);
            last = Some(kind);
            i = end;
            if let Some(err) = self.err.take() {
                return Err(err);
            }
            if self.done {
                break;
            }
        }
        Ok(ParsedAt {
            statement: if self.done { self.result.take() } else { None },
            end: i,
        })
    }

    /// Parse the entire input wrapped as `VALUES(<sql>)`.
    pub fn parse_wrapped_expression(&mut self) -> ParseResult<SelectStmt> {
        self.reset_stack();
        self.feed(tk::VALUES, 0, 0);
        if let Some(err) = self.err.take() {
            return Err(err);
        }
        self.feed(tk::LP, 0, 0);
        if let Some(err) = self.err.take() {
            return Err(err);
        }
        let mut last: Option<u16> = Some(tk::LP);
        let mut closed = false;
        let mut i = 0usize;
        loop {
            self.scanner.scan(i);
            let mut kind = self.scanner.kind;
            let mut end = self.scanner.end;
            if kind == tk::SPACE || kind == tk::COMMENT {
                i = end;
                continue;
            }
            if kind == tk::EOF {
                if !closed {
                    kind = tk::RP;
                    closed = true;
                } else if last == Some(tk::SEMI) {
                    kind = tk::EOF;
                } else if last == Some(tk::EOF) {
                    break;
                } else {
                    kind = tk::SEMI;
                }
                end = i;
            } else if kind == tk::WINDOW || kind == tk::OVER || kind == tk::FILTER {
                kind = classify_context_keyword(&mut self.probe, kind, end, last);
            } else if kind == tk::ID
                && self.dialect == Dialect::Xdb
                && end - i == 5
                && is_ilike(self.sql.as_bytes(), i)
            {
                kind = classify_ilike(&mut self.probe, end);
            } else if kind == tk::ILLEGAL {
                return Err(SqlParseError::new(
                    format!("unrecognized token: \"{}\"", &self.sql[i..end]),
                    SqlParseErrorKind::Syntax,
                    Some(i),
                    Some(end),
                ));
            }
            self.feed(kind, i, end);
            last = Some(kind);
            i = end;
            if let Some(err) = self.err.take() {
                return Err(err);
            }
            if self.done {
                break;
            }
        }
        if self.done {
            if !closed {
                return Err(SqlParseError::new(
                    "not an expression",
                    SqlParseErrorKind::Syntax,
                    Some(i),
                    Some(self.limit),
                ));
            }
            if let Some(Statement::Select(sel)) = self.result.take() {
                return Ok(*sel);
            }
        }
        Err(SqlParseError::new(
            "incomplete input",
            SqlParseErrorKind::Syntax,
            Some(i),
            Some(i),
        ))
    }

    // ─────────────────────────── LALR stack machine ───────────────────────────

    fn reset_stack(&mut self) {
        self.top = 0;
        let init_len = self.tables.yy_stack_depth.min(self.max_depth.max(1));
        self.stack_end = init_len - 1;
        if self.state.len() < init_len {
            self.state.resize(init_len, 0);
            self.v.resize_with(init_len, || SemVal::Empty);
            self.st.resize(init_len, -1);
            self.en.resize(init_len, -1);
            self.h.resize(init_len, 0);
            self.f.resize(init_len, 0);
        }
        self.state[0] = 0;
        self.v[0] = SemVal::Empty;
        self.st[0] = -1;
        self.en[0] = -1;
        self.h[0] = 0;
        self.f[0] = 0;
        self.n_var = 0;
        self.var_names = None;
        self.constraint_name = None;
        self.has_with = false;
        self.in_trigger = false;
        self.explain = 0;
        self.explain_start = 0;
        self.n_err = 0;
        self.err = None;
        self.done = false;
        self.result = None;
    }

    fn grow(&mut self) -> bool {
        let old_size = self.stack_end + 1;
        let mut new_size = old_size * 2 + 100;
        if new_size > self.max_depth {
            new_size = self.max_depth;
            if new_size <= old_size {
                return false;
            }
        }
        if new_size > self.state.len() {
            self.state.resize(new_size, 0);
            self.v.resize_with(new_size, || SemVal::Empty);
            self.st.resize(new_size, -1);
            self.en.resize(new_size, -1);
            self.h.resize(new_size, 0);
            self.f.resize(new_size, 0);
        }
        self.stack_end = new_size - 1;
        true
    }

    fn overflow(&mut self) {
        self.top = 0;
        if self.n_err == 0 {
            self.error(
                "Recursion limit",
                self.look_start as isize,
                self.look_end as isize,
                SqlParseErrorKind::Limit,
            );
        }
    }

    fn charge_step(&mut self) -> bool {
        if let Some(m) = self.meter {
            if let Err(e) = m.charge_steps(1) {
                self.n_err += 1;
                self.err = Some(SqlParseError::from_limit(e));
                return false;
            }
        }
        true
    }

    fn feed(&mut self, major: u16, ts: usize, te: usize) {
        let t = self.tables;
        self.look_type = major;
        self.look_start = ts;
        self.look_end = te;
        if !self.charge_step() {
            return;
        }
        let mut act = self.state[self.top];
        loop {
            act = t.find_shift_action(major, act);
            if act >= t.yy_min_reduce {
                let rule = usize::from(act - t.yy_min_reduce);
                if t.rule_nrhs[rule] == 0 && self.top >= self.stack_end && !self.grow() {
                    self.overflow();
                    return;
                }
                if !self.charge_step() {
                    return;
                }
                act = self.reduce(rule);
            } else if act <= t.yy_max_shiftreduce {
                self.shift(act, major, ts, te);
                return;
            } else if act == t.yy_accept_action {
                self.top = self.top.saturating_sub(1);
                return;
            } else {
                self.syntax_error_at(ts, te);
                return;
            }
        }
    }

    fn shift(&mut self, mut act: u16, major: u16, ts: usize, te: usize) {
        let t = self.tables;
        let k = self.top + 1;
        if k > self.stack_end && !self.grow() {
            self.overflow();
            return;
        }
        self.top = k;
        if act > t.yy_max_shift {
            act += t.yy_min_reduce - t.yy_min_shiftreduce;
        }
        self.state[k] = act;
        self.v[k] = SemVal::Tok(major);
        self.st[k] = ts as isize;
        self.en[k] = te as isize;
        self.h[k] = 0;
        self.f[k] = 0;
    }

    fn reduce(&mut self, rule: usize) -> u16 {
        let t = self.tables;
        let n = usize::from(t.rule_nrhs[rule]);
        let b = self.top + 1 - n;
        let mut rs = -1isize;
        let mut re = -1isize;
        if n == 0 {
            self.v[b] = SemVal::Empty;
            self.h[b] = 0;
            self.f[b] = 0;
        } else {
            for k in b..=self.top {
                if self.st[k] >= 0 {
                    if rs < 0 {
                        rs = self.st[k];
                    }
                    re = self.en[k];
                }
            }
        }
        self.rs = rs;
        self.re = re;
        let action = t.rule_action[rule];
        if action != ActionId::PassThrough && action != ActionId::Pass0 {
            let val = run_action(action, self, b);
            self.v[b] = val;
        }
        // Drop any remaining RHS stack values above `b`.
        for k in (b + 1)..=self.top {
            self.v[k] = SemVal::Empty;
        }
        let act = t.find_reduce_action(self.state[b - 1], t.rule_lhs[rule]);
        self.top = b;
        self.state[b] = act;
        self.st[b] = rs;
        self.en[b] = re;
        act
    }
}
