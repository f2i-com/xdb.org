//! Database-header validation (`btree.c lockBtree:3312-3480`).

use xdb_core::TextEncoding;

use super::header::{read_raw_header, RawDbHeader, HEADER_SIZE};
use crate::errors::{SqliteError, SqliteResult};

/// `os.h PENDING_BYTE`: start of the lock range (`0x40000000`, 1 GiB).
pub const PENDING_BYTE: u32 = 0x4000_0000;

/// `btreeInt.h BTCURSOR_MAX_DEPTH`: deepest B-tree SQLite will descend.
pub const BTREE_MAX_DEPTH: usize = 20;

/// `SQLITE_MAX_FILE_FORMAT`.
pub const MAX_SCHEMA_FORMAT: u32 = 4;

/// `PENDING_BYTE_PAGE` (`btreeInt.h:612`): the lock-byte page, never part of database content.
#[inline]
pub const fn lock_byte_page(page_size: u32) -> u32 {
    (PENDING_BYTE / page_size) + 1
}

/// Payload local-storage limits derived from `usable_size` (`btree.c:3459-3462`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadLimits {
    pub max_local: u32,
    pub min_local: u32,
    pub max_leaf: u32,
    pub min_leaf: u32,
}

#[inline]
pub const fn payload_limits(usable_size: u32) -> PayloadLimits {
    let min_local = (((usable_size - 12) * 32) / 255) - 23;
    let max_local = (((usable_size - 12) * 64) / 255) - 23;
    PayloadLimits {
        max_local,
        min_local,
        max_leaf: usable_size - 35,
        min_leaf: min_local,
    }
}

/// Validated metadata derived from page 1 for the B-tree layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub header: Option<RawDbHeader>,
    pub page_size: u32,
    pub usable_size: u32,
    pub page_count: u32,
    pub text_encoding: TextEncoding,
    pub schema_format: u32,
    pub auto_vacuum: bool,
    pub incremental_vacuum: bool,
    pub write_version_too_new: bool,
    pub wal_mode: bool,
    pub first_freelist_trunk: u32,
    pub freelist_count: u32,
    pub lock_byte_page: u32,
    pub max_cells_per_page: u32,
    pub limits: PayloadLimits,
}

/// Page size formula as `lockBtree` computes it: `(b[16] << 8) | (b[17] << 16)`.
#[inline]
pub fn header_page_size(page1: &[u8]) -> u32 {
    (u32::from(page1[16]) << 8) | (u32::from(page1[17]) << 16)
}

/// True for valid SQLite page sizes (powers of two in `512..=65536`).
#[inline]
pub const fn is_valid_page_size(n: u32) -> bool {
    n > 256 && n <= 65536 && (n & (n - 1)) == 0
}

#[inline]
const fn max_cells(page_size: u32) -> u32 {
    (page_size - 8) / 6
}

/// [`DatabaseInfo`] for an empty (0-page) database.
pub fn empty_database_info(page_size: u32) -> DatabaseInfo {
    let ps = if is_valid_page_size(page_size) {
        page_size
    } else {
        4096
    };
    DatabaseInfo {
        header: None,
        page_size: ps,
        usable_size: ps,
        page_count: 0,
        text_encoding: TextEncoding::Utf8,
        schema_format: 0,
        auto_vacuum: false,
        incremental_vacuum: false,
        write_version_too_new: false,
        wal_mode: false,
        first_freelist_trunk: 0,
        freelist_count: 0,
        lock_byte_page: lock_byte_page(ps),
        max_cells_per_page: max_cells(ps),
        limits: payload_limits(ps),
    }
}

/// Validate page 1 of a non-empty database in `lockBtree`'s exact check order.
pub fn validate_database_header(
    page1: &[u8],
    page_count: u32,
    source_page_size: u32,
    file_page_count: Option<u32>,
) -> SqliteResult<DatabaseInfo> {
    if page1.len() < HEADER_SIZE {
        return Err(SqliteError::not_a_db(format!(
            "page 1 is only {} bytes",
            page1.len()
        )));
    }
    let h = read_raw_header(page1).expect("len >= 100");
    if !h.magic_ok {
        return Err(SqliteError::not_a_db("bad magic string"));
    }
    if h.read_version > 2 {
        return Err(SqliteError::not_a_db(format!(
            "read version {} is newer than 2",
            h.read_version
        )));
    }
    if h.max_payload_fraction != 64 || h.min_payload_fraction != 32 || h.leaf_payload_fraction != 32
    {
        return Err(SqliteError::not_a_db(format!(
            "payload fractions {}/{}/{}, expected 64/32/32",
            h.max_payload_fraction, h.min_payload_fraction, h.leaf_payload_fraction
        )));
    }
    let page_size = header_page_size(page1);
    if !is_valid_page_size(page_size) {
        return Err(SqliteError::not_a_db(format!(
            "invalid page size {page_size}"
        )));
    }
    if page_size != source_page_size {
        return Err(SqliteError::corrupt(format!(
            "header page size {page_size} differs from the page source's {source_page_size}"
        )));
    }
    if let Some(fpc) = file_page_count {
        if page_count > fpc {
            return Err(SqliteError::corrupt(format!(
                "database size {page_count} pages exceeds the file's {fpc} pages"
            )));
        }
    }
    let usable_size = page_size.saturating_sub(u32::from(h.reserved_bytes));
    if usable_size < 480 {
        return Err(SqliteError::not_a_db(format!(
            "usable size {usable_size} is below 480"
        )));
    }
    Ok(DatabaseInfo {
        header: Some(h),
        page_size,
        usable_size,
        page_count,
        text_encoding: TextEncoding::from_header_field(h.text_encoding),
        schema_format: h.schema_format,
        auto_vacuum: h.largest_root_page != 0,
        incremental_vacuum: h.incremental_vacuum != 0,
        write_version_too_new: h.write_version > 2,
        wal_mode: h.read_version == 2,
        first_freelist_trunk: h.first_freelist_trunk,
        freelist_count: h.freelist_count,
        lock_byte_page: lock_byte_page(page_size),
        max_cells_per_page: max_cells(page_size),
        limits: payload_limits(usable_size),
    })
}
