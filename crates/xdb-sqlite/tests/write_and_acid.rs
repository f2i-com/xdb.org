//! Phase 3 Stage S2 integration tests:
//! - Full DDL (`CREATE TABLE/INDEX/VIEW`, `DROP`, `ALTER TABLE`)
//! - Full DML (`INSERT`, `UPSERT`, `UPDATE`, `DELETE`, `RETURNING`, `STRICT`, `WITHOUT ROWID`,
//!   `AUTOINCREMENT`, `GENERATED VIRTUAL/STORED`, `NOT NULL`, `UNIQUE`, `CHECK`)
//! - Multi-level B-tree splits, overflow page chains, mass deletion & freelist reuse
//! - Transactions & nested savepoints (`BEGIN`, `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`, `COMMIT`, `ROLLBACK`)
//! - Fault-injecting VFS crash recovery verified by BOTH `xdb-sqlite` Stage S1 hot-journal overlay
//!   AND real C SQLite (`node:sqlite` `PRAGMA integrity_check`)!

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use xdb_core::vfs::{
    FileKind, IoError, IoErrorCode, IoResult, LockLevel, MemoryVfs, MemoryVfsOptions, OpenFlags,
    Vfs, VfsFile,
};
use xdb_core::{BudgetMeter, LimitKind, ResourceLimits, SqlValue};
use xdb_sqlite::{
    open_database, open_database_from_bytes, ConnectionOptions, OpenOptions, QueryParams,
    SqliteConnection, SqliteError,
};

fn assert_integrity_ok(conn: &mut SqliteConnection) {
    let res = conn.execute("PRAGMA integrity_check").expect("integrity_check");
    assert_eq!(res.rows.len(), 1, "integrity errors: {:?}", res.rows);
    assert!(
        res.rows[0][0].exact_eq(&SqlValue::text("ok")),
        "expected integrity_check = ok, got {:?}",
        res.rows
    );
}

