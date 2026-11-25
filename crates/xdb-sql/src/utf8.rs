//! Byte-level SQLite UTF-8 primitives (`utf.c` / `printf.c` `StrAccum`).
//!
//! Because `xdb_core::SqlValue::Text` already stores raw UTF-8 bytes (`Box<[u8]>`),
//! every SQLite string function operates directly on raw byte slices with exact
//! `sqlite3Utf8Read` / `SQLITE_SKIP_UTF8` semantics, preserving invalid UTF-8 bytes
//! identically to SQLite C.

use std::borrow::Cow;

use xdb_core::value::number_to_text;
use xdb_core::{Meter, SqlValue};

use crate::errors::{SqlError, SqlResult};

/// Default SQLite `SQLITE_MAX_LENGTH` (`1_000_000_000`).
pub const DEFAULT_MAX_LENGTH: usize = 1_000_000_000;
/// Default SQLite `SQLITE_MAX_LIKE_PATTERN_LENGTH` (`50_000`).
pub const DEFAULT_MAX_LIKE_PATTERN_LENGTH: usize = 50_000;

/// `sqlite3Utf8Trans1` (`utf.c:52`).
const TRANS1: [u8; 64] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
];

/// Returns `sqlite3_value_text()` bytes for `v` (or `None` for `NULL`).
/// `TEXT` and `BLOB` borrow their raw bytes without copying; numbers render via `number_to_text`.
#[inline]
pub fn text_bytes_of(v: &SqlValue) -> Option<Cow<'_, [u8]>> {
    match v {
        SqlValue::Null => None,
        SqlValue::Text(b) | SqlValue::Blob(b) => Some(Cow::Borrowed(b)),
        SqlValue::Int(_) | SqlValue::Real(_) => {
            number_to_text(v).map(|s| Cow::Owned(s.into_bytes()))
        }
    }
}

/// `sqlite3_value_bytes()` in UTF-8: byte length of `v`'s text/blob form, `0` for `NULL`.
#[inline]
pub fn value_byte_len(v: &SqlValue) -> usize {
    match v {
        SqlValue::Null => 0,
        SqlValue::Text(b) | SqlValue::Blob(b) => b.len(),
        SqlValue::Int(_) | SqlValue::Real(_) => number_to_text(v).map_or(0, |s| s.len()),
    }
}

/// `sqlite3Utf8Read` (`utf.c:173`): reads one SQLite UTF-8 codepoint starting at `b[i]`,
/// returning `(codepoint, next_index)`. Past end returns `(0, i + 1)`.
/// Overlong sequences and UTF-16 surrogates decode to `U+FFFD`.
#[inline]
pub fn utf8_read(b: &[u8], mut i: usize) -> (u32, usize) {
    let n = b.len();
    let mut c = if i < n { u32::from(b[i]) } else { 0 };
    i += 1;
    if c >= 0xc0 {
        c = u32::from(TRANS1[(c - 0xc0) as usize]);
        while i < n && (b[i] & 0xc0) == 0x80 {
            c = (c << 6).wrapping_add(u32::from(b[i] & 0x3f));
            i += 1;
        }
        if c < 0x80 || (c & 0xffff_f800) == 0xd800 {
            c = 0xfffd;
        }
    }
    (c, i)
}

/// `SQLITE_SKIP_UTF8` (`sqliteInt.h:4721`): returns byte index after the character at `i`.
#[inline]
pub fn skip_utf8(b: &[u8], mut i: usize) -> usize {
    if i >= b.len() {
        return i;
    }
    let lead = b[i];
    i += 1;
    if lead >= 0xc0 {
        while i < b.len() && (b[i] & 0xc0) == 0x80 {
            i += 1;
        }
    }
    i
}

/// Index of the first NUL byte in `b`, or `b.len()`.
#[inline]
pub fn c_str_len(b: &[u8]) -> usize {
    b.iter().position(|&x| x == 0).unwrap_or(b.len())
}

