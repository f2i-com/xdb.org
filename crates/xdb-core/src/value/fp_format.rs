//! Floating-point formatting with SQLite 3.54.1 semantics (`printf.c:545-780`).

use super::fp_core::{fp_decode, FpSpecial};

/// `SQLITE_FP_PRECISION_LIMIT` (`printf.c:176`).
pub const FP_PRECISION_LIMIT: i32 = 100_000_000;

/// Default significant digits for `REAL -> TEXT` (`db->nFpDigit`, `main.c:3474`).
pub const DEFAULT_FP_DIGITS: i32 = 17;

/// `printf` float conversion specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatConversion {
    F,
    LowerE,
    UpperE,
    LowerG,
    UpperG,
}

/// Optional prefix for non-negative values (`'+'` or `' '`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SignPrefix {
    #[default]
    None,
    Plus,
    Space,
}

impl SignPrefix {
    #[inline]
    fn as_str(self) -> &'static str {
        match self {
            SignPrefix::None => "",
            SignPrefix::Plus => "+",
            SignPrefix::Space => " ",
        }
    }
}

/// Parsed flags/width/precision of one `printf` float conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatFormatSpec {
    pub conversion: FloatConversion,
    /// Precision; `None` or negative means default (`6`). Clamped to `FP_PRECISION_LIMIT`.
    pub precision: Option<i32>,
    /// Minimum field width (default `0`).
    pub width: usize,
    /// `'-'` flag: left-justify within `width`.
    pub left_justify: bool,
    /// `'+'` or `' '` flag.
    pub sign_prefix: SignPrefix,
    /// `'#'` flag: always show decimal point; `%g` keeps trailing zeros.
    pub alternate_form: bool,
    /// `'!'` flag: up to 20 significant digits instead of 16; strips trailing zeros but keeps `.0`.
    pub alt_form2: bool,
    /// `'0'` flag: pad with zeros after sign; `±Inf` renders as `9.0e+999`.
    pub zero_pad: bool,
    /// `','` flag: thousands separators in the integer part.
    pub thousands: bool,
}

