//! SQL arithmetic and bitwise operators with SQLite 3.54.1 semantics
//! (`vdbe.c:1952`, `2091`, `2744`).

use super::int64::{add_i64, div_i64, mul_i64, rem_i64, sub_i64};
use super::numeric::{int_value_enc, numeric_value_enc, real_value_enc};
use super::utf::TextEncoding;
use super::SqlValue;

/// Arithmetic operators sharing `OP_Add`'s body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArithOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

/// Bitwise operators sharing `OP_BitAnd`'s body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitOp {
    And,
    Or,
    ShiftLeft,
    ShiftRight,
}

/// `int_math` block of `OP_Add`..`OP_Remainder`. Returns `None` to fall back to `fp_math`.
fn int_math(op: ArithOp, left: i64, right: i64) -> Option<SqlValue> {
    match op {
        ArithOp::Add => add_i64(left, right).map(SqlValue::Int),
        ArithOp::Subtract => sub_i64(left, right).map(SqlValue::Int),
        ArithOp::Multiply => mul_i64(left, right).map(SqlValue::Int),
        ArithOp::Divide => {
            if right == 0 {
                return Some(SqlValue::Null);
            }
            if right == -1 && left == i64::MIN {
                return None; // overflow: fp_math
            }
            div_i64(left, right).map(SqlValue::Int)
        }
        ArithOp::Remainder => {
            if right == 0 {
                return Some(SqlValue::Null);
            }
            rem_i64(left, right).map(SqlValue::Int)
        }
    }
}

/// `fp_math` block: re-reads original operands via `real_value_enc` / `int_value_enc`.
fn fp_math(op: ArithOp, left: &SqlValue, right: &SqlValue, enc: TextEncoding) -> SqlValue {
    let r = if op == ArithOp::Remainder {
        let a = int_value_enc(right, enc);
        if a == 0 {
            return SqlValue::Null;
        }
        let m = rem_i64(int_value_enc(left, enc), a).unwrap_or(0);
        m as f64
    } else {
        let r_a = real_value_enc(right, enc);
        let r_b = real_value_enc(left, enc);
        match op {
            ArithOp::Add => r_b + r_a,
            ArithOp::Subtract => r_b - r_a,
            ArithOp::Multiply => r_b * r_a,
            ArithOp::Divide => {
                if r_a == 0.0 {
                    return SqlValue::Null;
                }
                r_b / r_a
            }
            ArithOp::Remainder => unreachable!(),
        }
    };
    SqlValue::real(r)
}

/// `OP_Add` / `OP_Subtract` / `OP_Multiply` / `OP_Divide` / `OP_Remainder`.
#[inline]
pub fn arith(op: ArithOp, left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith_enc(op, left, right, TextEncoding::Utf8)
}

/// Encoding-aware arithmetic (`BLOB` operands decoded using `enc` when `enc != Utf8`).
#[inline]
pub fn arith_enc(op: ArithOp, left: &SqlValue, right: &SqlValue, enc: TextEncoding) -> SqlValue {
    match (left, right) {
        (SqlValue::Int(l), SqlValue::Int(r)) => {
            if let Some(res) = int_math(op, *l, *r) {
                return res;
            }
            return fp_math(op, left, right, enc);
        }
        (SqlValue::Null, _) | (_, SqlValue::Null) => return SqlValue::Null,
        (SqlValue::Real(l), SqlValue::Real(r)) if op != ArithOp::Remainder => {
            if l.is_nan() || r.is_nan() {
                return SqlValue::Null;
            }
            let res = match op {
                ArithOp::Add => *l + *r,
                ArithOp::Subtract => *l - *r,
                ArithOp::Multiply => *l * *r,
                ArithOp::Divide => {
                    if *r == 0.0 {
                        return SqlValue::Null;
                    }
                    *l / *r
                }
                ArithOp::Remainder => unreachable!(),
            };
            return SqlValue::real(res);
        }
        (SqlValue::Real(l), SqlValue::Int(r)) if op != ArithOp::Remainder => {
            if l.is_nan() {
                return SqlValue::Null;
            }
            let rf = *r as f64;
            let res = match op {
                ArithOp::Add => *l + rf,
                ArithOp::Subtract => *l - rf,
                ArithOp::Multiply => *l * rf,
                ArithOp::Divide => {
                    if *r == 0 {
                        return SqlValue::Null;
                    }
                    *l / rf
                }
                ArithOp::Remainder => unreachable!(),
            };
            return SqlValue::real(res);
        }
        (SqlValue::Int(l), SqlValue::Real(r)) if op != ArithOp::Remainder => {
            if r.is_nan() {
                return SqlValue::Null;
            }
            let lf = *l as f64;
            let res = match op {
                ArithOp::Add => lf + *r,
                ArithOp::Subtract => lf - *r,
                ArithOp::Multiply => lf * *r,
                ArithOp::Divide => {
                    if *r == 0.0 {
                        return SqlValue::Null;
                    }
                    lf / *r
                }
                ArithOp::Remainder => unreachable!(),
            };
            return SqlValue::real(res);
        }
        (SqlValue::Real(r), _) | (_, SqlValue::Real(r)) => {
            if r.is_nan() {
                return SqlValue::Null;
            }
            return fp_math(op, left, right, enc);
        }
        _ => {}
    }
    let nl = numeric_value_enc(left, enc);
    let nr = numeric_value_enc(right, enc);
    if let (SqlValue::Int(l), SqlValue::Int(r)) = (&nl, &nr) {
        if let Some(res) = int_math(op, *l, *r) {
            return res;
        }
    }
    fp_math(op, left, right, enc)
}

