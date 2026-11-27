//! SQLite 3.54.1-exact scalar value representation and semantics.
//!
//! Unlike the TypeScript prototype, Rust represents integers natively as `i64`,
//! floats natively as `f64`, and `TEXT` as raw bytes (`Box<[u8]>`). That removes
//! both the JS `number`/`bigint`/`SqlReal` split and UTF-16 `U+FFFD` loss on
//! non-UTF-8 `TEXT` values.

pub mod affinity;
pub mod arith;
pub mod atof;
pub mod cast;
pub mod collation;
pub mod compare;
pub mod fp_core;
pub mod fp_format;
pub mod int64;
pub mod numeric;
pub mod utf;

use std::fmt;

pub use affinity::{
    affinity_name, affinity_of_decl_type, affinity_type, apply_affinity, apply_affinity_raw,
    apply_comparison_affinity, comparison_affinity, in_affinity, is_numeric_affinity,
    number_to_text,
};
pub use arith::{
    add, arith, arith_enc, bit_and, bit_not, bit_not_enc, bit_or, bitwise, bitwise_enc, divide,
    multiply, negate, negate_enc, remainder, shift_left, shift_right, subtract, ArithOp, BitOp,
};
pub use atof::{
    atof, atof_flags, atoi64, dec_or_hex_to_i64, get_int32, is_sql_digit, is_sql_space,
    is_sql_xdigit, AtoFResult, Atoi64Result,
};
pub use cast::{blob_to_text, cast_to_type, cast_value, text_to_blob};
pub use collation::{
    compare_bytes_binary, compare_bytes_nocase_utf8, compare_bytes_rtrim_utf8, compare_text_bytes,
    comparison_collation_name, BuiltinCollation, Collation, CollationRegistry, OperandCollation,
};
pub use compare::{blob_compare, compare_op, int_float_compare, mem_compare, CompareOp};
pub use fp_core::{fp10_convert2, fp10_convert2_full, fp_decode, FpDecoded, FpSpecial};
pub use fp_format::{
    format_float, real_to_text, real_to_text_buf, FloatConversion, FloatFormatSpec, SignPrefix,
    DEFAULT_FP_DIGITS, FP_PRECISION_LIMIT,
};
pub use int64::{
    add_i64, div_i64, i64_to_real, int64_to_text, mul_i64, real_same_as_int, real_to_exact_i64,
    real_to_i64, real_to_integer_affinity, rem_i64, sub_i64,
};
pub use numeric::{
    apply_numeric_affinity, boolean_value, boolean_value_enc, int_value, int_value_enc,
    integer_literal_value, mem_real_value_rc, must_be_int, numeric_value, numeric_value_enc,
    numerify, real_literal_value, real_value, real_value_enc, text_numeric_value,
};
pub use utf::{sqlite_utf16_to_utf8, utf16_decode_units, utf16_encode_units, TextEncoding};

/// SQLite's five storage classes, numbered as `sqlite3_value_type()` except `NULL = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum StorageClass {
    Null = 0,
    Integer = 1,
    Real = 2,
    Text = 3,
    Blob = 4,
}

/// Column and expression affinity codes (`SQLITE_AFF_*`, `sqliteInt.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Affinity {
    None = 0x40,
    Blob = 0x41,
    Text = 0x42,
    Numeric = 0x43,
    Integer = 0x44,
    Real = 0x45,
    Flexnum = 0x46,
}

/// A canonical SQLite scalar value.
///
/// Invariant: `SqlValue::Real(r)` is never `NaN` (build with [`SqlValue::real`],
/// which folds `NaN` to `SqlValue::Null`, matching SQLite).
#[derive(Clone, Debug)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    /// Raw bytes of a `TEXT` value (UTF-8 by default; preserves invalid UTF-8 verbatim).
    Text(Box<[u8]>),
    /// Raw bytes of a `BLOB` value.
    Blob(Box<[u8]>),
}

impl SqlValue {
    /// Construct a canonical `REAL` value; `NaN` becomes `SqlValue::Null`.
    #[inline]
    pub fn real(v: f64) -> Self {
        if v.is_nan() {
            SqlValue::Null
        } else {
            SqlValue::Real(v)
        }
    }

