//! SQLite 3 binary varint and record encoder (`fileformat2 §2.1`, `vdbeaux.c`).

use xdb_core::value::utf::encode_utf8_to_stored_text;
use xdb_core::{SqlValue, TextEncoding};

/// Write an unsigned 64-bit integer as a 1..=9 byte SQLite Huffman varint into a fixed slice,
/// returning the number of bytes written. `buf` must have at least 9 bytes available.
#[inline]
pub fn put_varint_slice(v: u64, buf: &mut [u8]) -> usize {
    if v <= 0x7f {
        buf[0] = v as u8;
        return 1;
    }
    if v <= 0x3fff {
        buf[0] = ((v >> 7) as u8) | 0x80;
        buf[1] = (v & 0x7f) as u8;
        return 2;
    }
    if (v & 0xff00_0000_0000_0000) != 0 {
        buf[8] = (v & 0xff) as u8;
        let mut x = v >> 8;
        for i in (0..8).rev() {
            buf[i] = ((x & 0x7f) as u8) | 0x80;
            x >>= 7;
        }
        return 9;
    }
    let mut tmp = [0u8; 9];
    let mut n = 0usize;
    let mut x = v;
    while x > 0 {
        tmp[n] = ((x & 0x7f) as u8) | 0x80;
        x >>= 7;
        n += 1;
    }
    tmp[0] &= 0x7f;
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}

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
    encode_record_with_format(values, enc, 4)
}

/// Encode `values` as a SQLite 3 binary record honoring `schema_format` (`< 4` avoids
/// serial types 8 and 9 for integer `0` and `1`).
pub fn encode_record_with_format(
    values: &[SqlValue],
    enc: TextEncoding,
    schema_format: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_record_with_format_into(values, enc, schema_format, &mut out);
    out
}

/// Encode `values` as a SQLite 3 binary record into a reusable `out` buffer (clears `out` first).
pub fn encode_record_with_format_into(
    values: &[SqlValue],
    enc: TextEncoding,
    schema_format: u32,
    out: &mut Vec<u8>,
) {
    encode_record_iter_into(values.len(), values.iter(), enc, schema_format, out);
}

/// Encode borrowed `&SqlValue` slices into a reusable `out` buffer without cloning `Text`/`Blob` values.
pub fn encode_record_refs_into(
    values: &[&SqlValue],
    enc: TextEncoding,
    schema_format: u32,
    out: &mut Vec<u8>,
) {
    encode_record_iter_into(values.len(), values.iter().copied(), enc, schema_format, out);
}

fn encode_record_iter_into<'a, I>(
    n: usize,
    values: I,
    enc: TextEncoding,
    schema_format: u32,
    out: &mut Vec<u8>,
) where
    I: Iterator<Item = &'a SqlValue> + Clone,
{
    out.clear();
    let mut stack_serials = [0u64; 32];
    let mut heap_serials: Vec<u64> = if n > 32 {
        vec![0u64; n]
    } else {
        Vec::new()
    };
    let serials: &mut [u64] = if n > 32 {
        &mut heap_serials
    } else {
        &mut stack_serials[..n]
    };

    let mut encoded_texts: Vec<Option<Vec<u8>>> = if enc == TextEncoding::Utf8 {
        Vec::new()
    } else {
        Vec::with_capacity(n)
    };
    let mut header_body_len = 0usize;
    let mut payload_body_len = 0usize;

    for (idx, v) in values.clone().enumerate() {
        let (st, blen) = match v {
            SqlValue::Null => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                (0u64, 0usize)
            }
            SqlValue::Int(0) if schema_format >= 4 => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                (8u64, 0usize)
            }
            SqlValue::Int(1) if schema_format >= 4 => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                (9u64, 0usize)
            }
            SqlValue::Int(i) => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                int_serial_type(*i)
            }
            SqlValue::Real(r) if r.is_nan() => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                (0u64, 0usize)
            }
            SqlValue::Real(_) => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                (7u64, 8usize)
            }
            SqlValue::Text(bytes) => {
                if enc == TextEncoding::Utf8 {
                    let len = bytes.len();
                    (13 + 2 * (len as u64), len)
                } else {
                    let stored = encode_utf8_to_stored_text(bytes, enc);
                    let len = stored.len();
                    encoded_texts.push(Some(stored));
                    (13 + 2 * (len as u64), len)
                }
            }
            SqlValue::Blob(bytes) => {
                if enc != TextEncoding::Utf8 {
                    encoded_texts.push(None);
                }
                let len = bytes.len();
                (12 + 2 * (len as u64), len)
            }
        };
        serials[idx] = st;
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

    out.reserve(hdr_len + payload_body_len);
    put_varint(hdr_len as u64, out);
    for &st in serials.iter() {
        put_varint(st, out);
    }

    for (idx, v) in values.enumerate() {
        match v {
            SqlValue::Null => {}
            SqlValue::Int(_) if matches!(serials[idx], 8 | 9) => {}
            SqlValue::Int(i) => write_int_be(*i, serials[idx], out),
            SqlValue::Real(r) if r.is_nan() => {}
            SqlValue::Real(r) => out.extend_from_slice(&r.to_bits().to_be_bytes()),
            SqlValue::Text(bytes) => {
                if let Some(Some(ref stored)) = encoded_texts.get(idx) {
                    out.extend_from_slice(stored);
                } else {
                    out.extend_from_slice(bytes);
                }
            }
            SqlValue::Blob(bytes) => out.extend_from_slice(bytes),
        }
    }
}

