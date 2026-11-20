//! SQLite 3.54.1 date and time functions (`date.c`).
//!
//! Direct port of SQLite's Julian-day millisecond `DateTime` representation (`i_jd: i64`),
//! parsing (`parseYyyyMmDd`, `parseHhMmSs`, `parseTimezone`), modifiers (`auto`, `ceiling`,
//! `floor`, `julianday`, `unixepoch`, `start of ...`, `end of ...`, `weekday N`, `subsec`,
//! `localtime`, `utc`, `+/-YYYY-MM-DD`, `+/-HH:MM:SS`, `+/-NNN units`), and rendering
//! (`date`, `time`, `datetime`, `julianday`, `unixepoch`, `strftime`, `timediff`).

use xdb_core::value::{
    atof, format_float, int64_to_text, real_same_as_int, FloatConversion, FloatFormatSpec,
    SqlValue,
};

use crate::errors::{SqlError, SqlResult};
use crate::functions::FunctionContext;
use crate::utf8::{ascii_lower_byte, c_str_len, text_bytes_of};

/// `INT_464269060799999`: `iJD` of `9999-12-31 23:59:59.999`.
const MAX_IJD: i64 = 464_269_060_799_999;
/// `1970-01-01 00:00:00` in `iJD` milliseconds.
const UNIX_EPOCH_JD_MS: i64 = 210_866_760_000_000;
const UNIX_EPOCH_JD_S: i64 = 210_866_760_000;

#[derive(Debug, Clone, Default)]
struct DateTime {
    i_jd: i64,
    y: i32,
    m: i32,
    d: i32,
    h: i32,
    min: i32,
    tz: i32,
    s: f64,
    valid_jd: bool,
    valid_ymd: bool,
    valid_hms: bool,
    n_floor: i32,
    raw_s: bool,
    is_error: bool,
    use_subsec: bool,
    is_utc: bool,
    is_local: bool,
}

impl DateTime {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn set_error(&mut self) {
        self.clear();
        self.is_error = true;
    }

    fn clear_ymd_hms_tz(&mut self) {
        self.valid_ymd = false;
        self.valid_hms = false;
        self.tz = 0;
    }
}

#[inline]
fn i64_from_double(x: f64) -> i64 {
    if x.is_nan() {
        0
    } else if x >= 9_223_372_036_854_775_808.0 {
        i64::MAX
    } else if x <= -9_223_372_036_854_775_808.0 {
        i64::MIN
    } else {
        x as i64
    }
}

#[inline]
fn c_int(x: f64) -> i32 {
    if x.is_nan() {
        0
    } else if x >= 2_147_483_647.0 {
        i32::MAX
    } else if x <= -2_147_483_648.0 {
        i32::MIN
    } else {
        x as i32
    }
}

#[inline]
fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

#[inline]
fn is_space(c: u8) -> bool {
    c == b' ' || (0x09..=0x0d).contains(&c)
}

fn starts_with_i(z: &[u8], from: usize, lit_lower: &[u8]) -> bool {
    for (k, &b) in lit_lower.iter().enumerate() {
        if ascii_lower_byte(at(z, from + k)) != b {
            return false;
        }
    }
    true
}

const DIGIT_MAX: [i32; 6] = [12, 14, 24, 31, 59, 14712];

/// `getDigits` (`date.c:112`).
fn get_digits(z: &[u8], mut i: usize, fmt: &[u8], out: &mut [i32; 3]) -> usize {
    let mut cnt = 0;
    let mut f = 0;
    loop {
        let mut n = (fmt[f] - b'0') as usize;
        let min = i32::from(fmt[f + 1] - b'0');
        let max = DIGIT_MAX[(fmt[f + 2] - b'a') as usize];
        let next_c = fmt.get(f + 3).copied().unwrap_or(0);
        let mut val = 0i32;
        while n > 0 {
            let c = at(z, i);
            if !c.is_ascii_digit() {
                return cnt;
            }
            val = val * 10 + i32::from(c - b'0');
            i += 1;
            n -= 1;
        }
        if val < min || val > max || (next_c != 0 && next_c != at(z, i)) {
            return cnt;
        }
        out[cnt] = val;
        i += 1;
        cnt += 1;
        f += 4;
        if next_c == 0 {
            return cnt;
        }
    }
}

fn parse_timezone(z: &[u8], mut i: usize, p: &mut DateTime) -> bool {
    while is_space(at(z, i)) {
        i += 1;
    }
    p.tz = 0;
    let c = at(z, i);
    let sgn = match c {
        b'-' => -1,
        b'+' => 1,
        b'Z' | b'z' => {
            i += 1;
            p.is_local = false;
            p.is_utc = true;
            while is_space(at(z, i)) {
                i += 1;
            }
            return at(z, i) != 0;
        }
        _ => return c != 0,
    };
    i += 1;
    let mut dig = [0i32; 3];
    if get_digits(z, i, b"20b:20e", &mut dig) != 2 {
        return true;
    }
    i += 5;
    p.tz = sgn * (dig[1] + dig[0] * 60);
    if p.tz == 0 {
        p.is_local = false;
        p.is_utc = true;
    }
    while is_space(at(z, i)) {
        i += 1;
    }
    at(z, i) != 0
}

