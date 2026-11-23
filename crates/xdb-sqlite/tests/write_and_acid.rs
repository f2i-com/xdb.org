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

#[test]
fn acid_update_only_crash_journals_page1_and_rw_reopen_recovers() {
    let vfs = Arc::new(FaultVfs::new());
    let db_path = "/acid_upd.db";

    // Commit Transaction 1: create table on page 2, leaving page 1 unchanged during data UPDATEs.
    let mut conn = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE acct (id INTEGER PRIMARY KEY, balance INT);
         INSERT INTO acct VALUES (1, 100), (2, 200);",
    )
    .unwrap();
    drop(conn);

    // Reopen and perform an UPDATE that ONLY mutates page 2 (not page 1 in B-tree),
    // crashing after page 1's header write (or page 2's write) mid-commit!
    let mut conn = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    vfs.arm_crash_after_main_writes(1);
    let err = conn
        .execute("UPDATE acct SET balance = 999 WHERE id = 1;")
        .expect_err("simulated crash must fail UPDATE commit");
    assert!(matches!(err, SqliteError::Io(_)));
    assert!(vfs.exists("/acid_upd.db-journal").unwrap());
    vfs.disarm();
    drop(conn);

    // 1. Stage S1 read-only hot-journal overlay must recover original balance = 100 AND original page 1 header!
    let ro = open_database(vfs.as_ref(), db_path, OpenOptions::default()).unwrap();
    assert_eq!(ro.mode(), xdb_sqlite::SnapshotMode::HotJournalOverlay);
    let r1 = ro.query("SELECT id, balance FROM acct ORDER BY id").unwrap();
    assert!(r1.rows[0][1].exact_eq(&SqlValue::Int(100)));
    assert!(r1.rows[1][1].exact_eq(&SqlValue::Int(200)));
    drop(ro);

    // 2. Real C SQLite must recover cleanly from the exact crashed (main, -journal) bytes!
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
    let journal_bytes = read_all("/acid_upd.db-journal");
    if let Some(json) = verify_with_real_sqlite(
        "update_only_crash_recovery",
        &main_bytes,
        Some(&journal_bytes),
        Some("SELECT id, balance FROM acct ORDER BY id"),
    ) {
        assert!(
            json.contains("\"balance\":100") && json.contains("\"balance\":200"),
            "C SQLite update-only crash recovery mismatch: {json}"
        );
    }

    // 3. Reopening the crashed VFS database in R/W mode must materialize recovery, remove the
    // stale hot journal, and allow subsequent writes cleanly!
    let mut rw = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    assert!(!vfs.exists("/acid_upd.db-journal").unwrap());
    assert_integrity_ok(&mut rw);
    let r2 = rw.execute("SELECT balance FROM acct WHERE id = 1").unwrap();
    assert!(r2.rows[0][0].exact_eq(&SqlValue::Int(100)));
    rw.execute("UPDATE acct SET balance = 150 WHERE id = 1").unwrap();
    assert_integrity_ok(&mut rw);
    let final_bytes = rw.to_bytes();
    if let Some(json) = verify_with_real_sqlite(
        "rw_post_recovery_write",
        &final_bytes,
        None,
        Some("SELECT balance FROM acct WHERE id = 1"),
    ) {
        assert!(json.contains("\"balance\":150"), "unexpected post-recovery state: {json}");
    }
}

#[test]
fn vfs_busy_lock_prevents_concurrent_commit_and_preserves_state() {
    let vfs = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    let db_path = "/busy.db";

    let mut conn = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    conn.execute("CREATE TABLE t(x INT); INSERT INTO t VALUES (1);")
        .unwrap();

    // Hold a SHARED lock via another VFS handle on the same database file.
    let mut reader = vfs
        .open(db_path, OpenFlags::readonly(FileKind::MainDb))
        .unwrap();
    assert!(reader.lock(LockLevel::Shared).unwrap());

    // Writer commit must fail with IoErrorCode::Busy rather than writing without an EXCLUSIVE lock.
    let err = conn
        .execute("INSERT INTO t VALUES (2);")
        .expect_err("commit under active SHARED lock must return Busy");
    match err {
        SqliteError::Io(io) => assert_eq!(io.code, IoErrorCode::Busy),
        other => panic!("expected Io(Busy), got {other:?}"),
    }

    reader.unlock(LockLevel::None).unwrap();
    reader.close().unwrap();

    // After reader releases lock, commits succeed cleanly and no partial row leaked.
    let r = conn.execute("SELECT count(*) FROM t").unwrap();
    assert!(r.rows[0][0].exact_eq(&SqlValue::Int(1)));
    conn.execute("INSERT INTO t VALUES (2);").unwrap();
    let r2 = conn.execute("SELECT count(*) FROM t").unwrap();
    assert!(r2.rows[0][0].exact_eq(&SqlValue::Int(2)));
    assert_integrity_ok(&mut conn);
}

