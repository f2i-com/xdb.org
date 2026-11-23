//! Pure safe Rust SQLite 3.54.1 JSON1 functions and table-valued functions (`json_each`,
//! `json_tree`, plus `generate_series`).
//!
//! Zero external dependencies; enforces nesting depth limits (SQLite `JSON_MAX_DEPTH = 1000`)
//! and length limits.

use xdb_core::value::{atof, int_value, number_to_text, real_same_as_int, real_to_i64, SqlValue};

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
            if r.is_finite() {
                Ok(JsonVal::Real(*r))
            } else {
                Ok(JsonVal::Null)
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

struct JsonParser<'a> {
    src: &'a [u8],
    pos: usize,
    allow_json5: bool,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn new(src: &'a str, allow_json5: bool) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            allow_json5,
            depth: 0,
        }
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
        match b {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string(b'"').map(JsonVal::Str),
            b'\'' if self.allow_json5 => self.parse_string(b'\'').map(JsonVal::Str),
            b't' => self.expect_kw(b"true").map(|()| JsonVal::Bool(true)),
            b'f' => self.expect_kw(b"false").map(|()| JsonVal::Bool(false)),
            b'n' => self.expect_kw(b"null").map(|()| JsonVal::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            b'+' | b'.' if self.allow_json5 => self.parse_number(),
            b'I' if self.allow_json5 => {
                self.expect_kw(b"Infinity")
                    .map(|()| JsonVal::Real(f64::INFINITY))
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

    fn parse_object(&mut self) -> Result<JsonVal, usize> {
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return Err(self.pos + 1);
        }
        self.pos += 1; // '{'
        self.skip_ws()?;
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonVal::Object(entries));
        }
        loop {
            self.skip_ws()?;
            let key = match self.peek() {
                Some(b'"') => self.parse_string(b'"')?,
                Some(b'\'') if self.allow_json5 => self.parse_string(b'\'')?,
                Some(c) if self.allow_json5 && is_json5_ident_start(c) => self.parse_ident()?,
                _ => return Err(self.pos + 1),
            };
            self.skip_ws()?;
            if self.peek() != Some(b':') {
                return Err(self.pos + 1);
            }
            self.pos += 1;
            let val = self.parse_value()?;
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
        let mut out = String::new();
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b == quote {
                self.pos += 1;
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
                    b'0' if self.allow_json5 => out.push('\0'),
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
                        if esc < 0x80 {
                            out.push(esc as char);
                        } else {
                            let start = self.pos - 1;
                            let max_end = (start + 4).min(self.src.len());
                            let ch = decode_one_utf8(&self.src[start..max_end])
                                .ok_or(start + 1)?;
                            out.push(ch);
                            self.pos = start + ch.len_utf8();
                        }
                    }
                    _ => return Err(self.pos),
                }
            } else if b < 0x80 {
                out.push(b as char);
                self.pos += 1;
            } else {
                let max_end = (self.pos + 4).min(self.src.len());
                let ch = decode_one_utf8(&self.src[self.pos..max_end]).ok_or(self.pos + 1)?;
                out.push(ch);
                self.pos += ch.len_utf8();
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
        if self.allow_json5 && self.src[self.pos..].starts_with(b"Infinity") {
            self.pos += 8;
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

fn parse_json_metered(src: &str, json5: bool, ctx: &FunctionContext<'_>) -> SqlResult<JsonVal> {
    if src.len() >= 64 {
        ctx.charge_steps((src.len() / 64) as u64)?;
    }
    let mut mem = crate::utf8::MemoryGuard::new(ctx.meter);
    mem.reserve(src.len() as u64)?;
    parse_json(src, json5)
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
                    out.push_str("-9.0e999");
                } else {
                    out.push_str("9.0e999");
                }
            } else if let Some(s) = number_to_text(&SqlValue::Real(*r)) {
                out.push_str(&s);
            }
        }
        JsonVal::Str(s) => {
            if out.len().saturating_add(s.len()).saturating_add(2) > max_len {
                return Err(SqlError::too_big());
            }
            mem.reserve((s.len() + 2) as u64)?;
            let start = out.len();
            push_json_escaped_str(s, out);
            let actual = out.len() - start;
            if actual > s.len() + 2 {
                mem.reserve((actual - (s.len() + 2)) as u64)?;
            }
            if out.len() > max_len {
                return Err(SqlError::too_big());
            }
            return Ok(());
        }
        JsonVal::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_json_compact_limited(item, out, max_len, depth + 1, ctx, mem)?;
            }
            out.push(']');
        }
        JsonVal::Object(entries) => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if out.len().saturating_add(k.len()).saturating_add(3) > max_len {
                    return Err(SqlError::too_big());
                }
                let kstart = out.len();
                push_json_escaped_str(k, out);
                out.push(':');
                mem.reserve((out.len() - kstart) as u64)?;
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
                let est = indent_bytes.saturating_add(k.len()).saturating_add(4);
                if out.len().saturating_add(est) > max_len {
                    return Err(SqlError::too_big());
                }
                mem.reserve(est as u64)?;
                if est >= 256 {
                    ctx.charge_steps((est / 256) as u64)?;
                }
                for _ in 0..=level {
                    out.push_str(indent);
                }
                push_json_escaped_str(k, out);
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