/// `sqlite3Utf8CharLen(z, -1)` (`utf.c:471`): count characters up to the first NUL byte.
#[inline]
pub fn utf8_char_len_cstr(b: &[u8]) -> usize {
    let mut r = 0;
    let mut i = 0;
    while i < b.len() && b[i] != 0 {
        i = skip_utf8(b, i);
        r += 1;
    }
    r
}

/// Count characters across all `n` bytes (stopping at `b.len()`, not at embedded NULs).
#[inline]
pub fn utf8_char_len_bytes(b: &[u8]) -> usize {
    let mut r = 0;
    let mut i = 0;
    while i < b.len() {
        i = skip_utf8(b, i);
        r += 1;
    }
    r
}

/// `sqlite3AppendOneUtf8Character` (`utf.c:114`): appends 1..=4 byte UTF-8 encoding of `v`.
pub fn append_utf8_raw(out: &mut Vec<u8>, v: u32) {
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x800 {
        out.push((0xc0 + ((v >> 6) & 0x1f)) as u8);
        out.push((0x80 + (v & 0x3f)) as u8);
    } else if v < 0x1_0000 {
        out.push((0xe0 + ((v >> 12) & 0x0f)) as u8);
        out.push((0x80 + ((v >> 6) & 0x3f)) as u8);
        out.push((0x80 + (v & 0x3f)) as u8);
    } else {
        out.push((0xf0 + ((v >> 18) & 0x07)) as u8);
        out.push((0x80 + ((v >> 12) & 0x3f)) as u8);
        out.push((0x80 + ((v >> 6) & 0x3f)) as u8);
        out.push((0x80 + (v & 0x3f)) as u8);
    }
}

/// RAII cumulative memory reservation guard for materialized buffers/rows.
#[derive(Debug)]
pub struct MemoryGuard<'m> {
    meter: Option<&'m dyn Meter>,
    reserved: u64,
}

impl<'m> MemoryGuard<'m> {
    #[inline]
    pub fn new(meter: Option<&'m dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    #[inline]
    pub fn reserve(&mut self, bytes: u64) -> SqlResult<()> {
        if bytes > 0 {
            if let Some(m) = self.meter {
                m.reserve_memory(bytes)?;
            }
            self.reserved = self.reserved.saturating_add(bytes);
        }
        Ok(())
    }

    #[inline]
    pub fn release(&mut self, bytes: u64) {
        let take = bytes.min(self.reserved);
        if take > 0 {
            if let Some(m) = self.meter {
                m.release_memory(take);
            }
            self.reserved -= take;
        }
    }
}

impl Drop for MemoryGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(m) = self.meter {
                m.release_memory(self.reserved);
            }
            self.reserved = 0;
        }
    }
}

/// Conservative heap size estimate for a single [`SqlValue`].
#[inline]
pub fn estimate_value_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Null | SqlValue::Int(_) | SqlValue::Real(_) => 24,
        SqlValue::Text(b) | SqlValue::Blob(b) => 24 + b.len() as u64,
    }
}

/// Conservative heap size estimate for one materialized row.
#[inline]
pub fn estimate_row_bytes(row: &[SqlValue]) -> u64 {
    let mut bytes = 32u64;
    for v in row {
        bytes = bytes.saturating_add(estimate_value_bytes(v));
    }
    bytes
}

/// Length-limited and meter-aware byte accumulator (SQLite `StrAccum`).
#[derive(Debug)]
pub struct ByteAccum<'m> {
    pub buf: Vec<u8>,
    pub limit: usize,
    /// True for `StrAccum` (`printf`, `quote`, `group_concat`) which reserves 1 byte for NUL.
    pub str_accum: bool,
    meter: Option<&'m dyn Meter>,
    charged: usize,
    step_pending: usize,
}

