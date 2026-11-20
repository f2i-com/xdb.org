//! Read-only B-tree engine: metered LRU pager, lazy record decoder, cursors, and `Btree`.

#[allow(clippy::module_inception)]
pub mod btree;
pub mod cursor;
pub mod pager;
pub mod record_view;

pub use btree::{Btree, BtreeOptions, CellStats, Freelist, PageStats, PtrmapEntry, RawSchemaRow};
pub use cursor::{IndexCursor, RecordKey, SeekOp, SeekResult, TableCursor};
pub use pager::{Pager, PagerStats, DEFAULT_CACHE_BYTES};
pub use record_view::{RecordView, MAX_RECORD_HEADER};
