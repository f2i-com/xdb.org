//! Collating sequences, ported from SQLite 3.54.1 `main.c`, `util.c`, `callback.c`, `vdbeaux.c`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::utf::{sqlite_utf16_to_utf8, TextEncoding};

/// SQLite's three built-in collations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinCollation {
    Binary,
    Nocase,
    Rtrim,
}

impl BuiltinCollation {
    pub fn name(self) -> &'static str {
        match self {
            BuiltinCollation::Binary => "BINARY",
            BuiltinCollation::Nocase => "NOCASE",
            BuiltinCollation::Rtrim => "RTRIM",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("BINARY") {
            Some(BuiltinCollation::Binary)
        } else if name.eq_ignore_ascii_case("NOCASE") {
            Some(BuiltinCollation::Nocase)
        } else if name.eq_ignore_ascii_case("RTRIM") {
            Some(BuiltinCollation::Rtrim)
        } else {
            None
        }
    }
}

#[inline]
fn fold_ascii(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 0x20
    } else {
        b
    }
}

/// `sqlite3BinaryCompare` (`main.c:1059`) / `sqlite3BlobCompare`: `memcmp`, then length.
#[inline]
pub fn compare_bytes_binary(a: &[u8], b: &[u8]) -> Ordering {
    a.cmp(b)
}

/// `nocaseCollatingFunc` (`main.c:1116`) over UTF-8 bytes with `sqlite3_strnicmp` (`util.c:442`):
/// folds ASCII `A-Z` only and stops at a NUL byte in the left operand (if both have NUL at
/// that position, total byte lengths decide, so `"a\0x" == "A\0y"` under `NOCASE`).
pub fn compare_bytes_nocase_utf8(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let x = a[i];
        if x == 0 {
            let fy = fold_ascii(b[i]);
            if fy != 0 {
                return Ordering::Less;
            }
            return a.len().cmp(&b.len());
        }
        let fx = fold_ascii(x);
        let fy = fold_ascii(b[i]);
        if fx != fy {
            return fx.cmp(&fy);
        }
    }
    a.len().cmp(&b.len())
}

/// `rtrimCollFunc` (`main.c:1084`) over UTF-8 bytes: drop trailing `0x20` bytes, then `memcmp`.
pub fn compare_bytes_rtrim_utf8(a: &[u8], b: &[u8]) -> Ordering {
    let mut na = a.len();
    let mut nb = b.len();
    while na > 0 && a[na - 1] == b' ' {
        na -= 1;
    }
    while nb > 0 && b[nb - 1] == b' ' {
        nb -= 1;
    }
    a[..na].cmp(&b[..nb])
}

/// Compare two stored `TEXT` values (bytes in database encoding `enc`) with a built-in collation:
/// `BINARY` is `memcmp` of stored bytes in every encoding; `NOCASE`/`RTRIM` in UTF-16 first
/// translate both operands to UTF-8 via [`sqlite_utf16_to_utf8`].
pub fn compare_text_bytes(
    coll: BuiltinCollation,
    a: &[u8],
    b: &[u8],
    enc: TextEncoding,
) -> Ordering {
    if coll == BuiltinCollation::Binary {
        return compare_bytes_binary(a, b);
    }
    if enc != TextEncoding::Utf8 {
        let ua = sqlite_utf16_to_utf8(a, enc);
        let ub = sqlite_utf16_to_utf8(b, enc);
        return match coll {
            BuiltinCollation::Nocase => compare_bytes_nocase_utf8(&ua, &ub),
            BuiltinCollation::Rtrim => compare_bytes_rtrim_utf8(&ua, &ub),
            BuiltinCollation::Binary => unreachable!(),
        };
    }
    match coll {
        BuiltinCollation::Nocase => compare_bytes_nocase_utf8(a, b),
        BuiltinCollation::Rtrim => compare_bytes_rtrim_utf8(a, b),
        BuiltinCollation::Binary => unreachable!(),
    }
}

/// User-supplied collation comparator over UTF-8 `TEXT` byte slices.
pub type CustomCollationFn = Arc<dyn Fn(&[u8], &[u8]) -> Ordering + Send + Sync>;

/// Either a built-in collation (with encoding-aware byte comparison) or a user collation.
#[derive(Clone)]
pub enum Collation {
    Builtin {
        kind: BuiltinCollation,
        encoding: TextEncoding,
    },
    Custom {
        name: String,
        func: CustomCollationFn,
    },
}

