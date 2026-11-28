//! Pure safe Rust SQLite 3.54.1 JSON1 functions and table-valued functions (`json_each`,
//! `json_tree`, plus `generate_series`).
//!
//! Zero external dependencies; enforces nesting depth limits (SQLite `JSON_MAX_DEPTH = 1000`)
//! and length limits.

use xdb_core::value::{atof, int_value, number_to_text, SqlValue};

use crate::errors::{SqlError, SqlResult};
use crate::functions::FunctionContext;
use crate::utf8::text_bytes_of;

const JSON_MAX_DEPTH: usize = 1000;

/// Parsed JSON value preserving object key order and SQLite numeric distinction.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonVal {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Array(Vec<JsonVal>),
    Object(Vec<(String, JsonVal)>),
}

impl JsonVal {
    pub fn sqlite_type_name(&self) -> &'static str {
        match self {
            JsonVal::Null => "null",
            JsonVal::Bool(true) => "true",
            JsonVal::Bool(false) => "false",
            JsonVal::Int(_) => "integer",
            JsonVal::Real(_) => "real",
            JsonVal::Str(_) => "text",
            JsonVal::Array(_) => "array",
            JsonVal::Object(_) => "object",
        }
    }

    /// Convert to SQL value as `json_extract` / `->>` / `json_each.value` returns it.
    pub fn to_sql_extracted(&self) -> SqlValue {
        match self {
            JsonVal::Null => SqlValue::Null,
            JsonVal::Bool(b) => SqlValue::Int(i64::from(*b)),
            JsonVal::Int(i) => SqlValue::Int(*i),
            JsonVal::Real(r) => SqlValue::real(*r),
            JsonVal::Str(s) => SqlValue::text(s.as_str()),
            JsonVal::Array(_) | JsonVal::Object(_) => {
                let mut out = String::new();
                render_json_compact(self, &mut out);
                SqlValue::text(out)
            }
        }
    }

    /// Convert to SQL JSON text (`->` operator).
    pub fn to_sql_json(&self) -> SqlValue {
        let mut out = String::new();
        render_json_compact(self, &mut out);
        SqlValue::text(out)
    }

    /// Atom value for `json_each` / `json_tree` (`NULL` for arrays/objects/null).
    pub fn to_sql_atom(&self) -> SqlValue {
        match self {
            JsonVal::Array(_) | JsonVal::Object(_) => SqlValue::Null,
            other => other.to_sql_extracted(),
        }
    }
}

/// Convert a SQL value to a [`JsonVal`]. If `is_json` is true and `v` is TEXT, parse it as JSON.
pub fn json_value_from_sql(v: &SqlValue, is_json: bool) -> SqlResult<JsonVal> {
    match v {
        SqlValue::Null => Ok(JsonVal::Null),
        SqlValue::Int(i) => Ok(JsonVal::Int(*i)),
        SqlValue::Real(r) => {
            if r.is_nan() {
                Ok(JsonVal::Null)
            } else {
                Ok(JsonVal::Real(*r))
            }
        }
        SqlValue::Text(b) => {
            let s = String::from_utf8_lossy(b);
            if is_json {
                parse_json(&s, true)
            } else {
                Ok(JsonVal::Str(s.into_owned()))
            }
        }
        SqlValue::Blob(_) => Err(SqlError::runtime("JSON cannot hold BLOB values")),
    }
}

// ───────────────────────────── parser (RFC 8259 + JSON5) ─────────────────────────────

struct JsonParser<'a, 'm> {
    src: &'a [u8],
    pos: usize,
    allow_json5: bool,
    depth: usize,
    meter: Option<&'m dyn xdb_core::Meter>,
    mem: crate::utf8::MemoryGuard<'m>,
    limit_err: Option<xdb_core::ResourceLimitError>,
}

impl<'a, 'm> JsonParser<'a, 'm> {
    fn new(src: &'a str, allow_json5: bool) -> Self {
        Self::with_meter(src, allow_json5, None)
    }

    fn with_meter(
        src: &'a str,
        allow_json5: bool,
        meter: Option<&'m dyn xdb_core::Meter>,
    ) -> Self {
        Self::from_bytes(src.as_bytes(), allow_json5, meter)
    }

    fn from_bytes(
        src: &'a [u8],
        allow_json5: bool,
        meter: Option<&'m dyn xdb_core::Meter>,
    ) -> Self {
        Self {
            src,
            pos: 0,
            allow_json5,
            depth: 0,
            meter,
            mem: crate::utf8::MemoryGuard::new(meter),
            limit_err: None,
        }
    }

