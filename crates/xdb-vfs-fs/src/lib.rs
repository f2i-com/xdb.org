//! Standard filesystem [`Vfs`] backed by `std::fs` (zero `unsafe`).
//!
//! Provides synchronous file I/O with in-process SQLite 5-level locking via
//! [`LockManager`], matching XDB stages S1 (snapshot read) and S2 (exclusive write).

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

    fn lock_key(file: &File, path: &str) -> String {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(m) = file.metadata() {
                return format!("fs:unix:{}:{}", m.dev(), m.ino());
            }
        }
        #[cfg(not(unix))]
        let _ = file;
        match fs::canonicalize(path) {
            Ok(p) => format!("fs:{}", p.to_string_lossy()),
            Err(_) => format!("fs:{path}"),
        }
    }
}

fn map_open_err(err: &io::Error, path: &str) -> IoError {
    let code = match err.kind() {
        io::ErrorKind::IsADirectory => IoErrorCode::CantOpenIsDir,
        io::ErrorKind::PermissionDenied => {
            if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
                IoErrorCode::CantOpenIsDir
            } else {
                IoErrorCode::Perm
            }
        }
        _ => IoErrorCode::CantOpen,
    };
    IoError::with_path(code, err.to_string(), path)
}

fn is_readonly_fallback_err(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    )
}

impl Vfs for StdFsVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>> {
        let (file, readonly, kernel_delete_on_close) = match flags.mode {
            OpenMode::ReadOnly => {
                let f = OpenOptions::new()
                    .read(true)
                    .open(path)
                    .map_err(|e| map_open_err(&e, path))?;
                (f, true, false)
            }
            OpenMode::ReadWrite => match OpenOptions::new().read(true).write(true).open(path) {
                Ok(f) => (f, false, false),
                Err(e) if is_readonly_fallback_err(e.kind()) => {
                    // SQLite fallback: open read-only if write permission or ROFS blocks write.
                    let f = OpenOptions::new()
                        .read(true)
                        .open(path)
                        .map_err(|e2| map_open_err(&e2, path))?;
                    (f, true, false)
                }
                Err(e) => return Err(map_open_err(&e, path)),
            },
            OpenMode::Create => {
                #[cfg(windows)]
                let tried_new = if flags.delete_on_close {
                    use std::os::windows::fs::OpenOptionsExt;
                    // Try atomic CREATE_NEW + FILE_FLAG_DELETE_ON_CLOSE first so new temp
                    // files are kernel-managed and immune to NTFS CreationTime tunneling.
                    let mut opts = OpenOptions::new();
                    opts.read(true)
                        .write(true)
                        .create_new(true)
                        .custom_flags(0x0400_0000)
                        .share_mode(1 | 2 | 4);
                    opts.open(path).ok().map(|f| (f, false, true))
                } else {
                    None
                };
                #[cfg(not(windows))]
                let tried_new: Option<(File, bool, bool)> = None;
                if let Some(res) = tried_new {
                    res
                } else {
                    match OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(path)
                    {
                        Ok(f) => (f, false, false),
                        Err(e) if is_readonly_fallback_err(e.kind()) => {
                            let f = OpenOptions::new()
                                .read(true)
                                .open(path)
                                .map_err(|e2| map_open_err(&e2, path))?;
                            (f, true, false)
                        }
                        Err(e) => return Err(map_open_err(&e, path)),
                    }
                }
            }
            OpenMode::CreateExclusive => {
                let mut opts = OpenOptions::new();
                opts.read(true).write(true).create_new(true);
                #[cfg(windows)]
                let kdoc = if flags.delete_on_close {
                    use std::os::windows::fs::OpenOptionsExt;
                    // FILE_FLAG_DELETE_ON_CLOSE (0x0400_0000); safe for CREATE_NEW since
                    // Windows never follows an existing symlink with CREATE_NEW.
                    opts.custom_flags(0x0400_0000).share_mode(1 | 2 | 4);
                    true
                } else {
                    false
                };
                #[cfg(not(windows))]
                let kdoc = false;
                let f = opts.open(path).map_err(|e| map_open_err(&e, path))?;
                (f, false, kdoc)
            }
        };

