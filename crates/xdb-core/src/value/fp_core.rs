//! SQLite 3.54.1's binary64 <-> decimal conversion core, ported bit-exactly.
//!
//! Both directions reproduce SQLite's exact algorithms (`util.c:592-850`, `1392-1510`):
//! - [`fp10_convert2`] (`sqlite3Fp10Convert2`, `util.c:787`)
//! - [`fp_decode`] (`sqlite3FpDecode`, `util.c:1392`)

use std::sync::OnceLock;

const BIT52: u64 = 1u64 << 52;
const BIT55_MINUS_2: u64 = (1u64 << 55) - 2;

/// Range of powers of ten handled by the converters (`util.c:567-568`).
pub const POWERSOF10_FIRST: i32 = -348;
pub const POWERSOF10_LAST: i32 = 347;
const N_POW: usize = (POWERSOF10_LAST - POWERSOF10_FIRST + 1) as usize;

// Tables from util.c:592-693 (generated upstream by tool/mkfptab.c --round).
const A_BASE: [u64; 27] = [
    0x8000000000000000, 0xa000000000000000, 0xc800000000000000, 0xfa00000000000000,
    0x9c40000000000000, 0xc350000000000000, 0xf424000000000000, 0x9896800000000000,
    0xbebc200000000000, 0xee6b280000000000, 0x9502f90000000000, 0xba43b74000000000,
    0xe8d4a51000000000, 0x9184e72a00000000, 0xb5e620f480000000, 0xe35fa931a0000000,
    0x8e1bc9bf04000000, 0xb1a2bc2ec5000000, 0xde0b6b3a76400000, 0x8ac7230489e80000,
    0xad78ebc5ac620000, 0xd8d726b7177a8000, 0x878678326eac9000, 0xa968163f0a57b400,
    0xd3c21bcecceda100, 0x84595161401484a0, 0xa56fa5b99019a5c8,
];

const A_SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae, 0xcf42894a5dce35ea, 0xa76c582338ed2621, 0x873e4f75e2224e68,
    0xda7f5bf590966848, 0xb080392cc4349dec, 0x8e938662882af53e, 0xe65829b3046b0afa,
    0xba121a4650e4ddeb, 0x964e858c91ba2655, 0xf2d56790ab41c2a2, 0xc428d05aa4751e4c,
    0x9e74d1b791e07e48, 0xcccccccccccccccc, 0xcecb8f27f4200f3a, 0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd, 0xda01ee641a708de9, 0xb01ae745b101e9e4, 0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81, 0xb9a74a0637ce2ee1, 0x95f83d0a1fb69cd9, 0xf24a01a73cf2dccf,
    0xc3b8358109e84f07, 0x9e19db92b4e31ba9,
];

const A_SCALE_LO: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b,
    0x0cb4a5a3, 0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc,
    0x00000000, 0x999090b6, 0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9,
    0x6d953e2c, 0x4abdaf10, 0xbc633b39, 0x0a862f81, 0x6c07a2c2,
];

struct PowerTable {
    hi: Box<[u64; N_POW]>,
    lo: Box<[u32; N_POW]>,
}

static POWERS: OnceLock<PowerTable> = OnceLock::new();

fn compute_power_of_ten(p: i32) -> (u64, u32) {
    let (mut g, mut n) = if p < 0 {
        if p == -1 {
            return (A_SCALE[13], A_SCALE_LO[13]);
        }
        let g = p / 27;
        let n = p % 27;
        if n != 0 {
            (g - 1, n + 27)
        } else {
            (g, n)
        }
    } else if p < 27 {
        return (A_BASE[p as usize], 0);
    } else {
        (p / 27, p % 27)
    };
    let s = A_SCALE[(g + 13) as usize];
    let s_lo = A_SCALE_LO[(g + 13) as usize];
    if n == 0 {
        return (s, s_lo);
    }
    let _ = &mut g;
    let _ = &mut n;
    let b = A_BASE[n as usize];
    let r = (s as u128) * (b as u128) + (((s_lo as u128) * (b as u128)) >> 32);
    let mut lo = ((r >> 32) & 0xffff_ffff) as u32;
    let mut x = (r >> 64) as u64;
    if (x & (1u64 << 63)) == 0 {
        x = (x << 1) | u64::from((lo >> 31) & 1);
        lo = lo.wrapping_shl(1) | 1;
    }
    (x, lo)
}

