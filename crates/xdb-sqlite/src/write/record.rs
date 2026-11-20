//! SQLite 3 binary varint and record encoder (`fileformat2 §2.1`, `vdbeaux.c`).

use xdb_core::value::utf::encode_utf8_to_stored_text;
use xdb_core::{SqlValue, TextEncoding};

/// Write an unsigned 64-bit integer as a 1..=9 byte SQLite Huffman varint (`sqlite3PutVarint`).
pub fn put_varint(v: u64, out: &mut Vec<u8>) {
    if v <= 0x7f {
        out.push(v as u8);
        return;
    }
    if v <= 0x3fff {
        out.push(((v >> 7) as u8) | 0x80);
        out.push((v & 0x7f) as u8);
        return;
    }
    // High bit set (values >= 2^56) use 9 bytes, with all 8 bits in the 9th byte.
    if (v & (0xff00_0000_0000_0000)) != 0 {
        let mut buf = [0u8; 9];
        buf[8] = (v & 0xff) as u8;
        let mut x = v >> 8;
        for i in (0..8).rev() {
            buf[i] = ((x & 0x7f) as u8) | 0x80;
            x >>= 7;
        }
        out.extend_from_slice(&buf);
        return;
    }
    let mut buf = [0u8; 9];
    let mut n = 0usize;
    let mut x = v;
    while x > 0 {
        buf[n] = ((x & 0x7f) as u8) | 0x80;
        x >>= 7;
        n += 1;
    }
    buf[0] &= 0x7f;
    for i in (0..n).rev() {
        out.push(buf[i]);
    }
}

/// Number of bytes `put_varint(v)` produces.
#[inline]
pub fn varint_size(v: u64) -> usize {
    if v <= 0x7f {
        1
    } else if v <= 0x3fff {
        2
    } else if (v & 0xff00_0000_0000_0000) != 0 {
        9
    } else {
        let mut x = v;
        let mut n = 0;
        while x > 0 {
            x >>= 7;
            n += 1;
        }
        n
    }
}

/// Encode `values` as a SQLite 3 binary record (`sqlite3VdbeMakeRecord`) for database
/// encoding `enc` (assuming schema format 4, where `0` and `1` use serial types 8 and 9).
pub fn encode_record(values: &[SqlValue], enc: TextEncoding) -> Vec<u8> {
    let mut serials: Vec<u64> = Vec::with_capacity(values.len());
    let mut encoded_texts: Vec<Option<Vec<u8>>> = Vec::with_capacity(values.len());
    let mut header_body_len = 0usize;
    let mut payload_body_len = 0usize;

    for v in values {
        let (st, blen, txt) = match v {
            SqlValue::Null => (0u64, 0usize, None),
            SqlValue::Int(0) => (8u64, 0usize, None),
            SqlValue::Int(1) => (9u64, 0usize, None),
            SqlValue::Int(i) => {
                let (s, l) = int_serial_type(*i);
                (s, l, None)
            }
            SqlValue::Real(_) => (7u64, 8usize, None),
            SqlValue::Text(bytes) => {
                if enc == TextEncoding::Utf8 {
                    let len = bytes.len();
                    (13 + 2 * (len as u64), len, None)
                } else {
                    let stored = encode_utf8_to_stored_text(bytes, enc);
                    let len = stored.len();
                    (13 + 2 * (len as u64), len, Some(stored))
                }
            }
            SqlValue::Blob(bytes) => {
                let len = bytes.len();
                (12 + 2 * (len as u64), len, None)
            }
        };
        serials.push(st);
        encoded_texts.push(txt);
        header_body_len += varint_size(st);
        payload_body_len += blen;
    }

    // Total header size includes its own leading varint length.
    let mut hdr_len = header_body_len + 1;
    let hdr_len_varint_sz = varint_size(hdr_len as u64);
    if hdr_len_varint_sz > 1 {
        hdr_len = header_body_len + hdr_len_varint_sz;
        let second_sz = varint_size(hdr_len as u64);
        if second_sz > hdr_len_varint_sz {
            hdr_len = header_body_len + second_sz;
        }
    }

    let mut out = Vec::with_capacity(hdr_len + payload_body_len);
    put_varint(hdr_len as u64, &mut out);
    for &st in &serials {
        put_varint(st, &mut out);
    }

    for (idx, v) in values.iter().enumerate() {
        match v {
            SqlValue::Null | SqlValue::Int(0) | SqlValue::Int(1) => {}
            SqlValue::Int(i) => write_int_be(*i, serials[idx], &mut out),
            SqlValue::Real(r) => out.extend_from_slice(&r.to_bits().to_be_bytes()),
            SqlValue::Text(bytes) => {
                if let Some(ref stored) = encoded_texts[idx] {
                    out.extend_from_slice(stored);
                } else {
                    out.extend_from_slice(bytes);
                }
            }
            SqlValue::Blob(bytes) => out.extend_from_slice(bytes),
        }
    }

    out
}

#[inline]
fn int_serial_type(i: i64) -> (u64, usize) {
    if (-128..=127).contains(&i) {
        (1, 1)
    } else if (-32768..=32767).contains(&i) {
        (2, 2)
    } else if (-8_388_608..=8_388_607).contains(&i) {
        (3, 3)
    } else if (-2_147_483_648..=2_147_483_647).contains(&i) {
        (4, 4)
    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&i) {
        (5, 6)
    } else {
        (6, 8)
    }
}

#[inline]
fn write_int_be(i: i64, serial_type: u64, out: &mut Vec<u8>) {
    let be = i.to_be_bytes();
    match serial_type {
        1 => out.push(be[7]),
        2 => out.extend_from_slice(&be[6..8]),
        3 => out.extend_from_slice(&be[5..8]),
        4 => out.extend_from_slice(&be[4..8]),
        5 => out.extend_from_slice(&be[2..8]),
        6 => out.extend_from_slice(&be[0..8]),
        _ => {}
    }
}
