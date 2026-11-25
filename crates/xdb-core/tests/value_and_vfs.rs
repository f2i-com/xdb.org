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

#[test]
fn pass3_hex_budget_meter_sticky_kind_and_vfs_bounds() {
    use xdb_core::{BudgetMeter, LimitKind, Meter, ResourceLimits};

    // 1. Bare "0x" / "0X" is not a valid hex integer in dec_or_hex_to_i64.
    assert_ne!(dec_or_hex_to_i64(b"0x").rc, 0);
    assert_ne!(dec_or_hex_to_i64(b"0X").rc, 0);
    let ok_hex = dec_or_hex_to_i64(b"0x10");
    assert_eq!(ok_hex.rc, 0);
    assert_eq!(ok_hex.value, 16);

    // 2. BudgetMeter preserves the original tripped LimitKind after exhaustion.
    let m = BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000),
        max_memory_bytes: Some(128),
        max_page_reads: Some(10),
    });
    let err1 = m.reserve_memory(256).unwrap_err();
    assert_eq!(err1.kind, LimitKind::Memory);
    // Subsequent step/page charges must STILL report LimitKind::Memory (sticky original cause).
    let err2 = m.charge_steps(1).unwrap_err();
    assert_eq!(err2.kind, LimitKind::Memory);
    let err3 = m.charge_page_reads(1).unwrap_err();
    assert_eq!(err3.kind, LimitKind::Memory);

    // 3. MemoryVfs: read-only open with delete_on_close must NOT delete existing file,
    //    and huge u64::MAX offset read/write must error cleanly without integer overflow.
    let vfs = MemoryVfs::new(MemoryVfsOptions::default());
    {
        let mut f = vfs.open("keep.db", OpenFlags::create(FileKind::MainDb)).unwrap();
        f.write(b"hello", 0).unwrap();
        f.close().unwrap();
    }
    {
        let mut ro_flags = OpenFlags::readonly(FileKind::MainDb);
        ro_flags.delete_on_close = true;
        let mut ro = vfs.open("keep.db", ro_flags).unwrap();
        let mut buf = [0xAAu8; 4];
        assert_eq!(ro.read(&mut buf, u64::MAX).unwrap(), 0);
        assert_eq!(buf, [0u8; 4]);
        ro.close().unwrap();
    }
    assert!(vfs.exists("keep.db").unwrap());
    {
        let mut rw = vfs.open("keep.db", OpenFlags::create(FileKind::MainDb)).unwrap();
        assert!(rw.write(b"x", u64::MAX).is_err());
    }
}