fn parse_hh_mm_ss(z: &[u8], mut i: usize, p: &mut DateTime) -> bool {
    let mut dig = [0i32; 3];
    if get_digits(z, i, b"20c:20e", &mut dig) != 2 {
        return true;
    }
    let h = dig[0];
    let min = dig[1];
    let s;
    let mut ms = 0.0;
    i += 5;
    if at(z, i) == b':' {
        i += 1;
        if get_digits(z, i, b"20e", &mut dig) != 1 {
            return true;
        }
        s = dig[0];
        i += 2;
        if at(z, i) == b'.' && at(z, i + 1).is_ascii_digit() {
            let mut r_scale = 1.0;
            i += 1;
            while at(z, i).is_ascii_digit() {
                ms = ms * 10.0 + f64::from(at(z, i) - b'0');
                r_scale *= 10.0;
                i += 1;
            }
            ms /= r_scale;
            if ms > 0.999 {
                ms = 0.999;
            }
        }
    } else {
        s = 0;
    }
    if h == 24 && (min != 0 || s != 0 || ms != 0.0) {
        return true;
    }
    p.valid_jd = false;
    p.raw_s = false;
    p.valid_hms = true;
    p.h = h;
    p.min = min;
    p.s = f64::from(s) + ms;
    parse_timezone(z, i, p)
}

fn days_of_month(y: i32, m: i32) -> i32 {
    if (1u32 << m) & 0x15aa != 0 {
        return 31;
    }
    if m != 2 {
        return 30;
    }
    if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
        29
    } else {
        28
    }
}

fn compute_floor(p: &mut DateTime) {
    let mx_d = days_of_month(p.y, p.m);
    p.n_floor = if p.d > mx_d { p.d - mx_d } else { 0 };
}

fn compute_jd(p: &mut DateTime) {
    if p.valid_jd {
        return;
    }
    let (mut y, mut m, d) = if p.valid_ymd {
        (p.y, p.m, p.d)
    } else {
        let mut d = 1;
        if p.valid_hms && p.h == 24 {
            p.h = 0;
            d = 2;
        }
        (2000, 1, d)
    };
    if !(-4713..=9999).contains(&y) || p.raw_s {
        p.set_error();
        return;
    }
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = (y + 4800) / 100;
    let b = 38 - a + (a / 4);
    let x1 = (36525 * (y + 4716)) / 100;
    let x2 = (306001 * (m + 1)) / 10000;
    let mut i_jd = i64_from_double((f64::from(x1 + x2 + d + b) - 1524.5) * 86_400_000.0);
    p.valid_jd = true;
    if p.valid_hms {
        i_jd = i_jd.wrapping_add(
            i64::from(p.h) * 3_600_000
                + i64::from(p.min) * 60_000
                + i64_from_double(p.s * 1000.0 + 0.5),
        );
        if p.tz != 0 {
            i_jd = i_jd.wrapping_sub(i64::from(p.tz) * 60_000);
            p.valid_ymd = false;
            p.valid_hms = false;
            p.tz = 0;
            p.is_utc = true;
            p.is_local = false;
        }
    }
    p.i_jd = i_jd;
}

