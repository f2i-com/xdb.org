//! Text helpers shared by the parser and the `build.c` model: `sqlite3Dequote`,
//! ASCII case-insensitive comparison, and the declared-type text rules of
//! `sqlite3AddColumn` (`build.c:1550`).

use crate::tokenizer::{is_quote_char, is_sql_space};

/// `sqlite3Dequote` (`util.c:305`) on raw token text: remove the quotes of
/// `"x"`, `'x'`, `` `x` `` or `[x]`, un-doubling inner quotes.
pub fn dequote(z: &str) -> String {
    let bytes = z.as_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    let mut q = bytes[0];
    if !is_quote_char(q) {
        return z.to_string();
    }
    if q == b'[' {
        q = b']';
    }
    let len = bytes.len();
    if len >= 2 {
        let inner = &bytes[1..len - 1];
        if bytes[len - 1] == q && !inner.contains(&q) {
            return z[1..len - 1].to_string();
        }
    }
    let mut out = String::with_capacity(len.saturating_sub(2));
    let mut run_start = 1;
    let mut i = 1;
    while i < len {
        if bytes[i] == q {
            if i + 1 < len && bytes[i + 1] == q {
                out.push_str(&z[run_start..=i]);
                i += 2;
                run_start = i;
                continue;
            } else {
                out.push_str(&z[run_start..i]);
                return out;
            }
        }
        i += 1;
    }
    out.push_str(&z[run_start..]);
    out
}

/// `sqlite3StrICmp(a, b) == 0`: ASCII-only case folding.
#[inline]
pub fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn matches_at(a: &str, at: usize, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if at + bb.len() > ab.len() {
        return false;
    }
    ab[at..at + bb.len()].eq_ignore_ascii_case(bb)
}

/// The standard type names (`global.c` `sqlite3StdType`): a column whose type is
/// exactly one of these has `Column.eCType` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardType {
    Any,
    Blob,
    Int,
    Integer,
    Real,
    Text,
}

impl StandardType {
    pub const ALL: &'static [StandardType] = &[
        StandardType::Any,
        StandardType::Blob,
        StandardType::Int,
        StandardType::Integer,
        StandardType::Real,
        StandardType::Text,
    ];

    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            StandardType::Any => "ANY",
            StandardType::Blob => "BLOB",
            StandardType::Int => "INT",
            StandardType::Integer => "INTEGER",
            StandardType::Real => "REAL",
            StandardType::Text => "TEXT",
        }
    }
}

/// Output of [`stored_type`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredType {
    pub stored: Option<String>,
    pub standard_type: Option<StandardType>,
    pub pragma_type: String,
}

/// The declared type as `sqlite3AddColumn` stores it, from the raw `typetoken`
/// text (`None` when the column has no type): trailing `"generated always"`
/// trim (`build.c:1573`), `sqlite3DequoteToken`, standard-type check, then
/// `sqlite3Dequote`.
pub fn stored_type(raw_type: Option<&str>) -> StoredType {
    let Some(raw) = raw_type else {
        return StoredType {
            stored: None,
            standard_type: None,
            pragma_type: String::new(),
        };
    };
    if raw.is_empty() {
        return StoredType {
            stored: None,
            standard_type: None,
            pragma_type: String::new(),
        };
    }
    let bytes = raw.as_bytes();
    let mut n = bytes.len();
    if n >= 16 && matches_at(raw, n - 6, "always") {
        let mut m = n - 6;
        while m > 0 && is_sql_space(bytes[m - 1]) {
            m -= 1;
        }
        n = m;
        if m >= 9 && matches_at(raw, m - 9, "generated") {
            m -= 9;
            while m > 0 && is_sql_space(bytes[m - 1]) {
                m -= 1;
            }
            n = m;
        }
    }
    let mut z = &raw[..n];
    if n >= 3 {
        let zb = z.as_bytes();
        // sqlite3DequoteToken (util.c:383): strip matching outer quotes when no inner quotes exist.
        let expected_close = match zb[0] {
            b'"' => Some(b'"'),
            b'\'' => Some(b'\''),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(close) = expected_close {
            if zb[n - 1] == close {
                let mut inner = true;
                let mut i = 1;
                while i < n - 1 {
                    if is_quote_char(zb[i]) || zb[i] == close {
                        inner = false;
                        break;
                    }
                    i += 1;
                }
                if inner {
                    z = &z[1..n - 1];
                    n -= 2;
                }
            }
        }
        for &std_ty in StandardType::ALL {
            let s = std_ty.as_str();
            if n == s.len() && same_name(z, s) {
                return StoredType {
                    stored: None,
                    standard_type: Some(std_ty),
                    pragma_type: s.to_string(),
                };
            }
        }
    }
    if n == 0 {
        return StoredType {
            stored: None,
            standard_type: None,
            pragma_type: String::new(),
        };
    }
    let stored = dequote(z);
    StoredType {
        pragma_type: stored.clone(),
        stored: Some(stored),
        standard_type: None,
    }
}
