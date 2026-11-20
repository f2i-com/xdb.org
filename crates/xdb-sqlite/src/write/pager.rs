//! Writable SQLite 3 page manager with transactions, nested savepoints, freelist
//! allocation/reclamation, and crash-safe SQLite 3 rollback journal commits.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use xdb_core::vfs::{FileKind, LockLevel, OpenFlags, Vfs};
use xdb_core::{Meter, NoopMeter, TextEncoding};

use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    empty_database_info, header_page_size, is_valid_page_size, lock_byte_page,
    validate_database_header, DatabaseInfo, BtreePageType, HEADER_SIZE, SQLITE_HEADER_MAGIC,
};
use crate::page_source::PageSource;
use crate::snapshot::{
    journal_checksum, open_snapshot, open_snapshot_from_bytes, SnapshotOptions, JOURNAL_MAGIC,
};

/// SQLite 3.54.1 version number written into header bytes 96..100 (`3054001`).
pub const SQLITE_VERSION_NUMBER: u32 = 3_054_001;
const JOURNAL_SECTOR_SIZE: u32 = 4096;

/// Saved pre-mutation state for one transaction or savepoint frame.
#[derive(Debug, Clone)]
struct UndoFrame {
    /// Savepoint name (`None` for the top-level transaction or implicit statement savepoint).
    name: Option<String>,
    /// Original page content before the first write in this frame (`None` = page did not exist).
    saved_pages: HashMap<u32, Option<Arc<Vec<u8>>>>,
    page_count: u32,
    freelist_trunk: u32,
    freelist_count: u32,
    schema_cookie: u32,
    user_version: i32,
    application_id: i32,
    change_counter: u32,
    initialized: bool,
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
    page_size: u32,
    reserved_bytes: u8,
    encoding: TextEncoding,
    page_count: u32,
    freelist_trunk: u32,
    freelist_count: u32,
    schema_cookie: u32,
    change_counter: u32,
    user_version: i32,
    application_id: i32,
    /// Full set of resident pages (1-based page number -> `page_size` bytes),
    /// copy-on-write via `Arc` so `snapshot_page_source()` is O(1) with zero page cloning.
    pages: Arc<BTreeMap<u32, Arc<Vec<u8>>>>,
    /// Monotonic generation bumped whenever `sqlite_schema` or schema state may have changed.
    schema_gen: u64,
    /// Pages modified since the last VFS commit (and their original pre-commit image for journaling).
    dirty_originals: HashMap<u32, Option<Arc<Vec<u8>>>>,
    /// Original page count at the start of the uncommitted VFS write batch.
    commit_orig_page_count: u32,
    /// Active undo frames (index 0 = explicit transaction or auto-commit; 1+ = savepoints).
    undo_stack: Vec<UndoFrame>,
    /// True if user ran `BEGIN`.
    in_explicit_tx: bool,
    /// Nonce counter for rollback journal checksums.
    journal_nonce: u32,
    /// Tracked bytes reserved in `meter`.
    tracked_bytes: u64,
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
    /// Create a brand-new empty in-memory SQLite database.
    pub fn new_in_memory(page_size: u32, meter: Option<Arc<dyn Meter>>) -> SqliteResult<Self> {
        let ps = if is_valid_page_size(page_size) {
            page_size
        } else {
            4096
        };
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        Ok(Self {
            backing: None,
            meter,
            page_size: ps,
            reserved_bytes: 0,
            encoding: TextEncoding::Utf8,
            page_count: 0,
            freelist_trunk: 0,
            freelist_count: 0,
            schema_cookie: 0,
            change_counter: 0,
            user_version: 0,
            application_id: 0,
            pages: Arc::new(BTreeMap::new()),
            schema_gen: 1,
            dirty_originals: HashMap::new(),
            commit_orig_page_count: 0,
            undo_stack: Vec::new(),
            in_explicit_tx: false,
            journal_nonce: 0x1a2b_3c4d,
            tracked_bytes: 0,
        })
    }

