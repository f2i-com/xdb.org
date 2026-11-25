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

/// Read one UTF-8 / WTF-8 code point from `bytes[*pos..]` matching SQLite's `sqlite3Utf8Read`:
/// preserves 3-byte WTF-8 surrogate code points (`0xD800..=0xDFFF`) so unpaired UTF-16 surrogates
/// round-trip losslessly, while mapping invalid/overlong UTF-8 sequences to `U+FFFD`.
#[inline]
pub(crate) fn read_wtf8_codepoint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let &b0 = bytes.get(*pos)?;
    *pos += 1;
    if b0 < 0x80 {
        return Some(u32::from(b0));
    }
    if b0 < 0xc0 {
        return Some(0xfffd);
    }
    if b0 < 0xe0 {
        let Some(&b1) = bytes.get(*pos) else {
            return Some(0xfffd);
        };
        if (b1 & 0xc0) != 0x80 {
            return Some(0xfffd);
        }
        *pos += 1;
        let cp = (u32::from(b0 & 0x1f) << 6) | u32::from(b1 & 0x3f);
        return Some(if cp < 0x80 { 0xfffd } else { cp });
    }
    if b0 < 0xf0 {
        let Some(&b1) = bytes.get(*pos) else {
            return Some(0xfffd);
        };
        if (b1 & 0xc0) != 0x80 {
            return Some(0xfffd);
        }
        let Some(&b2) = bytes.get(*pos + 1) else {
            *pos += 1;
            return Some(0xfffd);
        };
        if (b2 & 0xc0) != 0x80 {
            *pos += 1;
            return Some(0xfffd);
        }
        *pos += 2;
        let cp = (u32::from(b0 & 0x0f) << 12) | (u32::from(b1 & 0x3f) << 6) | u32::from(b2 & 0x3f);
        return Some(if cp < 0x800 { 0xfffd } else { cp });
    }
    if b0 < 0xf8 {
        let Some(&b1) = bytes.get(*pos) else {
            return Some(0xfffd);
        };
        if (b1 & 0xc0) != 0x80 {
            return Some(0xfffd);
        }
        let Some(&b2) = bytes.get(*pos + 1) else {
            *pos += 1;
            return Some(0xfffd);
        };
        if (b2 & 0xc0) != 0x80 {
            *pos += 1;
            return Some(0xfffd);
        }
        let Some(&b3) = bytes.get(*pos + 2) else {
            *pos += 2;
            return Some(0xfffd);
        };
        if (b3 & 0xc0) != 0x80 {
            *pos += 2;
            return Some(0xfffd);
        }
        *pos += 3;
        let cp = (u32::from(b0 & 0x07) << 18)
            | (u32::from(b1 & 0x3f) << 12)
            | (u32::from(b2 & 0x3f) << 6)
            | u32::from(b3 & 0x3f);
        return Some(if !(0x10000..=0x10ffff).contains(&cp) {
            0xfffd
        } else {
            cp
        });
    }
    // Match SQLite `sqlite3Utf8Read` (`utf.c:130`): consume any trailing continuation
    // bytes after an invalid lead byte (`>= 0xF8`) and emit a single `U+FFFD`.
    while let Some(&b) = bytes.get(*pos) {
        if (b & 0xc0) == 0x80 {
            *pos += 1;
        } else {
            break;
        }
    }
    Some(0xfffd)
}

/// Zero-allocation streaming iterator yielding UTF-16 code units from a UTF-8 / WTF-8 byte slice.
pub(crate) struct Utf8AsUtf16Units<'a> {
    bytes: &'a [u8],
    pos: usize,
    pending_lo: Option<u16>,
}

impl<'a> Utf8AsUtf16Units<'a> {
    #[inline]
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            pending_lo: None,
        }
    }
}

impl Iterator for Utf8AsUtf16Units<'_> {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<u16> {
        if let Some(lo) = self.pending_lo.take() {
            return Some(lo);
        }
        let cp = read_wtf8_codepoint(self.bytes, &mut self.pos)?;
        if cp < 0x10000 {
            Some(cp as u16)
        } else {
            let v = cp - 0x10000;
            self.pending_lo = Some((0xdc00 + (v & 0x3ff)) as u16);
            Some((0xd800 + (v >> 10)) as u16)
        }
    }
}

/// Encode UTF-8/WTF-8 bytes to UTF-16LE or UTF-16BE, preserving unpaired surrogates
/// (matching SQLite's `sqlite3VdbeMemTranslate` UTF-8 -> UTF-16).
pub fn utf8_to_utf16_bytes(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    if enc == TextEncoding::Utf8 {
        return bytes.to_vec();
    }
    let le = enc == TextEncoding::Utf16Le;
    let mut out = Vec::with_capacity(bytes.len() * 2);
    if bytes.is_ascii() {
        if le {
            for &b in bytes {
                out.push(b);
                out.push(0);
            }
        } else {
            for &b in bytes {
                out.push(0);
                out.push(b);
            }
        }
        return out;
    }
    for u in Utf8AsUtf16Units::new(bytes) {
        let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
        out.extend_from_slice(&b);
    }
    out
}

/// Lossless UTF-16 -> WTF-8 decoder (drops a trailing odd byte, matching SQLite).
///
/// Only valid surrogate pairs (`0xD800..=0xDBFF` followed by `0xDC00..=0xDFFF`) are
/// combined into a 4-byte UTF-8 supplementary scalar; unpaired high or low surrogates
/// anywhere in the stream are preserved as 3-byte WTF-8 sequences (`0xED 0xA0..0xBF ...`)
/// so round-trips (`TEXT -> BLOB` / `UTF-16 BINARY` comparisons) never corrupt adjacent code units.
pub fn utf16_to_wtf8(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
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
        if (0xd800..=0xdbff).contains(&c) && i < n {
            let c2 = read_u16(bytes, i, le) as u32;
            if (0xdc00..=0xdfff).contains(&c2) {
                i += 2;
                c = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
            }
        }
        write_utf8_unchecked(&mut out, c);
    }
    out
}

/// Decode bytes stored in `enc` to UTF-8 / WTF-8 bytes:
/// - For `Utf8`, bytes are returned verbatim (preserving invalid UTF-8 bytes, matching SQLite).
/// - For `Utf16Le`/`Utf16Be`, decodes losslessly to WTF-8 via [`utf16_to_wtf8`].
pub fn decode_stored_text_to_utf8(bytes: &[u8], enc: TextEncoding) -> Vec<u8> {
    match enc {
        TextEncoding::Utf8 => bytes.to_vec(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => utf16_to_wtf8(bytes, enc),
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
