//! SQLite 3.54.1 `printf()` / `format()` (`printf.c:210` in `SQLITE_PRINTF_SQLFUNC` mode).

use std::borrow::Cow;

use xdb_core::value::{fp_decode, int_value, real_value, FpSpecial, FP_PRECISION_LIMIT};
use xdb_core::SqlValue;

use crate::errors::{SqlError, SqlResult};
use crate::functions::FunctionContext;
use crate::utf8::{c_str_len, skip_utf8, text_bytes_of, ByteAccum};

#[derive(Clone, Copy, PartialEq, Eq)]
enum XType {
    Radix,
    Float,
    Exp,
    Generic,
    Size,
    String,
    Percent,
    CharX,
    EscapeQ,
    EscapeBigQ,
    EscapeW,
    EscapeJ,
    EscapeBigJ,
    Ordinal,
    Decimal,
    Invalid,
}

#[derive(Clone, Copy)]
struct EtInfo {
    base: u32,
    signed: bool,
    xtype: XType,
    upper_hex: bool,
    exp_char: u8,
    prefix: &'static [u8],
}

fn lookup_fmt(c: u8) -> EtInfo {
    match c {
        b'd' | b'i' => EtInfo {
            base: 10,
            signed: true,
            xtype: XType::Decimal,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'u' => EtInfo {
            base: 10,
            signed: false,
            xtype: XType::Decimal,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'r' => EtInfo {
            base: 10,
            signed: true,
            xtype: XType::Ordinal,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'o' => EtInfo {
            base: 8,
            signed: false,
            xtype: XType::Radix,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"0",
        },
        b'x' => EtInfo {
            base: 16,
            signed: false,
            xtype: XType::Radix,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"0x",
        },
        b'X' => EtInfo {
            base: 16,
            signed: false,
            xtype: XType::Radix,
            upper_hex: true,
            exp_char: b'e',
            prefix: b"0X",
        },
        b'p' => EtInfo {
            base: 16,
            signed: false,
            xtype: XType::Radix,
            upper_hex: true,
            exp_char: b'e',
            prefix: b"0x",
        },
        b'f' => EtInfo {
            base: 0,
            signed: true,
            xtype: XType::Float,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'e' => EtInfo {
            base: 0,
            signed: true,
            xtype: XType::Exp,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'E' => EtInfo {
            base: 0,
            signed: true,
            xtype: XType::Exp,
            upper_hex: false,
            exp_char: b'E',
            prefix: b"",
        },
        b'g' => EtInfo {
            base: 0,
            signed: true,
            xtype: XType::Generic,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'G' => EtInfo {
            base: 0,
            signed: true,
            xtype: XType::Generic,
            upper_hex: false,
            exp_char: b'E',
            prefix: b"",
        },
        b's' | b'z' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::String,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'c' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::CharX,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'%' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::Percent,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'q' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::EscapeQ,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'Q' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::EscapeBigQ,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'w' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::EscapeW,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'j' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::EscapeJ,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'J' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::EscapeBigJ,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        b'n' => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::Size,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
        _ => EtInfo {
            base: 0,
            signed: false,
            xtype: XType::Invalid,
            upper_hex: false,
            exp_char: b'e',
            prefix: b"",
        },
    }
}

const SMALL_PRECISION: i32 = 70 - 10 - (70 / 3);
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

#[inline]
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

struct Args<'a> {
    args: &'a [SqlValue],
    idx: usize,
}

impl<'a> Args<'a> {
    fn new(args: &'a [SqlValue], first: usize) -> Self {
        Self { args, idx: first }
    }

    fn int(&mut self) -> i64 {
        if self.idx < self.args.len() {
            let v = &self.args[self.idx];
            self.idx += 1;
            int_value(v)
        } else {
            0
        }
    }

    fn double(&mut self) -> f64 {
        if self.idx < self.args.len() {
            let v = &self.args[self.idx];
            self.idx += 1;
            real_value(v)
        } else {
            0.0
        }
    }

    fn text(&mut self) -> Option<Cow<'a, [u8]>> {
        if self.idx < self.args.len() {
            let v = &self.args[self.idx];
            self.idx += 1;
            text_bytes_of(v)
        } else {
            None
        }
    }
}

fn format_unsigned_radix(mut mag: u64, base: u32, upper: bool) -> String {
    if mag == 0 {
        return "0".to_string();
    }
    let digits: &[u8] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut buf = [0u8; 64];
    let mut pos = buf.len();
    let b = u64::from(base);
    while mag > 0 {
        pos -= 1;
        buf[pos] = digits[(mag % b) as usize];
        mag /= b;
    }
    String::from_utf8(buf[pos..].to_vec()).expect("ASCII digits")
}

fn adjust_width_for_utf8(bytes: &[u8], mut width: i32, altform2: bool) -> i32 {
    if altform2 && width > 0 {
        for &b in bytes {
            if (b & 0xc0) == 0x80 {
                width = width.saturating_add(1);
            }
        }
    }
    width
}

/// Core SQLite `sqlite3_str_vappendf` implementation writing into `acc`.
pub fn str_appendf(
    acc: &mut ByteAccum<'_>,
    format: &[u8],
    args: &[SqlValue],
    first: usize,
) -> SqlResult<()> {
    let mut a = Args::new(args, first);
    let fmt = &format[..c_str_len(format)];
    let fmt_end = fmt.len();
    let mut i = 0usize;

    while i < fmt_end {
        let mut c = fmt[i];
        if c != b'%' {
            let mut k = i;
            while k < fmt_end && fmt[k] != b'%' {
                k += 1;
            }
            acc.push_bytes(&fmt[i..k])?;
            if k >= fmt_end {
                break;
            }
            i = k;
        }
        i += 1;
        if i >= fmt_end {
            acc.push(b'%')?;
            break;
        }
        c = fmt[i];

        let mut flag_leftjustify = false;
        let mut flag_prefix: u8 = 0;
        let mut c_thousand = false;
        let mut flag_alternateform = false;
        let mut flag_altform2 = false;
        let mut flag_zeropad = false;
        let mut width: i32 = 0;
        let mut precision: i32 = -1;

        loop {
            let mut done = false;
            match c {
                b'-' => flag_leftjustify = true,
                b'+' => flag_prefix = b'+',
                b' ' => flag_prefix = b' ',
                b'#' => flag_alternateform = true,
                b'!' => flag_altform2 = true,
                b'0' => flag_zeropad = true,
                b',' => c_thousand = true,
                b'l' => {
                    i += 1;
                    c = at(fmt, i);
                    if c == b'l' {
                        i += 1;
                        c = at(fmt, i);
                    }
                    done = true;
                }
                b'1'..=b'9' => {
                    let mut wx: u32 = u32::from(c - b'0');
                    loop {
                        i += 1;
                        c = at(fmt, i);
                        if !c.is_ascii_digit() {
                            break;
                        }
                        wx = wx.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                    }
                    width = (wx & 0x7fff_ffff) as i32;
                    if c != b'.' && c != b'l' {
                        done = true;
                    } else {
                        i -= 1;
                    }
                }
                b'*' => {
                    width = a.int() as i32;
                    if width < 0 {
                        flag_leftjustify = true;
                        width = if width >= -2_147_483_647 { -width } else { 0 };
                    }
                    let next = at(fmt, i + 1);
                    if next != b'.' && next != b'l' {
                        i += 1;
                        c = at(fmt, i);
                        done = true;
                    }
                }
                b'.' => {
                    i += 1;
                    c = at(fmt, i);
                    if c == b'*' {
                        precision = a.int() as i32;
                        if precision < 0 {
                            precision = if precision >= -2_147_483_647 {
                                -precision
                            } else {
                                -1
                            };
                        }
                        i += 1;
                        c = at(fmt, i);
                    } else {
                        let mut px: u32 = 0;
                        while c.is_ascii_digit() {
                            px = px.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                            i += 1;
                            c = at(fmt, i);
                        }
                        precision = (px & 0x7fff_ffff) as i32;
                    }
                    if c == b'l' {
                        i -= 1;
                    } else {
                        done = true;
                    }
                }
                _ => done = true,
            }
            if done {
                break;
            }
            i += 1;
            c = at(fmt, i);
            if c == 0 {
                break;
            }
        }

        let infop = lookup_fmt(c);
        let out_bytes: Vec<u8>;
        match infop.xtype {
            XType::Radix | XType::Ordinal | XType::Decimal => {
                if infop.xtype != XType::Decimal {
                    c_thousand = false;
                }
                let v = a.int();
                let (mag, prefix): (u64, u8) = if infop.signed {
                    if v < 0 {
                        (v.unsigned_abs(), b'-')
                    } else {
                        (v as u64, flag_prefix)
                    }
                } else {
                    (v as u64, 0)
                };
                if mag == 0 {
                    flag_alternateform = false;
                }
                let prefix_len = i32::from(prefix != 0);
                if flag_zeropad && precision < width - prefix_len {
                    precision = width - prefix_len;
                }
                if precision >= SMALL_PRECISION {
                    let mut n = (precision as usize).saturating_add(10);
                    if c_thousand {
                        n = n.saturating_add((precision / 3) as usize);
                    }
                    if n > acc.limit {
                        return Err(SqlError::too_big());
                    }
                }
                let mut digits = format_unsigned_radix(mag, infop.base, infop.upper_hex);
                if infop.xtype == XType::Ordinal {
                    let mut x = (mag % 10) as usize;
                    let tens = (mag / 10) % 10;
                    if x >= 4 || tens == 1 {
                        x = 0;
                    }
                    let suffix = &["th", "st", "nd", "rd"][x];
                    digits.push_str(suffix);
                }
                if precision > 0 && (precision as usize) > digits.len() {
                    let est = (precision as usize).saturating_add(if c_thousand {
                        (precision as usize) / 3
                    } else {
                        0
                    });
                    acc.check_scratch(est)?;
                    let pad = precision as usize - digits.len();
                    let mut padded = String::with_capacity(precision as usize);
                    for _ in 0..pad {
                        padded.push('0');
                    }
                    padded.push_str(&digits);
                    digits = padded;
                }
                if c_thousand && digits.len() > 3 {
                    let len = digits.len();
                    let grouped_len = len.saturating_add(len / 3);
                    acc.check_scratch(grouped_len)?;
                    let mut k = ((len - 1) % 3) + 1;
                    let mut grouped = String::with_capacity(grouped_len);
                    grouped.push_str(&digits[..k]);
                    while k < len {
                        grouped.push(',');
                        grouped.push_str(&digits[k..k + 3]);
                        k += 3;
                    }
                    digits = grouped;
                }
                let mut final_buf = Vec::with_capacity(digits.len() + 4);
                if flag_alternateform && !infop.prefix.is_empty() {
                    final_buf.extend_from_slice(infop.prefix);
                }
                if prefix != 0 {
                    final_buf.push(prefix);
                }
                final_buf.extend_from_slice(digits.as_bytes());
                out_bytes = final_buf;
            }
            XType::Float | XType::Exp | XType::Generic => {
                if let Some(special) = render_float(
                    acc,
                    a.double(),
                    infop,
                    precision,
                    width,
                    flag_prefix,
                    flag_leftjustify,
                    flag_alternateform,
                    flag_altform2,
                    flag_zeropad,
                    c_thousand,
                )? {
                    out_bytes = special.into_bytes();
                } else {
                    i += 1;
                    continue;
                }
            }
            XType::Size => {
                width = 0;
                out_bytes = Vec::new();
            }
            XType::Percent => {
                out_bytes = vec![b'%'];
            }
            XType::CharX => {
                let mut ch = Vec::with_capacity(4);
                if let Some(t) = a.text() {
                    let c0 = at(&t, 0);
                    ch.push(c0);
                    let mut p = 1;
                    if (c0 & 0xc0) == 0xc0 {
                        while ch.len() < 4 && (at(&t, p) & 0xc0) == 0x80 {
                            ch.push(t[p]);
                            p += 1;
                        }
                    }
                } else {
                    ch.push(0);
                }
                if precision > 1 {
                    let p_usize = precision as usize;
                    let total = p_usize.checked_mul(ch.len()).ok_or_else(SqlError::too_big)?;
                    if acc.exceeds(total) {
                        return Err(SqlError::too_big());
                    }
                    width = width.saturating_sub(precision - 1);
                    if width > 1 && !flag_leftjustify {
                        acc.push_repeat(b' ', (width - 1) as usize)?;
                        width = 0;
                    }
                    for _ in 0..(p_usize - 1) {
                        acc.push_bytes(&ch)?;
                    }
                }
                flag_altform2 = true;
                width = adjust_width_for_utf8(&ch, width, flag_altform2);
                out_bytes = ch;
            }
            XType::String => {
                let t = a.text();
                let slice: &[u8] = t.as_deref().unwrap_or(b"");
                let length = if precision >= 0 {
                    if flag_altform2 {
                        let mut z = 0;
                        let mut rem = precision;
                        let mut ticks = 0u64;
                        while rem > 0 && at(slice, z) != 0 {
                            ticks += 1;
                            if (ticks & 255) == 0 {
                                acc.charge_steps(1)?;
                            }
                            z = skip_utf8(slice, z);
                            rem -= 1;
                        }
                        z
                    } else {
                        let mut z = 0;
                        let limit = precision as usize;
                        while z < limit && at(slice, z) != 0 {
                            z += 1;
                        }
                        if z >= 256 {
                            acc.charge_steps((z / 256) as u64)?;
                        }
                        z
                    }
                } else {
                    let len = c_str_len(slice);
                    if len >= 256 {
                        acc.charge_steps((len / 256) as u64)?;
                    }
                    len
                };
                acc.check_scratch(length)?;
                out_bytes = slice[..length].to_vec();
                width = adjust_width_for_utf8(&out_bytes, width, flag_altform2);
            }
            XType::EscapeJ | XType::EscapeBigJ => {
                let t = a.text();
                escape_json(
                    acc,
                    t.as_deref(),
                    infop.xtype == XType::EscapeBigJ,
                    precision,
                    width,
                    flag_altform2,
                    flag_leftjustify,
                )?;
                i += 1;
                continue;
            }
            XType::EscapeQ | XType::EscapeBigQ | XType::EscapeW => {
                let t = a.text();
                out_bytes = escape_quote(
                    acc,
                    t.as_deref(),
                    infop.xtype,
                    precision,
                    flag_alternateform,
                    flag_altform2,
                )?;
                width = adjust_width_for_utf8(&out_bytes, width, flag_altform2);
            }
            XType::Invalid => return Ok(()),
        }

        let pad = width - (out_bytes.len() as i32);
        if pad > 0 {
            if !flag_leftjustify {
                acc.push_repeat(b' ', pad as usize)?;
            }
            acc.push_bytes(&out_bytes)?;
            if flag_leftjustify {
                acc.push_repeat(b' ', pad as usize)?;
            }
        } else {
            acc.push_bytes(&out_bytes)?;
        }
        i += 1;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_float(
    acc: &mut ByteAccum<'_>,
    realvalue: f64,
    infop: EtInfo,
    precision_in: i32,
    width: i32,
    flag_prefix: u8,
    flag_leftjustify: bool,
    flag_alternateform: bool,
    flag_altform2: bool,
    flag_zeropad: bool,
    c_thousand: bool,
) -> SqlResult<Option<String>> {
    let mut xtype = infop.xtype;
    let mut precision = if precision_in < 0 { 6 } else { precision_in };
    if precision > FP_PRECISION_LIMIT {
        precision = FP_PRECISION_LIMIT;
    }
    let i_round = match xtype {
        XType::Float => -precision,
        XType::Generic => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
        _ => precision + 1,
    };
    let decoded = fp_decode(realvalue, i_round, if flag_altform2 { 20 } else { 16 });
    let mut digits = decoded.digits;
    let mut i_dp = decoded.i_dp;

    match decoded.special {
        FpSpecial::Nan => {
            return Ok(Some(if flag_zeropad { "null" } else { "NaN" }.to_string()));
        }
        FpSpecial::Inf => {
            if flag_zeropad {
                digits = "9".to_string();
                i_dp = 1000;
            } else if decoded.sign == '-' {
                return Ok(Some("-Inf".to_string()));
            } else if flag_prefix != 0 {
                return Ok(Some(format!("{}Inf", flag_prefix as char)));
            } else {
                return Ok(Some("Inf".to_string()));
            }
        }
        FpSpecial::Finite => {}
    }

    let sn = digits.len();
    let prefix: u8 = if decoded.sign == '-' {
        if flag_alternateform && flag_prefix == 0 && xtype == XType::Float && i_dp <= i_round {
            0
        } else {
            b'-'
        }
    } else {
        flag_prefix
    };

    let mut exp = i_dp - 1;
    let flag_rtz;
    if xtype == XType::Generic {
        precision -= 1;
        flag_rtz = !flag_alternateform;
        if exp < -4 || exp > precision {
            xtype = XType::Exp;
        } else {
            precision -= exp;
            xtype = XType::Float;
        }
    } else {
        flag_rtz = flag_altform2;
    }

    let mut e2 = if xtype == XType::Exp { 0 } else { i_dp - 1 };
    let mut sz_needed = (e2.max(0) as usize)
        .saturating_add(precision.max(0) as usize)
        .saturating_add(width.max(0) as usize)
        .saturating_add(10);
    if c_thousand && e2 > 0 {
        sz_needed = sz_needed.saturating_add(((e2 + 2) / 3) as usize);
    }
    if acc.len().saturating_add(sz_needed).saturating_add(1) > acc.limit {
        return Err(SqlError::too_big());
    }

    let flag_dp = precision > 0 || flag_alternateform || flag_altform2;
    let dbytes = digits.as_bytes();
    let mut out = String::with_capacity(64);
    if prefix != 0 {
        out.push(prefix as char);
    }
    let mut j: usize = 0;
    if e2 < 0 {
        out.push('0');
    } else if c_thousand {
        while e2 >= 0 {
            if j < sn {
                out.push(dbytes[j] as char);
                j += 1;
            } else {
                out.push('0');
            }
            if e2 % 3 == 0 && e2 > 1 {
                out.push(',');
            }
            e2 -= 1;
        }
    } else {
        j = ((e2 + 1) as usize).min(sn);
        out.push_str(&digits[..j]);
        e2 -= j as i32;
        if e2 >= 0 {
            for _ in 0..=(e2 as usize) {
                out.push('0');
            }
            e2 = -1;
        }
    }

    if flag_dp {
        out.push('.');
    }
    if e2 < -1 && precision > 0 {
        let nn = (-1 - e2).min(precision);
        for _ in 0..(nn as usize) {
            out.push('0');
        }
        precision -= nn;
    }
    if precision > 0 {
        let nn = ((sn - j) as i32).min(precision);
        if nn > 0 {
            out.push_str(&digits[j..j + nn as usize]);
            precision -= nn;
        }
        if precision > 0 && !flag_rtz {
            for _ in 0..(precision as usize) {
                out.push('0');
            }
        }
    }
    if flag_rtz && flag_dp {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            if flag_altform2 {
                out.push('0');
            } else {
                out.pop();
            }
        }
    }
    if xtype == XType::Exp {
        exp = i_dp - 1;
        out.push(infop.exp_char as char);
        if exp < 0 {
            out.push('-');
            exp = -exp;
        } else {
            out.push('+');
        }
        if exp >= 100 {
            out.push((b'0' + (exp / 100) as u8) as char);
            exp %= 100;
        }
        out.push((b'0' + (exp / 10) as u8) as char);
        out.push((b'0' + (exp % 10) as u8) as char);
    }

    let length = out.len() as i32;
    if length < width {
        let w_usize = width as usize;
        acc.check_scratch(w_usize)?;
        let n_pad = (width - length) as usize;
        if flag_leftjustify {
            for _ in 0..n_pad {
                out.push(' ');
            }
        } else if !flag_zeropad {
            let mut padded = String::with_capacity(w_usize);
            for _ in 0..n_pad {
                padded.push(' ');
            }
            padded.push_str(&out);
            out = padded;
        } else {
            let adj = usize::from(prefix != 0);
            let mut padded = String::with_capacity(w_usize);
            padded.push_str(&out[..adj]);
            for _ in 0..n_pad {
                padded.push('0');
            }
            padded.push_str(&out[adj..]);
            out = padded;
        }
    }

    acc.push_str(&out)?;
    Ok(None)
}

fn escape_quote(
    acc: &mut ByteAccum<'_>,
    arg: Option<&[u8]>,
    xtype: XType,
    precision: i32,
    mut flag_alternateform: bool,
    altform2: bool,
) -> SqlResult<Vec<u8>> {
    let limit = acc.limit;
    let mut need_quote = 0u8;
    let escarg: &[u8] = match arg {
        None => {
            if xtype == XType::EscapeBigQ {
                b"NULL"
            } else {
                b"(NULL)"
            }
        }
        Some(b) => {
            if xtype == XType::EscapeBigQ {
                need_quote = 1;
            }
            b
        }
    };
    let q = if xtype == XType::EscapeW {
        flag_alternateform = false;
        b'"'
    } else {
        b'\''
    };
    let mut k = precision;
    let mut i = 0usize;
    let mut n = 0usize;
    let mut ticks = 0u64;
    while k != 0 {
        let ch = at(escarg, i);
        if ch == 0 {
            break;
        }
        ticks += 1;
        if (ticks & 255) == 0 {
            acc.charge_steps(1)?;
        }
        if ch == q {
            n += 1;
        }
        if altform2 && (ch & 0xc0) == 0xc0 {
            while (at(escarg, i + 1) & 0xc0) == 0x80 {
                i += 1;
            }
        }
        i += 1;
        k -= 1;
    }
    if flag_alternateform {
        if i >= 256 {
            acc.charge_steps((i / 256) as u64)?;
        }
        let mut n_back = 0usize;
        let mut n_ctrl = 0usize;
        for &ch in &escarg[..i] {
            if ch == b'\\' {
                n_back += 1;
            } else if ch <= 0x1f {
                n_ctrl += 1;
            }
        }
        if n_ctrl > 0 || xtype == XType::EscapeQ {
            n = n
                .saturating_add(n_back)
                .saturating_add(n_ctrl.saturating_mul(5));
            if xtype == XType::EscapeBigQ {
                n = n.saturating_add(10);
                need_quote = 2;
            }
        } else {
            flag_alternateform = false;
        }
    }
    let total = n.saturating_add(i).saturating_add(3);
    if total > 70 && total > limit {
        return Err(SqlError::too_big());
    }
    acc.check_scratch(total)?;
    let mut out = Vec::with_capacity(total);
    if need_quote == 2 {
        out.extend_from_slice(b"unistr('");
    } else if need_quote == 1 {
        out.push(b'\'');
    }
    if flag_alternateform {
        for &ch in &escarg[..i] {
            if ch == q {
                out.push(ch);
                out.push(ch);
            } else if ch == b'\\' {
                out.push(b'\\');
                out.push(b'\\');
            } else if ch <= 0x1f {
                out.push(b'\\');
                out.push(b'u');
                out.push(b'0');
                out.push(b'0');
                out.push(if ch >= 0x10 { b'1' } else { b'0' });
                out.push(HEX_LOWER[(ch & 0x0f) as usize]);
            } else {
                out.push(ch);
            }
        }
    } else {
        for &ch in &escarg[..i] {
            out.push(ch);
            if ch == q {
                out.push(ch);
            }
        }
    }
    if need_quote != 0 {
        out.push(b'\'');
        if need_quote == 2 {
            out.push(b')');
        }
    }
    Ok(out)
}

fn escape_json(
    acc: &mut ByteAccum<'_>,
    escarg: Option<&[u8]>,
    quoted: bool,
    precision: i32,
    width: i32,
    altform2: bool,
    leftjustify: bool,
) -> SqlResult<()> {
    let start = acc.len();
    if let Some(src) = escarg {
        if quoted {
            acc.push(b'"')?;
        }
        let mut px: usize = if precision < 0 {
            0x7fff_ffff
        } else if altform2 {
            let mut target = precision as usize;
            let mut k = 0;
            while k < target && at(src, k) != 0 {
                if (src[k] & 0xc0) == 0x80 {
                    target += 1;
                }
                k += 1;
            }
            if k == target {
                while (at(src, target) & 0xc0) == 0x80 {
                    target += 1;
                }
            }
            target
        } else {
            precision as usize
        };
        if px > src.len() {
            px = src.len();
        }
        let mut i = 0;
        let mut j = 0;
        while i < px {
            let ch = src[i];
            if ch <= 0x1f || ch == b'"' || ch == b'\\' {
                if j < i {
                    acc.push_bytes(&src[j..i])?;
                }
                j = i + 1;
                if ch == 0 {
                    break;
                }
                acc.push(b'\\')?;
                if ch > 0x1f {
                    acc.push(ch)?;
                } else if ((1u32 << ch) & 0x3700) != 0 {
                    acc.push(b"btn?fr"[(ch - 8) as usize])?;
                } else {
                    acc.push_str("u00")?;
                    acc.push(HEX_LOWER[(ch >> 4) as usize])?;
                    acc.push(HEX_LOWER[(ch & 0x0f) as usize])?;
                }
            }
            i += 1;
        }
        if j < i {
            acc.push_bytes(&src[j..i])?;
        }
        if quoted {
            acc.push(b'"')?;
        }
    } else if quoted {
        acc.push_str("null")?;
    }

    if width > 0 {
        let n = acc.len() - start;
        let mut len = n;
        if altform2 && n > 0 {
            for &b in &acc.buf[start..] {
                if (b & 0xc0) == 0x80 {
                    len -= 1;
                }
            }
        }
        if (width as usize) > len {
            let sp = width as usize - len;
            acc.push_repeat(b' ', sp)?;
            if !leftjustify && n > 0 {
                acc.buf.copy_within(start..start + n, start + sp);
                acc.buf[start..start + sp].fill(b' ');
            }
        }
    }
    Ok(())
}

/// SQLite `printf(FORMAT, ...)` / `format(FORMAT, ...)`.
pub fn sql_printf(args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    if args.is_empty() {
        return Ok(SqlValue::Null);
    }
    let Some(fmt) = text_bytes_of(&args[0]) else {
        return Ok(SqlValue::Null);
    };
    let mut acc = ByteAccum::new(ctx.limits.max_length, true, ctx.meter);
    str_appendf(&mut acc, &fmt, args, 1)?;
    Ok(SqlValue::Text(acc.into_boxed_bytes()))
}
