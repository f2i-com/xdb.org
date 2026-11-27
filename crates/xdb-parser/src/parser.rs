//! Lemon LALR(1) parser driver (`tool/lempar.c` `Parse()` with `YYNOERRORRECOVERY`)
//! and the token loop of `sqlite3RunParser` (`tokenize.c`).

use std::collections::HashMap;

use xdb_core::Meter;

use crate::actions::{mvalues_append_in_place, run_action, SemVal};
use crate::ast::{Expr, Name, SelectStmt, Span, Statement};
use crate::errors::{ParseResult, SqlParseError, SqlParseErrorKind};
use crate::lalr_tables::{ActionId, LalrTables, SQLITE_GRAMMAR, XDB_GRAMMAR};
use crate::tokenizer::{
    classify_context_keyword_metered, classify_ilike_metered, is_ilike, Dialect, Scanner,
};
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

const INIT_STACK_CAP: usize = 32;
const MAX_POOLED_STACK_CAP: usize = 64;

struct ParserStacks {
    state: Vec<u16>,
    v: Vec<SemVal>,
    st: Vec<isize>,
    en: Vec<isize>,
    h: Vec<u32>,
    f: Vec<u8>,
}

impl ParserStacks {
    fn new_initial() -> Self {
        let mut v = Vec::with_capacity(INIT_STACK_CAP);
        v.resize_with(INIT_STACK_CAP, || SemVal::Empty);
        Self {
            state: vec![0; INIT_STACK_CAP],
            v,
            st: vec![-1; INIT_STACK_CAP],
            en: vec![-1; INIT_STACK_CAP],
            h: vec![0; INIT_STACK_CAP],
            f: vec![0; INIT_STACK_CAP],
        }
    }
}

thread_local! {
    static PARSER_STACK_POOL: std::cell::Cell<Option<ParserStacks>> = const { std::cell::Cell::new(None) };
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

    // Parser stack (pooled 32-slot heap vectors reused via thread-local cache)
    pub(crate) state: Vec<u16>,
    pub(crate) v: Vec<SemVal>,
    pub(crate) st: Vec<isize>,
    pub(crate) en: Vec<isize>,
    pub(crate) h: Vec<u32>,
    pub(crate) f: Vec<u8>,
    top: usize,
    high_water: usize,
    dirty: bool,
    stack_cap: usize,
    stack_end: usize,
    pending_steps: u64,
    pending_mem: u64,
    reserved_stack_mem: u64,
    reserved_ast_mem: u64,

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

impl Drop for Parser<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.meter {
            let total = self.reserved_stack_mem.saturating_add(self.reserved_ast_mem);
            if total > 0 {
                m.release_memory(total);
                self.reserved_stack_mem = 0;
                self.reserved_ast_mem = 0;
            }
        }
        if self.v.capacity() <= MAX_POOLED_STACK_CAP
            && self.state.capacity() <= MAX_POOLED_STACK_CAP
            && self.v.len() >= INIT_STACK_CAP
        {
            self.clear_stack_values();
            let stacks = ParserStacks {
                state: std::mem::take(&mut self.state),
                v: std::mem::take(&mut self.v),
                st: std::mem::take(&mut self.st),
                en: std::mem::take(&mut self.en),
                h: std::mem::take(&mut self.h),
                f: std::mem::take(&mut self.f),
            };
            let _ = PARSER_STACK_POOL.try_with(|pool| {
                pool.set(Some(stacks));
            });
        }
    }
}