/// Ask real C SQLite (via Node 24's built-in `node:sqlite`) to open `db_bytes` (plus optional
/// `journal_bytes` for hot-journal crash recovery), run `PRAGMA integrity_check`, and execute
/// an optional verification SQL query returning JSON.
fn verify_with_real_sqlite(
    tag: &str,
    db_bytes: &[u8],
    journal_bytes: Option<&[u8]>,
    check_sql: Option<&str>,
) -> Option<String> {
    let dir = std::env::temp_dir().join(format!("xdb_rs_p3_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let db_path = dir.join("test.db");
    std::fs::write(&db_path, db_bytes).expect("write test.db");
    if let Some(jb) = journal_bytes {
        std::fs::write(dir.join("test.db-journal"), jb).expect("write test.db-journal");
    }

    let script = r#"
const { DatabaseSync } = require('node:sqlite');
const db = new DatabaseSync(process.argv[1]);
const ic = db.prepare('PRAGMA integrity_check').all();
if (ic.length !== 1 || ic[0].integrity_check !== 'ok') {
  console.error('SQLite integrity_check FAILED:', ic);
  process.exit(2);
}
if (process.argv[2]) {
  const rows = db.prepare(process.argv[2]).all();
  process.stdout.write(JSON.stringify(rows));
}
db.close();
"#;
    let mut cmd = Command::new("node");
    cmd.arg("-e").arg(script).arg(&db_path);
    if let Some(sql) = check_sql {
        cmd.arg(sql);
    }
    let output = cmd.output();
    let _ = std::fs::remove_dir_all(&dir);

    let out = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping node:sqlite check ({e})");
            return None;
        }
    };
    assert!(
        out.status.success(),
        "real C SQLite verification FAILED for {tag}:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn ddl_dml_constraints_upsert_returning_and_alter() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. CREATE TABLE with IPK AUTOINCREMENT, UNIQUE, NOT NULL, CHECK, DEFAULT, STORED & VIRTUAL generated cols
    conn.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE COLLATE NOCASE,
            name TEXT NOT NULL DEFAULT 'anon',
            score INT CHECK (score >= 0),
            doubled INT GENERATED ALWAYS AS (score * 2) VIRTUAL,
            tag TEXT GENERATED ALWAYS AS (lower(name) || ':' || score) STORED
        );
        CREATE INDEX idx_users_score ON users(score DESC, name);
        CREATE INDEX idx_users_hi ON users(email) WHERE score >= 50;
        CREATE VIEW top_users AS SELECT id, name, score, doubled, tag FROM users WHERE score >= 50;",
    )
    .unwrap();

    // 2. INSERT + RETURNING + last_insert_rowid() + changes()
    let ins = conn
        .execute(
            "INSERT INTO users(email, name, score) VALUES
                ('Alice@Example.com', 'Alice', 80),
                ('bob@example.com', 'Bob', 30)
             RETURNING id, email, doubled, tag",
        )
        .unwrap();
    assert_eq!(ins.changes, 2);
    assert_eq!(ins.last_insert_rowid, 2);
    assert_eq!(ins.rows.len(), 2);
    assert!(ins.rows[0][2].exact_eq(&SqlValue::Int(160)));
    assert!(ins.rows[0][3].exact_eq(&SqlValue::text("alice:80")));

    let counters = conn
        .query("SELECT last_insert_rowid(), changes(), total_changes()")
        .unwrap();
    assert!(counters.rows[0][0].exact_eq(&SqlValue::Int(2)));
    assert!(counters.rows[0][1].exact_eq(&SqlValue::Int(2)));
    assert!(counters.rows[0][2].exact_eq(&SqlValue::Int(2)));

    // 3. Constraint checks: NOT NULL, CHECK, NOCASE UNIQUE
    assert!(conn
        .execute("INSERT INTO users(email, name, score) VALUES (NULL, 'x', 1)")
        .is_err());
    assert!(conn
        .execute("INSERT INTO users(email, name, score) VALUES ('c@x.com', 'c', -5)")
        .is_err());
    assert!(conn
        .execute("INSERT INTO users(email, name, score) VALUES ('alice@example.com', 'Dup', 10)")
        .is_err());

    // 4. UPSERT: ON CONFLICT(email) DO UPDATE SET ... using excluded.*
    let ups = conn
        .execute(
            "INSERT INTO users(email, name, score) VALUES ('ALICE@EXAMPLE.COM', 'Alice2', 95)
             ON CONFLICT(email) DO UPDATE SET name = excluded.name, score = users.score + 5
             RETURNING id, name, score, doubled, tag",
        )
        .unwrap();
    assert_eq!(ups.rows.len(), 1);
    assert!(ups.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(ups.rows[0][1].exact_eq(&SqlValue::text("Alice2")));
    assert!(ups.rows[0][2].exact_eq(&SqlValue::Int(85)));
    assert!(ups.rows[0][3].exact_eq(&SqlValue::Int(170)));
    assert!(ups.rows[0][4].exact_eq(&SqlValue::text("alice2:85")));

    // 5. INSERT OR IGNORE & INSERT OR REPLACE
    conn.execute("INSERT OR IGNORE INTO users(email, name, score) VALUES ('bob@example.com', 'Bob2', 99)")
        .unwrap();
    let bob = conn.query("SELECT name, score FROM users WHERE email = 'bob@example.com'").unwrap();
    assert!(bob.rows[0][0].exact_eq(&SqlValue::text("Bob")));

    // 6. UPDATE + DELETE + RETURNING
    let upd = conn
        .execute("UPDATE users SET score = score + 25 WHERE name = 'Bob' RETURNING id, score, doubled")
        .unwrap();
    assert_eq!(upd.rows.len(), 1);
    assert!(upd.rows[0][1].exact_eq(&SqlValue::Int(55)));
    assert!(upd.rows[0][2].exact_eq(&SqlValue::Int(110)));

    // Both Alice2 (85) and Bob (55) now appear in view top_users!
    let top = conn.query("SELECT name, score FROM top_users ORDER BY score DESC").unwrap();
    assert_eq!(top.rows.len(), 2);

    // 7. STRICT table & WITHOUT ROWID table
    conn.execute(
        "CREATE TABLE strict_t (k INT PRIMARY KEY, v TEXT NOT NULL) STRICT;
         CREATE TABLE kv (k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID;",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO strict_t VALUES ('not-an-int', 'ok')")
        .is_err());
    conn.execute("INSERT INTO strict_t VALUES (1, 'hello')").unwrap();
    conn.execute("INSERT INTO kv VALUES ('b', x'02'), ('a', x'01'), ('c', x'03')")
        .unwrap();
    let kv_rows = conn.query("SELECT k, hex(v) FROM kv ORDER BY k").unwrap();
    assert_eq!(kv_rows.rows.len(), 3);
    assert!(kv_rows.rows[0][0].exact_eq(&SqlValue::text("a")));

    // 8. ALTER TABLE ADD COLUMN, RENAME COLUMN, DROP COLUMN, RENAME TO
    conn.execute("ALTER TABLE strict_t ADD COLUMN note TEXT DEFAULT 'n/a'")
        .unwrap();
    let st1 = conn.query("SELECT k, v, note FROM strict_t").unwrap();
    assert!(st1.rows[0][2].exact_eq(&SqlValue::text("n/a")));
    conn.execute("ALTER TABLE strict_t RENAME COLUMN note TO remark")
        .unwrap();
    conn.execute("ALTER TABLE strict_t DROP COLUMN remark").unwrap();
    conn.execute("ALTER TABLE strict_t RENAME TO strict_renamed")
        .unwrap();
    let st2 = conn.query("SELECT k, v FROM strict_renamed").unwrap();
    assert_eq!(st2.rows.len(), 1);

    // 9. CREATE TABLE ... AS SELECT
    conn.execute("CREATE TABLE summary AS SELECT count(*) AS n, sum(score) AS total FROM users")
        .unwrap();
    let sum = conn.query("SELECT n, total FROM summary").unwrap();
    assert!(sum.rows[0][0].exact_eq(&SqlValue::Int(2)));
    assert!(sum.rows[0][1].exact_eq(&SqlValue::Int(140)));

    // 10. PRAGMA integrity_check in Rust AND real C SQLite!
    assert_integrity_ok(&mut conn);
    let bytes = conn.to_bytes();
    let out = verify_with_real_sqlite(
        "ddl_dml",
        &bytes,
        None,
        Some("SELECT id, name, score, doubled, tag FROM top_users ORDER BY id"),
    );
    if let Some(json) = out {
        assert!(json.contains("Alice2") && json.contains("170") && json.contains("alice2:85"));
    }
}

