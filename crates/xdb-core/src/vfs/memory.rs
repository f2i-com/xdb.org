//! Quota-enforced in-memory [`Vfs`] with SQLite 5-level locking.
//!
//! Designed for tests, temporary databases, WASM, and sandboxed hosts (such as ZIPP)
//! that grant an isolated in-memory filesystem with strict per-file and total byte caps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::errors::{IoError, IoErrorCode, IoResult};
use super::lock::{LockHandle, LockLevel, LockManager};
use super::{OpenFlags, OpenMode, Vfs, VfsFile};

/// SQLite's default memdb size limit (`SQLITE_MEMDB_DEFAULT_MAXSIZE`, 1 GiB).
pub const MEMORY_VFS_DEFAULT_MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

static INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`MemoryVfs`].
#[derive(Debug, Clone)]
pub struct MemoryVfsOptions {
    pub lock_manager: Option<LockManager>,
    /// Maximum size of any single file in bytes.
    pub max_file_size: usize,
    /// Optional aggregate quota across all live files in this VFS (for sandboxes).
    pub max_total_bytes: Option<usize>,
}

impl Default for MemoryVfsOptions {
    fn default() -> Self {
        Self {
            lock_manager: None,
            max_file_size: MEMORY_VFS_DEFAULT_MAX_FILE_SIZE,
            max_total_bytes: None,
        }
    }
}

#[derive(Debug)]
struct MemNode {
    id: u64,
    data: Vec<u8>,
    linked: bool,
    open_handles: usize,
}

#[derive(Debug)]
struct MemFsState {
    files: HashMap<String, Arc<Mutex<MemNode>>>,
    total_bytes: usize,
    max_file_size: usize,
    max_total_bytes: Option<usize>,
    next_node_id: u64,
}

impl MemFsState {
    fn new_node(&mut self, data: Vec<u8>) -> Arc<Mutex<MemNode>> {
        self.next_node_id += 1;
        Arc::new(Mutex::new(MemNode {
            id: self.next_node_id,
            data,
            linked: true,
            open_handles: 0,
        }))
    }

    fn check_and_grow(
        &mut self,
        node: &mut MemNode,
        new_size: usize,
        path: &str,
    ) -> IoResult<()> {
        if new_size > self.max_file_size {
            return Err(IoError::with_path(
                IoErrorCode::Full,
                format!("file would exceed max_file_size ({})", self.max_file_size),
                path,
            ));
        }
        let old_size = node.data.len();
        if new_size > old_size {
            let delta = new_size - old_size;
            let next_total = self.total_bytes.saturating_add(delta);
            if let Some(cap) = self.max_total_bytes {
                if next_total > cap {
                    return Err(IoError::with_path(
                        IoErrorCode::Full,
                        format!("VFS would exceed max_total_bytes ({cap})"),
                        path,
                    ));
                }
            }
            node.data.resize(new_size, 0);
            self.total_bytes = next_total;
        }
        Ok(())
    }

    fn shrink(&mut self, node: &mut MemNode, new_size: usize) {
        let old_size = node.data.len();
        if new_size < old_size {
            let delta = old_size - new_size;
            if new_size == 0 {
                node.data = Vec::new();
            } else {
                node.data.truncate(new_size);
                if node.data.capacity() > new_size.saturating_mul(2).max(4096) {
                    node.data.shrink_to_fit();
                }
            }
            self.total_bytes = self.total_bytes.saturating_sub(delta);
        }
    }

    fn reclaim_if_dead(&mut self, node: &mut MemNode) {
        if !node.linked && node.open_handles == 0 {
            let len = node.data.len();
            node.data = Vec::new();
            self.total_bytes = self.total_bytes.saturating_sub(len);
        }
    }
}

/// Pure safe Rust in-memory [`Vfs`] with SQLite locking and optional byte quotas.
#[derive(Debug, Clone)]
pub struct MemoryVfs {
    state: Arc<Mutex<MemFsState>>,
    locks: LockManager,
    key_prefix: String,
}

impl Default for MemoryVfs {
    fn default() -> Self {
        Self::new(MemoryVfsOptions::default())
    }
}