    #[inline]
    fn charge_node(&mut self, bytes: u64) -> Result<(), usize> {
        if let Some(m) = self.meter {
            if let Err(e) = m.charge_steps(1) {
                self.limit_err = Some(e);
                return Err(self.pos + 1);
            }
            if bytes > 0 {
                if let Err(SqlError::ResourceLimit(e)) = self.mem.reserve(bytes) {
                    self.limit_err = Some(e);
                    return Err(self.pos + 1);
                }
            }
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) -> Result<(), usize> {
        loop {
            while let Some(b) = self.peek() {
                match b {
                    b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                    0x0b | 0x0c if self.allow_json5 => self.pos += 1,
                    _ => break,
                }
            }
            if self.allow_json5 && self.pos + 1 < self.src.len() && self.src[self.pos] == b'/' {
                if self.src[self.pos + 1] == b'/' {
                    self.pos += 2;
                    while self.pos < self.src.len()
                        && self.src[self.pos] != b'\n'
                        && self.src[self.pos] != b'\r'
                    {
                        self.pos += 1;
                    }
                    continue;
                } else if self.src[self.pos + 1] == b'*' {
                    let start = self.pos;
                    self.pos += 2;
                    let mut closed = false;
                    while self.pos + 1 < self.src.len() {
                        if self.src[self.pos] == b'*' && self.src[self.pos + 1] == b'/' {
                            self.pos += 2;
                            closed = true;
                            break;
                        }
                        self.pos += 1;
                    }
                    if !closed {
                        return Err(start + 1);
                    }
                    continue;
                }
            }
            break;
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<JsonVal, usize> {
        self.skip_ws()?;
        let Some(b) = self.peek() else {
            return Err(self.pos + 1);
        };
        self.charge_node(std::mem::size_of::<JsonVal>() as u64)?;
        match b {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => {
                let s = self.parse_string(b'"')?;
                Ok(JsonVal::Str(s))
            }
            b'\'' if self.allow_json5 => {
                let s = self.parse_string(b'\'')?;
                Ok(JsonVal::Str(s))
            }
            b't' => self.expect_kw(b"true").map(|()| JsonVal::Bool(true)),
            b'f' => self.expect_kw(b"false").map(|()| JsonVal::Bool(false)),
            b'n' => {
                if self.src[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Ok(JsonVal::Null)
                } else if self.allow_json5 && self.match_kw_ci(b"nan") {
                    Ok(JsonVal::Null)
                } else {
                    Err(self.pos + 1)
                }
            }
            b'N' if self.allow_json5 && self.match_kw_ci(b"nan") => Ok(JsonVal::Null),
            b'q' | b'Q' if self.allow_json5 && self.match_kw_ci(b"qnan") => Ok(JsonVal::Null),
            b's' | b'S' if self.allow_json5 && self.match_kw_ci(b"snan") => Ok(JsonVal::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            b'+' | b'.' if self.allow_json5 => self.parse_number(),
            b'i' | b'I' if self.allow_json5 => {
                if self.match_kw_ci(b"infinity") || self.match_kw_ci(b"inf") {
                    Ok(JsonVal::Real(f64::INFINITY))
                } else {
                    Err(self.pos + 1)
                }
            }
            _ => Err(self.pos + 1),
        }
    }

    fn expect_kw(&mut self, kw: &[u8]) -> Result<(), usize> {
        if self.src[self.pos..].starts_with(kw) {
            self.pos += kw.len();
            Ok(())
        } else {
            Err(self.pos + 1)
        }
    }

    fn match_kw_ci(&mut self, kw: &[u8]) -> bool {
        let rest = &self.src[self.pos..];
        if rest.len() >= kw.len() && rest[..kw.len()].eq_ignore_ascii_case(kw) {
            self.pos += kw.len();
            true
        } else {
            false
        }
    }

    fn parse_object(&mut self) -> Result<JsonVal, usize> {
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return Err(self.pos + 1);
        }
        self.pos += 1; // '{'
        self.skip_ws()?;
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonVal::Object(Vec::new()));
        }
        let mut entries = Vec::with_capacity(4);
        loop {
            self.skip_ws()?;
            let key = match self.peek() {
                Some(b'"') => self.parse_string(b'"')?,
                Some(b'\'') if self.allow_json5 => self.parse_string(b'\'')?,
                Some(c) if self.allow_json5 && is_json5_ident_start(c) => {
                    let k = self.parse_ident()?;
                    self.charge_node(k.len() as u64)?;
                    k
                }
                _ => return Err(self.pos + 1),
            };
            self.skip_ws()?;
            if self.peek() != Some(b':') {
                return Err(self.pos + 1);
            }
            self.pos += 1;
            let val = self.parse_value()?;
            self.charge_node(32)?;
            entries.push((key, val));
            self.skip_ws()?;
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.allow_json5 && self.peek() == Some(b'}') {
                        self.pos += 1;
                        break;
                    }
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.pos + 1),
            }
        }
        self.depth -= 1;
        Ok(JsonVal::Object(entries))
    }

    fn parse_array(&mut self) -> Result<JsonVal, usize> {
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return Err(self.pos + 1);
        }
        self.pos += 1; // '['
        self.skip_ws()?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonVal::Array(items));
        }
        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_ws()?;
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.allow_json5 && self.peek() == Some(b']') {
                        self.pos += 1;
                        break;
                    }
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.pos + 1),
            }
        }
        self.depth -= 1;
        Ok(JsonVal::Array(items))
    }

    fn parse_ident(&mut self) -> Result<String, usize> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.pos + 1);
        }
        Ok(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
    }

    fn parse_string(&mut self, quote: u8) -> Result<String, usize> {
        self.pos += 1; // opening quote
        let start = self.pos;
        let bytes = self.src;
        let len = bytes.len();
        // Fast unescaped slice scan: avoids byte-by-byte `String::push` & repeated reallocs!
        while self.pos < len {
            let b = bytes[self.pos];
            if b == quote {
                let slice = &bytes[start..self.pos];
                self.pos += 1;
                if self.meter.is_some() && !slice.is_empty() {
                    self.charge_node(slice.len() as u64)?;
                }
                return Ok(String::from_utf8_lossy(slice).into_owned());
            }
            if b < 0x20 {
                return Err(self.pos + 1);
            }
            if b == b'\\' {
                break;
            }
            self.pos += 1;
        }
        let prefix_len = self.pos - start;
        if prefix_len > 0 {
            self.charge_node(prefix_len as u64)?;
        }
        let mut out = String::with_capacity(prefix_len + 16);
        let mut reserved_len = prefix_len;
        if prefix_len > 0 {
            out.push_str(&String::from_utf8_lossy(&bytes[start..self.pos]));
        }
        while self.pos < self.src.len() {
            if out.len().saturating_sub(reserved_len) >= 4096 {
                let delta = out.len() - reserved_len;
                self.charge_node(delta as u64)?;
                reserved_len = out.len();
            }
            let b = self.src[self.pos];
            if b == quote {
                self.pos += 1;
                if out.len() > reserved_len {
                    self.charge_node((out.len() - reserved_len) as u64)?;
                }
                return Ok(out);
            }
            if b < 0x20 {
                return Err(self.pos + 1);
            }
            if b == b'\\' {
                self.pos += 1;
                let Some(esc) = self.peek() else {
                    return Err(self.pos + 1);
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'\'' if self.allow_json5 => out.push('\''),
                    b'v' if self.allow_json5 => out.push('\u{000b}'),
                    b'0' if self.allow_json5 => {
                        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            return Err(self.pos + 1);
                        }
                        out.push('\0');
                    }
                    b'\n' if self.allow_json5 => {}
                    b'\r' if self.allow_json5 => {
                        if self.peek() == Some(b'\n') {
                            self.pos += 1;
                        }
                    }
                    b'x' if self.allow_json5 => {
                        let code = self.parse_hex(2)?;
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                    b'u' => {
                        let code = self.parse_hex(4)?;
                        if (0xd800..=0xdbff).contains(&code) {
                            if self.pos + 6 <= self.src.len()
                                && self.src[self.pos] == b'\\'
                                && self.src[self.pos + 1] == b'u'
                            {
                                let save = self.pos;
                                self.pos += 2;
                                if let Ok(low) = self.parse_hex(4) {
                                    if (0xdc00..=0xdfff).contains(&low) {
                                        let combined =
                                            0x1_0000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                                        out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                                        continue;
                                    }
                                }
                                self.pos = save;
                            }
                            out.push('\u{FFFD}');
                        } else if (0xdc00..=0xdfff).contains(&code) {
                            out.push('\u{FFFD}');
                        } else {
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ if self.allow_json5 => {
                        // JSON5 forbids decimal digit escapes (`\1`..`\9`); `\0` is handled above.
                        if esc.is_ascii_digit() {
                            return Err(self.pos);
                        }
                        if esc < 0x80 {
                            out.push(esc as char);
                        } else {
                            let start = self.pos - 1;
                            let max_end = (start + 4).min(self.src.len());
                            if let Some(ch) = decode_one_utf8(&self.src[start..max_end]) {
                                out.push(ch);
                                self.pos = start + ch.len_utf8();
                            } else {
                                out.push('\u{FFFD}');
                            }
                        }
                    }
                    _ => return Err(self.pos),
                }
            } else if b < 0x80 {
                out.push(b as char);
                self.pos += 1;
            } else {
                let max_end = (self.pos + 4).min(self.src.len());
                if let Some(ch) = decode_one_utf8(&self.src[self.pos..max_end]) {
                    out.push(ch);
                    self.pos += ch.len_utf8();
                } else {
                    out.push('\u{FFFD}');
                    self.pos += 1;
                }
            }
        }
        Err(self.pos + 1)
    }

    fn parse_hex(&mut self, n: usize) -> Result<u32, usize> {
        if self.pos + n > self.src.len() {
            return Err(self.pos + 1);
        }
        let mut val = 0u32;
        for _ in 0..n {
            let b = self.src[self.pos];
            let d = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a' + 10),
                b'A'..=b'F' => u32::from(b - b'A' + 10),
                _ => return Err(self.pos + 1),
            };
            val = (val << 4) | d;
            self.pos += 1;
        }
        Ok(val)
    }

    fn parse_number(&mut self) -> Result<JsonVal, usize> {
        let start = self.pos;
        let mut neg = false;
        if self.peek() == Some(b'-') {
            neg = true;
            self.pos += 1;
        } else if self.allow_json5 && self.peek() == Some(b'+') {
            self.pos += 1;
        }
        if self.allow_json5 && (self.match_kw_ci(b"infinity") || self.match_kw_ci(b"inf")) {
            return Ok(JsonVal::Real(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        // JSON5 hex integer: 0x...
        if self.allow_json5
            && self.pos + 2 <= self.src.len()
            && self.src[self.pos] == b'0'
            && matches!(self.src[self.pos + 1], b'x' | b'X')
        {
            self.pos += 2;
            let hstart = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            if self.pos == hstart {
                return Err(self.pos + 1);
            }
            let hex_str = std::str::from_utf8(&self.src[hstart..self.pos]).unwrap_or("");
            if let Ok(u) = u64::from_str_radix(hex_str, 16) {
                if !neg && u <= i64::MAX as u64 {
                    return Ok(JsonVal::Int(u as i64));
                }
                if neg && u <= (i64::MAX as u64) + 1 {
                    return Ok(JsonVal::Int(u.wrapping_neg() as i64));
                }
            }
            let f = u128::from_str_radix(hex_str, 16).map(|x| x as f64).unwrap_or(f64::INFINITY);
            return Ok(JsonVal::Real(if neg { -f } else { f }));
        }

        let mut is_float = false;
        let int_start = self.pos;
        if self.peek() == Some(b'0') {
            self.pos += 1;
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.pos + 1);
            }
        } else if self.peek().is_some_and(|c| (b'1'..=b'9').contains(&c)) {
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        } else if self.allow_json5 && self.peek() == Some(b'.') {
            // Leading dot allowed in JSON5
        } else {
            return Err(self.pos + 1);
        }

        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            let fstart = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            if !self.allow_json5 && self.pos == fstart {
                return Err(self.pos + 1);
            }
            if self.pos == fstart && self.pos == int_start + 1 {
                return Err(self.pos + 1);
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let estart = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == estart {
                return Err(self.pos + 1);
            }
        }

        let slice = &self.src[start..self.pos];
        if !is_float {
            let digits = &self.src[int_start..self.pos];
            if !digits.is_empty() && digits.len() <= 18 {
                let mut acc = 0u64;
                for &d in digits {
                    acc = acc * 10 + u64::from(d - b'0');
                }
                let iv = if neg { -(acc as i64) } else { acc as i64 };
                return Ok(JsonVal::Int(iv));
            }
            let s = std::str::from_utf8(slice).unwrap_or("");
            if let Ok(iv) = s.parse::<i64>() {
                return Ok(JsonVal::Int(iv));
            }
        }
        let r = atof(slice);
        Ok(JsonVal::Real(r.value))
    }
}

fn is_json5_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

/// Parse JSON text, returning the 1-based byte error offset on failure.
pub fn parse_json_with_pos(src: &str, allow_json5: bool) -> Result<JsonVal, usize> {
    let mut p = JsonParser::new(src, allow_json5);
    let val = p.parse_value()?;
    p.skip_ws()?;
    if p.pos < p.src.len() {
        return Err(p.pos + 1);
    }
    Ok(val)
}

pub fn parse_json(src: &str, allow_json5: bool) -> SqlResult<JsonVal> {
    parse_json_with_pos(src, allow_json5).map_err(|_| SqlError::runtime("malformed JSON"))
}

// ───────────────────────────── rendering ─────────────────────────────

fn push_json_escaped_str_metered(
    s: &str,
    out: &mut String,
    max_len: usize,
    ctx: Option<&FunctionContext<'_>>,
    mem: &mut crate::utf8::MemoryGuard<'_>,
) -> SqlResult<()> {
    let bytes = s.as_bytes();
    if let Some(c) = ctx {
        if bytes.len() >= 256 {
            c.charge_steps((bytes.len() / 256) as u64)?;
        }
    }
    let mut esc_len: usize = 2; // surrounding quotes
    for &b in bytes {
        let add = match b {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        };
        esc_len = esc_len.saturating_add(add);
    }
    if out.len().saturating_add(esc_len) > max_len {
        return Err(SqlError::too_big());
    }
    mem.reserve(esc_len as u64)?;
    out.reserve(esc_len);
    push_json_escaped_str(s, out);
    Ok(())
}

fn push_json_escaped_str(s: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let code = c as u8;
                out.push_str("\\u00");
                out.push(HEX[(code >> 4) as usize] as char);
                out.push(HEX[(code & 0x0f) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn decode_one_utf8(slice: &[u8]) -> Option<char> {
    if slice.is_empty() {
        return None;
    }
    let b0 = slice[0];
    let len = if b0 < 0x80 {
        1
    } else if (b0 & 0xe0) == 0xc0 {
        2
    } else if (b0 & 0xf0) == 0xe0 {
        3
    } else if (b0 & 0xf8) == 0xf0 {
        4
    } else {
        return None;
    };
    if slice.len() < len {
        return None;
    }
    std::str::from_utf8(&slice[..len]).ok()?.chars().next()
}

pub fn render_json_compact(val: &JsonVal, out: &mut String) {
    let mut mem = crate::utf8::MemoryGuard::new(None);
    let _ = render_json_compact_limited(val, out, usize::MAX, 0, None, &mut mem);
}

/// Parse JSON text while charging per-node steps and cumulative AST memory against `meter`.
pub fn parse_json_metered_with(
    src: &str,
    allow_json5: bool,
    meter: Option<&dyn xdb_core::Meter>,
) -> SqlResult<JsonVal> {
    if let Some(m) = meter {
        if src.len() >= 64 {
            m.charge_steps((src.len() / 64) as u64)?;
        }
    }
    let mut p = JsonParser::with_meter(src, allow_json5, meter);
    let val = match p.parse_value() {
        Ok(v) => v,
        Err(_) => {
            if let Some(e) = p.limit_err {
                return Err(SqlError::ResourceLimit(e));
            }
            return Err(SqlError::runtime("malformed JSON"));
        }
    };
    if p.skip_ws().is_err() || p.pos < p.src.len() {
        if let Some(e) = p.limit_err {
            return Err(SqlError::ResourceLimit(e));
        }
        return Err(SqlError::runtime("malformed JSON"));
    }
    Ok(val)
}

/// Render a [`JsonVal`] to a compact JSON `String`, enforcing `meter` steps & memory.
pub fn render_json_compact_metered(
    val: &JsonVal,
    meter: Option<&dyn xdb_core::Meter>,
) -> SqlResult<String> {
    let mut out = String::new();
    let mut mem = crate::utf8::MemoryGuard::new(meter);
    let ctx = FunctionContext::new(meter, 0);
    render_json_compact_limited(val, &mut out, usize::MAX, 0, Some(&ctx), &mut mem)?;
    Ok(out)
}

/// Render an iterator of `&JsonVal` elements as a JSON array without cloning the elements.
pub fn render_json_array_iter_metered<'a, I>(
    items: I,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue>
where
    I: IntoIterator<Item = &'a JsonVal>,
{
    let mut out = String::new();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    let max_len = ctx.limits.max_length;
    mem.reserve(2)?;
    out.push('[');
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            mem.reserve(1)?;
            out.push(',');
        }
        render_json_compact_limited(item, &mut out, max_len, 1, Some(ctx), &mut mem)?;
    }
    out.push(']');
    if out.len() > max_len {
        return Err(SqlError::too_big());
    }
    Ok(SqlValue::text(out))
}

/// Render an iterator of `(&str, &JsonVal)` pairs as a JSON object without cloning.
pub fn render_json_object_iter_metered<'a, I>(
    entries: I,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue>
where
    I: IntoIterator<Item = (&'a str, &'a JsonVal)>,
{
    let mut out = String::new();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    let max_len = ctx.limits.max_length;
    mem.reserve(2)?;
    out.push('{');
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i > 0 {
            mem.reserve(1)?;
            out.push(',');
        }
        if out.len().saturating_add(k.len()).saturating_add(3) > max_len {
            return Err(SqlError::too_big());
        }
        push_json_escaped_str_metered(k, &mut out, max_len.saturating_sub(1), Some(ctx), &mut mem)?;
        out.push(':');
        mem.reserve(1)?;
        render_json_compact_limited(v, &mut out, max_len, 1, Some(ctx), &mut mem)?;
    }
    out.push('}');
    if out.len() > max_len {
        return Err(SqlError::too_big());
    }
    Ok(SqlValue::text(out))
}

/// Render a [`JsonVal`] to a SQL `TEXT` value while enforcing `ctx.limits.max_length`
/// and charging cumulative memory + steps against `ctx.meter`.
pub fn render_json_value_metered(val: &JsonVal, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    let mut out = String::new();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    render_json_compact_limited(val, &mut out, ctx.limits.max_length, 0, Some(ctx), &mut mem)?;
    Ok(SqlValue::text(out))
}

fn render_json_extracted_metered(val: &JsonVal, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
    match val {
        JsonVal::Null => Ok(SqlValue::Null),
        JsonVal::Bool(b) => Ok(SqlValue::Int(i64::from(*b))),
        JsonVal::Int(i) => Ok(SqlValue::Int(*i)),
        JsonVal::Real(r) => Ok(SqlValue::real(*r)),
        JsonVal::Str(s) => {
            if s.len() > ctx.limits.max_length {
                return Err(SqlError::too_big());
            }
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            mem.reserve(s.len() as u64)?;
            Ok(SqlValue::text(s.as_str()))
        }
        JsonVal::Array(_) | JsonVal::Object(_) => render_json_value_metered(val, ctx),
    }
}

fn json_value_from_sql_guarded(
    v: &SqlValue,
    is_json: bool,
    ctx: &FunctionContext<'_>,
    mem: &mut crate::utf8::MemoryGuard<'_>,
) -> SqlResult<JsonVal> {
    match v {
        SqlValue::Null => Ok(JsonVal::Null),
        SqlValue::Int(i) => Ok(JsonVal::Int(*i)),
        SqlValue::Real(r) => {
            if r.is_nan() {
                Ok(JsonVal::Null)
            } else {
                Ok(JsonVal::Real(*r))
            }
        }
        SqlValue::Text(b) => {
            if is_json {
                let s = String::from_utf8_lossy(b);
                let parsed = parse_json_metered(&s, true, ctx)?;
                mem.reserve((b.len() as u64).saturating_add(32))?;
                Ok(parsed)
            } else {
                mem.reserve((b.len() as u64).saturating_add(32))?;
                let s = String::from_utf8_lossy(b);
                Ok(JsonVal::Str(s.into_owned()))
            }
        }
        SqlValue::Blob(_) => Err(SqlError::runtime("JSON cannot hold BLOB values")),
    }
}

pub(crate) fn json_value_from_sql_metered(
    v: &SqlValue,
    is_json: bool,
    ctx: &FunctionContext<'_>,
) -> SqlResult<JsonVal> {
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    json_value_from_sql_guarded(v, is_json, ctx, &mut mem)
}

fn parse_json_bytes_with_pos_metered(
    src: &[u8],
    json5: bool,
    ctx: &FunctionContext<'_>,
    trailing_returns_one: bool,
) -> SqlResult<Result<JsonVal, usize>> {
    if src.len() >= 64 {
        ctx.charge_steps((src.len() / 64) as u64)?;
    }
    let mut p = JsonParser::from_bytes(src, json5, ctx.meter);
    let res = match p.parse_value() {
        Ok(val) => match p.skip_ws() {
            Ok(()) if p.pos >= p.src.len() => Ok(val),
            Ok(()) => Err(if trailing_returns_one { 1 } else { p.pos + 1 }),
            Err(pos) => Err(pos),
        },
        Err(pos) => Err(pos),
    };
    if let Some(e) = p.limit_err {
        return Err(SqlError::ResourceLimit(e));
    }
    Ok(res)
}

fn parse_json_bytes_metered(
    src: &[u8],
    json5: bool,
    ctx: &FunctionContext<'_>,
) -> SqlResult<JsonVal> {
    parse_json_bytes_with_pos_metered(src, json5, ctx, false)?
        .map_err(|_| SqlError::runtime("malformed JSON"))
}

/// Canonicalize JSON / JSON5 raw bytes into compact RFC-8259 JSON while preserving:
/// - `-0` integer sign (`json('-0') -> '-0'`), and
/// - raw non-UTF-8 bytes inside string literals (`json(X'22FF22') -> X'22FF22'`), matching SQLite.
fn canonicalize_json_bytes_metered(
    src: &[u8],
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    if src.len() >= 64 {
        ctx.charge_steps((src.len() / 64) as u64)?;
    }
    let mut out = crate::utf8::ByteAccum::new(ctx.limits.max_length, false, ctx.meter);
    struct Canon<'a, 'm> {
        p: JsonParser<'a, 'm>,
        out: &'a mut crate::utf8::ByteAccum<'m>,
    }
    impl<'a, 'm> Canon<'a, 'm> {
        fn push_escaped_byte(&mut self, b: u8) -> SqlResult<()> {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            match b {
                b'"' => self.out.push_bytes(b"\\\""),
                b'\\' => self.out.push_bytes(b"\\\\"),
                0x08 => self.out.push_bytes(b"\\b"),
                0x0c => self.out.push_bytes(b"\\f"),
                b'\n' => self.out.push_bytes(b"\\n"),
                b'\r' => self.out.push_bytes(b"\\r"),
                b'\t' => self.out.push_bytes(b"\\t"),
                0x00..=0x1f => self.out.push_bytes(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(b >> 4) as usize],
                    HEX[(b & 0x0f) as usize],
                ]),
                _ => self.out.push(b),
            }
        }

        fn push_escaped_raw(&mut self, bytes: &[u8]) -> SqlResult<()> {
            self.out.push(b'"')?;
            for &b in bytes {
                self.push_escaped_byte(b)?;
            }
            self.out.push(b'"')
        }

        fn emit_escaped_string(&mut self, quote: u8) -> SqlResult<()> {
            let map_err = |p: &JsonParser<'_, '_>| -> SqlError {
                if let Some(e) = p.limit_err {
                    SqlError::ResourceLimit(e)
                } else {
                    SqlError::runtime("malformed JSON")
                }
            };
            self.p.pos += 1;
            self.out.push(b'"')?;
            let mut ticks = 0u64;
            while self.p.pos < self.p.src.len() {
                ticks += 1;
                if (ticks & 255) == 0 && self.p.charge_node(0).is_err() {
                    return Err(map_err(&self.p));
                }
                let b = self.p.src[self.p.pos];
                if b == quote {
                    self.p.pos += 1;
                    return self.out.push(b'"');
                }
                if b < 0x20 {
                    return Err(SqlError::runtime("malformed JSON"));
                }
                if b == b'\\' {
                    self.p.pos += 1;
                    let esc = *self
                        .p
                        .src
                        .get(self.p.pos)
                        .ok_or_else(|| SqlError::runtime("malformed JSON"))?;
                    self.p.pos += 1;
                    match esc {
                        b'"' => self.push_escaped_byte(b'"')?,
                        b'\\' => self.push_escaped_byte(b'\\')?,
                        b'/' => self.push_escaped_byte(b'/')?,
                        b'b' => self.push_escaped_byte(0x08)?,
                        b'f' => self.push_escaped_byte(0x0c)?,
                        b'n' => self.push_escaped_byte(b'\n')?,
                        b'r' => self.push_escaped_byte(b'\r')?,
                        b't' => self.push_escaped_byte(b'\t')?,
                        b'\'' if self.p.allow_json5 => self.push_escaped_byte(b'\'')?,
                        b'v' if self.p.allow_json5 => self.push_escaped_byte(0x0b)?,
                        b'0' if self.p.allow_json5 => {
                            if self.p.peek().is_some_and(|c| c.is_ascii_digit()) {
                                return Err(SqlError::runtime("malformed JSON"));
                            }
                            self.push_escaped_byte(0)?;
                        }
                        b'\n' if self.p.allow_json5 => {}
                        b'\r' if self.p.allow_json5 => {
                            if self.p.peek() == Some(b'\n') {
                                self.p.pos += 1;
                            }
                        }
                        b'x' if self.p.allow_json5 => {
                            let h = self.p.parse_hex(2).map_err(|_| map_err(&self.p))?;
                            let mut ubuf = [0u8; 4];
                            let s = char::from_u32(h).unwrap_or('\u{FFFD}').encode_utf8(&mut ubuf);
                            for &ub in s.as_bytes() {
                                self.push_escaped_byte(ub)?;
                            }
                        }
                        b'u' => {
                            let code = self.p.parse_hex(4).map_err(|_| map_err(&self.p))?;
                            let mut ubuf = [0u8; 4];
                            let ch = if (0xd800..=0xdbff).contains(&code) {
                                let save = self.p.pos;
                                if self.p.pos + 6 <= self.p.src.len()
                                    && self.p.src[self.p.pos] == b'\\'
                                    && self.p.src[self.p.pos + 1] == b'u'
                                {
                                    self.p.pos += 2;
                                    if let Ok(low) = self.p.parse_hex(4) {
                                        if (0xdc00..=0xdfff).contains(&low) {
                                            let cp =
                                                0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                                            char::from_u32(cp).unwrap_or('\u{FFFD}')
                                        } else {
                                            self.p.pos = save;
                                            '\u{FFFD}'
                                        }
                                    } else {
                                        self.p.pos = save;
                                        '\u{FFFD}'
                                    }
                                } else {
                                    '\u{FFFD}'
                                }
                            } else if (0xdc00..=0xdfff).contains(&code) {
                                '\u{FFFD}'
                            } else {
                                char::from_u32(code).unwrap_or('\u{FFFD}')
                            };
                            for &ub in ch.encode_utf8(&mut ubuf).as_bytes() {
                                self.push_escaped_byte(ub)?;
                            }
                        }
                        _ if self.p.allow_json5 => {
                            if esc.is_ascii_digit() {
                                return Err(SqlError::runtime("malformed JSON"));
                            }
                            self.push_escaped_byte(esc)?;
                        }
                        _ => return Err(SqlError::runtime("malformed JSON")),
                    }
                } else {
                    self.push_escaped_byte(b)?;
                    self.p.pos += 1;
                }
            }
            Err(SqlError::runtime("malformed JSON"))
        }

        fn emit_value(&mut self) -> SqlResult<()> {
            let map_err = |p: &JsonParser<'_, '_>| -> SqlError {
                if let Some(e) = p.limit_err {
                    SqlError::ResourceLimit(e)
                } else {
                    SqlError::runtime("malformed JSON")
                }
            };
            self.p.skip_ws().map_err(|_| map_err(&self.p))?;
            self.p.charge_node(0).map_err(|_| map_err(&self.p))?;
            if self.p.depth >= JSON_MAX_DEPTH {
                return Err(SqlError::runtime("malformed JSON"));
            }
            let b = self.p.peek().ok_or_else(|| SqlError::runtime("malformed JSON"))?;
            match b {
                b'{' => {
                    self.p.pos += 1;
                    self.p.depth += 1;
                    self.out.push(b'{')?;
                    self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                    let mut first = true;
                    if self.p.peek() == Some(b'}') {
                        self.p.pos += 1;
                        self.p.depth -= 1;
                        self.out.push(b'}')?;
                        return Ok(());
                    }
                    loop {
                        self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                        let kb = self.p.peek().ok_or_else(|| SqlError::runtime("malformed JSON"))?;
                        if !first {
                            self.out.push(b',')?;
                        }
                        first = false;
                        if kb == b'"' || (self.p.allow_json5 && kb == b'\'') {
                            self.emit_escaped_string(kb)?;
                        } else if self.p.allow_json5 && is_json5_ident_start(kb) {
                            let ident = self.p.parse_ident().map_err(|_| map_err(&self.p))?;
                            self.push_escaped_raw(ident.as_bytes())?;
                        } else {
                            return Err(SqlError::runtime("malformed JSON"));
                        }
                        self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                        if self.p.peek() != Some(b':') {
                            return Err(SqlError::runtime("malformed JSON"));
                        }
                        self.p.pos += 1;
                        self.out.push(b':')?;
                        self.emit_value()?;
                        self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                        match self.p.peek() {
                            Some(b',') => {
                                self.p.pos += 1;
                                self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                                if self.p.allow_json5 && self.p.peek() == Some(b'}') {
                                    self.p.pos += 1;
                                    self.p.depth -= 1;
                                    self.out.push(b'}')?;
                                    return Ok(());
                                }
                            }
                            Some(b'}') => {
                                self.p.pos += 1;
                                self.p.depth -= 1;
                                self.out.push(b'}')?;
                                return Ok(());
                            }
                            _ => return Err(SqlError::runtime("malformed JSON")),
                        }
                    }
                }
                b'[' => {
                    self.p.pos += 1;
                    self.p.depth += 1;
                    self.out.push(b'[')?;
                    self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                    let mut first = true;
                    if self.p.peek() == Some(b']') {
                        self.p.pos += 1;
                        self.p.depth -= 1;
                        self.out.push(b']')?;
                        return Ok(());
                    }
                    loop {
                        if !first {
                            self.out.push(b',')?;
                        }
                        first = false;
                        self.emit_value()?;
                        self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                        match self.p.peek() {
                            Some(b',') => {
                                self.p.pos += 1;
                                self.p.skip_ws().map_err(|_| map_err(&self.p))?;
                                if self.p.allow_json5 && self.p.peek() == Some(b']') {
                                    self.p.pos += 1;
                                    self.p.depth -= 1;
                                    self.out.push(b']')?;
                                    return Ok(());
                                }
                            }
                            Some(b']') => {
                                self.p.pos += 1;
                                self.p.depth -= 1;
                                self.out.push(b']')?;
                                return Ok(());
                            }
                            _ => return Err(SqlError::runtime("malformed JSON")),
                        }
                    }
                }
                b'"' => self.emit_escaped_string(b'"'),
                b'\'' if self.p.allow_json5 => self.emit_escaped_string(b'\''),
                _ => {
                    let start = self.p.pos;
                    let val = self.p.parse_value().map_err(|_| map_err(&self.p))?;
                    let slice = &self.p.src[start..self.p.pos];
                    if matches!(val, JsonVal::Int(0)) && slice.first() == Some(&b'-') {
                        self.out.push_bytes(b"-0")
                    } else {
                        let mut tmp = String::new();
                        render_json_compact(&val, &mut tmp);
                        self.out.push_str(&tmp)
                    }
                }
            }
        }
    }
    let mut c = Canon {
        p: JsonParser::from_bytes(src, true, ctx.meter),
        out: &mut out,
    };
    c.emit_value()?;
    let skip_ok = c.p.skip_ws().is_ok() && c.p.pos >= c.p.src.len();
    if let Some(e) = c.p.limit_err {
        return Err(SqlError::ResourceLimit(e));
    }
    if !skip_ok {
        return Err(SqlError::runtime("malformed JSON"));
    }
    Ok(SqlValue::Text(out.into_boxed_bytes()))
}

