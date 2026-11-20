//! Synchronous storage I/O interfaces (`Vfs`, `VfsFile`, `ByteSource`),
//! SQLite-compatible error codes, 5-level [`LockManager`], and quota-enforced [`MemoryVfs`].

pub mod errors;
pub mod lock;
pub mod memory;

use std::fmt;

pub use errors::{IoError, IoErrorCode, IoResult};
pub use lock::{LockHandle, LockLevel, LockManager, LockSnapshot};
pub use memory::{MemoryVfs, MemoryVfsOptions, MEMORY_VFS_DEFAULT_MAX_FILE_SIZE};

/// How a file is opened (`SQLITE_OPEN_READONLY` / `READWRITE` / `CREATE` / `EXCLUSIVE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenMode {
    ReadOnly,
    ReadWrite,
    Create,
    CreateExclusive,
}

/// Purpose of the opened file (`SQLITE_OPEN_MAIN_DB`, etc.).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FileKind {
    #[default]
    MainDb,
    MainJournal,
    Wal,
    TempDb,
    TempJournal,
    TransientDb,
    SubJournal,
    SuperJournal,
    Other,
}

/// Flags passed to [`Vfs::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    pub mode: OpenMode,
    pub kind: FileKind,
    pub delete_on_close: bool,
}

impl OpenFlags {
    pub const fn readonly(kind: FileKind) -> Self {
        Self {
            mode: OpenMode::ReadOnly,
            kind,
            delete_on_close: false,
        }
    }

    pub const fn create(kind: FileKind) -> Self {
        Self {
            mode: OpenMode::Create,
            kind,
            delete_on_close: false,
        }
    }
}

/// A synchronous virtual file system (native disk, in-memory, sandboxed capability VFS, OPFS).
pub trait Vfs: fmt::Debug + Send + Sync {
    /// Open a file according to `flags`.
    fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>>;

    /// Delete a file. Returns `IoErrorCode::IoErrDeleteNoEnt` if absent.
    fn delete(&self, path: &str, sync_dir: bool) -> IoResult<()>;

    /// True if a file exists at `path` (including 0-byte files; callers checking
    /// SQLite's `xAccess(EXISTS)` semantics check `file.size() > 0`).
    fn exists(&self, path: &str) -> IoResult<bool>;
}

/// An open file handle. All operations are synchronous; dropping releases locks.
pub trait VfsFile: fmt::Debug + Send {
    fn path(&self) -> &str;
    fn is_readonly(&self) -> bool;

    /// Read up to `buf.len()` bytes starting at `offset`. Returns bytes actually read;
    /// the remainder of `buf` is zero-filled (`SQLITE_IOERR_SHORT_READ` contract).
    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<usize>;

    /// Write all of `buf` at `offset`, extending with zeros if needed.
    fn write(&mut self, buf: &[u8], offset: u64) -> IoResult<()>;

    /// Current logical file size in bytes.
    fn size(&self) -> IoResult<u64>;

    /// Resize the file (shrinking or extending with zeros).
    fn truncate(&mut self, size: u64) -> IoResult<()>;

    /// Flush changes to stable storage. No-op on read-only handles.
    fn sync(&mut self, data_only: bool) -> IoResult<()>;

    /// Raise the lock to `level`. Returns `Ok(false)` on contention (`SQLITE_BUSY`).
    fn lock(&mut self, level: LockLevel) -> IoResult<bool>;

    /// Lower the lock to `SHARED` or `NONE`.
    fn unlock(&mut self, level: LockLevel) -> IoResult<()>;

    /// True if any handle holds `RESERVED` or higher on this file.
    fn check_reserved_lock(&self) -> IoResult<bool>;

    /// Current lock level held by this handle.
    fn lock_level(&self) -> LockLevel;

    /// Explicitly close and release locks. Idempotent.
    fn close(&mut self) -> IoResult<()>;
}

/// Read-only random-access byte source for file-format readers (CSV, Parquet, etc.).
pub trait ByteSource: fmt::Debug + Send {
    fn size(&self) -> IoResult<u64>;
    fn read(&mut self, offset: u64, length: usize) -> IoResult<Vec<u8>>;
    fn cache_key(&self) -> Option<&str> {
        None
    }
}

/// `ByteSource` backed by an open [`VfsFile`].
#[derive(Debug)]
pub struct FileByteSource {
    file: Box<dyn VfsFile>,
    cache_key: Option<String>,
}

impl FileByteSource {
    pub fn new(file: Box<dyn VfsFile>, cache_key: Option<String>) -> Self {
        Self { file, cache_key }
    }
}

impl ByteSource for FileByteSource {
    fn size(&self) -> IoResult<u64> {
        self.file.size()
    }

    fn read(&mut self, offset: u64, length: usize) -> IoResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; length];
        let n = self.file.read(&mut buf, offset)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn cache_key(&self) -> Option<&str> {
        self.cache_key.as_deref()
    }
}

/// `ByteSource` backed by an owned in-memory byte vector.
#[derive(Debug, Clone)]
pub struct BytesByteSource {
    bytes: Vec<u8>,
    cache_key: Option<String>,
}

impl BytesByteSource {
    pub fn new(bytes: Vec<u8>, cache_key: Option<String>) -> Self {
        Self { bytes, cache_key }
    }
}

impl ByteSource for BytesByteSource {
    fn size(&self) -> IoResult<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read(&mut self, offset: u64, length: usize) -> IoResult<Vec<u8>> {
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if off >= self.bytes.len() || length == 0 {
            return Ok(Vec::new());
        }
        let end = self.bytes.len().min(off.saturating_add(length));
        Ok(self.bytes[off..end].to_vec())
    }

    fn cache_key(&self) -> Option<&str> {
        self.cache_key.as_deref()
    }
}
