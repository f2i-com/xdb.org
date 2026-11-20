//! Standard filesystem [`Vfs`] backed by `std::fs` (zero `unsafe`).
//!
//! Provides synchronous file I/O with in-process SQLite 5-level locking via
//! [`LockManager`], matching XDB stages S1 (snapshot read) and S2 (exclusive write).

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use xdb_core::{
    IoError, IoErrorCode, IoResult, LockHandle, LockLevel, LockManager, OpenFlags, OpenMode, Vfs,
    VfsFile,
};

/// Native filesystem VFS over `std::fs`.
#[derive(Debug, Clone, Default)]
pub struct StdFsVfs {
    locks: LockManager,
}

impl StdFsVfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_lock_manager(locks: LockManager) -> Self {
        Self { locks }
    }

    pub fn lock_manager(&self) -> &LockManager {
        &self.locks
    }

    fn canonical_key(path: &str) -> String {
        match fs::canonicalize(path) {
            Ok(p) => format!("fs:{}", p.to_string_lossy()),
            Err(_) => format!("fs:{path}"),
        }
    }
}

fn map_open_err(err: &io::Error, path: &str) -> IoError {
    let code = match err.kind() {
        io::ErrorKind::PermissionDenied => IoErrorCode::Perm,
        io::ErrorKind::IsADirectory => IoErrorCode::CantOpenIsDir,
        _ => IoErrorCode::CantOpen,
    };
    IoError::with_path(code, err.to_string(), path)
}

impl Vfs for StdFsVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>> {
        let (file, readonly) = match flags.mode {
            OpenMode::ReadOnly => {
                let f = OpenOptions::new()
                    .read(true)
                    .open(path)
                    .map_err(|e| map_open_err(&e, path))?;
                (f, true)
            }
            OpenMode::ReadWrite => match OpenOptions::new().read(true).write(true).open(path) {
                Ok(f) => (f, false),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    // SQLite fallback: open read-only if write permission is denied.
                    let f = OpenOptions::new()
                        .read(true)
                        .open(path)
                        .map_err(|e2| map_open_err(&e2, path))?;
                    (f, true)
                }
                Err(e) => return Err(map_open_err(&e, path)),
            },
            OpenMode::Create => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                {
                    Ok(f) => (f, false),
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        let f = OpenOptions::new()
                            .read(true)
                            .open(path)
                            .map_err(|e2| map_open_err(&e2, path))?;
                        (f, true)
                    }
                    Err(e) => return Err(map_open_err(&e, path)),
                }
            }
            OpenMode::CreateExclusive => {
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(|e| map_open_err(&e, path))?;
                (f, false)
            }
        };

        if file
            .metadata()
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrFstat, e.to_string(), path))?
            .is_dir()
        {
            return Err(IoError::with_path(
                IoErrorCode::CantOpenIsDir,
                format!("cannot open directory as database: {path}"),
                path,
            ));
        }

        let lock = self.locks.open_handle(Self::canonical_key(path));
        Ok(Box::new(StdFsFile {
            path: path.to_string(),
            readonly,
            delete_on_close: flags.delete_on_close,
            file: Some(file),
            lock,
        }))
    }

    fn delete(&self, path: &str, _sync_dir: bool) -> IoResult<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(IoError::with_path(
                IoErrorCode::IoErrDeleteNoEnt,
                e.to_string(),
                path,
            )),
            Err(e) => Err(IoError::with_path(
                IoErrorCode::IoErrDelete,
                e.to_string(),
                path,
            )),
        }
    }

    fn exists(&self, path: &str) -> IoResult<bool> {
        Ok(Path::new(path).exists())
    }
}

#[derive(Debug)]
struct StdFsFile {
    path: String,
    readonly: bool,
    delete_on_close: bool,
    file: Option<File>,
    lock: LockHandle,
}

impl StdFsFile {
    fn file_mut(&mut self) -> IoResult<&mut File> {
        self.file.as_mut().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {}", self.path),
                &self.path,
            )
        })
    }

    fn file_ref(&self) -> IoResult<&File> {
        self.file.as_ref().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {}", self.path),
                &self.path,
            )
        })
    }
}

impl VfsFile for StdFsFile {
    fn path(&self) -> &str {
        &self.path
    }

    fn is_readonly(&self) -> bool {
        self.readonly
    }

    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<usize> {
        let path = self.path.clone();
        let f = self.file_mut()?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrRead, e.to_string(), &path))?;
        let mut total = 0usize;
        while total < buf.len() {
            match f.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(IoError::with_path(
                        IoErrorCode::IoErrRead,
                        e.to_string(),
                        &path,
                    ));
                }
            }
        }
        if total < buf.len() {
            buf[total..].fill(0);
        }
        Ok(total)
    }

    fn write(&mut self, buf: &[u8], offset: u64) -> IoResult<()> {
        if self.readonly {
            return Err(IoError::with_path(
                IoErrorCode::ReadOnly,
                format!("cannot write: file is open read-only: {}", self.path),
                &self.path,
            ));
        }
        if buf.is_empty() {
            self.file_ref()?;
            return Ok(());
        }
        let path = self.path.clone();
        let f = self.file_mut()?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrWrite, e.to_string(), &path))?;
        f.write_all(buf)
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrWrite, e.to_string(), &path))?;
        Ok(())
    }

    fn size(&self) -> IoResult<u64> {
        let f = self.file_ref()?;
        f.metadata()
            .map(|m| m.len())
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrFstat, e.to_string(), &self.path))
    }

    fn truncate(&mut self, size: u64) -> IoResult<()> {
        if self.readonly {
            return Err(IoError::with_path(
                IoErrorCode::ReadOnly,
                format!("cannot truncate: file is open read-only: {}", self.path),
                &self.path,
            ));
        }
        let path = self.path.clone();
        let f = self.file_mut()?;
        f.set_len(size)
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrTruncate, e.to_string(), &path))
    }

    fn sync(&mut self, data_only: bool) -> IoResult<()> {
        let path = self.path.clone();
        let f = self.file_ref()?;
        if self.readonly {
            return Ok(());
        }
        let res = if data_only {
            f.sync_data()
        } else {
            f.sync_all()
        };
        res.map_err(|e| IoError::with_path(IoErrorCode::IoErrFsync, e.to_string(), &path))
    }

    fn lock(&mut self, level: LockLevel) -> IoResult<bool> {
        self.file_ref()?;
        if self.readonly && level >= LockLevel::Reserved && level > self.lock.level() {
            return Err(IoError::with_path(
                IoErrorCode::IoErrLock,
                format!("cannot take a write lock on a read-only file: {}", self.path),
                &self.path,
            ));
        }
        self.lock.lock(level)
    }

    fn unlock(&mut self, level: LockLevel) -> IoResult<()> {
        self.file_ref()?;
        self.lock.unlock(level)
    }

    fn check_reserved_lock(&self) -> IoResult<bool> {
        self.file_ref()?;
        self.lock.check_reserved_lock()
    }

    fn lock_level(&self) -> LockLevel {
        self.lock.level()
    }

    fn close(&mut self) -> IoResult<()> {
        if self.file.is_none() {
            return Ok(());
        }
        self.lock.release();
        self.file = None;
        if self.delete_on_close {
            let _ = fs::remove_file(&self.path);
        }
        Ok(())
    }
}

impl Drop for StdFsFile {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