impl FloatFormatSpec {
    pub const fn new(conversion: FloatConversion) -> Self {
        Self {
            conversion,
            precision: None,
            width: 0,
            left_justify: false,
            sign_prefix: SignPrefix::None,
            alternate_form: false,
            alt_form2: false,
            zero_pad: false,
            thousands: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum XType {
    Float,
    Exp,
    Generic,
}

/// Render one `printf` float conversion exactly as SQLite 3.54.1 does.
pub fn format_float(realvalue: f64, spec: FloatFormatSpec) -> String {
    let mut precision = spec.precision.unwrap_or(-1);
    let width = spec.width;
    let flag_prefix = spec.sign_prefix.as_str();
    let flag_alt = spec.alternate_form;
    let flag_alt2 = spec.alt_form2;
    let flag_zeropad = spec.zero_pad;

    let mut xtype = match spec.conversion {
        FloatConversion::F => XType::Float,
        FloatConversion::LowerE | FloatConversion::UpperE => XType::Exp,
        FloatConversion::LowerG | FloatConversion::UpperG => XType::Generic,
    };
    let exp_char = match spec.conversion {
        FloatConversion::UpperE | FloatConversion::UpperG => 'E',
        _ => 'e',
    };

    if precision < 0 {
        precision = 6;
    }
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
        XType::Exp => precision + 1,
    };

    let s = fp_decode(realvalue, i_round, if flag_alt2 { 20 } else { 16 });
    let mut digits = s.digits;
    let mut i_dp = s.i_dp;

    match s.special {
        FpSpecial::Nan => {
            return pad_spaces(
                if flag_zeropad { "null" } else { "NaN" },
                width,
                spec.left_justify,
            );
        }
        FpSpecial::Inf => {
            if flag_zeropad {
                digits = "9".to_string();
                i_dp = 1000;
            } else {
                let text = if s.sign == '-' {
                    "-Inf".to_string()
                } else if !flag_prefix.is_empty() {
                    format!("{flag_prefix}Inf")
                } else {
                    "Inf".to_string()
                };
                return pad_spaces(&text, width, spec.left_justify);
            }
        }
        FpSpecial::Finite => {}
    }

    let digit_bytes = digits.as_bytes();
    let sn = digit_bytes.len();

    let prefix = if s.sign == '-' {
        if flag_alt && flag_prefix.is_empty() && xtype == XType::Float && i_dp <= i_round {
            ""
        } else {
            "-"
        }
    } else {
        flag_prefix
    };

    let mut exp = i_dp - 1;
    let flag_rtz = if xtype == XType::Generic {
        precision -= 1;
        let rtz = !flag_alt;
        if exp < -4 || exp > precision {
            xtype = XType::Exp;
        } else {
            precision -= exp;
            xtype = XType::Float;
        }
        rtz
    } else {
        flag_alt2
    };

    let mut e2 = if xtype == XType::Exp { 0 } else { i_dp - 1 };
    let flag_dp = precision > 0 || flag_alt || flag_alt2;

    let mut out = String::with_capacity(32);
    out.push_str(prefix);

    // Digits before the decimal point.
    let mut j = 0usize;
    if e2 < 0 {
        out.push('0');
    } else if spec.thousands {
        while e2 >= 0 {
            if j < sn {
                out.push(digit_bytes[j] as char);
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
            push_zeros(&mut out, (e2 + 1) as usize);
            e2 = -1;
        }
    }

    if flag_dp {
        out.push('.');
    }

    // Zeros after the decimal point before the first significant digit.
    if e2 < -1 && precision > 0 {
        let nn = (-1 - e2).min(precision);
        push_zeros(&mut out, nn as usize);
        precision -= nn;
    }

    // Significant digits after the decimal point.
    if precision > 0 {
        let nn = ((sn - j) as i32).min(precision);
        if nn > 0 {
            out.push_str(&digits[j..j + nn as usize]);
            precision -= nn;
        }
        if precision > 0 && !flag_rtz {
            push_zeros(&mut out, precision as usize);
        }
    }

    // Remove trailing zeros, and '.' if nothing follows ('!' keeps ".0").
    if flag_rtz && flag_dp {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            if flag_alt2 {
                out.push('0');
            } else {
                out.pop();
            }
        }
    }

    // Exponent suffix: e±NN(N).
    if xtype == XType::Exp {
        exp = i_dp - 1;
        out.push(exp_char);
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

    if out.len() < width {
        let n_pad = width - out.len();
        if spec.left_justify {
            push_spaces(&mut out, n_pad);
        } else if !flag_zeropad {
            let mut padded = String::with_capacity(width);
            push_spaces(&mut padded, n_pad);
            padded.push_str(&out);
            out = padded;
        } else {
            let adj = usize::from(!prefix.is_empty());
            let mut padded = String::with_capacity(width);
            padded.push_str(&out[..adj]);
            push_zeros(&mut padded, n_pad);
            padded.push_str(&out[adj..]);
            out = padded;
        }
    }

    out
}

fn push_zeros(out: &mut String, n: usize) {
    out.reserve(n);
    for _ in 0..n {
        out.push('0');
    }
}

fn push_spaces(out: &mut String, n: usize) {
    out.reserve(n);
    for _ in 0..n {
        out.push(' ');
    }
}

fn pad_spaces(text: &str, width: usize, left_justify: bool) -> String {
    if text.len() >= width {
        return text.to_string();
    }
    let n_pad = width - text.len();
    let mut out = String::with_capacity(width);
    if left_justify {
        out.push_str(text);
        push_spaces(&mut out, n_pad);
    } else {
        push_spaces(&mut out, n_pad);
        out.push_str(text);
    }
    out
}

/// `REAL -> TEXT` as SQLite renders it (`vdbeMemRenderNum`, `vdbemem.c:106`):
/// `printf("%!.*g", n_fp_digit, r)`. Default `n_fp_digit = 17`.
pub fn real_to_text(r: f64, n_fp_digit: i32) -> String {
    let mut spec = FloatFormatSpec::new(FloatConversion::LowerG);
    spec.precision = Some(n_fp_digit);
    spec.alt_form2 = true;
    format_float(r, spec)
}