fn parse_yyyy_mm_dd(z: &[u8], p: &mut DateTime) -> bool {
    let mut i = 0;
    let mut neg = false;
    if at(z, 0) == b'-' {
        i = 1;
        neg = true;
    }
    let mut dig = [0i32; 3];
    if get_digits(z, i, b"40f-21a-21d", &mut dig) != 3 {
        return true;
    }
    let mut y = dig[0];
    let mut m = dig[1];
    let mut d = dig[2];
    i += 10;
    while is_space(at(z, i)) || at(z, i) == b'T' {
        i += 1;
    }
    if !parse_hh_mm_ss(z, i, p) {
        if p.h == 24 {
            let mx_d = days_of_month(y, m);
            p.h = 0;
            d += 1;
            if d > mx_d {
                d -= mx_d;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        }
    } else if at(z, i) == 0 {
        p.valid_hms = false;
    } else {
        return true;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -y } else { y };
    p.m = m;
    p.d = d;
    compute_floor(p);
    if p.tz != 0 {
        compute_jd(p);
    }
    false
}

fn set_datetime_to_current(ctx: &FunctionContext<'_>, p: &mut DateTime) -> bool {
    let now = ctx.now_julian_ms;
    if now > 0 {
        p.i_jd = now;
        p.valid_jd = true;
        p.is_utc = true;
        p.is_local = false;
        p.clear_ymd_hms_tz();
        false
    } else {
        true
    }
}

fn set_raw_date_number(p: &mut DateTime, r: f64) {
    p.s = r;
    p.raw_s = true;
    if (0.0..5_373_484.5).contains(&r) {
        p.i_jd = i64_from_double(r * 86_400_000.0 + 0.5);
        p.valid_jd = true;
    }
}

fn parse_date_or_time(
    ctx: &FunctionContext<'_>,
    name: &str,
    z: &[u8],
    p: &mut DateTime,
) -> SqlResult<bool> {
    if !parse_yyyy_mm_dd(z, p) {
        return Ok(false);
    }
    if !parse_hh_mm_ss(z, 0, p) {
        return Ok(false);
    }
    if z.eq_ignore_ascii_case(b"now") {
        ctx.require_pure(name)?;
        return Ok(set_datetime_to_current(ctx, p));
    }
    let r = atof(z);
    if r.rc > 0 {
        set_raw_date_number(p, r.value);
        return Ok(false);
    }
    if z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond") {
        ctx.require_pure(name)?;
        p.use_subsec = true;
        return Ok(set_datetime_to_current(ctx, p));
    }
    Ok(true)
}

#[inline]
fn valid_julian_day(i_jd: i64) -> bool {
    (0..=MAX_IJD).contains(&i_jd)
}

fn compute_ymd(p: &mut DateTime) {
    if p.valid_ymd {
        return;
    }
    if !p.valid_jd {
        p.y = 2000;
        p.m = 1;
        p.d = 1;
    } else if !valid_julian_day(p.i_jd) {
        p.set_error();
        return;
    } else {
        let z = ((p.i_jd + 43_200_000) / 86_400_000) as i32;
        let alpha = ((f64::from(z) + 32044.75) / 36524.25) as i32 - 52;
        let a = z + 1 + alpha - ((alpha + 100) / 4) + 25;
        let b = a + 1524;
        let c = ((f64::from(b) - 122.1) / 365.25) as i32;
        let d = (36525 * (c & 32767)) / 100;
        let e = (f64::from(b - d) / 30.6001) as i32;
        let x1 = (30.6001 * f64::from(e)) as i32;
        p.d = b - d - x1;
        p.m = if e < 14 { e - 1 } else { e - 13 };
        p.y = if p.m > 2 { c - 4716 } else { c - 4715 };
    }
    p.valid_ymd = true;
}

fn compute_hms(p: &mut DateTime) {
    if p.valid_hms {
        return;
    }
    compute_jd(p);
    let day_ms = (p.i_jd.wrapping_add(43_200_000) % 86_400_000) as i32;
    p.s = f64::from(day_ms % 60_000) / 1000.0;
    let day_min = day_ms / 60_000;
    p.min = day_min % 60;
    p.h = day_min / 60;
    p.raw_s = false;
    p.valid_hms = true;
}

fn compute_ymd_hms(p: &mut DateTime) {
    compute_ymd(p);
    compute_hms(p);
}

fn auto_adjust_date(p: &mut DateTime) {
    if !p.raw_s || p.valid_jd {
        p.raw_s = false;
    } else if p.s >= -210_866_760_000.0 && p.s <= 253_402_300_799.0 {
        let r = p.s * 1000.0 + 210_866_760_000_000.0;
        p.clear_ymd_hms_tz();
        p.i_jd = i64_from_double(r + 0.5);
        p.valid_jd = true;
        p.raw_s = false;
    }
}

const XFORM_NAME: [&[u8]; 6] = [b"second", b"minute", b"hour", b"day", b"month", b"year"];
const XFORM_LIMIT: [f64; 6] = [
    4.6427e14_f32 as f64,
    7.7379e12_f32 as f64,
    1.2897e11_f32 as f64,
    5_373_485.0_f32 as f64,
    176_546.0_f32 as f64,
    14_713.0_f32 as f64,
];
const XFORM_SECS: [f64; 6] = [1.0, 60.0, 3600.0, 86400.0, 2_592_000.0, 31_536_000.0];

