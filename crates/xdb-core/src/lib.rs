//! Core primitives for XDB:
//!
//! - [`meter`]: deterministic step and memory budgets for sandboxed hosts (such as ZIPP).
//! - [`value`]: SQLite 3.54.1-exact scalar values, conversions, float formatting,
//!   affinity, collations, comparisons, casts, and arithmetic.
//! - [`vfs`]: synchronous storage I/O traits, SQLite 5-level lock manager, and
//!   quota-capped in-memory VFS.

#![forbid(unsafe_code)]

pub mod meter;
pub mod value;
pub mod vfs;

pub use meter::{BudgetMeter, LimitKind, Meter, NoopMeter, ResourceLimitError, ResourceLimits};
pub use value::{Affinity, SqlValue, StorageClass, TextEncoding};
pub use vfs::{
    ByteSource, BytesByteSource, FileByteSource, FileKind, IoError, IoErrorCode, IoResult,
    LockHandle, LockLevel, LockManager, LockSnapshot, MemoryVfs, MemoryVfsOptions, OpenFlags,
    OpenMode, Vfs, VfsFile,
};