#[test]
fn btree_512b_uneven_three_way_split_and_large_page1_ddl_verified_by_c_sqlite() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512,
        ..ConnectionOptions::default()
    })
    .unwrap();

    // 1. Long CREATE TABLE (> 412B) on 512-byte page 1 forces page-1 single-cell overflow split
    // (0-cell TableInterior on page 1 with right_child holding the cell).
    let long_cols = (0..18)
        .map(|i| format!("long_column_name_{i:02} TEXT DEFAULT 'val'"))
        .collect::<Vec<_>>()
        .join(", ");
    let ddl = format!("CREATE TABLE big_ddl (id INTEGER PRIMARY KEY, {long_cols})");
    assert!(ddl.len() > 420);
    conn.execute(&ddl).unwrap();
    conn.execute("INSERT INTO big_ddl(id) VALUES (1)").unwrap();

    // 2. Uneven cell sizes on a 512-byte TableLeaf page trigger a 3-way split cleanly.
    conn.execute("CREATE TABLE uneven (id INTEGER PRIMARY KEY, p BLOB)")
        .unwrap();
    // 240B + 460B + 240B cannot fit in 2x512B leaves if split naively at midpoint!
    conn.execute("INSERT INTO uneven VALUES (1, zeroblob(240)), (3, zeroblob(240))")
        .unwrap();
    conn.execute("INSERT INTO uneven VALUES (2, zeroblob(460))")
        .unwrap();
    for i in 4..40 {
        let sz = if i % 3 == 0 { 470 } else { 220 };
        conn.execute(&format!("INSERT INTO uneven VALUES ({i}, zeroblob({sz}))"))
            .unwrap();
    }
    assert_integrity_ok(&mut conn);

    let bytes = conn.to_bytes();
    if let Some(json) = verify_with_real_sqlite(
        "512b_uneven_split_and_page1_ddl",
        &bytes,
        None,
        Some("SELECT count(*) AS n, sum(length(p)) AS total FROM uneven"),
    ) {
        assert!(json.contains("\"n\":39"), "C SQLite uneven split mismatch: {json}");
    }
}

#[test]
fn ddl_alter_table_savepoint_and_dml_edge_cases() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // Multi-statement script with leading comments/whitespace + CREATE TABLE + ALTER TABLE ADD COLUMN
    // (previously panicked slicing AST spans against statement-local SQL instead of full SQL).
    conn.execute(
        "/* leading comment */
         CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
         -- second statement
         ALTER TABLE t ADD COLUMN tag TEXT DEFAULT 'def';",
    )
    .unwrap();
    conn.execute("INSERT INTO t(name) VALUES ('a'), ('b')").unwrap();

    // RENAME TABLE must update sqlite_sequence so AUTOINCREMENT continues cleanly.
    conn.execute("ALTER TABLE t RENAME TO t2").unwrap();
    let seq = conn
        .execute("SELECT name, seq FROM sqlite_sequence WHERE name = 't2'")
        .unwrap();
    assert_eq!(seq.rows.len(), 1);
    assert!(seq.rows[0][1].exact_eq(&SqlValue::Int(2)));

    // RENAME COLUMN must rewrite dependent index & table definitions cleanly.
    conn.execute("CREATE INDEX idx_t2_tag ON t2(tag)").unwrap();
    conn.execute("ALTER TABLE t2 RENAME COLUMN tag TO label").unwrap();
    let q = conn
        .execute("SELECT label FROM t2 WHERE label = 'def'")
        .unwrap();
    assert_eq!(q.rows.len(), 2);

    // ADD COLUMN must reject PRIMARY KEY, UNIQUE, STORED generated, and NOT NULL without default.
    assert!(conn.execute("ALTER TABLE t2 ADD COLUMN k INT PRIMARY KEY").is_err());
    assert!(conn.execute("ALTER TABLE t2 ADD COLUMN u INT UNIQUE").is_err());
    assert!(conn
        .execute("ALTER TABLE t2 ADD COLUMN g INT GENERATED ALWAYS AS (id + 1) STORED")
        .is_err());
    assert!(conn.execute("ALTER TABLE t2 ADD COLUMN nn INT NOT NULL").is_err());

    // CREATE TABLE AS SELECT deduplicates duplicate column names with :1, :2.
    conn.execute("CREATE TABLE dup AS SELECT 1 AS x, 2 AS x, 3 AS x")
        .unwrap();
    let dr = conn.execute("SELECT * FROM dup").unwrap();
    assert_eq!(dr.columns, vec!["x", "x:1", "x:2"]);

    // SAVEPOINT without BEGIN starts a transaction that COMMIT can close, and BEGIN inside it errors.
    conn.execute("SAVEPOINT s1").unwrap();
    assert!(conn.execute("BEGIN").is_err());
    conn.execute("INSERT INTO t2(name) VALUES ('c')").unwrap();
    conn.execute("COMMIT").unwrap();
    assert!(conn
        .execute("SELECT count(*) FROM t2")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(3)));

    // Omitted INTEGER PRIMARY KEY with DEFAULT still auto-generates rowid (SQLite ignores DEFAULT on IPK).
    conn.execute("CREATE TABLE ipk_def (id INTEGER PRIMARY KEY DEFAULT 99, v TEXT)")
        .unwrap();
    conn.execute("INSERT INTO ipk_def(v) VALUES ('first'), ('second')")
        .unwrap();
    let ir = conn.execute("SELECT id FROM ipk_def ORDER BY id").unwrap();
    assert!(ir.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(ir.rows[1][0].exact_eq(&SqlValue::Int(2)));

    // INSERT OR IGNORE ignores NOT NULL / CHECK violations, but STRICT datatype mismatch still errors!
    conn.execute(
        "CREATE TABLE s_tbl (id INT PRIMARY KEY, n INT NOT NULL CHECK (n > 0)) STRICT",
    )
    .unwrap();
    conn.execute("INSERT OR IGNORE INTO s_tbl VALUES (1, NULL), (2, -5), (3, 10)")
        .unwrap();
    assert!(conn
        .execute("SELECT count(*) FROM s_tbl")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));
    assert!(conn
        .execute("INSERT OR IGNORE INTO s_tbl VALUES (4, 'not-an-int')")
        .is_err());

    // WITHOUT ROWID: secondary UNIQUE UPSERT DO UPDATE + ADD COLUMN DEFAULT + DROP COLUMN record rewrite.
    conn.execute(
        "CREATE TABLE wr (k TEXT PRIMARY KEY, u TEXT UNIQUE, v INT) WITHOUT ROWID",
    )
    .unwrap();
    conn.execute("INSERT INTO wr VALUES ('k1', 'u1', 10), ('k2', 'u2', 20), ('k3', 'u3', 30)")
        .unwrap();
    conn.execute(
        "INSERT INTO wr VALUES ('k9', 'u1', 99)
         ON CONFLICT(u) DO UPDATE SET v = (SELECT max(v) + 1 FROM wr)",
    )
    .unwrap();
    let wr_v = conn.execute("SELECT v FROM wr WHERE k = 'k1'").unwrap();
    assert!(wr_v.rows[0][0].exact_eq(&SqlValue::Int(31)));
    // Secondary UNIQUE violation during DO UPDATE must be rejected.
    assert!(conn
        .execute(
            "INSERT INTO wr VALUES ('k1', 'u1', 0)
             ON CONFLICT(k) DO UPDATE SET u = 'u2'",
        )
        .is_err());

    conn.execute("ALTER TABLE wr ADD COLUMN d REAL DEFAULT 2.5")
        .unwrap();
    // Deleting a pre-ADD-COLUMN row (fewer stored fields on disk than schema) must still find and remove it!
    conn.execute("DELETE FROM wr WHERE k = 'k3'").unwrap();
    let wr_d = conn.execute("SELECT d FROM wr WHERE k = 'k1'").unwrap();
    assert!(wr_d.rows[0][0].exact_eq(&SqlValue::Real(2.5)));
    conn.execute("ALTER TABLE wr DROP COLUMN v").unwrap();
    let wr_after = conn.execute("SELECT k, u, d FROM wr ORDER BY k").unwrap();
    assert_eq!(wr_after.rows.len(), 2);
    assert!(wr_after.rows[0][2].exact_eq(&SqlValue::Real(2.5)));

    assert_integrity_ok(&mut conn);
    let bytes = conn.to_bytes();
    verify_with_real_sqlite(
        "ddl_alter_without_rowid",
        &bytes,
        None,
        Some("SELECT k, u, d FROM wr ORDER BY k"),
    );
}

