//! Stage S1 read-only database snapshots (main file + WAL overlay + hot-journal rollback).

use std::collections::HashMap;

use xdb_core::{
    FileKind, IoError, IoErrorCode, LockLevel, MemoryVfs, OpenFlags, Vfs, VfsFile,
};

use super::image::{ByteImage, FileImage, JournalImage, MemImage};
use super::journal::{
    playback_journal, read_super_journal, JournalPlayback, JournalSegment, JournalStopReason,
};
use super::wal::{
    recover_wal, wal_frame_offset, WalHeader, WalRecovery, WalStopReason, WAL_FRAME_HEADER_SIZE,
};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    header_page_size, is_valid_page_size, read_raw_header, RawDbHeader, HEADER_SIZE,
};
use crate::page_source::PageSource;

/// `SQLITE_DEFAULT_PAGE_SIZE`.
pub const SQLITE_DEFAULT_PAGE_SIZE: u32 = 4096;
/// `os_unix.c MAX_PATHNAME` (win32 SQLite uses 1040).
pub const DEFAULT_MAX_PATHNAME: usize = 512;

/// How a snapshot's pages are produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotMode {
    Empty,
    Rollback,
    Wal,
    HotJournalOverlay,
}

/// How the snapshot handled a `-wal` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalStatus {
    Applied,
    Absent,
    IgnoredByOption,
    IgnoredEmptyDatabase,
    IgnoredRollbackMode,
}

/// How the snapshot handled a `-journal` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalStatus {
    HotApplied,
    HotNoValidHeader,
    NotHotZeroHeader,
    NotHotReservedLock,
    NotHotEmptyDatabase,
    NotHotSuperJournalMissing,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WalMode {
    #[default]
    Apply,
    Ignore,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HotJournalMode {
    #[default]
    Overlay,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOptions {
    pub wal: WalMode,
    pub hot_journal: HotJournalMode,
    pub max_pathname: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            wal: WalMode::Apply,
            hot_journal: HotJournalMode::Overlay,
            max_pathname: DEFAULT_MAX_PATHNAME,
        }
    }
}

/// Diagnostic metadata about the `-wal` file.
#[derive(Debug, Clone)]
pub struct WalInfo {
    pub path: String,
    pub status: WalStatus,
    pub file_size: u64,
    pub header: Option<WalHeader>,
    pub header_valid: bool,
    pub frame_slots: u32,
    pub valid_frames: u32,
    pub mx_frame: u32,
    pub commits: u32,
    pub n_page: u32,
    pub uncommitted_frames: u32,
    pub invalid_frames: u32,
    pub stale_salt_frames: u32,
    pub stop_reason: WalStopReason,
    pub pages_in_wal: usize,
}

impl WalInfo {
    fn empty(path: String, status: WalStatus, file_size: u64) -> Self {
        Self {
            path,
            status,
            file_size,
            header: None,
            header_valid: false,
            frame_slots: 0,
            valid_frames: 0,
            mx_frame: 0,
            commits: 0,
            n_page: 0,
            uncommitted_frames: 0,
            invalid_frames: 0,
            stale_salt_frames: 0,
            stop_reason: WalStopReason::NoValidHeader,
            pages_in_wal: 0,
        }
    }

    fn from_recovery(path: String, status: WalStatus, r: &WalRecovery) -> Self {
        Self {
            path,
            status,
            file_size: r.file_size,
            header: r.header,
            header_valid: r.header_valid,
            frame_slots: r.frame_slots,
            valid_frames: r.valid_frames,
            mx_frame: r.mx_frame,
            commits: r.commits,
            n_page: r.n_page,
            uncommitted_frames: r.uncommitted_frames,
            invalid_frames: r.invalid_frames,
            stale_salt_frames: r.stale_salt_frames,
            stop_reason: r.stop_reason,
            pages_in_wal: r.pages_in_wal,
        }
    }
}

