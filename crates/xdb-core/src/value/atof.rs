//! Text -> number parsing, ported from SQLite 3.54.1 `util.c`:
//! `sqlite3AtoF`, `sqlite3Atoi64`, `sqlite3DecOrHexToI64`, and `sqlite3GetInt32`.
//!
//! Operates directly on `&[u8]` (UTF-8 or raw BLOB bytes).
//!
//! Embedded NUL asymmetry (matching SQLite):
//! - [`atof`] stops at the first `0x00` byte (zero-terminated C string).
//! - [`atoi64`] inspects the entire slice and treats an embedded `0x00` as trailing garbage.

use super::fp_core::fp10_convert2;

/// `sqlite3Isspace`: exactly `' '`, `'\t'`, `'\n'`, `'\x0b'`, `'\x0c'`, `'\r'`.
#[inline]
pub fn is_sql_space(b: u8) -> bool {
    b == b' ' || (0x09..=0x0d).contains(&b)
}

/// `sqlite3Isdigit`: ASCII `'0'..='9'` only.
#[inline]
pub fn is_sql_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

/// `sqlite3Isxdigit`: ASCII hex digits only.
#[inline]
pub fn is_sql_xdigit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

#[inline]
fn hex_to_u8(b: u8) -> u8 {
    if b <= b'9' {
        b - b'0'
    } else {
        (b & 0x0f) + 9
    }
}

/// `sqlite3AtoF` result-code bit flags (`util.c:850-860`).
pub mod atof_flags {
    /// Some prefix of the input is a valid number.
    pub const PREFIX: i32 = 1;
    /// Has a decimal point or an `eNNN` exponent (not an integer literal).
    pub const REAL: i32 = 2;
    /// Value is exactly `0.0` (not an underflow).
    pub const EXACT_ZERO: i32 = 4;
    /// More than ~19 significant digits; excess was truncated.
    pub const MANY_DIGITS: i32 = 8;
}

/// Result of [`atof`] (`sqlite3AtoF`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtoFResult {
    /// Parsed `f64` (sign applied, `±Inf` on overflow, never `NaN`; `0.0` when nothing parsed).
    pub value: f64,
    /// Signed 32-bit return code: `> 0` whole text is a number; `< 0` only a prefix is
    /// (`(-16) | flags`); `== 0` no numeric prefix. Low 4 bits are [`atof_flags`].
    pub rc: i32,
    /// Byte index where parsing stopped before trailing-whitespace skipping.
    pub end: usize,
}

const LIMIT_U64_DIV10: u64 = 1844674407370955160; // (u64::MAX - 9) / 10

