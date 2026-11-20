//! Corruption & fuzz-hardening test against all 8 upstream SQLite 3.54.1
//! `fuzzdata1.db`..`fuzzdata8.db` corpora (11,210 malformed database images).
//!
//! Dogfoods our own Rust `open_database` to read `fuzzdata*.db`, decodes both
//! `db.dbcontent` blobs and `xsql` dbsqlfuzz hex-encoded images (matching
//! `test/fuzzcheck.c`), opens every single image via `open_database_from_image`
//! under a `BudgetMeter`, and walks every table and index — asserting ZERO
//! panics and ZERO hangs across the entire corpus.

use std::path::PathBuf;
use std::sync::Arc;

use xdb_core::{BudgetMeter, ResourceLimits, SqlValue};
use xdb_sqlite::{
    open_database, open_database_from_image, ColumnHiddenKind, ColumnSpec, OpenOptions,
    ScanOptions, SqliteError,
};
use xdb_vfs_fs::StdFsVfs;

const MX_FILE_SZ: usize = 10_000_000;

fn fuzz_dir() -> PathBuf {
    PathBuf::from(r"E:\repos\xdb\xdb\reference\sqlite-version-3.54.1\test")
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn is_db_sql(a: &[u8]) -> bool {
    if a.len() > 4 && a[0] == b'\n' && a[1] == b'-' && a[2] == b'-' && a[3] == b'\n' {
        return true;
    }
    let mut p = 0usize;
    while p < a.len() && (a[p] == b' ' || (0x09..=0x0d).contains(&a[p])) {
        p += 1;
    }
    let mut buf = [0u8; 8];
    let mut len = 0usize;
    while p < a.len() && len < 8 {
        if a[p].is_ascii_hexdigit() {
            buf[len] = a[p];
            len += 1;
        }
        p += 1;
    }
    &buf[..len] == b"53514c69"
}

fn decode_database(z_in: &[u8]) -> Option<Vec<u8>> {
    let n = z_in.len();
    if n < 4 {
        return None;
    }
    let mut a = vec![0u8; 4096];
    let mut mx = 0usize;
    let mut k = 0u32;
    let mut b = 0u8;
    let mut i = 0usize;
    while i < n {
        let c = z_in[i];
        if c.is_ascii_hexdigit() {
            k = k.wrapping_add(1);
            if (k & 1) != 0 {
                b = hex_val(c) * 16;
            } else {
                b = b.wrapping_add(hex_val(c));
                let j = ((k >> 1) as usize).wrapping_sub(1);
                if j >= MX_FILE_SZ {
                    return None;
                }
                if j >= a.len() {
                    let mut size = a.len() * 2;
                    while size <= j {
                        size = size.saturating_mul(2);
                    }
                    a.resize(size.min(MX_FILE_SZ), 0);
                }
                if j >= mx {
                    mx = (j + 4095) & !4095;
                    if mx > MX_FILE_SZ {
                        mx = MX_FILE_SZ;
                    }
                }
                a[j] = b;
            }
        } else if c == b'[' && i + 3 < n {
            let mut q = 1usize;
            let mut kk = 0u32;
            let mut ok = true;
            while i + q < n && z_in[i + q] != b']' {
                let d = z_in[i + q];
                if !d.is_ascii_hexdigit() {
                    ok = false;
                    break;
                }
                kk = kk.wrapping_mul(16).wrapping_add(u32::from(hex_val(d)));
                q += 1;
            }
            if ok && i + q < n {
                k = kk.wrapping_mul(2);
                i += q + 1;
                continue;
            }
        } else if c == b'\n'
            && i + 4 < n
            && z_in[i + 1] == b'-'
            && z_in[i + 2] == b'-'
            && z_in[i + 3] == b'\n'
        {
            break;
        }
        i += 1;
    }
    a.truncate(mx);
    Some(a)
}

fn value_to_bytes(v: SqlValue) -> Vec<u8> {
    match v {
        SqlValue::Blob(b) | SqlValue::Text(b) => b.into_vec(),
        _ => Vec::new(),
    }
}

fn exercise_image(bytes: Vec<u8>) -> Result<(), SqliteError> {
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(200_000),
        max_memory_bytes: Some(32 * 1024 * 1024),
        max_page_reads: Some(5_000),
    }));
    let db = open_database_from_image(
        bytes,
        OpenOptions {
            meter: Some(meter),
            ..OpenOptions::default()
        },
    )?;

    // Walk every table (stored columns only) and every index.
    for t in db.tables() {
        let reader = db.table(&t.name)?;
        let stored: Vec<ColumnSpec> = reader
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| ColumnSpec::Cid(c.cid))
            .collect();
        let opts = ScanOptions {
            columns: Some(stored),
            rowid: !reader.without_rowid(),
        };
        if let Ok(mut cur) = reader.cursor(Some(&opts)) {
            while let Ok(true) = cur.next() {
                if cur.row().is_err() {
                    break;
                }
            }
        }
    }
    for ix_info in db.indexes() {
        if let Ok(ix) = db.index(&ix_info.name) {
            if let Ok(mut cur) = ix.cursor() {
                while let Ok(true) = cur.next() {
                    if cur.row().is_err() {
                        break;
                    }
                }
            }
        }
    }
    let _ = db.btree().freelist();
    Ok(())
}