#[test]
fn index_scan_hints_respect_collation_and_comparison_affinity_and_close_releases_memory() {
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: None,
        max_memory_bytes: Some(1_000_000),
        max_page_reads: None,
    }));
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(meter.clone()),
        ..ConnectionOptions::default()
    })
    .unwrap();
    conn.execute(
        "CREATE TABLE items (name TEXT, code TEXT);
         CREATE INDEX idx_items_name ON items(name);
         CREATE INDEX idx_items_code ON items(code);
         INSERT INTO items VALUES ('alice', '01'), ('ALICE', '1'), ('bob', '2');",
    )
    .unwrap();

    // IndexEq on BINARY index must NOT skip 'ALICE' when query requests COLLATE NOCASE.
    let r1 = conn
        .execute("SELECT name FROM items WHERE name = 'alice' COLLATE NOCASE ORDER BY name")
        .unwrap();
    assert_eq!(r1.rows.len(), 2);

    // IndexEq on TEXT index with REAL RHS uses NUMERIC comparison affinity: '01' == 1.0 and '1' == 1.0!
    let r2 = conn
        .execute("SELECT code FROM items WHERE code = CAST(1 AS REAL) ORDER BY code")
        .unwrap();
    assert_eq!(r2.rows.len(), 2);

    // Close releases tracked memory and rejects further operations / cursors cleanly.
    let bytes = conn.to_bytes();
    assert!(meter.usage().memory_bytes > 0);
    conn.close().unwrap();
    assert_eq!(meter.usage().memory_bytes, 0);
    assert!(matches!(
        conn.execute("SELECT 1"),
        Err(SqliteError::DatabaseClosed)
    ));
    assert!(matches!(
        conn.seed_json(r#"{"t":[]}"#),
        Err(SqliteError::DatabaseClosed)
    ));

    // Read-only Database::close() must also invalidate active RowCursor reads.
    let ro = open_database_from_bytes(&bytes, None, None, OpenOptions::default()).unwrap();
    let mut cur = ro.table("items").unwrap().cursor(None).unwrap();
    assert!(cur.next().unwrap());
    ro.close().unwrap();
    assert!(matches!(cur.row(), Err(SqliteError::DatabaseClosed)));
}