    /// Construct a `TEXT` value from a UTF-8 string slice.
    #[inline]
    pub fn text(s: impl AsRef<str>) -> Self {
        SqlValue::Text(s.as_ref().as_bytes().to_vec().into_boxed_slice())
    }

    /// Construct a `TEXT` value from raw bytes (preserving invalid UTF-8 verbatim).
    #[inline]
    pub fn text_bytes(b: impl Into<Box<[u8]>>) -> Self {
        SqlValue::Text(b.into())
    }

    /// Construct a `BLOB` value from bytes.
    #[inline]
    pub fn blob(b: impl Into<Box<[u8]>>) -> Self {
        SqlValue::Blob(b.into())
    }

    #[inline]
    pub fn storage_class(&self) -> StorageClass {
        match self {
            SqlValue::Null => StorageClass::Null,
            SqlValue::Int(_) => StorageClass::Integer,
            SqlValue::Real(_) => StorageClass::Real,
            SqlValue::Text(_) => StorageClass::Text,
            SqlValue::Blob(_) => StorageClass::Blob,
        }
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }

    #[inline]
    pub fn is_integer(&self) -> bool {
        matches!(self, SqlValue::Int(_))
    }

    #[inline]
    pub fn is_real(&self) -> bool {
        matches!(self, SqlValue::Real(_))
    }

    #[inline]
    pub fn is_text(&self) -> bool {
        matches!(self, SqlValue::Text(_))
    }

    #[inline]
    pub fn is_blob(&self) -> bool {
        matches!(self, SqlValue::Blob(_))
    }

    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            SqlValue::Int(i) => Some(i),
            _ => None,
        }
    }

    #[inline]
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            SqlValue::Real(r) => Some(r),
            _ => None,
        }
    }

    #[inline]
    pub fn as_text_bytes(&self) -> Option<&[u8]> {
        match self {
            SqlValue::Text(b) => Some(b),
            _ => None,
        }
    }

    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SqlValue::Text(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    #[inline]
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            SqlValue::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// True when `self` satisfies the canonical representation invariant (never NaN).
    #[inline]
    pub fn is_canonical(&self) -> bool {
        match *self {
            SqlValue::Real(r) => !r.is_nan(),
            _ => true,
        }
    }

    /// Exact storage-class and bit-identical representation equality (for tests/oracles).
    /// Note: distinguishes `Int(1)` from `Real(1.0)` and `-0.0` from `+0.0`.
    pub fn exact_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SqlValue::Null, SqlValue::Null) => true,
            (SqlValue::Int(a), SqlValue::Int(b)) => a == b,
            (SqlValue::Real(a), SqlValue::Real(b)) => a.to_bits() == b.to_bits(),
            (SqlValue::Text(a), SqlValue::Text(b)) => a == b,
            (SqlValue::Blob(a), SqlValue::Blob(b)) => a == b,
            _ => false,
        }
    }

    /// Debug rendering that never conflates storage classes: `NULL`, `1`, `1.0r`, `"a"`, `x'01'`.
    pub fn describe(&self) -> String {
        match self {
            SqlValue::Null => "NULL".to_string(),
            SqlValue::Int(i) => i.to_string(),
            SqlValue::Real(r) => {
                if *r == 0.0 && r.is_sign_negative() {
                    "-0r".to_string()
                } else {
                    format!("{r}r")
                }
            }
            SqlValue::Text(b) => match std::str::from_utf8(b) {
                Ok(s) => format!("{s:?}"),
                Err(_) => format!("text_x'{}'", hex_lower(b)),
            },
            SqlValue::Blob(b) => format!("x'{}'", hex_lower(b)),
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

impl From<i64> for SqlValue {
    #[inline]
    fn from(v: i64) -> Self {
        SqlValue::Int(v)
    }
}

impl From<i32> for SqlValue {
    #[inline]
    fn from(v: i32) -> Self {
        SqlValue::Int(i64::from(v))
    }
}

impl From<f64> for SqlValue {
    #[inline]
    fn from(v: f64) -> Self {
        SqlValue::real(v)
    }
}

impl From<&str> for SqlValue {
    #[inline]
    fn from(s: &str) -> Self {
        SqlValue::text(s)
    }
}

impl From<String> for SqlValue {
    #[inline]
    fn from(s: String) -> Self {
        SqlValue::Text(s.into_bytes().into_boxed_slice())
    }
}

impl fmt::Display for SqlValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}
