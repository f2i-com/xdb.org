//! Type affinity, ported from SQLite 3.54.1 (`build.c`, `vdbe.c`, `expr.c`).

use super::fp_format::{real_to_text, DEFAULT_FP_DIGITS};
use super::int64::{i64_to_real, int64_to_text, real_to_integer_affinity};
use super::numeric::apply_numeric_affinity;
use super::{Affinity, SqlValue};

/// `sqlite3IsNumericAffinity` (`sqliteInt.h`): `NUMERIC`, `INTEGER`, `REAL`, and `FLEXNUM`.
#[inline]
pub fn is_numeric_affinity(aff: Option<Affinity>) -> bool {
    matches!(
        aff,
        Some(Affinity::Numeric | Affinity::Integer | Affinity::Real | Affinity::Flexnum)
    )
}

const H_CHAR: u32 = 0x6368_6172; // 'char'
const H_CLOB: u32 = 0x636c_6f62; // 'clob'
const H_TEXT: u32 = 0x7465_7874; // 'text'
const H_BLOB: u32 = 0x626c_6f62; // 'blob'
const H_REAL: u32 = 0x7265_616c; // 'real'
const H_FLOA: u32 = 0x666c_6f61; // 'floa'
const H_DOUB: u32 = 0x646f_7562; // 'doub'
const H_INT: u32 = 0x0069_6e74; // 'int'

/// `sqlite3AffinityType` (`build.c:1711`): the affinity of a type name, as used by
/// `CAST(x AS name)` and declared column types. Scanning stops at an embedded NUL byte.
pub fn affinity_type(name: &[u8]) -> Affinity {
    let mut h: u32 = 0;
    let mut aff = Affinity::Numeric;
    for &x in name {
        if x == 0 {
            break;
        }
        let lc = if x.is_ascii_uppercase() { x + 0x20 } else { x };
        h = h.wrapping_shl(8).wrapping_add(u32::from(lc));
        if h == H_CHAR || h == H_CLOB || h == H_TEXT {
            aff = Affinity::Text;
        } else if h == H_BLOB && matches!(aff, Affinity::Numeric | Affinity::Real) {
            aff = Affinity::Blob;
        } else if (h == H_REAL || h == H_FLOA || h == H_DOUB) && aff == Affinity::Numeric {
            aff = Affinity::Real;
        } else if (h & 0x00ff_ffff) == H_INT {
            aff = Affinity::Integer;
            break;
        }
    }
    aff
}

/// Affinity of a table column with declared type `decl` (`sqlite3AddColumn`, `build.c:1550`):
/// absent or empty declared type is `BLOB`; otherwise [`affinity_type`].
pub fn affinity_of_decl_type(decl: Option<&[u8]>) -> Affinity {
    match decl {
        None | Some([]) => Affinity::Blob,
        Some(bytes) => affinity_type(bytes),
    }
}

/// `sqlite3VdbeMemStringify` / `vdbeMemRenderNum` (`vdbemem.c:471`): text of an `Int` or `Real`.
pub fn number_to_text(v: &SqlValue) -> Option<String> {
    match v {
        SqlValue::Int(i) => Some(int64_to_text(*i)),
        SqlValue::Real(r) => Some(real_to_text(*r, DEFAULT_FP_DIGITS)),
        _ => None,
    }
}

/// `applyAffinity` (`vdbe.c:397`): note that `Int` stays `Int` under `Affinity::Real` here
/// (the register value `OP_MakeRecord` writes); use [`apply_affinity`] for column read-back.
pub fn apply_affinity_raw(v: SqlValue, aff: Affinity) -> SqlValue {
    match v {
        SqlValue::Null | SqlValue::Blob(_) => v,
        SqlValue::Int(_) => match aff {
            Affinity::Text => SqlValue::from(number_to_text(&v).expect("int")),
            _ => v,
        },
        SqlValue::Real(r) => match aff {
            Affinity::Numeric | Affinity::Integer | Affinity::Real => {
                match real_to_integer_affinity(r) {
                    Some(i) => SqlValue::Int(i),
                    None => SqlValue::Real(r),
                }
            }
            Affinity::Text => SqlValue::from(real_to_text(r, DEFAULT_FP_DIGITS)),
            _ => SqlValue::Real(r),
        },
        SqlValue::Text(ref bytes) => {
            if is_numeric_affinity(Some(aff)) {
                apply_numeric_affinity(bytes, true).unwrap_or(v)
            } else {
                v
            }
        }
    }
}

/// The value a column of affinity `aff` holds after `v` is stored and read back
/// (`OP_Affinity` + `OP_RealAffinity`). Under `Affinity::Real`, integer results become `Real`.
pub fn apply_affinity(v: SqlValue, aff: Affinity) -> SqlValue {
    let r = apply_affinity_raw(v, aff);
    if aff == Affinity::Real {
        if let SqlValue::Int(i) = r {
            return SqlValue::Real(i64_to_real(i));
        }
    }
    r
}

/// `sqlite3CompareAffinity` (`expr.c:350`) on two operand affinities.
pub fn comparison_affinity(left: Option<Affinity>, right: Option<Affinity>) -> Affinity {
    let a1 = left.unwrap_or(Affinity::None);
    let a2 = right.unwrap_or(Affinity::None);
    if a1 > Affinity::None && a2 > Affinity::None {
        if a1 >= Affinity::Numeric || a2 >= Affinity::Numeric {
            Affinity::Numeric
        } else {
            Affinity::Blob
        }
    } else if a1 <= Affinity::None {
        a2
    } else {
        a1
    }
}

/// The affinity `lhs IN (...)` compares with (`exprINAffinity`, `expr.c:3530`).
pub fn in_affinity(lhs: Option<Affinity>, rhs_select_column: Option<Option<Affinity>>) -> Affinity {
    match rhs_select_column {
        Some(rhs) => comparison_affinity(rhs, lhs),
        None => lhs.unwrap_or(Affinity::None),
    }
}

/// Conversions `OP_Eq..OP_Ge` make to their operands before comparing (`vdbe.c:2443-2475`).
pub fn apply_comparison_affinity(
    left: SqlValue,
    right: SqlValue,
    aff: Option<Affinity>,
) -> (SqlValue, SqlValue) {
    let Some(aff) = aff else {
        return (left, right);
    };
    if aff >= Affinity::Numeric {
        let l = match left {
            SqlValue::Text(ref b) => apply_numeric_affinity(b, false).unwrap_or(left),
            other => other,
        };
        let r = match right {
            SqlValue::Text(ref b) => apply_numeric_affinity(b, false).unwrap_or(right),
            other => other,
        };
        return (l, r);
    }
    if aff == Affinity::Text && (left.is_text() || right.is_text()) {
        return (stringify_number(left), stringify_number(right));
    }
    (left, right)
}

fn stringify_number(v: SqlValue) -> SqlValue {
    match v {
        SqlValue::Int(i) => SqlValue::from(int64_to_text(i)),
        SqlValue::Real(r) => SqlValue::from(real_to_text(r, DEFAULT_FP_DIGITS)),
        other => other,
    }
}

/// Display name of an affinity code.
pub fn affinity_name(aff: Option<Affinity>) -> &'static str {
    match aff {
        None | Some(Affinity::None) => "NONE",
        Some(Affinity::Blob) => "BLOB",
        Some(Affinity::Text) => "TEXT",
        Some(Affinity::Numeric) => "NUMERIC",
        Some(Affinity::Integer) => "INTEGER",
        Some(Affinity::Real) => "REAL",
        Some(Affinity::Flexnum) => "FLEXNUM",
    }
}