fn power_table() -> &'static PowerTable {
    POWERS.get_or_init(|| {
        let mut hi = Box::new([0u64; N_POW]);
        let mut lo = Box::new([0u32; N_POW]);
        for p in POWERSOF10_FIRST..=POWERSOF10_LAST {
            let (h, l) = compute_power_of_ten(p);
            let idx = (p - POWERSOF10_FIRST) as usize;
            hi[idx] = h;
            lo[idx] = l;
        }
        PowerTable { hi, lo }
    })
}

/// `floor(log2(10^p))`, approximately (`util.c:733`).
#[inline]
fn pwr10to2(p: i32) -> i32 {
    (p * 108853) >> 15
}

/// `floor(log10(2^p))`, approximately (`util.c:734`).
#[inline]
fn pwr2to10(p: i32) -> i32 {
    (p * 78913) >> 18
}

// 10^0 .. 10^22 are exact f64 values.
const EXACT_POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// `sqlite3Fp10Convert2` (`util.c:787`): the double SQLite produces for `d * 10^p`,
/// for a `u64` `d > 0`.
#[inline]
pub fn fp10_convert2(d: u64, p: i32) -> f64 {
    if d < (1u64 << 53) && (0..=22).contains(&p) {
        let r = (d as f64) * EXACT_POW10[p as usize];
        if r < 9007199254740992.0 {
            return r;
        }
    }
    fp10_convert2_full(d, p)
}

/// Full port of `sqlite3Fp10Convert2` with no shortcuts.
pub fn fp10_convert2_full(d: u64, p: i32) -> f64 {
    if d == 0 || p < POWERSOF10_FIRST {
        return 0.0;
    }
    if p > POWERSOF10_LAST {
        return f64::INFINITY;
    }
    let table = power_table();
    let b = 64 - (d.leading_zeros() as i32);
    let lp = pwr10to2(p);
    let mut e = 53 - b - lp;
    if e > 1074 {
        if e >= 1130 {
            return 0.0;
        }
        e = 1074;
    }
    let s = (-(e - (64 - b) + lp + 3)) as u32;
    let idx = (p - POWERSOF10_FIRST) as usize;
    let mut pwr10h = table.hi[idx];
    let mut pwr10l = table.lo[idx];
    if pwr10l != 0 {
        pwr10h = pwr10h.wrapping_add(1);
        pwr10l = !pwr10l;
    }
    let x = d << ((64 - b) as u32);
    let prod = (x as u128) * (pwr10h as u128);
    let mut hi = (prod >> 64) as u64;
    let mid1 = ((prod >> 32) & 0xffff_ffff) as u32;
    let mut sticky: u64 = 1;
    if (hi & ((1u64 << s) - 1)) == 0 {
        let mid2 = (((x as u128) * ((pwr10l as u128) << 32)) >> 96) as u32;
        sticky = u64::from(mid1.wrapping_sub(mid2) > 1);
        if mid1 < mid2 {
            hi = hi.wrapping_sub(1);
        }
    }
    let mut u = (hi >> s) | sticky;
    if u >= BIT55_MINUS_2 {
        u = (u >> 1) | (u & 1);
        e -= 1;
    }
    let mut m = (u + 1 + ((u >> 2) & 1)) >> 2;
    if e <= -972 {
        return f64::INFINITY;
    }
    if (m & BIT52) != 0 {
        m = (m & !BIT52) | (((1075 - e) as u64) << 52);
    }
    f64::from_bits(m)
}

/// `sqlite3Fp2Convert10` (`util.c:763`): for `r == m * 2^e` (`m` with top bit set),
/// a decimal significand with at least `n` (`1..=18`) digits and its power of ten.
fn fp2_convert10(m: u64, e: i32, n: i32) -> (u64, i32) {
    let table = power_table();
    let p = n - 1 - pwr2to10(e + 63);
    let idx = (p - POWERSOF10_FIRST) as usize;
    let h = (((m as u128) * (table.hi[idx] as u128)) >> 64) as u64;
    let d = if n == 18 {
        let h2 = h >> (-(e + pwr10to2(p) + 2)) as u32;
        h2.wrapping_add(h2.wrapping_shl(1) & 2) >> 1
    } else {
        h >> (-(e + pwr10to2(p) + 1)) as u32
    };
    (d, -p)
}

/// Special-value classification in `FpDecoded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpSpecial {
    Finite,
    Inf,
    Nan,
}

/// Decoded floating-point digits (`struct FpDecode`, `sqliteInt.h:4945`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpDecoded {
    /// `'-'` for negative non-zero values, `'+'` otherwise (`-0.0` decodes as `'+'`).
    pub sign: char,
    pub special: FpSpecial,
    /// Significant ASCII digits, without leading or trailing zeros (`"0"` for zero; empty when special).
    pub digits: String,
    /// Position of the decimal point relative to the first digit: `value = 0.DIGITS * 10^i_dp`.
    pub i_dp: i32,
}