#[test]
fn pass4_wtf8_surrogates_utf16_binary_sticky_meter_and_unregister() {
    use std::sync::Arc;
    use xdb_core::value::utf::{decode_stored_text_to_utf8, encode_utf8_to_stored_text};
    use xdb_core::{BudgetMeter, LimitKind, Meter, ResourceLimits, TextEncoding};

    // 1. Trailing unpaired UTF-16 surrogates round-trip losslessly via WTF-8 (matching SQLite's
    //    `sqlite3VdbeMemTranslate`), and WTF-8 surrogate bytes encode & compare identically
    //    between streaming `compare_utf8` and raw stored UTF-16 `compare_stored`.
    let raw_le_hi: [u8; 4] = [0x41, 0x00, 0x00, 0xD8]; // "A" + trailing U+D800
    let raw_le_lo: [u8; 4] = [0x41, 0x00, 0xFF, 0xDF]; // "A" + trailing U+DFFF
    let wtf_hi = decode_stored_text_to_utf8(&raw_le_hi, TextEncoding::Utf16Le);
    let wtf_lo = decode_stored_text_to_utf8(&raw_le_lo, TextEncoding::Utf16Le);
    assert_eq!(encode_utf8_to_stored_text(&wtf_hi, TextEncoding::Utf16Le), raw_le_hi);
    assert_eq!(encode_utf8_to_stored_text(&wtf_lo, TextEncoding::Utf16Le), raw_le_lo);

    let bin_le = Collation::builtin(BuiltinCollation::Binary, TextEncoding::Utf16Le);
    assert_eq!(
        bin_le.compare_utf8(&wtf_hi, &wtf_lo),
        bin_le.compare_stored(&raw_le_hi, &raw_le_lo, TextEncoding::Utf16Le)
    );
    let raw_be_hi: [u8; 4] = [0x00, 0x41, 0xD8, 0x00];
    let wtf_be = decode_stored_text_to_utf8(&raw_be_hi, TextEncoding::Utf16Be);
    assert_eq!(encode_utf8_to_stored_text(&wtf_be, TextEncoding::Utf16Be), raw_be_hi);
    // Mid-string WTF-8 surrogate (`A` + U+D800 + `B`) preserves exact U+D800 code unit (not U+FFFD).
    let mid_wtf8 = [b'A', 0xED, 0xA0, 0x80, b'B'];
    assert_eq!(
        encode_utf8_to_stored_text(&mid_wtf8, TextEncoding::Utf16Le),
        [0x41, 0x00, 0x00, 0xD8, 0x42, 0x00]
    );

    // 2. BudgetMeter records sticky `tripped_used >= limit` even after memory guards drop.
    let m = BudgetMeter::new(ResourceLimits {
        max_steps: None,
        max_memory_bytes: Some(100),
        max_page_reads: None,
    });
    m.reserve_memory(60).unwrap();
    let e1 = m.reserve_memory(80).unwrap_err();
    assert_eq!(e1.kind, LimitKind::Memory);
    assert_eq!(e1.limit, 100);
    assert_eq!(e1.used, 140);
    m.release_memory(60);
    assert_eq!(m.usage().memory_bytes, 0);
    let e2 = m.charge_steps(1).unwrap_err();
    assert_eq!(e2.kind, LimitKind::Memory);
    assert_eq!(e2.used, 140);

    // 3. CollationRegistry::unregister removes custom override of a built-in name.
    let mut reg = CollationRegistry::new(TextEncoding::Utf8);
    reg.register("BINARY", Arc::new(|a, b| b.cmp(a)));
    assert_eq!(
        reg.find("BINARY").unwrap().compare_utf8(b"a", b"b"),
        Ordering::Greater
    );
    assert!(reg.unregister("binary"));
    assert_eq!(
        reg.find("BINARY").unwrap().compare_utf8(b"a", b"b"),
        Ordering::Less
    );
}

