//! `CAST(x AS type)`, ported from SQLite 3.54.1 `sqlite3VdbeMemCast` (`vdbemem.c:926`).
//!
//! Because `SqlValue::Text` stores raw bytes in Rust, `CAST(x'ff' AS TEXT)` and
//! `CAST(CAST(x'ff' AS TEXT) AS BLOB)` preserve invalid UTF-8 bytes verbatim, matching
//! SQLite 100% (eliminating the JS UTF-16 `U+FFFD` loss).

use super::affinity::{affinity_type, number_to_text};
use super::numeric::{int_value, numerify, real_value};
use super::utf::{decode_stored_text_to_utf8, encode_utf8_to_stored_text, TextEncoding};
use super::{Affinity, SqlValue};

/// `CAST(text AS BLOB)` in database encoding `enc`.
pub fn text_to_blob(text: &[u8], enc: TextEncoding) -> Box<[u8]> {
    encode_utf8_to_stored_text(text, enc).into_boxed_slice()
}

/// `CAST(blob AS TEXT)` in database encoding `enc`:
/// - For `Utf8`, raw bytes are kept verbatim (invalid UTF-8 survives round-trips).
/// - For `Utf16Le` / `Utf16Be`, trailing odd byte is dropped and bytes are translated via
///   `sqlite3VdbeMemTranslate`.
pub fn blob_to_text(blob: &[u8], enc: TextEncoding) -> Box<[u8]> {
    decode_stored_text_to_utf8(blob, enc).into_boxed_slice()
}

/// `sqlite3VdbeMemCast`: `CAST(v AS <affinity>)`.
/// Returns `Err` only if `aff == Affinity::Flexnum` (which no `CAST` target produces).
pub fn cast_value(v0: SqlValue, aff: Affinity, enc: TextEncoding) -> Result<SqlValue, &'static str> {
    if v0.is_null() {
        return Ok(SqlValue::Null);
    }
    // Numeric targets read a UTF-16 database's BLOB as UTF-16 text (sqlite3AtoF with enc).
    let v = match (&v0, enc, aff) {
        (
            SqlValue::Blob(b),
            TextEncoding::Utf16Le | TextEncoding::Utf16Be,
            Affinity::Numeric | Affinity::Integer | Affinity::Real,
        ) => SqlValue::Text(blob_to_text(b, enc)),
        _ => v0,
    };

    match aff {
        Affinity::None | Affinity::Blob => match v {
            SqlValue::Blob(_) => Ok(v),
            SqlValue::Text(b) => Ok(SqlValue::Blob(text_to_blob(&b, enc))),
            other => {
                let s = number_to_text(&other).expect("numeric");
                Ok(SqlValue::Blob(text_to_blob(s.as_bytes(), enc)))
            }
        },
        Affinity::Numeric => Ok(numerify(&v)),
        Affinity::Integer => Ok(SqlValue::Int(int_value(&v))),
        Affinity::Real => Ok(SqlValue::Real(real_value(&v))),
        Affinity::Text => match v {
            SqlValue::Text(_) => Ok(v),
            SqlValue::Blob(b) => Ok(SqlValue::Text(blob_to_text(&b, enc))),
            other => Ok(SqlValue::from(number_to_text(&other).expect("numeric"))),
        },
        Affinity::Flexnum => Err("CAST to FLEXNUM affinity is not supported"),
    }
}

/// `CAST(v AS type_name)`.
pub fn cast_to_type(v: SqlValue, type_name: &[u8], enc: TextEncoding) -> SqlValue {
    cast_value(v, affinity_type(type_name), enc).expect("affinity_type never returns FLEXNUM")
}
