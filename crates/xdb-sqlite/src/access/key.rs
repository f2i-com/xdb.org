//! Index key comparison: SQLite's `KeyInfo` and `UnpackedRecord`, and
//! `sqlite3VdbeRecordCompareWithSkip` (`vdbeaux.c:4709`), comparing a record
//! stored in an index B-tree with an unpacked search key.

use std::cmp::Ordering;

use xdb_core::value::collation::{
    compare_bytes_binary, compare_bytes_nocase_utf8, compare_bytes_rtrim_utf8, BuiltinCollation,
    Collation, CollationRegistry,
};
use xdb_core::value::utf::{decode_stored_text_to_utf8, encode_utf8_to_stored_text};
use xdb_core::value::int_float_compare;
use xdb_core::{SqlValue, TextEncoding};

use crate::btree::RecordKey;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{decode_fixed, get_varint32, serial_type_length};
use crate::schema::IndexInfo;

/// One record field of a [`KeyInfo`]: its collation name and direction.
#[derive(Debug, Clone)]
pub struct KeyFieldSpec<'a> {
    pub collation: &'a str,
    pub desc: bool,
}

/// How each field of an index record compares (SQLite's `KeyInfo`).
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub encoding: TextEncoding,
    pub n_key_field: usize,
    pub n_all_field: usize,
    /// Per field: `None` means `BINARY` (`memcmp` of stored bytes).
    pub collations: Vec<Option<Collation>>,
    /// Per field: `true` when the field sorts `DESC`.
    pub desc: Vec<bool>,
}

/// Resolve a collation name for `encoding` (`BINARY` returns `None`).
pub fn resolve_collation(
    name: &str,
    encoding: TextEncoding,
    registry: Option<&CollationRegistry>,
) -> SqliteResult<Option<Collation>> {
    if let Some(b) = BuiltinCollation::from_name(name) {
        return Ok(if b == BuiltinCollation::Binary {
            None
        } else {
            Some(Collation::builtin(b, encoding))
        });
    }
    if let Some(reg) = registry {
        if let Some(user) = reg.find(name) {
            return Ok(Some(user.clone()));
        }
    }
    Err(SqliteError::UnknownCollation(name.to_string()))
}

/// Build a [`KeyInfo`] for records described by `fields`.
pub fn make_key_info(
    encoding: TextEncoding,
    fields: &[KeyFieldSpec<'_>],
    n_key_field: usize,
    registry: Option<&CollationRegistry>,
) -> SqliteResult<KeyInfo> {
    if n_key_field > fields.len() {
        return Err(SqliteError::misuse(format!(
            "n_key_field {n_key_field} outside 0..={}",
            fields.len()
        )));
    }
    let mut desc = Vec::with_capacity(fields.len());
    let mut collations = Vec::with_capacity(fields.len());
    for f in fields {
        desc.push(f.desc);
        collations.push(resolve_collation(f.collation, encoding, registry)?);
    }
    Ok(KeyInfo {
        encoding,
        n_key_field,
        n_all_field: fields.len(),
        collations,
        desc,
    })
}

/// `sqlite3KeyInfoOfIndex` for an [`IndexInfo`] (or WITHOUT ROWID primary-key B-tree).
pub fn key_info_for_index(
    index: &IndexInfo,
    encoding: TextEncoding,
    registry: Option<&CollationRegistry>,
) -> SqliteResult<KeyInfo> {
    let specs: Vec<KeyFieldSpec<'_>> = index
        .columns
        .iter()
        .map(|c| KeyFieldSpec {
            collation: &c.collation,
            desc: c.desc,
        })
        .collect();
    make_key_info(encoding, &specs, index.key_count, registry)
}

#[derive(Debug, Clone)]
enum PreparedField {
    Null,
    Int(i64),
    Real(f64),
    Text {
        stored_bytes: Vec<u8>,
        utf8_bytes: Vec<u8>,
    },
    Blob(Vec<u8>),
}

#[inline]
fn byte_at(buf: &[u8], off: usize) -> u8 {
    buf.get(off).copied().unwrap_or(0)
}

#[inline]
fn read_f64_padded(buf: &[u8], off: usize) -> f64 {
    let mut b = [0u8; 8];
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = byte_at(buf, off + i);
    }
    f64::from_be_bytes(b)
}

#[inline]
fn ord_to_i32(o: Ordering) -> i32 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// An unpacked search key (SQLite's `UnpackedRecord`) for [`crate::btree::IndexCursor`].
#[derive(Debug, Clone)]
pub struct SearchKey {
    pub key_info: KeyInfo,
    pub default_rc: i8,
    pub corrupt: bool,
    pub eq_seen: bool,
    fields: Vec<PreparedField>,
}

