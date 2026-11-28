//! Writable SQLite 3 page manager with transactions, nested savepoints, freelist
//! allocation/reclamation, and crash-safe SQLite 3 rollback journal commits.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use xdb_core::vfs::{FileKind, IoError, IoErrorCode, LockLevel, OpenFlags, Vfs};
use xdb_core::{Meter, NoopMeter, TextEncoding};

use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    empty_database_info, header_page_size, is_valid_page_size, lock_byte_page, payload_limits,
    validate_database_header, BtreePageType, DatabaseInfo, PayloadLimits, HEADER_SIZE,
    MAX_SCHEMA_FORMAT, SQLITE_HEADER_MAGIC,
};
use crate::page_source::PageSource;
use crate::snapshot::{
    journal_checksum, open_snapshot, open_snapshot_from_bytes, playback_journal, JournalStatus,
    SnapshotMode, SnapshotOptions, WalStatus, JOURNAL_MAGIC,
};

/// SQLite 3.54.1 version number written into header bytes 96..100 (`3054001`).
pub const SQLITE_VERSION_NUMBER: u32 = 3_054_001;
const JOURNAL_SECTOR_SIZE: u32 = 4096;
/// Direct-indexed page table ceiling (4 GiB at 4 KiB pages); larger sparse page numbers
/// transparently fall back to a `BTreeMap`.
const DIRECT_PAGE_LIMIT: usize = 1_048_576;

/// O(1) direct-indexed resident page store with copy-on-write sharing (`Arc<[u8]>`).
#[derive(Debug, Clone, Default)]
struct PageStore {
    direct: Vec<Option<Arc<[u8]>>>,
    overflow: BTreeMap<u32, Arc<[u8]>>,
}

impl PageStore {
    #[inline]
    fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn get(&self, pgno: u32) -> Option<&Arc<[u8]>> {
        let idx = pgno.checked_sub(1)? as usize;
        if idx < DIRECT_PAGE_LIMIT {
            self.direct.get(idx).and_then(Option::as_ref)
        } else {
            self.overflow.get(&pgno)
        }
    }

    #[inline]
    fn get_mut(&mut self, pgno: u32) -> Option<&mut Arc<[u8]>> {
        let idx = pgno.checked_sub(1)? as usize;
        if idx < DIRECT_PAGE_LIMIT {
            self.direct.get_mut(idx).and_then(Option::as_mut)
        } else {
            self.overflow.get_mut(&pgno)
        }
    }

    #[inline]
    fn contains_key(&self, pgno: u32) -> bool {
        self.get(pgno).is_some()
    }

    #[inline]
    fn insert(&mut self, pgno: u32, data: Arc<[u8]>) {
        if let Some(idx) = pgno.checked_sub(1).map(|v| v as usize) {
            if idx < DIRECT_PAGE_LIMIT {
                if idx >= self.direct.len() {
                    self.direct.resize(idx + 1, None);
                }
                self.direct[idx] = Some(data);
                return;
            }
        }
        self.overflow.insert(pgno, data);
    }

    #[inline]
    fn remove(&mut self, pgno: u32) -> Option<Arc<[u8]>> {
        if let Some(idx) = pgno.checked_sub(1).map(|v| v as usize) {
            if idx < DIRECT_PAGE_LIMIT {
                return self.direct.get_mut(idx).and_then(Option::take);
            }
        }
        self.overflow.remove(&pgno)
    }

    fn total_bytes(&self) -> u64 {
        let mut total = 0u64;
        for p in self.direct.iter().flatten() {
            total = total.saturating_add(p.len() as u64);
        }
        for p in self.overflow.values() {
            total = total.saturating_add(p.len() as u64);
        }
        total
    }

    fn clear(&mut self) {
        self.direct.clear();
        self.overflow.clear();
    }

    fn for_each<F: FnMut(u32, &[u8])>(&self, mut f: F) {
        for (idx, slot) in self.direct.iter().enumerate() {
            if let Some(p) = slot {
                f((idx + 1) as u32, p);
            }
        }
        for (&pgno, p) in &self.overflow {
            f(pgno, p);
        }
    }
}

/// Obtain a mutable slice into an `Arc<[u8]>`, mutating strictly in-place with zero copies
/// when uniquely owned (`strong_count == 1`), or copying on write when shared.
#[inline]
fn arc_bytes_make_mut(slot: &mut Arc<[u8]>) -> &mut [u8] {
    if Arc::get_mut(slot).is_none() {
        *slot = Arc::from(slot.as_ref());
    }
    Arc::get_mut(slot).expect("unique Arc<[u8]> after COW copy")
}

/// Saved pre-mutation state for one transaction or savepoint frame.
const UNDO_BITMAP_MAX_PGNO: u32 = 65_536;

#[derive(Debug, Clone)]
struct UndoFrame {
    /// Savepoint name (`None` for the top-level transaction or implicit statement savepoint).
    name: Option<String>,
    /// Flat 1-bit-per-page presence bitmap for `pgno <= UNDO_BITMAP_MAX_PGNO` (2 L1 ops, 0 hashing).
    saved_bitmap: Vec<u64>,
    /// Original page content before the first write in this frame (`None` = page did not exist).
    saved_pages: Vec<(u32, Option<Arc<[u8]>>)>,
    /// Sparse overflow map for rare `pgno > UNDO_BITMAP_MAX_PGNO`.
    saved_overflow: HashMap<u32, Option<Arc<[u8]>>>,
    /// Bytes reserved in `WritePager::meter` / `tracked_bytes` for `Some(_)` COW undo snapshots.
    saved_bytes: u64,
    page_count: u32,
    freelist_trunk: u32,
    freelist_count: u32,
    schema_cookie: u32,
    user_version: i32,
    application_id: i32,
    change_counter: u32,
    initialized: bool,
}

impl UndoFrame {
    #[inline]
    fn is_saved(&self, pgno: u32) -> bool {
        if pgno <= UNDO_BITMAP_MAX_PGNO {
            let w = (pgno >> 6) as usize;
            w < self.saved_bitmap.len() && (self.saved_bitmap[w] & (1u64 << (pgno & 63))) != 0
        } else {
            self.saved_overflow.contains_key(&pgno)
        }
    }

    #[inline]
    fn record(&mut self, pgno: u32, orig: Option<Arc<[u8]>>, sz: u64) {
        if pgno <= UNDO_BITMAP_MAX_PGNO {
            let w = (pgno >> 6) as usize;
            if w >= self.saved_bitmap.len() {
                self.saved_bitmap.resize(w + 1, 0);
            }
            self.saved_bitmap[w] |= 1u64 << (pgno & 63);
            self.saved_pages.push((pgno, orig));
        } else {
            self.saved_overflow.insert(pgno, orig);
        }
        self.saved_bytes += sz;
    }
}

/// Optional backing store for durable commits via rollback journal.
struct VfsBacking {
    vfs: Arc<dyn Vfs>,
    path: String,
}

impl std::fmt::Debug for VfsBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsBacking")
            .field("path", &self.path)
            .finish()
    }
}

/// Read-write SQLite 3 page store with full ACID rollback journal and nested savepoint support.
#[derive(Debug)]
pub struct WritePager {
    backing: Option<VfsBacking>,
    meter: Arc<dyn Meter>,
    noop_meter: bool,
    page_size: u32,
    reserved_bytes: u8,
    limits: PayloadLimits,
    encoding: TextEncoding,
    page_count: u32,
    freelist_trunk: u32,
    freelist_count: u32,
    schema_cookie: u32,
    schema_format: u32,
    change_counter: u32,
    user_version: i32,
    application_id: i32,
    /// Full set of resident pages (1-based page number -> `page_size` bytes),
    /// direct-indexed & copy-on-write via `Arc` so `snapshot_page_source()` is O(1)
    /// and page reads share `Arc<[u8]>` buffers with zero 4 KiB copies!
    pages: Arc<PageStore>,
    /// Monotonic generation bumped whenever `sqlite_schema` or schema state may have changed.
    schema_gen: u64,
    /// Monotonic generation bumped on every page mutation or rollback (for warm view caching).
    mut_gen: u64,
    /// Pages modified since the last VFS commit (and their original pre-commit image for journaling).
    dirty_originals: HashMap<u32, Option<Arc<[u8]>>>,
    /// Fast dirty flag for in-memory (`backing.is_none()`) databases, avoiding per-write HashMap ops.
    dirty_in_memory: bool,
    /// Original page count at the start of the uncommitted VFS write batch.
    commit_orig_page_count: u32,
    /// Active undo frames (index 0 = explicit transaction or auto-commit; 1+ = savepoints).
    undo_stack: Vec<UndoFrame>,
    /// True if user ran `BEGIN`.
    in_explicit_tx: bool,
    /// Nonce counter for rollback journal checksums.
    journal_nonce: u32,
    /// True if a prior `flush_to_vfs` failed after syncing `-journal`, leaving potentially
    /// torn main-DB pages on disk that MUST be recovered from `-journal` before truncating it.
    disk_needs_journal_recovery: bool,
    /// Tracked bytes reserved in `meter`.
    tracked_bytes: u64,
    /// Staged `PRAGMA page_size = N` requested after initialization; applied on next `VACUUM`.
    pending_page_size: Option<u32>,
}

impl Drop for WritePager {
    fn drop(&mut self) {
        if self.tracked_bytes > 0 {
            self.meter.release_memory(self.tracked_bytes);
            self.tracked_bytes = 0;
        }
    }
}

impl WritePager {
    /// Release all resident pages, undo buffers, and metered memory.
    pub fn close(&mut self) {
        self.pages = Arc::new(PageStore::new());
        self.dirty_originals.clear();
        self.dirty_in_memory = false;
        self.undo_stack.clear();
        self.in_explicit_tx = false;
        self.page_count = 0;
        self.mut_gen = self.mut_gen.wrapping_add(1);
        if self.tracked_bytes > 0 {
            self.meter.release_memory(self.tracked_bytes);
            self.tracked_bytes = 0;
        }
    }

