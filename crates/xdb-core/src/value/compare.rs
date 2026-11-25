//! Value comparison, ported from SQLite 3.54.1 `vdbeaux.c` and `vdbe.c`.
//!
//! Sort order: `NULL < INTEGER/REAL (exact numerical) < TEXT (by collation) < BLOB (memcmp)`.

use std::cmp::Ordering;

use super::affinity::apply_comparison_affinity;
use super::collation::{compare_bytes_binary, Collation};
use super::{Affinity, SqlValue};

/// `sqlite3IntFloatCompare` (`vdbeaux.c:4527`): exact mathematical comparison of an
/// `i64` with an `f64` (never rounds `i` lossily to `f64`).
/// `NaN` sorts below every integer (like `NULL`), though canonical `SqlValue::Real` is never `NaN`.
#[inline]
pub fn int_float_compare(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        return Ordering::Greater;
    }
    // Both -2^63 and +2^63 are exact IEEE-754 f64 values.
    if r < -9223372036854775808.0 {
        return Ordering::Greater;
    }
    if r >= 9223372036854775808.0 {
        return Ordering::Less;
    }
    // Now -2^63 <= r < 2^63, so `r as i64` truncates toward zero with no saturation.
    let y = r as i64;
    match i.cmp(&y) {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => {
            let s = y as f64;
            if r > s {
                Ordering::Less
            } else if r < s {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
    }
}

/// `sqlite3BlobCompare` (`vdbeaux.c:4484`): `memcmp` over common length, then shorter is less.
#[inline]
pub fn blob_compare(a: &[u8], b: &[u8]) -> Ordering {
    compare_bytes_binary(a, b)
}

#[inline]
fn class_rank(v: &SqlValue) -> u8 {
    match v {
        SqlValue::Null => 0,
        SqlValue::Real(r) if r.is_nan() => 0,
        SqlValue::Int(_) | SqlValue::Real(_) => 1,
        SqlValue::Text(_) => 2,
        SqlValue::Blob(_) => 3,
    }
}

/// `sqlite3MemCompare` (`vdbeaux.c:4555`): ascending sort order (`ORDER BY` / index order).
/// Two `NULL`s compare equal. `TEXT` uses `coll` (defaulting to `BINARY` UTF-8).
#[inline]
pub fn mem_compare(a: &SqlValue, b: &SqlValue, coll: Option<&Collation>) -> Ordering {
    match (a, b) {
        (SqlValue::Int(x), SqlValue::Int(y)) => x.cmp(y),
        (SqlValue::Text(x), SqlValue::Text(y)) => match coll {
            Some(c) => c.compare_utf8(x, y),
            None => compare_bytes_binary(x, y),
        },
        (SqlValue::Blob(x), SqlValue::Blob(y)) => compare_bytes_binary(x, y),
        (SqlValue::Null, SqlValue::Null) => Ordering::Equal,
        (SqlValue::Real(x), SqlValue::Real(y)) => {
            if !x.is_nan() && !y.is_nan() {
                if x < y {
                    Ordering::Less
                } else if x > y {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            } else if x.is_nan() && y.is_nan() {
                Ordering::Equal
            } else if x.is_nan() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (SqlValue::Int(x), SqlValue::Real(y)) => {
            if y.is_nan() {
                Ordering::Greater
            } else {
                int_float_compare(*x, *y)
            }
        }
        (SqlValue::Real(x), SqlValue::Int(y)) => {
            if x.is_nan() {
                Ordering::Less
            } else {
                int_float_compare(*y, *x).reverse()
            }
        }
        (SqlValue::Null, SqlValue::Real(r)) | (SqlValue::Real(r), SqlValue::Null)
            if r.is_nan() =>
        {
            Ordering::Equal
        }
        _ => class_rank(a).cmp(&class_rank(b)),
    }
}

/// SQL comparison operators (`IS` / `IS NOT` are `OP_Eq` / `OP_Ne` with `SQLITE_NULLEQ`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
}

impl CompareOp {
    pub fn as_sql(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Ne => "<>",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
            CompareOp::Is => "IS",
            CompareOp::IsNot => "IS NOT",
        }
    }
}

#[inline]
fn is_null_or_nan(v: &SqlValue) -> bool {
    match v {
        SqlValue::Null => true,
        SqlValue::Real(r) => r.is_nan(),
        _ => false,
    }
}

/// Evaluates `left <op> right` (`OP_Eq..OP_Ge`, `vdbe.c:2345`), returning `Some(true/false)`
/// or `None` (`NULL`).
pub fn compare_op(
    op: CompareOp,
    left: SqlValue,
    right: SqlValue,
    aff: Option<Affinity>,
    coll: Option<&Collation>,
) -> Option<bool> {
    let l_null = is_null_or_nan(&left);
    let r_null = is_null_or_nan(&right);
    if l_null || r_null {
        return match op {
            CompareOp::Is => Some(l_null && r_null),
            CompareOp::IsNot => Some(!(l_null && r_null)),
            _ => None,
        };
    }
    let (l, r) = apply_comparison_affinity(left, right, aff);
    let c = mem_compare(&l, &r, coll);
    let res = match op {
        CompareOp::Eq | CompareOp::Is => c == Ordering::Equal,
        CompareOp::Ne | CompareOp::IsNot => c != Ordering::Equal,
        CompareOp::Lt => c == Ordering::Less,
        CompareOp::Le => c != Ordering::Greater,
        CompareOp::Gt => c == Ordering::Greater,
        CompareOp::Ge => c != Ordering::Less,
    };
    Some(res)
}
