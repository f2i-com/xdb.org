//! SQLite 3.54.1 core scalar functions (`func.c`).

use xdb_core::value::{int_value, is_sql_xdigit, mem_compare, real_value, SqlValue};

use crate::errors::{SqlError, SqlResult};
use crate::functions::like::{like_invoke, GLOB_INFO, LIKE_INFO};
use crate::functions::printf::{sql_printf, str_appendf};
use crate::functions::FunctionContext;
use crate::utf8::{
    ascii_lower_byte, c_str_len, skip_utf8, text_bytes_of, utf8_char_len_cstr, utf8_read,
    ByteAccum,
};

pub const SQLITE_VERSION: &str = "3.54.1";
pub const SQLITE_SOURCE_ID: &str =
    "2026-10-16 14:45:20 555e07ef33f69a0957d24d8e555b702464a131d6f1607f4c3c1e4551e8e6ca76";

pub const COMPILE_OPTIONS: &[&str] = &[
    "ATOMIC_INTRINSICS=1",
    "DEFAULT_AUTOVACUUM",
    "DEFAULT_CACHE_SIZE=-16384",
    "DEFAULT_FILE_FORMAT=4",
    "DEFAULT_JOURNAL_SIZE_LIMIT=-1",
    "DEFAULT_MMAP_SIZE=0",
    "DEFAULT_PAGE_SIZE=8192",
    "DEFAULT_RECURSIVE_TRIGGERS",
    "DEFAULT_SYNCHRONOUS=2",
    "DEFAULT_WAL_AUTOCHECKPOINT=1000",
    "DEFAULT_WAL_SYNCHRONOUS=2",
    "DQS=0",
    "ENABLE_COLUMN_METADATA",
    "ENABLE_MATH_FUNCTIONS",
    "ENABLE_PERCENTILE",
    "MAX_ATTACHED=10",
    "MAX_COLUMN=2000",
    "MAX_COMPOUND_SELECT=500",
    "MAX_EXPR_DEPTH=1000",
    "MAX_FUNCTION_ARG=1000",
    "MAX_LENGTH=1000000000",
    "MAX_LIKE_PATTERN_LENGTH=50000",
    "MAX_PAGE_COUNT=0xfffffffe",
    "MAX_PAGE_SIZE=65536",
    "MAX_SQL_LENGTH=1000000000",
    "MAX_TRIGGER_DEPTH=1000",
    "MAX_VARIABLE_NUMBER=32766",
    "OMIT_LOAD_EXTENSION",
    "THREADSAFE=0",
];

fn wrong_arity(name: &str) -> SqlError {
    SqlError::runtime(format!("wrong number of arguments to function {name}()"))
}