    /// Create a brand-new empty in-memory SQLite database.
    pub fn new_in_memory(page_size: u32, meter: Option<Arc<dyn Meter>>) -> SqliteResult<Self> {
        let ps = if is_valid_page_size(page_size) {
            page_size
        } else {
            4096
        };
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        let noop_meter = meter.is_noop();
        Ok(Self {
            backing: None,
            meter,
            noop_meter,
            page_size: ps,
            reserved_bytes: 0,
            limits: payload_limits(ps),
            encoding: TextEncoding::Utf8,
            page_count: 0,
            freelist_trunk: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 4,
            change_counter: 0,
            user_version: 0,
            application_id: 0,
            pages: Arc::new(PageStore::new()),
            schema_gen: 1,
            mut_gen: 1,
            dirty_originals: HashMap::new(),
            dirty_in_memory: false,
            commit_orig_page_count: 0,
            undo_stack: Vec::new(),
            in_explicit_tx: false,
            journal_nonce: 0x1a2b_3c4d,
            disk_needs_journal_recovery: false,
            tracked_bytes: 0,
            pending_page_size: None,
        })
    }

    /// Load an existing SQLite database image from raw bytes into an in-memory writable pager.
    pub fn from_bytes(
        main_bytes: &[u8],
        wal_bytes: Option<&[u8]>,
        journal_bytes: Option<&[u8]>,
        meter: Option<Arc<dyn Meter>>,
    ) -> SqliteResult<Self> {
        Self::from_owned_bytes(
            main_bytes.to_vec(),
            wal_bytes.map(|b| b.to_vec()),
            journal_bytes.map(|b| b.to_vec()),
            meter,
        )
    }

    /// Load an existing SQLite database image from owned buffers without cloning them.
    pub fn from_owned_bytes(
        main_bytes: Vec<u8>,
        wal_bytes: Option<Vec<u8>>,
        journal_bytes: Option<Vec<u8>>,
        meter: Option<Arc<dyn Meter>>,
    ) -> SqliteResult<Self> {
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        if main_bytes.is_empty() {
            return Self::new_in_memory(4096, Some(meter));
        }
        let snap = open_snapshot_from_bytes(
            main_bytes,
            wal_bytes,
            journal_bytes,
            SnapshotOptions::default(),
        )?;
        Self::from_snapshot(snap, None, meter)
    }

    /// Open (or create if missing/empty) a writable SQLite database over `vfs` at `path`.
    pub fn open_vfs(
        vfs: Arc<dyn Vfs>,
        path: &str,
        page_size: u32,
        meter: Option<Arc<dyn Meter>>,
    ) -> SqliteResult<Self> {
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        let exists = vfs.exists(path)?;
        if !exists {
            // Create empty file immediately so path is valid.
            let mut f = vfs.open(path, OpenFlags::create(FileKind::MainDb))?;
            let _ = f.close();
            let mut pager = Self::new_in_memory(page_size, Some(meter))?;
            pager.backing = Some(VfsBacking {
                vfs,
                path: path.to_string(),
            });
            return Ok(pager);
        }

        // Open existing database (applying any hot journal or WAL overlay into our initial state!).
        // Loop up to 4 times so if another writer recovers or commits between releasing the
        // snapshot Shared lock and acquiring the Exclusive recovery lock, we re-snapshot cleanly
        // instead of overwriting newer on-disk state.
        for attempt in 0..4 {
            let pre_disk_header = read_disk_header_fingerprint(vfs.as_ref(), path)?;
            let snap = open_snapshot(vfs.as_ref(), path, SnapshotOptions::default())?;
            let snap_mode = snap.mode;
            let journal_needs_cleanup = snap.journal.as_ref().is_some_and(|j| {
                matches!(
                    j.status,
                    JournalStatus::HotApplied | JournalStatus::NotHotEmptyDatabase
                )
            });
            let wal_applied = snap
                .wal
                .as_ref()
                .is_some_and(|w| w.status == WalStatus::Applied);
            let had_wal_version = snap
                .header
                .is_some_and(|h| h.write_version == 2 || h.read_version == 2);
            let mut pager = Self::from_snapshot(
                snap,
                Some(VfsBacking {
                    vfs: vfs.clone(),
                    path: path.to_string(),
                }),
                meter.clone(),
            )?;
            if pager.page_count == 0 && is_valid_page_size(page_size) {
                pager.page_size = page_size;
                pager.limits = payload_limits(pager.usable_size());
            }
            // Materialize any recovered hot-journal (including rollback-to-empty `db_size == 0`
            // and stale journal left alongside an empty 0-byte main file!) or WAL state onto the
            // main DB file under Exclusive lock before allowing further writes.
            if snap_mode == SnapshotMode::HotJournalOverlay
                || journal_needs_cleanup
                || wal_applied
                || (had_wal_version && pager.page_count > 0)
            {
                let clear_j = snap_mode == SnapshotMode::HotJournalOverlay || journal_needs_cleanup;
                let clear_w = wal_applied || had_wal_version;
                let committed = pager.materialize_recovered_state_to_vfs(
                    clear_j,
                    clear_w,
                    wal_applied,
                    &pre_disk_header,
                )?;
                if !committed {
                    if attempt + 1 < 4 {
                        continue;
                    }
                    return Err(IoError::with_path(
                        IoErrorCode::Busy,
                        format!("database file changed concurrently during recovery: {path}"),
                        path,
                    )
                    .into());
                }
            }
            return Ok(pager);
        }
        unreachable!()
    }

    fn from_snapshot(
        mut snap: crate::snapshot::Snapshot,
        backing: Option<VfsBacking>,
        meter: Arc<dyn Meter>,
    ) -> SqliteResult<Self> {
        // RAII guard so metered page memory is never leaked if loading/validation fails.
        struct TrackedGuard {
            meter: Arc<dyn Meter>,
            bytes: u64,
        }
        impl Drop for TrackedGuard {
            fn drop(&mut self) {
                if self.bytes > 0 {
                    self.meter.release_memory(self.bytes);
                }
            }
        }

        let page_size = snap.page_size();
        let page_count = snap.page_count();
        let mut pages = PageStore::new();
        let mut guard = TrackedGuard {
            meter: meter.clone(),
            bytes: 0,
        };

        if page_count > 0 {
            for pgno in 1..=page_count {
                if pgno == lock_byte_page(page_size) {
                    continue;
                }
                meter.charge_page_reads(1)?;
                meter.reserve_memory(u64::from(page_size))?;
                guard.bytes += u64::from(page_size);
                let buf = snap.read_page(pgno)?;
                pages.insert(pgno, Arc::from(buf));
            }
        }
        // Release the snapshot's read locks and file handles now that pages are in memory.
        let _ = snap.close();

        let (
            reserved_bytes,
            encoding,
            freelist_trunk,
            freelist_count,
            schema_cookie,
            schema_format,
            change_counter,
            user_version,
            application_id,
        ) = if let Some(p1) = pages.get(1) {
            let info = validate_database_header(p1, page_count, page_size, Some(page_count))?;
            let h = info.header.expect("header present");
            if info.auto_vacuum {
                return Err(SqliteError::misuse(
                    "writing to auto_vacuum databases is not yet supported",
                ));
            }
            if info.write_version_too_new {
                return Err(SqliteError::misuse(format!(
                    "cannot open database for writing: unsupported write format version {}",
                    h.write_version
                )));
            }
            if h.schema_format > MAX_SCHEMA_FORMAT {
                return Err(SqliteError::schema_format("unsupported file format"));
            }
            let sf = if h.schema_format == 0 {
                4
            } else {
                h.schema_format
            };
            (
                h.reserved_bytes,
                info.text_encoding,
                h.first_freelist_trunk,
                h.freelist_count,
                h.schema_cookie,
                sf,
                h.file_change_counter,
                h.user_version,
                h.application_id,
            )
        } else {
            (0, TextEncoding::Utf8, 0, 0, 0, 4, 0, 0, 0)
        };

        // Normalize in-memory writable copies loaded from WAL-mode bytes to rollback mode (1/1)
        // so exported single-file `.db` images are self-contained rollback databases.
        if backing.is_none() {
            if let Some(p1_arc) = pages.get_mut(1) {
                let p1 = arc_bytes_make_mut(p1_arc);
                if p1[18] == 2 || p1[19] == 2 {
                    p1[18] = 1;
                    p1[19] = 1;
                }
            }
        }

        let tracked = guard.bytes;
        guard.bytes = 0;
        let noop_meter = meter.is_noop();

        Ok(Self {
            backing,
            meter,
            noop_meter,
            page_size,
            reserved_bytes,
            limits: payload_limits(page_size - u32::from(reserved_bytes)),
            encoding,
            page_count,
            freelist_trunk,
            freelist_count,
            schema_cookie,
            schema_format,
            change_counter,
            user_version,
            application_id,
            pages: Arc::new(pages),
            schema_gen: 1,
            mut_gen: 1,
            dirty_originals: HashMap::new(),
            dirty_in_memory: false,
            commit_orig_page_count: page_count,
            undo_stack: Vec::new(),
            in_explicit_tx: false,
            journal_nonce: change_counter.wrapping_add(0x55aa_1234),
            disk_needs_journal_recovery: false,
            tracked_bytes: tracked,
            pending_page_size: None,
        })
    }