fn parse_json_metered(src: &str, json5: bool, ctx: &FunctionContext<'_>) -> SqlResult<JsonVal> {
    parse_json_metered_with(src, json5, ctx.meter)
}

fn render_json_compact_limited(
    val: &JsonVal,
    out: &mut String,
    max_len: usize,
    depth: usize,
    ctx: Option<&FunctionContext<'_>>,
    mem: &mut crate::utf8::MemoryGuard<'_>,
) -> SqlResult<()> {
    if depth > JSON_MAX_DEPTH {
        return Err(SqlError::runtime("JSON nesting depth exceeded"));
    }
    if let Some(c) = ctx {
        c.charge_steps(1)?;
    }
    let before = out.len();
    match val {
        JsonVal::Null => out.push_str("null"),
        JsonVal::Bool(true) => out.push_str("true"),
        JsonVal::Bool(false) => out.push_str("false"),
        JsonVal::Int(i) => out.push_str(&i.to_string()),
        JsonVal::Real(r) => {
            if !r.is_finite() {
                if r.is_nan() {
                    out.push_str("null");
                } else if r.is_sign_negative() {
                    out.push_str("-9.0e+999");
                } else {
                    out.push_str("9.0e+999");
                }
            } else if let Some(s) = number_to_text(&SqlValue::Real(*r)) {
                out.push_str(&s);
            }
        }
        JsonVal::Str(s) => {
            push_json_escaped_str_metered(s, out, max_len, ctx, mem)?;
            return Ok(());
        }
        JsonVal::Array(items) => {
            mem.reserve(2)?;
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    mem.reserve(1)?;
                    out.push(',');
                }
                render_json_compact_limited(item, out, max_len, depth + 1, ctx, mem)?;
            }
            out.push(']');
        }
        JsonVal::Object(entries) => {
            mem.reserve(2)?;
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    mem.reserve(1)?;
                    out.push(',');
                }
                if out.len().saturating_add(k.len()).saturating_add(3) > max_len {
                    return Err(SqlError::too_big());
                }
                push_json_escaped_str_metered(k, out, max_len.saturating_sub(1), ctx, mem)?;
                out.push(':');
                mem.reserve(1)?;
                render_json_compact_limited(v, out, max_len, depth + 1, ctx, mem)?;
            }
            out.push('}');
        }
    }
    if out.len() > max_len {
        return Err(SqlError::too_big());
    }
    if out.len() > before && !matches!(val, JsonVal::Array(_) | JsonVal::Object(_)) {
        mem.reserve((out.len() - before) as u64)?;
    }
    Ok(())
}