/// Dispatch any SQLite core scalar function by lowercase ASCII name.
/// Returns `Ok(None)` if not a core scalar function.
pub fn call_core_scalar(
    lower_name: &str,
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<SqlValue>> {
    let val = match lower_name {
        "abs" => {
            if args.len() != 1 {
                return Err(wrong_arity("abs"));
            }
            abs_fn(&args[0])?
        }
        "coalesce" => {
            if args.len() < 2 {
                return Err(wrong_arity("coalesce"));
            }
            args.iter()
                .find(|v| !v.is_null())
                .cloned()
                .unwrap_or(SqlValue::Null)
        }
        "ifnull" => {
            if args.len() != 2 {
                return Err(wrong_arity("ifnull"));
            }
            if args[0].is_null() {
                args[1].clone()
            } else {
                args[0].clone()
            }
        }
        "nullif" => {
            if args.len() != 2 {
                return Err(wrong_arity("nullif"));
            }
            if mem_compare(&args[0], &args[1], ctx.collation) == std::cmp::Ordering::Equal {
                SqlValue::Null
            } else {
                args[0].clone()
            }
        }
        "iif" | "if" => {
            // SQLite 3.48+: iif(c1, v1, c2, v2, ..., [default]) with >= 2 args
            if args.len() < 2 {
                return Err(wrong_arity(lower_name));
            }
            let mut res = SqlValue::Null;
            let mut i = 0;
            while i + 1 < args.len() {
                if xdb_core::value::boolean_value(&args[i], false) {
                    res = args[i + 1].clone();
                    i = args.len(); // found
                    break;
                }
                i += 2;
            }
            if i == args.len() - 1 {
                res = args[i].clone();
            }
            res
        }
        "typeof" => {
            if args.len() != 1 {
                return Err(wrong_arity("typeof"));
            }
            let t = match &args[0] {
                SqlValue::Null => "null",
                SqlValue::Int(_) => "integer",
                SqlValue::Real(_) => "real",
                SqlValue::Text(_) => "text",
                SqlValue::Blob(_) => "blob",
            };
            SqlValue::text(t)
        }
        "length" => {
            if args.len() != 1 {
                return Err(wrong_arity("length"));
            }
            length_fn(&args[0])
        }
        "octet_length" => {
            if args.len() != 1 {
                return Err(wrong_arity("octet_length"));
            }
            match &args[0] {
                SqlValue::Null => SqlValue::Null,
                SqlValue::Blob(b) | SqlValue::Text(b) => SqlValue::Int(b.len() as i64),
                SqlValue::Int(_) | SqlValue::Real(_) => SqlValue::Int(
                    xdb_core::value::number_to_text(&args[0]).map_or(0, |s| s.len() as i64),
                ),
            }
        }
        "char" => char_fn(args, ctx)?,
        "unicode" => {
            if args.len() != 1 {
                return Err(wrong_arity("unicode"));
            }
            match text_bytes_of(&args[0]) {
                None => SqlValue::Null,
                Some(b) => {
                    if b.is_empty() || b[0] == 0 {
                        SqlValue::Null
                    } else {
                        let (cp, _) = utf8_read(&b, 0);
                        SqlValue::Int(i64::from(cp))
                    }
                }
            }
        }
        "hex" => {
            if args.len() != 1 {
                return Err(wrong_arity("hex"));
            }
            hex_fn(&args[0], ctx)?
        }
        "unhex" => {
            if args.is_empty() || args.len() > 2 {
                return Err(wrong_arity("unhex"));
            }
            unhex_fn(args, ctx)?
        }
        "unistr" => {
            if args.len() != 1 {
                return Err(wrong_arity("unistr"));
            }
            unistr_fn(&args[0], ctx)?
        }
        "quote" => {
            if args.len() != 1 {
                return Err(wrong_arity("quote"));
            }
            quote_fn(&args[0], false, ctx)?
        }
        "unistr_quote" => {
            if args.len() != 1 {
                return Err(wrong_arity("unistr_quote"));
            }
            quote_fn(&args[0], true, ctx)?
        }
        "lower" => {
            if args.len() != 1 {
                return Err(wrong_arity("lower"));
            }
            ascii_case_fn(&args[0], false, ctx)?
        }
        "upper" => {
            if args.len() != 1 {
                return Err(wrong_arity("upper"));
            }
            ascii_case_fn(&args[0], true, ctx)?
        }
        "substr" | "substring" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(wrong_arity(lower_name));
            }
            substr_fn(args, ctx)?
        }
        "instr" => {
            if args.len() != 2 {
                return Err(wrong_arity("instr"));
            }
            instr_fn(&args[0], &args[1], ctx)?
        }
        "replace" => {
            if args.len() != 3 {
                return Err(wrong_arity("replace"));
            }
            replace_fn(&args[0], &args[1], &args[2], ctx)?
        }
        "trim" => {
            if args.is_empty() || args.len() > 2 {
                return Err(wrong_arity("trim"));
            }
            trim_fn(args, true, true, ctx)?
        }
        "ltrim" => {
            if args.is_empty() || args.len() > 2 {
                return Err(wrong_arity("ltrim"));
            }
            trim_fn(args, true, false, ctx)?
        }
        "rtrim" => {
            if args.is_empty() || args.len() > 2 {
                return Err(wrong_arity("rtrim"));
            }
            trim_fn(args, false, true, ctx)?
        }
        "concat" => {
            if args.is_empty() {
                return Err(wrong_arity("concat"));
            }
            concat_fn(args, ctx)?
        }
        "concat_ws" => {
            if args.len() < 2 {
                return Err(wrong_arity("concat_ws"));
            }
            concat_ws_fn(args, ctx)?
        }
        "zeroblob" => {
            if args.len() != 1 {
                return Err(wrong_arity("zeroblob"));
            }
            let raw = int_value(&args[0]).max(0);
            if raw > ctx.limits.max_length as i64 {
                return Err(SqlError::too_big());
            }
            let n = raw as usize;
            ctx.charge_steps((n / 256) as u64 + 1)?;
            if let Some(m) = ctx.meter {
                m.reserve_memory(n as u64)?;
                m.release_memory(n as u64);
            }
            SqlValue::Blob(vec![0u8; n].into_boxed_slice())
        }
        "randomblob" => {
            if args.len() != 1 {
                return Err(wrong_arity("randomblob"));
            }
            ctx.require_pure("randomblob")?;
            let raw = int_value(&args[0]).max(1);
            if raw > ctx.limits.max_length as i64 {
                return Err(SqlError::too_big());
            }
            let n = raw as usize;
            ctx.charge_steps((n / 64) as u64 + 1)?;
            if let Some(m) = ctx.meter {
                m.reserve_memory(n as u64)?;
                m.release_memory(n as u64);
            }
            let mut out = vec![0u8; n];
            ctx.fill_random(&mut out);
            SqlValue::Blob(out.into_boxed_slice())
        }
        "random" => {
            if !args.is_empty() {
                return Err(wrong_arity("random"));
            }
            ctx.require_pure("random")?;
            let mut b = [0u8; 8];
            ctx.fill_random(&mut b);
            let r = i64::from_le_bytes(b);
            // SQLite never returns i64::MIN (uses -(r & i64::MAX) when negative).
            let val = if r == i64::MIN { 0 } else { r };
            SqlValue::Int(val)
        }
        "min" => {
            if args.len() < 2 {
                return Err(wrong_arity("min"));
            }
            minmax_scalar(args, ctx, false)
        }
        "max" => {
            if args.len() < 2 {
                return Err(wrong_arity("max"));
            }
            minmax_scalar(args, ctx, true)
        }
        "like" => like_invoke(
            args,
            ctx,
            if ctx.case_sensitive_like {
                crate::functions::LIKE_CASE_SENSITIVE_INFO
            } else {
                LIKE_INFO
            },
        )?,
        "glob" => {
            if args.len() != 2 {
                return Err(wrong_arity("glob"));
            }
            like_invoke(args, ctx, GLOB_INFO)?
        }
        "printf" | "format" => sql_printf(args, ctx)?,
        "likelihood" => {
            if args.len() != 2 {
                return Err(wrong_arity("likelihood"));
            }
            match args[1] {
                SqlValue::Real(p) if (0.0..=1.0).contains(&p) => args[0].clone(),
                _ => {
                    return Err(SqlError::runtime(
                        "second argument of likelihood() must be a constant between 0.0 and 1.0",
                    ));
                }
            }
        }
        "likely" | "unlikely" => {
            if args.len() != 1 {
                return Err(wrong_arity(lower_name));
            }
            args[0].clone()
        }
        "sqlite_version" => {
            if !args.is_empty() {
                return Err(wrong_arity("sqlite_version"));
            }
            SqlValue::text(SQLITE_VERSION)
        }
        "sqlite_source_id" => {
            if !args.is_empty() {
                return Err(wrong_arity("sqlite_source_id"));
            }
            SqlValue::text(SQLITE_SOURCE_ID)
        }
        "sqlite_compileoption_used" => {
            if args.len() != 1 {
                return Err(wrong_arity("sqlite_compileoption_used"));
            }
            match text_bytes_of(&args[0]) {
                None => SqlValue::Null,
                Some(b) => SqlValue::Int(i64::from(compile_option_used(&b))),
            }
        }
        "sqlite_compileoption_get" => {
            if args.len() != 1 {
                return Err(wrong_arity("sqlite_compileoption_get"));
            }
            let raw = int_value(&args[0]);
            if let Ok(idx) = usize::try_from(raw) {
                if idx < COMPILE_OPTIONS.len() {
                    SqlValue::text(COMPILE_OPTIONS[idx])
                } else {
                    SqlValue::Null
                }
            } else {
                SqlValue::Null
            }
        }
        "changes" => {
            if !args.is_empty() {
                return Err(wrong_arity("changes"));
            }
            ctx.require_pure("changes")?;
            SqlValue::Int(ctx.changes)
        }
        "total_changes" => {
            if !args.is_empty() {
                return Err(wrong_arity("total_changes"));
            }
            ctx.require_pure("total_changes")?;
            SqlValue::Int(ctx.total_changes)
        }
        "last_insert_rowid" => {
            if !args.is_empty() {
                return Err(wrong_arity("last_insert_rowid"));
            }
            ctx.require_pure("last_insert_rowid")?;
            SqlValue::Int(ctx.last_insert_rowid)
        }
        _ => return Ok(None),
    };
    Ok(Some(val))
}