        let file_meta = file
            .metadata()
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrFstat, e.to_string(), path))?;
        if file_meta.is_dir() {
            return Err(IoError::with_path(
                IoErrorCode::CantOpenIsDir,
                format!("cannot open directory as database: {path}"),
                path,
            ));
        }

        let lock = self.locks.open_handle(Self::lock_key(&file, path));
        // Never unlink a file that fell back to read-only access or is kernel-managed.
        let effective_delete_on_close =
            flags.delete_on_close && !readonly && !kernel_delete_on_close;
        // Resolve the parent directory without following a symlink in the final component so
        // `delete_on_close` unlinks the entry itself rather than a symlink's target.
        let delete_path = if effective_delete_on_close {
            resolve_no_follow_final(Path::new(path))
        } else {
            PathBuf::from(path)
        };
        let (was_symlink, link_identity, opened_identity) = if effective_delete_on_close {
            let sym_meta = fs::symlink_metadata(&delete_path).ok();
            let is_sym = sym_meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
            let link_id = file_identity_from_meta(&sym_meta);
            let open_id = file_identity_from_meta(&Some(file_meta));
            (is_sym, link_id, open_id)
        } else {
            (false, None, None)
        };
        Ok(Box::new(StdFsFile {
            path: path.to_string(),
            delete_path,
            was_symlink,
            link_identity,
            opened_identity,
            readonly,
            delete_on_close: effective_delete_on_close,
            file: Some(file),
            lock,
        }))
    }

    fn delete(&self, path: &str, sync_dir: bool) -> IoResult<()> {
        match fs::remove_file(path) {
            Ok(()) => {
                #[cfg(unix)]
                if sync_dir {
                    let parent = Path::new(path)
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    if let Ok(dir) = File::open(parent) {
                        let _ = dir.sync_all();
                    }
                }
                #[cfg(not(unix))]
                let _ = sync_dir;
                Ok(())
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
                Err(IoError::with_path(
                    IoErrorCode::IoErrDeleteNoEnt,
                    e.to_string(),
                    path,
                ))
            }
            Err(e) => Err(IoError::with_path(
                IoErrorCode::IoErrDelete,
                e.to_string(),
                path,
            )),
        }
    }

    fn exists(&self, path: &str) -> IoResult<bool> {
        match fs::metadata(path) {
            Ok(m) => Ok(!m.is_dir()),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
                Ok(false)
            }
            Err(e) => Err(IoError::with_path(
                IoErrorCode::IoErrAccess,
                e.to_string(),
                path,
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { dev: u64, ino: u64 },
    #[cfg(not(unix))]
    Created(std::time::SystemTime),
}

fn file_identity_from_meta(meta: &Option<fs::Metadata>) -> Option<FileIdentity> {
    let m = meta.as_ref()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity::Unix {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        m.created().ok().map(FileIdentity::Created)
    }
}

fn resolve_no_follow_final(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    if let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) {
        if let Ok(canon_parent) = fs::canonicalize(parent) {
            return canon_parent.join(name);
        }
    }
    abs
}

#[derive(Debug)]
struct StdFsFile {
    path: String,
    delete_path: PathBuf,
    was_symlink: bool,
    link_identity: Option<FileIdentity>,
    opened_identity: Option<FileIdentity>,
    readonly: bool,
    delete_on_close: bool,
    file: Option<File>,
    lock: LockHandle,
}

impl StdFsFile {
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
        let Self { path, file, .. } = self;
        let f = file.as_mut().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {path}"),
                path.as_str(),
            )
        })?;
        if buf.is_empty() {
            return Ok(0);
        }
        if offset > i64::MAX as u64 {
            buf.fill(0);
            return Ok(0);
        }
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrRead, e.to_string(), path.as_str()))?;
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
                        path.as_str(),
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
        let Self {
            path,
            readonly,
            file,
            ..
        } = self;
        let f = file.as_mut().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {path}"),
                path.as_str(),
            )
        })?;
        if *readonly {
            return Err(IoError::with_path(
                IoErrorCode::ReadOnly,
                format!("cannot write: file is open read-only: {path}"),
                path.as_str(),
            ));
        }
        if buf.is_empty() {
            return Ok(());
        }
        f.seek(SeekFrom::Start(offset)).map_err(|e| {
            IoError::with_path(IoErrorCode::IoErrWrite, e.to_string(), path.as_str())
        })?;
        f.write_all(buf).map_err(|e| {
            let code = if e.kind() == io::ErrorKind::StorageFull {
                IoErrorCode::Full
            } else {
                IoErrorCode::IoErrWrite
            };
            IoError::with_path(code, e.to_string(), path.as_str())
        })?;
        Ok(())
    }

    fn size(&self) -> IoResult<u64> {
        let f = self.file_ref()?;
        f.metadata()
            .map(|m| m.len())
            .map_err(|e| IoError::with_path(IoErrorCode::IoErrFstat, e.to_string(), &self.path))
    }

    fn truncate(&mut self, size: u64) -> IoResult<()> {
        let Self {
            path,
            readonly,
            file,
            ..
        } = self;
        let f = file.as_mut().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {path}"),
                path.as_str(),
            )
        })?;
        if *readonly {
            return Err(IoError::with_path(
                IoErrorCode::ReadOnly,
                format!("cannot truncate: file is open read-only: {path}"),
                path.as_str(),
            ));
        }
        f.set_len(size).map_err(|e| {
            let code = if e.kind() == io::ErrorKind::StorageFull {
                IoErrorCode::Full
            } else {
                IoErrorCode::IoErrTruncate
            };
            IoError::with_path(code, e.to_string(), path.as_str())
        })
    }

    fn sync(&mut self, data_only: bool) -> IoResult<()> {
        let Self {
            path,
            readonly,
            file,
            ..
        } = self;
        let f = file.as_ref().ok_or_else(|| {
            IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {path}"),
                path.as_str(),
            )
        })?;
        if *readonly {
            return Ok(());
        }
        let res = if data_only {
            f.sync_data()
        } else {
            f.sync_all()
        };
        res.map_err(|e| IoError::with_path(IoErrorCode::IoErrFsync, e.to_string(), path.as_str()))
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
        let same_entry = if self.delete_on_close {
            match fs::symlink_metadata(&self.delete_path).ok() {
                Some(cur_meta) => {
                    let cur_is_sym = cur_meta.file_type().is_symlink();
                    if cur_is_sym != self.was_symlink {
                        false
                    } else if cur_is_sym {
                        let link_ok = match self.link_identity {
                            Some(expected) => file_identity_from_meta(&Some(cur_meta)) == Some(expected),
                            None => false,
                        };
                        let target_ok = match self.opened_identity {
                            Some(expected) => {
                                file_identity_from_meta(&fs::metadata(&self.delete_path).ok())
                                    == Some(expected)
                            }
                            None => false,
                        };
                        link_ok && target_ok
                    } else {
                        match self.opened_identity {
                            Some(expected) => {
                                file_identity_from_meta(&Some(cur_meta)) == Some(expected)
                            }
                            None => false,
                        }
                    }
                }
                None => false,
            }
        } else {
            false
        };
        #[cfg(unix)]
        {
            if self.delete_on_close && same_entry {
                let _ = fs::remove_file(&self.delete_path);
            }
            self.file = None;
        }
        #[cfg(not(unix))]
        {
            self.file = None;
            if self.delete_on_close && same_entry {
                let _ = fs::remove_file(&self.delete_path);
            }
        }
        self.lock.release();
        Ok(())
    }
}