#[test]
fn pass5_utf16_mid_surrogates_fp_fastpath_and_concurrent_meter() {
    use std::sync::Arc;
    use xdb_core::value::utf::{decode_stored_text_to_utf8, encode_utf8_to_stored_text};
    use xdb_core::value::{
        format_float, real_to_text, real_to_text_buf, FloatConversion, FloatFormatSpec,
        DEFAULT_FP_DIGITS,
    };
    use xdb_core::{BudgetMeter, LimitKind, Meter, ResourceLimits, TextEncoding};

    // 1. Mid-string unpaired UTF-16 surrogates round-trip losslessly via WTF-8 and compare
    //    accurately under UTF-16 BINARY, NOCASE, and RTRIM collations.
    let raw_le_a: [u8; 6] = [0x41, 0x00, 0x00, 0xD8, 0x62, 0x00]; // "A" + U+D800 + "b"
    let raw_le_b: [u8; 6] = [0x41, 0x00, 0x00, 0xDC, 0x42, 0x00]; // "A" + U+DC00 + "B"
    let wtf_a = decode_stored_text_to_utf8(&raw_le_a, TextEncoding::Utf16Le);
    let wtf_b = decode_stored_text_to_utf8(&raw_le_b, TextEncoding::Utf16Le);
    assert_eq!(encode_utf8_to_stored_text(&wtf_a, TextEncoding::Utf16Le), raw_le_a);
    assert_eq!(encode_utf8_to_stored_text(&wtf_b, TextEncoding::Utf16Le), raw_le_b);

    let bin_le = Collation::builtin(BuiltinCollation::Binary, TextEncoding::Utf16Le);
    assert_eq!(
        bin_le.compare_utf8(&wtf_a, &wtf_b),
        bin_le.compare_stored(&raw_le_a, &raw_le_b, TextEncoding::Utf16Le)
    );
    // NOCASE on non-UTF-8 DB translates via SQLite's `sqlite3VdbeMemTranslate`, matching `compare_stored`!
    let nocase_le = Collation::builtin(BuiltinCollation::Nocase, TextEncoding::Utf16Le);
    assert_eq!(
        nocase_le.compare_utf8(&wtf_a, &wtf_b),
        nocase_le.compare_stored(&raw_le_a, &raw_le_b, TextEncoding::Utf16Le)
    );
    // Same trailing surrogate with ASCII case difference ("A" + "b" + U+D800 vs "a" + "B" + U+D800)
    let raw_le_c1: [u8; 6] = [0x41, 0x00, 0x62, 0x00, 0x00, 0xD8];
    let raw_le_c2: [u8; 6] = [0x61, 0x00, 0x42, 0x00, 0x00, 0xD8];
    let wtf_c1 = decode_stored_text_to_utf8(&raw_le_c1, TextEncoding::Utf16Le);
    let wtf_c2 = decode_stored_text_to_utf8(&raw_le_c2, TextEncoding::Utf16Le);
    assert_eq!(nocase_le.compare_utf8(&wtf_c1, &wtf_c2), Ordering::Equal);

    let raw_le_trail_sp: [u8; 8] = [0x41, 0x00, 0x00, 0xD8, 0x62, 0x00, 0x20, 0x00];
    let wtf_sp = decode_stored_text_to_utf8(&raw_le_trail_sp, TextEncoding::Utf16Le);
    let rtrim_le = Collation::builtin(BuiltinCollation::Rtrim, TextEncoding::Utf16Le);
    assert_eq!(rtrim_le.compare_utf8(&wtf_a, &wtf_sp), Ordering::Equal);

    // 2. Stack `real_to_text_buf` matches `real_to_text` across tricky float boundary values.
    let samples = [
        0.0f64,
        -0.0,
        1.0,
        -1.0,
        1.5,
        0.1,
        0.00001,
        0.000001,
        1e-22,
        1e-23,
        1e22,
        1e23,
        1e-300,
        1e300,
        9007199254740991.0,
        -9007199254740992.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut buf = [0u8; 48];
    for x in samples {
        let s1 = real_to_text(x, DEFAULT_FP_DIGITS);
        let s2 = real_to_text_buf(x, DEFAULT_FP_DIGITS, &mut buf);
        assert_eq!(s2, s1, "real_to_text_buf mismatch for {x:?}");
    }

    // 3. `format_float` honors width/precision cleanly (and clamps huge widths to FP_PRECISION_LIMIT).
    let mut spec = FloatFormatSpec::new(FloatConversion::F);
    spec.precision = Some(2);
    spec.width = 24;
    let formatted = format_float(1.5, spec);
    assert_eq!(formatted.len(), 24);
    assert!(formatted.ends_with("1.50"));

    // 4. Multi-threaded BudgetMeter trip produces consistent (kind, used >= limit) with no torn reads.
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(200),
        max_memory_bytes: Some(1024),
        max_page_reads: Some(50),
    }));
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let m = Arc::clone(&meter);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let res = match t % 3 {
                        0 => m.charge_steps(10),
                        1 => m.charge_page_reads(5),
                        _ => m.reserve_memory(128),
                    };
                    if let Err(e) = res {
                        assert!(matches!(
                            e.kind,
                            LimitKind::Steps | LimitKind::Memory | LimitKind::PageReads
                        ));
                        assert!(e.used > e.limit, "torn trip read: {e:?}");
                        return e.kind;
                    }
                }
                panic!("expected meter to trip");
            })
        })
        .collect();
    let kinds: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(kinds.iter().all(|k| *k == kinds[0]), "divergent trip kinds: {kinds:?}");

    // 5. CollationRegistry stack lookup handles both short (<64B) and long (>64B) custom names.
    let mut reg = CollationRegistry::new(TextEncoding::Utf8);
    let long_name = "c".repeat(80);
    reg.register("my_short", Arc::new(|a, b| b.cmp(a)));
    reg.register(long_name.clone(), Arc::new(|a, b| b.cmp(a)));
    assert_eq!(reg.find("MY_SHORT").unwrap().compare_utf8(b"a", b"b"), Ordering::Greater);
    assert_eq!(
        reg.find(&long_name.to_ascii_uppercase())
            .unwrap()
            .compare_utf8(b"a", b"b"),
        Ordering::Greater
    );
}