/// `sqlite3AtoF` (`util.c:878`): parses `[ws][+-]digits[.digits][(e|E)[+-]digits][ws]`,
/// stopping at the first `0x00` byte.
pub fn atof(bytes: &[u8]) -> AtoFResult {
    let len = bytes.len();
    let mut i = 0usize;
    let byte_at = |idx: usize| -> u8 {
        if idx < len {
            bytes[idx]
        } else {
            0
        }
    };
    let mut c = byte_at(0);
    let mut neg = false;
    let mut s: u64 = 0;
    let mut d: i32 = 0;
    let mut m_state: i32 = 0;

    while is_sql_space(c) {
        i += 1;
        c = byte_at(i);
    }

    if c == b'-' || c == b'+' {
        neg = c == b'-';
        i += 1;
        c = byte_at(i);
    }

    if is_sql_digit(c) {
        m_state = 1;
        s = u64::from(c - b'0');
        i += 1;
        while i < len {
            let dig = bytes[i].wrapping_sub(b'0');
            if dig > 9 {
                break;
            }
            s = s * 10 + u64::from(dig);
            i += 1;
            if s >= LIMIT_U64_DIV10 {
                m_state = 9;
                while i < len && bytes[i].wrapping_sub(b'0') <= 9 {
                    i += 1;
                    d += 1;
                }
                break;
            }
        }
        c = byte_at(i);
    }

    // Decimal point.
    if c == b'.' {
        i += 1;
        c = byte_at(i);
        if is_sql_digit(c) {
            m_state |= 1;
            loop {
                if s < LIMIT_U64_DIV10 {
                    s = s * 10 + u64::from(c - b'0');
                    d -= 1;
                } else {
                    m_state = 11;
                }
                i += 1;
                c = byte_at(i);
                if !is_sql_digit(c) {
                    break;
                }
            }
        } else if m_state == 0 {
            return AtoFResult {
                value: 0.0,
                rc: 0,
                end: i,
            };
        }
        m_state |= 2;
    } else if m_state == 0 {
        return AtoFResult {
            value: 0.0,
            rc: 0,
            end: i,
        };
    }

    // Exponent.
    if c == b'e' || c == b'E' {
        let e_pos = i;
        i += 1;
        c = byte_at(i);
        let mut esign: i32 = 1;
        if c == b'-' {
            esign = -1;
            i += 1;
            c = byte_at(i);
        } else if c == b'+' {
            i += 1;
            c = byte_at(i);
        }
        if is_sql_digit(c) {
            let mut exp = i32::from(c - b'0');
            i += 1;
            c = byte_at(i);
            m_state |= 2;
            while is_sql_digit(c) {
                exp = if exp < 10000 {
                    exp * 10 + i32::from(c - b'0')
                } else {
                    10000
                };
                i += 1;
                c = byte_at(i);
            }
            d += esign * exp;
        } else {
            i = e_pos;
            c = byte_at(i);
        }
    }

    let mut value = if s == 0 {
        m_state |= 4;
        0.0
    } else {
        fp10_convert2(s, d)
    };
    if neg {
        value = -value;
    }

    let end = i;
    if c == 0 {
        return AtoFResult {
            value,
            rc: m_state,
            end,
        };
    }
    if is_sql_space(c) {
        loop {
            i += 1;
            c = byte_at(i);
            if !is_sql_space(c) {
                break;
            }
        }
        if c == 0 {
            return AtoFResult {
                value,
                rc: m_state,
                end,
            };
        }
    }
    AtoFResult {
        value,
        rc: (-16) | m_state,
        end,
    }
}

/// Result of [`atoi64`] (`sqlite3Atoi64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Atoi64Result {
    /// - `-1`: not even a prefix looks like an integer (`value == 0`)
    /// - `0`: success, fits in `i64`
    /// - `1`: excess non-space text after the integer (`value` is the prefix)
    /// - `2`: integer too large for `i64` (saturated to `i64::MAX` / `i64::MIN`)
    /// - `3`: exactly `9223372036854775808` (`value == i64::MAX`)
    pub rc: i8,
    pub value: i64,
}

const POW63_DIGITS: &[u8; 19] = b"9223372036854775808";

