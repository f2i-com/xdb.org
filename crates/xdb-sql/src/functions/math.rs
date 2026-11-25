//! SQLite 3.54.1 built-in math functions (`SQLITE_ENABLE_MATH_FUNCTIONS`, `func.c:2500+`).

use xdb_core::value::{real_value, SqlValue};

use crate::errors::{SqlError, SqlResult};

/// SQLite's `sqlite3_value_numeric_type` check used by math functions:
/// non-numeric strings/blobs (whose `atof` rc <= 0) return `NULL` for math functions!
fn math_real_arg(v: &SqlValue) -> Option<f64> {
    match v {
        SqlValue::Null | SqlValue::Blob(_) => None,
        SqlValue::Int(i) => Some(*i as f64),
        SqlValue::Real(r) => Some(*r),
        SqlValue::Text(b) => {
            let r = xdb_core::value::atof(b);
            if r.rc > 0 {
                Some(r.value)
            } else {
                None
            }
        }
    }
}

fn unary_math(args: &[SqlValue], name: &str, f: impl FnOnce(f64) -> f64) -> SqlResult<SqlValue> {
    if args.len() != 1 {
        return Err(SqlError::runtime(format!(
            "wrong number of arguments to function {name}()"
        )));
    }
    let Some(x) = math_real_arg(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    Ok(SqlValue::real(f(x)))
}

fn binary_math(
    args: &[SqlValue],
    name: &str,
    f: impl FnOnce(f64, f64) -> f64,
) -> SqlResult<SqlValue> {
    if args.len() != 2 {
        return Err(SqlError::runtime(format!(
            "wrong number of arguments to function {name}()"
        )));
    }
    let (Some(x), Some(y)) = (math_real_arg(&args[0]), math_real_arg(&args[1])) else {
        return Ok(SqlValue::Null);
    };
    Ok(SqlValue::real(f(x, y)))
}

/// Dispatch any SQLite built-in math function by lowercase ASCII name.
/// Returns `Ok(None)` if `lower_name` is not a built-in math function.
pub fn call_math(lower_name: &str, args: &[SqlValue]) -> SqlResult<Option<SqlValue>> {
    let val = match lower_name {
        "acos" => unary_math(args, "acos", f64::acos)?,
        "acosh" => unary_math(args, "acosh", f64::acosh)?,
        "asin" => unary_math(args, "asin", f64::asin)?,
        "asinh" => unary_math(args, "asinh", f64::asinh)?,
        "atan" => unary_math(args, "atan", f64::atan)?,
        "atan2" => binary_math(args, "atan2", f64::atan2)?,
        "atanh" => unary_math(args, "atanh", f64::atanh)?,
        "ceil" | "ceiling" => unary_math(args, lower_name, f64::ceil)?,
        "cos" => unary_math(args, "cos", f64::cos)?,
        "cosh" => unary_math(args, "cosh", f64::cosh)?,
        "degrees" => unary_math(args, "degrees", |x| x * (180.0 / std::f64::consts::PI))?,
        "exp" => unary_math(args, "exp", f64::exp)?,
        "floor" => unary_math(args, "floor", f64::floor)?,
        "ln" => unary_math(args, "ln", |x| if x <= 0.0 { f64::NAN } else { x.ln() })?,
        "log10" => unary_math(args, "log10", |x| {
            if x <= 0.0 {
                f64::NAN
            } else {
                x.log10()
            }
        })?,
        "log2" => unary_math(args, "log2", |x| if x <= 0.0 { f64::NAN } else { x.log2() })?,
        "log" => match args.len() {
            1 => unary_math(args, "log", |x| if x <= 0.0 { f64::NAN } else { x.log10() })?,
            2 => binary_math(args, "log", |b, x| {
                if b <= 0.0 || b == 1.0 || x <= 0.0 {
                    f64::NAN
                } else {
                    x.ln() / b.ln()
                }
            })?,
            _ => {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function log()",
                ))
            }
        },
        "mod" => binary_math(args, "mod", |x, y| if y == 0.0 { f64::NAN } else { x % y })?,
        "pi" => {
            if !args.is_empty() {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function pi()",
                ));
            }
            SqlValue::Real(std::f64::consts::PI)
        }
        "pow" | "power" => binary_math(args, lower_name, f64::powf)?,
        "radians" => unary_math(args, "radians", |x| x * (std::f64::consts::PI / 180.0))?,
        "sin" => unary_math(args, "sin", f64::sin)?,
        "sinh" => unary_math(args, "sinh", f64::sinh)?,
        "sqrt" => unary_math(args, "sqrt", |x| if x < 0.0 { f64::NAN } else { x.sqrt() })?,
        "tan" => unary_math(args, "tan", f64::tan)?,
        "tanh" => unary_math(args, "tanh", f64::tanh)?,
        "trunc" => unary_math(args, "trunc", f64::trunc)?,
        "sign" => {
            if args.len() != 1 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function sign()",
                ));
            }
            match &args[0] {
                SqlValue::Null | SqlValue::Blob(_) => SqlValue::Null,
                SqlValue::Int(i) => SqlValue::Int(i.signum()),
                SqlValue::Real(r) => SqlValue::Int(if *r > 0.0 {
                    1
                } else if *r < 0.0 {
                    -1
                } else {
                    0
                }),
                SqlValue::Text(b) => {
                    let r = xdb_core::value::atof(b);
                    if r.rc <= 0 {
                        SqlValue::Null
                    } else if r.value > 0.0 {
                        SqlValue::Int(1)
                    } else if r.value < 0.0 {
                        SqlValue::Int(-1)
                    } else {
                        SqlValue::Int(0)
                    }
                }
            }
        }
        "round" => {
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function round()",
                ));
            }
            if args[0].is_null() || (args.len() == 2 && args[1].is_null()) {
                SqlValue::Null
            } else {
                let r = real_value(&args[0]);
                let n = if args.len() == 2 {
                    let raw = xdb_core::value::int_value(&args[1]);
                    raw.clamp(0, 30) as i32
                } else {
                    0
                };
                SqlValue::real(sqlite_round(r, n))
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(val))
}

/// SQLite `roundFunc` (`func.c:361`): half-away-from-zero for `n == 0` and decimal-formatted
/// rounding for `n > 0`, handling `|r| >= 4503599627370496.0` (already integral in f64).
fn sqlite_round(r: f64, n: i32) -> f64 {
    if !r.is_finite() {
        return r;
    }
    if n == 0 {
        if r.abs() >= 4503599627370496.0 {
            return r;
        }
        if r < 0.0 {
            (r - 0.5).ceil()
        } else {
            (r + 0.5).floor()
        }
    } else if r.abs() >= 4503599627370496.0 {
        r
    } else {
        let formatted = xdb_core::value::format_float(
            r,
            xdb_core::value::FloatFormatSpec {
                conversion: xdb_core::value::FloatConversion::F,
                precision: Some(n),
                ..xdb_core::value::FloatFormatSpec::new(xdb_core::value::FloatConversion::F)
            },
        );
        xdb_core::value::atof(formatted.as_bytes()).value
    }
}