fn parse_json_path(path: &str) -> SqlResult<Vec<PathStep>> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(SqlError::runtime(format!("JSON path error near '{path}'")));
    }
    let mut i = 1;
    let mut steps = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(SqlError::runtime(format!("JSON path error near '{path}'")));
                }
                if bytes[i] == b'"' {
                    let mut p = JsonParser::new(&path[i..], false);
                    let key = p
                        .parse_string(b'"')
                        .map_err(|_| SqlError::runtime(format!("JSON path error near '{path}'")))?;
                    i += p.pos;
                    steps.push(PathStep::Key(key));
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if i == start {
                        return Err(SqlError::runtime(format!("JSON path error near '{path}'")));
                    }
                    steps.push(PathStep::Key(path[start..i].to_string()));
                }
            }
            b'[' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b']' {
                        i += 1;
                        steps.push(PathStep::FromEnd(None));
                    } else if i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == start || i >= bytes.len() || bytes[i] != b']' {
                            return Err(SqlError::runtime(format!(
                                "JSON path error near '{path}'"
                            )));
                        }
                        // Huge array index saturates to `usize::MAX` -> cleanly returns NULL on lookup
                        // instead of erroring (matches SQLite on both 32-bit WASM and 64-bit).
                        let n: usize = path[start..i].parse().unwrap_or(usize::MAX);
                        i += 1;
                        steps.push(PathStep::FromEnd(Some(n)));
                    } else {
                        return Err(SqlError::runtime(format!("JSON path error near '{path}'")));
                    }
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start || i >= bytes.len() || bytes[i] != b']' {
                        return Err(SqlError::runtime(format!("JSON path error near '{path}'")));
                    }
                    let idx: usize = path[start..i].parse().unwrap_or(usize::MAX);
                    i += 1;
                    steps.push(PathStep::Index(idx));
                }
            }
            _ => return Err(SqlError::runtime(format!("JSON path error near '{path}'"))),
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
        SqlValue::Real(r) => {
            let iv = real_to_i64(*r);
            if real_same_as_int(*r, iv) {
                if iv < 0 {
                    Some(format!("$[#-{}]", iv.unsigned_abs()))
                } else {
                    Some(format!("$[{iv}]"))
                }
            } else {
                let s = number_to_text(rhs).unwrap_or_default();
                Some(format!("$.\"{s}\""))
            }
        }
        SqlValue::Text(b) | SqlValue::Blob(b) => {
            let s = String::from_utf8_lossy(b);
            if s.starts_with('$') {
                Some(s.into_owned())
            } else {
                let mut quoted = String::from("$.");
                push_json_escaped_str(&s, &mut quoted);
                Some(quoted)
            }
        }
    }
}