#[test]
fn pass6_utf16_strict_weak_order_nan_compare_and_const_fp() {
    use xdb_core::value::{compare_op, CompareOp};

    // 1. UTF-16 NOCASE & RTRIM obey strict weak ordering (no cycle!) even on invalid UTF-8
    //    bytes (`\x80`) mixed with WTF-8 unpaired surrogates (`\xed\xa0\x80`) and U+FFFD.
    let samples: [&[u8]; 5] = [
        b"\x80",
        b"\xed\xa0\x80",
        b"\xef\xbf\xbd",
        b"A ",
        b"a",
    ];
    for enc in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
        for kind in [BuiltinCollation::Nocase, BuiltinCollation::Rtrim] {
            let coll = Collation::builtin(kind, enc);
            for &a in &samples {
                assert_eq!(coll.compare_utf8(a, a), Ordering::Equal);
                for &b in &samples {
                    let ab = coll.compare_utf8(a, b);
                    let ba = coll.compare_utf8(b, a);
                    assert_eq!(ab, ba.reverse(), "antisymmetry violated for {kind:?} {enc:?}");
                    for &c in &samples {
                        let bc = coll.compare_utf8(b, c);
                        let ac = coll.compare_utf8(a, c);
                        if ab == Ordering::Less && bc == Ordering::Less {
                            assert_eq!(ac, Ordering::Less, "transitivity(<) violated for {kind:?} {enc:?}");
                        }
                        if ab == Ordering::Equal && bc == Ordering::Equal {
                            assert_eq!(ac, Ordering::Equal, "transitivity(=) violated for {kind:?} {enc:?}");
                        }
                    }
                }
            }
        }
    }

    // 2. `compare_op` treats raw non-canonical `SqlValue::Real(f64::NAN)` identically to `NULL`.
    let nan = SqlValue::Real(f64::NAN);
    assert_eq!(compare_op(CompareOp::Eq, nan.clone(), nan.clone(), None, None), None);
    assert_eq!(compare_op(CompareOp::Ne, nan.clone(), SqlValue::Int(1), None, None), None);
    assert_eq!(compare_op(CompareOp::Lt, nan.clone(), SqlValue::Int(1), None, None), None);
    assert_eq!(compare_op(CompareOp::Is, nan.clone(), SqlValue::Null, None, None), Some(true));
    assert_eq!(compare_op(CompareOp::Is, SqlValue::Null, nan.clone(), None, None), Some(true));
    assert_eq!(compare_op(CompareOp::IsNot, nan.clone(), SqlValue::Null, None, None), Some(false));
    assert_eq!(compare_op(CompareOp::Is, nan.clone(), SqlValue::Int(0), None, None), Some(false));

    // 3. `compare_bytes_nocase_utf8` equal-byte fast path preserves embedded NUL semantics.
    assert_eq!(
        Collation::NOCASE_UTF8.compare_utf8(b"abc\0z", b"ABC\0a"),
        Ordering::Equal
    );
    assert_eq!(
        Collation::NOCASE_UTF8.compare_utf8(b"abc\0", b"ABC\0x"),
        Ordering::Less
    );

    // 4. Compile-time `POWERS` table & 2-digit `fp_decode_stack` formatting exact parity.
    let mut buf = [0u8; 48];
    for x in [0.0, -0.0, 1.0, -1.0, 1e-308, 1e-22, 1e-1, 123456.789, 1e22, 1e23, 1e308] {
        assert_eq!(real_to_text_buf(x, DEFAULT_FP_DIGITS, &mut buf), real_to_text(x, DEFAULT_FP_DIGITS));
    }
}