impl fmt::Debug for Collation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Collation::Builtin { kind, encoding } => f
                .debug_struct("Collation::Builtin")
                .field("kind", kind)
                .field("encoding", encoding)
                .finish(),
            Collation::Custom { name, .. } => f
                .debug_struct("Collation::Custom")
                .field("name", name)
                .finish(),
        }
    }
}

impl Collation {
    pub const BINARY_UTF8: Self = Collation::Builtin {
        kind: BuiltinCollation::Binary,
        encoding: TextEncoding::Utf8,
    };
    pub const NOCASE_UTF8: Self = Collation::Builtin {
        kind: BuiltinCollation::Nocase,
        encoding: TextEncoding::Utf8,
    };
    pub const RTRIM_UTF8: Self = Collation::Builtin {
        kind: BuiltinCollation::Rtrim,
        encoding: TextEncoding::Utf8,
    };

    pub fn builtin(kind: BuiltinCollation, encoding: TextEncoding) -> Self {
        Collation::Builtin { kind, encoding }
    }

    pub fn name(&self) -> &str {
        match self {
            Collation::Builtin { kind, .. } => kind.name(),
            Collation::Custom { name, .. } => name,
        }
    }

    /// Compare two `TEXT` byte slices stored in the connection's UTF-8 form.
    pub fn compare_utf8(&self, a: &[u8], b: &[u8]) -> Ordering {
        match self {
            Collation::Builtin { kind, .. } => {
                compare_text_bytes(*kind, a, b, TextEncoding::Utf8)
            }
            Collation::Custom { func, .. } => func(a, b),
        }
    }

    /// Compare two `TEXT` byte slices stored in database encoding `enc`.
    pub fn compare_stored(&self, a: &[u8], b: &[u8], enc: TextEncoding) -> Ordering {
        match self {
            Collation::Builtin { kind, .. } => compare_text_bytes(*kind, a, b, enc),
            Collation::Custom { func, .. } => {
                if enc == TextEncoding::Utf8 {
                    func(a, b)
                } else {
                    let ua = sqlite_utf16_to_utf8(a, enc);
                    let ub = sqlite_utf16_to_utf8(b, enc);
                    func(&ua, &ub)
                }
            }
        }
    }
}

/// Connection collation registry (ASCII case-insensitive name lookup).
#[derive(Debug, Clone)]
pub struct CollationRegistry {
    encoding: TextEncoding,
    by_name: HashMap<String, Collation>,
}

impl CollationRegistry {
    pub fn new(encoding: TextEncoding) -> Self {
        let mut by_name = HashMap::new();
        for kind in [
            BuiltinCollation::Binary,
            BuiltinCollation::Nocase,
            BuiltinCollation::Rtrim,
        ] {
            by_name.insert(kind.name().to_string(), Collation::builtin(kind, encoding));
        }
        Self { encoding, by_name }
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    pub fn register(&mut self, name: impl Into<String>, func: CustomCollationFn) -> Collation {
        let name = name.into();
        let key = name.to_ascii_uppercase();
        let coll = Collation::Custom { name, func };
        self.by_name.insert(key, coll.clone());
        coll
    }

    pub fn unregister(&mut self, name: &str) -> bool {
        let key = name.to_ascii_uppercase();
        if matches!(key.as_str(), "BINARY" | "NOCASE" | "RTRIM") {
            return false;
        }
        self.by_name.remove(&key).is_some()
    }

    pub fn find(&self, name: &str) -> Option<&Collation> {
        self.by_name.get(&name.to_ascii_uppercase())
    }
}

/// Collation metadata carried by an expression operand (`sqlite3ExprCollSeq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperandCollation<'a> {
    pub name: Option<&'a str>,
    pub explicit: bool,
}

/// `sqlite3BinaryCompareCollSeq` (`expr.c:432`).
pub fn comparison_collation_name<'a>(
    left: Option<OperandCollation<'a>>,
    right: Option<OperandCollation<'a>>,
) -> Option<&'a str> {
    if let Some(ref l) = left {
        if l.explicit {
            return l.name;
        }
    }
    if let Some(ref r) = right {
        if r.explicit {
            return r.name;
        }
    }
    left.and_then(|l| l.name).or_else(|| right.and_then(|r| r.name))
}