impl MemoryVfs {
    pub fn new(options: MemoryVfsOptions) -> Self {
        let id = INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            state: Arc::new(Mutex::new(MemFsState {
                files: HashMap::new(),
                total_bytes: 0,
                max_file_size: options.max_file_size,
                max_total_bytes: options.max_total_bytes,
                next_node_id: 0,
            })),
            locks: options.lock_manager.unwrap_or_default(),
            key_prefix: format!("mem#{id}:"),
        }
    }

    pub fn lock_manager(&self) -> &LockManager {
        &self.locks
    }

    /// Return the lock-table key currently associated with `path`.
    /// If `path` exists in this VFS, returns the active node's lock key (matching [`Vfs::open`]);
    /// otherwise returns the fallback path-based key.
    pub fn lock_key(&self, path: &str) -> String {
        let st = self.state.lock().expect("vfs poisoned");
        if let Some(node) = st.files.get(path) {
            let nid = node.lock().expect("node poisoned").id;
            return self.node_lock_key(nid);
        }
        format!("{}{}", self.key_prefix, path)
    }

    fn node_lock_key(&self, node_id: u64) -> String {
        format!("{}node#{node_id}", self.key_prefix)
    }

    pub fn total_bytes(&self) -> usize {
        self.state.lock().expect("vfs poisoned").total_bytes
    }

    /// Return a copy of `path`'s contents if present.
    pub fn get_file(&self, path: &str) -> Option<Vec<u8>> {
        let st = self.state.lock().expect("vfs poisoned");
        let node = st.files.get(path)?.clone();
        drop(st);
        let data = node.lock().expect("node poisoned").data.clone();
        Some(data)
    }

    /// Create or replace `path`'s contents synchronously (enforcing quotas).
    pub fn set_file(&self, path: impl Into<String>, bytes: Vec<u8>) -> IoResult<()> {
        let path = path.into();
        let mut st = self.state.lock().expect("vfs poisoned");
        if bytes.len() > st.max_file_size {
            return Err(IoError::with_path(
                IoErrorCode::Full,
                format!("file would exceed max_file_size ({})", st.max_file_size),
                &path,
            ));
        }
        let existing = st.files.get(&path).cloned();
        let old_len = existing
            .as_ref()
            .map_or(0, |n| n.lock().expect("node poisoned").data.len());
        let new_len = bytes.len();
        let next_total = st.total_bytes.saturating_sub(old_len).saturating_add(new_len);
        if let Some(cap) = st.max_total_bytes {
            if next_total > cap {
                return Err(IoError::with_path(
                    IoErrorCode::Full,
                    format!("VFS would exceed max_total_bytes ({cap})"),
                    &path,
                ));
            }
        }
        match existing {
            Some(node) => {
                node.lock().expect("node poisoned").data = bytes;
            }
            None => {
                let node = st.new_node(bytes);
                st.files.insert(path, node);
            }
        }
        st.total_bytes = next_total;
        Ok(())
    }
}

impl Vfs for MemoryVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>> {
        let mut st = self.state.lock().expect("vfs poisoned");
        let existing = st.files.get(path).cloned();
        let node = match flags.mode {
            OpenMode::ReadOnly | OpenMode::ReadWrite => match existing {
                Some(n) => n,
                None => {
                    return Err(IoError::with_path(
                        IoErrorCode::CantOpen,
                        format!("unable to open file (no such file): {path}"),
                        path,
                    ));
                }
            },
            OpenMode::Create => match existing {
                Some(n) => n,
                None => {
                    let n = st.new_node(Vec::new());
                    st.files.insert(path.to_string(), n.clone());
                    n
                }
            },
            OpenMode::CreateExclusive => {
                if existing.is_some() {
                    return Err(IoError::with_path(
                        IoErrorCode::CantOpen,
                        format!("unable to create file (file exists): {path}"),
                        path,
                    ));
                }
                let n = st.new_node(Vec::new());
                st.files.insert(path.to_string(), n.clone());
                n
            }
        };
        let node_id = {
            let mut guard = node.lock().expect("node poisoned");
            guard.open_handles += 1;
            guard.id
        };
        drop(st);

        let lock = self.locks.open_handle(self.node_lock_key(node_id));
        Ok(Box::new(MemoryFile {
            path: path.to_string(),
            readonly: flags.mode == OpenMode::ReadOnly,
            delete_on_close: flags.delete_on_close,
            node,
            state: self.state.clone(),
            lock,
            closed: false,
        }))
    }

    fn delete(&self, path: &str, _sync_dir: bool) -> IoResult<()> {
        let mut st = self.state.lock().expect("vfs poisoned");
        match st.files.remove(path) {
            Some(node) => {
                let mut guard = node.lock().expect("node poisoned");
                guard.linked = false;
                st.reclaim_if_dead(&mut guard);
                Ok(())
            }
            None => Err(IoError::with_path(
                IoErrorCode::IoErrDeleteNoEnt,
                format!("cannot delete (no such file): {path}"),
                path,
            )),
        }
    }

    fn exists(&self, path: &str) -> IoResult<bool> {
        let st = self.state.lock().expect("vfs poisoned");
        Ok(st.files.contains_key(path))
    }
}