fn render_json_pretty(
    val: &JsonVal,
    indent: &str,
    level: usize,
    out: &mut String,
    ctx: &FunctionContext<'_>,
    mem: &mut crate::utf8::MemoryGuard<'_>,
) -> SqlResult<()> {
    if level > JSON_MAX_DEPTH {
        return Err(SqlError::runtime("JSON nesting depth exceeded"));
    }
    ctx.charge_steps(1)?;
    let max_len = ctx.limits.max_length;
    match val {
        JsonVal::Array(items) if !items.is_empty() => {
            mem.reserve(2)?;
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                let indent_bytes = indent.len().saturating_mul(level + 1);
                if out.len().saturating_add(indent_bytes) > max_len {
                    return Err(SqlError::too_big());
                }
                mem.reserve(indent_bytes as u64)?;
                if indent_bytes >= 256 {
                    ctx.charge_steps((indent_bytes / 256) as u64)?;
                }
                for _ in 0..=level {
                    out.push_str(indent);
                }
                render_json_pretty(item, indent, level + 1, out, ctx, mem)?;
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
                mem.reserve(2)?;
                if out.len() > max_len {
                    return Err(SqlError::too_big());
                }
            }
            let indent_bytes = indent.len().saturating_mul(level);
            if out.len().saturating_add(indent_bytes).saturating_add(1) > max_len {
                return Err(SqlError::too_big());
            }
            mem.reserve((indent_bytes + 1) as u64)?;
            for _ in 0..level {
                out.push_str(indent);
            }
            out.push(']');
        }
        JsonVal::Object(entries) if !entries.is_empty() => {
            mem.reserve(2)?;
            out.push_str("{\n");
            for (i, (k, v)) in entries.iter().enumerate() {
                let indent_bytes = indent.len().saturating_mul(level + 1);
                if out.len().saturating_add(indent_bytes).saturating_add(2) > max_len {
                    return Err(SqlError::too_big());
                }
                mem.reserve((indent_bytes + 2) as u64)?;
                if indent_bytes >= 256 {
                    ctx.charge_steps((indent_bytes / 256) as u64)?;
                }
                for _ in 0..=level {
                    out.push_str(indent);
                }
                push_json_escaped_str_metered(k, out, max_len.saturating_sub(2), Some(ctx), mem)?;
                out.push_str(": ");
                render_json_pretty(v, indent, level + 1, out, ctx, mem)?;
                if i + 1 < entries.len() {
                    out.push(',');
                }
                out.push('\n');
                mem.reserve(2)?;
                if out.len() > max_len {
                    return Err(SqlError::too_big());
                }
            }
            let indent_bytes = indent.len().saturating_mul(level);
            if out.len().saturating_add(indent_bytes).saturating_add(1) > max_len {
                return Err(SqlError::too_big());
            }
            mem.reserve((indent_bytes + 1) as u64)?;
            for _ in 0..level {
                out.push_str(indent);
            }
            out.push('}');
        }
        other => render_json_compact_limited(other, out, max_len, level, Some(ctx), mem)?,
    }
    if out.len() > max_len {
        return Err(SqlError::too_big());
    }
    Ok(())
}