#[inline]
pub fn add(left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith(ArithOp::Add, left, right)
}

#[inline]
pub fn subtract(left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith(ArithOp::Subtract, left, right)
}

#[inline]
pub fn multiply(left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith(ArithOp::Multiply, left, right)
}

#[inline]
pub fn divide(left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith(ArithOp::Divide, left, right)
}

#[inline]
pub fn remainder(left: &SqlValue, right: &SqlValue) -> SqlValue {
    arith(ArithOp::Remainder, left, right)
}

/// Unary minus of a non-literal expression: `0 - v` (`expr.c:5401`).
#[inline]
pub fn negate(v: &SqlValue) -> SqlValue {
    subtract(&SqlValue::Int(0), v)
}

/// Encoding-aware unary minus: `0 - v` decoding `BLOB` via `enc`.
#[inline]
pub fn negate_enc(v: &SqlValue, enc: TextEncoding) -> SqlValue {
    arith_enc(ArithOp::Subtract, &SqlValue::Int(0), v, enc)
}

/// `OP_BitAnd` / `OP_BitOr` / `OP_ShiftLeft` / `OP_ShiftRight` (`vdbe.c:2091`).
pub fn bitwise(op: BitOp, left: &SqlValue, right: &SqlValue) -> SqlValue {
    bitwise_enc(op, left, right, TextEncoding::Utf8)
}

/// Encoding-aware bitwise operation (`BLOB` operands decoded using `enc`).
pub fn bitwise_enc(op: BitOp, left: &SqlValue, right: &SqlValue, enc: TextEncoding) -> SqlValue {
    if left.is_null() || right.is_null() {
        return SqlValue::Null;
    }
    let a = int_value_enc(left, enc);
    let b = int_value_enc(right, enc);
    match op {
        BitOp::And => SqlValue::Int(a & b),
        BitOp::Or => SqlValue::Int(a | b),
        BitOp::ShiftLeft | BitOp::ShiftRight => {
            if b == 0 {
                return SqlValue::Int(a);
            }
            let mut shl = op == BitOp::ShiftLeft;
            let n: u32 = if b < 0 {
                shl = !shl;
                if b > -64 {
                    (-b) as u32
                } else {
                    64
                }
            } else if b < 64 {
                b as u32
            } else {
                64
            };
            if n >= 64 {
                return SqlValue::Int(if a >= 0 || shl { 0 } else { -1 });
            }
            if shl {
                SqlValue::Int(((a as u64) << n) as i64)
            } else {
                SqlValue::Int(a >> n)
            }
        }
    }
}

#[inline]
pub fn bit_and(left: &SqlValue, right: &SqlValue) -> SqlValue {
    bitwise(BitOp::And, left, right)
}

#[inline]
pub fn bit_or(left: &SqlValue, right: &SqlValue) -> SqlValue {
    bitwise(BitOp::Or, left, right)
}

#[inline]
pub fn shift_left(left: &SqlValue, right: &SqlValue) -> SqlValue {
    bitwise(BitOp::ShiftLeft, left, right)
}

#[inline]
pub fn shift_right(left: &SqlValue, right: &SqlValue) -> SqlValue {
    bitwise(BitOp::ShiftRight, left, right)
}

/// `~v` (`OP_BitNot`): `NULL` stays `NULL`, else ones' complement of `int_value(v)`.
#[inline]
pub fn bit_not(v: &SqlValue) -> SqlValue {
    bit_not_enc(v, TextEncoding::Utf8)
}

/// Encoding-aware `~v` (`BLOB` decoded using `enc`).
#[inline]
pub fn bit_not_enc(v: &SqlValue, enc: TextEncoding) -> SqlValue {
    if v.is_null() {
        SqlValue::Null
    } else {
        SqlValue::Int(!int_value_enc(v, enc))
    }
}