/// `sqlite3FpDecode` (`util.c:1392`): decode `r` into rounded decimal digits.
pub fn fp_decode(mut r: f64, mut i_round: i32, mx_round: i32) -> FpDecoded {
    let mx_round = mx_round.max(1);
    let sign = if r < 0.0 {
        r = -r;
        '-'
    } else if r == 0.0 {
        return FpDecoded {
            sign: '+',
            special: FpSpecial::Finite,
            digits: "0".to_string(),
            i_dp: 1,
        };
    } else {
        '+'
    };

    let bits = r.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0x7ff {
        let special = if bits != 0x7ff0_0000_0000_0000 {
            FpSpecial::Nan
        } else {
            FpSpecial::Inf
        };
        return FpDecoded {
            sign,
            special,
            digits: String::new(),
            i_dp: 0,
        };
    }

    let mut v = bits & 0x000f_ffff_ffff_ffff;
    if e == 0 {
        let nn = v.leading_zeros() as i32;
        v <<= nn as u32;
        e = -1074 - nn;
    } else {
        v = (v << 11) | (1u64 << 63);
        e -= 1086;
    }

    let n_digits = if i_round <= 0 || i_round >= 18 {
        18
    } else {
        i_round + 1
    };
    let (dv, exp) = fp2_convert10(v, e, n_digits);

    // Format dv into a small stack buffer with 2 leading spare slots (for prepended '0' and rounding carry).
    let mut tmp = [0u8; 24];
    let mut len = 0usize;
    let mut rem = dv;
    if rem == 0 {
        tmp[0] = b'0';
        len = 1;
    } else {
        while rem > 0 {
            tmp[len] = b'0' + (rem % 10) as u8;
            rem /= 10;
            len += 1;
        }
        tmp[..len].reverse();
    }

    let mut buf = [0u8; 26];
    let mut z = 2usize;
    let mut n = len;
    buf[z..z + n].copy_from_slice(&tmp[..n]);
    let mut i_dp = (n as i32) + exp;

    if i_round <= 0 {
        i_round = i_dp.saturating_sub(i_round);
        if i_round == 0 && buf[z] >= b'5' {
            i_round = 1;
            z -= 1;
            buf[z] = b'0';
            n += 1;
            i_dp += 1;
        }
    }

    if i_round > 0 && ((i_round as usize) < n || (n as i32) > mx_round) {
        if i_round > mx_round {
            i_round = mx_round;
        }
        if i_round == 17 {
            // util.c:1474-1503: 17-digit shortening heuristic.
            if buf[z + 15] == b'9' && buf[z + 14] == b'9' {
                let mut jj = 14usize;
                while jj > 0 && buf[z + jj - 1] == b'9' {
                    jj -= 1;
                }
                let v2: u64 = if jj == 0 {
                    1
                } else {
                    let mut acc = u64::from(buf[z] - b'0');
                    for kk in 1..jj {
                        acc = acc * 10 + u64::from(buf[z + kk] - b'0');
                    }
                    acc + 1
                };
                if r == fp10_convert2(v2, exp + (n as i32) - (jj as i32)) {
                    i_round = (jj + 1) as i32;
                }
            } else if i_dp >= (n as i32)
                || (buf[z + 15] == b'0' && buf[z + 14] == b'0' && buf[z + 13] == b'0')
            {
                let mut jj = 13usize;
                while jj > 0 && buf[z + jj - 1] == b'0' {
                    jj -= 1;
                }
                let mut v2 = u64::from(buf[z] - b'0');
                for kk in 1..jj {
                    v2 = v2 * 10 + u64::from(buf[z + kk] - b'0');
                }
                if r == fp10_convert2(v2, exp + (n as i32) - (jj as i32)) {
                    i_round = (jj + 1) as i32;
                }
            }
        }
        let round_idx = i_round as usize;
        n = round_idx;
        if buf[z + round_idx] >= b'5' {
            let mut j = round_idx - 1;
            loop {
                buf[z + j] += 1;
                if buf[z + j] <= b'9' {
                    break;
                }
                buf[z + j] = b'0';
                if j == 0 {
                    z -= 1;
                    buf[z] = b'1';
                    n += 1;
                    i_dp += 1;
                    break;
                }
                j -= 1;
            }
        }
    }

    while n > 0 && buf[z + n - 1] == b'0' {
        n -= 1;
    }

    let digits = String::from_utf8(buf[z..z + n].to_vec()).expect("ASCII digits");
    FpDecoded {
        sign,
        special: FpSpecial::Finite,
        digits,
        i_dp,
    }
}