fn parse_modifier(
    ctx: &FunctionContext<'_>,
    name: &str,
    z: &[u8],
    p: &mut DateTime,
    idx: usize,
) -> SqlResult<bool> {
    let mut rc = true;
    match ascii_lower_byte(at(z, 0)) {
        b'a' => {
            if z.eq_ignore_ascii_case(b"auto") {
                if idx > 1 {
                    return Ok(true);
                }
                auto_adjust_date(p);
                rc = false;
            }
        }
        b'c' => {
            if z.eq_ignore_ascii_case(b"ceiling") {
                compute_jd(p);
                p.clear_ymd_hms_tz();
                p.n_floor = 0;
                rc = false;
            }
        }
        b'e' => {
            if starts_with_i(z, 0, b"end of ") && (p.valid_jd || p.valid_ymd || p.valid_hms) {
                let unit = &z[7..];
                compute_ymd(p);
                p.valid_hms = true;
                p.h = 23;
                p.min = 59;
                p.s = 59.999;
                p.raw_s = false;
                p.tz = 0;
                p.valid_jd = false;
                if unit.eq_ignore_ascii_case(b"month") {
                    p.d = 1;
                    p.m += 1;
                    if p.m > 12 {
                        p.y += 1;
                        p.m = 1;
                    }
                    compute_floor(p);
                    compute_jd(p);
                    p.i_jd = p.i_jd.wrapping_sub(i64::from(p.n_floor + 1) * 86_400_000);
                    p.clear_ymd_hms_tz();
                    rc = false;
                } else if unit.eq_ignore_ascii_case(b"year") {
                    p.m = 12;
                    p.d = 31;
                    rc = false;
                } else if unit.eq_ignore_ascii_case(b"day") {
                    rc = false;
                }
            }
        }
        b'f' => {
            if z.eq_ignore_ascii_case(b"floor") {
                compute_jd(p);
                p.i_jd = p.i_jd.wrapping_sub(i64::from(p.n_floor) * 86_400_000);
                p.clear_ymd_hms_tz();
                rc = false;
            }
        }
        b'j' => {
            if z.eq_ignore_ascii_case(b"julianday") {
                if idx > 1 {
                    return Ok(true);
                }
                if p.valid_jd && p.raw_s {
                    rc = false;
                    p.raw_s = false;
                }
            }
        }
        b'l' => {
            // Deterministic sandbox localtime: applies configured `local_tz_offset_minutes` (default 0 = UTC).
            if z.eq_ignore_ascii_case(b"localtime") {
                ctx.require_pure(name)?;
                if !p.is_local {
                    compute_jd(p);
                    p.i_jd = p
                        .i_jd
                        .wrapping_add(i64::from(ctx.local_tz_offset_minutes) * 60_000);
                    p.clear_ymd_hms_tz();
                }
                p.is_utc = false;
                p.is_local = true;
                rc = false;
            }
        }
        b'u' => {
            if z.eq_ignore_ascii_case(b"unixepoch") && p.raw_s {
                if idx > 1 {
                    return Ok(true);
                }
                let r = p.s * 1000.0 + 210_866_760_000_000.0;
                if (0.0..464_269_060_800_000.0).contains(&r) {
                    p.clear_ymd_hms_tz();
                    p.i_jd = i64_from_double(r + 0.5);
                    p.valid_jd = true;
                    p.raw_s = false;
                    rc = false;
                }
            } else if z.eq_ignore_ascii_case(b"utc") {
                ctx.require_pure(name)?;
                if !p.is_utc {
                    compute_jd(p);
                    p.i_jd = p
                        .i_jd
                        .wrapping_sub(i64::from(ctx.local_tz_offset_minutes) * 60_000);
                    p.clear_ymd_hms_tz();
                }
                p.is_utc = true;
                p.is_local = false;
                rc = false;
            }
        }
        b'w' => {
            if starts_with_i(z, 0, b"weekday ") {
                let tail = &z[8..];
                let a = atof(tail);
                let r = a.value;
                if a.rc > 0 && (-6.0..=6.0).contains(&r) {
                    let mut n = r as i32;
                    if real_same_as_int(r, i64::from(n)) {
                        compute_ymd_hms(p);
                        p.tz = 0;
                        p.valid_jd = false;
                        compute_jd(p);
                        let mut wd = (((p.i_jd + 129_600_000) / 86_400_000) % 7) as i32;
                        if n < 0 {
                            n = -n;
                        }
                        if wd != n {
                            if wd > n {
                                wd -= 7;
                            }
                            p.i_jd = p.i_jd.wrapping_add(i64::from(n - wd) * 86_400_000);
                            if tail.contains(&b'-') {
                                p.i_jd = p.i_jd.wrapping_sub(7 * 86_400_000);
                            }
                        }
                        p.clear_ymd_hms_tz();
                        rc = false;
                    }
                }
            }
        }
        b's' => {
            if starts_with_i(z, 0, b"start of ") {
                if p.valid_jd || p.valid_ymd || p.valid_hms {
                    let unit = &z[9..];
                    compute_ymd(p);
                    p.valid_hms = true;
                    p.h = 0;
                    p.min = 0;
                    p.s = 0.0;
                    p.raw_s = false;
                    p.tz = 0;
                    p.valid_jd = false;
                    if unit.eq_ignore_ascii_case(b"month") {
                        p.d = 1;
                        rc = false;
                    } else if unit.eq_ignore_ascii_case(b"year") {
                        p.m = 1;
                        p.d = 1;
                        rc = false;
                    } else if unit.eq_ignore_ascii_case(b"day") {
                        rc = false;
                    }
                }
            } else if z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond") {
                p.use_subsec = true;
                rc = false;
            }
        }
        b'+' | b'-' | b'0'..=b'9' => {
            rc = numeric_modifier(z, p);
        }
        _ => {}
    }
    Ok(rc)
}

