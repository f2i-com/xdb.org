//! SQLite variable-length integers (`fileformat2 §1.6`; `util.c`).
//!
//! 1..=9 bytes, big-endian 7-bit groups with bit 7 as continuation; the 9th byte
//! contributes all 8 bits. Out-of-bounds bytes read as `0` so truncated varints
//! never panic (callers validate extent afterwards, matching SQLite).

/// Longest possible SQLite varint in bytes.
pub const VARINT_MAX_LENGTH: usize = 9;

#[inline]
fn byte_at(b: &[u8], idx: usize) -> u8 {
    b.get(idx).copied().unwrap_or(0)
}

/// `sqlite3GetVarint` (`util.c:1627`): decode the varint at `b[off]` as a signed `i64`
/// and return `(value, byte_len)`.
#[inline]
pub fn get_varint(b: &[u8], off: usize) -> (i64, usize) {
    let x = byte_at(b, off);
    if x < 0x80 {
        return (i64::from(x), 1);
    }
    let mut v = u64::from(x & 0x7f);
    for i in 1..8usize {
        let byte = byte_at(b, off + i);
        v = (v << 7) | u64::from(byte & 0x7f);
        if byte < 0x80 {
            return (v as i64, i + 1);
        }
    }
    let last = byte_at(b, off + 8);
    v = (v << 8) | u64::from(last);
    (v as i64, 9)
}

/// Byte length of the varint at `b[off]` without decoding (`1..=9`).
#[inline]
pub fn varint_len_at(b: &[u8], off: usize) -> usize {
    for i in 0..8usize {
        if (byte_at(b, off + i) & 0x80) == 0 {
            return i + 1;
        }
    }
    9
}

/// `sqlite3GetVarint32` (`util.c:1713`): decode a varint as `u32`; values that do not
/// fit in `0..=u32::MAX` saturate to `0xffff_ffff`. Returns `(value, byte_len)`.
#[inline]
pub fn get_varint32(b: &[u8], off: usize) -> (u32, usize) {
    let x = byte_at(b, off);
    if x < 0x80 {
        return (u32::from(x), 1);
    }
    let y = byte_at(b, off + 1);
    if y < 0x80 {
        let v = (u32::from(x & 0x7f) << 7) | u32::from(y);
        return (v, 2);
    }
    let z = byte_at(b, off + 2);
    if z < 0x80 {
        let v = (u32::from(x & 0x7f) << 14) | (u32::from(y & 0x7f) << 7) | u32::from(z);
        return (v, 3);
    }
    let (w, len) = get_varint(b, off);
    let u = w as u64;
    let v = if u <= u64::from(u32::MAX) {
        u as u32
    } else {
        0xffff_ffff
    };
    (v, len)
}

/// `sqlite3VarintLen`: bytes needed to encode `v` (`1..=9`).
#[inline]
pub fn varint_len(v: i64) -> usize {
    let mut u = v as u64;
    let mut n = 1usize;
    while {
        u >>= 7;
        u != 0
    } {
        n += 1;
    }
    n.min(9)
}

/// `sqlite3PutVarint` (`util.c:1595`): encode `v` at `b[off..]` and return bytes written (`1..=9`).
/// Panics if `b.len() - off < varint_len(v)`.
pub fn put_varint(b: &mut [u8], off: usize, v: i64) -> usize {
    let mut u = v as u64;
    if u <= 0x7f {
        b[off] = u as u8;
        return 1;
    }
    if u <= 0x3fff {
        b[off] = ((u >> 7) as u8 & 0x7f) | 0x80;
        b[off + 1] = (u & 0x7f) as u8;
        return 2;
    }
    if (u & 0xff00_0000_0000_0000) != 0 {
        b[off + 8] = (u & 0xff) as u8;
        u >>= 8;
        for i in (0..8usize).rev() {
            b[off + i] = ((u & 0x7f) as u8) | 0x80;
            u >>= 7;
        }
        return 9;
    }
    let mut tmp = [0u8; 9];
    let mut n = 0usize;
    loop {
        tmp[n] = ((u & 0x7f) as u8) | 0x80;
        n += 1;
        u >>= 7;
        if u == 0 {
            break;
        }
    }
    tmp[0] &= 0x7f;
    for i in 0..n {
        b[off + i] = tmp[n - 1 - i];
    }
    n
}

/// Payload-size varint of a B-tree cell (`btreeParseCellPtr`, `btree.c:1305`):
/// 7 bits from each of up to 9 bytes (the 9th byte does NOT contribute 8 bits), wrapped to `u32`.
#[inline]
pub fn get_payload_size_varint(b: &[u8], off: usize) -> (u32, usize) {
    let mut x = byte_at(b, off);
    if x < 0x80 {
        return (u32::from(x), 1);
    }
    let mut n = u32::from(x & 0x7f);
    let mut i = 0usize;
    loop {
        i += 1;
        x = byte_at(b, off + i);
        n = n.wrapping_shl(7) | u32::from(x & 0x7f);
        if x < 0x80 || i >= 8 {
            break;
        }
    }
    (n, i + 1)
}