#[derive(Debug)]
struct MemoryFile {
    path: String,
    readonly: bool,
    delete_on_close: bool,
    node: Arc<Mutex<MemNode>>,
    state: Arc<Mutex<MemFsState>>,
    lock: LockHandle,
    closed: bool,
}

impl MemoryFile {
    fn ensure_live(&self) -> IoResult<()> {
        if self.closed {
            Err(IoError::with_path(
                IoErrorCode::Misuse,
                format!("file is closed: {}", self.path),
                &self.path,
            ))
        } else {
            Ok(())
        }
    }

    fn ensure_writable(&self, op: &str) -> IoResult<()> {
        self.ensure_live()?;
        if self.readonly {
            Err(IoError::with_path(
                IoErrorCode::ReadOnly,
                format!("cannot {op}: file is open read-only: {}", self.path),
                &self.path,
            ))
        } else {
            Ok(())
        }
    }
}

impl VfsFile for MemoryFile {
    fn path(&self) -> &str {
        &self.path
    }

    fn is_readonly(&self) -> bool {
        self.readonly
    }

    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<usize> {
        self.ensure_live()?;
        let guard = self.node.lock().expect("node poisoned");
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        let avail = guard.data.len().saturating_sub(off);
        let n = buf.len().min(avail);
        if n > 0 {
            buf[..n].copy_from_slice(&guard.data[off..off + n]);
        }
        if n < buf.len() {
            buf[n..].fill(0);
        }
        Ok(n)
    }

    fn write(&mut self, buf: &[u8], offset: u64) -> IoResult<()> {
        self.ensure_writable("write")?;
        if buf.is_empty() {
            return Ok(());
        }
        let off = usize::try_from(offset).map_err(|_| {
            IoError::with_path(IoErrorCode::Full, "offset exceeds address space", &self.path)
        })?;
        let end = off.checked_add(buf.len()).ok_or_else(|| {
            IoError::with_path(IoErrorCode::Full, "write overflows address space", &self.path)
        })?;
        let mut st = self.state.lock().expect("vfs poisoned");
        let mut guard = self.node.lock().expect("node poisoned");
        if end > guard.data.len() {
            st.check_and_grow(&mut guard, end, &self.path)?;
        }
        guard.data[off..end].copy_from_slice(buf);
        Ok(())
    }

    fn size(&self) -> IoResult<u64> {
        self.ensure_live()?;
        Ok(self.node.lock().expect("node poisoned").data.len() as u64)
    }

    fn truncate(&mut self, size: u64) -> IoResult<()> {
        self.ensure_writable("truncate")?;
        let target = usize::try_from(size).map_err(|_| {
            IoError::with_path(IoErrorCode::Full, "size exceeds address space", &self.path)
        })?;
        let mut st = self.state.lock().expect("vfs poisoned");
        let mut guard = self.node.lock().expect("node poisoned");
        if target < guard.data.len() {
            st.shrink(&mut guard, target);
        } else if target > guard.data.len() {
            st.check_and_grow(&mut guard, target, &self.path)?;
        }
        Ok(())
    }

    fn sync(&mut self, _data_only: bool) -> IoResult<()> {
        self.ensure_live()
    }

    fn lock(&mut self, level: LockLevel) -> IoResult<bool> {
        self.ensure_live()?;
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
        self.ensure_live()?;
        self.lock.unlock(level)
    }

    fn check_reserved_lock(&self) -> IoResult<bool> {
        self.ensure_live()?;
        self.lock.check_reserved_lock()
    }

    fn lock_level(&self) -> LockLevel {
        self.lock.level()
    }

    fn close(&mut self) -> IoResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        {
            let mut st = self.state.lock().expect("vfs poisoned");
            if self.delete_on_close {
                if let Some(cur) = st.files.get(&self.path) {
                    if Arc::ptr_eq(cur, &self.node) {
                        st.files.remove(&self.path);
                        self.node.lock().expect("node poisoned").linked = false;
                    }
                }
            }
            let mut guard = self.node.lock().expect("node poisoned");
            guard.open_handles = guard.open_handles.saturating_sub(1);
            st.reclaim_if_dead(&mut guard);
        }
        self.lock.release();
        Ok(())
    }
}

impl Drop for MemoryFile {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