fn numeric_modifier(mut z: &[u8], p: &mut DateTime) -> bool {
    let mut dig = [0i32; 3];
    let mut n = 1usize;
    while at(z, n) != 0 {
        let c = at(z, n);
        if c == b':' || is_space(c) {
            break;
        }
        if c == b'-' {
            if n == 5 && get_digits(z, 1, b"40f", &mut dig) == 1 {
                break;
            }
            if n == 6 && get_digits(z, 1, b"50f", &mut dig) == 1 {
                break;
            }
        }
        n += 1;
    }
    let pre = atof(&z[..n]);
    if pre.rc <= 0 {
        return true;
    }
    let mut r = pre.value;
    let z0 = at(z, 0);
    let mut z2 = z;
    let mut z2_off = 0usize;

    if at(z, n) == b'-' {
        if z0 != b'+' && z0 != b'-' {
            return true;
        }
        if n == 5 {
            if get_digits(z, 1, b"40f-20a-20d", &mut dig) != 3 {
                return true;
            }
        } else {
            if get_digits(z, 1, b"50f-20a-20d", &mut dig) != 3 {
                return true;
            }
            z = &z[1..];
        }
        let y = dig[0];
        let m = dig[1];
        let mut d = dig[2];
        if m >= 12 || d >= 31 {
            return true;
        }
        compute_ymd_hms(p);
        p.valid_jd = false;
        if z0 == b'-' {
            p.y -= y;
            p.m -= m;
            d = -d;
        } else {
            p.y += y;
            p.m += m;
        }
        let x = if p.m > 0 {
            (p.m - 1) / 12
        } else {
            (p.m - 12) / 12
        };
        p.y += x;
        p.m -= x * 12;
        compute_floor(p);
        compute_jd(p);
        p.valid_hms = false;
        p.valid_ymd = false;
        p.i_jd = p.i_jd.wrapping_add(i64::from(d) * 86_400_000);
        if at(z, 11) == 0 {
            return false;
        }
        if is_space(at(z, 11)) && get_digits(z, 12, b"20c:20e", &mut dig) == 2 {
            z2 = z;
            z2_off = 12;
            n = 2;
        } else {
            return true;
        }
    }

    if at(z2, z2_off + n) == b':' {
        if !at(z2, z2_off).is_ascii_digit() {
            z2_off += 1;
        }
        let mut tx = DateTime::default();
        if parse_hh_mm_ss(z2, z2_off, &mut tx) {
            return true;
        }
        compute_jd(&mut tx);
        let mut tx_jd = tx.i_jd.wrapping_sub(43_200_000);
        let day = tx_jd / 86_400_000;
        tx_jd = tx_jd.wrapping_sub(day * 86_400_000);
        if z0 == b'-' {
            tx_jd = tx_jd.wrapping_neg();
        }
        compute_jd(p);
        p.clear_ymd_hms_tz();
        p.i_jd = p.i_jd.wrapping_add(tx_jd);
        return false;
    }

    let mut k = n;
    while is_space(at(z, k)) {
        k += 1;
    }
    let unit = &z[k..];
    let unit = &unit[..c_str_len(unit)];
    let mut un = unit.len();
    if !(3..=10).contains(&un) {
        return true;
    }
    if ascii_lower_byte(unit[un - 1]) == b's' {
        un -= 1;
    }
    compute_jd(p);
    let r_rounder = if r < 0.0 { -0.5 } else { 0.5 };
    p.n_floor = 0;
    let mut rc = true;
    for i in 0..XFORM_NAME.len() {
        let xname = XFORM_NAME[i];
        let lim = XFORM_LIMIT[i];
        if xname.len() == un && starts_with_i(unit, 0, &xname[..un]) && r > -lim && r < lim {
            if i == 4 {
                compute_ymd_hms(p);
                p.m += r.trunc() as i32;
                let x = if p.m > 0 {
                    (p.m - 1) / 12
                } else {
                    (p.m - 12) / 12
                };
                p.y += x;
                p.m -= x * 12;
                compute_floor(p);
                p.valid_jd = false;
                r -= r.trunc();
            } else if i == 5 {
                let y = r.trunc() as i32;
                compute_ymd_hms(p);
                p.y += y;
                compute_floor(p);
                p.valid_jd = false;
                r -= r.trunc();
            }
            compute_jd(p);
            p.i_jd = p
                .i_jd
                .wrapping_add(i64_from_double(r * 1000.0 * XFORM_SECS[i] + r_rounder));
            rc = false;
            break;
        }
    }
    p.clear_ymd_hms_tz();
    rc
}