/// Encode a 2-integer SQLite record `(a, b)` directly into `buf` (returns written byte length <= 19).
#[inline]
pub(crate) fn encode_two_int_record_stack(
    a: i64,
    b: i64,
    schema_format: u32,
    buf: &mut [u8; 20],
) -> usize {
    let (st1, l1) = if schema_format >= 4 && a == 0 {
        (8u64, 0usize)
    } else if schema_format >= 4 && a == 1 {
        (9u64, 0usize)
    } else {
        int_serial_type(a)
    };
    let (st2, l2) = if schema_format >= 4 && b == 0 {
        (8u64, 0usize)
    } else if schema_format >= 4 && b == 1 {
        (9u64, 0usize)
    } else {
        int_serial_type(b)
    };
    buf[0] = 3;
    buf[1] = st1 as u8;
    buf[2] = st2 as u8;
    let mut pos = 3;
    if l1 > 0 {
        let be = a.to_be_bytes();
        buf[pos..pos + l1].copy_from_slice(&be[8 - l1..8]);
        pos += l1;
    }
    if l2 > 0 {
        let be = b.to_be_bytes();
        buf[pos..pos + l2].copy_from_slice(&be[8 - l2..8]);
        pos += l2;
    }
    pos
}

/// Splice a single UTF-8 record column `target_sidx` inside raw `old_payload` into `out`
/// without decoding or allocating untouched columns. Returns `Ok(true)` on success, or
/// `Ok(false)` if `target_sidx` is absent in `old_payload` (e.g. `ALTER TABLE ADD COLUMN` row).
pub(crate) fn splice_single_utf8_record_field(
    old_payload: &[u8],
    target_sidx: usize,
    new_val: &SqlValue,
    schema_format: u32,
    out: &mut Vec<u8>,
) -> bool {
    use crate::format::{get_varint32, serial_type_length};
    if old_payload.is_empty() {
        return false;
    }
    let b0 = old_payload[0];
    let (old_hdr_len_u32, hdr_start) = if b0 < 0x80 {
        (u32::from(b0), 1usize)
    } else {
        get_varint32(old_payload, 0)
    };
    let old_hdr_len = old_hdr_len_u32 as usize;
    if old_hdr_len < hdr_start || old_hdr_len > old_payload.len() {
        return false;
    }

    let mut hpos = hdr_start;
    let mut boff = old_hdr_len;
    let mut col = 0usize;
    let mut found: Option<(usize, usize, usize, usize)> = None;
    while hpos < old_hdr_len {
        let st_start = hpos;
        let b = old_payload[hpos];
        let (st, n) = if b < 0x80 {
            (u32::from(b), 1usize)
        } else {
            get_varint32(old_payload, hpos)
        };
        hpos += n;
        if hpos > old_hdr_len {
            return false;
        }
        let len = serial_type_length(st);
        let Some(next_boff) = boff.checked_add(len) else {
            return false;
        };
        if next_boff > old_payload.len() {
            return false;
        }
        if col == target_sidx {
            found = Some((st_start, hpos, boff, next_boff));
            break;
        }
        boff = next_boff;
        col += 1;
    }
    let Some((st_start, st_end, val_start, val_end)) = found else {
        return false;
    };

    let mut stack_body = [0u8; 8];
    let (new_st, new_body): (u64, &[u8]) = match new_val {
        SqlValue::Null => (0, &[]),
        SqlValue::Int(0) if schema_format >= 4 => (8, &[]),
        SqlValue::Int(1) if schema_format >= 4 => (9, &[]),
        SqlValue::Int(i) => {
            let (st, l) = int_serial_type(*i);
            let be = i.to_be_bytes();
            stack_body[..l].copy_from_slice(&be[8 - l..8]);
            (st, &stack_body[..l])
        }
        SqlValue::Real(r) if r.is_nan() => (0, &[]),
        SqlValue::Real(r) => {
            stack_body = r.to_bits().to_be_bytes();
            (7, &stack_body[..8])
        }
        SqlValue::Text(bytes) => (13 + 2 * (bytes.len() as u64), bytes.as_ref()),
        SqlValue::Blob(bytes) => (12 + 2 * (bytes.len() as u64), bytes.as_ref()),
    };

    let mut new_st_buf = [0u8; 9];
    let new_st_len = put_varint_slice(new_st, &mut new_st_buf);
    let old_hdr_body = old_hdr_len - hdr_start;
    let new_hdr_body = old_hdr_body - (st_end - st_start) + new_st_len;
    let mut new_hdr_len = new_hdr_body + 1;
    let v1 = varint_size(new_hdr_len as u64);
    if v1 > 1 {
        new_hdr_len = new_hdr_body + v1;
        let v2 = varint_size(new_hdr_len as u64);
        if v2 > v1 {
            new_hdr_len = new_hdr_body + v2;
        }
    }

    let new_body_total = (old_payload.len() - old_hdr_len) - (val_end - val_start) + new_body.len();
    out.clear();
    out.reserve(new_hdr_len + new_body_total);
    put_varint(new_hdr_len as u64, out);
    out.extend_from_slice(&old_payload[hdr_start..st_start]);
    out.extend_from_slice(&new_st_buf[..new_st_len]);
    out.extend_from_slice(&old_payload[st_end..old_hdr_len]);
    out.extend_from_slice(&old_payload[old_hdr_len..val_start]);
    out.extend_from_slice(new_body);
    out.extend_from_slice(&old_payload[val_end..]);
    true
}