#[test]
fn pass7_memory_vfs_growth_nan_helpers_swar_nocase_and_int64_buf() {
    use xdb_core::value::int64::int64_to_text_buf;
    use xdb_core::value::utf::{utf16_to_wtf8, utf8_to_utf16_bytes};
    use xdb_core::value::{
        apply_affinity_raw, boolean_value, cast_value, compare_bytes_nocase_utf8, int64_to_text,
    };
    use xdb_core::{BudgetMeter, MemoryVfsOptions, Meter, ResourceLimits};

    // 1. MemoryVfs growth is amortized O(N) (not O(N^2) from try_reserve_exact): 512 x 4 KiB = 2 MiB.
    let vfs = MemoryVfs::new(MemoryVfsOptions::default());
    let mut f = vfs
        .open("grow.db", OpenFlags::create(FileKind::MainDb))
        .unwrap();
    let page = vec![0x5Au8; 4096];
    for p in 0..512u64 {
        f.write(&page, p * 4096).unwrap();
    }
    assert_eq!(f.size().unwrap(), 512 * 4096);
    let mut check = [0u8; 4096];
    assert_eq!(f.read(&mut check, 511 * 4096).unwrap(), 4096);
    assert_eq!(check, [0x5Au8; 4096]);
    f.close().unwrap();

    // 2. Raw non-canonical `SqlValue::Real(f64::NAN)` treated identically to NULL in boolean/cast/affinity.
    let nan = SqlValue::Real(f64::NAN);
    assert!(!boolean_value(&nan, false));
    assert!(boolean_value(&nan, true));
    for aff in [
        Affinity::None,
        Affinity::Blob,
        Affinity::Numeric,
        Affinity::Integer,
        Affinity::Real,
        Affinity::Text,
    ] {
        assert!(cast_value(nan.clone(), aff, TextEncoding::Utf8).unwrap().is_null());
        assert!(apply_affinity_raw(nan.clone(), aff).is_null());
    }

    // 3. `BudgetMeter::memory_bytes()` matches `usage().memory_bytes` across reserve/release.
    let meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(1000),
        max_memory_bytes: Some(8192),
        max_page_reads: None,
    });
    assert_eq!(meter.memory_bytes(), 0);
    meter.reserve_memory(1234).unwrap();
    assert_eq!(meter.memory_bytes(), 1234);
    assert_eq!(meter.memory_bytes(), meter.usage().memory_bytes);
    meter.release_memory(1234);
    assert_eq!(meter.memory_bytes(), 0);

    // 4. SWAR 8-byte `compare_bytes_nocase_utf8` preserves case folding & embedded NUL semantics.
    assert_eq!(
        compare_bytes_nocase_utf8(b"0123456789abcdefXYZ", b"0123456789abcdefxyz"),
        Ordering::Equal
    );
    assert_eq!(
        compare_bytes_nocase_utf8(b"0123456789abcdefA", b"0123456789abcdefb"),
        Ordering::Less
    );
    assert_eq!(
        compare_bytes_nocase_utf8(b"abcdefgh\0zzz", b"abcdefgh\0aaa"),
        Ordering::Equal
    );
    assert_eq!(
        compare_bytes_nocase_utf8(b"abc\0defghijk", b"abc\0ZZZZZZZZ"),
        Ordering::Equal
    );
    assert_eq!(
        compare_bytes_nocase_utf8(b"abcdefgh\0", b"abcdefgh\0x"),
        Ordering::Less
    );

    // 5. Stack 2-digit `int64_to_text_buf` & ASCII `utf8_to_utf16_bytes` fast path parity.
    let mut ibuf = [0u8; 24];
    for v in [
        i64::MIN,
        -1_000_000,
        -100,
        -99,
        -10,
        -9,
        -1,
        0,
        1,
        9,
        10,
        99,
        100,
        9999,
        i64::MAX,
    ] {
        let s = std::str::from_utf8(int64_to_text_buf(v, &mut ibuf)).unwrap();
        assert_eq!(s, v.to_string());
        assert_eq!(int64_to_text(v), v.to_string());
    }
    let ascii = b"Hello, SQLite 3.54.1!";
    for enc in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
        let u16b = utf8_to_utf16_bytes(ascii, enc);
        assert_eq!(u16b.len(), ascii.len() * 2);
        assert_eq!(utf16_to_wtf8(&u16b, enc), ascii);
    }
}

