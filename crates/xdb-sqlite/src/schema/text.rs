//! Text helpers for the SQLite schema loader (`sqlite3UpperToLower`,
//! `sqlite3_column_text`, `sqlite3GetUInt32`).

use xdb_core::value::utf::decode_stored_text_to_utf8;
use xdb_core::value::number_to_text;
use xdb_core::{SqlValue, TextEncoding};

/// ASCII-only lower case (`sqlite3UpperToLower`): the key of SQLite's case-insensitive name hashes.
#[inline]
pub fn ascii_fold(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// `sqlite3_stricmp(a, b) == 0`, including its NULL rules (`None` equals only `None`).
#[inline]
pub fn same_text(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// The value `sqlite3_exec` hands its callback for one column (`sqlite3_column_text`):
/// `NULL` stays `None`; `INTEGER`/`REAL` render as text; `BLOB` is reinterpreted as text
/// in database encoding `enc`; `TEXT` as is; truncated at the first NUL byte.
pub fn column_text(v: &SqlValue, enc: TextEncoding) -> Option<String> {
    let raw_utf8: Vec<u8> = match v {
        SqlValue::Null => return None,
        SqlValue::Int(_) | SqlValue::Real(_) => number_to_text(v)?.into_bytes(),
        SqlValue::Text(b) => b.to_vec(),
        SqlValue::Blob(b) => decode_stored_text_to_utf8(b, enc),
    };
    let end = raw_utf8
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(raw_utf8.len());
    Some(String::from_utf8_lossy(&raw_utf8[..end]).into_owned())
}

/// `sqlite3GetUInt32` (`util.c:1545`): ASCII decimal digits only, whole string, at most `u32::MAX`.
pub fn get_u32(z: &str) -> Option<u32> {
    let b = z.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + u64::from(c - b'0');
        if v > u64::from(u32::MAX) {
            return None;
        }
    }
    Some(v as u32)
}
