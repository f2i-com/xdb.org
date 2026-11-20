//! Errors raised by the SQL parser and the `build.c` table/index model.

use std::fmt;

use xdb_core::ResourceLimitError;

/// Where a parse or DDL definition error originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlParseErrorKind {
    /// `near "X": syntax error`, `incomplete input`, `unrecognized token`, etc.
    Syntax,
    /// Errors SQLite raises while building a table/index definition (`build.c`).
    Definition,
    /// Valid SQL not supported by this entry point (e.g., multiple statements, `CREATE TABLE ... AS SELECT` in `build_table`).
    Unsupported,
    /// SQLite resource limits: `Recursion limit`, `Expression tree is too large`, etc.
    Limit,
    /// Sandbox `Meter` budget exceeded while parsing.
    ResourceLimit,
}

/// Error returned by SQL parsing or DDL definition building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlParseError {
    pub message: String,
    pub kind: SqlParseErrorKind,
    /// UTF-8 byte offset of the offending token/text, or `None`.
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// Underlying sandbox resource-limit error when `kind == SqlParseErrorKind::ResourceLimit`.
    pub resource_limit: Option<ResourceLimitError>,
}

impl SqlParseError {
    pub fn new(
        message: impl Into<String>,
        kind: SqlParseErrorKind,
        start: Option<usize>,
        end: Option<usize>,
    ) -> Self {
        Self {
            message: message.into(),
            kind,
            start,
            end,
            resource_limit: None,
        }
    }

    pub fn from_limit(err: ResourceLimitError) -> Self {
        Self {
            message: err.to_string(),
            kind: SqlParseErrorKind::ResourceLimit,
            start: None,
            end: None,
            resource_limit: Some(err),
        }
    }

    /// SQLite result code (`SQLITE_ERROR = 1`).
    pub const fn code(&self) -> u32 {
        1
    }
}

impl fmt::Display for SqlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SqlParseError {}

impl From<ResourceLimitError> for SqlParseError {
    fn from(err: ResourceLimitError) -> Self {
        Self::from_limit(err)
    }
}

pub type ParseResult<T> = Result<T, SqlParseError>;
