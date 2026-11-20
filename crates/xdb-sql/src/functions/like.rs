//! `like(PATTERN, STRING[, ESCAPE])`, `glob(PATTERN, STRING)`, and XDB `ILIKE`.
//!
//! Direct port of SQLite 3.54.1 `likeFunc` and `patternCompare` (`func.c:742-985`),
//! linearized to a single backtrack point (zero recursion depth) with metered work.

use xdb_core::SqlValue;

use crate::errors::{SqlError, SqlResult};
use crate::functions::FunctionContext;
use crate::utf8::{text_bytes_of, utf8_char_len_cstr, utf8_read, value_byte_len};

/// SQLite `compareInfo` (`func.c:669`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompareInfo {
    /// `'*'` or `'%'`, or `0` when disabled by an equal `ESCAPE`.
    pub match_all: u32,
    /// `'?'` or `'_'`, or `0` when disabled.
    pub match_one: u32,
    /// `'['` for `GLOB`, `0` for `LIKE`.
    pub match_set: u32,
    /// ASCII case-insensitive (`LIKE` / `ILIKE`).
    pub no_case: bool,
}

pub const GLOB_INFO: CompareInfo = CompareInfo {
    match_all: b'*' as u32,
    match_one: b'?' as u32,
    match_set: b'[' as u32,
    no_case: false,
};

pub const LIKE_INFO: CompareInfo = CompareInfo {
    match_all: b'%' as u32,
    match_one: b'_' as u32,
    match_set: 0,
    no_case: true,
};

pub const LIKE_CASE_SENSITIVE_INFO: CompareInfo = CompareInfo {
    match_all: b'%' as u32,
    match_one: b'_' as u32,
    match_set: 0,
    no_case: false,
};

