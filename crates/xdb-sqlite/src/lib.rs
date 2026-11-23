//! Pure safe Rust SQLite 3 reader: file format, Stage S1 snapshots (WAL + hot-journal
//! overlay), metered LRU pager, lazy record decoder, table/index B-tree cursors,
//! SQLite `sqlite3InitOne` schema catalog, index key comparison, and logical table/index readers.

#![forbid(unsafe_code)]

pub mod access;
pub mod btree;
pub mod database;
pub mod errors;
pub mod format;
pub mod page_source;
pub mod schema;
pub mod snapshot;
pub mod sql;
pub mod write;

pub use access::{
    key_info_for_index, make_key_info, resolve_collation, KeyFieldSpec, KeyInfo, SearchKey,
};
pub use btree::{
    Btree, BtreeOptions, CellStats, Freelist, IndexCursor, PageStats, Pager, PagerStats,
    PtrmapEntry, RawSchemaRow, RecordKey, RecordView, SeekOp, SeekResult, TableCursor,
    DEFAULT_CACHE_BYTES, MAX_RECORD_HEADER,
};
pub use database::{
    open_database, open_database_from_bytes, open_database_from_image, ColumnSpec,
    IndexEntryCursor, IndexReader, OpenOptions, RowCursor, ScanOptions, SqliteDatabase,
    TableReader,
};
pub use errors::{SqliteError, SqliteFormatErrorCode, SqliteResult};
pub use format::{
    empty_database_info, header_page_size, in_header_size_valid, is_valid_page_size,
    lock_byte_page, payload_limits, read_raw_header, validate_database_header, BtreePage,
    BtreePageType, CellInfo, DatabaseInfo, PayloadLimits, PtrmapType, RawDbHeader, SerialType,
    BTREE_MAX_DEPTH, HEADER_SIZE, MAX_SCHEMA_FORMAT, PENDING_BYTE, SQLITE_HEADER_MAGIC,
};
pub use page_source::{PageSource, SlicePageSource};
pub use schema::{
    ascii_fold, column_default_value, column_text, get_u32, load_schema, real_read_back, same_text,
    value_from_expr, ColumnHiddenKind, ColumnInfo, IndexColumnInfo, IndexInfo, LoadSchemaOptions,
    SchemaCatalog, SchemaEntry, SchemaObject, TableInfo, TriggerInfo, ViewInfo, VirtualTableInfo,
    MAX_SCHEMA_FILE_FORMAT,
};
pub use snapshot::{
    open_snapshot, open_snapshot_from_bytes, open_snapshot_from_image, HotJournalMode, JournalInfo,
    JournalPlayback, JournalSegment, JournalStatus, JournalStopReason, Snapshot, SnapshotMode,
    SnapshotOptions, WalHeader, WalInfo, WalMode, WalRecovery, WalStatus, WalStopReason,
};
pub use write::{
    ConnectionOptions, ExecResult, SeedInsertMode, SeedReport, SqliteConnection, WritePager,
};
pub use xdb_core::SqlValue;
pub use xdb_sql::{QueryOptions, QueryParams, QueryResult};