#[test]
fn pass2_three_level_btree_delete_rebalancing_and_utf16_verified_by_c_sqlite() {
    // 1. 512B pages -> force 3-level Table AND Index B-trees, then delete contiguous middle
    //    ranges to empty entire interior subtrees (exercising balanced sibling merge/redistribution).
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512,
        ..ConnectionOptions::default()
    })
    .unwrap();
    conn.execute(
        "CREATE TABLE big (id INTEGER PRIMARY KEY, k TEXT, pad TEXT);
         CREATE INDEX idx_big_k ON big(k);",
    )
    .unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 1..=260i64 {
        let key = format!("key_{i:05}_{}", "x".repeat(28));
        let pad = format!("pad_{i:05}_{}", "y".repeat(60));
        let params = QueryParams::positional(vec![
            SqlValue::Int(i),
            SqlValue::text(key),
            SqlValue::text(pad),
        ]);
        conn.execute_with_params("INSERT INTO big VALUES (?1, ?2, ?3)", &params)
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    assert_integrity_ok(&mut conn);

    // Delete large contiguous interior slices (emptying multiple leaf & interior nodes).
    conn.execute(
        "BEGIN;
         DELETE FROM big WHERE id BETWEEN 25 AND 115;
         DELETE FROM big WHERE id BETWEEN 130 AND 235;
         COMMIT;",
    )
    .unwrap();
    assert_integrity_ok(&mut conn);
    let cnt = conn.execute("SELECT count(*) FROM big").unwrap();
    assert!(cnt.rows[0][0].exact_eq(&SqlValue::Int(260 - 91 - 106)));

    // Verify with REAL C SQLite (`PRAGMA integrity_check` catches `Child page depth differs`!).
    let bytes = conn.to_bytes();
    let c_out = verify_with_real_sqlite(
        "pass2_3level_btree_rebalance",
        &bytes,
        None,
        Some("SELECT count(*) AS n FROM big"),
    );
    if let Some(json) = c_out {
        assert!(json.contains("\"n\":63"), "unexpected C SQLite count: {json}");
    }

    // 2. UTF-16LE encoding: WITHOUT ROWID & secondary index BINARY ordering must use UTF-16
    //    code-unit order (U+10000 surrogate 0xD800 < U+E000 0xE000, opposite of UTF-8!).
    let mut u16_conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    u16_conn.execute("PRAGMA encoding = 'UTF-16le'").unwrap();
    assert!(u16_conn
        .execute("PRAGMA encoding")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::text("UTF-16le")));
    u16_conn
        .execute(
            "CREATE TABLE wr_u16 (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
             CREATE TABLE t_u16 (id INTEGER PRIMARY KEY, s TEXT);
             CREATE INDEX idx_t_u16_s ON t_u16(s);",
        )
        .unwrap();
    let p_high = QueryParams::positional(vec![SqlValue::text("\u{E000}"), SqlValue::Int(2)]);
    let p_supp = QueryParams::positional(vec![SqlValue::text("\u{10000}"), SqlValue::Int(1)]);
    u16_conn
        .execute_with_params("INSERT INTO wr_u16 VALUES (?1, ?2)", &p_high)
        .unwrap();
    u16_conn
        .execute_with_params("INSERT INTO wr_u16 VALUES (?1, ?2)", &p_supp)
        .unwrap();
    u16_conn
        .execute_with_params("INSERT INTO t_u16 VALUES (?2, ?1)", &p_high)
        .unwrap();
    u16_conn
        .execute_with_params("INSERT INTO t_u16 VALUES (?2, ?1)", &p_supp)
        .unwrap();
    assert_integrity_ok(&mut u16_conn);

    // Real C SQLite `PRAGMA integrity_check` verifies index & WITHOUT ROWID B-tree key order!
    let u16_bytes = u16_conn.to_bytes();
    let c_u16 = verify_with_real_sqlite(
        "pass2_utf16le_btree_order",
        &u16_bytes,
        None,
        Some("SELECT v FROM wr_u16 ORDER BY k"),
    );
    if let Some(json) = c_u16 {
        assert_eq!(json, r#"[{"v":1},{"v":2}]"#);
    }
}

