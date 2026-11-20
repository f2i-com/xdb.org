//! Stage S2 writable SQLite 3 engine: binary record encoder, ACID rollback-journal pager,
//! balanced SQLite 3 B-tree writer, and full read-write [`SqliteConnection`].

pub mod btree;
pub mod connection;
pub mod pager;
pub mod record;
pub mod seed;

pub use btree::{
    check_integrity, create_btree, drop_btree, index_delete, index_insert, table_delete,
    table_insert, table_max_rowid,
};
pub use connection::{ConnectionOptions, ExecResult, SqliteConnection};
pub use pager::WritePager;
pub use record::{encode_record, put_varint, varint_size};
pub use seed::{SeedInsertMode, SeedReport};