impl<'m> ByteAccum<'m> {
    pub fn new(limit: usize, str_accum: bool, meter: Option<&'m dyn Meter>) -> Self {
        Self {
            buf: Vec::new(),
            limit,
            str_accum,
            meter,
            charged: 0,
            step_pending: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    #[inline]
    pub fn exceeds(&self, extra: usize) -> bool {
        let extra_nul = usize::from(self.str_accum);
        self.buf
            .len()
            .checked_add(extra)
            .and_then(|n| n.checked_add(extra_nul))
            .is_none_or(|n| n > self.limit)
    }

    pub fn reserve_extra(&mut self, extra: usize) -> SqlResult<()> {
        if self.exceeds(extra) {
            return Err(SqlError::too_big());
        }
        let target = self.buf.len() + extra;
        if target > self.charged {
            let delta = target - self.charged;
            if let Some(m) = self.meter {
                m.reserve_memory(delta as u64)?;
                // Update `self.charged` immediately so `Drop` releases `delta` even if
                // `charge_steps` below returns a resource-limit error.
                self.charged = target;
                self.step_pending = self.step_pending.saturating_add(delta);
                if self.step_pending >= 256 {
                    let steps = (self.step_pending / 256) as u64;
                    self.step_pending %= 256;
                    m.charge_steps(steps)?;
                }
            } else {
                self.charged = target;
            }
        }
        self.buf.reserve(extra);
        Ok(())
    }

    /// Check that an upcoming temporary allocation of `extra` bytes fits within both
    /// `max_length` and the meter's memory/step limits before allocating it.
    pub fn check_scratch(&self, extra: usize) -> SqlResult<()> {
        if self.exceeds(extra) {
            return Err(SqlError::too_big());
        }
        if let Some(m) = self.meter {
            if extra > 0 {
                m.reserve_memory(extra as u64)?;
                m.release_memory(extra as u64);
                if extra >= 256 {
                    m.charge_steps((extra / 256) as u64)?;
                }
            }
        }
        Ok(())
    }

    #[inline]
    pub fn charge_steps(&self, steps: u64) -> SqlResult<()> {
        if let Some(m) = self.meter {
            if steps > 0 {
                m.charge_steps(steps)?;
            }
        }
        Ok(())
    }

    #[inline]
    pub fn push(&mut self, b: u8) -> SqlResult<()> {
        self.reserve_extra(1)?;
        self.buf.push(b);
        Ok(())
    }

    #[inline]
    pub fn push_bytes(&mut self, src: &[u8]) -> SqlResult<()> {
        if src.is_empty() {
            return Ok(());
        }
        self.reserve_extra(src.len())?;
        self.buf.extend_from_slice(src);
        Ok(())
    }

    #[inline]
    pub fn push_str(&mut self, s: &str) -> SqlResult<()> {
        self.push_bytes(s.as_bytes())
    }

    pub fn push_repeat(&mut self, byte: u8, count: usize) -> SqlResult<()> {
        if count == 0 {
            return Ok(());
        }
        self.reserve_extra(count)?;
        self.buf.resize(self.buf.len() + count, byte);
        Ok(())
    }

    pub fn push_utf8(&mut self, cp: u32) -> SqlResult<()> {
        let need = if cp < 0x80 {
            1
        } else if cp < 0x800 {
            2
        } else if cp < 0x1_0000 {
            3
        } else {
            4
        };
        self.reserve_extra(need)?;
        append_utf8_raw(&mut self.buf, cp);
        Ok(())
    }

    /// Consume into a `Box<[u8]>`, releasing temporary meter charge on drop.
    pub fn into_boxed_bytes(mut self) -> Box<[u8]> {
        std::mem::take(&mut self.buf).into_boxed_slice()
    }
}

impl Drop for ByteAccum<'_> {
    fn drop(&mut self) {
        if self.charged > 0 {
            if let Some(m) = self.meter {
                m.release_memory(self.charged as u64);
            }
            self.charged = 0;
        }
    }
}

/// ASCII case-insensitive equality.
#[inline]
pub fn ascii_eq_ignore_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Fold ASCII uppercase to lowercase (matching SQLite `sqlite3UpperToLower`).
#[inline]
pub fn ascii_lower_byte(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 0x20
    } else {
        b
    }
}
