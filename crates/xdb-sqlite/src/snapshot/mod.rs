//! Read-only Stage S1 snapshots: main file + in-heap WAL recovery + in-memory hot-journal rollback.

pub mod image;
pub mod journal;
#[allow(clippy::module_inception)]
pub mod snapshot;
pub mod wal;

pub use journal::{
    is_super_journal_name, journal_checksum, playback_journal, read_super_journal, JournalPlayback,
    JournalSegment, JournalStopReason, JOURNAL_MAGIC, MAX_SEGMENT_DETAILS, READER_SECTOR_SIZE,
};
pub use snapshot::{
    open_snapshot, open_snapshot_from_bytes, open_snapshot_from_image, HotJournalMode, JournalInfo,
    JournalStatus, Snapshot, SnapshotMode, SnapshotOptions, WalInfo, WalMode, WalStatus,
    DEFAULT_MAX_PATHNAME, SQLITE_DEFAULT_PAGE_SIZE,
};
pub use wal::{
    recover_wal, wal_checksum_bytes, wal_frame_offset, WalHeader, WalRecovery, WalStopReason,
    WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE, WAL_MAGIC, WAL_MAX_VERSION,
};