#[test]
fn pass8_utf8_5byte_fffd_stringify_nan_swar_mixed_case_and_zero_meter() {
    use std::cmp::Ordering;
    use xdb_core::value::utf::utf8_to_utf16_bytes;
    use xdb_core::value::{apply_comparison_affinity, compare_bytes_nocase_utf8};
    use xdb_core::{BudgetMeter, Meter, ResourceLimits};

    // 1. Invalid 5-byte and 6-byte UTF-8 sequences consume trailing continuations and emit a
    //    SINGLE U+FFFD replacement character (matching SQLite `sqlite3Utf8Read`, utf.c:130).
    let five_byte = [0xF8u8, 0x88, 0x80, 0x80, 0x80, b'A'];
    let u16_five = utf8_to_utf16_bytes(&five_byte, TextEncoding::Utf16Le);
    assert_eq!(u16_five, vec![0xFD, 0xFF, b'A', 0x00]);

    let six_byte = [0xFCu8, 0x84, 0x80, 0x80, 0x80, 0x80, b'Z'];
    let u16_six = utf8_to_utf16_bytes(&six_byte, TextEncoding::Utf16Le);
    assert_eq!(u16_six, vec![0xFD, 0xFF, b'Z', 0x00]);

    let lone_fe_ff = [0xFEu8, 0xFF, b'!'];
    let u16_fe = utf8_to_utf16_bytes(&lone_fe_ff, TextEncoding::Utf16Le);
    assert_eq!(u16_fe, vec![0xFD, 0xFF, 0xFD, 0xFF, b'!', 0x00]);

    // 2. `apply_comparison_affinity(..., Some(Affinity::Text))` turns raw `Real(NaN)` into `Null`.
    let (l, r) = apply_comparison_affinity(
        SqlValue::Real(f64::NAN),
        SqlValue::text("NaN"),
        Some(Affinity::Text),
    );
    assert!(l.is_null(), "NaN must stringify to Null, got {l:?}");
    assert!(r.exact_eq(&SqlValue::text("NaN")));

    // 3. SWAR mixed-case 8-byte fast path in `compare_bytes_nocase_utf8`.
    assert_eq!(
        compare_bytes_nocase_utf8(
            b"AbCdEfGhIjKlMnOpQrStUvWxYz012345",
            b"aBcDeFgHiJkLmNoPqRsTuVwXyZ012345"
        ),
        Ordering::Equal
    );
    assert_eq!(
        compare_bytes_nocase_utf8(
            b"AbCdEfGhIjKlMnOpQrStUvWxYa",
            b"aBcDeFgHiJkLmNoPqRsTuVwXyZ"
        ),
        Ordering::Less
    );
    // Non-ASCII bytes differing only in bit 5 (e.g. 0xC1 vs 0xE1) MUST NOT fold as ASCII!
    assert_eq!(
        compare_bytes_nocase_utf8(b"AbCdEfG\xC1", b"aBcDeFg\xE1"),
        Ordering::Less
    );
    // Punctuation differing only in bit 5 (`@` = 0x40 vs `` ` `` = 0x60) MUST NOT fold!
    assert_eq!(
        compare_bytes_nocase_utf8(b"AbCdEfG@xyz", b"aBcDeFg`xyz"),
        Ordering::Less
    );
    // Embedded NUL stops comparison after mixed-case SWAR chunks.
    assert_eq!(
        compare_bytes_nocase_utf8(b"AbCdEfGh\0zzz", b"aBcDeFgH\0aaa"),
        Ordering::Equal
    );

    // 4. `BudgetMeter` zero-delta calls short-circuit cleanly while honoring sticky exhaustion.
    let m = BudgetMeter::new(ResourceLimits {
        max_steps: Some(2),
        max_memory_bytes: Some(16),
        max_page_reads: Some(1),
    });
    assert!(m.charge_steps(0).is_ok());
    assert!(m.charge_page_reads(0).is_ok());
    assert!(m.reserve_memory(0).is_ok());
    assert_eq!(m.usage().steps, 0);
    assert!(m.charge_steps(2).is_ok());
    assert!(m.charge_steps(1).is_err());
    assert!(
        m.charge_steps(0).is_err(),
        "sticky exhaustion must persist even on zero-delta calls"
    );
}

