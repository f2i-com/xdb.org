//! Record format: serial types and value decoding (`fileformat2 §2.1`, `vdbeaux.c:3983-4094`).

use xdb_core::value::utf::decode_stored_text_to_utf8;
use xdb_core::{SqlValue, StorageClass, TextEncoding};

/// Named fixed serial-type codes (`0..=11`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum SerialType {
    Null = 0,
    Int8 = 1,
    Int16 = 2,
    Int24 = 3,
    Int32 = 4,
    Int48 = 5,
    Int64 = 6,
    Float64 = 7,
    Zero = 8,
    One = 9,
    Reserved10 = 10,
    Reserved11 = 11,
}

const SMALL_SIZES: [usize; 12] = [0, 1, 2, 3, 4, 6, 8, 8, 0, 0, 0, 0];

/// `sqlite3VdbeSerialTypeLen`: body byte length of serial type `t`.
#[inline]
pub const fn serial_type_length(t: u32) -> usize {
    if t >= 12 {
        ((t - 12) >> 1) as usize
    } else {
        SMALL_SIZES[t as usize]
    }
}

/// Storage class a serial type decodes to (ignoring `NaN` for type 7, which reads as `NULL`).
#[inline]
pub const fn serial_type_storage(t: u32) -> StorageClass {
    if t >= 12 {
        if (t & 1) == 1 {
            StorageClass::Text
        } else {
            StorageClass::Blob
        }
    } else if t == 7 {
        StorageClass::Real
    } else if t == 0 || t == 10 || t == 11 {
        StorageClass::Null
    } else {
        StorageClass::Integer
    }
}

/// Decode a fixed-size serial type (`0..=11`) starting at `b[off..]`.
pub fn decode_fixed(b: &[u8], off: usize, t: u32) -> SqlValue {
    match t {
        0 | 10 | 11 => SqlValue::Null,
        1 => SqlValue::Int(i64::from(b[off] as i8)),
        2 => SqlValue::Int(i64::from(i16::from_be_bytes([b[off], b[off + 1]]))),
        3 => {
            let sign = if (b[off] & 0x80) != 0 { 0xffu8 } else { 0x00u8 };
            SqlValue::Int(i64::from(i32::from_be_bytes([
                sign,
                b[off],
                b[off + 1],
                b[off + 2],
            ])))
        }
        4 => SqlValue::Int(i64::from(i32::from_be_bytes([
            b[off],
            b[off + 1],
            b[off + 2],
            b[off + 3],
        ]))),
        5 => {
            let sign = if (b[off] & 0x80) != 0 { 0xffu8 } else { 0x00u8 };
            SqlValue::Int(i64::from_be_bytes([
                sign,
                sign,
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
                b[off + 4],
                b[off + 5],
            ]))
        }
        6 => SqlValue::Int(i64::from_be_bytes([
            b[off],
            b[off + 1],
            b[off + 2],
            b[off + 3],
            b[off + 4],
            b[off + 5],
            b[off + 6],
            b[off + 7],
        ])),
        7 => {
            let bits = u64::from_be_bytes([
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
                b[off + 4],
                b[off + 5],
                b[off + 6],
                b[off + 7],
            ]);
            SqlValue::real(f64::from_bits(bits))
        }
        8 => SqlValue::Int(0),
        9 => SqlValue::Int(1),
        _ => panic!("serial type {t} has a variable-length body"),
    }
}

/// Decode any serial type from a contiguous body slice `bytes`.
pub fn decode_serial_value(bytes: &[u8], t: u32, enc: TextEncoding) -> SqlValue {
    if t < 12 {
        return decode_fixed(bytes, 0, t);
    }
    if (t & 1) == 1 {
        SqlValue::Text(decode_stored_text_to_utf8(bytes, enc).into_boxed_slice())
    } else {
        SqlValue::Blob(bytes.to_vec().into_boxed_slice())
    }
}