    /// Persist recovered hot-journal or WAL pages back to the main DB file on disk under
    /// Exclusive lock and clean up the stale `-journal` / `-wal` / `-shm` companion files.
    /// Returns `Ok(false)` if another process modified or already recovered the files before
    /// our Exclusive lock was acquired, signalling `open_vfs` to re-snapshot safely.
    fn materialize_recovered_state_to_vfs(
        &mut self,
        clear_hot_journal: bool,
        clear_wal: bool,
        require_wal_present: bool,
        expected_disk: &DiskRecoveryFingerprint,
    ) -> SqliteResult<bool> {
        let Some(ref backing) = self.backing else {
            return Ok(true);
        };
        let vfs = backing.vfs.clone();
        let main_path = backing.path.clone();
        let ps = self.page_size;

        let mut main_file = vfs.open(&main_path, OpenFlags::create(FileKind::MainDb))?;
        if let Err(e) = acquire_exclusive_lock(main_file.as_mut(), &main_path) {
            let _ = main_file.close();
            return Err(e);
        }

        let write_res: SqliteResult<bool> = (|| {
            // Re-verify on-disk main DB, `-wal`, and `-journal` fingerprints under Exclusive lock
            // so we never overwrite commits or recovery performed concurrently before our lock.
            let cur_size = main_file.size()?;
            let mut cur_hdr = [0u8; 100];
            if cur_size >= 100 {
                main_file.read(&mut cur_hdr, 0)?;
            }
            let cur_wal = read_companion_fingerprint(vfs.as_ref(), &main_path, "-wal", FileKind::Wal)?;
            let cur_journal =
                read_companion_fingerprint(vfs.as_ref(), &main_path, "-journal", FileKind::MainJournal)?;
            if cur_size != expected_disk.main_size
                || cur_hdr != expected_disk.main_hdr
                || cur_wal != expected_disk.wal
                || cur_journal != expected_disk.journal
            {
                return Ok(false);
            }
            if clear_hot_journal && cur_journal.is_none() {
                return Ok(false);
            }
            if require_wal_present && cur_wal.is_none() {
                return Ok(false);
            }

            let lbp = lock_byte_page(ps);
            // Write and sync data pages 2..=page_count BEFORE updating page 1 to rollback
            // mode (`1/1`) and removing `-wal`/`-journal`, so a mid-recovery crash leaves
            // the original header & companion log intact for full replay.
            for pgno in 2..=self.page_count {
                if pgno == lbp {
                    continue;
                }
                if let Some(bytes) = self.pages.get(pgno) {
                    let off = u64::from(pgno - 1) * u64::from(ps);
                    main_file.write(bytes, off)?;
                }
            }
            let target_size = u64::from(self.page_count) * u64::from(ps);
            if main_file.size()? != target_size {
                main_file.truncate(target_size)?;
            }
            if self.page_count > 1 {
                main_file.sync(false)?;
            }
            if clear_wal && self.page_count > 0 {
                if let Some(p1_arc) = Arc::make_mut(&mut self.pages).get_mut(1) {
                    let p1 = arc_bytes_make_mut(p1_arc);
                    if p1[18] == 2 || p1[19] == 2 {
                        p1[18] = 1;
                        p1[19] = 1;
                    }
                }
            }
            if let Some(bytes) = self.pages.get(1) {
                main_file.write(bytes, 0)?;
            }
            main_file.sync(false)?;
            if clear_hot_journal {
                let jpath = format!("{main_path}-journal");
                if vfs.exists(&jpath)? {
                    if let Err(e) = vfs.delete(&jpath, true) {
                        if e.code != IoErrorCode::IoErrDeleteNoEnt {
                            return Err(e.into());
                        }
                    }
                }
            }
            if clear_wal {
                for suffix in ["-wal", "-shm"] {
                    let spath = format!("{main_path}{suffix}");
                    if vfs.exists(&spath)? {
                        if let Err(e) = vfs.delete(&spath, true) {
                            if e.code != IoErrorCode::IoErrDeleteNoEnt {
                                return Err(e.into());
                            }
                        }
                    }
                }
            }
            Ok(true)
        })();

        let _ = main_file.unlock(LockLevel::None);
        let _ = main_file.close();
        let committed = write_res?;
        if committed {
            self.commit_orig_page_count = self.page_count;
        }
        Ok(committed)
    }

    #[inline]
    pub fn meter(&self) -> &Arc<dyn Meter> {
        &self.meter
    }

    #[inline]
    pub fn is_noop_meter(&self) -> bool {
        self.noop_meter
    }

    #[inline]
    pub fn mut_gen(&self) -> u64 {
        self.mut_gen
    }

