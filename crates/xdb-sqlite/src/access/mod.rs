//! Index key comparison (`KeyInfo`, `SearchKey` / `sqlite3VdbeRecordCompareWithSkip`).

pub mod key;

pub use key::{key_info_for_index, make_key_info, resolve_collation, KeyFieldSpec, KeyInfo, SearchKey};