/// Diagnostic metadata about the `-journal` file.
#[derive(Debug, Clone)]
pub struct JournalInfo {
    pub path: String,
    pub status: JournalStatus,
    pub file_size: u64,
    pub super_journal: Option<String>,
    pub played: bool,
    pub page_size: u32,
    pub sector_size: u32,
    pub original_page_count: u32,
    pub segments: Vec<JournalSegment>,
    pub segment_count: usize,
    pub records_played: u32,
    pub records_applied: u32,
    pub records_skipped: u32,
    pub stop_reason: JournalStopReason,
}

impl JournalInfo {
    fn not_hot(
        path: String,
        status: JournalStatus,
        file_size: u64,
        super_journal: Option<String>,
    ) -> Self {
        Self {
            path,
            status,
            file_size,
            super_journal,
            played: false,
            page_size: 0,
            sector_size: 0,
            original_page_count: 0,
            segments: Vec::new(),
            segment_count: 0,
            records_played: 0,
            records_applied: 0,
            records_skipped: 0,
            stop_reason: JournalStopReason::NoMoreHeaders,
        }
    }
}

/// A read-only, point-in-time SQLite database snapshot implementing [`PageSource`].
#[derive(Debug)]
pub struct Snapshot {
    pub path: String,
    pub mode: SnapshotMode,
    pub page_size: u32,
    pub page_count: u32,
    pub header: Option<RawDbHeader>,
    pub main_file_size: u64,
    pub wal: Option<WalInfo>,
    pub journal: Option<JournalInfo>,
    image: Box<dyn ByteImage>,
    wal_file: Option<Box<dyn VfsFile>>,
    frame_of: Option<HashMap<u32, u32>>,
    wal_page_size: u32,
    closed: bool,
}