fn is_date(
    ctx: &FunctionContext<'_>,
    name: &str,
    args: &[SqlValue],
    from: usize,
    p: &mut DateTime,
) -> SqlResult<bool> {
    let argc = args.len().saturating_sub(from);
    p.clear();
    if argc == 0 {
        ctx.require_pure(name)?;
        return Ok(set_datetime_to_current(ctx, p));
    }
    match &args[from] {
        SqlValue::Int(i) => set_raw_date_number(p, *i as f64),
        SqlValue::Real(r) => set_raw_date_number(p, *r),
        other => {
            let Some(tb) = text_bytes_of(other) else {
                return Ok(true);
            };
            let z = &tb[..c_str_len(&tb)];
            if parse_date_or_time(ctx, name, z, p)? {
                return Ok(true);
            }
        }
    }
    for i in 1..argc {
        let Some(tb) = text_bytes_of(&args[from + i]) else {
            return Ok(true);
        };
        let z = &tb[..c_str_len(&tb)];
        if parse_modifier(ctx, name, z, p, i)? {
            return Ok(true);
        }
    }
    compute_jd(p);
    if p.is_error || !valid_julian_day(p.i_jd) {
        return Ok(true);
    }
    if argc == 1 && p.valid_ymd && p.d > 28 {
        p.valid_ymd = false;
    }
    Ok(false)
}

fn fmt_int(v: i32, width: usize, zero_pad: bool) -> String {
    let neg = v < 0;
    let abs = i64::from(v).unsigned_abs();
    let mut digits = abs.to_string();
    if zero_pad {
        let prec = width.saturating_sub(usize::from(neg));
        if digits.len() < prec {
            let mut z = "0".repeat(prec - digits.len());
            z.push_str(&digits);
            digits = z;
        }
    }
    let out = if neg { format!("-{digits}") } else { digits };
    if out.len() < width {
        format!("{}{out}", " ".repeat(width - out.len()))
    } else {
        out
    }
}

fn two(v: i32) -> String {
    fmt_int(v, 2, true)
}

fn digit_char(v: i32) -> char {
    (b'0' + (v % 10) as u8) as char
}

fn ymd_text(x: &DateTime) -> String {
    let y = x.y.abs();
    let mut out = String::with_capacity(11);
    if x.y < 0 {
        out.push('-');
    }
    out.push(digit_char(y / 1000));
    out.push(digit_char(y / 100));
    out.push(digit_char(y / 10));
    out.push(digit_char(y));
    out.push('-');
    out.push(digit_char(x.m / 10));
    out.push(digit_char(x.m));
    out.push('-');
    out.push(digit_char(x.d / 10));
    out.push(digit_char(x.d));
    out
}

fn hms_text(x: &DateTime) -> String {
    let mut out = String::with_capacity(12);
    out.push(digit_char(x.h / 10));
    out.push(digit_char(x.h));
    out.push(':');
    out.push(digit_char(x.min / 10));
    out.push(digit_char(x.min));
    out.push(':');
    if x.use_subsec {
        let s = c_int(1000.0 * x.s + 0.5);
        out.push(digit_char(s / 10_000));
        out.push(digit_char(s / 1_000));
        out.push('.');
        out.push(digit_char(s / 100));
        out.push(digit_char(s / 10));
        out.push(digit_char(s));
    } else {
        let s = c_int(x.s);
        out.push(digit_char(s / 10));
        out.push(digit_char(s));
    }
    out
}

fn days_after_jan01(p: &DateTime) -> i32 {
    let mut jan01 = p.clone();
    jan01.valid_jd = false;
    jan01.m = 1;
    jan01.d = 1;
    compute_jd(&mut jan01);
    ((p.i_jd - jan01.i_jd + 43_200_000) / 86_400_000) as i32
}

fn days_after_monday(p: &DateTime) -> i32 {
    (((p.i_jd + 43_200_000) / 86_400_000) % 7) as i32
}

fn days_after_sunday(p: &DateTime) -> i32 {
    (((p.i_jd + 129_600_000) / 86_400_000) % 7) as i32
}

fn iso_thursday(x: &DateTime) -> DateTime {
    let mut y = x.clone();
    y.i_jd = y
        .i_jd
        .wrapping_add(i64::from(3 - days_after_monday(x)) * 86_400_000);
    y.valid_ymd = false;
    compute_ymd(&mut y);
    y
}

