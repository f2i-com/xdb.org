//! Pure safe Rust SQLite 3.54.1 + XDB dialect LALR(1) SQL parser and
//! `build.c` schema model.
//!
//! - Uses static Lemon LALR(1) parse tables compiled directly into `.rodata`.
//! - Supports optional [`xdb_core::Meter`] step charging so hostile/deeply-nested
//!   SQL is deterministically bounded inside sandboxed hosts like ZIPP.

#![forbid(unsafe_code)]

mod actions;
pub mod ast;
pub mod errors;
mod lalr_tables;
pub mod parser;
pub mod schema;
pub mod text;
pub mod tokenizer;
pub mod tokens;

use xdb_core::Meter;

pub use actions::{dequote_number, DequotedNumber};
pub use ast::*;
pub use errors::{ParseResult, SqlParseError, SqlParseErrorKind};
pub use parser::{ParseOptions, ParsedAt, Parser};
pub use schema::{
    build_index, build_table, declared_type, expr_children, get_int32_token,
    is_constant_or_function, BuildOptions, CheckDefinition, ColumnDefinition, ColumnHidden,
    DeclaredType, ForeignKeyDefinition, IndexColumnDefinition, IndexDefinition, IndexOrigin,
    OnError, PrimaryKeyColumn, TableDefinition,
};
pub use text::{dequote, same_name, stored_type, StandardType, StoredType};
pub use tokenizer::{
    classify_context_keyword, classify_ilike, is_id_char, is_quote_char, is_sql_space,
    parser_tokens, raw_tokens, split_statements, sql_end, Dialect, Scanner, Token,
};
pub use tokens::{falls_back_to_id, keyword_code, tk, FALLBACK_TO_ID, TK_COUNT};

/// Parse one statement starting at byte `offset`, like `sqlite3_prepare`.
pub fn parse_statement_at(
    sql: &str,
    offset: usize,
    options: &ParseOptions,
) -> ParseResult<ParsedAt> {
    Parser::new(sql, options, None).parse_at(offset)
}

/// Parse one statement starting at byte `offset` under a sandbox [`Meter`].
pub fn parse_statement_at_metered(
    sql: &str,
    offset: usize,
    options: &ParseOptions,
    meter: &dyn Meter,
) -> ParseResult<ParsedAt> {
    Parser::new(sql, options, Some(meter)).parse_at(offset)
}

/// Parse exactly one SQL statement (optional trailing `;`, whitespace/comments allowed).
pub fn parse_statement(sql: &str, options: &ParseOptions) -> ParseResult<Statement> {
    parse_statement_inner(sql, options, None)
}

/// Parse exactly one SQL statement under a sandbox [`Meter`].
pub fn parse_statement_metered(
    sql: &str,
    options: &ParseOptions,
    meter: &dyn Meter,
) -> ParseResult<Statement> {
    parse_statement_inner(sql, options, Some(meter))
}

fn parse_statement_inner(
    sql: &str,
    options: &ParseOptions,
    meter: Option<&dyn Meter>,
) -> ParseResult<Statement> {
    let ParsedAt { statement, end } = Parser::new(sql, options, meter).parse_at(0)?;
    let Some(stmt) = statement else {
        return Err(SqlParseError::new(
            "no statement",
            SqlParseErrorKind::Unsupported,
            Some(0),
            Some(0),
        ));
    };
    let mut s = Scanner::new(sql, options.dialect);
    let mut i = end;
    loop {
        if let Some(m) = meter {
            m.charge_steps(1).map_err(SqlParseError::from_limit)?;
        }
        s.scan(i);
        if s.kind == tk::EOF {
            break;
        }
        if s.kind != tk::SPACE && s.kind != tk::COMMENT && s.kind != tk::SEMI {
            return Err(SqlParseError::new(
                "more than one statement",
                SqlParseErrorKind::Unsupported,
                Some(i),
                Some(sql.len()),
            ));
        }
        i = s.end;
    }
    Ok(stmt)
}

/// Parse a standalone expression (the whole input), as SQLite parses it inside `VALUES(...)`.
pub fn parse_expression(sql: &str, options: &ParseOptions) -> ParseResult<Expr> {
    parse_expression_inner(sql, options, None)
}

/// Parse a standalone expression under a sandbox [`Meter`].
pub fn parse_expression_metered(
    sql: &str,
    options: &ParseOptions,
    meter: &dyn Meter,
) -> ParseResult<Expr> {
    parse_expression_inner(sql, options, Some(meter))
}

fn parse_expression_inner(
    sql: &str,
    options: &ParseOptions,
    meter: Option<&dyn Meter>,
) -> ParseResult<Expr> {
    let select = Parser::new(sql, options, meter).parse_wrapped_expression()?;
    if select.with.is_some()
        || select.order_by.is_some()
        || select.order_by_all.is_some()
        || select.limit.is_some()
    {
        return Err(SqlParseError::new(
            "not an expression",
            SqlParseErrorKind::Syntax,
            Some(0),
            Some(sql.len()),
        ));
    }
    let SelectBody::Values(mut values) = select.body else {
        return Err(SqlParseError::new(
            "not an expression",
            SqlParseErrorKind::Syntax,
            Some(0),
            Some(sql.len()),
        ));
    };
    if values.rows.len() != 1 {
        return Err(SqlParseError::new(
            "not an expression",
            SqlParseErrorKind::Syntax,
            Some(0),
            Some(sql.len()),
        ));
    }
    let mut items = values.rows.pop().unwrap().items;
    if items.len() != 1 {
        let at = items[0].span().end;
        let mut s = Scanner::new(sql, options.dialect);
        let mut i = at;
        loop {
            s.scan(i);
            if s.kind != tk::SPACE && s.kind != tk::COMMENT {
                break;
            }
            i = s.end;
        }
        return Err(SqlParseError::new(
            format!("near \"{}\": syntax error", &sql[i..s.end]),
            SqlParseErrorKind::Syntax,
            Some(i),
            Some(s.end),
        ));
    }
    Ok(items.pop().unwrap())
}