impl Snapshot {
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn close(&mut self) -> SqliteResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let r1 = if let Some(ref mut wf) = self.wal_file {
            wf.close()
        } else {
            Ok(())
        };
        self.wal_file = None;
        self.frame_of = None;
        let r2 = self.image.close();
        r1.and(r2)?;
        Ok(())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl PageSource for Snapshot {
    fn page_size(&self) -> u32 {
        self.page_size
    }

    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn read_page(&mut self, pgno: u32) -> SqliteResult<Vec<u8>> {
        if self.closed {
            return Err(IoError::with_path(
                IoErrorCode::Misuse,
                format!("snapshot is closed: {}", self.path),
                &self.path,
            )
            .into());
        }
        if pgno < 1 || pgno > self.page_count {
            return Err(SqliteError::corrupt(format!(
                "page {pgno} is outside the database (1..={})",
                self.page_count
            )));
        }
        let ps = self.page_size as usize;
        let mut buf = vec![0u8; ps];
        if let (Some(frame_of), Some(wal_file)) = (&self.frame_of, &mut self.wal_file) {
            if let Some(&frame) = frame_of.get(&pgno) {
                let wps = self.wal_page_size as usize;
                let off = wal_frame_offset(frame, self.wal_page_size) + WAL_FRAME_HEADER_SIZE;
                let dst = if wps >= ps { &mut buf[..] } else { &mut buf[..wps] };
                let _ = wal_file.read(dst, off)?;
                return Ok(buf);
            }
        }
        self.image.read(&mut buf, (pgno as u64 - 1) * (ps as u64))?;
        Ok(buf)
    }

    fn close(&mut self) -> SqliteResult<()> {
        Snapshot::close(self)
    }
}

/// Build a [`Snapshot`] directly from the raw bytes of a single main database file
/// (no `-wal`, no `-journal`), matching `sqlite3_deserialize` / `openDatabaseFromImage`.
/// Returns `(snapshot, file_page_count)`.
pub fn open_snapshot_from_image(bytes: Vec<u8>) -> (Snapshot, u32) {
    let mut page_size = SQLITE_DEFAULT_PAGE_SIZE;
    if bytes.len() >= 18 {
        let ps = header_page_size(&bytes);
        if is_valid_page_size(ps) {
            page_size = ps;
        }
    }
    let file_pages = u32::try_from(bytes.len().div_ceil(page_size as usize)).unwrap_or(u32::MAX);
    let mut page_count = file_pages;
    if bytes.len() >= HEADER_SIZE {
        let n_page = u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
        if n_page != 0 && bytes[24..28] == bytes[92..96] {
            page_count = n_page;
        }
    }
    let empty = bytes.is_empty();
    let header = if empty {
        None
    } else if bytes.len() >= HEADER_SIZE {
        read_raw_header(&bytes)
    } else {
        let mut padded = [0u8; HEADER_SIZE];
        padded[..bytes.len()].copy_from_slice(&bytes);
        read_raw_header(&padded)
    };
    let mode = if empty {
        SnapshotMode::Empty
    } else if bytes.get(19).copied() == Some(2) {
        SnapshotMode::Wal
    } else {
        SnapshotMode::Rollback
    };
    let main_file_size = bytes.len() as u64;
    let snapshot = Snapshot {
        path: ":image:".to_string(),
        mode,
        page_size,
        page_count: if empty { 0 } else { page_count },
        header,
        main_file_size,
        wal: None,
        journal: None,
        image: Box::new(MemImage::new(bytes)),
        wal_file: None,
        frame_of: None,
        wal_page_size: 0,
        closed: false,
    };
    (snapshot, file_pages)
}

fn open_if_present(
    vfs: &dyn Vfs,
    path: &str,
    kind: FileKind,
) -> SqliteResult<Option<(Box<dyn VfsFile>, u64)>> {
    if !vfs.exists(path)? {
        return Ok(None);
    }
    let mut file = vfs.open(path, OpenFlags::readonly(kind))?;
    let size = match file.size() {
        Ok(s) => s,
        Err(e) => {
            let _ = file.close();
            return Err(e.into());
        }
    };
    if size == 0 {
        let _ = file.close();
        return Ok(None);
    }
    Ok(Some((file, size)))
}

fn access_exists(vfs: &dyn Vfs, path: &str) -> SqliteResult<bool> {
    if !vfs.exists(path)? {
        return Ok(false);
    }
    let mut file = match vfs.open(path, OpenFlags::readonly(FileKind::SuperJournal)) {
        Ok(f) => f,
        Err(_) => return Ok(true),
    };
    let res = file.size().map(|s| s > 0);
    let _ = file.close();
    Ok(res?)
}

/// Open a Stage S1 read-only snapshot from `vfs` at `path`.
pub fn open_snapshot(
    vfs: &dyn Vfs,
    path: &str,
    options: SnapshotOptions,
) -> SqliteResult<Snapshot> {
    let mut main = vfs.open(path, OpenFlags::readonly(FileKind::MainDb))?;
    if !main.lock(LockLevel::Shared)? {
        let _ = main.close();
        return Err(IoError::with_path(
            IoErrorCode::Busy,
            format!("database is locked: {path}"),
            path,
        )
        .into());
    }

    let main_size = match main.size() {
        Ok(s) => s,
        Err(e) => {
            let _ = main.unlock(LockLevel::None);
            let _ = main.close();
            return Err(e.into());
        }
    };
    let mut main_head = [0u8; HEADER_SIZE];
    if let Err(e) = main.read(&mut main_head, 0) {
        let _ = main.unlock(LockLevel::None);
        let _ = main.close();
        return Err(e.into());
    }
    let main_header_ps = header_page_size(&main_head);
    let pager_page_size = if is_valid_page_size(main_header_ps) {
        main_header_ps
    } else {
        SQLITE_DEFAULT_PAGE_SIZE
    };
    let main_has_reserved = match main.check_reserved_lock() {
        Ok(v) => v,
        Err(e) => {
            let _ = main.unlock(LockLevel::None);
            let _ = main.close();
            return Err(e.into());
        }
    };

    let mut image: Box<dyn ByteImage> = Box::new(FileImage::new(main, main_size));

    // ---- 1. Hot rollback journal ----
    let journal_path = format!("{path}-journal");
    let mut journal_info: Option<JournalInfo> = None;
    if let Some((mut jfile, jsize)) = open_if_present(vfs, &journal_path, FileKind::MainJournal)? {
        let (info, playback) = match examine_journal(
            vfs,
            path,
            main_has_reserved,
            main_size,
            jfile.as_mut(),
            jsize,
            &journal_path,
            pager_page_size,
            options.hot_journal,
            options.max_pathname,
        ) {
            Ok(res) => res,
            Err(e) => {
                let _ = jfile.close();
                let _ = image.close();
                return Err(e);
            }
        };
        journal_info = Some(info);
        if let Some(pb) = playback {
            if pb.played {
                image = Box::new(JournalImage::new(image, jfile, &pb));
            } else {
                let _ = jfile.close();
            }
        } else {
            let _ = jfile.close();
        }
    }

    // ---- 2. WAL ----
    let wal_path = format!("{path}-wal");
    let mut wal_info: Option<WalInfo> = None;
    let mut wal_file: Option<Box<dyn VfsFile>> = None;
    let mut wal_recovery: Option<WalRecovery> = None;

    if options.wal == WalMode::Ignore {
        if let Some((mut wf, wsize)) = open_if_present(vfs, &wal_path, FileKind::Wal)? {
            let _ = wf.close();
            wal_info = Some(WalInfo::empty(
                wal_path.clone(),
                WalStatus::IgnoredByOption,
                wsize,
            ));
        }
    } else if let Some((mut wf, wsize)) = open_if_present(vfs, &wal_path, FileKind::Wal)? {
        // SQLite `pagerOpenWalIfPresent`: ONLY apply `-wal` when the main database is non-empty,
        // no hot rollback journal was applied, AND page 1's header indicates WAL format (version 2).
        let main_is_wal = if image.size() >= HEADER_SIZE as u64
            && !journal_info
                .as_ref()
                .is_some_and(|j| j.status == JournalStatus::HotApplied)
        {
            let mut probe = [0u8; HEADER_SIZE];
            match image.read(&mut probe, 0) {
                Ok(()) => probe[18] == 2 || probe[19] == 2,
                Err(_) => false,
            }
        } else {
            false
        };
        if image.size() == 0 {
            let _ = wf.close();
            wal_info = Some(WalInfo::empty(
                wal_path.clone(),
                WalStatus::IgnoredEmptyDatabase,
                wsize,
            ));
        } else if !main_is_wal {
            let _ = wf.close();
            wal_info = Some(WalInfo::empty(
                wal_path.clone(),
                WalStatus::IgnoredRollbackMode,
                wsize,
            ));
        } else {
            let rec = match recover_wal(wf.as_mut(), wsize) {
                Ok(r) => r,
                Err(e) => {
                    let _ = wf.close();
                    let _ = image.close();
                    return Err(e.into());
                }
            };
            wal_info = Some(WalInfo::from_recovery(
                wal_path.clone(),
                WalStatus::Applied,
                &rec,
            ));
            if rec.mx_frame > 0 {
                wal_file = Some(wf);
            } else {
                let _ = wf.close();
            }
            wal_recovery = Some(rec);
        }
    }

    // ---- 3. lockBtree page-1 validation ----
    let mut page1 = [0u8; HEADER_SIZE];
    let p1_frame = wal_recovery
        .as_ref()
        .and_then(|r| r.frame_of.get(&1).copied());
    let read_p1_res = if let (Some(frame), Some(ref mut wf), Some(ref rec)) =
        (p1_frame, &mut wal_file, &wal_recovery)
    {
        let wps = rec.header.expect("valid wal header").page_size;
        wf.read(&mut page1, wal_frame_offset(frame, wps) + WAL_FRAME_HEADER_SIZE)
            .map(|_| ())
    } else {
        image.read(&mut page1, 0)
    };
    if let Err(e) = read_p1_res {
        if let Some(ref mut wf) = wal_file {
            let _ = wf.close();
        }
        let _ = image.close();
        return Err(e.into());
    }

    let header = read_raw_header(&page1).expect("100 bytes");
    let wal_n_page = wal_recovery.as_ref().map_or(0, |r| r.n_page);
    let size_valid = header.page_count != 0 && page1[24..28] == page1[92..96];

    let (page_size, page_count) = if !size_valid && wal_n_page == 0 && image.size() == 0 {
        (pager_page_size, 0u32)
    } else {
        let checked = match check_page1(&page1, &header, size_valid, wal_n_page, image.size()) {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref mut wf) = wal_file {
                    let _ = wf.close();
                }
                let _ = image.close();
                return Err(e);
            }
        };
        if let Some(ref rec) = wal_recovery {
            if let Some(whdr) = rec.header {
                if rec.mx_frame > 0 && whdr.page_size != checked.0 {
                    if let Some(ref mut wf) = wal_file {
                        let _ = wf.close();
                    }
                    let _ = image.close();
                    return Err(SqliteError::corrupt(format!(
                        "WAL page size {} does not match database page size {}",
                        whdr.page_size, checked.0
                    )));
                }
            }
        }
        if options.wal == WalMode::Apply && wal_info.is_none() && page1[19] == 2 {
            wal_info = Some(WalInfo::empty(wal_path, WalStatus::Absent, 0));
        }
        checked
    };

    let mode = if page_count == 0 {
        SnapshotMode::Empty
    } else if journal_info
        .as_ref()
        .is_some_and(|j| j.status == JournalStatus::HotApplied)
    {
        SnapshotMode::HotJournalOverlay
    } else if wal_info
        .as_ref()
        .is_some_and(|w| matches!(w.status, WalStatus::Applied | WalStatus::Absent))
    {
        SnapshotMode::Wal
    } else {
        SnapshotMode::Rollback
    };

    let (frame_of, wal_page_size) = match wal_recovery {
        Some(rec) if rec.mx_frame > 0 => (
            Some(rec.frame_of),
            rec.header.map_or(0, |h| h.page_size),
        ),
        _ => (None, 0),
    };

    Ok(Snapshot {
        path: path.to_string(),
        mode,
        page_size,
        page_count,
        header: if page_count == 0 { None } else { Some(header) },
        main_file_size: main_size,
        wal: wal_info,
        journal: journal_info,
        image,
        wal_file,
        frame_of,
        wal_page_size,
        closed: false,
    })
}