#[test]
fn fuzzdata_all_11210_malformed_images_never_panic_or_hang() {
    let dir = fuzz_dir();
    if !dir.join("fuzzdata1.db").exists() {
        eprintln!("skipping fuzzdata test: {} not found", dir.display());
        return;
    }

    let vfs = StdFsVfs::new();
    let mut total_images = 0usize;
    let mut opened_ok = 0usize;
    let mut rejected_corrupt = 0usize;
    let mut rejected_notadb = 0usize;
    let mut rejected_format = 0usize;
    let mut rejected_limit = 0usize;

    for idx in 1..=8 {
        let path = dir.join(format!("fuzzdata{idx}.db"));
        let db = open_database(&vfs, path.to_str().unwrap(), OpenOptions::default())
            .unwrap_or_else(|e| panic!("failed to open {}: {e}", path.display()));

        // Read `db.dbcontent`
        if let Ok(tbl) = db.table("db") {
            let opts = ScanOptions {
                columns: Some(vec![ColumnSpec::from("dbcontent")]),
                rowid: false,
            };
            let mut cur = tbl.cursor(Some(&opts)).unwrap();
            while cur.next().unwrap() {
                let bytes = value_to_bytes(cur.value(0).unwrap());
                total_images += 1;
                match exercise_image(bytes) {
                    Ok(()) => opened_ok += 1,
                    Err(e) if e.is_corrupt() => rejected_corrupt += 1,
                    Err(e) if e.is_not_a_db() => rejected_notadb += 1,
                    Err(SqliteError::SchemaFormat(_)) => rejected_format += 1,
                    Err(SqliteError::ResourceLimit(_)) => rejected_limit += 1,
                    Err(other) => panic!("fuzzdata{idx} db unexpected error: {other}"),
                }
            }
        }

        // Read `xsql.sqltext` dbsqlfuzz images
        if let Ok(tbl) = db.table("xsql") {
            let opts = ScanOptions {
                columns: Some(vec![ColumnSpec::from("sqltext")]),
                rowid: false,
            };
            let mut cur = tbl.cursor(Some(&opts)).unwrap();
            while cur.next().unwrap() {
                let text = value_to_bytes(cur.value(0).unwrap());
                if !is_db_sql(&text) || text.len() < 10 {
                    continue;
                }
                let Some(mut img) = decode_database(&text) else {
                    continue;
                };
                if img.len() >= 20 && img[18] == 2 && img[19] == 2 {
                    img[18] = 1;
                    img[19] = 1;
                }
                total_images += 1;
                match exercise_image(img) {
                    Ok(()) => opened_ok += 1,
                    Err(e) if e.is_corrupt() => rejected_corrupt += 1,
                    Err(e) if e.is_not_a_db() => rejected_notadb += 1,
                    Err(SqliteError::SchemaFormat(_)) => rejected_format += 1,
                    Err(SqliteError::ResourceLimit(_)) => rejected_limit += 1,
                    Err(other) => panic!("fuzzdata{idx} xsql unexpected error: {other}"),
                }
            }
        }
    }

    eprintln!(
        "fuzzdata1..8 PASSED: {total_images} images tested -> {opened_ok} opened+walked, \
         {rejected_corrupt} corrupt, {rejected_notadb} notadb, {rejected_format} format, \
         {rejected_limit} meter-halted (0 panics, 0 hangs)"
    );
    assert!(
        total_images >= 11_000,
        "expected >= 11,000 fuzz images, got {total_images}"
    );
}