impl<'a> Parser<'a> {
    pub fn new(sql: &'a str, options: &ParseOptions, meter: Option<&'a dyn Meter>) -> Self {
        let tables = match options.dialect {
            Dialect::Sqlite => &SQLITE_GRAMMAR,
            Dialect::Xdb => &XDB_GRAMMAR,
        };
        let scanner = Scanner::new_lazy(sql, options.dialect);
        let probe = Scanner::new_lazy(sql, options.dialect);
        let limit = scanner.limit;
        let stacks = PARSER_STACK_POOL
            .try_with(|pool| pool.take())
            .ok()
            .flatten()
            .unwrap_or_else(ParserStacks::new_initial);
        let init_cap = INIT_STACK_CAP
            .min(tables.yy_stack_depth)
            .min(options.max_parser_depth.max(1));
        let stack_cap = stacks.v.len();
        let meter = meter.filter(|m| !m.is_noop());
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
            state: stacks.state,
            v: stacks.v,
            st: stacks.st,
            en: stacks.en,
            h: stacks.h,
            f: stacks.f,
            top: 0,
            high_water: 0,
            dirty: false,
            stack_cap,
            stack_end: init_cap - 1,
            pending_steps: 0,
            pending_mem: 0,
            reserved_stack_mem: 0,
            reserved_ast_mem: 0,
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
        if self.err.is_none() {
            let s = if start >= 0 {
                Some(start as usize)
            } else {
                None
            };
            let e = if end >= 0 { Some(end as usize) } else { None };
            self.err = Some(SqlParseError::new(message, kind, s, e));
        }
    }

