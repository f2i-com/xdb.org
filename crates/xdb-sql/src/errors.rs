//! Error types for the `xdb-sql` query engine.

use std::fmt;

use xdb_core::ResourceLimitError;
use xdb_parser::SqlParseError;

/// Error returned while parsing, binding, planning, or executing SQL queries.
#[derive(Debug)]
pub enum SqlError {
    /// SQL syntax or dialect parse error.
    Parse(SqlParseError),
    /// Runtime SQL execution error (e.g., division/overflow, invalid function args,
    /// scalar subquery returning >1 row, generated column cycle, missing table/column).
    Runtime(String),
    /// String or blob exceeded configured SQLite length limit (`SQLITE_TOOBIG`).
    TooBig(String),
    /// Storage / catalog backend error.
    Storage(String),
    /// Unsupported SQL feature (e.g., DML/DDL in a read-only SELECT execution).
    Unsupported(String),
    /// Deterministic sandbox resource limit reached (steps, memory, or page reads).
    ResourceLimit(ResourceLimitError),
    /// Trigger `RAISE(IGNORE | ROLLBACK | ABORT | FAIL [, message])` signal.
    Raise {
        action: xdb_parser::RaiseAction,
        message: Option<String>,
    },
}

impl SqlError {
    #[inline]
    pub fn runtime(msg: impl Into<String>) -> Self {
        SqlError::Runtime(msg.into())
    }

    #[inline]
    pub fn too_big() -> Self {
        SqlError::TooBig("string or blob too big".to_string())
    }

    #[inline]
    pub fn storage(msg: impl Into<String>) -> Self {
        SqlError::Storage(msg.into())
    }

    #[inline]
    pub fn unsupported(msg: impl Into<String>) -> Self {
        SqlError::Unsupported(msg.into())
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Parse(e) => write!(f, "{e}"),
            SqlError::Runtime(msg)
            | SqlError::TooBig(msg)
            | SqlError::Storage(msg)
            | SqlError::Unsupported(msg) => f.write_str(msg),
            SqlError::ResourceLimit(e) => write!(f, "{e}"),
            SqlError::Raise { action, message } => match message {
                Some(m) => f.write_str(m),
                None => match action {
                    xdb_parser::RaiseAction::Ignore => f.write_str("RAISE(IGNORE)"),
                    _ => f.write_str("constraint failed"),
                },
            },
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlError::Parse(e) => Some(e),
            SqlError::ResourceLimit(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SqlParseError> for SqlError {
    fn from(e: SqlParseError) -> Self {
        if let Some(lim) = e.resource_limit {
            SqlError::ResourceLimit(lim)
        } else {
            SqlError::Parse(e)
        }
    }
}

impl From<ResourceLimitError> for SqlError {
    fn from(e: ResourceLimitError) -> Self {
        SqlError::ResourceLimit(e)
    }
}

/// Convenience result alias for `xdb-sql`.
pub type SqlResult<T> = Result<T, SqlError>;
