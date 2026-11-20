//! Column defaults as SQLite computes them for records predating
//! `ALTER TABLE ADD COLUMN` (`sqlite3ColumnDefault` / `sqlite3ValueFromExpr`),
//! plus `OP_RealAffinity` read-back.

use xdb_core::value::utf::decode_stored_text_to_utf8;
use xdb_core::value::{
    affinity_type, apply_affinity_raw, atof, cast_value, dec_or_hex_to_i64, i64_to_real, numerify,
};
use xdb_core::{Affinity, SqlValue, TextEncoding};
use xdb_parser::{dequote, expr_children, get_int32_token, same_name, BinaryOp, Expr, QuoteStyle, UnaryOp};

/// `OP_RealAffinity` (`vdbe.c:2206`): an `INTEGER` becomes `REAL` `(double)i`;
/// anything else is unchanged.
#[inline]
pub fn real_read_back(v: SqlValue) -> SqlValue {
    match v {
        SqlValue::Int(i) => SqlValue::Real(i64_to_real(i)),
        other => other,
    }
}

fn is_hex_token(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 2 && b[0] == b'0' && (b[1] | 0x20) == b'x'
}

fn is_true_false_id(e: &Expr) -> Option<bool> {
    if let Expr::Column(c) = e {
        if c.table.is_none() && c.column.quote == QuoteStyle::None {
            if c.column.name.len() == 4 && same_name(&c.column.name, "true") {
                return Some(true);
            }
            if c.column.name.len() == 5 && same_name(&c.column.name, "false") {
                return Some(false);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Fold {
    is_false: bool,
    has_func: bool,
}

fn fold_flags(e: &Expr) -> Fold {
    match e {
        Expr::Number(n) => Fold {
            is_false: !n.is_float && !n.separators && get_int32_token(&n.text) == Some(0),
            has_func: false,
        },
        Expr::Boolean { value, .. } => Fold {
            is_false: !*value,
            has_func: false,
        },
        Expr::Function(_) | Expr::Like { .. } | Expr::CurrentTime { .. } => Fold {
            is_false: false,
            has_func: true,
        },
        Expr::Binary {
            op, left, right, ..
        } => {
            let l = fold_flags(left);
            let r = fold_flags(right);
            if *op == BinaryOp::And && (l.is_false || r.is_false) && !l.has_func && !r.has_func {
                return Fold {
                    is_false: true,
                    has_func: false,
                };
            }
            Fold {
                is_false: false,
                has_func: matches!(op, BinaryOp::Ptr | BinaryOp::Ptr2) || l.has_func || r.has_func,
            }
        }
        Expr::InList {
            not,
            operand,
            items,
            ..
        } => {
            let op = fold_flags(operand);
            if items.is_empty() && !op.has_func {
                return Fold {
                    is_false: !*not,
                    has_func: false,
                };
            }
            let mut has_func = op.has_func;
            for x in items {
                has_func = has_func || fold_flags(x).has_func;
            }
            Fold {
                is_false: false,
                has_func,
            }
        }
        Expr::IsNull { not, operand, .. } => {
            if is_literal_after_fold(operand) {
                return Fold {
                    is_false: !*not,
                    has_func: false,
                };
            }
            Fold {
                is_false: false,
                has_func: fold_flags(operand).has_func,
            }
        }
        _ => {
            let mut has_func = false;
            for x in expr_children(e) {
                has_func = has_func || fold_flags(x).has_func;
            }
            Fold {
                is_false: false,
                has_func,
            }
        }
    }
}

fn is_literal_after_fold(mut e: &Expr) -> bool {
    while let Expr::Unary {
        op: UnaryOp::Pos | UnaryOp::Neg,
        operand,
        ..
    } = e
    {
        e = operand;
    }
    if matches!(e, Expr::Number(_) | Expr::String(_) | Expr::Blob(_)) {
        return true;
    }
    folded_int(e).is_some()
}

fn folded_int(e: &Expr) -> Option<i32> {
    match e {
        Expr::Number(n) => {
            if n.is_float {
                None
            } else {
                get_int32_token(&n.text)
            }
        }
        Expr::IsNull { not, operand, .. } => {
            if is_literal_after_fold(operand) {
                Some(if *not { 1 } else { 0 })
            } else {
                None
            }
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            if *op != BinaryOp::And {
                return None;
            }
            let l = fold_flags(left);
            let r = fold_flags(right);
            if (l.is_false || r.is_false) && !l.has_func && !r.has_func {
                Some(0)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn collapse_unary(e: &Expr) -> Expr {
    if let Expr::Unary {
        span,
        op: op @ (UnaryOp::Pos | UnaryOp::Neg),
        operand,
    } = e
    {
        let inner = collapse_unary(operand);
        if let Expr::Unary {
            op: UnaryOp::Pos,
            operand: inner_op,
            ..
        } = inner
        {
            return Expr::Unary {
                span: *span,
                op: *op,
                operand: inner_op,
            };
        }
        return Expr::Unary {
            span: *span,
            op: *op,
            operand: Box::new(inner),
        };
    }
    e.clone()
}

struct Tagged {
    v: SqlValue,
    benc: TextEncoding,
}

fn utf16_numeric_text(b: &[u8], enc: TextEncoding) -> Vec<u8> {
    let n = b.len() & !1;
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(n / 2);
    let mut i = 0;
    while i < n {
        let hi = if le { b[i + 1] } else { b[i] };
        if hi != 0 {
            break;
        }
        out.push(if le { b[i] } else { b[i + 1] });
        i += 2;
    }
    out
}

fn numeric_operand(t: &Tagged) -> SqlValue {
    if let SqlValue::Blob(ref b) = t.v {
        if t.benc != TextEncoding::Utf8 {
            return SqlValue::Text(utf16_numeric_text(b, t.benc).into_boxed_slice());
        }
    }
    t.v.clone()
}

fn cast_tagged(t: Tagged, aff: Affinity, enc: TextEncoding) -> Tagged {
    if let SqlValue::Blob(ref b) = t.v {
        if matches!(aff, Affinity::Blob | Affinity::None) {
            return t;
        }
        if aff == Affinity::Text {
            let bytes = if enc != TextEncoding::Utf8 {
                &b[..b.len() & !1]
            } else {
                &b[..]
            };
            let decoded = decode_stored_text_to_utf8(bytes, t.benc);
            return Tagged {
                v: SqlValue::Text(decoded.into_boxed_slice()),
                benc: enc,
            };
        }
        let op = numeric_operand(&t);
        let cast_enc = if matches!(op, SqlValue::Blob(_)) {
            t.benc
        } else {
            TextEncoding::Utf8
        };
        let v = cast_value(op, aff, cast_enc).unwrap_or(SqlValue::Null);
        return Tagged { v, benc: enc };
    }
    let v = cast_value(t.v, aff, enc).unwrap_or(SqlValue::Null);
    Tagged { v, benc: enc }
}

fn tagged(v: SqlValue, enc: TextEncoding) -> Tagged {
    Tagged { v, benc: enc }
}

/// `sqlite3ValueFromExpr` (`vdbemem.c:1795`) for a `DEFAULT` expression.
pub fn value_from_expr(expr: &Expr, affinity: Affinity, enc: TextEncoding) -> Option<SqlValue> {
    let collapsed = collapse_unary(expr);
    value_from_expr_tagged(&collapsed, affinity, enc).map(|t| t.v)
}

fn value_from_expr_tagged(expr: &Expr, affinity: Affinity, enc: TextEncoding) -> Option<Tagged> {
    let mut owned = collapse_unary(expr);
    while let Expr::Unary {
        op: UnaryOp::Pos,
        operand,
        ..
    } = owned
    {
        owned = collapse_unary(&operand);
    }

    if let Expr::Cast {
        operand, type_name, ..
    } = &owned
    {
        let raw = type_name
            .as_ref()
            .map(|t| dequote(&t.text))
            .unwrap_or_default();
        let aff = affinity_type(raw.as_bytes());
        let inner = value_from_expr_tagged(operand, aff, enc)?;
        let c = cast_tagged(inner, aff, enc);
        return Some(Tagged {
            v: apply_affinity_raw(c.v, affinity),
            benc: c.benc,
        });
    }

    let mut neg = false;
    let mut e_ref = &owned;
    if let Expr::Unary {
        op: UnaryOp::Neg,
        operand,
        ..
    } = e_ref
    {
        let is_neg_lit = folded_int(operand).is_some()
            || matches!(operand.as_ref(), Expr::Number(n) if !is_hex_token(&n.text));
        if is_neg_lit {
            e_ref = operand;
            neg = true;
        }
    }

    if let Some(iv) = folded_int(e_ref) {
        let v = if neg { -i64::from(iv) } else { i64::from(iv) };
        let eff_aff = if affinity == Affinity::Blob {
            Affinity::Numeric
        } else {
            affinity
        };
        return Some(tagged(apply_affinity_raw(SqlValue::Int(v), eff_aff), enc));
    }

    match e_ref {
        Expr::String(s) => Some(tagged(
            apply_affinity_raw(SqlValue::text(&s.value), affinity),
            enc,
        )),
        Expr::Number(n) => {
            if !n.is_float {
                let d = dec_or_hex_to_i64(n.text.as_bytes());
                if d.rc == 0 {
                    // Notice: d.value is non-negative for unsigned literals, except 0x8000... (which is negative i64::MIN).
                    // If neg && d.value == i64::MIN -> cannot negate as i64 without overflow, falls through to text path.
                    if let Some(v) = if !neg {
                        Some(d.value)
                    } else {
                        d.value.checked_neg()
                    } {
                        let eff_aff = if affinity == Affinity::Blob {
                            Affinity::Numeric
                        } else {
                            affinity
                        };
                        return Some(tagged(apply_affinity_raw(SqlValue::Int(v), eff_aff), enc));
                    }
                }
            }
            let text = if neg {
                format!("-{}", n.text)
            } else {
                n.text.clone()
            };
            if affinity == Affinity::Blob {
                if n.is_float {
                    return Some(tagged(SqlValue::real(atof(text.as_bytes()).value), enc));
                }
                return Some(tagged(
                    apply_affinity_raw(SqlValue::from(text), Affinity::Numeric),
                    enc,
                ));
            }
            Some(tagged(apply_affinity_raw(SqlValue::from(text), affinity), enc))
        }
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => {
            let inner = value_from_expr_tagged(operand, affinity, enc)?;
            let n = numerify(&numeric_operand(&inner));
            let out = match n {
                SqlValue::Null => SqlValue::Null,
                SqlValue::Real(r) => SqlValue::real(-r),
                SqlValue::Int(i) => {
                    if i == i64::MIN {
                        SqlValue::Real(9_223_372_036_854_775_808.0)
                    } else {
                        SqlValue::Int(-i)
                    }
                }
                other => other,
            };
            Some(tagged(apply_affinity_raw(out, affinity), enc))
        }
        Expr::Null { .. } | Expr::Variable(_) => Some(tagged(SqlValue::Null, enc)),
        Expr::Blob(b) => Some(Tagged {
            v: SqlValue::blob(b.value.clone().into_boxed_slice()),
            benc: TextEncoding::Utf8,
        }),
        Expr::Boolean { value, .. } => Some(tagged(
            apply_affinity_raw(SqlValue::Int(i64::from(*value)), affinity),
            enc,
        )),
        Expr::Column(_) => {
            let tf = is_true_false_id(e_ref)?;
            Some(tagged(
                apply_affinity_raw(SqlValue::Int(i64::from(tf)), affinity),
                enc,
            ))
        }
        Expr::InList {
            not,
            operand,
            items,
            ..
        } => {
            if !items.is_empty() || fold_flags(operand).has_func {
                return None;
            }
            Some(tagged(
                apply_affinity_raw(SqlValue::Int(i64::from(*not)), affinity),
                enc,
            ))
        }
        _ => None,
    }
}

/// The value a field absent from a short record reads as (`sqlite3ColumnDefault`).
pub fn column_default_value(expr: Option<&Expr>, affinity: Affinity, enc: TextEncoding) -> SqlValue {
    let Some(expr) = expr else {
        return SqlValue::Null;
    };
    let Some(v) = value_from_expr(expr, affinity, enc) else {
        return SqlValue::Null;
    };
    if affinity == Affinity::Real {
        real_read_back(v)
    } else {
        v
    }
}