/// Dispatch any SQLite date/time function by lowercase ASCII name.
/// Returns `Ok(None)` if not a date/time function.
pub fn call_date(
    lower_name: &str,
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<SqlValue>> {
    let val = match lower_name {
        "julianday" => {
            let mut x = DateTime::default();
            if is_date(ctx, "julianday", args, 0, &mut x)? {
                SqlValue::Null
            } else {
                compute_jd(&mut x);
                SqlValue::real((x.i_jd as f64) / 86_400_000.0)
            }
        }
        "unixepoch" => {
            let mut x = DateTime::default();
            if is_date(ctx, "unixepoch", args, 0, &mut x)? {
                SqlValue::Null
            } else {
                compute_jd(&mut x);
                if x.use_subsec {
                    SqlValue::real(((x.i_jd - UNIX_EPOCH_JD_MS) as f64) / 1000.0)
                } else {
                    SqlValue::Int((x.i_jd / 1000) - UNIX_EPOCH_JD_S)
                }
            }
        }
        "datetime" => datetime_named(args, ctx, "datetime")?,
        "time" => time_named(args, ctx, "time")?,
        "date" => date_named(args, ctx, "date")?,
        "current_time" => {
            if !args.is_empty() {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function current_time()",
                ));
            }
            time_named(&[], ctx, "current_time")?
        }
        "current_date" => {
            if !args.is_empty() {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function current_date()",
                ));
            }
            date_named(&[], ctx, "current_date")?
        }
        "current_timestamp" => {
            if !args.is_empty() {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function current_timestamp()",
                ));
            }
            datetime_named(&[], ctx, "current_timestamp")?
        }
        "strftime" => strftime_fn(args, ctx)?,
        "timediff" => {
            if args.len() != 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function timediff()",
                ));
            }
            timediff_fn(&args[0], &args[1], ctx)?
        }
        _ => return Ok(None),
    };
    Ok(Some(val))
}

pub fn date_named(args: &[SqlValue], ctx: &FunctionContext<'_>, name: &str) -> SqlResult<SqlValue> {
    let mut x = DateTime::default();
    if is_date(ctx, name, args, 0, &mut x)? {
        return Ok(SqlValue::Null);
    }
    compute_ymd(&mut x);
    Ok(SqlValue::text(ymd_text(&x)))
}

pub fn time_named(args: &[SqlValue], ctx: &FunctionContext<'_>, name: &str) -> SqlResult<SqlValue> {
    let mut x = DateTime::default();
    if is_date(ctx, name, args, 0, &mut x)? {
        return Ok(SqlValue::Null);
    }
    compute_hms(&mut x);
    Ok(SqlValue::text(hms_text(&x)))
}

pub fn datetime_named(
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
    name: &str,
) -> SqlResult<SqlValue> {
    let mut x = DateTime::default();
    if is_date(ctx, name, args, 0, &mut x)? {
        return Ok(SqlValue::Null);
    }
    compute_ymd_hms(&mut x);
    Ok(SqlValue::text(format!("{} {}", ymd_text(&x), hms_text(&x))))
}

