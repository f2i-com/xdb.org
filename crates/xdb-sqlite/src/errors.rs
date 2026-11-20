//! SQLite format, schema, reader, I/O, and sandbox resource errors.

use std::fmt;

use xdb_core::{IoError, ResourceLimitError};

/// Format-level error codes (`SQLITE_CORRUPT = 11`, `SQLITE_NOTADB = 26`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqliteFormatErrorCode {
    Corrupt,
    NotADb,
}

impl SqliteFormatErrorCode {
    pub const fn result_code(self) -> u32 {
        match self {
            SqliteFormatErrorCode::Corrupt => 11,
            SqliteFormatErrorCode::NotADb => 26,
        }
    }

    pub const fn standard_message(self) -> &'static str {
        match self {
            SqliteFormatErrorCode::Corrupt => "database disk image is malformed",
            SqliteFormatErrorCode::NotADb => "file is not a database",
        }
    }
}

/// Unified error type for `xdb-sqlite`.
#[derive(Debug, Clone)]
pub enum SqliteError {
    Format {
        code: SqliteFormatErrorCode,
        detail: String,
    },
    /// `prepare.c corruptSchema` (`SQLITE_CORRUPT`): `"malformed database schema (NAME)[ - DETAIL]"`.
    SchemaCorrupt {
        object_name: String,
        reason: String,
    },
    /// `SQLITE_ERROR` raised while loading schema (e.g., `"unsupported file format"`).
    SchemaFormat(String),
    /// A `VIRTUAL` generated column was projected without the SQL expression engine.
    VirtualColumn {
        table: String,
        column: String,
    },
    /// Named table, index, or column does not exist (or cannot be read directly, e.g. view/vtab).
    ObjectNotFound {
        message: String,
        object_name: String,
    },
    /// Database or cursor used after `close()`.
    DatabaseClosed,
    /// Unknown collation sequence name.
    UnknownCollation(String),
    /// SQL parse or query execution error.
    Sql(String),
    Io(IoError),
    ResourceLimit(ResourceLimitError),
    Misuse(String),
}

impl SqliteError {
    pub fn corrupt(detail: impl Into<String>) -> Self {
        SqliteError::Format {
            code: SqliteFormatErrorCode::Corrupt,
            detail: detail.into(),
        }
    }

    pub fn not_a_db(detail: impl Into<String>) -> Self {
        SqliteError::Format {
            code: SqliteFormatErrorCode::NotADb,
            detail: detail.into(),
        }
    }

    pub fn schema_corrupt(object_name: Option<&str>, reason: impl Into<String>) -> Self {
        SqliteError::SchemaCorrupt {
            object_name: object_name.unwrap_or("?").to_string(),
            reason: reason.into(),
        }
    }

    pub fn schema_format(message: impl Into<String>) -> Self {
        SqliteError::SchemaFormat(message.into())
    }

    pub fn virtual_column(table: impl Into<String>, column: impl Into<String>) -> Self {
        SqliteError::VirtualColumn {
            table: table.into(),
            column: column.into(),
        }
    }

    pub fn object_not_found(message: impl Into<String>, object_name: impl Into<String>) -> Self {
        SqliteError::ObjectNotFound {
            message: message.into(),
            object_name: object_name.into(),
        }
    }

    pub fn misuse(detail: impl Into<String>) -> Self {
        SqliteError::Misuse(detail.into())
    }

    pub fn is_corrupt(&self) -> bool {
        matches!(
            self,
            SqliteError::Format {
                code: SqliteFormatErrorCode::Corrupt,
                ..
            } | SqliteError::SchemaCorrupt { .. }
        )
    }

    pub fn is_not_a_db(&self) -> bool {
        matches!(
            self,
            SqliteError::Format {
                code: SqliteFormatErrorCode::NotADb,
                ..
            }
        )
    }

    pub fn is_schema_corrupt(&self) -> bool {
        matches!(self, SqliteError::SchemaCorrupt { .. })
    }

    pub fn is_resource_limit(&self) -> bool {
        matches!(self, SqliteError::ResourceLimit(_))
    }

    /// SQLite result code (`SQLITE_CORRUPT = 11`, `SQLITE_NOTADB = 26`, `SQLITE_ERROR = 1`, etc.).
    pub fn result_code(&self) -> u32 {
        match self {
            SqliteError::Format { code, .. } => code.result_code(),
            SqliteError::SchemaCorrupt { .. } => 11,
            SqliteError::SchemaFormat(_)
            | SqliteError::VirtualColumn { .. }
            | SqliteError::ObjectNotFound { .. }
            | SqliteError::UnknownCollation(_)
            | SqliteError::Sql(_) => 1,
            SqliteError::DatabaseClosed | SqliteError::Misuse(_) => 21,
            SqliteError::Io(_) => 10,
            SqliteError::ResourceLimit(_) => 9, // SQLITE_INTERRUPT
        }
    }
}

impl fmt::Display for SqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqliteError::Format { code, detail } => {
                write!(f, "{}: {}", code.standard_message(), detail)
            }
            SqliteError::SchemaCorrupt {
                object_name,
                reason,
            } => {
                if reason.is_empty() {
                    write!(f, "malformed database schema ({object_name})")
                } else {
                    write!(f, "malformed database schema ({object_name}) - {reason}")
                }
            }
            SqliteError::SchemaFormat(msg) => f.write_str(msg),
            SqliteError::VirtualColumn { table, column } => write!(
                f,
                "column \"{column}\" of table \"{table}\" is a VIRTUAL generated column: \
                 it is not stored in the file and computing it needs the SQL expression engine \
                 (project only stored columns)"
            ),
            SqliteError::ObjectNotFound { message, .. } => f.write_str(message),
            SqliteError::DatabaseClosed => f.write_str("the database is closed"),
            SqliteError::UnknownCollation(name) => {
                write!(f, "no such collation sequence: {name}")
            }
            SqliteError::Sql(msg) => f.write_str(msg),
            SqliteError::Io(e) => write!(f, "{e}"),
            SqliteError::ResourceLimit(e) => write!(f, "{e}"),
            SqliteError::Misuse(msg) => write!(f, "misuse: {msg}"),
        }
    }
}

impl std::error::Error for SqliteError {}

impl From<IoError> for SqliteError {
    fn from(e: IoError) -> Self {
        SqliteError::Io(e)
    }
}

impl From<ResourceLimitError> for SqliteError {
    fn from(e: ResourceLimitError) -> Self {
        SqliteError::ResourceLimit(e)
    }
}

impl From<xdb_sql::SqlError> for SqliteError {
    fn from(e: xdb_sql::SqlError) -> Self {
        match e {
            xdb_sql::SqlError::ResourceLimit(r) => SqliteError::ResourceLimit(r),
            other => SqliteError::Sql(other.to_string()),
        }
    }
}

pub type SqliteResult<T> = Result<T, SqliteError>;