#[test]
fn multilevel_btree_splits_overflows_deletes_and_real_sqlite_integrity() {
    // Use 512-byte pages to force deep 3-level Table & Index B-trees and multi-page overflow chains!
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512,
        ..ConnectionOptions::default()
    })
    .unwrap();

    conn.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, cat TEXT, val INT, payload BLOB);
         CREATE INDEX idx_items_cat_val ON items(cat, val DESC);
         CREATE TABLE wr_items (k TEXT PRIMARY KEY, payload TEXT) WITHOUT ROWID;",
    )
    .unwrap();

    // Insert 400 rows including large overflow payloads (up to ~3 KB = 6+ overflow pages each!)
    conn.execute("BEGIN").unwrap();
    for i in 1..=400i64 {
        let cat = format!("cat_{:02}", i % 17);
        let blob_len = if i % 25 == 0 { 2600 } else { 20 };
        let blob = vec![(i % 251) as u8; blob_len];
        let params = QueryParams::positional(vec![
            SqlValue::Int(i),
            SqlValue::text(&cat),
            SqlValue::Int(i * 7 % 1000),
            SqlValue::blob(blob),
        ]);
        conn.execute_with_params("INSERT INTO items VALUES (?, ?, ?, ?)", &params)
            .unwrap();

        let wr_params = QueryParams::positional(vec![
            SqlValue::text(format!("key_{:04}", (i * 137) % 10000)),
            SqlValue::text(if i % 30 == 0 {
                "X".repeat(1500)
            } else {
                format!("val_{i}")
            }),
        ]);
        conn.execute_with_params("INSERT OR REPLACE INTO wr_items VALUES (?, ?)", &wr_params)
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    assert_integrity_ok(&mut conn);
    assert!(conn.page_count() > 40, "expected multi-level B-tree with >40 pages");

    // Delete 75% of rows (including interior index keys and emptying leaves!)
    conn.execute("BEGIN").unwrap();
    conn.execute("DELETE FROM items WHERE id % 4 != 0").unwrap();
    conn.execute("DELETE FROM wr_items WHERE k < 'key_5000'").unwrap();
    conn.execute("COMMIT").unwrap();
    assert_integrity_ok(&mut conn);

    // Check freelist has reclaimed freed pages, then insert 100 new rows reusing the freelist!
    let fl_before = conn
        .execute("PRAGMA freelist_count")
        .unwrap()
        .rows[0][0]
        .as_i64()
        .unwrap();
    assert!(fl_before > 10, "expected reclaimed freelist pages, got {fl_before}");

    conn.execute("BEGIN").unwrap();
    for i in 1001..=1100i64 {
        let params = QueryParams::positional(vec![
            SqlValue::Int(i),
            SqlValue::text(format!("reused_{}", i % 5)),
            SqlValue::Int(i),
            SqlValue::blob(vec![0xAB; if i % 10 == 0 { 1200 } else { 16 }]),
        ]);
        conn.execute_with_params("INSERT INTO items VALUES (?, ?, ?, ?)", &params)
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    assert_integrity_ok(&mut conn);

    // Verify exact row count, index lookups, and overflow payload integrity with BOTH Rust AND real C SQLite!
    let expected_cnt = conn
        .query("SELECT count(*), sum(length(payload)) FROM items")
        .unwrap();
    assert!(expected_cnt.rows[0][0].exact_eq(&SqlValue::Int(200)));
    let expected_blob_sum = expected_cnt.rows[0][1].as_i64().unwrap();

    // Also test DROP TABLE reclaims all pages cleanly
    conn.execute("CREATE TABLE tmp_drop (x INT, y BLOB); INSERT INTO tmp_drop VALUES (1, zeroblob(2000)); DROP TABLE tmp_drop;")
        .unwrap();
    assert_integrity_ok(&mut conn);

    let bytes = conn.to_bytes();
    // Re-open with read-only SqliteDatabase to verify our reader reads 100% cleanly!
    let ro = open_database_from_bytes(&bytes, None, None, OpenOptions::default()).unwrap();
    let ro_res = ro
        .query("SELECT count(*), sum(length(payload)) FROM items")
        .unwrap();
    assert!(ro_res.rows[0][0].exact_eq(&SqlValue::Int(200)));
    assert!(ro_res.rows[0][1].exact_eq(&SqlValue::Int(expected_blob_sum)));

    // Verify with REAL C SQLite `PRAGMA integrity_check`!
    let c_out = verify_with_real_sqlite(
        "btree_splits",
        &bytes,
        None,
        Some("SELECT count(*) AS cnt, sum(length(payload)) AS total_bytes FROM items"),
    );
    if let Some(json) = c_out {
        let expected_sub = format!("\"cnt\":200,\"total_bytes\":{expected_blob_sum}");
        assert!(json.contains(&expected_sub), "C SQLite mismatch: {json}");
    }
}

#[test]
fn transactions_nested_savepoints_and_statement_abort() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'init')").unwrap();

    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'tx')").unwrap();
    conn.execute("SAVEPOINT sp1").unwrap();
    conn.execute("INSERT INTO t VALUES (3, 'sp1')").unwrap();
    conn.execute("SAVEPOINT sp2").unwrap();
    conn.execute("INSERT INTO t VALUES (4, 'sp2')").unwrap();

    // Statement-level ABORT must only undo the failing statement (not sp2!)
    assert!(conn
        .execute("INSERT INTO t VALUES (5, 'ok'), (1, 'dup')")
        .is_err());
    let cnt_now = conn.query("SELECT count(*) FROM t").unwrap();
    assert!(cnt_now.rows[0][0].exact_eq(&SqlValue::Int(4)));

    // Rollback to sp2 -> removes row 4, keeps rows 1,2,3
    conn.execute("ROLLBACK TO sp2").unwrap();
    assert!(conn.query("SELECT count(*) FROM t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(3)));

    // Release sp1 and commit -> rows 1,2,3 persist
    conn.execute("RELEASE sp1").unwrap();
    conn.execute("COMMIT").unwrap();
    assert!(conn.query("SELECT count(*) FROM t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(3)));

    // Full ROLLBACK reverts uncommitted writes
    conn.execute("BEGIN").unwrap();
    conn.execute("DELETE FROM t").unwrap();
    assert!(conn.query("SELECT count(*) FROM t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(0)));
    conn.execute("ROLLBACK").unwrap();
    assert!(conn.query("SELECT count(*) FROM t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(3)));
    assert_integrity_ok(&mut conn);
}

// ───────────────────────────── Fault-Injecting VFS Crash Test ─────────────────────────────

#[derive(Debug)]
struct FaultVfs {
    inner: Arc<MemoryVfs>,
    fail_main_writes_after: Arc<AtomicUsize>,
    main_writes_seen: Arc<AtomicUsize>,
    armed: Arc<AtomicBool>,
}

impl FaultVfs {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryVfs::new(MemoryVfsOptions::default())),
            fail_main_writes_after: Arc::new(AtomicUsize::new(usize::MAX)),
            main_writes_seen: Arc::new(AtomicUsize::new(0)),
            armed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn arm_crash_after_main_writes(&self, n: usize) {
        self.main_writes_seen.store(0, Ordering::SeqCst);
        self.fail_main_writes_after.store(n, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct FaultFile {
    inner: Box<dyn VfsFile>,
    is_main: bool,
    fail_after: Arc<AtomicUsize>,
    seen: Arc<AtomicUsize>,
    armed: Arc<AtomicBool>,
}

impl VfsFile for FaultFile {
    fn path(&self) -> &str {
        self.inner.path()
    }
    fn is_readonly(&self) -> bool {
        self.inner.is_readonly()
    }
    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<usize> {
        self.inner.read(buf, offset)
    }
    fn write(&mut self, buf: &[u8], offset: u64) -> IoResult<()> {
        if self.is_main && self.armed.load(Ordering::SeqCst) {
            let k = self.seen.fetch_add(1, Ordering::SeqCst);
            if k >= self.fail_after.load(Ordering::SeqCst) {
                return Err(IoError::new(
                    IoErrorCode::IoErrWrite,
                    "simulated power loss mid-commit",
                ));
            }
        }
        self.inner.write(buf, offset)
    }
    fn size(&self) -> IoResult<u64> {
        self.inner.size()
    }
    fn truncate(&mut self, size: u64) -> IoResult<()> {
        self.inner.truncate(size)
    }
    fn sync(&mut self, data_only: bool) -> IoResult<()> {
        self.inner.sync(data_only)
    }
    fn lock(&mut self, level: LockLevel) -> IoResult<bool> {
        self.inner.lock(level)
    }
    fn unlock(&mut self, level: LockLevel) -> IoResult<()> {
        self.inner.unlock(level)
    }
    fn check_reserved_lock(&self) -> IoResult<bool> {
        self.inner.check_reserved_lock()
    }
    fn lock_level(&self) -> LockLevel {
        self.inner.lock_level()
    }
    fn close(&mut self) -> IoResult<()> {
        self.inner.close()
    }
}

impl Vfs for FaultVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>> {
        let f = self.inner.open(path, flags)?;
        Ok(Box::new(FaultFile {
            inner: f,
            is_main: flags.kind == FileKind::MainDb,
            fail_after: self.fail_main_writes_after.clone(),
            seen: self.main_writes_seen.clone(),
            armed: self.armed.clone(),
        }))
    }
    fn delete(&self, path: &str, sync_dir: bool) -> IoResult<()> {
        self.inner.delete(path, sync_dir)
    }
    fn exists(&self, path: &str) -> IoResult<bool> {
        self.inner.exists(path)
    }
}

#[test]
fn acid_rollback_journal_crash_recovery_verified_by_rust_and_c_sqlite() {
    let vfs = Arc::new(FaultVfs::new());
    let db_path = "/acid.db";

    // Commit Transaction 1 cleanly: 2 rows committed to VFS!
    let mut conn = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE acct (id INTEGER PRIMARY KEY, balance INT);
         INSERT INTO acct VALUES (1, 1000), (2, 500);",
    )
    .unwrap();
    assert!(!vfs.exists("/acid.db-journal").unwrap());

    // Arm simulated power loss AFTER 1 dirty main page write (mid-commit of Transaction 2)!
    vfs.arm_crash_after_main_writes(1);
    let err = conn
        .execute(
            "BEGIN;
             UPDATE acct SET balance = 0 WHERE id = 1;
             UPDATE acct SET balance = 1500 WHERE id = 2;
             CREATE TABLE extra (x INT);
             INSERT INTO extra VALUES (999);
             COMMIT;",
        )
        .expect_err("simulated crash must fail COMMIT");
    assert!(matches!(err, SqliteError::Io(_)));

    // A genuine hot rollback journal MUST be present on the VFS!
    assert!(vfs.exists("/acid.db-journal").unwrap());
    vfs.disarm();

    // 1. Recover using our own read-only Stage S1 hot-journal overlay!
    let ro = open_database(vfs.as_ref(), db_path, OpenOptions::default()).unwrap();
    assert_eq!(ro.mode(), xdb_sqlite::SnapshotMode::HotJournalOverlay);
    let r1 = ro
        .query("SELECT id, balance FROM acct ORDER BY id")
        .unwrap();
    assert_eq!(r1.rows.len(), 2);
    assert!(r1.rows[0][1].exact_eq(&SqlValue::Int(1000)));
    assert!(r1.rows[1][1].exact_eq(&SqlValue::Int(500)));
    assert!(ro.query("SELECT * FROM extra").is_err(), "uncommitted table must not exist after crash");

    // 2. Recover using REAL C SQLite (`node:sqlite`) on the exact crashed (main, -journal) bytes!
    let read_all = |path: &str| -> Vec<u8> {
        let mut f = vfs
            .inner
            .open(path, OpenFlags::readonly(FileKind::Other))
            .expect("open");
        let sz = f.size().expect("size") as usize;
        let mut buf = vec![0u8; sz];
        f.read(&mut buf, 0).expect("read");
        buf
    };
    let main_bytes = read_all(db_path);
    let journal_bytes = read_all("/acid.db-journal");
    let c_out = verify_with_real_sqlite(
        "crash_recovery",
        &main_bytes,
        Some(&journal_bytes),
        Some("SELECT id, balance FROM acct ORDER BY id"),
    );
    if let Some(json) = c_out {
        assert!(
            json.contains("\"balance\":1000") && json.contains("\"balance\":500"),
            "C SQLite hot-journal recovery mismatch: {json}"
        );
    }
}

#[test]
fn sandbox_meter_bounds_writes() {
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(250),
        max_memory_bytes: None,
        max_page_reads: None,
    }));
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(meter),
        ..ConnectionOptions::default()
    })
    .unwrap();
    conn.execute("CREATE TABLE t (x INT)").unwrap();

    let err = conn
        .execute(
            "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 1000)
             INSERT INTO t SELECT x FROM s",
        )
        .expect_err("large metered insert must halt deterministically");
    match err {
        SqliteError::ResourceLimit(r) => assert_eq!(r.kind, LimitKind::Steps),
        other => panic!("expected ResourceLimit, got {other:?}"),
    }
    // Statement rollback guarantees 0 partial rows leaked into `t`!
    // (Check via a fresh unbudgeted view of the exported bytes.)
    let bytes = conn.to_bytes();
    let ro = open_database_from_bytes(&bytes, None, None, OpenOptions::default()).unwrap();
    let cnt = ro.query("SELECT count(*) FROM t").unwrap();
    assert!(cnt.rows[0][0].exact_eq(&SqlValue::Int(0)));
}