fn abs_fn(v: &SqlValue) -> SqlResult<SqlValue> {
    match v {
        SqlValue::Null => Ok(SqlValue::Null),
        SqlValue::Int(i) => match i.checked_abs() {
            Some(a) => Ok(SqlValue::Int(a)),
            None => Err(SqlError::runtime("integer overflow")),
        },
        SqlValue::Real(r) => Ok(SqlValue::real(r.abs())),
        SqlValue::Text(_) | SqlValue::Blob(_) => Ok(SqlValue::real(real_value(v).abs())),
    }
}

fn length_fn(v: &SqlValue) -> SqlValue {
    match v {
        SqlValue::Null => SqlValue::Null,
        SqlValue::Blob(b) => SqlValue::Int(b.len() as i64),
        SqlValue::Text(b) => SqlValue::Int(utf8_char_len_cstr(b) as i64),
        SqlValue::Int(_) | SqlValue::Real(_) => {
            SqlValue::Int(xdb_core::value::number_to_text(v).map_or(0, |s| s.len() as i64))
        }
    }
}

fn char_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let mut acc = ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    for arg in args {
        let x = int_value(arg);
        let cp: u32 = if (0..=0x10_ffff).contains(&x) {
            x as u32
        } else {
            0xfffd
        };
        acc.push_utf8(cp)?;
    }
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}