/// `sqlite3Atoi64` (`util.c:1173`) over the whole byte slice (an embedded `0x00` is trailing junk).
pub fn atoi64(bytes: &[u8]) -> Atoi64Result {
    let len = bytes.len();
    let mut i = 0usize;
    while i < len && is_sql_space(bytes[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < len {
        if bytes[i] == b'-' {
            neg = true;
            i += 1;
        } else if bytes[i] == b'+' {
            i += 1;
        }
    }
    let z_start = i;
    while i < len && bytes[i] == b'0' {
        i += 1;
    }
    let z_num = i;
    // Single-pass accumulation for the common <= 18-digit case (avoids scanning digits twice).
    let mut u: u64 = 0;
    let fast_end = len.min(z_num + 18);
    while i < fast_end {
        let dig = bytes[i].wrapping_sub(b'0');
        if dig > 9 {
            break;
        }
        u = u * 10 + u64::from(dig);
        i += 1;
    }
    while i < len && bytes[i].wrapping_sub(b'0') <= 9 {
        i += 1;
    }
    let n_digit = i - z_num;

    let mut rc: i8 = 0;
    if n_digit == 0 && z_start == z_num {
        rc = -1;
    } else {
        for &b in &bytes[i..len] {
            if !is_sql_space(b) {
                rc = 1;
                break;
            }
        }
    }

    if n_digit < 19 {
        let value = if neg {
            (u as i64).wrapping_neg()
        } else {
            u as i64
        };
        return Atoi64Result { rc, value };
    }

    let cmp = if n_digit > 19 {
        std::cmp::Ordering::Greater
    } else {
        bytes[z_num..i].cmp(POW63_DIGITS)
    };

    match cmp {
        std::cmp::Ordering::Less => {
            let mut u: u64 = 0;
            for &b in &bytes[z_num..i] {
                u = u * 10 + u64::from(b - b'0');
            }
            let value = if neg { -(u as i64) } else { u as i64 };
            Atoi64Result { rc, value }
        }
        std::cmp::Ordering::Greater => Atoi64Result {
            rc: 2,
            value: if neg { i64::MIN } else { i64::MAX },
        },
        std::cmp::Ordering::Equal => {
            if neg {
                Atoi64Result { rc, value: i64::MIN }
            } else {
                Atoi64Result {
                    rc: 3,
                    value: i64::MAX,
                }
            }
        }
    }
}

#[inline]
fn is_dec_or_hex_span_char(b: u8) -> bool {
    matches!(b, b'+' | b'-' | b' ' | b'\n' | b'\t') || is_sql_digit(b)
}

/// `sqlite3DecOrHexToI64` (`util.c:1276`): integer literal in decimal or `0x` hex.
pub fn dec_or_hex_to_i64(bytes: &[u8]) -> Atoi64Result {
    let len = bytes.len();
    let at = |k: usize| -> u8 { if k < len { bytes[k] } else { 0 } };
    if at(0) == b'0' && (at(1) == b'x' || at(1) == b'X') && is_sql_xdigit(at(2)) {
        let mut i = 2usize;
        while at(i) == b'0' {
            i += 1;
        }
        let mut k = i;
        let mut u: u64 = 0;
        while is_sql_xdigit(at(k)) {
            u = u.wrapping_shl(4).wrapping_add(u64::from(hex_to_u8(at(k))));
            k += 1;
        }
        let value = u as i64;
        if k - i > 16 {
            return Atoi64Result { rc: 2, value };
        }
        if at(k) != 0 {
            return Atoi64Result { rc: 1, value };
        }
        return Atoi64Result { rc: 0, value };
    }
    let mut n = 0usize;
    while n < len && is_dec_or_hex_span_char(bytes[n]) {
        n += 1;
    }
    if at(n) != 0 {
        n += 1;
    }
    atoi64(&bytes[..n])
}

/// `sqlite3GetInt32` (`util.c:1310`): parse a leading 32-bit signed integer (decimal or `0x` hex
/// up to `0x7fffffff`). Leading whitespace is not skipped; trailing bytes are ignored.
pub fn get_int32(bytes: &[u8]) -> Option<i32> {
    let len = bytes.len();
    let at = |k: usize| -> u8 { if k < len { bytes[k] } else { 0 } };
    let mut p = 0usize;
    let mut neg = false;
    if at(0) == b'-' {
        neg = true;
        p = 1;
    } else if at(0) == b'+' {
        p = 1;
    } else if at(0) == b'0' && (at(1) == b'x' || at(1) == b'X') && is_sql_xdigit(at(2)) {
        p = 2;
        while at(p) == b'0' {
            p += 1;
        }
        let mut u: u32 = 0;
        let mut i = 0usize;
        while i < 8 && is_sql_xdigit(at(p + i)) {
            u = (u << 4) | u32::from(hex_to_u8(at(p + i)));
            i += 1;
        }
        if (u & 0x8000_0000) == 0 && !is_sql_xdigit(at(p + i)) {
            return Some(u as i32);
        }
        return None;
    }
    if !is_sql_digit(at(p)) {
        return None;
    }
    while at(p) == b'0' {
        p += 1;
    }
    let mut v: i64 = 0;
    let mut i = 0usize;
    while i < 11 && is_sql_digit(at(p + i)) {
        v = v * 10 + i64::from(at(p + i) - b'0');
        i += 1;
    }
    if i > 10 {
        return None;
    }
    if v - i64::from(neg) > i64::from(i32::MAX) {
        return None;
    }
    Some(if neg { (-v) as i32 } else { v as i32 })
}