// ───────────────────────────── JSON paths ─────────────────────────────

#[derive(Debug, Clone)]
enum PathStep {
    Key(String),
    Index(usize),
    /// `[#-N]` relative to end of array (`N >= 1`), or `[#]` (`None`) for append.
    FromEnd(Option<usize>),
}

fn parse_json_path_suffix(
    path: &str,
    mut i: usize,
    start_depth: usize,
    ctx: &FunctionContext<'_>,
) -> SqlResult<Vec<PathStep>> {
    let bytes = path.as_bytes();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    let mut steps = Vec::new();
    while i < bytes.len() {
        ctx.charge_steps(1)?;
        if start_depth.saturating_add(steps.len()) >= JSON_MAX_DEPTH {
            return Err(SqlError::runtime("JSON nesting depth exceeded"));
        }
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                }
                if bytes[i] == b'"' {
                    let mut p = JsonParser::with_meter(&path[i..], false, ctx.meter);
                    let key = match p.parse_string(b'"') {
                        Ok(k) => k,
                        Err(_) => {
                            if let Some(lim) = p.limit_err.take() {
                                return Err(SqlError::ResourceLimit(lim));
                            }
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                    };
                    i += p.pos;
                    mem.reserve((key.len() as u64).saturating_add(32))?;
                    steps.push(PathStep::Key(key));
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    mem.reserve(((i - start) as u64).saturating_add(32))?;
                    steps.push(PathStep::Key(path[start..i].to_string()));
                }
            }
            b'[' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b']' {
                        i += 1;
                        mem.reserve(32)?;
                        steps.push(PathStep::FromEnd(None));
                    } else if i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == start || i >= bytes.len() || bytes[i] != b']' {
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                        let n: usize = path[start..i].parse().unwrap_or(usize::MAX);
                        i += 1;
                        mem.reserve(32)?;
                        steps.push(PathStep::FromEnd(Some(n)));
                    } else {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start || i >= bytes.len() || bytes[i] != b']' {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    let idx: usize = path[start..i].parse().unwrap_or(usize::MAX);
                    i += 1;
                    mem.reserve(32)?;
                    steps.push(PathStep::Index(idx));
                }
            }
            _ => return Err(SqlError::runtime(format!("bad JSON path: '{path}'"))),
        }
    }
    Ok(steps)
}

/// Normalize SQLite `->` / `->>` RHS label into a JSON path.
fn arrow_rhs_to_path(rhs: &SqlValue) -> Option<String> {
    match rhs {
        SqlValue::Null => None,
        SqlValue::Int(i) => {
            if *i < 0 {
                Some(format!("$[#-{}]", i.unsigned_abs()))
            } else {
                Some(format!("$[{i}]"))
            }
        }
        SqlValue::Real(_) => {
            let s = number_to_text(rhs).unwrap_or_default();
            Some(format!("$.\"{s}\""))
        }
        SqlValue::Text(b) | SqlValue::Blob(b) => {
            let s = String::from_utf8_lossy(b);
            if s.starts_with('$') {
                Some(s.into_owned())
            } else if s.len() >= 3 && s.starts_with('[') && s.ends_with(']') {
                Some(format!("${s}"))
            } else {
                let mut quoted = String::from("$.");
                push_json_escaped_str(&s, &mut quoted);
                Some(quoted)
            }
        }
    }
}

/// Walk a SQLite JSON path string directly against `root` without allocating `Vec<PathStep>`,
/// matching SQLite `json.c:jsonLookupStep` semantics (including returning `Ok(None)` when
/// stepping `[...]` into a non-array or `.key` into a non-object, while still rejecting
/// invalid path syntax such as `$a`, `$.`, or `[foo]` on an array).
fn lookup_json_path_str<'a>(
    root: &'a JsonVal,
    path: &str,
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<&'a JsonVal>> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
    }
    if bytes.len() > JSON_MAX_DEPTH {
        ctx.charge_steps((bytes.len() / 64) as u64)?;
        let mut steps_est = 0usize;
        let mut in_quote = false;
        let mut esc = false;
        for &b in &bytes[1..] {
            if in_quote {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_quote = false;
                }
            } else if b == b'"' {
                in_quote = true;
            } else if b == b'.' || b == b'[' {
                steps_est += 1;
                if steps_est > JSON_MAX_DEPTH {
                    return Err(SqlError::runtime("JSON path too deep"));
                }
            }
        }
    }
    let mut cur = root;
    let mut i = 1usize;
    let mut depth = 0usize;
    while i < bytes.len() {
        depth += 1;
        if depth > JSON_MAX_DEPTH {
            return Err(SqlError::runtime("JSON path too deep"));
        }
        if (depth & 31) == 0 {
            ctx.charge_steps(1)?;
        }
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                }
                let key_cow: std::borrow::Cow<'_, str> = if bytes[i] == b'"' {
                    let mut p = JsonParser::with_meter(&path[i..], false, ctx.meter);
                    let key = match p.parse_string(b'"') {
                        Ok(k) => k,
                        Err(_) => {
                            if let Some(lim) = p.limit_err.take() {
                                return Err(SqlError::ResourceLimit(lim));
                            }
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                    };
                    i += p.pos;
                    std::borrow::Cow::Owned(key)
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    std::borrow::Cow::Borrowed(&path[start..i])
                };
                match cur {
                    JsonVal::Object(entries) => {
                        let Some((_, next)) = entries.iter().find(|(ek, _)| ek == key_cow.as_ref())
                        else {
                            return Ok(None);
                        };
                        cur = next;
                    }
                    _ => return Ok(None),
                }
            }
            b'[' => {
                let JsonVal::Array(items) = cur else {
                    // SQLite `jsonLookupStep`: `[` on a non-array returns NULL immediately.
                    return Ok(None);
                };
                i += 1;
                if i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b']' {
                        return Ok(None);
                    } else if i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == start || i >= bytes.len() || bytes[i] != b']' {
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                        let n: usize = path[start..i].parse().unwrap_or(usize::MAX);
                        i += 1;
                        if n == 0 || n > items.len() {
                            return Ok(None);
                        }
                        cur = &items[items.len() - n];
                    } else {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start || i >= bytes.len() || bytes[i] != b']' {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    let idx: usize = path[start..i].parse().unwrap_or(usize::MAX);
                    i += 1;
                    let Some(next) = items.get(idx) else {
                        return Ok(None);
                    };
                    cur = next;
                }
            }
            _ => return Err(SqlError::runtime(format!("bad JSON path: '{path}'"))),
        }
    }
    Ok(Some(cur))
}

fn lookup_json_path_with_meta<'a>(
    root: &'a JsonVal,
    path: &str,
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<(String, String, SqlValue, &'a JsonVal)>> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
    }
    let mut cur = root;
    let mut cur_path = String::from("$");
    let mut p_path = String::from("$");
    let mut r_key = SqlValue::Null;
    let mut i = 1usize;
    let mut depth = 0usize;
    while i < bytes.len() {
        depth += 1;
        if depth > JSON_MAX_DEPTH {
            return Err(SqlError::runtime("JSON path too deep"));
        }
        if (depth & 31) == 0 {
            ctx.charge_steps(1)?;
        }
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                }
                let key_cow: std::borrow::Cow<'_, str> = if bytes[i] == b'"' {
                    let mut p = JsonParser::with_meter(&path[i..], false, ctx.meter);
                    let key = match p.parse_string(b'"') {
                        Ok(k) => k,
                        Err(_) => {
                            if let Some(lim) = p.limit_err.take() {
                                return Err(SqlError::ResourceLimit(lim));
                            }
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                    };
                    i += p.pos;
                    std::borrow::Cow::Owned(key)
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    std::borrow::Cow::Borrowed(&path[start..i])
                };
                match cur {
                    JsonVal::Object(entries) => {
                        let Some((_, next)) = entries.iter().find(|(ek, _)| ek == key_cow.as_ref())
                        else {
                            return Ok(None);
                        };
                        if i == bytes.len() {
                            p_path = cur_path.clone();
                            r_key = SqlValue::text(key_cow.as_ref());
                        }
                        cur_path = format_json_path_key(&cur_path, key_cow.as_ref());
                        cur = next;
                    }
                    _ => return Ok(None),
                }
            }
            b'[' => {
                let JsonVal::Array(items) = cur else {
                    return Ok(None);
                };
                i += 1;
                let idx = if i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b']' {
                        return Ok(None);
                    } else if i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == start || i >= bytes.len() || bytes[i] != b']' {
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                        let n: usize = path[start..i].parse().unwrap_or(usize::MAX);
                        i += 1;
                        if n == 0 || n > items.len() {
                            return Ok(None);
                        }
                        items.len() - n
                    } else {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start || i >= bytes.len() || bytes[i] != b']' {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    let idx: usize = path[start..i].parse().unwrap_or(usize::MAX);
                    i += 1;
                    if idx >= items.len() {
                        return Ok(None);
                    }
                    idx
                };
                if i == bytes.len() {
                    p_path = cur_path.clone();
                    r_key = SqlValue::Int(idx as i64);
                }
                cur_path = format!("{cur_path}[{idx}]");
                cur = &items[idx];
            }
            _ => return Err(SqlError::runtime(format!("bad JSON path: '{path}'"))),
        }
    }
    Ok(Some((cur_path, p_path, r_key, cur)))
}