fn hex_fn(v: &SqlValue, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let Some(bytes) = text_bytes_of(v) else {
        return Ok(SqlValue::text(""));
    };
    let out_len = bytes.len().checked_mul(2).ok_or_else(SqlError::too_big)?;
    if out_len > ctx.limits.max_length {
        return Err(SqlError::too_big());
    }
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve(out_len as u64)?;
    if out_len >= 256 {
        ctx.charge_steps((out_len / 256) as u64)?;
    }
    let mut out = Vec::with_capacity(out_len);
    for &b in bytes.iter() {
        out.push(HEX_UPPER[(b >> 4) as usize]);
        out.push(HEX_UPPER[(b & 0x0f) as usize]);
    }
    Ok(SqlValue::Text(out.into_boxed_slice()))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unhex_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let Some(src) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    let skip_chars: Option<std::collections::HashSet<u32>> = if args.len() == 2 {
        let Some(ign) = text_bytes_of(&args[1]) else {
            return Ok(SqlValue::Null);
        };
        let mut chars = std::collections::HashSet::new();
        let mut p = 0;
        let mut ticks = 0u64;
        while p < ign.len() && ign[p] != 0 {
            ticks += 1;
            if (ticks & 255) == 0 {
                ctx.charge_steps(1)?;
            }
            let (cp, next) = utf8_read(&ign, p);
            if chars.insert(cp) {
                mem.reserve(8)?;
            }
            p = next;
        }
        Some(chars)
    } else {
        None
    };

    let cap = (src.len() / 2).min(ctx.limits.max_length);
    mem.reserve(cap as u64)?;
    let mut out = Vec::with_capacity(cap);
    let mut hi: Option<u8> = None;
    let mut i = 0;
    let mut ticks = 0u64;
    while i < src.len() && src[i] != 0 {
        ticks += 1;
        if (ticks & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        let b = src[i];
        if let Some(v) = hex_val(b) {
            if let Some(h) = hi.take() {
                if out.len() >= ctx.limits.max_length {
                    return Err(SqlError::too_big());
                }
                out.push((h << 4) | v);
            } else {
                hi = Some(v);
            }
            i += 1;
        } else if let Some(ref skip) = skip_chars {
            if hi.is_some() {
                return Ok(SqlValue::Null);
            }
            let (cp, next) = utf8_read(&src, i);
            if !skip.contains(&cp) {
                return Ok(SqlValue::Null);
            }
            i = next;
        } else {
            return Ok(SqlValue::Null);
        }
    }
    if hi.is_some() {
        return Ok(SqlValue::Null);
    }
    Ok(SqlValue::Blob(out.into_boxed_slice()))
}

fn is_n_hex(b: &[u8], start: usize, n: usize) -> Option<u32> {
    if start + n > b.len() {
        return None;
    }
    let mut val = 0u32;
    for &byte in &b[start..start + n] {
        let d = hex_val(byte)?;
        val = (val << 4) | u32::from(d);
    }
    if val > 0x10_ffff {
        return None;
    }
    Some(val)
}

fn unistr_fn(v: &SqlValue, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let Some(z_in) = text_bytes_of(v) else {
        return Ok(SqlValue::Null);
    };
    let mut acc = ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    let mut i = 0;
    let n_in = z_in.len();
    while i < n_in {
        let mut z = i;
        while z < n_in && z_in[z] != b'\\' && z_in[z] != 0 {
            z += 1;
        }
        if z >= n_in || z_in[z] == 0 {
            acc.push_bytes(&z_in[i..n_in])?;
            break;
        }
        if z > i {
            acc.push_bytes(&z_in[i..z])?;
            i = z;
        }
        let n1 = z_in.get(i + 1).copied().unwrap_or(0);
        let cp = if n1 == b'\\' {
            i += 2;
            acc.push(b'\\')?;
            continue;
        } else if is_sql_xdigit(n1) {
            let v = is_n_hex(&z_in, i + 1, 4);
            i += 5;
            v
        } else if n1 == b'+' {
            let v = is_n_hex(&z_in, i + 2, 6);
            i += 8;
            v
        } else if n1 == b'u' {
            let v = is_n_hex(&z_in, i + 2, 4);
            i += 6;
            v
        } else if n1 == b'U' {
            let v = is_n_hex(&z_in, i + 2, 8);
            i += 10;
            v
        } else {
            None
        };
        let Some(code) = cp else {
            return Err(SqlError::runtime("invalid Unicode escape"));
        };
        acc.push_utf8(code)?;
    }
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}

fn quote_fn(v: &SqlValue, unistr: bool, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    match v {
        SqlValue::Null => Ok(SqlValue::text("NULL")),
        SqlValue::Int(i) => Ok(SqlValue::text(i.to_string())),
        SqlValue::Blob(b) => {
            const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
            let needed = b
                .len()
                .checked_mul(2)
                .and_then(|n| n.checked_add(4))
                .ok_or_else(SqlError::too_big)?;
            if needed > ctx.limits.max_length {
                return Err(SqlError::too_big());
            }
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            mem.reserve(needed as u64)?;
            if needed >= 256 {
                ctx.charge_steps((needed / 256) as u64)?;
            }
            let mut out = Vec::with_capacity(b.len() * 2 + 3);
            out.extend_from_slice(b"X'");
            for &byte in b.iter() {
                out.push(HEX_UPPER[(byte >> 4) as usize]);
                out.push(HEX_UPPER[(byte & 0x0f) as usize]);
            }
            out.push(b'\'');
            Ok(SqlValue::Text(out.into_boxed_slice()))
        }
        SqlValue::Real(_) => {
            let mut acc = ByteAccum::new(ctx.limits.max_length, true, ctx.meter);
            str_appendf(&mut acc, b"%!0.17g", std::slice::from_ref(v), 0)?;
            Ok(SqlValue::Text(acc.into_boxed_bytes()))
        }
        SqlValue::Text(_) => {
            let mut acc = ByteAccum::new(ctx.limits.max_length, true, ctx.meter);
            let fmt: &[u8] = if unistr { b"%#Q" } else { b"%Q" };
            str_appendf(&mut acc, fmt, std::slice::from_ref(v), 0)?;
            Ok(SqlValue::Text(acc.into_boxed_bytes()))
        }
    }
}

fn ascii_case_fn(v: &SqlValue, to_upper: bool, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let Some(bytes) = text_bytes_of(v) else {
        return Ok(SqlValue::Null);
    };
    if bytes.len() > ctx.limits.max_length {
        return Err(SqlError::too_big());
    }
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve(bytes.len() as u64)?;
    if bytes.len() >= 256 {
        ctx.charge_steps((bytes.len() / 256) as u64)?;
    }
    let mut out = bytes.into_owned();
    for b in &mut out {
        if to_upper {
            if b.is_ascii_lowercase() {
                *b -= 0x20;
            }
        } else if b.is_ascii_uppercase() {
            *b += 0x20;
        }
    }
    Ok(SqlValue::Text(out.into_boxed_slice()))
}

/// SQLite `substrFunc` (`func.c:236`): 1-based indexing, negative start counts from end,
/// negative length takes characters preceding start. BLOB operates on bytes, TEXT on UTF-8 chars.
fn substr_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    if args[0].is_null() || args[1].is_null() || (args.len() == 3 && args[2].is_null()) {
        return Ok(SqlValue::Null);
    }
    let mut p1 = int_value(&args[1]);
    let mut p2: i64 = if args.len() == 3 {
        int_value(&args[2])
    } else {
        i64::MAX
    };
    let mut neg_p2 = false;
    if p2 < 0 {
        neg_p2 = true;
        p2 = p2.saturating_neg();
    }

    if let SqlValue::Blob(bytes) = &args[0] {
        let len = bytes.len() as i64;
        if p1 < 0 {
            p1 = len.saturating_add(p1);
            if p1 < 0 {
                p2 = p2.saturating_add(p1);
                p1 = 0;
            }
        } else if p1 > 0 {
            p1 -= 1;
        } else if p2 > 0 {
            p2 -= 1;
        }
        if neg_p2 {
            p1 = p1.saturating_sub(p2);
            if p1 < 0 {
                p2 = p2.saturating_add(p1);
                p1 = 0;
            }
        }
        if p1 >= len || p2 <= 0 {
            return Ok(SqlValue::Blob(Box::new([])));
        }
        let start = usize::try_from(p1).unwrap_or(bytes.len());
        let take = usize::try_from(p2)
            .unwrap_or(usize::MAX)
            .min(bytes.len().saturating_sub(start));
        let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
        mem.reserve(take as u64)?;
        return Ok(SqlValue::Blob(bytes[start..start + take].into()));
    }

    let Some(bytes) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    // SQLite `substrFunc` (`func.c:372,423,433`) treats TEXT as NUL-terminated for
    // character counting and UTF-8 stepping (BLOBs above preserve embedded NULs).
    let n_bytes = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let mut start_byte: usize;

    if p1 < 0 {
        // Count characters from the end.
        let mut total_chars: i64 = 0;
        let mut idx = 0;
        while idx < n_bytes {
            if (total_chars & 255) == 0 {
                ctx.charge_steps(1)?;
            }
            idx = skip_utf8(&bytes, idx);
            total_chars += 1;
        }
        p1 = total_chars.saturating_add(p1);
        if p1 < 0 {
            p2 = p2.saturating_add(p1);
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }

    if neg_p2 {
        p1 = p1.saturating_sub(p2);
        if p1 < 0 {
            p2 = p2.saturating_add(p1);
            p1 = 0;
        }
    }

    if p2 <= 0 {
        return Ok(SqlValue::text(""));
    }

    start_byte = 0;
    let mut skip = p1;
    let mut iter_count = 0u64;
    while skip > 0 && start_byte < n_bytes {
        if (iter_count & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        iter_count += 1;
        start_byte = skip_utf8(&bytes, start_byte);
        skip -= 1;
    }
    let mut end_byte = start_byte;
    let mut take = p2;
    while take > 0 && end_byte < n_bytes {
        if (iter_count & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        iter_count += 1;
        end_byte = skip_utf8(&bytes, end_byte);
        take -= 1;
    }

    let slice = &bytes[start_byte..end_byte];
    if slice.len() > ctx.limits.max_length {
        return Err(SqlError::too_big());
    }
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve(slice.len() as u64)?;
    Ok(SqlValue::Text(slice.into()))
}

/// SQLite `instrFunc` (`func.c:412`): 1-based position (bytes for two BLOBs, chars otherwise).
/// Uses O(N + M) KMP search with step metering so adversarial needles cannot cause O(N*M) DoS.
fn instr_fn(hay: &SqlValue, needle: &SqlValue, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    if hay.is_null() || needle.is_null() {
        return Ok(SqlValue::Null);
    }
    let is_blob = hay.is_blob() && needle.is_blob();
    let Some(h) = text_bytes_of(hay) else {
        return Ok(SqlValue::Null);
    };
    let Some(n) = text_bytes_of(needle) else {
        return Ok(SqlValue::Null);
    };
    if n.is_empty() {
        return Ok(SqlValue::Int(1));
    }
    if n.len() > h.len() {
        return Ok(SqlValue::Int(0));
    }

    // Build KMP prefix table in O(M) with metering so `instr` is strictly linear-time.
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve((n.len().saturating_mul(std::mem::size_of::<usize>())) as u64)?;
    let mut lps = vec![0usize; n.len()];
    let mut len = 0usize;
    let mut idx = 1usize;
    while idx < n.len() {
        if (idx & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        if n[idx] == n[len] {
            len += 1;
            lps[idx] = len;
            idx += 1;
        } else if len != 0 {
            len = lps[len - 1];
        } else {
            lps[idx] = 0;
            idx += 1;
        }
    }

    // Search haystack in O(N).
    let mut i = 0usize;
    let mut j = 0usize;
    let mut found_byte: Option<usize> = None;
    while i < h.len() {
        if (i & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        if h[i] == n[j] {
            i += 1;
            j += 1;
            if j == n.len() {
                let start = i - j;
                // For TEXT, SQLite matches only on UTF-8 character boundaries (non-continuation byte).
                if is_blob || (h[start] & 0xc0) != 0x80 {
                    found_byte = Some(start);
                    break;
                }
                j = lps[j - 1];
            }
        } else if j != 0 {
            j = lps[j - 1];
        } else {
            i += 1;
        }
    }

    let Some(match_off) = found_byte else {
        return Ok(SqlValue::Int(0));
    };
    if is_blob {
        return Ok(SqlValue::Int((match_off + 1) as i64));
    }
    let mut char_pos: i64 = 1;
    let mut p = 0usize;
    while p < match_off {
        if (p & 255) == 0 {
            ctx.charge_steps(1)?;
        }
        p = skip_utf8(&h, p);
        char_pos += 1;
    }
    Ok(SqlValue::Int(char_pos))
}

/// SQLite `replaceFunc` (`func.c:1302`).
fn replace_fn(
    src_v: &SqlValue,
    pat_v: &SqlValue,
    rep_v: &SqlValue,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    let (Some(src), Some(pat), Some(rep)) = (
        text_bytes_of(src_v),
        text_bytes_of(pat_v),
        text_bytes_of(rep_v),
    ) else {
        return Ok(SqlValue::Null);
    };
    let src: &[u8] = &src;
    let pat: &[u8] = &pat;
    let rep: &[u8] = &rep;
    if pat.is_empty() || pat[0] == 0 || pat.len() > src.len() {
        let mut out_mem = crate::utf8::MemoryGuard::new(ctx.meter);
        out_mem.reserve(src.len() as u64)?;
        return Ok(SqlValue::Text(src.into()));
    }
    // Linear-time O(N + M) metered KMP non-overlapping replace (handles embedded NUL bytes too!).
    let m = pat.len();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve((m as u64).saturating_mul(std::mem::size_of::<usize>() as u64))?;
    let mut lps = vec![0usize; m];
    let mut len = 0usize;
    let mut k = 1usize;
    while k < m {
        if (k & 127) == 0 {
            ctx.charge_steps(1)?;
        }
        if pat[k] == pat[len] {
            len += 1;
            lps[k] = len;
            k += 1;
        } else if len != 0 {
            len = lps[len - 1];
        } else {
            lps[k] = 0;
            k += 1;
        }
    }

    let mut acc = ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    let mut last = 0usize;
    let mut i = 0usize;
    let mut j = 0usize;
    let mut ops = 0u64;
    while i < src.len() {
        ops += 1;
        if (ops & 127) == 0 {
            ctx.charge_steps(1)?;
        }
        if src[i] == pat[j] {
            i += 1;
            j += 1;
            if j == m {
                let match_start = i - m;
                acc.push_bytes(&src[last..match_start])?;
                acc.push_bytes(rep)?;
                last = i;
                j = 0;
            }
        } else if j != 0 {
            j = lps[j - 1];
        } else {
            i += 1;
        }
    }
    acc.push_bytes(&src[last..])?;
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}

/// SQLite `trimFunc` (`func.c:1197`): trims UTF-8 characters present in `chars` (default `' '`).
fn trim_fn(
    args: &[SqlValue],
    trim_left: bool,
    trim_right: bool,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    let Some(src) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    let cut_bytes: std::borrow::Cow<'_, [u8]> = if args.len() == 2 {
        let Some(c) = text_bytes_of(&args[1]) else {
            return Ok(SqlValue::Null);
        };
        c
    } else {
        std::borrow::Cow::Borrowed(b" ")
    };
    // SQLite `trimFunc` (`func.c:1610`) stops scanning `zCharSet` at the first NUL byte.
    let cut_len = cut_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(cut_bytes.len());
    let cut_bytes: &[u8] = &cut_bytes[..cut_len];

    // Fast O(N) stack-bitset path for ASCII trim character sets (99.9% of calls).
    if cut_bytes.is_ascii() {
        let mut mask = [false; 128];
        for &b in cut_bytes.iter() {
            mask[usize::from(b)] = true;
        }
        let mut start = 0usize;
        let mut end = src.len();
        if trim_left {
            while start < end && src[start] < 128 && mask[usize::from(src[start])] {
                start += 1;
            }
        }
        if trim_right {
            while end > start && src[end - 1] < 128 && mask[usize::from(src[end - 1])] {
                end -= 1;
            }
        }
        if src.len() >= 256 {
            ctx.charge_steps((src.len() / 256) as u64)?;
        }
        let mut out_mem = crate::utf8::MemoryGuard::new(ctx.meter);
        out_mem.reserve((end - start) as u64)?;
        return Ok(SqlValue::Text(src[start..end].into()));
    }

    // Split `cut_bytes` into individual UTF-8 character byte slices (metered).
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    if !cut_bytes.is_empty() {
        ctx.charge_steps((cut_bytes.len() / 64) as u64 + 1)?;
        mem.reserve(
            (cut_bytes.len() as u64)
                .saturating_mul(std::mem::size_of::<&[u8]>() as u64),
        )?;
    }
    let mut cut_chars: Vec<&[u8]> = Vec::new();
    let mut p = 0;
    while p < cut_bytes.len() {
        let next = skip_utf8(cut_bytes, p);
        cut_chars.push(&cut_bytes[p..next]);
        p = next;
    }

    let mut start = 0;
    let mut end = src.len();
    let mut ops = 0u64;

    if trim_left && !cut_chars.is_empty() {
        'outer_l: while start < end {
            for &ch in &cut_chars {
                ops += 1;
                if (ops & 127) == 0 {
                    ctx.charge_steps(1)?;
                }
                if end - start >= ch.len() && &src[start..start + ch.len()] == ch {
                    start += ch.len();
                    continue 'outer_l;
                }
            }
            break;
        }
    }

    if trim_right && !cut_chars.is_empty() {
        'outer_r: while end > start {
            for &ch in &cut_chars {
                ops += 1;
                if (ops & 127) == 0 {
                    ctx.charge_steps(1)?;
                }
                if end - start >= ch.len() && &src[end - ch.len()..end] == ch {
                    end -= ch.len();
                    continue 'outer_r;
                }
            }
            break;
        }
    }

    mem.reserve((end - start) as u64)?;
    Ok(SqlValue::Text(src[start..end].into()))
}

fn concat_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let mut acc = ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    for arg in args {
        if let Some(b) = text_bytes_of(arg) {
            acc.push_bytes(&b)?;
        }
    }
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}

fn concat_ws_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let Some(sep) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    let mut acc = ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    let mut first = true;
    for arg in &args[1..] {
        if let Some(b) = text_bytes_of(arg) {
            if !first {
                acc.push_bytes(&sep)?;
            }
            acc.push_bytes(&b)?;
            first = false;
        }
    }
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}