#[test]
fn pass2_torn_commit_recovery_zero_page_hot_journal_and_snapshot_guards() {
    // 1. Mid-commit I/O failure -> ROLLBACK -> next commit touching a DIFFERENT table/page
    //    must recover the torn main file from `-journal` BEFORE truncating `-journal`.
    let vfs = Arc::new(FaultVfs::new());
    let db_path = "/torn.db";
    let mut conn = SqliteConnection::open(vfs.clone(), db_path, ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, val TEXT);
         CREATE TABLE b (id INTEGER PRIMARY KEY, val TEXT);
         INSERT INTO a VALUES (1, 'orig_a');
         INSERT INTO b VALUES (1, 'orig_b');",
    )
    .unwrap();

    // Crash after writing 1 main-DB page (table `a`'s page or page 1) during multi-page commit.
    vfs.arm_crash_after_main_writes(1);
    let err = conn
        .execute(
            "BEGIN;
             UPDATE a SET val = 'corrupt_uncommitted_a' WHERE id = 1;
             UPDATE b SET val = 'corrupt_uncommitted_b' WHERE id = 1;
             COMMIT;",
        )
        .expect_err("simulated crash must fail COMMIT");
    assert!(matches!(err, SqliteError::Io(_)));
    vfs.disarm();

    // Explicit ROLLBACK + new commit touching ONLY table `b` must heal torn table `a` on disk!
    conn.execute("ROLLBACK").unwrap();
    conn.execute("INSERT INTO b VALUES (2, 'new_b')").unwrap();
    assert!(!vfs.exists("/torn.db-journal").unwrap());
    conn.close().unwrap();

    // Reopen fresh from VFS and verify BOTH Rust and real C SQLite see clean `orig_a` + `new_b`!
    let ro = open_database(vfs.as_ref(), db_path, OpenOptions::default()).unwrap();
    let ra = ro.query("SELECT val FROM a WHERE id = 1").unwrap();
    assert!(ra.rows[0][0].exact_eq(&SqlValue::text("orig_a")));
    let rb = ro.query("SELECT val FROM b ORDER BY id").unwrap();
    assert_eq!(rb.rows.len(), 2);
    assert!(rb.rows[0][0].exact_eq(&SqlValue::text("orig_b")));
    assert!(rb.rows[1][0].exact_eq(&SqlValue::text("new_b")));

    let mut f = vfs
        .inner
        .open(db_path, OpenFlags::readonly(FileKind::Other))
        .unwrap();
    let sz = f.size().unwrap() as usize;
    let mut disk_bytes = vec![0u8; sz];
    f.read(&mut disk_bytes, 0).unwrap();
    verify_with_real_sqlite(
        "pass2_torn_commit_healed",
        &disk_bytes,
        None,
        Some("SELECT val FROM a"),
    );

    // 2a. Crash during the VERY FIRST commit on an empty 0-page DB before any main write
    //     (main_size == 0, stale -journal present) -> RW reopen cleans up `-journal`.
    let vfs2 = Arc::new(FaultVfs::new());
    let empty_path = "/first_commit_crash.db";
    let mut c2 = SqliteConnection::open(vfs2.clone(), empty_path, ConnectionOptions::default()).unwrap();
    vfs2.arm_crash_after_main_writes(0);
    let _ = c2.execute("CREATE TABLE t(x INT)").unwrap_err();
    vfs2.disarm();
    assert!(vfs2.exists("/first_commit_crash.db-journal").unwrap());
    drop(c2);

    let mut c2_reopen =
        SqliteConnection::open(vfs2.clone(), empty_path, ConnectionOptions::default()).unwrap();
    assert!(!vfs2.exists("/first_commit_crash.db-journal").unwrap());
    assert_eq!(c2_reopen.page_count(), 0);
    c2_reopen.execute("CREATE TABLE ok_tbl(x INT); INSERT INTO ok_tbl VALUES (7);").unwrap();
    let r = c2_reopen.execute("SELECT x FROM ok_tbl").unwrap();
    assert!(r.rows[0][0].exact_eq(&SqlValue::Int(7)));

    // 2b. Crash during the VERY FIRST multi-page commit AFTER writing 1 main page
    //     (main_size > 0, hot journal with `db_size == 0` -> HotApplied recovering to 0 pages!).
    let vfs2b = Arc::new(FaultVfs::new());
    let empty_path_b = "/first_commit_crash_partial.db";
    let mut c2b =
        SqliteConnection::open(vfs2b.clone(), empty_path_b, ConnectionOptions::default()).unwrap();
    vfs2b.arm_crash_after_main_writes(1);
    let _ = c2b.execute("CREATE TABLE t(x INT)").unwrap_err();
    vfs2b.disarm();
    assert!(vfs2b.exists("/first_commit_crash_partial.db-journal").unwrap());
    drop(c2b);

    let mut c2b_reopen =
        SqliteConnection::open(vfs2b.clone(), empty_path_b, ConnectionOptions::default()).unwrap();
    assert!(!vfs2b.exists("/first_commit_crash_partial.db-journal").unwrap());
    assert_eq!(c2b_reopen.page_count(), 0);
    c2b_reopen
        .execute("CREATE TABLE ok_tbl2(x INT); INSERT INTO ok_tbl2 VALUES (9);")
        .unwrap();
    assert!(c2b_reopen
        .execute("SELECT x FROM ok_tbl2")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(9)));

    // 3. Snapshot mode guards:
    //    (a) Stale `-wal` bytes MUST be ignored when main DB header is in rollback mode (bytes 18..20 == 1, 1).
    let mut c_rb = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_rb.execute("CREATE TABLE s(x INT); INSERT INTO s VALUES (1);").unwrap();
    let rb_bytes = c_rb.to_bytes();
    assert_eq!(rb_bytes[18], 1);
    assert_eq!(rb_bytes[19], 1);
    let fake_wal = vec![0x37u8, 0x7f, 0x06, 0x82, 0, 0, 0, 0];
    let ro_rb = open_database_from_bytes(&rb_bytes, Some(&fake_wal), None, OpenOptions::default()).unwrap();
    assert_eq!(ro_rb.mode(), xdb_sqlite::SnapshotMode::Rollback);

    //    (b) Corrupt schema_format > 4 (e.g. 260 = 0x104, whose low byte is 4) must be rejected!
    let mut bad_fmt = rb_bytes.clone();
    bad_fmt[44..48].copy_from_slice(&260u32.to_be_bytes());
    assert!(open_database_from_bytes(&bad_fmt, None, None, OpenOptions::default()).is_err());

    //    (c) Valid hot journal with small sector_size (64 bytes < 512) must play back cleanly!
    let mut j64 = vec![0u8; 64];
    j64[0..8].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
    j64[8..12].copy_from_slice(&0i32.to_be_bytes()); // n_rec = 0
    j64[12..16].copy_from_slice(&12345u32.to_be_bytes()); // nonce
    j64[16..20].copy_from_slice(&0u32.to_be_bytes()); // db_size = 0
    j64[20..24].copy_from_slice(&64u32.to_be_bytes()); // sector_size = 64
    j64[24..28].copy_from_slice(&4096u32.to_be_bytes()); // page_size = 4096
    let ro_j64 =
        open_database_from_bytes(&rb_bytes, None, Some(&j64), OpenOptions::default()).unwrap();
    // Playing back the 64-byte-sector hot journal (db_size = 0) rolls back the 2-page DB to empty!
    assert_eq!(ro_j64.mode(), xdb_sqlite::SnapshotMode::Empty);
    assert_eq!(ro_j64.page_count(), 0);
    assert!(ro_j64.tables().is_empty());
}