fn parse_one_array_step(path: &str, i: &mut usize) -> SqlResult<PathStep> {
    let bytes = path.as_bytes();
    *i += 1; // skip '['
    if *i < bytes.len() && bytes[*i] == b'#' {
        *i += 1;
        if *i < bytes.len() && bytes[*i] == b']' {
            *i += 1;
            Ok(PathStep::FromEnd(None))
        } else if *i < bytes.len() && bytes[*i] == b'-' {
            *i += 1;
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            if *i == start || *i >= bytes.len() || bytes[*i] != b']' {
                return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
            }
            let n: usize = path[start..*i].parse().unwrap_or(usize::MAX);
            *i += 1;
            Ok(PathStep::FromEnd(Some(n)))
        } else {
            Err(SqlError::runtime(format!("bad JSON path: '{path}'")))
        }
    } else {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start || *i >= bytes.len() || bytes[*i] != b']' {
            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
        }
        let idx: usize = path[start..*i].parse().unwrap_or(usize::MAX);
        *i += 1;
        Ok(PathStep::Index(idx))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MutMode {
    Insert,
    Replace,
    Set,
}

fn step_can_create_into_empty(step: &PathStep) -> bool {
    matches!(
        step,
        PathStep::Key(_) | PathStep::Index(0) | PathStep::FromEnd(None) | PathStep::FromEnd(Some(0))
    )
}

fn build_created_container(steps: &[PathStep], new_val: JsonVal) -> Option<JsonVal> {
    if steps.iter().any(|s| !step_can_create_into_empty(s)) {
        return None;
    }
    let mut val = new_val;
    for step in steps.iter().rev() {
        val = match step {
            PathStep::Key(k) => JsonVal::Object(vec![(k.clone(), val)]),
            PathStep::Index(0) | PathStep::FromEnd(None) | PathStep::FromEnd(Some(0)) => {
                JsonVal::Array(vec![val])
            }
            _ => return None,
        };
    }
    Some(val)
}

fn mutate_path_str(
    root: &mut JsonVal,
    path: &str,
    new_val: JsonVal,
    mode: MutMode,
    ctx: &FunctionContext<'_>,
) -> SqlResult<()> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
    }
    if bytes.len() == 1 {
        if mode != MutMode::Insert {
            *root = new_val;
        }
        return Ok(());
    }
    let mut cur = root;
    let mut i = 1usize;
    let mut depth = 0usize;
    while i < bytes.len() {
        depth += 1;
        if depth > JSON_MAX_DEPTH {
            return Err(SqlError::runtime("JSON nesting depth exceeded"));
        }
        ctx.charge_steps(1)?;
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                }
                let key = if bytes[i] == b'"' {
                    let mut p = JsonParser::with_meter(&path[i..], false, ctx.meter);
                    let k = match p.parse_string(b'"') {
                        Ok(k) => k,
                        Err(_) => {
                            if let Some(lim) = p.limit_err.take() {
                                return Err(SqlError::ResourceLimit(lim));
                            }
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                    };
                    i += p.pos;
                    k
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    path[start..i].to_string()
                };
                let JsonVal::Object(entries) = cur else {
                    return Ok(());
                };
                if i == bytes.len() {
                    if let Some(pos) = entries.iter().position(|(ek, _)| ek == &key) {
                        if mode != MutMode::Insert {
                            entries[pos].1 = new_val;
                        }
                    } else if mode != MutMode::Replace {
                        entries.push((key, new_val));
                    }
                    return Ok(());
                }
                if let Some(pos) = entries.iter().position(|(ek, _)| ek == &key) {
                    cur = &mut entries[pos].1;
                } else if mode != MutMode::Replace {
                    let rem = parse_json_path_suffix(path, i, depth, ctx)?;
                    if let Some(created) = build_created_container(&rem, new_val) {
                        entries.push((key, created));
                    }
                    return Ok(());
                } else {
                    return Ok(());
                }
            }
            b'[' => {
                let JsonVal::Array(items) = cur else {
                    return Ok(());
                };
                let step = parse_one_array_step(path, &mut i)?;
                if i == bytes.len() {
                    match step {
                        PathStep::Index(idx) => {
                            if idx < items.len() && mode != MutMode::Insert {
                                items[idx] = new_val;
                            } else if idx == items.len() && mode != MutMode::Replace {
                                items.push(new_val);
                            }
                        }
                        PathStep::FromEnd(None) | PathStep::FromEnd(Some(0)) => {
                            if mode != MutMode::Replace {
                                items.push(new_val);
                            }
                        }
                        PathStep::FromEnd(Some(n)) => {
                            if n <= items.len() && mode != MutMode::Insert {
                                let idx = items.len() - n;
                                items[idx] = new_val;
                            }
                        }
                        PathStep::Key(_) => {}
                    }
                    return Ok(());
                }
                match step {
                    PathStep::Index(idx) => {
                        if idx < items.len() {
                            cur = &mut items[idx];
                        } else if idx == items.len() && mode != MutMode::Replace {
                            let rem = parse_json_path_suffix(path, i, depth, ctx)?;
                            if let Some(created) = build_created_container(&rem, new_val) {
                                items.push(created);
                            }
                            return Ok(());
                        } else {
                            return Ok(());
                        }
                    }
                    PathStep::FromEnd(None) | PathStep::FromEnd(Some(0)) => {
                        if mode != MutMode::Replace {
                            let rem = parse_json_path_suffix(path, i, depth, ctx)?;
                            if let Some(created) = build_created_container(&rem, new_val) {
                                items.push(created);
                            }
                        }
                        return Ok(());
                    }
                    PathStep::FromEnd(Some(n)) => {
                        if n > items.len() {
                            return Ok(());
                        }
                        let len = items.len();
                        cur = &mut items[len - n];
                    }
                    PathStep::Key(_) => return Ok(()),
                }
            }
            _ => return Err(SqlError::runtime(format!("bad JSON path: '{path}'"))),
        }
    }
    Ok(())
}

/// Lazily removes the node at `path`, returning `Ok(true)` if the root `$` itself was removed.
fn remove_path_str(
    root: &mut JsonVal,
    path: &str,
    ctx: &FunctionContext<'_>,
) -> SqlResult<bool> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
    }
    if bytes.len() == 1 {
        return Ok(true);
    }
    let mut cur = root;
    let mut i = 1usize;
    let mut depth = 0usize;
    while i < bytes.len() {
        depth += 1;
        if depth > JSON_MAX_DEPTH {
            return Err(SqlError::runtime("JSON nesting depth exceeded"));
        }
        ctx.charge_steps(1)?;
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                }
                let key_cow: std::borrow::Cow<'_, str> = if bytes[i] == b'"' {
                    let mut p = JsonParser::with_meter(&path[i..], false, ctx.meter);
                    let k = match p.parse_string(b'"') {
                        Ok(k) => k,
                        Err(_) => {
                            if let Some(lim) = p.limit_err.take() {
                                return Err(SqlError::ResourceLimit(lim));
                            }
                            return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                        }
                    };
                    i += p.pos;
                    std::borrow::Cow::Owned(k)
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("bad JSON path: '{path}'")));
                    }
                    std::borrow::Cow::Borrowed(&path[start..i])
                };
                let JsonVal::Object(entries) = cur else {
                    return Ok(false);
                };
                if i == bytes.len() {
                    if let Some(pos) = entries.iter().position(|(ek, _)| ek == key_cow.as_ref()) {
                        entries.remove(pos);
                    }
                    return Ok(false);
                }
                let Some(pos) = entries.iter().position(|(ek, _)| ek == key_cow.as_ref()) else {
                    return Ok(false);
                };
                cur = &mut entries[pos].1;
            }
            b'[' => {
                let JsonVal::Array(items) = cur else {
                    return Ok(false);
                };
                let step = parse_one_array_step(path, &mut i)?;
                if i == bytes.len() {
                    match step {
                        PathStep::Index(idx) => {
                            if idx < items.len() {
                                items.remove(idx);
                            }
                        }
                        PathStep::FromEnd(Some(n)) if n > 0 && n <= items.len() => {
                            let idx = items.len() - n;
                            items.remove(idx);
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                match step {
                    PathStep::Index(idx) => {
                        let Some(elem) = items.get_mut(idx) else {
                            return Ok(false);
                        };
                        cur = elem;
                    }
                    PathStep::FromEnd(Some(n)) if n > 0 && n <= items.len() => {
                        let len = items.len();
                        cur = &mut items[len - n];
                    }
                    _ => return Ok(false),
                }
            }
            _ => return Err(SqlError::runtime(format!("bad JSON path: '{path}'"))),
        }
    }
    Ok(false)
}

