//! Raw field extraction for the 100-byte SQLite 3 database header (`fileformat2 §1.3`).

pub const HEADER_SIZE: usize = 100;

/// `"SQLite format 3\0"`
pub const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Unvalidated fields extracted from bytes `0..100` of page 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawDbHeader {
    pub magic_ok: bool,
    pub page_size_raw: u16,
    /// Decoded page size (raw value `1` -> `65536`); not yet validated.
    pub page_size: u32,
    pub write_version: u8,
    pub read_version: u8,
    pub reserved_bytes: u8,
    pub usable_size: i32,
    pub max_payload_fraction: u8,
    pub min_payload_fraction: u8,
    pub leaf_payload_fraction: u8,
    pub file_change_counter: u32,
    pub page_count: u32,
    pub first_freelist_trunk: u32,
    pub freelist_count: u32,
    pub schema_cookie: u32,
    pub schema_format: u32,
    pub default_cache_size: i32,
    pub largest_root_page: u32,
    pub text_encoding: u32,
    pub user_version: i32,
    pub incremental_vacuum: u32,
    pub application_id: i32,
    pub version_valid_for: u32,
    pub sqlite_version_number: u32,
}

#[inline]
fn u16_be(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32_be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// True when `b` starts with `"SQLite format 3\0"`.
#[inline]
pub fn has_sqlite_magic(b: &[u8]) -> bool {
    b.len() >= SQLITE_HEADER_MAGIC.len() && &b[..16] == SQLITE_HEADER_MAGIC
}

/// Extract every header field from the first 100 bytes of `b`.
/// Returns `None` if `b.len() < 100`.
pub fn read_raw_header(b: &[u8]) -> Option<RawDbHeader> {
    if b.len() < HEADER_SIZE {
        return None;
    }
    let page_size_raw = u16_be(b, 16);
    let page_size = if page_size_raw == 1 {
        65536
    } else {
        u32::from(page_size_raw)
    };
    let reserved_bytes = b[20];
    Some(RawDbHeader {
        magic_ok: has_sqlite_magic(b),
        page_size_raw,
        page_size,
        write_version: b[18],
        read_version: b[19],
        reserved_bytes,
        usable_size: (page_size as i32) - i32::from(reserved_bytes),
        max_payload_fraction: b[21],
        min_payload_fraction: b[22],
        leaf_payload_fraction: b[23],
        file_change_counter: u32_be(b, 24),
        page_count: u32_be(b, 28),
        first_freelist_trunk: u32_be(b, 32),
        freelist_count: u32_be(b, 36),
        schema_cookie: u32_be(b, 40),
        schema_format: u32_be(b, 44),
        default_cache_size: u32_be(b, 48) as i32,
        largest_root_page: u32_be(b, 52),
        text_encoding: u32_be(b, 56),
        user_version: u32_be(b, 60) as i32,
        incremental_vacuum: u32_be(b, 64),
        application_id: u32_be(b, 68) as i32,
        version_valid_for: u32_be(b, 92),
        sqlite_version_number: u32_be(b, 96),
    })
}

/// True when the in-header database size (offset 28) is valid (`page_count != 0` and
/// `file_change_counter == version_valid_for`).
#[inline]
pub fn in_header_size_valid(h: &RawDbHeader) -> bool {
    h.page_count != 0 && h.file_change_counter == h.version_valid_for
}