impl Drop for StdFsFile {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pass6_stdfs_read_bounds_and_delete_on_close() {
        let vfs = StdFsVfs::new();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "xdb_vfs_fs_pass6_{}_{}.tmp",
            std::process::id(),
            unique
        ));
        let path_str = path.to_string_lossy().into_owned();

        let mut flags = OpenFlags::create(xdb_core::FileKind::TempDb);
        flags.mode = OpenMode::CreateExclusive;
        flags.delete_on_close = true;
        let mut f = vfs.open(&path_str, flags).expect("open exclusive delete_on_close");
        f.write(b"hello", 0).unwrap();

        // Empty buffer read returns Ok(0) even at extreme offset without seeking error.
        let mut empty = [];
        assert_eq!(f.read(&mut empty, u64::MAX).unwrap(), 0);

        // Offset > i64::MAX zero-fills buffer and returns Ok(0) (SQLite short-read contract).
        let mut buf = [0xAAu8; 8];
        assert_eq!(f.read(&mut buf, (i64::MAX as u64) + 1).unwrap(), 0);
        assert_eq!(buf, [0u8; 8]);

        // Normal read returns written bytes.
        let mut out = [0u8; 5];
        assert_eq!(f.read(&mut out, 0).unwrap(), 5);
        assert_eq!(&out, b"hello");

        f.close().unwrap();
        assert!(!path.exists(), "delete_on_close file should be removed after close");
    }

    #[test]
    fn pass7_stdfs_create_delete_on_close_atomic() {
        let vfs = StdFsVfs::new();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "xdb_vfs_fs_pass7_create_{}_{}.tmp",
            std::process::id(),
            unique
        ));
        let path_str = path.to_string_lossy().into_owned();

        let mut flags = OpenFlags::create(xdb_core::FileKind::TempDb);
        flags.mode = OpenMode::Create;
        flags.delete_on_close = true;
        let mut f = vfs.open(&path_str, flags).expect("open create delete_on_close");
        f.write(b"pass7_atomic_del", 0).unwrap();
        let mut out = [0u8; 16];
        assert_eq!(f.read(&mut out, 0).unwrap(), 16);
        assert_eq!(&out, b"pass7_atomic_del");
        f.close().unwrap();
        assert!(!path.exists(), "OpenMode::Create delete_on_close should clean up on close");
    }
}
