//! SQLite-style VFS I/O error codes and error type (`sqlite.h.in:451-561`).

use std::fmt;

const SQLITE_IOERR: u32 = 10;

const fn ioerr(n: u32) -> u32 {
    SQLITE_IOERR | (n << 8)
}

/// SQLite-style I/O result-code names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoErrorCode {
    Error,
    Perm,
    Busy,
    ReadOnly,
    IoErr,
    IoErrRead,
    IoErrShortRead,
    IoErrWrite,
    IoErrFsync,
    IoErrDirFsync,
    IoErrTruncate,
    IoErrFstat,
    IoErrUnlock,
    IoErrRdLock,
    IoErrDelete,
    IoErrNoMem,
    IoErrAccess,
    IoErrCheckReservedLock,
    IoErrLock,
    IoErrClose,
    IoErrDirClose,
    IoErrDeleteNoEnt,
    IoErrCorruptFs,
    Full,
    CantOpen,
    CantOpenIsDir,
    CantOpenFullPath,
    Misuse,
}

impl IoErrorCode {
    /// Numeric SQLite (extended) result code (`sqlite.h.in:451-552`).
    pub const fn result_code(self) -> u32 {
        match self {
            IoErrorCode::Error => 1,
            IoErrorCode::Perm => 3,
            IoErrorCode::Busy => 5,
            IoErrorCode::ReadOnly => 8,
            IoErrorCode::IoErr => SQLITE_IOERR,
            IoErrorCode::IoErrRead => ioerr(1),
            IoErrorCode::IoErrShortRead => ioerr(2),
            IoErrorCode::IoErrWrite => ioerr(3),
            IoErrorCode::IoErrFsync => ioerr(4),
            IoErrorCode::IoErrDirFsync => ioerr(5),
            IoErrorCode::IoErrTruncate => ioerr(6),
            IoErrorCode::IoErrFstat => ioerr(7),
            IoErrorCode::IoErrUnlock => ioerr(8),
            IoErrorCode::IoErrRdLock => ioerr(9),
            IoErrorCode::IoErrDelete => ioerr(10),
            IoErrorCode::IoErrNoMem => ioerr(12),
            IoErrorCode::IoErrAccess => ioerr(13),
            IoErrorCode::IoErrCheckReservedLock => ioerr(14),
            IoErrorCode::IoErrLock => ioerr(15),
            IoErrorCode::IoErrClose => ioerr(16),
            IoErrorCode::IoErrDirClose => ioerr(17),
            IoErrorCode::IoErrDeleteNoEnt => ioerr(23),
            IoErrorCode::IoErrCorruptFs => ioerr(33),
            IoErrorCode::Full => 13,
            IoErrorCode::CantOpen => 14,
            IoErrorCode::CantOpenIsDir => 14 | (2 << 8),
            IoErrorCode::CantOpenFullPath => 14 | (3 << 8),
            IoErrorCode::Misuse => 21,
        }
    }
}

/// An I/O failure carrying a SQLite result code and optional path.
#[derive(Debug, Clone)]
pub struct IoError {
    pub code: IoErrorCode,
    pub message: String,
    pub path: Option<String>,
}

impl IoError {
    pub fn new(code: IoErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }

    pub fn with_path(
        code: IoErrorCode,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            path: Some(path.into()),
        }
    }

    #[inline]
    pub const fn result_code(&self) -> u32 {
        self.code.result_code()
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} ({}): {}", self.code, self.result_code(), self.message)
    }
}

impl std::error::Error for IoError {}

pub type IoResult<T> = Result<T, IoError>;