#[test]
fn pass2_connection_ddl_dml_constraints_upsert_and_alter() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. UPDATE OR REPLACE must NOT corrupt indexes or resurrect replaced rows.
    conn.execute(
        "CREATE TABLE urep (id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO urep VALUES (1, 10, 'a'), (2, 20, 'b');",
    )
    .unwrap();
    let res = conn
        .execute("UPDATE OR REPLACE urep SET u = 20, v = 'new'")
        .unwrap();
    assert_eq!(res.changes, 1);
    let rows = conn.execute("SELECT id, u, v FROM urep ORDER BY id").unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(rows.rows[0][1].exact_eq(&SqlValue::Int(20)));
    assert!(rows.rows[0][2].exact_eq(&SqlValue::text("new")));
    assert_integrity_ok(&mut conn);

    conn.execute(
        "CREATE TABLE urep2 (id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO urep2 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
         UPDATE OR REPLACE urep2 SET id = id + 1;",
    )
    .unwrap();
    assert_integrity_ok(&mut conn);

    // 2. INSERT OR FAIL & UPDATE OR FAIL preserve earlier rows and leave failing row intact.
    conn.execute(
        "CREATE TABLE fail_t (id INTEGER PRIMARY KEY, u INT UNIQUE);
         INSERT INTO fail_t VALUES (2, 20), (3, 30);",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT OR FAIL INTO fail_t VALUES (1, 10), (2, 22), (4, 40)")
        .is_err());
    let f1 = conn.execute("SELECT id FROM fail_t ORDER BY id").unwrap();
    assert_eq!(f1.rows.len(), 3); // 1 kept, 2 untouched, 3 untouched, 4 not inserted!
    assert!(f1.rows[0][0].exact_eq(&SqlValue::Int(1)));

    assert!(conn
        .execute("UPDATE OR FAIL fail_t SET u = CASE id WHEN 1 THEN 100 WHEN 2 THEN 30 ELSE 300 END")
        .is_err());
    let f2 = conn.execute("SELECT id, u FROM fail_t ORDER BY id").unwrap();
    assert!(f2.rows[0][1].exact_eq(&SqlValue::Int(100)));
    assert!(f2.rows[1][1].exact_eq(&SqlValue::Int(20)));
    assert!(f2.rows[2][1].exact_eq(&SqlValue::Int(30)));

    // 3. Schema-level ON CONFLICT (NOT NULL REPLACE DEFAULT, IGNORE, UNIQUE REPLACE, PK ROLLBACK).
    conn.execute(
        "CREATE TABLE nn_rep (a INT NOT NULL ON CONFLICT REPLACE DEFAULT 7, b INT);
         INSERT INTO nn_rep VALUES (NULL, 1);",
    )
    .unwrap();
    assert!(conn
        .execute("SELECT a FROM nn_rep")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(7)));

    conn.execute(
        "CREATE TABLE nn_def (a INT NOT NULL DEFAULT 42);
         INSERT OR REPLACE INTO nn_def VALUES (NULL);",
    )
    .unwrap();
    assert!(conn
        .execute("SELECT a FROM nn_def")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(42)));

    conn.execute(
        "CREATE TABLE u_rep (a INT UNIQUE ON CONFLICT REPLACE, b INT);
         INSERT INTO u_rep VALUES (1, 10), (1, 20);",
    )
    .unwrap();
    assert!(conn
        .execute("SELECT b FROM u_rep WHERE a = 1")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(20)));

    conn.execute("CREATE TABLE pk_rb (a INT PRIMARY KEY ON CONFLICT ROLLBACK)")
        .unwrap();
    conn.execute("BEGIN; INSERT INTO pk_rb VALUES (1);").unwrap();
    assert!(conn.execute("INSERT INTO pk_rb VALUES (1)").is_err());
    assert!(!conn.in_transaction(), "ON CONFLICT ROLLBACK must roll back transaction");
    assert_eq!(conn.execute("SELECT * FROM pk_rb").unwrap().rows.len(), 0);

    // 4. UPDATE ... FROM multi-table JOIN, WITH ... UPDATE/DELETE, SET (a, b) = (SELECT ...).
    conn.execute(
        "CREATE TABLE dst (id INT PRIMARY KEY, x INT, y TEXT);
         CREATE TABLE s1 (id INT, k INT);
         CREATE TABLE s2 (k INT, label TEXT);
         INSERT INTO dst VALUES (1, 0, 'old'), (2, 0, 'keep');
         INSERT INTO s1 VALUES (1, 10);
         INSERT INTO s2 VALUES (10, 'joined');
         UPDATE dst SET x = s1.k, y = s2.label FROM s1 JOIN s2 ON s1.k = s2.k WHERE dst.id = s1.id;",
    )
    .unwrap();
    let d1 = conn.execute("SELECT x, y FROM dst WHERE id = 1").unwrap();
    assert!(d1.rows[0][0].exact_eq(&SqlValue::Int(10)));
    assert!(d1.rows[0][1].exact_eq(&SqlValue::text("joined")));

    conn.execute(
        "WITH cte(v) AS (SELECT 99) UPDATE dst SET x = (SELECT v FROM cte) WHERE id = 2;
         WITH del_ids(id) AS (SELECT 2) DELETE FROM dst WHERE id IN (SELECT id FROM del_ids);
         UPDATE dst SET (x, y) = (SELECT 77, 'multi') WHERE id = 1;",
    )
    .unwrap();
    let d2 = conn.execute("SELECT id, x, y FROM dst").unwrap();
    assert_eq!(d2.rows.len(), 1);
    assert!(d2.rows[0][1].exact_eq(&SqlValue::Int(77)));
    assert!(d2.rows[0][2].exact_eq(&SqlValue::text("multi")));

    // 5. UPSERT table alias + expression target + partial-index parity.
    conn.execute(
        "CREATE TABLE up_t (id INT PRIMARY KEY, email TEXT, v INT);
         CREATE UNIQUE INDEX idx_up_email ON up_t(lower(email));
         INSERT INTO up_t VALUES (1, 'Alice@Ex.com', 10);
         INSERT INTO up_t AS dest VALUES (1, 'Alice@Ex.com', 5)
           ON CONFLICT(id) DO UPDATE SET v = dest.v + excluded.v;
         INSERT INTO up_t VALUES (2, 'alice@ex.com', 99)
           ON CONFLICT(lower(email)) DO UPDATE SET v = excluded.v;",
    )
    .unwrap();
    assert!(conn
        .execute("SELECT v FROM up_t WHERE id = 1")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(99)));

    // 6. STRICT generated columns (VIRTUAL + STORED) & generated column cycle rejection.
    conn.execute(
        "CREATE TABLE sg (a ANY, b INT GENERATED ALWAYS AS (a) VIRTUAL, c INT GENERATED ALWAYS AS (a) STORED) STRICT",
    )
    .unwrap();
    conn.execute("INSERT INTO sg(a) VALUES (5)").unwrap();
    assert!(conn.execute("INSERT INTO sg(a) VALUES ('not_int')").is_err());
    assert!(conn
        .execute(
            "CREATE TABLE cyc (a INT, b INT GENERATED ALWAYS AS (c + 1), c INT GENERATED ALWAYS AS (b + 1))",
        )
        .is_err());

    // 7. DEFAULT (true), CHECK(rowid > 0), partial index WHERE rowid > 1, non-deterministic CREATE INDEX.
    conn.execute("CREATE TABLE dt (a INT DEFAULT (true), CHECK(rowid > 0))").unwrap();
    conn.execute("INSERT INTO dt(rowid) VALUES (1)").unwrap();
    assert!(conn
        .execute("SELECT a FROM dt")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));
    assert!(conn.execute("INSERT INTO dt(rowid, a) VALUES (0, 1)").is_err());

    conn.execute(
        "CREATE TABLE pr (a INT);
         CREATE UNIQUE INDEX idx_pr ON pr(a) WHERE rowid > 1;
         INSERT INTO pr(rowid, a) VALUES (1, 10), (2, 10);",
    )
    .unwrap();
    assert!(conn.execute("INSERT INTO pr(rowid, a) VALUES (3, 10)").is_err());
    assert!(conn
        .execute("CREATE INDEX bad_nondet ON pr(a + random())")
        .is_err());
    assert!(conn
        .execute("CREATE INDEX bad_ts ON pr(a) WHERE a > CURRENT_TIMESTAMP")
        .is_err());

    // 8. IPK float/text coercion, INSERT(id, rowid) order, i64::MAX rowid probe, AUTOINCREMENT neg + sqlite_sequence DML.
    conn.execute("CREATE TABLE ipk_t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    conn.execute("INSERT INTO ipk_t VALUES (4503599627370496.0, 'p52'), ('1.0', 'one')")
        .unwrap();
    conn.execute("INSERT INTO ipk_t(id, rowid, v) VALUES (10, 20, 'ir')")
        .unwrap();
    conn.execute("INSERT INTO ipk_t(rowid, id, v) VALUES (30, 40, 'ri')")
        .unwrap();
    conn.execute("INSERT INTO ipk_t VALUES (9223372036854775807, 'max')")
        .unwrap();
    conn.execute("INSERT INTO ipk_t(v) VALUES ('probed')").unwrap();
    let ipk_ids = conn.execute("SELECT id FROM ipk_t ORDER BY id").unwrap();
    assert_eq!(ipk_ids.rows.len(), 6);
    assert!(ipk_ids.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(ipk_ids.rows[1][0].exact_eq(&SqlValue::Int(20)));
    assert!(ipk_ids.rows[2][0].exact_eq(&SqlValue::Int(40)));

    conn.execute("CREATE TABLE ai_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .unwrap();
    conn.execute("INSERT INTO ai_t VALUES (-5, 'neg')").unwrap();
    conn.execute("INSERT INTO ai_t(v) VALUES ('pos')").unwrap();
    let ai_rows = conn.execute("SELECT id FROM ai_t ORDER BY id").unwrap();
    assert!(ai_rows.rows[0][0].exact_eq(&SqlValue::Int(-5)));
    assert!(ai_rows.rows[1][0].exact_eq(&SqlValue::Int(1)));
    conn.execute("DELETE FROM ai_t; DELETE FROM sqlite_sequence WHERE name = 'ai_t';")
        .unwrap();
    conn.execute("INSERT INTO ai_t(v) VALUES ('reset')").unwrap();
    assert!(conn
        .execute("SELECT id FROM ai_t")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));

    // 9. Schema qualifier, TEMP, namespace collision, DROP cross-type, CREATE TABLE AS SELECT empty affinities.
    assert!(conn.execute("CREATE TABLE bogus.x(a INT)").is_err());
    assert!(conn.execute("CREATE TEMP TABLE tmp_x(a INT)").is_err());
    assert!(conn.execute("CREATE TEMP VIEW tmp_v AS SELECT 1").is_err());
    assert!(conn.execute("CREATE TABLE idx_pr(a INT)").is_err());
    conn.execute("CREATE VIEW v_drop AS SELECT 1 AS c").unwrap();
    let err_dt = conn.execute("DROP TABLE IF EXISTS v_drop").unwrap_err();
    assert!(err_dt.to_string().contains("DROP VIEW"));
    let err_dv = conn.execute("DROP VIEW IF EXISTS pr").unwrap_err();
    assert!(err_dv.to_string().contains("DROP TABLE"));

    conn.execute("CREATE TABLE src_aff (a INT, b TEXT, c REAL, d BLOB, e NUM)")
        .unwrap();
    conn.execute(
        "CREATE TABLE dst_aff AS SELECT a, b, c, d, e, 1+2 AS s, CAST(NULL AS INT) AS n FROM src_aff",
    )
    .unwrap();
    let ti = conn.execute("PRAGMA table_info(dst_aff)").unwrap();
    assert!(ti.rows[0][2].exact_eq(&SqlValue::text("INT")));
    assert!(ti.rows[1][2].exact_eq(&SqlValue::text("TEXT")));
    assert!(ti.rows[2][2].exact_eq(&SqlValue::text("REAL")));
    assert!(ti.rows[3][2].exact_eq(&SqlValue::text("")));
    assert!(ti.rows[4][2].exact_eq(&SqlValue::text("NUM")));
    assert!(ti.rows[5][2].exact_eq(&SqlValue::text("")));
    assert!(ti.rows[6][2].exact_eq(&SqlValue::text("INT")));

    // 10. ALTER TABLE DROP COLUMN (view check + comment before comma) & PRAGMA synchronous.
    conn.execute(
        "CREATE TABLE dc (a INT /* comment before comma */, b INT, c INT);
         CREATE VIEW v_dc AS SELECT c FROM dc;",
    )
    .unwrap();
    assert!(conn.execute("ALTER TABLE dc DROP COLUMN c").is_err());
    conn.execute("ALTER TABLE dc DROP COLUMN a").unwrap();
    assert_eq!(conn.execute("PRAGMA table_info(dc)").unwrap().rows.len(), 2);
    assert!(conn.execute("ALTER TABLE dc RENAME TO idx_pr").is_err());

    assert!(conn
        .execute("PRAGMA synchronous")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(2)));
    conn.execute("PRAGMA synchronous = NORMAL").unwrap();
    assert!(conn
        .execute("PRAGMA synchronous")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));

    assert_integrity_ok(&mut conn);
    let bytes = conn.to_bytes();
    verify_with_real_sqlite(
        "pass2_connection_all",
        &bytes,
        None,
        Some("SELECT id, u, v FROM urep"),
    );
}
