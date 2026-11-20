//! Text encodings for SQLite `TEXT` values (ported from SQLite 3.54.1 `utf.c`).

/// Database text encodings, numbered as `SQLITE_UTF8` / `SQLITE_UTF16LE` / `SQLITE_UTF16BE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TextEncoding {
    Utf8 = 1,
    Utf16Le = 2,
    Utf16Be = 3,
}

impl TextEncoding {
    #[inline]
    pub fn from_header_field(field56: u32) -> Self {
        match field56 & 3 {
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            _ => TextEncoding::Utf8,
        }
    }
}

/// `WRITE_UTF8` (`utf.c:64`): write any code value below `2^21` (surrogates included, no validation).
#[inline]
fn write_utf8_unchecked(out: &mut Vec<u8>, c: u32) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.push((0xc0 + ((c >> 6) & 0x1f)) as u8);
        out.push((0x80 + (c & 0x3f)) as u8);
    } else if c < 0x10000 {
        out.push((0xe0 + ((c >> 12) & 0x0f)) as u8);
        out.push((0x80 + ((c >> 6) & 0x3f)) as u8);
        out.push((0x80 + (c & 0x3f)) as u8);
    } else {
        out.push((0xf0 + ((c >> 18) & 0x07)) as u8);
        out.push((0x80 + ((c >> 12) & 0x3f)) as u8);
        out.push((0x80 + ((c >> 6) & 0x3f)) as u8);
        out.push((0x80 + (c & 0x3f)) as u8);
    }
}

/// `sqlite3VdbeMemTranslate` from UTF-16 to UTF-8 (`utf.c:290-345`), built without
/// `SQLITE_REPLACE_INVALID_UTF` (the default):
///
/// - a trailing odd byte is dropped;
/// - any surrogate unit `c` followed by another unit `c2` is combined with it as
///   `(c2 & 0x3FF) + ((c & 0x3F) << 10) + (((c & 0x3C0) + 0x40) << 10)` without checking
///   whether `c` is high or `c2` is low;
/// - a surrogate in the final unit is written as a 3-byte WTF-8 sequence.
///
/// SQLite uses this when a UTF-8-only collation (`NOCASE`, `RTRIM`) compares text in a
/// UTF-16 database (`vdbeaux.c:4424`).
pub fn sqlite_utf16_to_utf8(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    if enc == TextEncoding::Utf8 {
        return bytes.to_vec();
    }
    let n = bytes.len() & !1;
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let mut c = read_u16(bytes, i, le) as u32;
        i += 2;
        if (0xd800..0xe000).contains(&c) && i < n {
            let c2 = read_u16(bytes, i, le) as u32;
            i += 2;
            c = (c2 & 0x03ff) + ((c & 0x003f) << 10) + (((c & 0x03c0) + 0x0040) << 10);
        }
        write_utf8_unchecked(&mut out, c);
    }
    out
}

/// Read UTF-16 code units verbatim (dropping a trailing odd byte, as SQLite does).
pub fn utf16_decode_units(bytes: &[u8], enc: TextEncoding) -> Vec<u16> {
    let n = bytes.len() >> 1;
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(read_u16(bytes, 2 * i, le));
    }
    out
}

/// Encode UTF-16 code units into bytes in the specified byte order.
pub fn utf16_encode_units(units: &[u16], enc: TextEncoding) -> Vec<u8> {
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(units.len() * 2);
    for &u in units {
        let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
        out.extend_from_slice(&b);
    }
    out
}

/// Encode UTF-8 bytes to UTF-16LE or UTF-16BE (using WHATWG replacement for invalid UTF-8).
pub fn utf8_to_utf16_bytes(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    if enc == TextEncoding::Utf8 {
        return bytes.to_vec();
    }
    let s = String::from_utf8_lossy(bytes);
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for u in s.encode_utf16() {
        let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
        out.extend_from_slice(&b);
    }
    out
}

/// Decode bytes stored in `enc` to UTF-8 bytes:
/// - For `Utf8`, bytes are returned verbatim (preserving invalid UTF-8 bytes, matching SQLite).
/// - For `Utf16Le`/`Utf16Be`, translates via [`sqlite_utf16_to_utf8`].
pub fn decode_stored_text_to_utf8(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    match enc {
        TextEncoding::Utf8 => bytes.to_vec(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => sqlite_utf16_to_utf8(bytes, enc),
    }
}

/// Encode UTF-8 text bytes into the database's storage encoding `enc`.
pub fn encode_utf8_to_stored_text(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    match enc {
        TextEncoding::Utf8 => bytes.to_vec(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => utf8_to_utf16_bytes(bytes, enc),
    }
}

/// Decode UTF-16 bytes to a Rust `String` (unpaired surrogates become U+FFFD).
pub fn utf16_bytes_to_string_lossy(bytes: &[u8], enc: TextEncoding) -> String {
    let units = utf16_decode_units(bytes, enc);
    String::from_utf16_lossy(&units)
}

#[inline]
fn read_u16(bytes: &[u8], off: usize, le: bool) -> u16 {
    if le {
        u16::from_le_bytes([bytes[off], bytes[off + 1]])
    } else {
        u16::from_be_bytes([bytes[off], bytes[off + 1]])
    }
}