fn merge_patch(
    target: JsonVal,
    patch: JsonVal,
    depth: usize,
    ctx: &FunctionContext<'_>,
) -> SqlResult<JsonVal> {
    if depth > JSON_MAX_DEPTH {
        return Err(SqlError::runtime("JSON nesting depth exceeded"));
    }
    ctx.charge_steps(1)?;
    let JsonVal::Object(patch_entries) = patch else {
        return Ok(patch);
    };
    let mut base = match target {
        JsonVal::Object(entries) => entries,
        _ => Vec::new(),
    };
    if base.len().saturating_mul(patch_entries.len()) > 256 {
        let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
        mem.reserve((base.len() as u64).saturating_mul(48))?;
        let mut slots: Vec<Option<(String, JsonVal)>> = base.into_iter().map(Some).collect();
        let mut key_positions: std::collections::HashMap<String, std::collections::VecDeque<usize>> =
            std::collections::HashMap::with_capacity(slots.len());
        for (idx, entry) in slots.iter().enumerate() {
            ctx.charge_steps(1)?;
            if let Some((bk, _)) = entry {
                key_positions.entry(bk.clone()).or_default().push_back(idx);
            }
        }
        for (k, v) in patch_entries {
            ctx.charge_steps(1)?;
            if matches!(v, JsonVal::Null) {
                if let Some(positions) = key_positions.get_mut(&k) {
                    if let Some(pos) = positions.pop_front() {
                        slots[pos] = None;
                        if positions.is_empty() {
                            key_positions.remove(&k);
                        }
                    }
                }
            } else if let Some(positions) = key_positions.get(&k) {
                if let Some(&pos) = positions.front() {
                    if let Some((_, ref mut slot_val)) = slots[pos] {
                        let old = std::mem::replace(slot_val, JsonVal::Null);
                        *slot_val = merge_patch(old, v, depth + 1, ctx)?;
                    }
                }
            } else {
                let cleaned = merge_patch(JsonVal::Object(Vec::new()), v, depth + 1, ctx)?;
                let idx = slots.len();
                mem.reserve((k.len() as u64).saturating_add(48))?;
                key_positions.entry(k.clone()).or_default().push_back(idx);
                slots.push(Some((k, cleaned)));
            }
        }
        base = slots.into_iter().flatten().collect();
    } else {
        for (k, v) in patch_entries {
            ctx.charge_steps(1)?;
            if matches!(v, JsonVal::Null) {
                if let Some(pos) = base.iter().position(|(bk, _)| bk == &k) {
                    base.remove(pos);
                }
            } else if let Some(pos) = base.iter().position(|(bk, _)| bk == &k) {
                let old = std::mem::replace(&mut base[pos].1, JsonVal::Null);
                base[pos].1 = merge_patch(old, v, depth + 1, ctx)?;
            } else {
                let cleaned = merge_patch(JsonVal::Object(Vec::new()), v, depth + 1, ctx)?;
                base.push((k, cleaned));
            }
        }
    }
    Ok(JsonVal::Object(base))
}

fn format_json_path_key(base: &str, key: &str) -> String {
    let needs_quote = key.is_empty()
        || key.bytes().any(|b| {
            b == b'.'
                || b == b'['
                || b == b']'
                || b == b'"'
                || b == b'\\'
                || b.is_ascii_whitespace()
                || b < 0x20
        });
    if !needs_quote {
        format!("{base}.{key}")
    } else {
        let mut out = String::with_capacity(base.len() + key.len() + 4);
        out.push_str(base);
        out.push('.');
        push_json_escaped_str(key, &mut out);
        out
    }
}

/// Evaluate SQLite JSON arrow operators (`->` when `extract_text == false`, `->>` when true).
pub fn eval_json_arrow(lhs: &SqlValue, rhs: &SqlValue, extract_text: bool) -> SqlResult<SqlValue> {
    let ctx = FunctionContext::new(None, 0);
    eval_json_arrow_metered(lhs, rhs, extract_text, &ctx)
}

/// Metered variant of [`eval_json_arrow`] respecting `ctx.limits` and `ctx.meter`.
pub fn eval_json_arrow_metered(
    lhs: &SqlValue,
    rhs: &SqlValue,
    extract_text: bool,
    ctx: &FunctionContext<'_>,
) -> SqlResult<SqlValue> {
    if lhs.is_null() || rhs.is_null() {
        return Ok(SqlValue::Null);
    }
    let Some(lhs_bytes) = text_bytes_of(lhs) else {
        return Ok(SqlValue::Null);
    };
    let doc = parse_json_bytes_metered(&lhs_bytes, true, ctx)?;
    let Some(path_str) = arrow_rhs_to_path(rhs) else {
        return Ok(SqlValue::Null);
    };
    let Some(found) = lookup_json_path_str(&doc, &path_str, ctx)? else {
        return Ok(SqlValue::Null);
    };
    if extract_text {
        render_json_extracted_metered(found, ctx)
    } else {
        render_json_value_metered(found, ctx)
    }
}

/// Dispatch any SQLite JSON1 scalar function by lowercase ASCII name.
/// Returns `Ok(None)` if not a JSON scalar function; `Ok(Some((val, is_json_subtype)))` otherwise.
pub fn call_json_scalar(
    lower_name: &str,
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<(SqlValue, bool)>> {
    let res: (SqlValue, bool) = match lower_name {
        "json" => {
            if args.len() != 1 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json()",
                ));
            }
            match text_bytes_of(&args[0]) {
                None => (SqlValue::Null, false),
                Some(b) => (canonicalize_json_bytes_metered(&b, ctx)?, true),
            }
        }
        "json_valid" => {
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_valid()",
                ));
            }
            let flags = if args.len() == 2 {
                if args[1].is_null() {
                    return Ok(Some((SqlValue::Null, false)));
                }
                let f = int_value(&args[1]);
                if !(1..=15).contains(&f) {
                    return Err(SqlError::runtime(
                        "FLAGS parameter to json_valid() must be between 1 and 15",
                    ));
                }
                f
            } else {
                1
            };
            let v = match &args[0] {
                SqlValue::Null => SqlValue::Null,
                other => {
                    let Some(b) = text_bytes_of(other) else {
                        return Ok(Some((SqlValue::Null, false)));
                    };
                    if b.len() >= 64 {
                        ctx.charge_steps((b.len() / 64) as u64)?;
                    }
                    let ok = if (flags & 2) != 0 {
                        parse_json_bytes_with_pos_metered(&b, true, ctx, false)?.is_ok()
                    } else if (flags & 1) != 0 {
                        parse_json_bytes_with_pos_metered(&b, false, ctx, false)?.is_ok()
                    } else {
                        false
                    };
                    SqlValue::Int(i64::from(ok))
                }
            };
            (v, false)
        }
        "json_error_position" => {
            if args.len() != 1 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_error_position()",
                ));
            }
            let v = match text_bytes_of(&args[0]) {
                None => SqlValue::Null,
                Some(b) => match parse_json_bytes_with_pos_metered(&b, true, ctx, true)? {
                    Ok(_) => SqlValue::Int(0),
                    Err(byte_pos_1based) => {
                        let byte_idx = byte_pos_1based.saturating_sub(1).min(b.len());
                        let char_pos = b[..byte_idx]
                            .iter()
                            .filter(|&&byte| (byte & 0xc0) != 0x80)
                            .count()
                            + 1;
                        SqlValue::Int(char_pos as i64)
                    }
                },
            };
            (v, false)
        }
        "json_type" => {
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_type()",
                ));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let doc = parse_json_bytes_metered(&b, true, ctx)?;
            let target = if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                lookup_json_path_str(&doc, &ps, ctx)?
            } else {
                Some(&doc)
            };
            let v = match target {
                Some(node) => SqlValue::text(node.sqlite_type_name()),
                None => SqlValue::Null,
            };
            (v, false)
        }
        "json_array_length" => {
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_array_length()",
                ));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let doc = parse_json_bytes_metered(&b, true, ctx)?;
            let target = if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                lookup_json_path_str(&doc, &ps, ctx)?
            } else {
                Some(&doc)
            };
            let v = match target {
                Some(JsonVal::Array(items)) => SqlValue::Int(items.len() as i64),
                Some(_) => SqlValue::Int(0),
                None => SqlValue::Null,
            };
            (v, false)
        }
        "json_extract" => {
            if args.len() < 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_extract()",
                ));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let doc = parse_json_bytes_metered(&b, true, ctx)?;
            if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                if let Some(node) = lookup_json_path_str(&doc, &ps, ctx)? {
                    let is_j = matches!(node, JsonVal::Array(_) | JsonVal::Object(_));
                    (render_json_extracted_metered(node, ctx)?, is_j)
                } else {
                    (SqlValue::Null, false)
                }
            } else {
                static NULL_JSON: JsonVal = JsonVal::Null;
                let mut refs: Vec<&JsonVal> = Vec::with_capacity(args.len() - 1);
                for arg in &args[1..] {
                    let Some(pb) = text_bytes_of(arg) else {
                        return Ok(Some((SqlValue::Null, false)));
                    };
                    let ps = String::from_utf8_lossy(&pb);
                    refs.push(lookup_json_path_str(&doc, &ps, ctx)?.unwrap_or(&NULL_JSON));
                }
                (render_json_array_iter_metered(refs.into_iter(), ctx)?, true)
            }
        }
        "json_array" => {
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            mem.reserve((args.len() as u64).saturating_mul(32))?;
            let mut items = Vec::with_capacity(args.len());
            for (idx, arg) in args.iter().enumerate() {
                items.push(json_value_from_sql_guarded(
                    arg,
                    ctx.arg_is_json(idx),
                    ctx,
                    &mut mem,
                )?);
            }
            (render_json_value_metered(&JsonVal::Array(items), ctx)?, true)
        }
        "json_object" => {
            if !args.len().is_multiple_of(2) {
                return Err(SqlError::runtime(
                    "json_object() requires an even number of arguments",
                ));
            }
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            mem.reserve(((args.len() / 2) as u64).saturating_mul(48))?;
            let mut entries = Vec::with_capacity(args.len() / 2);
            let mut i = 0;
            while i < args.len() {
                let SqlValue::Text(ref kb) = args[i] else {
                    return Err(SqlError::runtime("json_object() labels must be TEXT"));
                };
                mem.reserve((kb.len() as u64).saturating_add(24))?;
                let key = String::from_utf8_lossy(kb).into_owned();
                let val = json_value_from_sql_guarded(
                    &args[i + 1],
                    ctx.arg_is_json(i + 1),
                    ctx,
                    &mut mem,
                )?;
                entries.push((key, val));
                i += 2;
            }
            (
                render_json_value_metered(&JsonVal::Object(entries), ctx)?,
                true,
            )
        }
        "json_quote" => {
            if args.len() != 1 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_quote()",
                ));
            }
            let jv = json_value_from_sql_metered(&args[0], ctx.arg_is_json(0), ctx)?;
            (render_json_value_metered(&jv, ctx)?, true)
        }
        "json_insert" | "json_replace" | "json_set" => {
            if args.is_empty() || args.len().is_multiple_of(2) {
                return Err(SqlError::runtime(format!(
                    "wrong number of arguments to function {lower_name}()"
                )));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let mut doc = parse_json_bytes_metered(&b, true, ctx)?;
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            mem.reserve(b.len() as u64)?;
            let mode = match lower_name {
                "json_insert" => MutMode::Insert,
                "json_replace" => MutMode::Replace,
                _ => MutMode::Set,
            };
            let mut i = 1;
            while i + 1 < args.len() {
                if let Some(pb) = text_bytes_of(&args[i]) {
                    let ps = String::from_utf8_lossy(&pb);
                    let val = json_value_from_sql_guarded(
                        &args[i + 1],
                        ctx.arg_is_json(i + 1),
                        ctx,
                        &mut mem,
                    )?;
                    mutate_path_str(&mut doc, &ps, val, mode, ctx)?;
                }
                i += 2;
            }
            (render_json_value_metered(&doc, ctx)?, true)
        }
        "json_remove" => {
            if args.is_empty() {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_remove()",
                ));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let mut doc = parse_json_bytes_metered(&b, true, ctx)?;
            let mut removed_root = false;
            for arg in &args[1..] {
                let Some(pb) = text_bytes_of(arg) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                if removed_root {
                    if ps.as_bytes().first() != Some(&b'$') {
                        return Err(SqlError::runtime(format!("bad JSON path: '{ps}'")));
                    }
                } else if remove_path_str(&mut doc, &ps, ctx)? {
                    removed_root = true;
                }
            }
            if removed_root {
                (SqlValue::Null, false)
            } else {
                (render_json_value_metered(&doc, ctx)?, true)
            }
        }
        "json_patch" => {
            if args.len() != 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_patch()",
                ));
            }
            let (Some(b1), Some(b2)) = (text_bytes_of(&args[0]), text_bytes_of(&args[1])) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let d1 = parse_json_bytes_metered(&b1, true, ctx)?;
            let d2 = parse_json_bytes_metered(&b2, true, ctx)?;
            let merged = merge_patch(d1, d2, 0, ctx)?;
            (render_json_value_metered(&merged, ctx)?, true)
        }
        "json_pretty" => {
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function json_pretty()",
                ));
            }
            let Some(b) = text_bytes_of(&args[0]) else {
                return Ok(Some((SqlValue::Null, false)));
            };
            let doc = parse_json_bytes_metered(&b, true, ctx)?;
            let indent = if args.len() == 2 {
                text_bytes_of(&args[1])
                    .map(|ib| String::from_utf8_lossy(&ib).into_owned())
                    .unwrap_or_else(|| "    ".to_string())
            } else {
                "    ".to_string()
            };
            let mut out = String::new();
            let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
            render_json_pretty(&doc, &indent, 0, &mut out, ctx, &mut mem)?;
            (SqlValue::text(out), true)
        }
        _ => return Ok(None),
    };
    Ok(Some(res))
}

