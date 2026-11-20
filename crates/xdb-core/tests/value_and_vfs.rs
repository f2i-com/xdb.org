//! Unit tests for SQLite 3.54.1 value semantics, locking, and quota-enforced VFS.

use std::cmp::Ordering;

use xdb_core::value::*;
use xdb_core::vfs::*;

#[test]
fn fp_formatting_matches_sqlite_rules() {
    assert_eq!(real_to_text(1.0, DEFAULT_FP_DIGITS), "1.0");
    assert_eq!(real_to_text(-0.0, DEFAULT_FP_DIGITS), "0.0");
    assert_eq!(real_to_text(0.1, DEFAULT_FP_DIGITS), "0.1");
    assert_eq!(real_to_text(49.47, DEFAULT_FP_DIGITS), "49.47");
    assert_eq!(real_to_text(1e17, DEFAULT_FP_DIGITS), "1.0e+17");
    assert_eq!(real_to_text(1e-5, DEFAULT_FP_DIGITS), "1.0e-05");
    assert_eq!(real_to_text(f64::INFINITY, DEFAULT_FP_DIGITS), "Inf");
    assert_eq!(real_to_text(f64::NEG_INFINITY, DEFAULT_FP_DIGITS), "-Inf");
    // SQLite 19-digit truncation quirk vs standard strtod:
    assert_eq!(
        atof(b"3500000000000000.2500001").value,
        3500000000000000.0
    );
}

#[test]
fn atof_and_atoi64_edge_cases() {
    // Embedded NUL asymmetry: atof stops at NUL (rc > 0), atoi64 sees trailing junk (rc == 1).
    let nul_num = b"12\0abc";
    assert!(atof(nul_num).rc > 0);
    assert_eq!(atof(nul_num).value, 12.0);
    assert_eq!(atoi64(nul_num), Atoi64Result { rc: 1, value: 12 });
    // Consequently arithmetic treats "12\0" as REAL 12.0!
    assert!(text_numeric_value(b"12\0").exact_eq(&SqlValue::Real(12.0)));

    // Exact 2^63 boundary.
    assert_eq!(
        atoi64(b"9223372036854775807"),
        Atoi64Result {
            rc: 0,
            value: i64::MAX
        }
    );
    assert_eq!(
        atoi64(b"9223372036854775808"),
        Atoi64Result {
            rc: 3,
            value: i64::MAX
        }
    );
    assert_eq!(
        atoi64(b"-9223372036854775808"),
        Atoi64Result {
            rc: 0,
            value: i64::MIN
        }
    );
    assert_eq!(
        atoi64(b"-9223372036854775809"),
        Atoi64Result {
            rc: 2,
            value: i64::MIN
        }
    );

    // Hex literals.
    assert_eq!(
        dec_or_hex_to_i64(b"0xffffffffffffffff"),
        Atoi64Result { rc: 0, value: -1 }
    );
    assert!(integer_literal_value(b"0x10000000000000000", false).is_none());
    assert!(integer_literal_value(b"9223372036854775808", true)
        .unwrap()
        .exact_eq(&SqlValue::Int(i64::MIN)));
    assert!(integer_literal_value(b"9223372036854775808", false)
        .unwrap()
        .exact_eq(&SqlValue::Real(9223372036854775808.0)));

    assert_eq!(get_int32(b"0x7fffffff"), Some(i32::MAX));
    assert_eq!(get_int32(b"0x80000000"), None);
    assert_eq!(get_int32(b"-2147483648"), Some(i32::MIN));
    assert_eq!(get_int32(b"-2147483649"), None);
}

#[test]
fn int_float_compare_is_mathematically_exact() {
    assert_eq!(
        int_float_compare(i64::MAX, 9223372036854775808.0),
        Ordering::Less
    );
    assert_eq!(
        int_float_compare(i64::MAX, 9223372036854774784.0),
        Ordering::Greater
    );
    assert_eq!(
        int_float_compare(i64::MIN, -9223372036854775808.0),
        Ordering::Equal
    );
    assert_eq!(
        int_float_compare(i64::MIN + 1, -9223372036854775808.0),
        Ordering::Greater
    );
    assert_eq!(
        int_float_compare(9007199254740993, 9007199254740992.0),
        Ordering::Greater
    );
    assert_eq!(int_float_compare(0, -0.5), Ordering::Greater);
    assert_eq!(int_float_compare(0, -0.0), Ordering::Equal);
    assert_eq!(int_float_compare(0, 0.5), Ordering::Less);
    assert_eq!(int_float_compare(0, f64::INFINITY), Ordering::Less);
    assert_eq!(int_float_compare(0, f64::NEG_INFINITY), Ordering::Greater);
}