    pub(crate) fn syntax_error_at(&mut self, s: usize, e: usize) {
        if s >= self.limit || self.sql.as_bytes().get(s).copied().unwrap_or(0) == 0 {
            self.error(
                "incomplete input",
                s as isize,
                e as isize,
                SqlParseErrorKind::Syntax,
            );
        } else {
            if self.meter.is_some()
                && (!self.charge_ast_mem(e.saturating_sub(s) as u64) || !self.flush_meters())
            {
                return;
            }
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
        if offset > self.sql.len() || !self.sql.is_char_boundary(offset) {
            return Err(SqlParseError::new(
                "invalid UTF-8 offset",
                SqlParseErrorKind::Syntax,
                Some(offset.min(self.sql.len())),
                Some(offset.min(self.sql.len())),
            ));
        }
        let mut i = offset;
        loop {
            self.scanner.scan(i);
            if self.scanner.kind == tk::SPACE || self.scanner.kind == tk::COMMENT {
                if !self.charge_steps_for_len(self.scanner.end.saturating_sub(i)) {
                    return Err(self.err.take().expect("limit error"));
                }
                i = self.scanner.end;
                continue;
            }
            if self.scanner.kind == tk::SEMI {
                if !self.charge_step() || !self.flush_meters() {
                    return Err(self.err.take().expect("limit error"));
                }
                return Ok(ParsedAt {
                    statement: None,
                    end: self.scanner.end,
                });
            }
            if self.scanner.kind == tk::EOF {
                if !self.flush_meters() {
                    return Err(self.err.take().expect("limit error"));
                }
                return Ok(ParsedAt {
                    statement: None,
                    end: i,
                });
            }
            break;
        }
        self.reset_stack();
        self.dirty = true;
        let mut last: Option<u16> = None;
        let mut have_first = true;
        loop {
            if have_first {
                have_first = false;
            } else {
                self.scanner.scan(i);
            }
            let mut kind = self.scanner.kind;
            let mut end = self.scanner.end;
            if kind == tk::SPACE || kind == tk::COMMENT {
                if !self.charge_steps_for_len(end.saturating_sub(i)) {
                    return Err(self.err.take().expect("limit error"));
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
                if !self.flush_steps() {
                    return Err(self.err.take().expect("limit error"));
                }
                kind = classify_context_keyword_metered(
                    &mut self.probe,
                    kind,
                    end,
                    last,
                    self.meter,
                )
                .map_err(SqlParseError::from_limit)?;
            } else if kind == tk::ID
                && self.dialect == Dialect::Xdb
                && end - i == 5
                && is_ilike(self.sql.as_bytes(), i)
            {
                if !self.flush_steps() {
                    return Err(self.err.take().expect("limit error"));
                }
                kind = classify_ilike_metered(&mut self.probe, end, self.meter)
                    .map_err(SqlParseError::from_limit)?;
            } else if kind == tk::ILLEGAL {
                let tok_len = end.saturating_sub(i);
                if !self.charge_steps_for_len(tok_len)
                    || (self.meter.is_some() && !self.charge_ast_mem(tok_len as u64))
                    || !self.flush_meters()
                {
                    return Err(self.err.take().expect("limit error"));
                }
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
                let _ = self.flush_meters();
                return Err(err);
            }
            if self.done {
                break;
            }
        }
        if !self.flush_meters() {
            return Err(self.err.take().expect("limit error"));
        }
        Ok(ParsedAt {
            statement: if self.done { self.result.take() } else { None },
            end: i,
        })
    }

    /// Parse the entire input wrapped as `VALUES(<sql>)`.
    pub fn parse_wrapped_expression(&mut self) -> ParseResult<SelectStmt> {
        self.reset_stack();
        self.dirty = true;
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
                if !self.charge_steps_for_len(end.saturating_sub(i)) {
                    return Err(self.err.take().expect("limit error"));
                }
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
                if !self.flush_steps() {
                    return Err(self.err.take().expect("limit error"));
                }
                kind = classify_context_keyword_metered(
                    &mut self.probe,
                    kind,
                    end,
                    last,
                    self.meter,
                )
                .map_err(SqlParseError::from_limit)?;
            } else if kind == tk::ID
                && self.dialect == Dialect::Xdb
                && end - i == 5
                && is_ilike(self.sql.as_bytes(), i)
            {
                if !self.flush_steps() {
                    return Err(self.err.take().expect("limit error"));
                }
                kind = classify_ilike_metered(&mut self.probe, end, self.meter)
                    .map_err(SqlParseError::from_limit)?;
            } else if kind == tk::ILLEGAL {
                let tok_len = end.saturating_sub(i);
                if !self.charge_steps_for_len(tok_len)
                    || (self.meter.is_some() && !self.charge_ast_mem(tok_len as u64))
                    || !self.flush_meters()
                {
                    return Err(self.err.take().expect("limit error"));
                }
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
                let _ = self.flush_meters();
                return Err(err);
            }
            if self.done {
                break;
            }
        }
        if !self.flush_meters() {
            return Err(self.err.take().expect("limit error"));
        }
        if self.done {
            if !closed {
                return Err(SqlParseError::new(
                    "not an expression",
                    SqlParseErrorKind::Syntax,
                    Some(i),
                    Some(i),
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

    fn clear_stack_values(&mut self) {
        if self.high_water == 0 && matches!(self.v[0], SemVal::Empty | SemVal::Tok(_)) {
            return;
        }
        let end = self.high_water.min(self.v.len().saturating_sub(1));
        for slot in &mut self.v[..=end] {
            if !matches!(*slot, SemVal::Empty | SemVal::Tok(_)) {
                *slot = SemVal::Empty;
            }
        }
        self.high_water = 0;
    }

    fn reset_stack(&mut self) {
        if !self.dirty {
            self.state[0] = 0;
            self.st[0] = -1;
            self.en[0] = -1;
            self.h[0] = 0;
            self.f[0] = 0;
            return;
        }
        self.clear_stack_values();
        self.top = 0;
        self.stack_end = self.stack_cap.min(self.max_depth.max(1)) - 1;
        if let Some(m) = self.meter {
            if self.reserved_ast_mem > 0 {
                m.release_memory(self.reserved_ast_mem);
                self.reserved_ast_mem = 0;
            }
        }
        self.pending_mem = 0;
        self.state[0] = 0;
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
        self.dirty = false;
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
        if new_size > self.stack_cap {
            let added = new_size - self.stack_cap;
            if let Some(m) = self.meter {
                let slot_bytes = (std::mem::size_of::<SemVal>() + 24) as u64;
                let bytes = (added as u64).saturating_mul(slot_bytes);
                if let Err(e) = m.reserve_memory(bytes) {
                    self.n_err += 1;
                    self.err = Some(SqlParseError::from_limit(e));
                    return false;
                }
                self.reserved_stack_mem = self.reserved_stack_mem.saturating_add(bytes);
            }
            self.state.resize(new_size, 0);
            self.v.resize_with(new_size, SemVal::default);
            self.st.resize(new_size, -1);
            self.en.resize(new_size, -1);
            self.h.resize(new_size, 0);
            self.f.resize(new_size, 0);
            self.stack_cap = new_size;
        }
        self.stack_end = new_size - 1;
        true
    }

    fn overflow(&mut self) {
        self.clear_stack_values();
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

    #[inline]
    fn charge_step(&mut self) -> bool {
        if self.meter.is_none() {
            return true;
        }
        self.pending_steps += 1;
        if self.pending_steps >= 64 {
            return self.flush_steps();
        }
        true
    }

    #[inline]
    fn charge_steps_for_len(&mut self, bytes: usize) -> bool {
        if self.meter.is_none() {
            return true;
        }
        let steps = ((bytes as u64) >> 6).max(1);
        self.pending_steps = self.pending_steps.saturating_add(steps);
        if self.pending_steps >= 64 {
            return self.flush_steps();
        }
        true
    }

    #[inline]
    pub(crate) fn alloc_sql_slice(&mut self, a: usize, z: usize) -> String {
        if z <= a {
            return String::new();
        }
        if self.meter.is_some() {
            let len = (z - a) as u64;
            if !self.charge_ast_mem(len) {
                return String::new();
            }
        }
        self.sql[a..z].to_string()
    }

    #[inline]
    pub(crate) fn clone_constraint_name(&mut self) -> Option<Name> {
        let len = self.constraint_name.as_ref()?.name.len();
        if self.meter.is_some() {
            let est = (len as u64).saturating_add(48);
            if !self.charge_ast_mem(est) {
                return None;
            }
        }
        self.constraint_name.clone()
    }

    fn flush_steps(&mut self) -> bool {
        if self.pending_steps == 0 {
            return true;
        }
        let steps = self.pending_steps;
        self.pending_steps = 0;
        if let Some(m) = self.meter {
            if let Err(e) = m.charge_steps(steps) {
                self.n_err += 1;
                if self.err.is_none() {
                    self.err = Some(SqlParseError::from_limit(e));
                }
                return false;
            }
        }
        true
    }

    #[inline]
    fn charge_ast_mem(&mut self, bytes: u64) -> bool {
        if self.meter.is_none() {
            return true;
        }
        self.pending_mem = self.pending_mem.saturating_add(bytes);
        if self.pending_mem >= 4096 {
            return self.flush_ast_mem();
        }
        true
    }

    fn flush_ast_mem(&mut self) -> bool {
        if self.pending_mem == 0 {
            return true;
        }
        let bytes = self.pending_mem;
        self.pending_mem = 0;
        if let Some(m) = self.meter {
            if let Err(e) = m.reserve_memory(bytes) {
                self.n_err += 1;
                if self.err.is_none() {
                    self.err = Some(SqlParseError::from_limit(e));
                }
                return false;
            }
            self.reserved_ast_mem = self.reserved_ast_mem.saturating_add(bytes);
        }
        true
    }

    fn flush_meters(&mut self) -> bool {
        self.flush_steps() && self.flush_ast_mem()
    }

    fn feed(&mut self, major: u16, ts: usize, te: usize) {
        let t = self.tables;
        self.look_type = major;
        self.look_start = ts;
        self.look_end = te;
        if !self.charge_steps_for_len(te.saturating_sub(ts)) {
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
                if self.err.is_some() {
                    return;
                }
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
        if k > self.high_water {
            self.high_water = k;
        }
        if act > t.yy_max_shift {
            act += t.yy_min_reduce - t.yy_min_shiftreduce;
        }
        self.state[k] = act;
        self.v[k] = SemVal::Tok(major);
        self.st[k] = ts as isize;
        self.en[k] = te as isize;
        self.h[k] = 0;
        // Dedicated bit 0x80 (distinct from `expr_flag::HAS_FUNC = 1` / `IS_FALSE = 2`)
        // records whether a string token contained doubled quotes `''`.
        self.f[k] = if major == tk::STRING && self.scanner.has_doubled_quote {
            0x80
        } else {
            0
        };
    }

    fn reduce(&mut self, rule: usize) -> u16 {
        let t = self.tables;
        let n = usize::from(t.rule_nrhs[rule]);
        let b = self.top + 1 - n;
        let mut rs = -1isize;
        let mut re = -1isize;
        if n == 1 {
            rs = self.st[b];
            re = self.en[b];
        } else if n == 0 {
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
        let mut needs_cleanup = true;
        if action == ActionId::PassThrough || action == ActionId::Pass0 {
            // Leave `self.v[b]` untouched.
        } else if action == ActionId::Pass1 {
            self.h[b] = self.h[b + 1];
            self.f[b] = self.f[b + 1];
            self.v[b] = std::mem::take(&mut self.v[b + 1]);
            if b > self.high_water {
                self.high_water = b;
            }
        } else if action == ActionId::Pass2 {
            self.h[b] = self.h[b + 2];
            self.f[b] = self.f[b + 2];
            self.v[b] = std::mem::take(&mut self.v[b + 2]);
            if b > self.high_water {
                self.high_water = b;
            }
        } else if action == ActionId::Nul {
            if n > 0 && !matches!(self.v[b], SemVal::Empty | SemVal::Tok(_)) {
                self.v[b] = SemVal::Empty;
            }
        } else if action == ActionId::NExprListAppend {
            let item = match std::mem::take(&mut self.v[b + 2]) {
                SemVal::Expr(e) => e,
                _ => Expr::Null {
                    span: Span::default(),
                },
            };
            if let SemVal::Exprs(ref mut list) = self.v[b] {
                list.push(item);
            } else {
                self.v[b] = SemVal::Exprs(vec![item]);
            }
            if self.h[b + 2] > self.h[b] {
                self.h[b] = self.h[b + 2];
            }
            self.f[b] = (self.f[b] | self.f[b + 2]) & expr_flag::HAS_FUNC;
            if b > self.high_water {
                self.high_water = b;
            }
            if self.meter.is_some() && !self.charge_ast_mem(128) {
                return 0;
            }
            needs_cleanup = false;
        } else if action == ActionId::MvaluesAppend {
            mvalues_append_in_place(self, b);
            if b > self.high_water {
                self.high_water = b;
            }
            if self.err.is_some() {
                return 0;
            }
            if self.meter.is_some() && !self.charge_ast_mem(192) {
                return 0;
            }
            needs_cleanup = false;
        } else if self.meter.is_none()
            && matches!(
                action,
                ActionId::TermInteger
                    | ActionId::TermString
                    | ActionId::ExprColumn1
                    | ActionId::NExprListSingle
            )
        {
            match action {
                ActionId::TermInteger => {
                    let s = rs.max(0) as usize;
                    let e = re.max(0) as usize;
                    let z = &self.sql[s..e];
                    let zb = z.as_bytes();
                    let is_hex = zb.len() > 1 && zb[0] == b'0' && (zb[1] | 0x20) == b'x';
                    let is_zero = if is_hex {
                        zb[2..].iter().all(|&c| c == b'0')
                    } else {
                        zb.iter().all(|&c| c == b'0')
                    };
                    self.h[b] = 1;
                    self.f[b] = if is_zero { expr_flag::IS_FALSE } else { 0 };
                    self.v[b] = SemVal::Expr(Expr::Number(crate::ast::NumberLiteral {
                        span: Span::new(s, e),
                        text: z.to_string(),
                        is_float: false,
                        is_hex,
                        separators: false,
                    }));
                }
                ActionId::TermString => {
                    let s = rs.max(0) as usize;
                    let e = re.max(0) as usize;
                    let value = if e >= s + 2
                        && (self.f[b] & 0x80) == 0
                        && self.sql.as_bytes()[s] == b'\''
                    {
                        self.sql[s + 1..e - 1].to_string()
                    } else {
                        crate::text::dequote(&self.sql[s..e])
                    };
                    self.h[b] = 1;
                    self.f[b] = 0;
                    self.v[b] = SemVal::Expr(Expr::String(crate::ast::StringLiteral {
                        span: Span::new(s, e),
                        value,
                    }));
                }
                ActionId::NExprListSingle => {
                    self.f[b] &= expr_flag::HAS_FUNC;
                    let item = match std::mem::take(&mut self.v[b]) {
                        SemVal::Expr(e) => e,
                        _ => Expr::Null {
                            span: Span::default(),
                        },
                    };
                    let mut list = Vec::with_capacity(2);
                    list.push(item);
                    self.v[b] = SemVal::Exprs(list);
                }
                _ => {
                    self.v[b] = run_action(action, self, b);
                }
            }
            if b > self.high_water {
                self.high_water = b;
            }
            needs_cleanup = false;
        } else {
            let pre_charged = if self.meter.is_some() && n >= 1 {
                let mut total = 0u64;
                for k in b..=self.top {
                    if self.en[k] > self.st[k]
                        && matches!(
                            self.v[k],
                            SemVal::Tok(
                                tk::STRING
                                    | tk::BLOB
                                    | tk::ID
                                    | tk::INTEGER
                                    | tk::FLOAT
                                    | tk::QNUMBER
                                    | tk::VARIABLE
                            )
                        )
                    {
                        let tok_len = (self.en[k] - self.st[k]) as u64;
                        if tok_len >= 256 {
                            if !self.charge_ast_mem(tok_len) {
                                return 0;
                            }
                            total = total.saturating_add(tok_len);
                        }
                    }
                }
                total
            } else {
                0
            };
            let val = run_action(action, self, b);
            if self.err.is_some() {
                self.v[b] = val;
                if b > self.high_water {
                    self.high_water = b;
                }
                return 0;
            }
            let mem_est = if self.meter.is_some() {
                let est = match &val {
                    SemVal::Expr(e) => {
                        let payload = match e {
                            Expr::String(s) => s.value.len() as u64,
                            Expr::Blob(b) => b.value.len() as u64,
                            Expr::Number(n) => n.text.len() as u64,
                            Expr::Column(c) => {
                                (c.schema.as_ref().map_or(0, |x| x.name.len())
                                    + c.table.as_ref().map_or(0, |x| x.name.len())
                                    + c.column.name.len()) as u64
                            }
                            Expr::Variable(v) => v.text.len() as u64,
                            Expr::Function(f) => f.name.name.len() as u64,
                            _ => 0,
                        };
                        (std::mem::size_of::<Expr>() as u64).saturating_add(payload)
                    }
                    SemVal::Exprs(_) | SemVal::ResultCols(_) | SemVal::ColDefs(_) => 128,
                    SemVal::Str(s) => 24u64.saturating_add(s.len() as u64),
                    SemVal::Name(nm) => 24u64.saturating_add(nm.name.len() as u64),
                    SemVal::CompoundAcc(_) | SemVal::Select(_) | SemVal::Stmt(_) => 192,
                    SemVal::Empty
                    | SemVal::Tok(_)
                    | SemVal::Bool(_)
                    | SemVal::Usize(_)
                    | SemVal::VtabArgSpan(_) => 0,
                    _ => 64,
                };
                est.saturating_sub(pre_charged)
            } else {
                0
            };
            self.v[b] = val;
            if b > self.high_water {
                self.high_water = b;
            }
            if mem_est > 0 && !self.charge_ast_mem(mem_est) {
                return 0;
            }
        }
        // Drop any remaining non-trivial RHS stack values above `b`.
        if needs_cleanup && n > 1 {
            for k in (b + 1)..=self.top {
                if !matches!(self.v[k], SemVal::Empty | SemVal::Tok(_)) {
                    self.v[k] = SemVal::Empty;
                }
            }
        }
        let act = t.find_reduce_action(self.state[b - 1], t.rule_lhs[rule]);
        self.top = b;
        self.state[b] = act;
        self.st[b] = rs;
        self.en[b] = re;
        act
    }
}