/// Convenience helper: open a read-only snapshot directly from in-memory bytes
/// (with optional `-wal` and `-journal` byte buffers).
pub fn open_snapshot_from_bytes(
    main_bytes: Vec<u8>,
    wal_bytes: Option<Vec<u8>>,
    journal_bytes: Option<Vec<u8>>,
    options: SnapshotOptions,
) -> SqliteResult<Snapshot> {
    let vfs = MemoryVfs::default();
    vfs.set_file("mem.db", main_bytes)?;
    if let Some(wal) = wal_bytes {
        vfs.set_file("mem.db-wal", wal)?;
    }
    if let Some(journal) = journal_bytes {
        vfs.set_file("mem.db-journal", journal)?;
    }
    open_snapshot(&vfs, "mem.db", options)
}

fn check_page1(
    page1: &[u8; HEADER_SIZE],
    header: &RawDbHeader,
    size_valid: bool,
    wal_n_page: u32,
    file_size: u64,
) -> SqliteResult<(u32, u32)> {
    if !header.magic_ok {
        return Err(SqliteError::not_a_db(
            "bad header magic (not \"SQLite format 3\")",
        ));
    }
    if page1[19] > 2 {
        return Err(SqliteError::not_a_db(format!(
            "unsupported read format version {} (header byte 19)",
            page1[19]
        )));
    }
    if page1[21] != 64 || page1[22] != 32 || page1[23] != 32 {
        return Err(SqliteError::not_a_db(format!(
            "payload fractions {}/{}/{} are not 64/32/32",
            page1[21], page1[22], page1[23]
        )));
    }
    let page_size = header_page_size(page1);
    if !is_valid_page_size(page_size) {
        return Err(SqliteError::not_a_db(format!(
            "invalid page size {page_size} (header bytes 16..17 = {})",
            header.page_size_raw
        )));
    }
    let n_page_file = if wal_n_page != 0 {
        wal_n_page
    } else {
        u32::try_from(file_size.div_ceil(u64::from(page_size))).unwrap_or(u32::MAX)
    };
    let n_page = if size_valid {
        header.page_count
    } else {
        n_page_file
    };
    if n_page > n_page_file {
        return Err(SqliteError::corrupt(format!(
            "the header says {n_page} pages but the {} holds {n_page_file}",
            if wal_n_page != 0 {
                "last WAL commit"
            } else {
                "file"
            }
        )));
    }
    let usable = page_size.saturating_sub(u32::from(page1[20]));
    if usable < 480 {
        return Err(SqliteError::not_a_db(format!(
            "usable page size {usable} is below 480 (page size {page_size}, reserved {})",
            page1[20]
        )));
    }
    Ok((page_size, n_page))
}