fn minmax_scalar(args: &[SqlValue], ctx: &FunctionContext<'_>, is_max: bool) -> SqlValue {
    if args[0].is_null() {
        return SqlValue::Null;
    }
    let mut best = 0usize;
    for i in 1..args.len() {
        if args[i].is_null() {
            return SqlValue::Null;
        }
        let cmp = mem_compare(&args[best], &args[i], ctx.collation);
        let better = if is_max {
            cmp == std::cmp::Ordering::Less
        } else {
            cmp != std::cmp::Ordering::Less
        };
        if better {
            best = i;
        }
    }
    args[best].clone()
}

fn is_id_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn compile_option_used(name: &[u8]) -> i32 {
    let mut z = &name[..c_str_len(name)];
    if z.len() >= 7 && z[..7].eq_ignore_ascii_case(b"sqlite_") {
        z = &z[7..];
    }
    let n = z.len();
    for opt in COMPILE_OPTIONS {
        let ob = opt.as_bytes();
        if n <= ob.len()
            && z.iter()
                .zip(ob.iter())
                .all(|(&a, &b)| ascii_lower_byte(a) == ascii_lower_byte(b))
        {
            let next = ob.get(n).copied().unwrap_or(0);
            if !is_id_byte(next) {
                return 1;
            }
        }
    }
    0
}