fn lookup_path<'a>(root: &'a JsonVal, steps: &[PathStep]) -> Option<&'a JsonVal> {
    let mut cur = root;
    for step in steps {
        match (cur, step) {
            (JsonVal::Object(entries), PathStep::Key(k)) => {
                cur = entries.iter().find(|(ek, _)| ek == k).map(|(_, v)| v)?;
            }
            (JsonVal::Array(items), PathStep::Index(idx)) => {
                cur = items.get(*idx)?;
            }
            (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
                if *n == 0 || *n > items.len() {
                    return None;
                }
                cur = &items[items.len() - *n];
            }
            _ => return None,
        }
    }
    Some(cur)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MutMode {
    Insert,
    Replace,
    Set,
}

fn mutate_path(root: &mut JsonVal, steps: &[PathStep], new_val: JsonVal, mode: MutMode) {
    if steps.is_empty() {
        if mode != MutMode::Insert {
            *root = new_val;
        }
        return;
    }
    let mut cur = root;
    for step in &steps[..steps.len() - 1] {
        match (cur, step) {
            (JsonVal::Object(entries), PathStep::Key(k)) => {
                let Some(pos) = entries.iter().position(|(ek, _)| ek == k) else {
                    return;
                };
                cur = &mut entries[pos].1;
            }
            (JsonVal::Array(items), PathStep::Index(idx)) => {
                let Some(elem) = items.get_mut(*idx) else {
                    return;
                };
                cur = elem;
            }
            (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
                if *n == 0 || *n > items.len() {
                    return;
                }
                let len = items.len();
                cur = &mut items[len - *n];
            }
            _ => return,
        }
    }
    match (cur, steps.last().unwrap()) {
        (JsonVal::Object(entries), PathStep::Key(k)) => {
            if let Some(pos) = entries.iter().position(|(ek, _)| ek == k) {
                if mode != MutMode::Insert {
                    entries[pos].1 = new_val;
                }
            } else if mode != MutMode::Replace {
                entries.push((k.clone(), new_val));
            }
        }
        (JsonVal::Array(items), PathStep::Index(idx)) => {
            if *idx < items.len() && mode != MutMode::Insert {
                items[*idx] = new_val;
            }
        }
        (JsonVal::Array(items), PathStep::FromEnd(None)) => {
            if mode != MutMode::Replace {
                items.push(new_val);
            }
        }
        (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
            if *n > 0 && *n <= items.len() && mode != MutMode::Insert {
                let idx = items.len() - *n;
                items[idx] = new_val;
            }
        }
        _ => {}
    }
}

