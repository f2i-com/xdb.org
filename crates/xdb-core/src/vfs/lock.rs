//! SQLite's five-level database file lock and in-process [`LockManager`]
//! (`os.h:98-102`, `os_unix.c:1866-2268`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::errors::{IoError, IoErrorCode, IoResult};

/// Lock levels numbered as SQLite's `NO_LOCK..EXCLUSIVE_LOCK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LockLevel {
    None = 0,
    Shared = 1,
    Reserved = 2,
    Pending = 3,
    Exclusive = 4,
}

impl LockLevel {
    pub const fn name(self) -> &'static str {
        match self {
            LockLevel::None => "NONE",
            LockLevel::Shared => "SHARED",
            LockLevel::Reserved => "RESERVED",
            LockLevel::Pending => "PENDING",
            LockLevel::Exclusive => "EXCLUSIVE",
        }
    }
}

/// Diagnostic view of one key's lock table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockSnapshot {
    pub handles: usize,
    pub shared_count: usize,
    pub writer_level: LockLevel,
}

#[derive(Debug)]
struct KeyState {
    handles: usize,
    shared_count: usize,
    writer: Option<(u64, LockLevel)>,
}

/// In-process SQLite 5-level lock table (`Send + Sync`, zero `unsafe`).
#[derive(Debug, Clone, Default)]
pub struct LockManager {
    inner: Arc<Mutex<HashMap<String, KeyState>>>,
    next_id: Arc<AtomicU64>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new lock owner on `key`, starting at `LockLevel::None`.
    pub fn open_handle(&self, key: impl Into<String>) -> LockHandle {
        let key = key.into();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut map = self.inner.lock().expect("lock manager poisoned");
        let st = map.entry(key.clone()).or_insert(KeyState {
            handles: 0,
            shared_count: 0,
            writer: None,
        });
        st.handles += 1;
        LockHandle {
            id,
            key,
            level: LockLevel::None,
            released: false,
            manager: Some(self.inner.clone()),
        }
    }

    /// Create a no-op lock handle (always grants valid transitions, `check_reserved_lock` is false).
    pub fn no_lock_handle(key: impl Into<String>) -> LockHandle {
        LockHandle {
            id: 0,
            key: key.into(),
            level: LockLevel::None,
            released: false,
            manager: None,
        }
    }

    pub fn is_locked(&self, key: &str) -> bool {
        let map = self.inner.lock().expect("lock manager poisoned");
        map.get(key).is_some_and(|st| st.shared_count > 0)
    }