#[test]
fn affinity_and_cast_preserve_sqlite_semantics_and_raw_utf8() {
    assert_eq!(affinity_type(b"FLOATING POINT"), Affinity::Integer);
    assert_eq!(affinity_type(b"CHARINT"), Affinity::Integer);
    assert_eq!(affinity_type(b"VARCHAR(20)"), Affinity::Text);
    assert_eq!(affinity_type(b"STRING"), Affinity::Numeric);
    assert_eq!(affinity_of_decl_type(None), Affinity::Blob);
    assert_eq!(affinity_of_decl_type(Some(b"")), Affinity::Blob);

    // REAL column turns integer into REAL on read-back; -0.0 becomes +0.0.
    assert!(apply_affinity(SqlValue::text("5"), Affinity::Real).exact_eq(&SqlValue::Real(5.0)));
    assert!(apply_affinity(SqlValue::Real(-0.0), Affinity::Real).exact_eq(&SqlValue::Real(0.0)));

    // Rust preserves invalid UTF-8 bytes verbatim in TEXT <-> BLOB casts!
    let raw = vec![0xffu8, 0x80, 0x00, 0xfe];
    let as_text = cast_value(
        SqlValue::blob(raw.clone()),
        Affinity::Text,
        TextEncoding::Utf8,
    )
    .unwrap();
    let roundtrip = cast_value(as_text, Affinity::Blob, TextEncoding::Utf8).unwrap();
    assert_eq!(roundtrip.as_blob(), Some(raw.as_slice()));
}

#[test]
fn collations_match_sqlite() {
    // Astral vs U+E000: UTF-8 byte order puts U+E000 (EE 80 80) BEFORE U+10000 (F0 90 80 80).
    let bmp_hi = "\u{E000}".as_bytes();
    let astral = "\u{10000}".as_bytes();
    assert_eq!(compare_bytes_binary(bmp_hi, astral), Ordering::Less);

    // NOCASE: ASCII-only + NUL stops comparison (then length decides).
    assert_eq!(compare_bytes_nocase_utf8(b"a\0x", b"A\0y"), Ordering::Equal);
    assert_eq!(
        compare_bytes_nocase_utf8(b"a\0x", b"A\0yy"),
        Ordering::Less
    );
    assert_eq!(
        compare_bytes_nocase_utf8("Ä".as_bytes(), "ä".as_bytes()),
        Ordering::Less
    );

    // RTRIM: trailing ASCII space only.
    assert_eq!(compare_bytes_rtrim_utf8(b"abc   ", b"abc"), Ordering::Equal);
    assert_eq!(compare_bytes_rtrim_utf8(b"abc\t", b"abc"), Ordering::Greater);
}

#[test]
fn arithmetic_and_bitwise_overflow_and_shifts() {
    assert!(add(&SqlValue::Int(i64::MAX), &SqlValue::Int(1))
        .exact_eq(&SqlValue::Real(9223372036854775808.0)));
    assert!(divide(&SqlValue::Int(7), &SqlValue::Int(2)).exact_eq(&SqlValue::Int(3)));
    assert!(divide(&SqlValue::Int(1), &SqlValue::Int(0)).is_null());
    assert!(divide(&SqlValue::Int(i64::MIN), &SqlValue::Int(-1))
        .exact_eq(&SqlValue::Real(9223372036854775808.0)));
    assert!(remainder(&SqlValue::Int(i64::MIN), &SqlValue::Int(-1)).exact_eq(&SqlValue::Int(0)));
    assert!(shift_left(&SqlValue::Int(1), &SqlValue::Int(64)).exact_eq(&SqlValue::Int(0)));
    assert!(shift_right(&SqlValue::Int(-5), &SqlValue::Int(64)).exact_eq(&SqlValue::Int(-1)));
    assert!(shift_left(&SqlValue::Int(8), &SqlValue::Int(-2)).exact_eq(&SqlValue::Int(2)));
    assert!(bit_not(&SqlValue::Int(0)).exact_eq(&SqlValue::Int(-1)));
}

#[test]
fn memory_vfs_locking_and_sandbox_quotas() {
    let vfs = MemoryVfs::new(MemoryVfsOptions {
        lock_manager: None,
        max_file_size: 100,
        max_total_bytes: Some(150),
    });
    let mut f1 = vfs.open("a.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    let mut f2 = vfs.open("b.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    f1.write(&[1u8; 80], 0).unwrap();
    f2.write(&[2u8; 70], 0).unwrap();
    assert_eq!(vfs.total_bytes(), 150);
    // Exceeds aggregate sandbox quota (150).
    let err = f2.write(&[3u8; 1], 70).unwrap_err();
    assert_eq!(err.code, IoErrorCode::Full);

    // 5-level SQLite lock transitions on the same file.
    let mut h1 = vfs.open("a.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    let mut h2 = vfs.open("a.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    assert!(h1.lock(LockLevel::Shared).unwrap());
    assert!(h2.lock(LockLevel::Shared).unwrap());
    assert!(h1.lock(LockLevel::Reserved).unwrap());
    assert!(!h2.lock(LockLevel::Reserved).unwrap());
    // h1 requests EXCLUSIVE while h2 holds SHARED -> moves to PENDING, returns false.
    assert!(!h1.lock(LockLevel::Exclusive).unwrap());
    assert_eq!(h1.lock_level(), LockLevel::Pending);
    h2.unlock(LockLevel::None).unwrap();
    assert!(h1.lock(LockLevel::Exclusive).unwrap());
    assert_eq!(h1.lock_level(), LockLevel::Exclusive);
}