impl SearchKey {
    pub fn new(key_info: KeyInfo, values: &[SqlValue], default_rc: i8) -> SqliteResult<Self> {
        let mut key = Self {
            key_info,
            default_rc,
            corrupt: false,
            eq_seen: false,
            fields: Vec::new(),
        };
        key.set(values, default_rc)?;
        Ok(key)
    }

    pub fn set(&mut self, values: &[SqlValue], default_rc: i8) -> SqliteResult<()> {
        if values.len() > self.key_info.n_all_field {
            return Err(SqliteError::misuse(format!(
                "a search key of {} fields for records of {}",
                values.len(),
                self.key_info.n_all_field
            )));
        }
        let enc = self.key_info.encoding;
        self.fields.clear();
        self.fields.reserve(values.len());
        for v in values {
            let f = match v {
                SqlValue::Null => PreparedField::Null,
                SqlValue::Int(i) => PreparedField::Int(*i),
                SqlValue::Real(r) => PreparedField::Real(*r),
                SqlValue::Text(b) => PreparedField::Text {
                    stored_bytes: encode_utf8_to_stored_text(b, enc),
                    utf8_bytes: b.to_vec(),
                },
                SqlValue::Blob(b) => PreparedField::Blob(b.to_vec()),
            };
            self.fields.push(f);
        }
        self.default_rc = default_rc;
        self.corrupt = false;
        self.eq_seen = false;
        Ok(())
    }

    #[inline]
    pub fn n_field(&self) -> usize {
        self.fields.len()
    }

    fn compare_collated(
        &self,
        coll: &Collation,
        rec_bytes: &[u8],
        key_stored: &[u8],
        key_utf8: &[u8],
    ) -> i32 {
        let enc = self.key_info.encoding;
        match coll {
            Collation::Builtin { kind, .. } => match (kind, enc) {
                (BuiltinCollation::Binary, _) => ord_to_i32(compare_bytes_binary(rec_bytes, key_stored)),
                (BuiltinCollation::Nocase, TextEncoding::Utf8) => {
                    ord_to_i32(compare_bytes_nocase_utf8(rec_bytes, key_stored))
                }
                (BuiltinCollation::Rtrim, TextEncoding::Utf8) => {
                    ord_to_i32(compare_bytes_rtrim_utf8(rec_bytes, key_stored))
                }
                _ => ord_to_i32(coll.compare_stored(rec_bytes, key_stored, enc)),
            },
            Collation::Custom { func, .. } => {
                if enc == TextEncoding::Utf8 {
                    ord_to_i32(func(rec_bytes, key_utf8))
                } else {
                    let rec_utf8 = decode_stored_text_to_utf8(rec_bytes, enc);
                    ord_to_i32(func(&rec_utf8, key_utf8))
                }
            }
        }
    }
}