#[inline]
fn lower_ascii(c: u32) -> u32 {
    if (0x41..=0x5a).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

#[inline]
fn upper_ascii(c: u32) -> u32 {
    if (0x61..=0x7a).contains(&c) {
        c - 0x20
    } else {
        c
    }
}

/// Convert raw UTF-8 bytes up to first NUL into a NUL-terminated `Vec<u32>` via `sqlite3Utf8Read`.
fn to_chars(bytes: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0;
    loop {
        let (c, next) = utf8_read(bytes, i);
        out.push(c);
        if c == 0 {
            break;
        }
        i = next;
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanKind {
    None,
    Ascii,
    Char,
    Set,
}

/// `patternCompare(zPattern, zString, pInfo, matchOther) == SQLITE_MATCH`.
/// Both slices must be NUL-terminated.
pub fn pattern_match(pat: &[u32], str_chars: &[u32], info: CompareInfo, match_other: u32) -> bool {
    let match_one = info.match_one;
    let match_all = info.match_all;
    let no_case = info.no_case;

    let mut pi: usize = 0;
    let mut si: usize = 0;
    let mut z_escaped: Option<usize> = None;

    let mut scan = ScanKind::None;
    let mut scan_pi: usize = 0;
    let mut scan_si: usize = 0;
    let mut scan_c: u32 = 0;
    let mut scan_c2: u32 = 0;

    loop {
        let mut matched = false;
        loop {
            let mut c = pat[pi];
            pi += 1;
            if c == 0 {
                matched = str_chars[si] == 0;
                break;
            }
            if c == match_all && match_all != 0 {
                loop {
                    c = pat[pi];
                    pi += 1;
                    if c == match_all {
                        continue;
                    }
                    if c == match_one && match_one != 0 {
                        if str_chars[si] == 0 {
                            return false;
                        }
                        si += 1;
                        continue;
                    }
                    break;
                }
                if c == 0 {
                    return true;
                }
                if c == match_other && match_other != 0 {
                    if info.match_set == 0 {
                        c = pat[pi];
                        pi += 1;
                        if c == 0 {
                            return false;
                        }
                    } else {
                        scan = ScanKind::Set;
                        scan_pi = pi - 1;
                        scan_si = si;
                        break;
                    }
                }
                if c < 0x80 {
                    scan = ScanKind::Ascii;
                    scan_c = if no_case { upper_ascii(c) } else { c };
                    scan_c2 = if no_case { lower_ascii(c) } else { c };
                } else {
                    scan = ScanKind::Char;
                    scan_c = c;
                }
                scan_pi = pi;
                scan_si = si;
                break;
            }
            if c == match_other && match_other != 0 {
                if info.match_set == 0 {
                    c = pat[pi];
                    pi += 1;
                    if c == 0 {
                        break;
                    }
                    z_escaped = Some(pi);
                } else {
                    let mut prior_c: u32 = 0;
                    let mut seen = false;
                    let mut invert = false;
                    c = str_chars[si];
                    si += 1;
                    if c == 0 {
                        break;
                    }
                    let mut c2 = pat[pi];
                    pi += 1;
                    if c2 == b'^' as u32 {
                        invert = true;
                        c2 = pat[pi];
                        pi += 1;
                    }
                    if c2 == b']' as u32 {
                        if c == b']' as u32 {
                            seen = true;
                        }
                        c2 = pat[pi];
                        pi += 1;
                    }
                    while c2 != 0 && c2 != b']' as u32 {
                        if c2 == b'-' as u32
                            && pat[pi] != b']' as u32
                            && pat[pi] != 0
                            && prior_c > 0
                        {
                            c2 = pat[pi];
                            pi += 1;
                            if c >= prior_c && c <= c2 {
                                seen = true;
                            }
                            prior_c = 0;
                        } else {
                            if c == c2 {
                                seen = true;
                            }
                            prior_c = c2;
                        }
                        c2 = pat[pi];
                        pi += 1;
                    }
                    if c2 == 0 || seen == invert {
                        break;
                    }
                    continue;
                }
            }
            let c2 = str_chars[si];
            si += 1;
            if c == c2 {
                continue;
            }
            if no_case && c < 0x80 && c2 < 0x80 && lower_ascii(c) == lower_ascii(c2) {
                continue;
            }
            if c == match_one && match_one != 0 && z_escaped != Some(pi) && c2 != 0 {
                continue;
            }
            break;
        }
        if matched {
            return true;
        }
        match scan {
            ScanKind::Ascii => {
                let mut k = scan_si;
                loop {
                    let x = str_chars[k];
                    if x == 0 {
                        return false;
                    }
                    if x == scan_c || x == scan_c2 {
                        break;
                    }
                    k += 1;
                }
                scan_si = k + 1;
                si = scan_si;
            }
            ScanKind::Char => {
                let mut k = scan_si;
                loop {
                    let x = str_chars[k];
                    k += 1;
                    if x == 0 {
                        return false;
                    }
                    if x == scan_c {
                        break;
                    }
                }
                scan_si = k;
                si = scan_si;
            }
            ScanKind::Set => {
                if str_chars[scan_si] == 0 {
                    return false;
                }
                si = scan_si;
                scan_si += 1;
            }
            ScanKind::None => return false,
        }
        pi = scan_pi;
        z_escaped = None;
    }
}

/// `likeFunc` (`func.c:921`). `args` is `[pattern, string]` or `[pattern, string, escape]`.
pub fn like_invoke(
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
    mut info: CompareInfo,
) -> SqlResult<SqlValue> {
    if args.len() < 2 || args.len() > 3 {
        return Err(SqlError::runtime(format!(
            "wrong number of arguments to function {}()",
            if info.match_set != 0 { "glob" } else { "like" }
        )));
    }
    let pattern_val = &args[0];
    if value_byte_len(pattern_val) > ctx.limits.max_like_pattern_length {
        return Err(SqlError::runtime("LIKE or GLOB pattern too complex"));
    }

    let match_other = if args.len() == 3 {
        let Some(esc_bytes) = text_bytes_of(&args[2]) else {
            return Ok(SqlValue::Null);
        };
        if utf8_char_len_cstr(&esc_bytes) != 1 {
            return Err(SqlError::runtime(
                "ESCAPE expression must be a single character",
            ));
        }
        let (ec, _) = utf8_read(&esc_bytes, 0);
        if ec == info.match_all {
            info.match_all = 0;
        }
        if ec == info.match_one {
            info.match_one = 0;
        }
        ec
    } else {
        info.match_set
    };

    let Some(pat_bytes) = text_bytes_of(pattern_val) else {
        return Ok(SqlValue::Null);
    };
    let Some(str_bytes) = text_bytes_of(&args[1]) else {
        return Ok(SqlValue::Null);
    };

    ctx.charge_steps((pat_bytes.len().min(4096) + str_bytes.len().min(4096)) as u64 / 16 + 1)?;
    let pat_chars = to_chars(&pat_bytes);
    let str_chars = to_chars(&str_bytes);
    Ok(SqlValue::Int(i64::from(pattern_match(
        &pat_chars,
        &str_chars,
        info,
        match_other,
    ))))
}