    pub fn path(&self) -> &str {
        self.backing.as_ref().map_or(":memory:", |b| b.path.as_str())
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn set_page_size(&mut self, new_size: u32) -> SqliteResult<()> {
        if !is_valid_page_size(new_size) || u32::from(self.reserved_bytes) + 480 > new_size {
            return Ok(()); // SQLite silently ignores invalid or reserved-incompatible PRAGMA page_size
        }
        if self.page_count > 0 {
            self.pending_page_size = Some(new_size);
            return Ok(());
        }
        self.page_size = new_size;
        self.limits = payload_limits(self.usable_size());
        self.pending_page_size = None;
        self.mut_gen = self.mut_gen.wrapping_add(1);
        Ok(())
    }

    pub fn pending_page_size(&self) -> Option<u32> {
        self.pending_page_size
    }

    pub fn set_schema_format(&mut self, fmt: u32) {
        if (1..=MAX_SCHEMA_FORMAT).contains(&fmt) {
            self.schema_format = fmt;
        }
    }

    #[inline]
    pub fn reserved_bytes(&self) -> u8 {
        self.reserved_bytes
    }

    pub fn set_reserved_bytes(&mut self, reserved: u8) {
        if self.page_count == 0 && u32::from(reserved) + 480 <= self.page_size {
            self.reserved_bytes = reserved;
            self.limits = payload_limits(self.page_size - u32::from(reserved));
        }
    }

    #[inline]
    pub fn usable_size(&self) -> u32 {
        self.page_size - u32::from(self.reserved_bytes)
    }

    #[inline]
    pub fn payload_limits(&self) -> PayloadLimits {
        self.limits
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    pub fn is_vfs_backed(&self) -> bool {
        self.backing.is_some()
    }

    pub fn vfs_handle(&self) -> Option<Arc<dyn Vfs>> {
        self.backing.as_ref().map(|b| Arc::clone(&b.vfs))
    }

    pub fn set_encoding(&mut self, enc: TextEncoding) {
        if self.page_count == 0 {
            self.encoding = enc;
            self.schema_gen = self.schema_gen.wrapping_add(1);
            self.mut_gen = self.mut_gen.wrapping_add(1);
        }
    }

    pub fn freelist_trunk(&self) -> u32 {
        self.freelist_trunk
    }

    pub fn freelist_count(&self) -> u32 {
        self.freelist_count
    }

    pub fn schema_cookie(&self) -> u32 {
        self.schema_cookie
    }

    pub fn schema_format(&self) -> u32 {
        self.schema_format
    }

    pub fn schema_gen(&self) -> u64 {
        self.schema_gen
    }

    pub fn invalidate_schema_cache(&mut self) {
        self.schema_gen = self.schema_gen.wrapping_add(1);
    }

    pub fn bump_schema_cookie(&mut self) -> SqliteResult<()> {
        self.ensure_initialized()?;
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        self.invalidate_schema_cache();
        self.sync_page1_header()
    }

    pub fn user_version(&self) -> i32 {
        self.user_version
    }

    pub fn set_user_version(&mut self, v: i32) -> SqliteResult<()> {
        self.ensure_initialized()?;
        self.user_version = v;
        self.sync_page1_header()
    }

    pub fn application_id(&self) -> i32 {
        self.application_id
    }

    pub fn set_application_id(&mut self, v: i32) -> SqliteResult<()> {
        self.ensure_initialized()?;
        self.application_id = v;
        self.sync_page1_header()
    }

    pub fn in_transaction(&self) -> bool {
        self.in_explicit_tx || self.undo_stack.iter().any(|f| f.name.is_some())
    }

    pub fn db_info(&self) -> DatabaseInfo {
        if self.page_count == 0 {
            let mut info = empty_database_info(self.page_size);
            info.usable_size = self.usable_size();
            info.text_encoding = self.encoding;
            info.limits = self.limits;
            return info;
        }
        let p1 = self.pages.get(1).expect("page 1 exists");
        validate_database_header(p1, self.page_count, self.page_size, Some(self.page_count))
            .expect("internal page 1 header always valid")
    }

    /// Initialize page 1 with a valid 100-byte SQLite 3 header and empty `sqlite_schema` TableLeaf.
    pub fn ensure_initialized(&mut self) -> SqliteResult<()> {
        if self.page_count > 0 {
            return Ok(());
        }
        let ps = self.page_size as usize;
        // Reserve memory BEFORE mutating undo/dirty state so a memory-limit error leaves pager clean.
        self.meter.reserve_memory(ps as u64)?;
        self.tracked_bytes += ps as u64;
        if let Err(e) = self.record_undo_for_page(1) {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(ps as u64);
            self.meter.release_memory(ps as u64);
            return Err(e);
        }
        self.record_dirty_original(1);

        let mut p1 = vec![0u8; ps];
        // Write 100-byte SQLite 3 header
        p1[0..16].copy_from_slice(SQLITE_HEADER_MAGIC);
        let ps_field: u16 = if self.page_size == 65536 {
            1
        } else {
            self.page_size as u16
        };
        p1[16..18].copy_from_slice(&ps_field.to_be_bytes());
        p1[18] = 1; // write version: rollback journal
        p1[19] = 1; // read version: rollback journal
        p1[20] = self.reserved_bytes;
        p1[21] = 64; // max payload fraction
        p1[22] = 32; // min payload fraction
        p1[23] = 32; // leaf payload fraction
        p1[24..28].copy_from_slice(&1u32.to_be_bytes()); // file change counter
        p1[28..32].copy_from_slice(&1u32.to_be_bytes()); // db size in pages
        p1[32..36].copy_from_slice(&0u32.to_be_bytes()); // first freelist trunk
        p1[36..40].copy_from_slice(&0u32.to_be_bytes()); // freelist count
        p1[40..44].copy_from_slice(&0u32.to_be_bytes()); // schema cookie
        p1[44..48].copy_from_slice(&self.schema_format.to_be_bytes()); // schema format
        p1[48..52].copy_from_slice(&0u32.to_be_bytes()); // default cache size
        p1[52..56].copy_from_slice(&0u32.to_be_bytes()); // largest root btree (0 = no autovacuum)
        let enc_u32: u32 = match self.encoding {
            TextEncoding::Utf8 => 1,
            TextEncoding::Utf16Le => 2,
            TextEncoding::Utf16Be => 3,
        };
        p1[56..60].copy_from_slice(&enc_u32.to_be_bytes());
        p1[60..64].copy_from_slice(&self.user_version.to_be_bytes());
        p1[64..68].copy_from_slice(&0u32.to_be_bytes()); // incremental vacuum
        p1[68..72].copy_from_slice(&self.application_id.to_be_bytes());
        p1[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
        p1[96..100].copy_from_slice(&SQLITE_VERSION_NUMBER.to_be_bytes());

        // Initialize empty TableLeaf B-tree header for `sqlite_schema` at offset 100
        init_empty_btree_page(&mut p1, 1, BtreePageType::TableLeaf, self.usable_size());

        Arc::make_mut(&mut self.pages).insert(1, Arc::from(p1));
        self.page_count = 1;
        self.change_counter = 1;
        self.mut_gen = self.mut_gen.wrapping_add(1);
        self.invalidate_schema_cache();
        Ok(())
    }

    /// Read page `pgno` (1-based).
    #[inline]
    pub fn read_page(&self, pgno: u32) -> SqliteResult<&[u8]> {
        if !self.noop_meter {
            self.meter.charge_steps(1)?;
        }
        self.pages
            .get(pgno)
            .map(|p| p.as_ref())
            .ok_or_else(|| SqliteError::corrupt(format!("missing page {pgno}")))
    }

    /// Obtain mutable access to page `pgno`, recording undo & dirty state automatically.
    /// Mutates strictly in place with zero 4 KiB memcpy whenever uniquely owned.
    #[inline]
    pub fn write_page_mut(&mut self, pgno: u32) -> SqliteResult<&mut [u8]> {
        if !self.noop_meter {
            self.meter.charge_steps(1)?;
        }
        // Single-lookup hot path: in-memory DB whose page is already recorded in top undo frame
        // (or no undo frame active) skips redundant existence & undo map probes.
        if self.backing.is_none()
            && self.undo_stack.last().is_none_or(|f| f.is_saved(pgno))
        {
            if !self.pages.contains_key(pgno) {
                return Err(SqliteError::corrupt(format!(
                    "write to unallocated page {pgno}"
                )));
            }
            self.dirty_in_memory = true;
            self.mut_gen = self.mut_gen.wrapping_add(1);
            let entry = Arc::make_mut(&mut self.pages)
                .get_mut(pgno)
                .expect("page exists");
            return Ok(arc_bytes_make_mut(entry));
        }
        if !self.pages.contains_key(pgno) {
            return Err(SqliteError::corrupt(format!(
                "write to unallocated page {pgno}"
            )));
        }
        self.record_undo_for_page(pgno)?;
        self.record_dirty_original(pgno);
        self.mut_gen = self.mut_gen.wrapping_add(1);
        let entry = Arc::make_mut(&mut self.pages)
            .get_mut(pgno)
            .expect("page exists");
        Ok(arc_bytes_make_mut(entry))
    }

    /// Replace full contents of page `pgno` (length must equal `page_size`).
    pub fn put_page(&mut self, pgno: u32, data: Vec<u8>) -> SqliteResult<()> {
        if pgno == 0 {
            return Err(SqliteError::corrupt("invalid page number 0"));
        }
        if data.len() != self.page_size as usize {
            return Err(SqliteError::misuse("page write length mismatch"));
        }
        if !self.noop_meter {
            self.meter.charge_steps(1)?;
        }
        // Reserve memory BEFORE recording undo/dirty metadata so OOM never leaves phantom state.
        let is_new = !self.pages.contains_key(pgno);
        if is_new {
            if !self.noop_meter {
                self.meter.reserve_memory(data.len() as u64)?;
            }
            self.tracked_bytes += data.len() as u64;
        }
        if let Err(e) = self.record_undo_for_page(pgno) {
            if is_new {
                self.tracked_bytes = self.tracked_bytes.saturating_sub(data.len() as u64);
                if !self.noop_meter {
                    self.meter.release_memory(data.len() as u64);
                }
            }
            return Err(e);
        }
        self.record_dirty_original(pgno);
        self.mut_gen = self.mut_gen.wrapping_add(1);
        Arc::make_mut(&mut self.pages).insert(pgno, Arc::from(data));
        Ok(())
    }

    /// Allocate a fresh zeroed page (reusing the SQLite freelist when non-empty).
    pub fn allocate_page(&mut self) -> SqliteResult<u32> {
        self.ensure_initialized()?;
        self.meter.charge_steps(2)?;

        // 1. Pop from freelist if available (`btree.c allocateBtreePage`).
        if (self.freelist_trunk == 0) != (self.freelist_count == 0) {
            return Err(SqliteError::corrupt(format!(
                "corrupt freelist header: trunk={}, count={}",
                self.freelist_trunk, self.freelist_count
            )));
        }
        if self.freelist_trunk != 0 && self.freelist_count > 0 {
            let trunk_pgno = self.freelist_trunk;
            let lbp = lock_byte_page(self.page_size);
            if trunk_pgno < 2 || trunk_pgno > self.page_count || trunk_pgno == lbp {
                return Err(SqliteError::corrupt(format!(
                    "corrupt freelist trunk page {trunk_pgno}"
                )));
            }
            let max_leaves = ((self.usable_size() as usize).saturating_sub(8)) / 4;
            let trunk = self.read_page(trunk_pgno)?.to_vec();
            let next_trunk = u32::from_be_bytes([trunk[0], trunk[1], trunk[2], trunk[3]]);
            let leaf_cnt = u32::from_be_bytes([trunk[4], trunk[5], trunk[6], trunk[7]]);
            if (leaf_cnt as usize) > max_leaves {
                return Err(SqliteError::corrupt(format!(
                    "corrupt freelist trunk {trunk_pgno}: leaf count {leaf_cnt} exceeds max {max_leaves}"
                )));
            }
            if leaf_cnt > 0 {
                let off = 8 + ((leaf_cnt - 1) as usize) * 4;
                let popped = u32::from_be_bytes([
                    trunk[off],
                    trunk[off + 1],
                    trunk[off + 2],
                    trunk[off + 3],
                ]);
                if popped < 2
                    || popped > self.page_count
                    || popped == lbp
                    || popped == trunk_pgno
                    || !self.pages.contains_key(popped)
                {
                    return Err(SqliteError::corrupt(format!(
                        "corrupt freelist leaf page {popped} on trunk {trunk_pgno}"
                    )));
                }
                let tbuf = self.write_page_mut(trunk_pgno)?;
                tbuf[4..8].copy_from_slice(&(leaf_cnt - 1).to_be_bytes());
                tbuf[off..off + 4].fill(0);
                self.freelist_count = self.freelist_count.saturating_sub(1);
                self.sync_page1_header()?;
                let pbuf = self.write_page_mut(popped)?;
                pbuf.fill(0);
                return Ok(popped);
            } else {
                // Trunk itself has no leaves: pop trunk page and advance head!
                if next_trunk != 0
                    && (next_trunk < 2
                        || next_trunk > self.page_count
                        || next_trunk == lbp
                        || next_trunk == trunk_pgno)
                {
                    return Err(SqliteError::corrupt(format!(
                        "corrupt next freelist trunk {next_trunk} on trunk {trunk_pgno}"
                    )));
                }
                self.freelist_trunk = next_trunk;
                self.freelist_count = self.freelist_count.saturating_sub(1);
                self.sync_page1_header()?;
                let pbuf = self.write_page_mut(trunk_pgno)?;
                pbuf.fill(0);
                return Ok(trunk_pgno);
            }
        }

        // 2. Extend database file by 1 page (skipping lock-byte page if reached).
        let mut next = self
            .page_count
            .checked_add(1)
            .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()))?;
        let lbp = lock_byte_page(self.page_size);
        if next == lbp {
            next = next
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()))?;
        }
        let ps = self.page_size as usize;
        self.put_page(next, vec![0u8; ps])?;
        self.page_count = next;
        self.sync_page1_header()?;
        Ok(next)
    }

    /// Allocate `count` fresh zeroed pages, syncing page 1's header only once at the end
    /// when extending the database (used by bulk index loading).
    pub fn allocate_pages_batch(&mut self, count: usize) -> SqliteResult<Vec<u32>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        self.ensure_initialized()?;
        if (self.freelist_trunk == 0) != (self.freelist_count == 0) {
            return Err(SqliteError::corrupt(format!(
                "corrupt freelist header: trunk={}, count={}",
                self.freelist_trunk, self.freelist_count
            )));
        }
        let mut out = Vec::with_capacity(count);
        if self.freelist_trunk != 0 && self.freelist_count > 0 {
            for _ in 0..count {
                out.push(self.allocate_page()?);
            }
            return Ok(out);
        }
        if !self.noop_meter {
            self.meter.charge_steps((count as u64).saturating_mul(2))?;
        }
        let ps = self.page_size as usize;
        let lbp = lock_byte_page(self.page_size);
        let mut cur = self.page_count;
        for _ in 0..count {
            let mut next = cur
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()))?;
            if next == lbp {
                next = next
                    .checked_add(1)
                    .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()))?;
            }
            self.put_page(next, vec![0u8; ps])?;
            cur = next;
            out.push(next);
        }
        self.page_count = cur;
        self.sync_page1_header()?;
        Ok(out)
    }

    /// Return `pgno` (> 1) to the SQLite freelist (`fileformat2 §1.5`).
    pub fn free_page(&mut self, pgno: u32) -> SqliteResult<()> {
        let lbp = lock_byte_page(self.page_size);
        if pgno <= 1 || pgno > self.page_count || pgno == lbp {
            return Err(SqliteError::misuse(format!("cannot free page {pgno}")));
        }
        self.meter.charge_steps(2)?;
        let max_leaves = ((self.usable_size() as usize).saturating_sub(8)) / 4;

        if self.freelist_trunk != 0 {
            let trunk_pgno = self.freelist_trunk;
            if trunk_pgno <= 1 || trunk_pgno > self.page_count || trunk_pgno == lbp || trunk_pgno == pgno {
                return Err(SqliteError::corrupt(format!(
                    "corrupt freelist trunk page number {trunk_pgno}"
                )));
            }
            let leaf_cnt = {
                let t = self.read_page(trunk_pgno)?;
                u32::from_be_bytes([t[4], t[5], t[6], t[7]]) as usize
            };
            if leaf_cnt > max_leaves {
                return Err(SqliteError::corrupt(format!(
                    "corrupt freelist trunk {trunk_pgno}: leaf count {leaf_cnt} exceeds max {max_leaves}"
                )));
            }
            if leaf_cnt < max_leaves {
                let next_fc = self
                    .freelist_count
                    .checked_add(1)
                    .ok_or_else(|| SqliteError::corrupt("freelist count overflow"))?;
                let off = 8 + leaf_cnt * 4;
                let tbuf = self.write_page_mut(trunk_pgno)?;
                tbuf[4..8].copy_from_slice(&((leaf_cnt + 1) as u32).to_be_bytes());
                tbuf[off..off + 4].copy_from_slice(&pgno.to_be_bytes());
                let zbuf = self.write_page_mut(pgno)?;
                zbuf.fill(0);
                self.freelist_count = next_fc;
                self.sync_page1_header()?;
                return Ok(());
            }
        }

        // Become the new freelist trunk page pointing to the old trunk.
        let next_fc = self
            .freelist_count
            .checked_add(1)
            .ok_or_else(|| SqliteError::corrupt("freelist count overflow"))?;
        let prev_trunk = self.freelist_trunk;
        let tbuf = self.write_page_mut(pgno)?;
        tbuf.fill(0);
        tbuf[0..4].copy_from_slice(&prev_trunk.to_be_bytes());
        tbuf[4..8].copy_from_slice(&0u32.to_be_bytes());
        self.freelist_trunk = pgno;
        self.freelist_count = next_fc;
        self.sync_page1_header()?;
        Ok(())
    }

    /// Update the 100-byte database header on page 1 with current metadata.
    pub fn sync_page1_header(&mut self) -> SqliteResult<()> {
        if self.page_count == 0 {
            return Ok(());
        }
        let pc = self.page_count;
        let ft = self.freelist_trunk;
        let fc = self.freelist_count;
        let sc = self.schema_cookie;
        let sf = self.schema_format;
        let uv = self.user_version;
        let aid = self.application_id;
        let cc = self.change_counter;

        let p1 = self.write_page_mut(1)?;
        p1[24..28].copy_from_slice(&cc.to_be_bytes());
        p1[28..32].copy_from_slice(&pc.to_be_bytes());
        p1[32..36].copy_from_slice(&ft.to_be_bytes());
        p1[36..40].copy_from_slice(&fc.to_be_bytes());
        p1[40..44].copy_from_slice(&sc.to_be_bytes());
        p1[44..48].copy_from_slice(&sf.to_be_bytes());
        p1[60..64].copy_from_slice(&uv.to_be_bytes());
        p1[68..72].copy_from_slice(&aid.to_be_bytes());
        p1[92..96].copy_from_slice(&cc.to_be_bytes());
        p1[96..100].copy_from_slice(&SQLITE_VERSION_NUMBER.to_be_bytes());
        Ok(())
    }

    // ───────────────────────────── Transactions & Savepoints ─────────────────────────────

    fn make_undo_frame(&self, name: Option<String>) -> UndoFrame {
        UndoFrame {
            name,
            saved_bitmap: Vec::new(),
            saved_pages: Vec::new(),
            saved_overflow: HashMap::new(),
            saved_bytes: 0,
            page_count: self.page_count,
            freelist_trunk: self.freelist_trunk,
            freelist_count: self.freelist_count,
            schema_cookie: self.schema_cookie,
            user_version: self.user_version,
            application_id: self.application_id,
            change_counter: self.change_counter,
            initialized: self.page_count > 0,
        }
    }

    #[inline]
    fn release_undo_bytes(&mut self, bytes: u64) {
        if bytes > 0 {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(bytes);
            if !self.noop_meter {
                self.meter.release_memory(bytes);
            }
        }
    }

    #[inline]
    fn record_undo_for_page(&mut self, pgno: u32) -> SqliteResult<()> {
        if let Some(frame) = self.undo_stack.last_mut() {
            if frame.is_saved(pgno) {
                return Ok(());
            }
            let existing = self.pages.get(pgno).cloned();
            let mut sz = 0u64;
            if let Some(ref arc) = existing {
                if !self.noop_meter {
                    sz = arc.len() as u64;
                    self.meter.reserve_memory(sz)?;
                    self.tracked_bytes += sz;
                }
            }
            frame.record(pgno, existing, sz);
        }
        Ok(())
    }

    #[inline]
    fn record_dirty_original(&mut self, pgno: u32) {
        if self.backing.is_none() {
            // In-memory / WASM mode never writes a disk rollback journal; flip a bool with
            // zero HashMap hashing and zero extra `Arc` reference count bumps!
            self.dirty_in_memory = true;
            return;
        }
        self.dirty_originals
            .entry(pgno)
            .or_insert_with(|| self.pages.get(pgno).cloned());
    }

    /// `BEGIN [TRANSACTION]`.
    pub fn begin(&mut self) -> SqliteResult<()> {
        if self.in_transaction() {
            return Err(SqliteError::Sql(
                "cannot start a transaction within a transaction".to_string(),
            ));
        }
        self.in_explicit_tx = true;
        self.undo_stack.push(self.make_undo_frame(None));
        Ok(())
    }

    /// Push a named (`SAVEPOINT name`) or unnamed statement-level savepoint.
    pub fn push_savepoint(&mut self, name: Option<String>) {
        self.undo_stack.push(self.make_undo_frame(name));
    }

    /// Pop the innermost unnamed statement-level savepoint, folding its undo entries into
    /// any enclosing frame (so outer savepoints/ROLLBACK can still undo the statement's writes).
    pub fn commit_statement_savepoint(&mut self) {
        if let Some(inner) = self.undo_stack.pop() {
            let mut released = 0u64;
            if let Some(parent) = self.undo_stack.last_mut() {
                let metered = !self.noop_meter;
                for (pgno, orig) in inner
                    .saved_pages
                    .into_iter()
                    .chain(inner.saved_overflow.into_iter())
                {
                    let sz = if metered {
                        orig.as_ref().map_or(0, |a| a.len() as u64)
                    } else {
                        0
                    };
                    if !parent.is_saved(pgno) {
                        parent.record(pgno, orig, sz);
                    } else {
                        released += sz;
                    }
                }
            } else {
                released = inner.saved_bytes;
            }
            self.release_undo_bytes(released);
        }
    }

    /// Commit an unnamed statement-level savepoint and flush immediately if in auto-commit mode.
    /// If auto-commit flushing to VFS fails, rolls back the statement savepoint cleanly!
    pub fn commit_statement_and_autocommit(&mut self) -> SqliteResult<()> {
        if !self.in_explicit_tx && self.undo_stack.len() == 1 && self.undo_stack[0].name.is_none() {
            if let Err(e) = self.flush_to_vfs() {
                self.rollback_statement_savepoint();
                self.dirty_originals.clear();
                self.dirty_in_memory = false;
                return Err(e);
            }
            if let Some(frame) = self.undo_stack.pop() {
                self.release_undo_bytes(frame.saved_bytes);
            }
            return Ok(());
        }
        self.commit_statement_savepoint();
        Ok(())
    }

    /// Roll back the innermost unnamed statement-level savepoint (on constraint violation).
    pub fn rollback_statement_savepoint(&mut self) {
        if let Some(frame) = self.undo_stack.pop() {
            self.apply_undo_frame(frame);
        }
        if !self.in_explicit_tx && self.undo_stack.is_empty() {
            self.dirty_originals.clear();
            self.dirty_in_memory = false;
        }
    }

    /// `ROLLBACK TO [SAVEPOINT] name`: reverts state back to the most recent matching savepoint,
    /// keeping the savepoint itself open on the stack (SQLite semantics!).
    pub fn rollback_to_savepoint(&mut self, name: &str) -> SqliteResult<()> {
        let Some(pos) = self.undo_stack.iter().rposition(|f| {
            f.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        }) else {
            return Err(SqliteError::Sql(format!("no such savepoint: {name}")));
        };
        while self.undo_stack.len() > pos + 1 {
            if let Some(f) = self.undo_stack.pop() {
                self.apply_undo_frame(f);
            }
        }
        // Revert the target savepoint's own changes while keeping it active.
        let target = self.undo_stack.pop().unwrap();
        let kept_name = target.name.clone();
        self.apply_undo_frame(target);
        self.undo_stack.push(self.make_undo_frame(kept_name));
        Ok(())
    }

    /// Returns `true` if `RELEASE <name>` would commit the outermost implicit savepoint transaction.
    pub fn will_release_commit(&self, name: &str) -> bool {
        if self.in_explicit_tx {
            return false;
        }
        self.undo_stack
            .iter()
            .rposition(|f| {
                f.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            == Some(0)
    }

    /// `RELEASE [SAVEPOINT] name`: merges matching savepoint (and nested ones) upward.
    /// If not inside an explicit `BEGIN` and the stack empties, commits to VFS!
    pub fn release_savepoint(&mut self, name: &str) -> SqliteResult<()> {
        let Some(pos) = self.undo_stack.iter().rposition(|f| {
            f.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        }) else {
            return Err(SqliteError::Sql(format!("no such savepoint: {name}")));
        };
        if !self.in_explicit_tx && pos == 0 {
            self.flush_to_vfs()?;
            let mut released = 0u64;
            for f in self.undo_stack.drain(..) {
                released += f.saved_bytes;
            }
            self.release_undo_bytes(released);
            return Ok(());
        }
        let drained: Vec<UndoFrame> = self.undo_stack.drain(pos..).collect();
        let mut released = 0u64;
        if let Some(parent) = self.undo_stack.last_mut() {
            let metered = !self.noop_meter;
            for frame in drained {
                for (pgno, orig) in frame
                    .saved_pages
                    .into_iter()
                    .chain(frame.saved_overflow.into_iter())
                {
                    let sz = if metered {
                        orig.as_ref().map_or(0, |a| a.len() as u64)
                    } else {
                        0
                    };
                    if !parent.is_saved(pgno) {
                        parent.record(pgno, orig, sz);
                    } else {
                        released += sz;
                    }
                }
            }
        } else {
            for frame in drained {
                released += frame.saved_bytes;
            }
        }
        self.release_undo_bytes(released);
        Ok(())
    }

    /// `COMMIT`: closes all open savepoints & transaction, flushes dirty pages atomically via
    /// rollback journal to the backing VFS (if any).
    pub fn commit(&mut self) -> SqliteResult<()> {
        if !self.in_transaction() {
            return Err(SqliteError::Sql(
                "cannot commit - no transaction is active".to_string(),
            ));
        }
        self.flush_to_vfs()?;
        self.in_explicit_tx = false;
        let mut released = 0u64;
        for f in self.undo_stack.drain(..) {
            released += f.saved_bytes;
        }
        self.release_undo_bytes(released);
        Ok(())
    }

    /// `ROLLBACK`: reverts all uncommitted writes back to the start of the active transaction.
    pub fn rollback(&mut self) -> SqliteResult<()> {
        if !self.in_transaction() {
            return Err(SqliteError::Sql(
                "cannot rollback - no transaction is active".to_string(),
            ));
        }
        self.in_explicit_tx = false;
        while let Some(frame) = self.undo_stack.pop() {
            self.apply_undo_frame(frame);
        }
        self.dirty_originals.clear();
        self.dirty_in_memory = false;
        // If a prior COMMIT failed mid-write leaving a hot journal on disk, best-effort heal
        // the backing VFS now (if still failing, `disk_needs_journal_recovery` stays true so
        // the next commit recovers BEFORE truncating `-journal`).
        if self.disk_needs_journal_recovery {
            let _ = self.recover_hot_journal_on_vfs();
        }
        Ok(())
    }

    /// Auto-commit flush when no explicit transaction or savepoint is active.
    pub fn autocommit_flush(&mut self) -> SqliteResult<()> {
        if !self.in_explicit_tx && self.undo_stack.is_empty() {
            self.flush_to_vfs()?;
        }
        Ok(())
    }

    fn apply_undo_frame(&mut self, frame: UndoFrame) {
        let undo_bytes = frame.saved_bytes;
        let pages_map = Arc::make_mut(&mut self.pages);
        for (pgno, orig) in frame
            .saved_pages
            .into_iter()
            .chain(frame.saved_overflow.into_iter())
        {
            match orig {
                Some(bytes) => {
                    pages_map.insert(pgno, bytes);
                }
                None => {
                    if let Some(removed) = pages_map.remove(pgno) {
                        let sz = removed.len() as u64;
                        self.tracked_bytes = self.tracked_bytes.saturating_sub(sz);
                        if !self.noop_meter {
                            self.meter.release_memory(sz);
                        }
                    }
                }
            }
        }
        self.release_undo_bytes(undo_bytes);
        self.page_count = frame.page_count;
        self.freelist_trunk = frame.freelist_trunk;
        self.freelist_count = frame.freelist_count;
        self.schema_cookie = frame.schema_cookie;
        self.user_version = frame.user_version;
        self.application_id = frame.application_id;
        self.change_counter = frame.change_counter;
        if !frame.initialized {
            let map = Arc::make_mut(&mut self.pages);
            let leftover = map.total_bytes();
            map.clear();
            if leftover > 0 {
                self.tracked_bytes = self.tracked_bytes.saturating_sub(leftover);
                if !self.noop_meter {
                    self.meter.release_memory(leftover);
                }
            }
        }
        self.mut_gen = self.mut_gen.wrapping_add(1);
        self.invalidate_schema_cache();
    }

    /// Play back an on-disk hot rollback journal left by an earlier failed `flush_to_vfs`,
    /// restoring torn main-DB pages and truncating to the journal's `original_page_count`
    /// BEFORE deleting `-journal`. Must be called with EXCLUSIVE lock held on `main_file`.
    fn playback_hot_journal_locked(
        vfs: &Arc<dyn Vfs>,
        main_path: &str,
        main_file: &mut dyn xdb_core::VfsFile,
        pager_page_size: u32,
    ) -> SqliteResult<()> {
        let journal_path = format!("{main_path}-journal");
        if !vfs.exists(&journal_path)? {
            return Ok(());
        }
        let mut jfile = vfs.open(&journal_path, OpenFlags::readonly(FileKind::MainJournal))?;
        let playback_res = (|| -> SqliteResult<()> {
            let jsize = jfile.size()?;
            if jsize > 0 {
                let pb = playback_journal(jfile.as_mut(), jsize, pager_page_size)?;
                if pb.played {
                    let ps = pb.page_size;
                    let mut buf = vec![0u8; ps as usize];
                    let mut pgnos: Vec<u32> = pb.records.keys().copied().collect();
                    pgnos.sort_unstable();
                    for pgno in pgnos {
                        if let Some(&j_off) = pb.records.get(&pgno) {
                            let n = jfile.read(&mut buf, j_off)?;
                            if n < ps as usize {
                                buf[n..].fill(0);
                            }
                            let off = u64::from(pgno - 1) * u64::from(ps);
                            main_file.write(&buf, off)?;
                        }
                    }
                    let orig_pages = pb.original_page_count;
                    let target_size = u64::from(orig_pages) * u64::from(ps);
                    if main_file.size()? != target_size {
                        main_file.truncate(target_size)?;
                    }
                    main_file.sync(false)?;
                }
            }
            Ok(())
        })();
        let _ = jfile.close();
        playback_res?;
        if let Err(e) = vfs.delete(&journal_path, true) {
            if e.code != IoErrorCode::IoErrDeleteNoEnt {
                return Err(e.into());
            }
        }
        Ok(())
    }

    fn recover_hot_journal_on_vfs(&mut self) -> SqliteResult<()> {
        let Some(ref backing) = self.backing else {
            self.disk_needs_journal_recovery = false;
            return Ok(());
        };
        let vfs = backing.vfs.clone();
        let main_path = backing.path.clone();
        let ps = self.page_size;
        let mut main_file = vfs.open(&main_path, OpenFlags::create(FileKind::MainDb))?;
        if let Err(e) = acquire_exclusive_lock(main_file.as_mut(), &main_path) {
            let _ = main_file.unlock(LockLevel::None);
            let _ = main_file.close();
            return Err(e);
        }
        let res = Self::playback_hot_journal_locked(&vfs, &main_path, main_file.as_mut(), ps);
        let _ = main_file.unlock(LockLevel::None);
        let _ = main_file.close();
        res?;
        self.disk_needs_journal_recovery = false;
        Ok(())
    }

    /// Atomically flush dirty pages to the backing VFS using a genuine SQLite 3 rollback journal.
    pub fn flush_to_vfs(&mut self) -> SqliteResult<()> {
        if !self.dirty_in_memory && self.dirty_originals.is_empty() {
            if self.disk_needs_journal_recovery {
                self.recover_hot_journal_on_vfs()?;
            }
            return Ok(());
        }

        // For VFS-backed databases, open & acquire EXCLUSIVE lock BEFORE bumping change_counter
        // so a `SQLITE_BUSY` failure leaves in-memory pager state completely untouched.
        type LockedVfsMain = (Arc<dyn Vfs>, String, Box<dyn xdb_core::VfsFile>);
        let mut locked_vfs: Option<LockedVfsMain> = None;
        if let Some(ref backing) = self.backing {
            let vfs = backing.vfs.clone();
            let main_path = backing.path.clone();
            let mut main_file = vfs.open(&main_path, OpenFlags::create(FileKind::MainDb))?;
            if let Err(e) = acquire_exclusive_lock(main_file.as_mut(), &main_path) {
                let _ = main_file.unlock(LockLevel::None);
                let _ = main_file.close();
                return Err(e);
            }
            // If an earlier commit failed mid-write leaving a hot journal + torn pages on disk,
            // recover the main DB from that hot journal BEFORE truncating `-journal`!
            if self.disk_needs_journal_recovery {
                if let Err(e) = Self::playback_hot_journal_locked(
                    &vfs,
                    &main_path,
                    main_file.as_mut(),
                    self.page_size,
                ) {
                    let _ = main_file.unlock(LockLevel::None);
                    let _ = main_file.close();
                    return Err(e);
                }
                self.disk_needs_journal_recovery = false;
            }
            locked_vfs = Some((vfs, main_path, main_file));
        }

        // Snapshot pre-commit page 1 BEFORE mutating change_counter so the rollback journal
        // ALWAYS records page 1's original on-disk image AND so an I/O failure restores page 1
        // cleanly without desyncing `self.pages[1]` from `self.change_counter`!
        let old_cc = self.change_counter;
        let old_p1 = if locked_vfs.is_some() {
            self.pages.get(1).cloned()
        } else {
            None
        };
        if self.page_count > 0 {
            self.record_dirty_original(1);
            let cc = self.change_counter.wrapping_add(1);
            let pc = self.page_count;
            let ft = self.freelist_trunk;
            let fc = self.freelist_count;
            let sc = self.schema_cookie;
            let uv = self.user_version;
            let aid = self.application_id;
            if let Some(p1_arc) = Arc::make_mut(&mut self.pages).get_mut(1) {
                let p1 = arc_bytes_make_mut(p1_arc);
                p1[24..28].copy_from_slice(&cc.to_be_bytes());
                p1[28..32].copy_from_slice(&pc.to_be_bytes());
                p1[32..36].copy_from_slice(&ft.to_be_bytes());
                p1[36..40].copy_from_slice(&fc.to_be_bytes());
                p1[40..44].copy_from_slice(&sc.to_be_bytes());
                p1[60..64].copy_from_slice(&uv.to_be_bytes());
                p1[68..72].copy_from_slice(&aid.to_be_bytes());
                p1[92..96].copy_from_slice(&cc.to_be_bytes());
                p1[96..100].copy_from_slice(&SQLITE_VERSION_NUMBER.to_be_bytes());
            }
            self.change_counter = cc;
            self.mut_gen = self.mut_gen.wrapping_add(1);
        }

        let Some((vfs, main_path, mut main_file)) = locked_vfs else {
            self.dirty_originals.clear();
            self.dirty_in_memory = false;
            self.commit_orig_page_count = self.page_count;
            return Ok(());
        };

        let journal_path = format!("{main_path}-journal");
        let ps = self.page_size;
        let mut journal_synced = false;

        let commit_res: SqliteResult<()> = (|| {
            // Collect original pre-transaction pages that existed on disk (for rollback journal).
            let mut journal_pages: Vec<(u32, &[u8])> = self
                .dirty_originals
                .iter()
                .filter_map(|(&pgno, orig)| orig.as_ref().map(|bytes| (pgno, bytes.as_ref())))
                .collect();
            journal_pages.sort_by_key(|(pgno, _)| *pgno);

            // Step 1: Always write & sync a genuine SQLite 3 rollback journal (including
            // `db_size = 0, n_rec = 0` on the initial commit so a crash mid-first-commit
            // cleanly truncates the partial DB back to 0 bytes, matching C SQLite!).
            self.journal_nonce = self
                .journal_nonce
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            let nonce = self.journal_nonce;
            let sector = JOURNAL_SECTOR_SIZE as usize;

            let mut jbuf = vec![0u8; sector];
            jbuf[0..8].copy_from_slice(JOURNAL_MAGIC);
            jbuf[8..12].copy_from_slice(&(journal_pages.len() as u32).to_be_bytes());
            jbuf[12..16].copy_from_slice(&nonce.to_be_bytes());
            jbuf[16..20].copy_from_slice(&self.commit_orig_page_count.to_be_bytes());
            jbuf[20..24].copy_from_slice(&JOURNAL_SECTOR_SIZE.to_be_bytes());
            jbuf[24..28].copy_from_slice(&ps.to_be_bytes());

            let mut jfile = vfs.open(&journal_path, OpenFlags::create(FileKind::MainJournal))?;
            let jres: SqliteResult<()> = (|| {
                // Truncate any stale prior journal contents so trailing segments can never survive.
                jfile.truncate(0)?;
                let mut joff = 0u64;
                jfile.write(&jbuf, joff)?;
                joff += sector as u64;

                let mut rec = Vec::with_capacity(4 + ps as usize + 4);
                for (pgno, orig_bytes) in &journal_pages {
                    rec.clear();
                    rec.extend_from_slice(&pgno.to_be_bytes());
                    rec.extend_from_slice(orig_bytes);
                    let cksum = journal_checksum(nonce, orig_bytes, 0, ps as usize);
                    rec.extend_from_slice(&cksum.to_be_bytes());
                    jfile.write(&rec, joff)?;
                    joff += rec.len() as u64;
                }
                jfile.sync(false)?;
                Ok(())
            })();
            let close_res = jfile.close();
            jres?;
            close_res?;
            journal_synced = true;

            // Step 2: Write all dirty pages to main database file and sync!
            let mut dirty_pgnos: Vec<u32> = self.dirty_originals.keys().copied().collect();
            dirty_pgnos.sort_unstable();
            for pgno in dirty_pgnos {
                if let Some(bytes) = self.pages.get(pgno) {
                    let off = u64::from(pgno - 1) * u64::from(ps);
                    main_file.write(bytes, off)?;
                }
            }
            let target_size = u64::from(self.page_count) * u64::from(ps);
            if main_file.size()? > target_size {
                main_file.truncate(target_size)?;
            }
            main_file.sync(false)?;

            // Step 3: Commit complete — delete rollback journal!
            vfs.delete(&journal_path, true)?;
            Ok(())
        })();

        let _ = main_file.unlock(LockLevel::None);
        let _ = main_file.close();
        if let Err(e) = commit_res {
            // Restore pre-commit page-1 header in memory so subsequent ROLLBACK or retry stays exact!
            self.change_counter = old_cc;
            if let Some(orig_p1) = old_p1 {
                Arc::make_mut(&mut self.pages).insert(1, orig_p1);
            }
            self.mut_gen = self.mut_gen.wrapping_add(1);
            if journal_synced {
                self.disk_needs_journal_recovery = true;
            }
            return Err(e);
        }

        self.dirty_originals.clear();
        self.commit_orig_page_count = self.page_count;
        Ok(())
    }

    /// Atomically replace the entire database image with a compacted `VACUUM` pager.
    /// When VFS-backed, journals all pre-VACUUM pages using the original page size before
    /// writing the compacted pages and truncating the main file (crash-safe even across
    /// `PRAGMA page_size` changes!).
    pub fn replace_from_vacuum(&mut self, mut compacted: WritePager) -> SqliteResult<()> {
        if self.in_transaction() {
            return Err(SqliteError::Sql(
                "cannot VACUUM from within a transaction".to_string(),
            ));
        }
        compacted.schema_cookie = self.schema_cookie.wrapping_add(1);
        compacted.change_counter = self.change_counter.wrapping_add(1);
        compacted.user_version = self.user_version;
        compacted.application_id = self.application_id;
        compacted.schema_format = self.schema_format;
        compacted.freelist_trunk = 0;
        compacted.freelist_count = 0;
        if compacted.page_count > 0 {
            compacted.sync_page1_header()?;
        }

        if let Some(ref backing) = self.backing {
            let vfs = backing.vfs.clone();
            let main_path = backing.path.clone();
            let journal_path = format!("{main_path}-journal");
            let old_ps = self.page_size;
            let new_ps = compacted.page_size;
            let orig_pc = self.commit_orig_page_count;

            let mut main_file = vfs.open(&main_path, OpenFlags::create(FileKind::MainDb))?;
            if let Err(e) = acquire_exclusive_lock(main_file.as_mut(), &main_path) {
                let _ = main_file.close();
                return Err(e);
            }

            let mut journal_synced = false;
            let commit_res: SqliteResult<()> = (|| {
                if self.disk_needs_journal_recovery {
                    Self::playback_hot_journal_locked(
                        &vfs,
                        &main_path,
                        main_file.as_mut(),
                        old_ps,
                    )?;
                    self.disk_needs_journal_recovery = false;
                }

                // Step 1: Journal every original on-disk page under `old_ps` so a mid-VACUUM
                // crash restores the pre-VACUUM database verbatim (including original page size).
                let old_lbp = lock_byte_page(old_ps);
                let mut journal_pages: Vec<(u32, Arc<[u8]>)> = Vec::new();
                for pgno in 1..=orig_pc {
                    if pgno == old_lbp {
                        continue;
                    }
                    if let Some(bytes) = self.pages.get(pgno) {
                        journal_pages.push((pgno, Arc::clone(bytes)));
                    }
                }
                self.journal_nonce = self
                    .journal_nonce
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                let nonce = self.journal_nonce;
                let sector = JOURNAL_SECTOR_SIZE as usize;

                let mut jbuf = vec![0u8; sector];
                jbuf[0..8].copy_from_slice(JOURNAL_MAGIC);
                jbuf[8..12].copy_from_slice(&(journal_pages.len() as u32).to_be_bytes());
                jbuf[12..16].copy_from_slice(&nonce.to_be_bytes());
                jbuf[16..20].copy_from_slice(&orig_pc.to_be_bytes());
                jbuf[20..24].copy_from_slice(&JOURNAL_SECTOR_SIZE.to_be_bytes());
                jbuf[24..28].copy_from_slice(&old_ps.to_be_bytes());

                let mut jfile = vfs.open(&journal_path, OpenFlags::create(FileKind::MainJournal))?;
                let jres: SqliteResult<()> = (|| {
                    jfile.truncate(0)?;
                    let mut joff = 0u64;
                    jfile.write(&jbuf, joff)?;
                    joff += sector as u64;
                    let mut rec = Vec::with_capacity(4 + old_ps as usize + 4);
                    for (pgno, orig_bytes) in &journal_pages {
                        rec.clear();
                        rec.extend_from_slice(&pgno.to_be_bytes());
                        rec.extend_from_slice(orig_bytes);
                        let cksum = journal_checksum(nonce, orig_bytes, 0, old_ps as usize);
                        rec.extend_from_slice(&cksum.to_be_bytes());
                        jfile.write(&rec, joff)?;
                        joff += rec.len() as u64;
                    }
                    jfile.sync(false)?;
                    Ok(())
                })();
                let close_res = jfile.close();
                jres?;
                close_res?;
                journal_synced = true;

                // Step 2: Write all compacted pages under `new_ps` and truncate excess pages.
                let new_lbp = lock_byte_page(new_ps);
                for pgno in 1..=compacted.page_count {
                    if pgno == new_lbp {
                        continue;
                    }
                    if let Some(bytes) = compacted.pages.get(pgno) {
                        let off = u64::from(pgno - 1) * u64::from(new_ps);
                        main_file.write(bytes, off)?;
                    }
                }
                let target_size = u64::from(compacted.page_count) * u64::from(new_ps);
                if main_file.size()? != target_size {
                    main_file.truncate(target_size)?;
                }
                main_file.sync(false)?;

                // Step 3: Commit complete — delete rollback journal!
                vfs.delete(&journal_path, true)?;
                Ok(())
            })();

            let _ = main_file.unlock(LockLevel::None);
            let _ = main_file.close();
            if let Err(e) = commit_res {
                if journal_synced {
                    self.disk_needs_journal_recovery = true;
                }
                return Err(e);
            }
        }

        // Swap in-memory state and reconcile metered memory accounting cleanly.
        if self.tracked_bytes > 0 {
            self.meter.release_memory(self.tracked_bytes);
        }
        self.tracked_bytes = compacted.tracked_bytes;
        compacted.tracked_bytes = 0;

        self.page_size = compacted.page_size;
        self.reserved_bytes = compacted.reserved_bytes;
        self.limits = compacted.limits;
        self.encoding = compacted.encoding;
        self.page_count = compacted.page_count;
        self.freelist_trunk = 0;
        self.freelist_count = 0;
        self.schema_cookie = compacted.schema_cookie;
        self.schema_format = compacted.schema_format;
        self.change_counter = compacted.change_counter;
        self.user_version = compacted.user_version;
        self.application_id = compacted.application_id;
        self.pages = Arc::clone(&compacted.pages);
        self.dirty_originals.clear();
        self.dirty_in_memory = false;
        self.commit_orig_page_count = self.page_count;
        self.pending_page_size = None;
        self.schema_gen = self.schema_gen.wrapping_add(1);
        self.mut_gen = self.mut_gen.wrapping_add(1);
        Ok(())
    }

    /// Export the complete live SQLite 3 database as a contiguous byte vector.
    /// Always produces a valid SQLite 3 `.db` file image readable by C SQLite & `xdb-sqlite`.
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.page_count == 0 {
            return Vec::new();
        }
        let ps = self.page_size as usize;
        let mut out = vec![0u8; (self.page_count as usize) * ps];
        let pc = self.page_count;
        self.pages.for_each(|pgno, page| {
            if pgno >= 1 && pgno <= pc {
                let off = ((pgno - 1) as usize) * ps;
                out[off..off + ps].copy_from_slice(page);
            }
        });
        // Validate header consistency in exported bytes
        let _ = header_page_size(&out);
        out
    }

    /// Build a live read-only [`crate::btree::Btree`] view over the current uncommitted pages
    /// (O(1) copy-on-write snapshot — zero disk I/O and zero page cloning!).
    pub fn snapshot_page_source(&self) -> Box<dyn PageSource> {
        Box::new(LivePagesSource {
            page_size: self.page_size,
            page_count: self.page_count,
            pages: Arc::clone(&self.pages),
        })
    }

    /// True if one or more nested savepoints are active beyond the outer explicit transaction.
    #[inline]
    pub fn has_savepoints(&self) -> bool {
        self.undo_stack.len() > usize::from(self.in_explicit_tx)
    }

    /// Build an O(1) copy-on-write in-memory overlay pager sharing `self.pages` via `Arc`.
    pub(crate) fn clone_cow_overlay(&self) -> Self {
        Self {
            backing: None,
            meter: Arc::new(NoopMeter),
            noop_meter: true,
            page_size: self.page_size,
            reserved_bytes: self.reserved_bytes,
            limits: self.limits,
            encoding: self.encoding,
            page_count: self.page_count,
            freelist_trunk: self.freelist_trunk,
            freelist_count: self.freelist_count,
            schema_cookie: self.schema_cookie,
            schema_format: self.schema_format,
            change_counter: self.change_counter,
            user_version: self.user_version,
            application_id: self.application_id,
            pages: Arc::clone(&self.pages),
            schema_gen: self.schema_gen,
            mut_gen: self.mut_gen,
            dirty_originals: HashMap::new(),
            dirty_in_memory: false,
            commit_orig_page_count: self.page_count,
            undo_stack: Vec::new(),
            in_explicit_tx: false,
            journal_nonce: self.journal_nonce,
            disk_needs_journal_recovery: false,
            tracked_bytes: 0,
            pending_page_size: None,
        }
    }
}