#[allow(clippy::too_many_arguments)]
fn examine_journal(
    vfs: &dyn Vfs,
    main_path: &str,
    main_has_reserved: bool,
    main_size: u64,
    jfile: &mut dyn VfsFile,
    jsize: u64,
    jpath: &str,
    pager_page_size: u32,
    hot_option: HotJournalMode,
    max_pathname: usize,
) -> SqliteResult<(JournalInfo, Option<JournalPlayback>)> {
    if main_has_reserved {
        return Ok((
            JournalInfo::not_hot(
                jpath.to_string(),
                JournalStatus::NotHotReservedLock,
                jsize,
                None,
            ),
            None,
        ));
    }
    if main_size == 0 {
        return Ok((
            JournalInfo::not_hot(
                jpath.to_string(),
                JournalStatus::NotHotEmptyDatabase,
                jsize,
                None,
            ),
            None,
        ));
    }
    let mut first = [0u8; 1];
    let _ = jfile.read(&mut first, 0)?;
    if first[0] == 0 {
        return Ok((
            JournalInfo::not_hot(
                jpath.to_string(),
                JournalStatus::NotHotZeroHeader,
                jsize,
                None,
            ),
            None,
        ));
    }
    let super_journal = read_super_journal(jfile, jsize, max_pathname)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    if let Some(ref sj) = super_journal {
        if !access_exists(vfs, sj)? {
            return Ok((
                JournalInfo::not_hot(
                    jpath.to_string(),
                    JournalStatus::NotHotSuperJournalMissing,
                    jsize,
                    super_journal,
                ),
                None,
            ));
        }
    }
    if hot_option == HotJournalMode::Refuse {
        return Err(IoError::with_path(
            IoErrorCode::ReadOnly,
            format!(
                "{main_path}: hot journal {jpath} must be rolled back before the database can be read (SQLITE_READONLY_ROLLBACK)"
            ),
            main_path,
        )
        .into());
    }
    let pb = playback_journal(jfile, jsize, pager_page_size)?;
    let status = if pb.played {
        JournalStatus::HotApplied
    } else {
        JournalStatus::HotNoValidHeader
    };
    let info = JournalInfo {
        path: jpath.to_string(),
        status,
        file_size: jsize,
        super_journal,
        played: pb.played,
        page_size: pb.page_size,
        sector_size: pb.sector_size,
        original_page_count: pb.original_page_count,
        segments: pb.segments.clone(),
        segment_count: pb.segment_count,
        records_played: pb.records_played,
        records_applied: pb.records_applied,
        records_skipped: pb.records_skipped,
        stop_reason: pb.stop_reason,
    };
    Ok((info, Some(pb)))
}