#[test]
fn pass9_swar_nocase_branchless_atof_atoi64_and_integral_real_buf() {
    use xdb_core::value::{atof, atoi64, compare_bytes_nocase_utf8, real_to_text, real_to_text_buf};

    // 1. Exhaustive ASCII & non-ASCII bit-5 pairs in branchless 8-byte SWAR NOCASE!
    for b in 0u8..=255 {
        let flipped = b ^ 0x20;
        let a = [b'a', b'B', b'c', b'D', b'e', b'F', b'g', b, b'x'];
        let c = [b'A', b'b', b'C', b'd', b'E', b'f', b'G', flipped, b'x'];
        let expected = if b.is_ascii_alphabetic() {
            Ordering::Equal
        } else if b == 0 {
            Ordering::Less
        } else if flipped == 0 {
            Ordering::Greater
        } else {
            b.to_ascii_lowercase().cmp(&flipped.to_ascii_lowercase())
        };
        assert_eq!(
            compare_bytes_nocase_utf8(&a, &c),
            expected,
            "SWAR nocase mismatch for byte 0x{b:02X} vs 0x{flipped:02X}"
        );
    }

    // 2. `atoi64` single-pass <= 18 digits and 19-digit boundary cases.
    let r = atoi64(b"999999999999999999");
    assert_eq!((r.value, r.rc), (999_999_999_999_999_999, 0));
    let r = atoi64(b"-999999999999999999");
    assert_eq!((r.value, r.rc), (-999_999_999_999_999_999, 0));
    let r = atoi64(b"9223372036854775807");
    assert_eq!((r.value, r.rc), (i64::MAX, 0));
    let r = atoi64(b"-9223372036854775808");
    assert_eq!((r.value, r.rc), (i64::MIN, 0));
    assert_eq!(atoi64(b"9223372036854775808").rc, 3);
    assert_eq!(atoi64(b"9223372036854775809").rc, 2);
    let r = atoi64(b"  +00042  ");
    assert_eq!((r.value, r.rc), (42, 0));
    let r = atoi64(b"123abc");
    assert_eq!((r.value, r.rc), (123, 1));

    // 3. `atof` direct slice digit loop parity.
    let f = atof(b"0.0");
    assert_eq!(f.value, 0.0);
    assert!(f.rc > 0);
    let f = atof(b"-12345.6789e+2");
    assert!((f.value - (-1234567.89)).abs() < 1e-9);
    assert!(f.rc > 0);
    let f = atof(b"  +42.5xyz");
    assert_eq!(f.value, 42.5);
    assert!(f.rc < 0);

    // 4. `real_to_text_buf` exact integral fast path matches `real_to_text` across boundaries.
    for r in [
        0.0f64,
        -0.0,
        1.0,
        -1.0,
        42.0,
        -9876543210.0,
        999_999_999_999_999.0,
        -999_999_999_999_999.0,
        1e15,
        -1e15,
        12.34567,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ] {
        for prec in [15, 16, 6] {
            let mut buf = [0u8; 48];
            let got = real_to_text_buf(r, prec, &mut buf);
            let expected = real_to_text(r, prec);
            assert_eq!(got, expected, "real_to_text_buf mismatch for {r} prec={prec}");
        }
    }
}