// ───────────────────────────── Table-Valued Functions ─────────────────────────────

/// Output of a built-in table-valued function (`json_each`, `json_tree`, `generate_series`).
#[derive(Debug, Clone)]
pub struct TableValuedResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

/// Evaluate a built-in table-valued function (`json_each`, `json_tree`, `generate_series`).
/// Returns `Ok(None)` if not a built-in TVF.
pub fn call_builtin_tvf(
    lower_name: &str,
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
) -> SqlResult<Option<TableValuedResult>> {
    call_builtin_tvf_with_limit(lower_name, args, ctx, None)
}

pub(crate) fn call_builtin_tvf_with_limit(
    lower_name: &str,
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
    row_cap: Option<usize>,
) -> SqlResult<Option<TableValuedResult>> {
    match lower_name {
        "json_each" => Ok(Some(eval_json_tvf(args, false, ctx)?)),
        "json_tree" => Ok(Some(eval_json_tvf(args, true, ctx)?)),
        "generate_series" => Ok(Some(eval_generate_series(args, ctx, row_cap)?)),
        _ => Ok(None),
    }
}

fn eval_json_tvf(
    args: &[SqlValue],
    recursive: bool,
    ctx: &FunctionContext<'_>,
) -> SqlResult<TableValuedResult> {
    let columns = vec![
        "key".to_string(),
        "value".to_string(),
        "type".to_string(),
        "atom".to_string(),
        "id".to_string(),
        "parent".to_string(),
        "fullkey".to_string(),
        "path".to_string(),
    ];
    if args.is_empty() {
        return Ok(TableValuedResult {
            columns,
            rows: Vec::new(),
        });
    }
    if args.len() > 2 {
        return Err(SqlError::runtime(format!(
            "wrong number of arguments to function {}()",
            if recursive { "json_tree" } else { "json_each" }
        )));
    }
    let Some(b) = text_bytes_of(&args[0]) else {
        return Ok(TableValuedResult {
            columns,
            rows: Vec::new(),
        });
    };
    let doc = parse_json_bytes_metered(&b, true, ctx)?;
    let (root_path, parent_path, root_key, target) = if args.len() == 2 {
        let Some(pb) = text_bytes_of(&args[1]) else {
            return Ok(TableValuedResult {
                columns,
                rows: Vec::new(),
            });
        };
        let ps = String::from_utf8_lossy(&pb);
        let Some(found) = lookup_json_path_with_meta(&doc, &ps, ctx)? else {
            return Ok(TableValuedResult {
                columns,
                rows: Vec::new(),
            });
        };
        found
    } else {
        ("$".to_string(), "$".to_string(), SqlValue::Null, &doc)
    };

    let mut rows = Vec::new();
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    let mut next_id: i64 = 0;

    if recursive {
        emit_tree(
            target,
            root_key,
            None,
            &root_path,
            &parent_path,
            0,
            &mut next_id,
            &mut rows,
            ctx,
            &mut mem,
        )?;
    } else {
        match target {
            JsonVal::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    ctx.charge_steps(1)?;
                    next_id += 1;
                    let fullkey = format!("{root_path}[{idx}]");
                    let row = vec![
                        SqlValue::Int(idx as i64),
                        render_json_extracted_metered(item, ctx)?,
                        SqlValue::text(item.sqlite_type_name()),
                        item.to_sql_atom(),
                        SqlValue::Int(next_id),
                        SqlValue::Null,
                        SqlValue::text(fullkey),
                        SqlValue::text(root_path.as_str()),
                    ];
                    mem.reserve(crate::utf8::estimate_row_bytes(&row))?;
                    rows.push(row);
                }
            }
            JsonVal::Object(entries) => {
                for (k, v) in entries {
                    ctx.charge_steps(1)?;
                    next_id += 1;
                    let fullkey = format_json_path_key(&root_path, k);
                    let row = vec![
                        SqlValue::text(k.as_str()),
                        render_json_extracted_metered(v, ctx)?,
                        SqlValue::text(v.sqlite_type_name()),
                        v.to_sql_atom(),
                        SqlValue::Int(next_id),
                        SqlValue::Null,
                        SqlValue::text(fullkey),
                        SqlValue::text(root_path.as_str()),
                    ];
                    mem.reserve(crate::utf8::estimate_row_bytes(&row))?;
                    rows.push(row);
                }
            }
            scalar => {
                ctx.charge_steps(1)?;
                let row = vec![
                    SqlValue::Null,
                    render_json_extracted_metered(scalar, ctx)?,
                    SqlValue::text(scalar.sqlite_type_name()),
                    scalar.to_sql_atom(),
                    SqlValue::Int(0),
                    SqlValue::Null,
                    SqlValue::text(root_path.as_str()),
                    SqlValue::text(root_path.as_str()),
                ];
                mem.reserve(crate::utf8::estimate_row_bytes(&row))?;
                rows.push(row);
            }
        }
    }

    Ok(TableValuedResult { columns, rows })
}

#[allow(clippy::too_many_arguments)]
fn emit_tree(
    node: &JsonVal,
    key: SqlValue,
    parent: Option<i64>,
    fullkey: &str,
    path: &str,
    depth: usize,
    next_id: &mut i64,
    rows: &mut Vec<Vec<SqlValue>>,
    ctx: &FunctionContext<'_>,
    mem: &mut crate::utf8::MemoryGuard<'_>,
) -> SqlResult<()> {
    if depth > JSON_MAX_DEPTH {
        return Err(SqlError::runtime("JSON nesting depth exceeded"));
    }
    ctx.charge_steps(1)?;
    let my_id = *next_id;
    *next_id += 1;
    let extracted = render_json_extracted_metered(node, ctx)?;
    let row = vec![
        key,
        extracted,
        SqlValue::text(node.sqlite_type_name()),
        node.to_sql_atom(),
        SqlValue::Int(my_id),
        parent.map_or(SqlValue::Null, SqlValue::Int),
        SqlValue::text(fullkey),
        SqlValue::text(path),
    ];
    mem.reserve(crate::utf8::estimate_row_bytes(&row))?;
    rows.push(row);
    match node {
        JsonVal::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                let fk = format!("{fullkey}[{idx}]");
                emit_tree(
                    item,
                    SqlValue::Int(idx as i64),
                    Some(my_id),
                    &fk,
                    fullkey,
                    depth + 1,
                    next_id,
                    rows,
                    ctx,
                    mem,
                )?;
            }
        }
        JsonVal::Object(entries) => {
            for (k, v) in entries {
                let fk = format_json_path_key(fullkey, k);
                emit_tree(
                    v,
                    SqlValue::text(k.as_str()),
                    Some(my_id),
                    &fk,
                    fullkey,
                    depth + 1,
                    next_id,
                    rows,
                    ctx,
                    mem,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn eval_generate_series(
    args: &[SqlValue],
    ctx: &FunctionContext<'_>,
    row_cap: Option<usize>,
) -> SqlResult<TableValuedResult> {
    if args.is_empty() || args.len() > 3 {
        return Err(SqlError::runtime(
            "wrong number of arguments to function generate_series()",
        ));
    }
    let columns = vec!["value".to_string()];
    if args.iter().any(SqlValue::is_null) || row_cap == Some(0) {
        return Ok(TableValuedResult {
            columns,
            rows: Vec::new(),
        });
    }
    let start = int_value(&args[0]);
    let stop = if args.len() >= 2 {
        int_value(&args[1])
    } else {
        i64::MAX
    };
    let step = if args.len() == 3 {
        let s = int_value(&args[2]);
        if s == 0 {
            1
        } else {
            s
        }
    } else {
        1
    };

    let mut rows = Vec::new();
    let mut cur = start;
    let mut reserved: u64 = 0;
    loop {
        if (step > 0 && cur > stop) || (step < 0 && cur < stop) {
            break;
        }
        if row_cap.is_some_and(|cap| rows.len() >= cap) {
            break;
        }
        ctx.charge_steps(1)?;
        if let Some(m) = ctx.meter {
            if let Err(e) = m.reserve_memory(48) {
                if reserved > 0 {
                    m.release_memory(reserved);
                }
                return Err(e.into());
            }
            reserved += 48;
        }
        rows.push(vec![SqlValue::Int(cur)]);
        match cur.checked_add(step) {
            Some(next) => cur = next,
            None => break,
        }
    }
    if reserved > 0 {
        if let Some(m) = ctx.meter {
            m.release_memory(reserved);
        }
    }
    Ok(TableValuedResult { columns, rows })
}