    pub fn snapshot(&self, key: &str) -> LockSnapshot {
        let map = self.inner.lock().expect("lock manager poisoned");
        match map.get(key) {
            None => LockSnapshot {
                handles: 0,
                shared_count: 0,
                writer_level: LockLevel::None,
            },
            Some(st) => LockSnapshot {
                handles: st.handles,
                shared_count: st.shared_count,
                writer_level: st.writer.map_or(LockLevel::None, |(_, lvl)| lvl),
            },
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("lock manager poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One open handle's lock state on a canonical key. Automatically releases on `Drop`.
#[derive(Debug)]
pub struct LockHandle {
    id: u64,
    key: String,
    level: LockLevel,
    released: bool,
    manager: Option<Arc<Mutex<HashMap<String, KeyState>>>>,
}

impl LockHandle {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn level(&self) -> LockLevel {
        self.level
    }

    fn misuse(&self, msg: impl Into<String>) -> IoError {
        IoError::with_path(IoErrorCode::Misuse, msg, self.key.clone())
    }

    fn check_live(&self) -> IoResult<()> {
        if self.released {
            Err(self.misuse("lock handle used after release"))
        } else {
            Ok(())
        }
    }

    fn check_lock_request(&self, target: LockLevel) -> IoResult<bool> {
        if self.level >= target {
            return Ok(true);
        }
        if target == LockLevel::Pending {
            return Err(self.misuse(
                "lock(PENDING) is illegal: PENDING is only reached by requesting EXCLUSIVE",
            ));
        }
        if self.level == LockLevel::None && target != LockLevel::Shared {
            return Err(self.misuse(format!(
                "illegal lock transition NONE -> {}: acquire SHARED first",
                target.name()
            )));
        }
        Ok(false)
    }

    fn check_unlock_request(&self, target: LockLevel) -> IoResult<bool> {
        if target != LockLevel::None && target != LockLevel::Shared {
            return Err(self.misuse(format!(
                "unlock({}) is illegal: unlock only to SHARED or NONE",
                target.name()
            )));
        }
        Ok(self.level <= target)
    }

    /// Raise the lock to `target`. Returns `Ok(false)` (`SQLITE_BUSY`) on contention.
    pub fn lock(&mut self, target: LockLevel) -> IoResult<bool> {
        self.check_live()?;
        if self.check_lock_request(target)? {
            return Ok(true);
        }
        let Some(ref mgr) = self.manager else {
            self.level = target;
            return Ok(true);
        };
        let mut map = mgr.lock().expect("lock manager poisoned");
        let st = map.get_mut(&self.key).expect("lock key missing");
        match target {
            LockLevel::Shared => {
                if let Some((_, wlvl)) = st.writer {
                    if wlvl >= LockLevel::Pending {
                        return Ok(false);
                    }
                }
                st.shared_count += 1;
                self.level = LockLevel::Shared;
                Ok(true)
            }
            LockLevel::Reserved => {
                if st.writer.is_some() {
                    return Ok(false);
                }
                st.writer = Some((self.id, LockLevel::Reserved));
                self.level = LockLevel::Reserved;
                Ok(true)
            }
            LockLevel::Exclusive => {
                if self.level == LockLevel::Shared {
                    if st.writer.is_some() || st.shared_count > 1 {
                        return Ok(false);
                    }
                    st.writer = Some((self.id, LockLevel::Exclusive));
                    self.level = LockLevel::Exclusive;
                    Ok(true)
                } else {
                    // Coming from RESERVED or PENDING: this handle owns writer.
                    self.level = LockLevel::Pending;
                    st.writer = Some((self.id, LockLevel::Pending));
                    if st.shared_count > 1 {
                        return Ok(false);
                    }
                    self.level = LockLevel::Exclusive;
                    st.writer = Some((self.id, LockLevel::Exclusive));
                    Ok(true)
                }
            }
            LockLevel::None | LockLevel::Pending => unreachable!(),
        }
    }

    /// Lower the lock to `SHARED` or `NONE`.
    pub fn unlock(&mut self, target: LockLevel) -> IoResult<()> {
        self.check_live()?;
        if self.check_unlock_request(target)? {
            return Ok(());
        }
        if let Some(ref mgr) = self.manager {
            let mut map = mgr.lock().expect("lock manager poisoned");
            if let Some(st) = map.get_mut(&self.key) {
                if let Some((wid, _)) = st.writer {
                    if wid == self.id {
                        st.writer = None;
                    }
                }
                if target == LockLevel::None {
                    st.shared_count = st.shared_count.saturating_sub(1);
                }
            }
        }
        self.level = target;
        Ok(())
    }

    /// True if any handle on this key holds `RESERVED` or higher.
    pub fn check_reserved_lock(&self) -> IoResult<bool> {
        self.check_live()?;
        let Some(ref mgr) = self.manager else {
            return Ok(false);
        };
        let map = mgr.lock().expect("lock manager poisoned");
        Ok(map.get(&self.key).is_some_and(|st| st.writer.is_some()))
    }

    /// Drop all locks and detach. Idempotent.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        let _ = self.unlock(LockLevel::None);
        self.released = true;
        if let Some(ref mgr) = self.manager {
            let mut map = mgr.lock().expect("lock manager poisoned");
            if let Some(st) = map.get_mut(&self.key) {
                st.handles = st.handles.saturating_sub(1);
                if st.handles == 0 {
                    map.remove(&self.key);
                }
            }
        }
    }
}

impl Drop for LockHandle {
    fn drop(&mut self) {
        self.release();
    }
}