type CompanionFingerprint = (u64, u64, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskRecoveryFingerprint {
    main_size: u64,
    main_hdr: [u8; 100],
    wal: Option<CompanionFingerprint>,
    journal: Option<CompanionFingerprint>,
}

/// Compute a full-content `(size, hash1, hash2)` fingerprint over a companion log file (`-wal` or
/// `-journal`) so mid-file overwrites (e.g. post-checkpoint WAL frames) are always detected.
fn read_companion_fingerprint(
    vfs: &dyn Vfs,
    main_path: &str,
    suffix: &str,
    kind: FileKind,
) -> SqliteResult<Option<CompanionFingerprint>> {
    let comp_path = format!("{main_path}{suffix}");
    if !vfs.exists(&comp_path)? {
        return Ok(None);
    }
    let mut cf = match vfs.open(&comp_path, OpenFlags::readonly(kind)) {
        Ok(f) => f,
        Err(e) if e.code == IoErrorCode::CantOpen => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let res = (|| -> SqliteResult<CompanionFingerprint> {
        let sz = cf.size()?;
        let mut h1: u64 = 0xcbf2_9ce4_8422_2325u64 ^ sz;
        let mut h2: u64 = 0x9e37_79b9_7f4a_7c15u64.wrapping_add(sz);
        let mut buf = [0u8; 8192];
        let mut off = 0u64;
        while off < sz {
            let chunk = ((sz - off) as usize).min(buf.len());
            cf.read(&mut buf[..chunk], off)?;
            for &b in &buf[..chunk] {
                h1 ^= u64::from(b);
                h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
                h2 = h2
                    .wrapping_mul(0x517c_c1b7_2722_0a95)
                    .wrapping_add(u64::from(b).wrapping_add(1));
                h2 ^= h2 >> 33;
            }
            off += chunk as u64;
        }
        Ok((sz, h1, h2))
    })();
    let _ = cf.close();
    res.map(Some)
}

/// Read the current on-disk main DB, `-wal`, and `-journal` fingerprint under Shared lock so
/// recovery can verify no concurrent writer committed or recovered before our Exclusive lock.
fn read_disk_header_fingerprint(
    vfs: &dyn Vfs,
    path: &str,
) -> SqliteResult<DiskRecoveryFingerprint> {
    let mut f = vfs.open(path, OpenFlags::readonly(FileKind::MainDb))?;
    let res = (|| {
        if !f.lock(LockLevel::Shared)? {
            return Err(IoError::with_path(
                IoErrorCode::Busy,
                format!("database is locked: {path}"),
                path,
            )
            .into());
        }
        let sz = f.size()?;
        let mut hdr = [0u8; 100];
        if sz >= 100 {
            f.read(&mut hdr, 0)?;
        }
        let wal = read_companion_fingerprint(vfs, path, "-wal", FileKind::Wal)?;
        let journal = read_companion_fingerprint(vfs, path, "-journal", FileKind::MainJournal)?;
        Ok(DiskRecoveryFingerprint {
            main_size: sz,
            main_hdr: hdr,
            wal,
            journal,
        })
    })();
    let _ = f.unlock(LockLevel::None);
    let _ = f.close();
    res
}

/// Acquire SQLite's Shared -> Reserved -> Exclusive lock progression, returning `Busy` on contention.
fn acquire_exclusive_lock(file: &mut dyn xdb_core::VfsFile, path: &str) -> SqliteResult<()> {
    for level in [LockLevel::Shared, LockLevel::Reserved, LockLevel::Exclusive] {
        if !file.lock(level)? {
            let _ = file.unlock(LockLevel::None);
            return Err(IoError::with_path(
                IoErrorCode::Busy,
                format!("database is locked: {path}"),
                path,
            )
            .into());
        }
    }
    Ok(())
}

/// Initialize a blank B-tree page header on `buf` (`pgno = 1` starts at offset 100).
pub fn init_empty_btree_page(buf: &mut [u8], pgno: u32, kind: BtreePageType, usable_size: u32) {
    let hdr = if pgno == 1 { HEADER_SIZE } else { 0 };
    buf[hdr..].fill(0);
    buf[hdr] = kind as u8;
    // first_freeblock = 0 (hdr+1..hdr+3)
    // n_cell = 0 (hdr+3..hdr+5)
    let content_raw: u16 = if usable_size == 65536 {
        0
    } else {
        usable_size as u16
    };
    buf[hdr + 5..hdr + 7].copy_from_slice(&content_raw.to_be_bytes());
    buf[hdr + 7] = 0; // fragmented_bytes
}

/// Read-only `PageSource` over a copy-on-write snapshot of `WritePager`'s pages.
#[derive(Debug)]
struct LivePagesSource {
    page_size: u32,
    page_count: u32,
    pages: Arc<PageStore>,
}

impl PageSource for LivePagesSource {
    fn page_size(&self) -> u32 {
        self.page_size
    }

    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn read_page(&mut self, pgno: u32) -> SqliteResult<Vec<u8>> {
        if pgno < 1 || pgno > self.page_count {
            return Err(SqliteError::corrupt(format!("page {pgno} out of range")));
        }
        Ok(self
            .pages
            .get(pgno)
            .map(|p| p.to_vec())
            .unwrap_or_else(|| vec![0u8; self.page_size as usize]))
    }

    fn read_page_shared(&mut self, pgno: u32) -> SqliteResult<Arc<[u8]>> {
        if pgno < 1 || pgno > self.page_count {
            return Err(SqliteError::corrupt(format!("page {pgno} out of range")));
        }
        Ok(self
            .pages
            .get(pgno)
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::from(vec![0u8; self.page_size as usize])))
    }
}
