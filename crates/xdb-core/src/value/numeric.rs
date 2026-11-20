//! Numeric views of [`SqlValue`], ported from SQLite 3.54.1 `vdbemem.c` / `vdbe.c`.

use super::atof::{atof, atoi64, dec_or_hex_to_i64, AtoFResult};
use super::int64::{real_same_as_int, real_to_exact_i64, real_to_i64, real_to_integer_affinity};
use super::SqlValue;

/// `sqlite3MemRealValueRC` (`vdbemem.c:735`) for a `TEXT`/`BLOB` byte slice.
#[inline]
pub fn mem_real_value_rc(bytes: &[u8]) -> AtoFResult {
    atof(bytes)
}

/// `sqlite3VdbeRealValue` (`vdbemem.c:770`): `CAST(v AS REAL)`'s double for non-NULL `v`.
#[inline]
pub fn real_value(v: &SqlValue) -> f64 {
    match v {
        SqlValue::Null => 0.0,
        SqlValue::Int(i) => *i as f64,
        SqlValue::Real(r) => *r,
        SqlValue::Text(b) | SqlValue::Blob(b) => atof(b).value,
    }
}

/// `sqlite3VdbeIntValue` (`vdbemem.c:641`): `CAST(v AS INTEGER)`'s `i64` for non-NULL `v`.
#[inline]
pub fn int_value(v: &SqlValue) -> i64 {
    match v {
        SqlValue::Null => 0,
        SqlValue::Int(i) => *i,
        SqlValue::Real(r) => real_to_i64(*r),
        SqlValue::Text(b) | SqlValue::Blob(b) => atoi64(b).value,
    }
}

/// `sqlite3VdbeBooleanValue` (`vdbemem.c:791`): `INTEGER != 0`; `NULL` -> `if_null`;
/// anything else `real_value(v) != 0.0`.
#[inline]
pub fn boolean_value(v: &SqlValue, if_null: bool) -> bool {
    match v {
        SqlValue::Null => if_null,
        SqlValue::Int(i) => *i != 0,
        SqlValue::Real(r) => *r != 0.0,
        SqlValue::Text(b) | SqlValue::Blob(b) => atof(b).value != 0.0,
    }
}

/// `computeNumericType` (`vdbe.c:469`): the `INTEGER` or `REAL` that arithmetic
/// operators use for a `TEXT`/`BLOB` operand. Never fails (`"abc"` -> `Int(0)`).
pub fn text_numeric_value(bytes: &[u8]) -> SqlValue {
    let r = atof(bytes);
    if (r.rc & 2) == 0 {
        let a = atoi64(bytes);
        if if r.rc <= 0 { a.rc <= 1 } else { a.rc == 0 } {
            return SqlValue::Int(a.value);
        }
    }
    SqlValue::Real(r.value)
}

/// `numericType()` (`vdbe.c:500`): `NULL`, `INTEGER`, and `REAL` unchanged;
/// `TEXT`/`BLOB` go through [`text_numeric_value`].
pub fn numeric_value(v: &SqlValue) -> SqlValue {
    match v {
        SqlValue::Null => SqlValue::Null,
        SqlValue::Int(i) => SqlValue::Int(*i),
        SqlValue::Real(r) => SqlValue::Real(*r),
        SqlValue::Text(b) | SqlValue::Blob(b) => text_numeric_value(b),
    }
}

/// `sqlite3VdbeMemNumerify` (`vdbemem.c:893`), i.e. `CAST(v AS NUMERIC)`.
pub fn numerify(v: &SqlValue) -> SqlValue {
    match v {
        SqlValue::Null => SqlValue::Null,
        SqlValue::Int(i) => SqlValue::Int(*i),
        SqlValue::Real(r) => SqlValue::Real(*r),
        SqlValue::Text(b) | SqlValue::Blob(b) => {
            let r = atof(b);
            if (r.rc & 2) == 0 {
                let a = atoi64(b);
                if a.rc < 2 {
                    return SqlValue::Int(a.value);
                }
            }
            let ix = real_to_i64(r.value);
            if real_same_as_int(r.value, ix) {
                return SqlValue::Int(ix);
            }
            SqlValue::Real(r.value)
        }
    }
}

/// `alsoAnInt` (`vdbe.c:329`).
fn also_an_int(bytes: &[u8], r: f64) -> Option<i64> {
    let i_value = real_to_i64(r);
    if real_same_as_int(r, i_value) {
        return Some(i_value);
    }
    let a = atoi64(bytes);
    if a.rc == 0 {
        Some(a.value)
    } else {
        None
    }
}

/// `applyNumericAffinity` (`vdbe.c:354`) for `TEXT` bytes: if the whole text
/// (ignoring surrounding whitespace, up to the first NUL) is a number, returns
/// `Int` or `Real`; otherwise returns `None` (caller keeps the original `Text`).
pub fn apply_numeric_affinity(bytes: &[u8], try_for_int: bool) -> Option<SqlValue> {
    let r = atof(bytes);
    if r.rc <= 0 {
        return None;
    }
    if (r.rc & 2) == 0 {
        if let Some(i) = also_an_int(bytes, r.value) {
            return Some(SqlValue::Int(i));
        }
    }
    if try_for_int {
        if let Some(i) = real_to_integer_affinity(r.value) {
            return Some(SqlValue::Int(i));
        }
    }
    Some(SqlValue::Real(r.value))
}

/// `OP_MustBeInt` (`vdbe.c:2166`): integer value for `INTEGER`, integral `REAL` in
/// `[-2^63, 2^63)`, or `TEXT` that numeric affinity converts to `INTEGER`.
pub fn must_be_int(v: &SqlValue) -> Option<i64> {
    match v {
        SqlValue::Null | SqlValue::Blob(_) => None,
        SqlValue::Int(i) => Some(*i),
        SqlValue::Real(r) => real_to_exact_i64(*r),
        SqlValue::Text(b) => match apply_numeric_affinity(b, true) {
            Some(SqlValue::Int(i)) => Some(i),
            _ => None,
        },
    }
}

/// Value of an SQL integer literal token (decimal or `0x` hex, no sign) as coded by
/// `codeInteger` (`expr.c:4430`), optionally negated. Returns `None` for hex literals
/// beyond 64 bits ("hex literal too big").
pub fn integer_literal_value(token: &[u8], negate: bool) -> Option<SqlValue> {
    let res = dec_or_hex_to_i64(token);
    if (res.rc == 3 && !negate) || res.rc == 2 || (negate && res.value == i64::MIN) {
        if token.len() >= 2 && token[0] == b'0' && (token[1] == b'x' || token[1] == b'X') {
            return None;
        }
        return Some(real_literal_value(token, negate));
    }
    if !negate {
        return Some(SqlValue::Int(res.value));
    }
    if res.rc == 3 {
        return Some(SqlValue::Int(i64::MIN));
    }
    Some(SqlValue::Int(-res.value))
}

/// Value of an SQL float literal token (`codeReal`, `expr.c:4412`).
pub fn real_literal_value(token: &[u8], negate: bool) -> SqlValue {
    let r = atof(token).value;
    SqlValue::Real(if negate { -r } else { r })
}