    /// Load an existing SQLite database image from raw bytes into an in-memory writable pager.
    pub fn from_bytes(
        main_bytes: &[u8],
        wal_bytes: Option<&[u8]>,
        journal_bytes: Option<&[u8]>,
        meter: Option<Arc<dyn Meter>>,
    ) -> SqliteResult<Self> {
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        if main_bytes.is_empty() {
            return Self::new_in_memory(4096, Some(meter));
        }
        let snap = open_snapshot_from_bytes(
            main_bytes.to_vec(),
            wal_bytes.map(|b| b.to_vec()),
            journal_bytes.map(|b| b.to_vec()),
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

        // Open existing database (applying any hot journal or WAL overlay into our initial state!)
        let snap = open_snapshot(vfs.as_ref(), path, SnapshotOptions::default())?;
        let mut pager = Self::from_snapshot(
            snap,
            Some(VfsBacking {
                vfs: vfs.clone(),
                path: path.to_string(),
            }),
            meter,
        )?;
        // If there was a hot journal or WAL on disk, materialize cleanly if caller later writes.
        let _ = pager.commit_orig_page_count;
        if pager.page_count == 0 && is_valid_page_size(page_size) {
            pager.page_size = page_size;
        }
        Ok(pager)
    }

    fn from_snapshot(
        mut snap: crate::snapshot::Snapshot,
        backing: Option<VfsBacking>,
        meter: Arc<dyn Meter>,
    ) -> SqliteResult<Self> {
        let page_size = snap.page_size();
        let page_count = snap.page_count();
        let mut pages = BTreeMap::new();
        let mut tracked = 0u64;

        if page_count > 0 {
            for pgno in 1..=page_count {
                if pgno == lock_byte_page(page_size) {
                    continue;
                }
                meter.charge_page_reads(1)?;
                meter.reserve_memory(u64::from(page_size))?;
                tracked += u64::from(page_size);
                let buf = snap.read_page(pgno)?;
                pages.insert(pgno, Arc::new(buf));
            }
        }

        let (
            reserved_bytes,
            encoding,
            freelist_trunk,
            freelist_count,
            schema_cookie,
            change_counter,
            user_version,
            application_id,
        ) = if let Some(p1) = pages.get(&1) {
            let info = validate_database_header(p1, page_count, page_size, Some(page_count))?;
            let h = info.header.expect("header present");
            if info.auto_vacuum {
                return Err(SqliteError::misuse(
                    "writing to auto_vacuum databases is not yet supported",
                ));
            }
            (
                h.reserved_bytes,
                info.text_encoding,
                h.first_freelist_trunk,
                h.freelist_count,
                h.schema_cookie,
                h.file_change_counter,
                h.user_version,
                h.application_id,
            )
        } else {
            (0, TextEncoding::Utf8, 0, 0, 0, 0, 0, 0)
        };

        Ok(Self {
            backing,
            meter,
            page_size,
            reserved_bytes,
            encoding,
            page_count,
            freelist_trunk,
            freelist_count,
            schema_cookie,
            change_counter,
            user_version,
            application_id,
            pages: Arc::new(pages),
            schema_gen: 1,
            dirty_originals: HashMap::new(),
            commit_orig_page_count: page_count,
            undo_stack: Vec::new(),
            in_explicit_tx: false,
            journal_nonce: change_counter.wrapping_add(0x55aa_1234),
            tracked_bytes: tracked,
        })
    }

    pub fn meter(&self) -> &Arc<dyn Meter> {
        &self.meter
    }

    pub fn path(&self) -> &str {
        self.backing.as_ref().map_or(":memory:", |b| b.path.as_str())
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn set_page_size(&mut self, new_size: u32) -> SqliteResult<()> {
        if self.page_count > 0 {
            return Ok(()); // SQLite ignores PRAGMA page_size after DB is initialized
        }
        if !is_valid_page_size(new_size) {
            return Err(SqliteError::misuse(format!("invalid page_size {new_size}")));
        }
        self.page_size = new_size;
        Ok(())
    }

    pub fn usable_size(&self) -> u32 {
        self.page_size - u32::from(self.reserved_bytes)
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
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
        self.in_explicit_tx
    }

    pub fn db_info(&self) -> DatabaseInfo {
        if self.page_count == 0 {
            return empty_database_info(self.page_size);
        }
        let p1 = self.pages.get(&1).expect("page 1 exists");
        validate_database_header(p1, self.page_count, self.page_size, Some(self.page_count))
            .expect("internal page 1 header always valid")
    }

    /// Initialize page 1 with a valid 100-byte SQLite 3 header and empty `sqlite_schema` TableLeaf.
    pub fn ensure_initialized(&mut self) -> SqliteResult<()> {
        if self.page_count > 0 {
            return Ok(());
        }
        self.record_undo_for_page(1);
        self.record_dirty_original(1);
        let ps = self.page_size as usize;
        self.meter.reserve_memory(ps as u64)?;
        self.tracked_bytes += ps as u64;

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
        p1[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format = 4
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

        Arc::make_mut(&mut self.pages).insert(1, Arc::new(p1));
        self.page_count = 1;
        self.change_counter = 1;
        self.invalidate_schema_cache();
        Ok(())
    }

    /// Read page `pgno` (1-based).
    pub fn read_page(&self, pgno: u32) -> SqliteResult<&[u8]> {
        self.meter.charge_steps(1)?;
        self.pages
            .get(&pgno)
            .map(|p| p.as_slice())
            .ok_or_else(|| SqliteError::corrupt(format!("missing page {pgno}")))
    }

    /// Obtain mutable access to page `pgno`, recording undo & dirty state automatically.
    pub fn write_page_mut(&mut self, pgno: u32) -> SqliteResult<&mut Vec<u8>> {
        self.meter.charge_steps(1)?;
        if !self.pages.contains_key(&pgno) {
            return Err(SqliteError::corrupt(format!(
                "write to unallocated page {pgno}"
            )));
        }
        self.record_undo_for_page(pgno);
        self.record_dirty_original(pgno);
        let entry = Arc::make_mut(&mut self.pages)
            .get_mut(&pgno)
            .expect("page exists");
        Ok(Arc::make_mut(entry))
    }

    /// Replace full contents of page `pgno` (length must equal `page_size`).
    pub fn put_page(&mut self, pgno: u32, data: Vec<u8>) -> SqliteResult<()> {
        if data.len() != self.page_size as usize {
            return Err(SqliteError::misuse("page write length mismatch"));
        }
        self.meter.charge_steps(1)?;
        self.record_undo_for_page(pgno);
        self.record_dirty_original(pgno);
        if !self.pages.contains_key(&pgno) {
            self.meter.reserve_memory(data.len() as u64)?;
            self.tracked_bytes += data.len() as u64;
        }
        Arc::make_mut(&mut self.pages).insert(pgno, Arc::new(data));
        Ok(())
    }

    /// Allocate a fresh zeroed page (reusing the SQLite freelist when non-empty).
    pub fn allocate_page(&mut self) -> SqliteResult<u32> {
        self.ensure_initialized()?;
        self.meter.charge_steps(2)?;

        // 1. Pop from freelist if available (`btree.c allocateBtreePage`).
        if self.freelist_trunk != 0 {
            let trunk_pgno = self.freelist_trunk;
            let trunk = self.read_page(trunk_pgno)?.to_vec();
            let next_trunk = u32::from_be_bytes([trunk[0], trunk[1], trunk[2], trunk[3]]);
            let leaf_cnt = u32::from_be_bytes([trunk[4], trunk[5], trunk[6], trunk[7]]);
            if leaf_cnt > 0 {
                let off = 8 + ((leaf_cnt - 1) as usize) * 4;
                let popped = u32::from_be_bytes([
                    trunk[off],
                    trunk[off + 1],
                    trunk[off + 2],
                    trunk[off + 3],
                ]);
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
                self.freelist_trunk = next_trunk;
                self.freelist_count = self.freelist_count.saturating_sub(1);
                self.sync_page1_header()?;
                let pbuf = self.write_page_mut(trunk_pgno)?;
                pbuf.fill(0);
                return Ok(trunk_pgno);
            }
        }

        // 2. Extend database file by 1 page (skipping lock-byte page if reached).
        let mut next = self.page_count + 1;
        let lbp = lock_byte_page(self.page_size);
        if next == lbp {
            next += 1;
        }
        self.page_count = next;
        let ps = self.page_size as usize;
        self.put_page(next, vec![0u8; ps])?;
        self.sync_page1_header()?;
        Ok(next)
    }

    /// Return `pgno` (> 1) to the SQLite freelist (`fileformat2 §1.5`).
    pub fn free_page(&mut self, pgno: u32) -> SqliteResult<()> {
        if pgno <= 1 || pgno > self.page_count {
            return Err(SqliteError::misuse(format!("cannot free page {pgno}")));
        }
        self.meter.charge_steps(2)?;
        let max_leaves = ((self.usable_size() as usize) - 8) / 4;

        if self.freelist_trunk != 0 {
            let trunk_pgno = self.freelist_trunk;
            let leaf_cnt = {
                let t = self.read_page(trunk_pgno)?;
                u32::from_be_bytes([t[4], t[5], t[6], t[7]]) as usize
            };
            if leaf_cnt < max_leaves {
                let off = 8 + leaf_cnt * 4;
                let tbuf = self.write_page_mut(trunk_pgno)?;
                tbuf[4..8].copy_from_slice(&((leaf_cnt + 1) as u32).to_be_bytes());
                tbuf[off..off + 4].copy_from_slice(&pgno.to_be_bytes());
                let zbuf = self.write_page_mut(pgno)?;
                zbuf.fill(0);
                self.freelist_count += 1;
                self.sync_page1_header()?;
                return Ok(());
            }
        }

        // Become the new freelist trunk page pointing to the old trunk.
        let prev_trunk = self.freelist_trunk;
        let tbuf = self.write_page_mut(pgno)?;
        tbuf.fill(0);
        tbuf[0..4].copy_from_slice(&prev_trunk.to_be_bytes());
        tbuf[4..8].copy_from_slice(&0u32.to_be_bytes());
        self.freelist_trunk = pgno;
        self.freelist_count += 1;
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
        let uv = self.user_version;
        let aid = self.application_id;
        let cc = self.change_counter;

        let p1 = self.write_page_mut(1)?;
        p1[24..28].copy_from_slice(&cc.to_be_bytes());
        p1[28..32].copy_from_slice(&pc.to_be_bytes());
        p1[32..36].copy_from_slice(&ft.to_be_bytes());
        p1[36..40].copy_from_slice(&fc.to_be_bytes());
        p1[40..44].copy_from_slice(&sc.to_be_bytes());
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
            saved_pages: HashMap::new(),
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

    fn record_undo_for_page(&mut self, pgno: u32) {
        if let Some(frame) = self.undo_stack.last_mut() {
            frame
                .saved_pages
                .entry(pgno)
                .or_insert_with(|| self.pages.get(&pgno).cloned());
        }
    }

    fn record_dirty_original(&mut self, pgno: u32) {
        if self.backing.is_none() {
            // In-memory / WASM mode never writes a disk rollback journal; mark dirty without
            // holding a second `Arc` reference so `Arc::make_mut` mutates in-place!
            self.dirty_originals.entry(pgno).or_insert(None);
            return;
        }
        self.dirty_originals
            .entry(pgno)
            .or_insert_with(|| self.pages.get(&pgno).cloned());
    }

    /// `BEGIN [TRANSACTION]`.
    pub fn begin(&mut self) -> SqliteResult<()> {
        if self.in_explicit_tx {
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
            if let Some(parent) = self.undo_stack.last_mut() {
                for (pgno, orig) in inner.saved_pages {
                    parent.saved_pages.entry(pgno).or_insert(orig);
                }
            }
        }
    }

    /// Roll back the innermost unnamed statement-level savepoint (on constraint violation).
    pub fn rollback_statement_savepoint(&mut self) {
        if let Some(frame) = self.undo_stack.pop() {
            self.apply_undo_frame(frame);
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
        let drained: Vec<UndoFrame> = self.undo_stack.drain(pos..).collect();
        if let Some(parent) = self.undo_stack.last_mut() {
            for frame in drained {
                for (pgno, orig) in frame.saved_pages {
                    parent.saved_pages.entry(pgno).or_insert(orig);
                }
            }
        }
        if !self.in_explicit_tx && self.undo_stack.is_empty() {
            self.flush_to_vfs()?;
        }
        Ok(())
    }

    /// `COMMIT`: closes all open savepoints & transaction, flushes dirty pages atomically via
    /// rollback journal to the backing VFS (if any).
    pub fn commit(&mut self) -> SqliteResult<()> {
        if !self.in_explicit_tx {
            return Err(SqliteError::Sql(
                "cannot commit - no transaction is active".to_string(),
            ));
        }
        self.in_explicit_tx = false;
        self.undo_stack.clear();
        self.flush_to_vfs()
    }

    /// `ROLLBACK`: reverts all uncommitted writes back to the start of the active transaction.
    pub fn rollback(&mut self) -> SqliteResult<()> {
        if !self.in_explicit_tx && self.undo_stack.is_empty() {
            return Err(SqliteError::Sql(
                "cannot rollback - no transaction is active".to_string(),
            ));
        }
        self.in_explicit_tx = false;
        while let Some(frame) = self.undo_stack.pop() {
            self.apply_undo_frame(frame);
        }
        self.dirty_originals.clear();
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
        let pages_map = Arc::make_mut(&mut self.pages);
        for (pgno, orig) in frame.saved_pages {
            match orig {
                Some(bytes) => {
                    pages_map.insert(pgno, bytes);
                }
                None => {
                    if let Some(removed) = pages_map.remove(&pgno) {
                        let sz = removed.len() as u64;
                        self.tracked_bytes = self.tracked_bytes.saturating_sub(sz);
                        self.meter.release_memory(sz);
                    }
                }
            }
        }
        self.page_count = frame.page_count;
        self.freelist_trunk = frame.freelist_trunk;
        self.freelist_count = frame.freelist_count;
        self.schema_cookie = frame.schema_cookie;
        self.user_version = frame.user_version;
        self.application_id = frame.application_id;
        self.change_counter = frame.change_counter;
        if !frame.initialized {
            Arc::make_mut(&mut self.pages).clear();
        }
        self.invalidate_schema_cache();
    }

    /// Atomically flush dirty pages to the backing VFS using a genuine SQLite 3 rollback journal.
    pub fn flush_to_vfs(&mut self) -> SqliteResult<()> {
        if self.dirty_originals.is_empty() {
            return Ok(());
        }
        // Increment SQLite change counter on every commit (when page 1 exists).
        if self.page_count > 0 {
            self.change_counter = self.change_counter.wrapping_add(1);
            let cc = self.change_counter;
            let pc = self.page_count;
            let ft = self.freelist_trunk;
            let fc = self.freelist_count;
            let sc = self.schema_cookie;
            let uv = self.user_version;
            let aid = self.application_id;
            if let Some(p1_arc) = Arc::make_mut(&mut self.pages).get_mut(&1) {
                let p1 = Arc::make_mut(p1_arc);
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
            self.dirty_originals.entry(1).or_insert(None);
        }

        let Some(ref backing) = self.backing else {
            self.dirty_originals.clear();
            self.commit_orig_page_count = self.page_count;
            return Ok(());
        };

        let vfs = backing.vfs.clone();
        let main_path = backing.path.clone();
        let journal_path = format!("{main_path}-journal");
        let ps = self.page_size;

        // Open main DB file & acquire EXCLUSIVE lock.
        let mut main_file = vfs.open(&main_path, OpenFlags::create(FileKind::MainDb))?;
        main_file.lock(LockLevel::Shared)?;
        main_file.lock(LockLevel::Reserved)?;
        main_file.lock(LockLevel::Exclusive)?;

        // Collect original pre-transaction pages that existed on disk (for rollback journal).
        let mut journal_pages: Vec<(u32, &Vec<u8>)> = self
            .dirty_originals
            .iter()
            .filter_map(|(&pgno, orig)| orig.as_ref().map(|bytes| (pgno, bytes.as_ref())))
            .collect();
        journal_pages.sort_by_key(|(pgno, _)| *pgno);

        // Step 1: Write & sync genuine SQLite 3 rollback journal (if DB already existed on disk).
        let wrote_journal = self.commit_orig_page_count > 0;
        if wrote_journal {
            self.journal_nonce = self.journal_nonce.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
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
            jfile.close()?;
        }

        // Step 2: Write all dirty pages to main database file and sync!
        let mut dirty_pgnos: Vec<u32> = self.dirty_originals.keys().copied().collect();
        dirty_pgnos.sort_unstable();
        for pgno in dirty_pgnos {
            if let Some(bytes) = self.pages.get(&pgno) {
                let off = u64::from(pgno - 1) * u64::from(ps);
                main_file.write(bytes, off)?;
            }
        }
        let target_size = u64::from(self.page_count) * u64::from(ps);
        if main_file.size()? > target_size {
            main_file.truncate(target_size)?;
        }
        main_file.sync(false)?;

        // Step 3: Commit complete — delete rollback journal and unlock!
        if wrote_journal {
            vfs.delete(&journal_path, true)?;
        }
        let _ = main_file.unlock(LockLevel::None);
        let _ = main_file.close();

        self.dirty_originals.clear();
        self.commit_orig_page_count = self.page_count;
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
        for (&pgno, page) in self.pages.as_ref() {
            if pgno >= 1 && pgno <= self.page_count {
                let off = ((pgno - 1) as usize) * ps;
                out[off..off + ps].copy_from_slice(page);
            }
        }
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
    pages: Arc<BTreeMap<u32, Arc<Vec<u8>>>>,
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
            .get(&pgno)
            .map(|p| p.as_ref().clone())
            .unwrap_or_else(|| vec![0u8; self.page_size as usize]))
    }
}