/// Zero-copy borrowed scalar value for direct UTF-8 binary record encoding (used by streaming
/// `seed_json` to avoid allocating `String` / `Box<[u8]>` / `SqlValue` per field).
#[derive(Debug, Clone, Copy)]
pub enum BorrowedRecordVal<'a> {
    Null,
    Int(i64),
    Real(f64),
    Text(&'a [u8]),
}

/// Encode up to 16 borrowed UTF-8 scalar values straight into `out` (clears `out` first).
pub fn encode_borrowed_record_utf8_into(
    values: &[BorrowedRecordVal<'_>],
    schema_format: u32,
    out: &mut Vec<u8>,
) {
    debug_assert!(values.len() <= 16);
    out.clear();
    let n = values.len();
    let mut serials = [0u64; 16];
    let mut header_body_len = 0usize;
    let mut payload_body_len = 0usize;
    for (idx, v) in values.iter().enumerate() {
        let (st, blen) = match *v {
            BorrowedRecordVal::Null => (0u64, 0usize),
            BorrowedRecordVal::Int(0) if schema_format >= 4 => (8u64, 0usize),
            BorrowedRecordVal::Int(1) if schema_format >= 4 => (9u64, 0usize),
            BorrowedRecordVal::Int(i) => int_serial_type(i),
            BorrowedRecordVal::Real(r) if r.is_nan() => (0u64, 0usize),
            BorrowedRecordVal::Real(_) => (7u64, 8usize),
            BorrowedRecordVal::Text(b) => (13 + 2 * (b.len() as u64), b.len()),
        };
        serials[idx] = st;
        header_body_len += varint_size(st);
        payload_body_len += blen;
    }
    let mut hdr_len = header_body_len + 1;
    let v1 = varint_size(hdr_len as u64);
    if v1 > 1 {
        hdr_len = header_body_len + v1;
        let v2 = varint_size(hdr_len as u64);
        if v2 > v1 {
            hdr_len = header_body_len + v2;
        }
    }
    out.reserve(hdr_len + payload_body_len);
    put_varint(hdr_len as u64, out);
    for &st in &serials[..n] {
        put_varint(st, out);
    }
    for (idx, v) in values.iter().enumerate() {
        match *v {
            BorrowedRecordVal::Null => {}
            BorrowedRecordVal::Int(_) if matches!(serials[idx], 8 | 9) => {}
            BorrowedRecordVal::Int(i) => write_int_be(i, serials[idx], out),
            BorrowedRecordVal::Real(r) if r.is_nan() => {}
            BorrowedRecordVal::Real(r) => out.extend_from_slice(&r.to_bits().to_be_bytes()),
            BorrowedRecordVal::Text(b) => out.extend_from_slice(b),
        }
    }
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