fn remove_path(root: &mut JsonVal, steps: &[PathStep]) {
    if steps.is_empty() {
        return;
    }
    let mut cur = root;
    for step in &steps[..steps.len() - 1] {
        match (cur, step) {
            (JsonVal::Object(entries), PathStep::Key(k)) => {
                let Some(pos) = entries.iter().position(|(ek, _)| ek == k) else {
                    return;
                };
                cur = &mut entries[pos].1;
            }
            (JsonVal::Array(items), PathStep::Index(idx)) => {
                let Some(elem) = items.get_mut(*idx) else {
                    return;
                };
                cur = elem;
            }
            (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
                if *n == 0 || *n > items.len() {
                    return;
                }
                let len = items.len();
                cur = &mut items[len - *n];
            }
            _ => return,
        }
    }
    match (cur, steps.last().unwrap()) {
        (JsonVal::Object(entries), PathStep::Key(k)) => {
            entries.retain(|(ek, _)| ek != k);
        }
        (JsonVal::Array(items), PathStep::Index(idx)) => {
            if *idx < items.len() {
                items.remove(*idx);
            }
        }
        (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
            if *n > 0 && *n <= items.len() {
                let idx = items.len() - *n;
                items.remove(idx);
            }
        }
        _ => {}
    }
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
    for (k, v) in patch_entries {
        ctx.charge_steps(1)?;
        if matches!(v, JsonVal::Null) {
            base.retain(|(bk, _)| bk != &k);
        } else if let Some(pos) = base.iter().position(|(bk, _)| bk == &k) {
            let old = std::mem::replace(&mut base[pos].1, JsonVal::Null);
            base[pos].1 = merge_patch(old, v, depth + 1, ctx)?;
        } else {
            let cleaned = merge_patch(JsonVal::Object(Vec::new()), v, depth + 1, ctx)?;
            base.push((k, cleaned));
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
    let lhs_str = std::str::from_utf8(&lhs_bytes).map_err(|_| SqlError::runtime("malformed JSON"))?;
    let doc = parse_json_metered(lhs_str, true, ctx)?;
    let Some(path_str) = arrow_rhs_to_path(rhs) else {
        return Ok(SqlValue::Null);
    };
    let steps = parse_json_path(&path_str)?;
    let Some(found) = lookup_path(&doc, &steps) else {
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
                Some(b) => {
                    let s = std::str::from_utf8(&b)
                        .map_err(|_| SqlError::runtime("malformed JSON"))?;
                    let doc = parse_json_metered(s, true, ctx)?;
                    (render_json_value_metered(&doc, ctx)?, true)
                }
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
                int_value(&args[1])
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
                    let ok = if let Ok(s) = std::str::from_utf8(&b) {
                        if (flags & 2) != 0 {
                            parse_json_with_pos(s, true).is_ok()
                        } else if (flags & 1) != 0 {
                            parse_json_with_pos(s, false).is_ok()
                        } else {
                            false
                        }
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
                Some(b) => match std::str::from_utf8(&b) {
                    Ok(s) => match parse_json_with_pos(s, true) {
                        Ok(_) => SqlValue::Int(0),
                        Err(pos) => SqlValue::Int(pos as i64),
                    },
                    Err(_) => SqlValue::Int(1),
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
            let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
            let doc = parse_json(s, true)?;
            let target = if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                let steps = parse_json_path(&ps)?;
                lookup_path(&doc, &steps)
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
            let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
            let doc = parse_json(s, true)?;
            let target = if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                let steps = parse_json_path(&ps)?;
                lookup_path(&doc, &steps)
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
            let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
            let doc = parse_json_metered(s, true, ctx)?;
            if args.len() == 2 {
                let Some(pb) = text_bytes_of(&args[1]) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                let steps = parse_json_path(&ps)?;
                if let Some(node) = lookup_path(&doc, &steps) {
                    let is_j = matches!(node, JsonVal::Array(_) | JsonVal::Object(_));
                    (render_json_extracted_metered(node, ctx)?, is_j)
                } else {
                    (SqlValue::Null, false)
                }
            } else {
                let mut arr = Vec::with_capacity(args.len() - 1);
                for arg in &args[1..] {
                    let Some(pb) = text_bytes_of(arg) else {
                        arr.push(JsonVal::Null);
                        continue;
                    };
                    let ps = String::from_utf8_lossy(&pb);
                    let steps = parse_json_path(&ps)?;
                    arr.push(
                        lookup_path(&doc, &steps)
                            .cloned()
                            .unwrap_or(JsonVal::Null),
                    );
                }
                (render_json_value_metered(&JsonVal::Array(arr), ctx)?, true)
            }
        }
        "json_array" => {
            let mut items = Vec::with_capacity(args.len());
            for (idx, arg) in args.iter().enumerate() {
                items.push(json_value_from_sql(arg, ctx.arg_is_json(idx))?);
            }
            (render_json_value_metered(&JsonVal::Array(items), ctx)?, true)
        }
        "json_object" => {
            if !args.len().is_multiple_of(2) {
                return Err(SqlError::runtime(
                    "json_object() requires an even number of arguments",
                ));
            }
            let mut entries = Vec::with_capacity(args.len() / 2);
            let mut i = 0;
            while i < args.len() {
                let SqlValue::Text(ref kb) = args[i] else {
                    return Err(SqlError::runtime("json_object() labels must be TEXT"));
                };
                let key = String::from_utf8_lossy(kb).into_owned();
                let val = json_value_from_sql(&args[i + 1], ctx.arg_is_json(i + 1))?;
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
            let jv = json_value_from_sql(&args[0], ctx.arg_is_json(0))?;
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
            let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
            let mut doc = parse_json_metered(s, true, ctx)?;
            let mode = match lower_name {
                "json_insert" => MutMode::Insert,
                "json_replace" => MutMode::Replace,
                _ => MutMode::Set,
            };
            let mut i = 1;
            while i + 1 < args.len() {
                if let Some(pb) = text_bytes_of(&args[i]) {
                    let ps = String::from_utf8_lossy(&pb);
                    let steps = parse_json_path(&ps)?;
                    let val = json_value_from_sql(&args[i + 1], ctx.arg_is_json(i + 1))?;
                    mutate_path(&mut doc, &steps, val, mode);
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
            let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
            let mut doc = parse_json_metered(s, true, ctx)?;
            let mut removed_root = false;
            for arg in &args[1..] {
                let Some(pb) = text_bytes_of(arg) else {
                    return Ok(Some((SqlValue::Null, false)));
                };
                let ps = String::from_utf8_lossy(&pb);
                let steps = parse_json_path(&ps)?;
                if steps.is_empty() {
                    removed_root = true;
                } else if !removed_root {
                    remove_path(&mut doc, &steps);
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
            let d1 = parse_json_metered(
                std::str::from_utf8(&b1).map_err(|_| SqlError::runtime("malformed JSON"))?,
                true,
                ctx,
            )?;
            let d2 = parse_json_metered(
                std::str::from_utf8(&b2).map_err(|_| SqlError::runtime("malformed JSON"))?,
                true,
                ctx,
            )?;
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
            let doc = parse_json_metered(
                std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?,
                true,
                ctx,
            )?;
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
    match lower_name {
        "json_each" => Ok(Some(eval_json_tvf(args, false, ctx)?)),
        "json_tree" => Ok(Some(eval_json_tvf(args, true, ctx)?)),
        "generate_series" => Ok(Some(eval_generate_series(args, ctx)?)),
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
    if args.is_empty() || args.len() > 2 {
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
    let s = std::str::from_utf8(&b).map_err(|_| SqlError::runtime("malformed JSON"))?;
    let doc = parse_json_metered(s, true, ctx)?;
    let (root_path, parent_path, root_key, target) = if args.len() == 2 {
        let Some(pb) = text_bytes_of(&args[1]) else {
            return Ok(TableValuedResult {
                columns,
                rows: Vec::new(),
            });
        };
        let ps = String::from_utf8_lossy(&pb).into_owned();
        let steps = parse_json_path(&ps)?;
        let Some(node) = lookup_path(&doc, &steps) else {
            return Ok(TableValuedResult {
                columns,
                rows: Vec::new(),
            });
        };
        // Reconstruct canonical parent path & root key matching SQLite `json_tree(doc, path)`.
        let mut cur_path = String::from("$");
        let mut p_path = String::from("$");
        let mut r_key = SqlValue::Null;
        let mut cur_node = &doc;
        for (idx, step) in steps.iter().enumerate() {
            if idx + 1 == steps.len() {
                p_path = cur_path.clone();
            }
            match (cur_node, step) {
                (JsonVal::Object(entries), PathStep::Key(k)) => {
                    cur_path = format_json_path_key(&cur_path, k);
                    if idx + 1 == steps.len() {
                        r_key = SqlValue::text(k.as_str());
                    }
                    cur_node = entries.iter().find(|(ek, _)| ek == k).map(|(_, v)| v).unwrap();
                }
                (JsonVal::Array(items), PathStep::Index(i)) => {
                    cur_path = format!("{cur_path}[{i}]");
                    if idx + 1 == steps.len() {
                        r_key = SqlValue::Int(*i as i64);
                    }
                    cur_node = &items[*i];
                }
                (JsonVal::Array(items), PathStep::FromEnd(Some(n))) => {
                    let i = items.len() - *n;
                    cur_path = format!("{cur_path}[{i}]");
                    if idx + 1 == steps.len() {
                        r_key = SqlValue::Int(i as i64);
                    }
                    cur_node = &items[i];
                }
                _ => break,
            }
        }
        (cur_path, p_path, r_key, node)
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
) -> SqlResult<TableValuedResult> {
    if args.is_empty() || args.len() > 3 {
        return Err(SqlError::runtime(
            "wrong number of arguments to function generate_series()",
        ));
    }
    let columns = vec!["value".to_string()];
    if args.iter().any(SqlValue::is_null) {
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