fn strftime_fn(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    if args.is_empty() {
        return Ok(SqlValue::Null);
    }
    let Some(fmt_tb) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    let z_fmt = &fmt_tb[..c_str_len(&fmt_tb)];
    let mut x = DateTime::default();
    if is_date(ctx, "strftime", args, 1, &mut x)? {
        return Ok(SqlValue::Null);
    }
    compute_jd(&mut x);
    compute_ymd_hms(&mut x);

    let mut out: Vec<u8> = Vec::with_capacity(z_fmt.len() + 16);
    let mut i = 0;
    while i < z_fmt.len() {
        if z_fmt[i] != b'%' {
            out.push(z_fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        let cf = at(z_fmt, i);
        i += 1;
        match cf {
            b'd' => out.extend_from_slice(two(x.d).as_bytes()),
            b'e' => out.extend_from_slice(fmt_int(x.d, 2, false).as_bytes()),
            b'f' => {
                let s = x.s.min(59.999);
                let mut spec = FloatFormatSpec::new(FloatConversion::F);
                spec.precision = Some(3);
                spec.width = 6;
                spec.zero_pad = true;
                out.extend_from_slice(format_float(s, spec).as_bytes());
            }
            b'F' => {
                out.extend_from_slice(
                    format!("{}-{}-{}", fmt_int(x.y, 4, true), two(x.m), two(x.d)).as_bytes(),
                );
            }
            b'G' => out.extend_from_slice(fmt_int(iso_thursday(&x).y, 4, true).as_bytes()),
            b'g' => out.extend_from_slice(two(iso_thursday(&x).y % 100).as_bytes()),
            b'H' => out.extend_from_slice(two(x.h).as_bytes()),
            b'k' => out.extend_from_slice(fmt_int(x.h, 2, false).as_bytes()),
            b'I' | b'l' => {
                let mut h = x.h;
                if h > 12 {
                    h -= 12;
                }
                if h == 0 {
                    h = 12;
                }
                out.extend_from_slice(fmt_int(h, 2, cf == b'I').as_bytes());
            }
            b'j' => out.extend_from_slice(fmt_int(days_after_jan01(&x) + 1, 3, true).as_bytes()),
            b'J' => {
                let mut spec = FloatFormatSpec::new(FloatConversion::LowerG);
                spec.precision = Some(16);
                out.extend_from_slice(
                    format_float((x.i_jd as f64) / 86_400_000.0, spec).as_bytes(),
                );
            }
            b'm' => out.extend_from_slice(two(x.m).as_bytes()),
            b'M' => out.extend_from_slice(two(x.min).as_bytes()),
            b'p' => out.extend_from_slice(if x.h >= 12 { b"PM" } else { b"AM" }),
            b'P' => out.extend_from_slice(if x.h >= 12 { b"pm" } else { b"am" }),
            b'R' => out.extend_from_slice(format!("{}:{}", two(x.h), two(x.min)).as_bytes()),
            b's' => {
                if x.use_subsec {
                    let mut spec = FloatFormatSpec::new(FloatConversion::F);
                    spec.precision = Some(3);
                    out.extend_from_slice(
                        format_float(((x.i_jd - UNIX_EPOCH_JD_MS) as f64) / 1000.0, spec)
                            .as_bytes(),
                    );
                } else {
                    out.extend_from_slice(
                        int64_to_text((x.i_jd / 1000) - UNIX_EPOCH_JD_S).as_bytes(),
                    );
                }
            }
            b'S' => out.extend_from_slice(two(c_int(x.s)).as_bytes()),
            b'T' => {
                out.extend_from_slice(
                    format!("{}:{}:{}", two(x.h), two(x.min), two(c_int(x.s))).as_bytes(),
                );
            }
            b'u' | b'w' => {
                let mut c = days_after_sunday(&x);
                if c == 0 && cf == b'u' {
                    c = 7;
                }
                out.push(b'0' + c as u8);
            }
            b'U' => {
                out.extend_from_slice(
                    two((days_after_jan01(&x) - days_after_sunday(&x) + 7) / 7).as_bytes(),
                );
            }
            b'V' => {
                out.extend_from_slice(two(days_after_jan01(&iso_thursday(&x)) / 7 + 1).as_bytes());
            }
            b'W' => {
                out.extend_from_slice(
                    two((days_after_jan01(&x) - days_after_monday(&x) + 7) / 7).as_bytes(),
                );
            }
            b'Y' => out.extend_from_slice(fmt_int(x.y, 4, true).as_bytes()),
            b'%' => out.push(b'%'),
            _ => return Ok(SqlValue::Null),
        }
    }
    Ok(SqlValue::Text(out.into_boxed_slice()))
}

fn timediff_fn(
    a: &SqlValue,
    b: &SqlValue,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    let mut d1 = DateTime::default();
    let mut d2 = DateTime::default();
    if is_date(ctx, "timediff", std::slice::from_ref(a), 0, &mut d1)? {
        return Ok(SqlValue::Null);
    }
    if is_date(ctx, "timediff", std::slice::from_ref(b), 0, &mut d2)? {
        return Ok(SqlValue::Null);
    }
    compute_ymd_hms(&mut d1);
    compute_ymd_hms(&mut d2);
    let sign;
    let mut y;
    let mut m;
    if d1.i_jd >= d2.i_jd {
        sign = '+';
        y = d1.y - d2.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = d1.m - d2.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.i_jd < d2.i_jd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m -= 1;
            if d2.m < 1 {
                d2.m = 12;
                d2.y -= 1;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.i_jd = (d1.i_jd - d2.i_jd) + 148_699_540_800_000;
    } else {
        sign = '-';
        y = d2.y - d1.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = d2.m - d1.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.i_jd > d2.i_jd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m += 1;
            if d2.m > 12 {
                d2.m = 1;
                d2.y += 1;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.i_jd = (d2.i_jd - d1.i_jd) + 148_699_540_800_000;
    }
    d1.clear_ymd_hms_tz();
    compute_ymd_hms(&mut d1);
    let mut spec = FloatFormatSpec::new(FloatConversion::F);
    spec.precision = Some(3);
    spec.width = 6;
    spec.zero_pad = true;
    Ok(SqlValue::text(format!(
        "{sign}{}-{}-{} {}:{}:{}",
        fmt_int(y, 4, true),
        two(m),
        two(d1.d - 1),
        two(d1.h),
        two(d1.min),
        format_float(d1.s, spec)
    )))
}
