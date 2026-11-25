//! 64-bit integer helpers with SQLite 3.54.1 semantics.

/// The largest `f64` strictly below `2^63` (`sqlite3RealToI64`'s saturation threshold,
/// `vdbemem.c:880`).
pub const I64_SAT: f64 = 9223372036854774784.0;

/// `sqlite3RealToI64` (`vdbemem.c:879`): truncate toward zero, saturating to
/// `i64::MIN` / `i64::MAX` beyond `±9223372036854774784.0` (so `±Inf` saturate).
/// `-0.0` gives `0`; `NaN` maps to `0`.
#[inline]
pub fn real_to_i64(r: f64) -> i64 {
    if r < -I64_SAT {
        return i64::MIN;
    }
    if r > I64_SAT {
        return i64::MAX;
    }
    if r.is_nan() {
        return 0;
    }
    r as i64
}

/// `(double)i64`: round to nearest, ties to even.
#[inline]
pub fn i64_to_real(i: i64) -> f64 {
    i as f64
}

/// `sqlite3RealSameAsInt` (`vdbemem.c:868`): `r` is `0.0` (either sign), or `r` is
/// bit-identical to `(double)i` and `i` is in `[-2^51, 2^51)`.
/// Intended for `i == real_to_i64(r)`.
#[inline]
pub fn real_same_as_int(r: f64, i: i64) -> bool {
    if r == 0.0 {
        return true;
    }
    (-2251799813685248..2251799813685248).contains(&i) && r.to_bits() == (i as f64).to_bits()
}

/// The integer that `sqlite3VdbeIntegerAffinity` (`vdbemem.c:802`) turns a `REAL`
/// into, or `None` if the `REAL` stays `REAL`: `r` must equal its truncation and
/// the truncation must not be `i64::MIN` or `i64::MAX` (ticket #3922).
#[inline]
pub fn real_to_integer_affinity(r: f64) -> Option<i64> {
    let ix = real_to_i64(r);
    if ix == i64::MIN || ix == i64::MAX {
        return None;
    }
    if r == (ix as f64) {
        Some(ix)
    } else {
        None
    }
}

/// `OP_MustBeInt`'s `REAL` rule (`vdbe.c:2170`): the integer equal to `r` when `r` is
/// integral and in `[-2^63, 2^63)`, else `None`.
#[inline]
pub fn real_to_exact_i64(r: f64) -> Option<i64> {
    if !(-9223372036854775808.0..9223372036854775808.0).contains(&r) {
        return None;
    }
    let ix = real_to_i64(r);
    if (ix as f64) == r {
        Some(ix)
    } else {
        None
    }
}

/// `sqlite3AddInt64` (`util.c:1951`): `a + b`, or `None` on `i64` overflow.
#[inline]
pub fn add_i64(a: i64, b: i64) -> Option<i64> {
    a.checked_add(b)
}

/// `sqlite3SubInt64` (`util.c:1971`): `a - b`, or `None` on `i64` overflow.
#[inline]
pub fn sub_i64(a: i64, b: i64) -> Option<i64> {
    a.checked_sub(b)
}

/// `sqlite3MulInt64` (`util.c:1986`): `a * b`, or `None` on `i64` overflow.
#[inline]
pub fn mul_i64(a: i64, b: i64) -> Option<i64> {
    a.checked_mul(b)
}

/// C's `i64` `a / b` (truncating toward zero) for `b != 0` and not `(i64::MIN / -1)`.
#[inline]
pub fn div_i64(a: i64, b: i64) -> Option<i64> {
    a.checked_div(b)
}

/// C's `i64` `a % b` (sign of the dividend) for `b != 0`; handles `b == -1` as `0`.
#[inline]
pub fn rem_i64(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    if b == -1 {
        return Some(0);
    }
    Some(a % b)
}

const DEC_DIGITS_LUT: &[u8; 200] = b"\
    0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

/// Stack-allocated `i64 -> ASCII` formatter (2 digits per division, zero heap allocations).
#[inline]
pub fn int64_to_text_buf(v: i64, buf: &mut [u8; 24]) -> &[u8] {
    let neg = v < 0;
    let mut n = v.unsigned_abs();
    let mut pos = buf.len();
    while n >= 100 {
        let rem = ((n % 100) as usize) * 2;
        n /= 100;
        pos -= 2;
        buf[pos] = DEC_DIGITS_LUT[rem];
        buf[pos + 1] = DEC_DIGITS_LUT[rem + 1];
    }
    if n >= 10 {
        let rem = (n as usize) * 2;
        pos -= 2;
        buf[pos] = DEC_DIGITS_LUT[rem];
        buf[pos + 1] = DEC_DIGITS_LUT[rem + 1];
    } else {
        pos -= 1;
        buf[pos] = b'0' + (n as u8);
    }
    if neg {
        pos -= 1;
        buf[pos] = b'-';
    }
    &buf[pos..]
}

/// `sqlite3Int64ToText` (`util.c:1088`): plain decimal with a leading `'-'` for negatives.
#[inline]
pub fn int64_to_text(v: i64) -> String {
    let mut buf = [0u8; 24];
    let s = int64_to_text_buf(v, &mut buf);
    // SAFETY-free: `DEC_DIGITS_LUT`, `'0'..='9'`, `'-'` are strictly ASCII.
    std::str::from_utf8(s).expect("ASCII").to_string()
}
