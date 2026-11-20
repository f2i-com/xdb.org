//! Low-level SQLite 3 binary format decoders.

pub mod db_header;
pub mod header;
pub mod page;
pub mod ptrmap;
pub mod record;
pub mod varint;

pub use db_header::{
    empty_database_info, header_page_size, is_valid_page_size, lock_byte_page, payload_limits,
    validate_database_header, DatabaseInfo, PayloadLimits, BTREE_MAX_DEPTH, MAX_SCHEMA_FORMAT,
    PENDING_BYTE,
};
pub use header::{
    has_sqlite_magic, in_header_size_valid, read_raw_header, RawDbHeader, HEADER_SIZE,
    SQLITE_HEADER_MAGIC,
};
pub use page::{
    local_payload_size, overflow_page_count, read_u32_be, BtreePage, BtreePageType, CellInfo,
};
pub use ptrmap::{is_ptrmap_page, ptrmap_entry_offset, ptrmap_page_for, PtrmapType};
pub use record::{
    decode_fixed, decode_serial_value, serial_type_length, serial_type_storage, SerialType,
};
pub use varint::{
    get_payload_size_varint, get_varint, get_varint32, put_varint, varint_len, varint_len_at,
    VARINT_MAX_LENGTH,
};
