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

#[test]
fn memory_vfs_quota_and_unlinked_open_handle_accounting() {
    let vfs = MemoryVfs::new(MemoryVfsOptions {
        lock_manager: None,
        max_file_size: 100,
        max_total_bytes: Some(100),
    });

    // set_file exceeding quota must not leave behind a phantom empty file.
    let err = vfs.set_file("too_big.db", vec![0u8; 101]).unwrap_err();
    assert_eq!(err.code, IoErrorCode::Full);
    assert!(!vfs.exists("too_big.db").unwrap());
    assert_eq!(vfs.total_bytes(), 0);

    // Deleting an open file must keep its bytes charged until the last handle closes.
    let mut h1 = vfs.open("live.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    h1.write(&[1u8; 60], 0).unwrap();
    assert_eq!(vfs.total_bytes(), 60);
    vfs.delete("live.db", false).unwrap();
    assert!(!vfs.exists("live.db").unwrap());
    assert_eq!(vfs.total_bytes(), 60);

    // Another file cannot bypass the aggregate quota while the unlinked handle holds 60B.
    let mut h2 = vfs.open("other.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    let err = h2.write(&[2u8; 41], 0).unwrap_err();
    assert_eq!(err.code, IoErrorCode::Full);
    h2.write(&[2u8; 40], 0).unwrap();
    assert_eq!(vfs.total_bytes(), 100);

    // Truncating or closing the unlinked handle updates only its own bytes, never corrupting h2.
    h1.truncate(10).unwrap();
    assert_eq!(vfs.total_bytes(), 50);
    h1.close().unwrap();
    assert_eq!(vfs.total_bytes(), 40);
    h2.close().unwrap();
    assert_eq!(vfs.total_bytes(), 40);
    vfs.delete("other.db", false).unwrap();
    assert_eq!(vfs.total_bytes(), 0);
}

#[test]
fn lock_upgrade_shared_to_exclusive_holds_pending_against_new_readers() {
    let lm = LockManager::new();
    let mut r1 = lm.open_handle("db");
    let mut r2 = lm.open_handle("db");
    let mut r3 = lm.open_handle("db");

    assert!(r1.lock(LockLevel::Shared).unwrap());
    assert!(r2.lock(LockLevel::Shared).unwrap());
    // Direct SHARED -> EXCLUSIVE while r2 holds SHARED must acquire PENDING and block new readers.
    assert!(!r1.lock(LockLevel::Exclusive).unwrap());
    assert_eq!(r1.level(), LockLevel::Pending);
    assert!(!r3.lock(LockLevel::Shared).unwrap());

    r2.unlock(LockLevel::None).unwrap();
    assert!(r1.lock(LockLevel::Exclusive).unwrap());
    assert_eq!(r1.level(), LockLevel::Exclusive);
}

#[test]
fn budget_meter_overflow_safe_and_non_wrapping() {
    use xdb_core::{BudgetMeter, LimitKind, Meter, ResourceLimits};

    let m_steps = BudgetMeter::new(ResourceLimits {
        max_steps: Some(100),
        max_memory_bytes: None,
        max_page_reads: None,
    });
    m_steps.charge_steps(50).unwrap();
    // Adding u64::MAX must not wrap around to a value <= 100.
    let err = m_steps.charge_steps(u64::MAX).unwrap_err();
    assert_eq!(err.kind, LimitKind::Steps);
    assert_eq!(m_steps.usage().steps, 50);

    let m_mem = BudgetMeter::new(ResourceLimits {
        max_steps: None,
        max_memory_bytes: Some(100),
        max_page_reads: None,
    });
    m_mem.reserve_memory(50).unwrap();
    let err = m_mem.reserve_memory(u64::MAX).unwrap_err();
    assert_eq!(err.kind, LimitKind::Memory);
    assert_eq!(m_mem.usage().memory_bytes, 50);
}

#[test]
fn value_utf16_binary_collation_nan_and_fp_edge_cases() {
    // SQLite UTF-16 BINARY ordering compares UTF-16 code units:
    // U+10000 (surrogate 0xD800 0xDC00) < U+E000 (0xE000) in UTF-16, opposite of UTF-8!
    let utf16_bin = Collation::builtin(BuiltinCollation::Binary, xdb_core::TextEncoding::Utf16Le);
    let utf8_bin = Collation::builtin(BuiltinCollation::Binary, xdb_core::TextEncoding::Utf8);
    assert_eq!(
        utf8_bin.compare_utf8("\u{10000}".as_bytes(), "\u{E000}".as_bytes()),
        Ordering::Greater
    );
    assert_eq!(
        utf16_bin.compare_utf8("\u{10000}".as_bytes(), "\u{E000}".as_bytes()),
        Ordering::Less
    );

    // SQLite ticket #3922: Real(-2^63) has fractional-safe exact representation as float but
    // must_be_int rejects -9223372036854775808.0 just like SQLite's sqlite3VdbeIntValue / mustBeInt.
    assert_eq!(must_be_int(&SqlValue::Real(-9223372036854775808.0)), None);
    assert_eq!(must_be_int(&SqlValue::Real(42.0)), Some(42));

    // atof trailing exponent sign without digits stops before 'e' (matching C SQLite).
    let res = atof(b"1e+");
    assert_eq!(res.value, 1.0);
    assert_eq!(res.end, 1);
    assert_eq!(atof(b"1e-").end, 1);

    // fp_core edge cases must never panic with overflow-checks=true.
    assert_eq!(fp10_convert2_full(0, 0), 0.0);
    let dec = fp_decode(1.0, i32::MIN, 0);
    assert_eq!(dec.special, FpSpecial::Finite);

    // mem_compare strict weak ordering if a raw Real(NaN) ever slips in.
    let nan = SqlValue::Real(f64::NAN);
    assert_eq!(mem_compare(&nan, &SqlValue::Null, Some(&utf8_bin)), Ordering::Equal);
    assert_eq!(mem_compare(&SqlValue::Null, &nan, Some(&utf8_bin)), Ordering::Equal);
    assert_eq!(mem_compare(&nan, &SqlValue::Int(0), Some(&utf8_bin)), Ordering::Less);
    assert_eq!(mem_compare(&SqlValue::Int(0), &nan, Some(&utf8_bin)), Ordering::Greater);
}

#[test]
fn memory_vfs_lock_key_matches_open_and_tracks_recreate() {
    let lm = LockManager::new();
    let vfs = MemoryVfs::new(MemoryVfsOptions {
        lock_manager: Some(lm.clone()),
        max_file_size: 1024 * 1024,
        max_total_bytes: None,
    });

    let mut f1 = vfs.open("app.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    let k1 = vfs.lock_key("app.db");
    assert!(f1.lock(LockLevel::Shared).unwrap());
    assert!(f1.lock(LockLevel::Exclusive).unwrap());

    // An external handle created via `vfs.lock_key("app.db")` must collide with `f1`'s lock!
    let mut ext = lm.open_handle(&k1);
    assert!(!ext.lock(LockLevel::Shared).unwrap());
    f1.unlock(LockLevel::None).unwrap();
    assert!(ext.lock(LockLevel::Shared).unwrap());
    ext.unlock(LockLevel::None).unwrap();

    // Unlink and recreate at the same path -> new node gets a distinct lock key.
    vfs.delete("app.db", false).unwrap();
    let _f2 = vfs.open("app.db", OpenFlags::create(FileKind::MainDb)).unwrap();
    let k2 = vfs.lock_key("app.db");
    assert_ne!(k1, k2);
}