impl RecordKey for SearchKey {
    fn compare_record(&mut self, buf: &[u8], start: usize, n: usize) -> i32 {
        let n_field = self.fields.len();
        if n_field == 0 {
            self.eq_seen = true;
            return i32::from(self.default_rc);
        }
        let b0 = byte_at(buf, start);
        let (sz_hdr, mut idx1) = if b0 < 0x80 {
            (usize::from(b0), 1usize)
        } else {
            let (v, l) = get_varint32(buf, start);
            (v as usize, l)
        };
        let mut d1 = sz_hdr;
        if d1 > n {
            self.corrupt = true;
            return 0;
        }
        let mut i = 0usize;
        loop {
            let p = start + idx1;
            let body = start + d1;
            let st: u32;
            let st_len: usize;
            let rc: i32;
            match &self.fields[i] {
                PreparedField::Int(rhs) => {
                    let sb = byte_at(buf, p);
                    if sb < 0x80 {
                        st = u32::from(sb);
                        st_len = 1;
                    } else {
                        let (v, l) = get_varint32(buf, p);
                        st = v;
                        st_len = l;
                    }
                    if st >= 10 {
                        rc = if st == 10 || st == 11 { -1 } else { 1 };
                    } else if st == 0 {
                        rc = -1;
                    } else {
                        let len = serial_type_length(st);
                        if d1.saturating_add(len) > n {
                            self.corrupt = true;
                            return 0;
                        }
                        if st == 7 {
                            rc = -ord_to_i32(int_float_compare(*rhs, read_f64_padded(buf, body)));
                        } else {
                            let lhs = match decode_fixed_padded(buf, body, st) {
                                SqlValue::Int(v) => v,
                                _ => 0,
                            };
                            rc = ord_to_i32(lhs.cmp(rhs));
                        }
                    }
                }
                PreparedField::Real(rhs) => {
                    let sb = byte_at(buf, p);
                    if sb < 0x80 {
                        st = u32::from(sb);
                        st_len = 1;
                    } else {
                        let (v, l) = get_varint32(buf, p);
                        st = v;
                        st_len = l;
                    }
                    if st >= 10 {
                        rc = if st == 10 || st == 11 { -1 } else { 1 };
                    } else if st == 0 {
                        rc = -1;
                    } else {
                        let len = serial_type_length(st);
                        if d1.saturating_add(len) > n {
                            self.corrupt = true;
                            return 0;
                        }
                        if st == 7 {
                            let r = read_f64_padded(buf, body);
                            rc = if r.is_nan() || r < *rhs {
                                -1
                            } else if r > *rhs {
                                1
                            } else {
                                0
                            };
                        } else {
                            let lhs = match decode_fixed_padded(buf, body, st) {
                                SqlValue::Int(v) => v,
                                _ => 0,
                            };
                            rc = ord_to_i32(int_float_compare(lhs, *rhs));
                        }
                    }
                }
                PreparedField::Text {
                    stored_bytes,
                    utf8_bytes,
                } => {
                    let sb = byte_at(buf, p);
                    if sb < 0x80 {
                        st = u32::from(sb);
                        st_len = 1;
                    } else {
                        let (v, l) = get_varint32(buf, p);
                        st = v;
                        st_len = l;
                    }
                    if st < 12 {
                        rc = -1;
                    } else if (st & 1) == 0 {
                        rc = 1;
                    } else {
                        let len = ((st - 12) >> 1) as usize;
                        if d1.saturating_add(len) > n || self.key_info.n_all_field <= i {
                            self.corrupt = true;
                            return 0;
                        }
                        let rec_slice = slice_padded(buf, body, len);
                        rc = match &self.key_info.collations[i] {
                            None => ord_to_i32(compare_bytes_binary(&rec_slice, stored_bytes)),
                            Some(coll) => {
                                self.compare_collated(coll, &rec_slice, stored_bytes, utf8_bytes)
                            }
                        };
                    }
                }
                PreparedField::Blob(key_bytes) => {
                    let sb = byte_at(buf, p);
                    if sb < 0x80 {
                        st = u32::from(sb);
                        st_len = 1;
                    } else {
                        let (v, l) = get_varint32(buf, p);
                        st = v;
                        st_len = l;
                    }
                    if st < 12 || (st & 1) == 1 {
                        rc = -1;
                    } else {
                        let len = ((st - 12) >> 1) as usize;
                        if d1.saturating_add(len) > n {
                            self.corrupt = true;
                            return 0;
                        }
                        let rec_slice = slice_padded(buf, body, len);
                        rc = ord_to_i32(compare_bytes_binary(&rec_slice, key_bytes));
                    }
                }
                PreparedField::Null => {
                    let sb = byte_at(buf, p);
                    if sb < 0x80 {
                        st = u32::from(sb);
                        st_len = 1;
                    } else {
                        let (v, l) = get_varint32(buf, p);
                        st = v;
                        st_len = l;
                    }
                    let is_null_like = if st == 0 || st == 10 || st == 11 {
                        true
                    } else if st == 7 {
                        if d1.saturating_add(8) > n {
                            self.corrupt = true;
                            return 0;
                        }
                        read_f64_padded(buf, body).is_nan()
                    } else {
                        false
                    };
                    rc = if is_null_like { 0 } else { 1 };
                }
            }
            if rc != 0 {
                return if self.key_info.desc[i] { -rc } else { rc };
            }
            i += 1;
            if i == n_field {
                break;
            }
            d1 = d1.saturating_add(serial_type_length(st));
            if d1 > n {
                self.corrupt = true;
                return 0;
            }
            idx1 += st_len;
            if idx1 >= sz_hdr {
                self.corrupt = true;
                return 0;
            }
        }
        self.eq_seen = true;
        i32::from(self.default_rc)
    }

    fn set_default_rc(&mut self, rc: i8) {
        self.default_rc = rc;
    }

    fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    fn clear_corrupt(&mut self) {
        self.corrupt = false;
    }
}

fn decode_fixed_padded(buf: &[u8], off: usize, st: u32) -> SqlValue {
    let len = serial_type_length(st);
    if off + len <= buf.len() {
        return decode_fixed(buf, off, st);
    }
    let mut tmp = [0u8; 8];
    for (i, slot) in tmp.iter_mut().take(len).enumerate() {
        *slot = byte_at(buf, off + i);
    }
    decode_fixed(&tmp, 0, st)
}

fn slice_padded<'a>(buf: &'a [u8], off: usize, len: usize) -> std::borrow::Cow<'a, [u8]> {
    if off.checked_add(len).is_some_and(|end| end <= buf.len()) {
        std::borrow::Cow::Borrowed(&buf[off..off + len])
    } else {
        let mut out = vec![0u8; len];
        if off < buf.len() {
            let avail = (buf.len() - off).min(len);
            out[..avail].copy_from_slice(&buf[off..off + avail]);
        }
        std::borrow::Cow::Owned(out)
    }
}
