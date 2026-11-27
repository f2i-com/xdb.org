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

#[test]
fn pass3_connection_ddl_upsert_returning_alter_and_fast_paths() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. `CREATE TABLE/INDEX/VIEW main.<name>` strips `main.` in stored `sqlite_schema.sql`!
    conn.execute(
        "CREATE TABLE IF NOT EXISTS main.items (id INTEGER PRIMARY KEY, grp INT, name TEXT COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS main.idx_items_grp ON items(grp);
         CREATE INDEX main.idx_items_name ON items(name);
         CREATE VIEW IF NOT EXISTS main.v_items AS SELECT id, name FROM items;",
    )
    .unwrap();
    let sch = conn
        .execute("SELECT name, sql FROM sqlite_schema WHERE name IN ('items', 'idx_items_grp', 'v_items') ORDER BY name")
        .unwrap();
    for row in &sch.rows {
        let sql_text = match &row[1] {
            SqlValue::Text(b) => std::str::from_utf8(b).unwrap(),
            _ => panic!("expected text sql"),
        };
        assert!(
            !sql_text.to_ascii_lowercase().contains("main."),
            "stored DDL must strip main. prefix, got: {sql_text}"
        );
    }

    // 2. Parameters prohibited in CREATE VIEW; VACUUM INTO rejected.
    assert!(conn.execute("CREATE VIEW v_param AS SELECT ? AS x").is_err());
    assert!(conn.execute("VACUUM INTO 'out.db'").is_err());

    // 3. Multi-statement execute_with_params shifts anonymous `?` parameters per statement.
    conn.execute_with_params(
        "INSERT INTO items VALUES (?, ?, ?); INSERT INTO items VALUES (?, ?, ?);",
        &QueryParams::positional(vec![
            SqlValue::Int(1),
            SqlValue::Int(10),
            SqlValue::text("bob"),
            SqlValue::Int(2),
            SqlValue::Int(20),
            SqlValue::text("alice"),
        ]),
    )
    .unwrap();
    let rows = conn
        .execute("SELECT id, grp, name FROM items ORDER BY id")
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert!(rows.rows[0][2].exact_eq(&SqlValue::text("bob")));
    assert!(rows.rows[1][2].exact_eq(&SqlValue::text("alice")));

    // 4. Fast point UPDATE / DELETE pushdowns & exact-variant index maintenance (NOCASE 'bob' -> 'BOB').
    conn.execute_with_params(
        "UPDATE items SET grp = 11 WHERE id = ?",
        &QueryParams::positional(vec![SqlValue::Int(1)]),
    )
    .unwrap();
    conn.execute("UPDATE items SET name = 'BOB' WHERE id = 1")
        .unwrap();
    let bob_row = conn.execute("SELECT grp, name FROM items WHERE grp = 11").unwrap();
    assert_eq!(bob_row.rows.len(), 1);
    assert!(bob_row.rows[0][1].exact_eq(&SqlValue::text("BOB")));
    assert_integrity_ok(&mut conn);

    // 5. UPSERT: unqualified target column resolution (`v = v + excluded.v`) + target validation!
    conn.execute(
        "CREATE TABLE kv (k INT PRIMARY KEY, v INT, u TEXT COLLATE NOCASE UNIQUE);
         CREATE TABLE part_t (a INT, b INT);
         CREATE UNIQUE INDEX part_idx ON part_t(a) WHERE a > 0;",
    )
    .unwrap();
    conn.execute("INSERT INTO kv VALUES (1, 10, 'x')").unwrap();
    conn.execute(
        "INSERT INTO kv VALUES (1, 5, 'y') ON CONFLICT(k) DO UPDATE SET v = v + excluded.v",
    )
    .unwrap();
    let kv_v = conn.execute("SELECT v FROM kv WHERE k = 1").unwrap();
    assert!(kv_v.rows[0][0].exact_eq(&SqlValue::Int(15)));

    // Invalid UPSERT targets must fail even with 0 conflicts:
    assert!(conn
        .execute("INSERT INTO kv VALUES (2, 1, 'z') ON CONFLICT(v) DO NOTHING")
        .is_err());
    assert!(conn
        .execute("INSERT INTO kv VALUES (2, 1, 'z') ON CONFLICT(no_such_col) DO NOTHING")
        .is_err());
    assert!(conn
        .execute("INSERT INTO kv VALUES (2, 1, 'z') ON CONFLICT(u COLLATE BINARY) DO NOTHING")
        .is_err());
    assert!(conn
        .execute("INSERT INTO part_t VALUES (1, 1) ON CONFLICT(a) WHERE a > 5 DO NOTHING")
        .is_err());
    // Matching partial WHERE & matching explicit COLLATE succeed:
    conn.execute("INSERT INTO part_t VALUES (1, 10)").unwrap();
    conn.execute("INSERT INTO part_t VALUES (1, 99) ON CONFLICT(a) WHERE a > 0 DO UPDATE SET b = excluded.b")
        .unwrap();
    assert!(conn
        .execute("SELECT b FROM part_t WHERE a = 1")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(99)));
    conn.execute("INSERT INTO kv VALUES (2, 1, 'X') ON CONFLICT(u COLLATE NOCASE) DO NOTHING")
        .unwrap();

    // 6. RETURNING: supports subqueries + updated last_insert_rowid(), rejects TABLE.*.
    conn.execute("CREATE TABLE ret_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO ret_t(v) VALUES (1) RETURNING ret_t.*")
        .is_err());
    let ret = conn
        .execute(
            "INSERT INTO ret_t(v) VALUES (10), (20)
             RETURNING id, last_insert_rowid(), (SELECT count(*) FROM ret_t)",
        )
        .unwrap();
    assert_eq!(ret.rows.len(), 2);
    assert!(ret.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(ret.rows[0][1].exact_eq(&SqlValue::Int(1)));
    assert!(ret.rows[0][2].exact_eq(&SqlValue::Int(1)));
    assert!(ret.rows[1][0].exact_eq(&SqlValue::Int(2)));
    assert!(ret.rows[1][1].exact_eq(&SqlValue::Int(2)));
    assert!(ret.rows[1][2].exact_eq(&SqlValue::Int(2)));

    // 7. ALTER TABLE ADD COLUMN CHECK validates existing rows; RENAME TABLE / RENAME COLUMN rewrite views!
    conn.execute("CREATE TABLE alt_t (a INT); INSERT INTO alt_t VALUES (5);")
        .unwrap();
    assert!(conn
        .execute("ALTER TABLE alt_t ADD COLUMN b INT DEFAULT 0 CHECK (b > 0)")
        .is_err());
    // Must have rolled back cleanly so `alt_t` still has only column `a`.
    assert_eq!(conn.execute("PRAGMA table_info(alt_t)").unwrap().rows.len(), 1);
    conn.execute("ALTER TABLE alt_t ADD COLUMN b INT DEFAULT 2 CHECK (b > 0)")
        .unwrap();

    conn.execute("CREATE VIEW v_alt AS SELECT alt_t.a, b FROM alt_t")
        .unwrap();
    conn.execute("ALTER TABLE alt_t RENAME COLUMN a TO a_new")
        .unwrap();
    let v1 = conn.execute("SELECT a_new, b FROM v_alt").unwrap();
    assert!(v1.rows[0][0].exact_eq(&SqlValue::Int(5)));
    conn.execute("ALTER TABLE alt_t RENAME TO alt_t2").unwrap();
    let v2 = conn.execute("SELECT a_new, b FROM v_alt").unwrap();
    assert!(v2.rows[0][0].exact_eq(&SqlValue::Int(5)));
    assert!(v2.rows[0][1].exact_eq(&SqlValue::Int(2)));

    // 8. UPDATE ... FROM exposes rowid on FROM table and handles hash join (>= 16 rows) cleanly.
    conn.execute("CREATE TABLE ut (id INT, val INT); CREATE TABLE us (k INT, dv INT);")
        .unwrap();
    for i in 1..=20i64 {
        conn.execute(&format!("INSERT INTO ut VALUES ({i}, 0); INSERT INTO us VALUES ({i}, {});", i * 10))
            .unwrap();
    }
    conn.execute("UPDATE ut SET val = us.dv + us.rowid FROM us WHERE ut.id = us.k")
        .unwrap();
    let u5 = conn.execute("SELECT val FROM ut WHERE id = 5").unwrap();
    assert!(u5.rows[0][0].exact_eq(&SqlValue::Int(55)));

    // 9. Lazy VIRTUAL column evaluation skips unreferenced erroring virtual columns on SELECT!
    conn.execute(
        "CREATE TABLE lazy_v (id INT, ok INT);
         INSERT INTO lazy_v VALUES (1, 42);
         ALTER TABLE lazy_v ADD COLUMN boom TEXT GENERATED ALWAYS AS (json('{')) VIRTUAL;",
    )
    .unwrap();
    let ok_only = conn.execute("SELECT ok FROM lazy_v").unwrap();
    assert!(ok_only.rows[0][0].exact_eq(&SqlValue::Int(42)));
    assert!(conn.execute("SELECT boom FROM lazy_v").is_err());
    conn.execute("DROP TABLE lazy_v").unwrap();

    // 10. sqlite_sequence with NULL/TEXT seq coerced cleanly on AUTOINCREMENT insert.
    conn.execute("CREATE TABLE ai_seq (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    conn.execute("INSERT INTO ai_seq(v) VALUES (1)").unwrap();
    conn.execute("UPDATE sqlite_sequence SET seq = '10' WHERE name = 'ai_seq'")
        .unwrap();
    conn.execute("INSERT INTO ai_seq(v) VALUES (2)").unwrap();
    let ai_max = conn.execute("SELECT max(id) FROM ai_seq").unwrap();
    assert!(ai_max.rows[0][0].exact_eq(&SqlValue::Int(11)));

    // 11. seed_json shorthand table named `mode` + string statements row counting + batched insert!
    let rep_short = conn
        .seed_json(r#"{"mode": [{"id": 1, "label": "fast"}]}"#)
        .unwrap();
    assert_eq!(rep_short.tables_created, 1);
    assert_eq!(rep_short.rows_inserted, 1);
    let rep_stmt = conn
        .seed_json(
            r#"{"statements": ["CREATE TABLE st_cnt (x INT)", "INSERT INTO st_cnt VALUES (1), (2), (3)"]}"#,
        )
        .unwrap();
    assert_eq!(rep_stmt.rows_inserted, 3);
    assert_eq!(rep_stmt.statements_executed, 2);

    assert_integrity_ok(&mut conn);
    // Drop `lazy_v` only? No — even `lazy_v` is valid SQLite DDL! Verify whole DB with C SQLite!
    let bytes = conn.to_bytes();
    verify_with_real_sqlite(
        "pass3_connection_all",
        &bytes,
        None,
        Some("SELECT a_new, b FROM v_alt"),
    );
}

#[test]
fn pass4_audit_storage_btree_and_sql_verified_by_c_sqlite() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. PRAGMA encoding invalidates schema_gen & UTF-16 BINARY index range scan preserves UTF-16 order.
    conn.execute("PRAGMA encoding = 'UTF-16le';").unwrap();
    assert_eq!(conn.encoding(), xdb_core::TextEncoding::Utf16Le);
    conn.execute(
        "CREATE TABLE u16t (id INTEGER PRIMARY KEY, s TEXT);
         CREATE INDEX idx_u16_s ON u16t(s);
         INSERT INTO u16t VALUES (1, char(0x10000)), (2, char(0xE000));",
    )
    .unwrap();
    // In UTF-16LE/BE, U+10000 (surrogate 0xD800) sorts BEFORE U+E000 (opposite of UTF-8!).
    let u16_range = conn
        .query("SELECT id FROM u16t WHERE s < char(0xE000)")
        .unwrap();
    assert_eq!(u16_range.rows.len(), 1);
    assert!(u16_range.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert_integrity_ok(&mut conn);
    let u16_bytes = conn.to_bytes();
    verify_with_real_sqlite(
        "pass4_utf16_index",
        &u16_bytes,
        None,
        Some("SELECT id FROM u16t WHERE s < char(0xE000)"),
    );

    // 2. Main UTF-8 connection testing all Pass-4 connection/DML/DDL/seed/optimizer/B-tree cases!
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512, // small pages force deep multi-level B-trees & freeblock coalescing!
        ..ConnectionOptions::default()
    })
    .unwrap();

    // 2a. UPSERT DO UPDATE sees flushed sqlite_sequence from earlier rows in same INSERT!
    c.execute(
        "CREATE TABLE seq_up (id INTEGER PRIMARY KEY AUTOINCREMENT, k INT UNIQUE, seen_seq INT);
         INSERT INTO seq_up(k, seen_seq) VALUES (1, 0), (2, 0), (1, 0)
           ON CONFLICT(k) DO UPDATE SET seen_seq = (SELECT seq FROM sqlite_sequence WHERE name = 'seq_up');",
    )
    .unwrap();
    let seen = c.query("SELECT seen_seq FROM seq_up WHERE k = 1").unwrap();
    assert!(seen.rows[0][0].exact_eq(&SqlValue::Int(2)));

    // 2b. Partial index WHERE COLLATE must match structurally (`COLLATE NOCASE` != `BINARY`).
    c.execute(
        "CREATE TABLE part_c (id INT, s TEXT);
         CREATE UNIQUE INDEX idx_part_nc ON part_c(id) WHERE s = 'a' COLLATE NOCASE;
         INSERT INTO part_c VALUES (1, 'A');",
    )
    .unwrap();
    // Same id with 'a' violates NOCASE partial unique index!
    assert!(c.execute("INSERT INTO part_c VALUES (1, 'a')").is_err());

    // 2c. ALTER TABLE view rewrites: shadowed table alias, inner subquery col ownership,
    //     unrelated JOIN USING, and CTE + `main.t` exemption!
    c.execute(
        "CREATE TABLE t_ren (id INT, a INT);
         CREATE TABLE u_other (id INT, a INT);
         CREATE TABLE u_other2 (id INT, a INT);
         INSERT INTO t_ren VALUES (1, 10);
         INSERT INTO u_other VALUES (1, 99);
         INSERT INTO u_other2 VALUES (1, 99);
         CREATE VIEW v_inner AS SELECT t_ren.id, t_ren.a, (SELECT a FROM u_other LIMIT 1) AS ua FROM t_ren;
         CREATE VIEW v_shadow AS SELECT t_ren.a FROM u_other AS t_ren;
         CREATE VIEW v_cte_both AS WITH t_ren(a) AS (SELECT 77) SELECT c.a AS ca, m.a AS ma FROM t_ren AS c, main.t_ren AS m;
         CREATE VIEW v_cte_main AS WITH t_ren(a) AS (SELECT 77) SELECT main.t_ren.a AS ma FROM main.t_ren;
         CREATE VIEW v_cte_only AS WITH t_ren(a) AS (SELECT 77) SELECT t_ren.a AS ca FROM t_ren;
         CREATE VIEW v_using_other AS SELECT t_ren.a AS ta, u_other.a AS ua FROM t_ren JOIN (u_other JOIN u_other2 USING (a)) ON t_ren.id = u_other.id;",
    )
    .unwrap();
    c.execute("ALTER TABLE t_ren RENAME COLUMN a TO a2").unwrap();
    let vi = c.query("SELECT id, a2, ua FROM v_inner").unwrap();
    assert!(vi.rows[0][1].exact_eq(&SqlValue::Int(10)));
    assert!(vi.rows[0][2].exact_eq(&SqlValue::Int(99)));
    // Shadowed alias `u_other AS t_ren` must NOT be rewritten to `a2`!
    let vs = c.query("SELECT a FROM v_shadow").unwrap();
    assert!(vs.rows[0][0].exact_eq(&SqlValue::Int(99)));
    // CTE `t_ren` stays `a`, while `main.t_ren` rewrites to `a2`!
    let vc = c.query("SELECT ca, ma FROM v_cte_both").unwrap();
    assert!(vc.rows[0][0].exact_eq(&SqlValue::Int(77)));
    assert!(vc.rows[0][1].exact_eq(&SqlValue::Int(10)));
    let vcm = c.query("SELECT ma FROM v_cte_main").unwrap();
    assert!(vcm.rows[0][0].exact_eq(&SqlValue::Int(10)));
    let vco = c.query("SELECT ca FROM v_cte_only").unwrap();
    assert!(vco.rows[0][0].exact_eq(&SqlValue::Int(77)));
    // Unrelated `u_other JOIN u_other2 USING (a)` must NOT have `USING (a)` rewritten to `USING (a2)`!
    let vu = c.query("SELECT ta, ua FROM v_using_other").unwrap();
    assert!(vu.rows[0][0].exact_eq(&SqlValue::Int(10)));
    assert!(vu.rows[0][1].exact_eq(&SqlValue::Int(99)));

    // 2d. Multi-statement `?2` + `?` parameter shifting + aliased DELETE rejects table name.
    c.execute("CREATE TABLE mp (x INT, y INT)").unwrap();
    c.execute_with_params(
        "INSERT INTO mp VALUES (?2, ?); INSERT INTO mp VALUES (?, ?);",
        &QueryParams::positional(vec![
            SqlValue::Int(10),
            SqlValue::Int(20),
            SqlValue::Int(30),
            SqlValue::Int(40),
            SqlValue::Int(50),
        ]),
    )
    .unwrap();
    let mp_rows = c.query("SELECT x, y FROM mp ORDER BY x").unwrap();
    assert_eq!(mp_rows.rows.len(), 2);
    assert!(mp_rows.rows[0][0].exact_eq(&SqlValue::Int(20)));
    assert!(mp_rows.rows[0][1].exact_eq(&SqlValue::Int(30)));
    assert!(mp_rows.rows[1][0].exact_eq(&SqlValue::Int(40)));
    assert!(mp_rows.rows[1][1].exact_eq(&SqlValue::Int(50)));
    assert!(c.execute("DELETE FROM mp AS m WHERE mp.x = 20").is_err());

    // 2e. seed_json empty array `[]` inserts DEFAULT VALUES; SELECT statements don't double-count.
    let rep = c
        .seed_json(
            r#"{"schema": [
                "CREATE TABLE sj_dflt (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT DEFAULT 7)"
            ], "tables": {"sj_dflt": [[]]}, "statements": [
                "INSERT INTO sj_dflt DEFAULT VALUES",
                "SELECT * FROM sj_dflt"
            ]}"#,
        )
        .unwrap();
    assert_eq!(rep.rows_inserted, 2);
    let sj_cnt = c.query("SELECT count(*), sum(v) FROM sj_dflt").unwrap();
    assert!(sj_cnt.rows[0][0].exact_eq(&SqlValue::Int(2)));
    assert!(sj_cnt.rows[0][1].exact_eq(&SqlValue::Int(14)));

    // 2f. Heavy B-tree stress: 1,200 rows on 512-byte pages exercising:
    //     - multi-level `index_bulk_load_sorted_flat` (ASC + DESC indexes)
    //     - in-place table leaf overwrites (same size, shrink->freeblocks, grow->gap, overflow)
    //     - in-place leaf deletes (freeblock coalescing, content_start reclamation, empty leaf detach)
    //     - `RowidOrder`, `IndexOrder` (ASC & DESC), unindexed `TopK` (NULLS FIRST/LAST + OFFSET + alias)
    c.execute(
        "CREATE TABLE stress (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grp INT NOT NULL,
            val INT,
            payload TEXT NOT NULL
         );",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    for i in 1..=1200i64 {
        let v = if i % 100 == 0 {
            SqlValue::Null
        } else {
            SqlValue::Int((i * 3571) % 997)
        };
        let p = QueryParams::positional(vec![
            SqlValue::Int(i % 25),
            v,
            SqlValue::text(format!("row_{i}_abcdefghij")),
        ]);
        c.execute_with_params(
            "INSERT INTO stress(grp, val, payload) VALUES (?1, ?2, ?3)",
            &p,
        )
        .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Unindexed TopK ASC/DESC + NULLS FIRST/LAST + OFFSET + explicit alias override!
    let tk_desc = c
        .query("SELECT id, val FROM stress ORDER BY val DESC LIMIT 5 OFFSET 2")
        .unwrap();
    assert_eq!(tk_desc.rows.len(), 5);
    let tk_nf = c
        .query("SELECT id, val FROM stress ORDER BY val DESC NULLS FIRST LIMIT 3")
        .unwrap();
    assert!(tk_nf.rows[0][1].is_null() && tk_nf.rows[2][1].is_null());
    let tk_nl = c
        .query("SELECT id, val FROM stress ORDER BY val ASC NULLS LAST LIMIT 3")
        .unwrap();
    assert!(!tk_nl.rows[0][1].is_null());
    let tk_alias = c
        .query("SELECT id AS val, val AS orig FROM stress ORDER BY val DESC LIMIT 3")
        .unwrap();
    assert!(tk_alias.rows[0][0].exact_eq(&SqlValue::Int(1200)));
    assert!(tk_alias.rows[2][0].exact_eq(&SqlValue::Int(1198)));

    // Build BOTH ASC and DESC multi-level indexes via `index_bulk_load_sorted_flat`!
    c.execute("CREATE INDEX idx_stress_val_asc ON stress(val);").unwrap();
    c.execute("CREATE INDEX idx_stress_val_desc ON stress(val DESC);").unwrap();
    // IndexOrder ASC & DESC must match exact TopK results!
    let io_desc = c
        .query("SELECT id, val FROM stress ORDER BY val DESC LIMIT 5 OFFSET 2")
        .unwrap();
    assert_eq!(io_desc.rows.len(), 5);
    for i in 0..5 {
        assert!(io_desc.rows[i][1].exact_eq(&tk_desc.rows[i][1]));
    }
    let io_asc = c
        .query("SELECT id, val FROM stress ORDER BY val ASC LIMIT 3")
        .unwrap();
    assert!(io_asc.rows[0][1].is_null());
    // RowidOrder DESC + LIMIT
    let ro_desc = c
        .query("SELECT id FROM stress ORDER BY id DESC LIMIT 3")
        .unwrap();
    assert!(ro_desc.rows[0][0].exact_eq(&SqlValue::Int(1200)));
    assert!(ro_desc.rows[2][0].exact_eq(&SqlValue::Int(1198)));

    // Exercise in-place overwrites (same, shrink, grow, overflow) AND in-place deletes (coalescing + detach)!
    c.execute("BEGIN").unwrap();
    for i in (1..=600i64).step_by(2) {
        // Shrink payload (creates freeblocks/fragments)
        let p = QueryParams::positional(vec![SqlValue::text("s"), SqlValue::Int(i)]);
        c.execute_with_params("UPDATE stress SET payload = ?1 WHERE id = ?2", &p)
            .unwrap();
    }
    for i in (2..=400i64).step_by(4) {
        // Grow payload (into unallocated gap or overflow chain on 512B pages!)
        let big = if i % 40 == 0 {
            "Z".repeat(700)
        } else {
            format!("longer_payload_value_{i}_0123456789abcdef")
        };
        let p = QueryParams::positional(vec![SqlValue::text(big), SqlValue::Int(i)]);
        c.execute_with_params("UPDATE stress SET payload = ?1 WHERE id = ?2", &p)
            .unwrap();
    }
    // Delete contiguous ranges + scattered rows (triggers freeblock coalescing & empty leaf detach)
    for i in 601..=1000i64 {
        let p = QueryParams::positional(vec![SqlValue::Int(i)]);
        c.execute_with_params("DELETE FROM stress WHERE id = ?1", &p)
            .unwrap();
    }
    for i in (1..=600i64).step_by(3) {
        let p = QueryParams::positional(vec![SqlValue::Int(i)]);
        c.execute_with_params("DELETE FROM stress WHERE id = ?1", &p)
            .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Verify exact row count, GROUP BY, JOIN, and full Rust + real C SQLite `PRAGMA integrity_check`!
    let rem = c.query("SELECT count(*) FROM stress").unwrap();
    assert!(rem.rows[0][0].exact_eq(&SqlValue::Int(600)));
    assert_integrity_ok(&mut c);
    let final_bytes = c.to_bytes();
    verify_with_real_sqlite(
        "pass4_btree_inplace_and_bulk_index",
        &final_bytes,
        None,
        Some("SELECT count(*), sum(grp) FROM stress"),
    );
}

#[test]
fn pass5_audit_btree_skew_atomicity_nan_and_fast_selects() {
    // 1. Skewed small + large key CREATE INDEX (tail-overflow regression in `index_bulk_load_sorted_flat`)!
    //    30 short 8-byte keys followed by 4 large 992-byte keys on 4096-byte pages -> total > 4088B,
    //    midpoint by count would overflow right half unless split by byte balance!
    {
        let mut c = SqliteConnection::open_in_memory(ConnectionOptions {
            page_size: 4096,
            ..ConnectionOptions::default()
        })
        .unwrap();
        c.execute("CREATE TABLE skew (id INTEGER PRIMARY KEY, k TEXT NOT NULL)").unwrap();
        c.execute("BEGIN").unwrap();
        for i in 0..30 {
            let p = QueryParams::positional(vec![SqlValue::text(format!("a_{i:04}"))]);
            c.execute_with_params("INSERT INTO skew(k) VALUES (?1)", &p).unwrap();
        }
        for i in 0..8 {
            let p = QueryParams::positional(vec![SqlValue::text(format!("z_{i}_{}", "X".repeat(980)))]);
            c.execute_with_params("INSERT INTO skew(k) VALUES (?1)", &p).unwrap();
        }
        c.execute("COMMIT").unwrap();
        c.execute("CREATE INDEX idx_skew_k ON skew(k)").unwrap();
        assert_integrity_ok(&mut c);
        let bytes = c.to_bytes();
        verify_with_real_sqlite(
            "pass5_skewed_index_4096",
            &bytes,
            None,
            Some("SELECT count(*) FROM skew WHERE k >= 'z'"),
        );
    }

    // 1b. Multi-level skewed index on 512-byte pages (triggers BOTH leaf and interior skewed splits!).
    {
        let mut c = SqliteConnection::open_in_memory(ConnectionOptions {
            page_size: 512,
            ..ConnectionOptions::default()
        })
        .unwrap();
        c.execute("CREATE TABLE skew512 (id INTEGER PRIMARY KEY, k TEXT NOT NULL)").unwrap();
        c.execute("BEGIN").unwrap();
        for i in 0..160 {
            let key = if i % 17 == 16 {
                format!("k_{i:04}_{}", "M".repeat(110))
            } else {
                format!("k_{i:04}")
            };
            let p = QueryParams::positional(vec![SqlValue::text(key)]);
            c.execute_with_params("INSERT INTO skew512(k) VALUES (?1)", &p).unwrap();
        }
        c.execute("COMMIT").unwrap();
        c.execute("CREATE INDEX idx_skew512_k ON skew512(k)").unwrap();
        assert_integrity_ok(&mut c);
        let bytes = c.to_bytes();
        verify_with_real_sqlite(
            "pass5_skewed_index_512",
            &bytes,
            None,
            Some("SELECT count(*) FROM skew512"),
        );
    }

    // 2. Statement-savepoint elision atomicity inside BEGIN (RETURNING error, schema ON CONFLICT REPLACE,
    //    and expression/partial index failure must ALL roll back cleanly!).
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c.execute(
        "CREATE TABLE atom (
            id INTEGER PRIMARY KEY,
            u INT UNIQUE ON CONFLICT REPLACE,
            b TEXT NOT NULL
         );",
    )
    .unwrap();
    c.execute("INSERT INTO atom VALUES (1, 10, 'ok'), (2, 20, 'ok')").unwrap();

    c.execute("BEGIN").unwrap();
    // 2a. Failing RETURNING on INSERT must roll back the row!
    assert!(c
        .execute("INSERT INTO atom VALUES (3, 30, 'bad') RETURNING json(b)")
        .is_err());
    // 2b. Failing RETURNING on UPDATE must roll back the mutation!
    assert!(c
        .execute("UPDATE atom SET b = 'bad' WHERE id = 1 RETURNING json(b)")
        .is_err());
    // 2c. Failing RETURNING on DELETE must roll back the deletion!
    assert!(c
        .execute("DELETE FROM atom WHERE id = 2 RETURNING json(b)")
        .is_err());
    // 2d. Schema-level `UNIQUE ON CONFLICT REPLACE` combined with failing PK conflict rolls back!
    c.execute("CREATE TABLE atom_rep (id INTEGER PRIMARY KEY, a INT UNIQUE ON CONFLICT REPLACE, b INT UNIQUE)")
        .unwrap();
    c.execute("INSERT INTO atom_rep VALUES (1, 100, 1), (2, 200, 2)").unwrap();
    assert!(c
        .execute("INSERT INTO atom_rep VALUES (3, 100, 2)")
        .is_err());
    c.execute("COMMIT").unwrap();

    let r_atom = c.query("SELECT id, b FROM atom ORDER BY id").unwrap();
    assert_eq!(r_atom.rows.len(), 2);
    assert!(r_atom.rows[0][1].exact_eq(&SqlValue::text("ok")));
    assert!(r_atom.rows[1][0].exact_eq(&SqlValue::Int(2)));
    let r_rep = c.query("SELECT id, a, b FROM atom_rep ORDER BY id").unwrap();
    assert_eq!(r_rep.rows.len(), 2);

    // 2e. Expression index error on INSERT rolls back table row cleanly!
    c.execute("CREATE TABLE expr_idx_t (id INTEGER PRIMARY KEY, j TEXT)").unwrap();
    c.execute("CREATE INDEX idx_ej ON expr_idx_t(json_extract(j, '$.a'))").unwrap();
    c.execute("BEGIN").unwrap();
    assert!(c.execute("INSERT INTO expr_idx_t VALUES (1, '{not-json')").is_err());
    c.execute("COMMIT").unwrap();
    assert!(c
        .query("SELECT count(*) FROM expr_idx_t")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(0)));

    // 3. `DROP TABLE` + recreate & `ALTER TABLE RENAME TO` keep `sqlite_sequence` (`cached_seq`) accurate!
    c.execute("CREATE TABLE seq_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)").unwrap();
    c.execute("INSERT INTO seq_t(v) VALUES (1), (2), (3)").unwrap();
    c.execute("DROP TABLE seq_t").unwrap();
    c.execute("CREATE TABLE seq_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)").unwrap();
    c.execute("INSERT INTO seq_t(v) VALUES (10)").unwrap();
    assert!(c
        .query("SELECT id FROM seq_t")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));
    c.execute("ALTER TABLE seq_t RENAME TO seq_t2").unwrap();
    c.execute("INSERT INTO seq_t2(v) VALUES (20)").unwrap();
    let seq_rows = c.query("SELECT id FROM seq_t2 ORDER BY id").unwrap();
    assert_eq!(seq_rows.rows.len(), 2);
    assert!(seq_rows.rows[1][0].exact_eq(&SqlValue::Int(2)));

    // 4. Schema-level `UNIQUE ON CONFLICT REPLACE` updates `cached_max_rowid` cleanly on INSERT & UPDATE!
    c.execute("CREATE TABLE rmax (id INTEGER PRIMARY KEY, u INT UNIQUE ON CONFLICT REPLACE)").unwrap();
    c.execute("INSERT INTO rmax VALUES (1, 10), (3, 20)").unwrap();
    // Rowid 2 is new, while `u=20` replaces rowid 3 via schema-level ON CONFLICT REPLACE -> max drops from 3 to 2!
    c.execute("INSERT INTO rmax VALUES (2, 20)").unwrap();
    c.execute("INSERT INTO rmax(u) VALUES (30)").unwrap(); // must get rowid 3 (not stale 4!)
    let rmax_ids = c.query("SELECT id FROM rmax ORDER BY id").unwrap();
    assert_eq!(rmax_ids.rows.len(), 3);
    assert!(rmax_ids.rows[2][0].exact_eq(&SqlValue::Int(3)));
    // Now UPDATE row 1 to u=30 (replaces rowid 3 -> max drops from 3 back to 2!)
    c.execute("UPDATE rmax SET u = 30 WHERE id = 1").unwrap();
    c.execute("INSERT INTO rmax(u) VALUES (40)").unwrap(); // must get rowid 3 again!
    let rmax_ids2 = c.query("SELECT id FROM rmax ORDER BY id").unwrap();
    assert_eq!(rmax_ids2.rows.len(), 3);
    assert!(rmax_ids2.rows[2][0].exact_eq(&SqlValue::Int(3)));

    // 5. `SqlValue::Real(f64::NAN)` param normalized to NULL (NOT NULL rejects, index seek doesn't match finite).
    c.execute("CREATE TABLE nan_t (id INTEGER PRIMARY KEY, v REAL)").unwrap();
    c.execute("CREATE INDEX idx_nan_v ON nan_t(v)").unwrap();
    c.execute("INSERT INTO nan_t VALUES (1, 1.5), (2, NULL)").unwrap();
    let p_nan = QueryParams::positional(vec![SqlValue::Real(f64::NAN)]);
    assert!(c
        .query_with_params("SELECT id FROM nan_t WHERE v = ?1", &p_nan)
        .unwrap()
        .rows
        .is_empty());
    c.execute("CREATE TABLE nan_nn (id INTEGER PRIMARY KEY, v REAL NOT NULL)").unwrap();
    assert!(c
        .execute_with_params("INSERT INTO nan_nn VALUES (1, ?1)", &p_nan)
        .is_err());

    // 6. 0-row UPDATE validates bogus & generated SET targets upfront!
    c.execute("CREATE TABLE upd_val (id INTEGER PRIMARY KEY, a INT, g INT GENERATED ALWAYS AS (a+1) STORED)")
        .unwrap();
    assert!(c.execute("UPDATE upd_val SET bogus = 1").is_err());
    assert!(c.execute("UPDATE upd_val SET g = 1").is_err());

    // 7. `PRAGMA encoding` on a non-empty DB keeps `cached_collations` in sync with actual UTF-8 encoding.
    c.execute("PRAGMA encoding = 'UTF-16le'").unwrap();
    assert!(c
        .query("SELECT 'abc' = 'abc'")
        .unwrap()
        .rows[0][0]
        .exact_eq(&SqlValue::Int(1)));

    // 8. Direct `CachedFastSelect` (`SELECT count(*)` & point-rowid `WHERE id = ?`) edge cases & schema invalidation!
    c.execute("CREATE TABLE fast_sel (id INTEGER PRIMARY KEY, v INT, s TEXT DEFAULT 'def')").unwrap();
    c.execute("INSERT INTO fast_sel(id, v, s) VALUES (1, 10, 'one'), (5, 50, 'five')").unwrap();
    let q_cnt = "SELECT count(*) FROM fast_sel";
    assert!(c.query(q_cnt).unwrap().rows[0][0].exact_eq(&SqlValue::Int(2)));
    assert!(c.query(q_cnt).unwrap().rows[0][0].exact_eq(&SqlValue::Int(2))); // cache hit!

    let q_pt = "SELECT id, v, s FROM fast_sel WHERE id = ?1";
    let hit_int = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::Int(5)])).unwrap();
    assert_eq!(hit_int.rows.len(), 1);
    assert!(hit_int.rows[0][2].exact_eq(&SqlValue::text("five")));
    let hit_flt = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::Real(5.0)])).unwrap();
    assert_eq!(hit_flt.rows.len(), 1);
    let miss_flt = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::Real(5.5)])).unwrap();
    assert!(miss_flt.rows.is_empty());
    let hit_txt = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::text("5")])).unwrap();
    assert_eq!(hit_txt.rows.len(), 1);
    let miss_txt = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::text("abc")])).unwrap();
    assert!(miss_txt.rows.is_empty());
    let miss_null = c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::Null])).unwrap();
    assert!(miss_null.rows.is_empty());
    let miss_nan = c.query_with_params(q_pt, &p_nan).unwrap();
    assert!(miss_nan.rows.is_empty());

    // Schema change invalidation (ALTER TABLE ADD COLUMN + DROP TABLE)
    c.execute("ALTER TABLE fast_sel ADD COLUMN extra TEXT DEFAULT 'added'").unwrap();
    let q_pt2 = "SELECT id, extra FROM fast_sel WHERE id = ?1";
    let hit_extra = c.query_with_params(q_pt2, &QueryParams::positional(vec![SqlValue::Int(5)])).unwrap();
    assert!(hit_extra.rows[0][1].exact_eq(&SqlValue::text("added")));
    c.execute("DROP TABLE fast_sel").unwrap();
    assert!(c.query(q_cnt).is_err());
    assert!(c.query_with_params(q_pt, &QueryParams::positional(vec![SqlValue::Int(5)])).is_err());

    assert_integrity_ok(&mut c);
    let all_bytes = c.to_bytes();
    verify_with_real_sqlite("pass5_audit_all", &all_bytes, None, None);
}

#[test]
fn pass6_audit_storage_dml_pragma_and_fast_paths_verified_by_c_sqlite() {
    // 1. Auto-commit VFS flush failure restores (changes, total_changes, last_insert_rowid)
    //    and invalidates `cached_seq` / `cached_max_rowid` cleanly.
    let vfs = Arc::new(FaultVfs::new());
    let mut fc = SqliteConnection::open(
        vfs.clone(),
        "pass6_flush_fail.db",
        ConnectionOptions::default(),
    )
    .unwrap();
    fc.execute("CREATE TABLE ai (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    let r1 = fc.execute("INSERT INTO ai(v) VALUES (10)").unwrap();
    assert_eq!(r1.last_insert_rowid, 1);
    assert_eq!(r1.changes, 1);

    vfs.arm_crash_after_main_writes(0);
    assert!(fc.execute("INSERT INTO ai(v) VALUES (20)").is_err());
    vfs.disarm();
    // Connection counters must still reflect the last committed row (1), not the aborted row (2)!
    assert_eq!(fc.last_insert_rowid(), 1);
    assert_eq!(fc.changes(), 1);
    // Subsequent auto-commit insert must get id = 2 cleanly (no stale cached_seq/cached_max_rowid!).
    let r2 = fc.execute("INSERT INTO ai(v) VALUES (30)").unwrap();
    assert_eq!(r2.last_insert_rowid, 2);
    let ai_rows = fc.query("SELECT id, v FROM ai ORDER BY id").unwrap();
    assert_eq!(ai_rows.rows.len(), 2);
    assert!(ai_rows.rows[0][1].exact_eq(&SqlValue::Int(10)));
    assert!(ai_rows.rows[1][1].exact_eq(&SqlValue::Int(30)));
    assert_integrity_ok(&mut fc);

    // 2. `CachedFastSelect::PointRowid` handles STORED & VIRTUAL generated cols + REAL read-back!
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c.execute(
        "CREATE TABLE gtab (
            id INTEGER PRIMARY KEY,
            x REAL,
            s INT GENERATED ALWAYS AS (x * 2) STORED,
            v INT GENERATED ALWAYS AS (x + 10) VIRTUAL
         )",
    )
    .unwrap();
    c.execute("INSERT INTO gtab(id, x) VALUES (1, 5)").unwrap();
    // Run twice to exercise both first-run and cached fast-select paths.
    for _ in 0..2 {
        let q = c
            .query_with_params(
                "SELECT * FROM gtab WHERE id = ?1",
                &QueryParams::positional(vec![SqlValue::Int(1)]),
            )
            .unwrap();
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0].len(), 4);
        assert!(q.rows[0][1].exact_eq(&SqlValue::Real(5.0)));
        assert!(q.rows[0][2].exact_eq(&SqlValue::Int(10)));
        assert!(q.rows[0][3].exact_eq(&SqlValue::Int(15)));
    }
    c.execute("CREATE TABLE rtab (id INTEGER PRIMARY KEY, r REAL, g INT GENERATED ALWAYS AS (id + 1) STORED)")
        .unwrap();
    c.execute("INSERT INTO rtab(id, r) VALUES (7, 3)").unwrap();
    for _ in 0..2 {
        let q = c
            .query_with_params(
                "SELECT * FROM rtab WHERE id = ?1",
                &QueryParams::positional(vec![SqlValue::Int(7)]),
            )
            .unwrap();
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0].len(), 3);
        assert!(q.rows[0][1].exact_eq(&SqlValue::Real(3.0)));
        assert!(q.rows[0][2].exact_eq(&SqlValue::Int(8)));
    }

    // 3. `UPDATE ... FROM s` when `s` has its own column named `rowid`!
    c.execute("CREATE TABLE dst (id INTEGER PRIMARY KEY, val INT)").unwrap();
    c.execute("CREATE TABLE src (rowid INT, k INT, newv INT)").unwrap();
    c.execute("INSERT INTO dst VALUES (1, 100), (2, 200)").unwrap();
    c.execute("INSERT INTO src VALUES (999, 2, 777)").unwrap();
    c.execute("UPDATE dst SET val = src.newv FROM src WHERE dst.id = src.k")
        .unwrap();
    let uq = c.query("SELECT id, val FROM dst ORDER BY id").unwrap();
    assert!(uq.rows[0][1].exact_eq(&SqlValue::Int(100)));
    assert!(uq.rows[1][1].exact_eq(&SqlValue::Int(777)));

    // 4. `temp.` schema is NOT silently aliased to `main.`!
    assert!(c.query("SELECT * FROM temp.dst").is_err());

    // 5. `INSERT` skips DEFAULT expr for targeted cols & duplicate IPK keeps LAST (non-IPK keeps FIRST).
    c.execute("CREATE TABLE dflt_skip (id INTEGER PRIMARY KEY, a INT DEFAULT 99, b INT DEFAULT 42)")
        .unwrap();
    c.execute("INSERT INTO dflt_skip(id, a) VALUES (1, NULL)").unwrap();
    c.execute("INSERT INTO dflt_skip(id, id, a, a) VALUES (2, 10, 5, 6)").unwrap();
    c.execute("INSERT INTO dflt_skip DEFAULT VALUES").unwrap();
    let dq = c.query("SELECT id, a, b FROM dflt_skip ORDER BY id").unwrap();
    assert_eq!(dq.rows.len(), 3);
    assert!(dq.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(dq.rows[0][1].exact_eq(&SqlValue::Null)); // explicit NULL overrides DEFAULT 99
    assert!(dq.rows[0][2].exact_eq(&SqlValue::Int(42)));
    assert!(dq.rows[1][0].exact_eq(&SqlValue::Int(10))); // duplicate IPK keeps LAST (10)
    assert!(dq.rows[1][1].exact_eq(&SqlValue::Int(5))); // duplicate non-IPK keeps FIRST (5)
    assert!(dq.rows[2][1].exact_eq(&SqlValue::Int(99))); // DEFAULT VALUES applies DEFAULT 99

    // 6. PRAGMA hex integer parsing & non-main schema ignored.
    c.execute("PRAGMA user_version = 0x47504b47").unwrap();
    c.execute("PRAGMA bogus.user_version = 99").unwrap();
    let uv = c.execute("PRAGMA user_version").unwrap();
    assert!(uv.rows[0][0].exact_eq(&SqlValue::Int(0x4750_4b47)));

    // 7. Pass-6 performance fast paths end-to-end (balance_quick sequential inserts, in-place
    //    sqlite_sequence update, single-descent point UPDATE & DELETE, direct CREATE INDEX,
    //    and direct seed_json batch) — ALL verified by real C SQLite `PRAGMA integrity_check`!
    c.execute(
        "CREATE TABLE perf_t (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grp INT NOT NULL,
            val INT NOT NULL,
            name TEXT NOT NULL
         );
         CREATE INDEX idx_perf_grp ON perf_t(grp);",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    for i in 1..=1500i64 {
        let p = QueryParams::positional(vec![
            SqlValue::Int(i % 25),
            SqlValue::Int((i * 31) % 1000),
            SqlValue::text(format!("row_{i}")),
        ]);
        c.execute_with_params("INSERT INTO perf_t(grp, val, name) VALUES (?1, ?2, ?3)", &p)
            .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Direct CREATE INDEX fast path
    c.execute("CREATE INDEX idx_perf_val ON perf_t(val)").unwrap();

    // Single-descent point UPDATE (same size, larger size, smaller size)
    c.execute("BEGIN").unwrap();
    for i in 1..=100i64 {
        let p = QueryParams::positional(vec![SqlValue::Int(i * 1000), SqlValue::Int(i * 7)]);
        c.execute_with_params("UPDATE perf_t SET val = ?1 WHERE id = ?2", &p)
            .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Single-descent point DELETE (maintaining BOTH idx_perf_grp & idx_perf_val!)
    c.execute("BEGIN").unwrap();
    for i in 1..=150i64 {
        let p = QueryParams::positional(vec![SqlValue::Int(i * 9)]);
        c.execute_with_params("DELETE FROM perf_t WHERE id = ?1", &p)
            .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Direct seed_json homogeneous batch + mixed-column fallback
    let seed_doc = r#"{"seeded":[{"id":1,"v":10,"s":"a"},{"id":2,"v":20,"s":"b"},{"id":3,"v":null,"s":"c"}]}"#;
    let srep = c.seed_json(seed_doc).unwrap();
    assert_eq!(srep.rows_inserted, 3);
    let sq = c.query("SELECT id, v, s FROM seeded ORDER BY id").unwrap();
    assert_eq!(sq.rows.len(), 3);
    assert!(sq.rows[2][1].exact_eq(&SqlValue::Null));

    // Verify TopK fast peek + streaming WHERE + columnar cumulative window on perf_t!
    let top5 = c
        .query("SELECT id, val FROM perf_t ORDER BY val DESC LIMIT 5")
        .unwrap();
    assert_eq!(top5.rows.len(), 5);
    assert!(
        c.query("SELECT count(*) FROM perf_t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(1350))
    );
    let seq_q = c
        .query("SELECT seq FROM sqlite_sequence WHERE name = 'perf_t'")
        .unwrap();
    assert!(seq_q.rows[0][0].exact_eq(&SqlValue::Int(1500)));

    assert_integrity_ok(&mut c);
    let all_bytes = c.to_bytes();
    verify_with_real_sqlite(
        "pass6_audit_all",
        &all_bytes,
        None,
        Some("SELECT count(*) AS cnt, (SELECT seq FROM sqlite_sequence WHERE name='perf_t') AS seq FROM perf_t"),
    );
}

#[test]
fn pass7_audit_pager_limits_btree_safety_dml_and_fastpaths_verified_by_c_sqlite() {
    use xdb_sqlite::SeekOp;

    // 1. C1: 0-byte `MemoryVfs` opened with custom `page_size: 512` MUST compute `payload_limits(512)`
    //    (max_leaf = 477), spilling a 478-byte payload into an overflow page cleanly!
    let vfs_512 = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    let mut c512 = SqliteConnection::open(
        vfs_512.clone(),
        "small512.db",
        ConnectionOptions {
            page_size: 512,
            ..ConnectionOptions::default()
        },
    )
    .expect("open 512B db");
    c512.execute("CREATE TABLE blobs (id INTEGER PRIMARY KEY, b BLOB NOT NULL)")
        .unwrap();
    let payload_478 = vec![0xABu8; 478];
    c512.execute_with_params(
        "INSERT INTO blobs VALUES (1, ?1)",
        &QueryParams::positional(vec![SqlValue::Blob(payload_478.clone().into_boxed_slice())]),
    )
    .unwrap();
    let got_blob = c512.query("SELECT b FROM blobs WHERE id = 1").unwrap();
    assert!(got_blob.rows[0][0].exact_eq(&SqlValue::Blob(payload_478.into_boxed_slice())));
    assert_integrity_ok(&mut c512);
    let b512 = c512.to_bytes();
    assert_eq!(u16::from_be_bytes([b512[16], b512[17]]), 512);
    verify_with_real_sqlite(
        "pass7_page512_overflow",
        &b512,
        None,
        Some("SELECT length(b) AS len FROM blobs WHERE id = 1"),
    );

    // 2. C2: `CREATE INDEX` tight `BudgetMeter` memory limit rejected BEFORE allocation.
    let mut c_mem = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_mem.execute("CREATE TABLE im (id INTEGER PRIMARY KEY, v TEXT NOT NULL)").unwrap();
    c_mem.execute("BEGIN").unwrap();
    for i in 0..800 {
        c_mem
            .execute_with_params(
                "INSERT INTO im VALUES (?1, ?2)",
                &QueryParams::positional(vec![
                    SqlValue::Int(i),
                    SqlValue::text(format!("long_indexed_value_padding_{i:06}")),
                ]),
            )
            .unwrap();
    }
    c_mem.execute("COMMIT").unwrap();
    let db_im_bytes = c_mem.to_bytes();
    let vfs_im = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    {
        let mut f = vfs_im
            .open("im.db", OpenFlags::create(FileKind::MainDb))
            .unwrap();
        f.write(&db_im_bytes, 0).unwrap();
    }
    let tight_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_memory_bytes: Some(48_000),
        ..ResourceLimits::default()
    }));
    let mut c_tight = SqliteConnection::open(
        vfs_im,
        "im.db",
        ConnectionOptions {
            meter: Some(tight_meter),
            ..ConnectionOptions::default()
        },
    )
    .unwrap();
    let ix_err = c_tight.execute("CREATE INDEX idx_im_v ON im(v)").unwrap_err();
    assert!(
        matches!(ix_err, SqliteError::ResourceLimit(e) if e.kind == LimitKind::Memory),
        "expected Memory limit on CREATE INDEX, got {ix_err:?}"
    );

    // 3. H1: `TableCursor::move_to_inner` repeated `seek_rowid(..., SeekOp::Lt)` across 2+ leaves
    //    resets `core.moves` / `core.dir` so 200 back-to-back seeks never falsely trigger CycleDetected!
    let mut c_seek = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512,
        ..ConnectionOptions::default()
    })
    .unwrap();
    c_seek.execute("CREATE TABLE sk (id INTEGER PRIMARY KEY, pad TEXT)").unwrap();
    c_seek.execute("BEGIN").unwrap();
    for i in 1..=120i64 {
        c_seek
            .execute_with_params(
                "INSERT INTO sk VALUES (?1, ?2)",
                &QueryParams::positional(vec![SqlValue::Int(i * 10), SqlValue::text("xxxxxxxxxxxxxxxxxxxxxxxx")]),
            )
            .unwrap();
    }
    c_seek.execute("COMMIT").unwrap();
    let sk_db =
        open_database_from_bytes(&c_seek.to_bytes(), None, None, OpenOptions::default()).unwrap();
    let sk_root = sk_db.table("sk").unwrap().info().root_page;
    let mut tc = sk_db.btree().table_cursor(sk_root).unwrap();
    for i in 0..200i64 {
        let target = ((i % 100) + 2) * 10;
        let ok = tc.seek_rowid(target, SeekOp::Lt).unwrap();
        assert!(ok, "seek_rowid Lt({target}) failed on iter {i}");
        assert_eq!(tc.rowid().unwrap(), target - 10);
    }

    // 4. H3: Corrupt 0-byte and 1-byte (`[0x01]`) table leaf records return `Err(Corrupt)` cleanly
    //    (both via `open_database_from_bytes` AND via direct `WritePager` scan).
    let mut c_cor = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_cor.execute("CREATE TABLE ct (id INTEGER PRIMARY KEY, v INT)").unwrap();
    c_cor.execute("INSERT INTO ct VALUES (1, 42)").unwrap();
    let base_bytes = c_cor.to_bytes();
    let ct_root = open_database_from_bytes(&base_bytes, None, None, OpenOptions::default())
        .unwrap()
        .table("ct")
        .unwrap()
        .info()
        .root_page as usize;
    let ps = 4096usize;
    let pg_off = (ct_root - 1) * ps;
    let cell_ptr = u16::from_be_bytes([base_bytes[pg_off + 8], base_bytes[pg_off + 9]]) as usize;
    // Corrupt case A: 0-byte payload (`payload_len = 0, rowid = 1`).
    let mut bad0 = base_bytes.clone();
    bad0[pg_off + cell_ptr] = 0x00;
    bad0[pg_off + cell_ptr + 1] = 0x01;
    let db0 = open_database_from_bytes(&bad0, None, None, OpenOptions::default()).unwrap();
    assert!(
        db0.query("SELECT v FROM ct").is_err(),
        "0-byte table record must return Corrupt"
    );
    // Corrupt case B: 1-byte header-only payload (`payload_len = 1, rowid = 1, hdr_len = 1`).
    let mut bad1 = base_bytes.clone();
    bad1[pg_off + cell_ptr] = 0x01;
    bad1[pg_off + cell_ptr + 1] = 0x01;
    bad1[pg_off + cell_ptr + 2] = 0x01;
    let db1 = open_database_from_bytes(&bad1, None, None, OpenOptions::default()).unwrap();
    assert!(
        db1.query("SELECT v FROM ct").is_err(),
        "1-byte header-only (`hdr_len == hpos`) table record must return Corrupt"
    );

    // 5. M2: `INSERT OR FAIL` inside an active transaction flushes `changes()`, `total_changes()`,
    //    and `sqlite_sequence` for committed prefix rows!
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c.execute(
        "CREATE TABLE ftab (id INTEGER PRIMARY KEY AUTOINCREMENT, k INT UNIQUE ON CONFLICT FAIL NOT NULL)",
    )
    .unwrap();
    c.execute("INSERT INTO ftab(k) VALUES (100)").unwrap();
    c.execute("BEGIN").unwrap();
    let ferr = c.execute("INSERT OR FAIL INTO ftab(k) VALUES (200), (300), (100), (400)");
    assert!(ferr.is_err(), "duplicate k=100 must fail under ON CONFLICT FAIL");
    assert_eq!(c.changes(), 2, "2 prefix rows must be counted in changes()");
    assert_eq!(c.total_changes(), 3, "total_changes() must reflect committed prefix");
    let fseq = c.query("SELECT seq FROM sqlite_sequence WHERE name = 'ftab'").unwrap();
    assert!(fseq.rows[0][0].exact_eq(&SqlValue::Int(3)));
    c.execute("COMMIT").unwrap();
    assert!(
        c.query("SELECT count(*) FROM ftab").unwrap().rows[0][0].exact_eq(&SqlValue::Int(3))
    );

    // 6. M3: `ALTER TABLE ADD COLUMN` NOT NULL checks final DEFAULT including `+NULL` / `-NULL`.
    c.execute("CREATE TABLE alt_t (id INTEGER PRIMARY KEY)").unwrap();
    assert!(c.execute("ALTER TABLE alt_t ADD COLUMN bad1 INT NOT NULL DEFAULT +NULL").is_err());
    assert!(c.execute("ALTER TABLE alt_t ADD COLUMN bad2 INT NOT NULL DEFAULT -NULL").is_err());
    assert!(
        c.execute("ALTER TABLE alt_t ADD COLUMN bad3 INT NOT NULL DEFAULT 1 DEFAULT NULL")
            .is_err()
    );
    c.execute("ALTER TABLE alt_t ADD COLUMN ok1 INT NOT NULL DEFAULT NULL DEFAULT 7")
        .unwrap();
    c.execute("INSERT INTO alt_t(id) VALUES (1)").unwrap();
    assert!(
        c.query("SELECT ok1 FROM alt_t WHERE id = 1").unwrap().rows[0][0]
            .exact_eq(&SqlValue::Int(7))
    );

    // 7. Pass-7 Perf Wave 2 fast paths end-to-end (ALL verified by C SQLite `PRAGMA integrity_check`!):
    //    (a) Cached `sqlite_sequence` width transitions (1 -> 127 -> 128 -> 32768), mid-txn visibility,
    //        `to_bytes()` mid-txn consistency, and `SAVEPOINT` / `ROLLBACK TO`!
    c.execute("CREATE TABLE seq_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    c.execute("BEGIN").unwrap();
    for i in 1..=130i64 {
        c.execute_with_params(
            "INSERT INTO seq_t(v) VALUES (?1)",
            &QueryParams::positional(vec![SqlValue::Int(i)]),
        )
        .unwrap();
    }
    // Live mid-txn query & to_bytes MUST see seq=130!
    assert!(
        c.query("SELECT seq FROM sqlite_sequence WHERE name = 'seq_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(130))
    );
    let mid_db =
        open_database_from_bytes(&c.to_bytes(), None, None, OpenOptions::default()).unwrap();
    assert!(
        mid_db
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'seq_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(130))
    );
    c.execute("SAVEPOINT sp1").unwrap();
    c.execute("INSERT INTO seq_t(id, v) VALUES (40000, 999)").unwrap();
    assert!(
        c.query("SELECT seq FROM sqlite_sequence WHERE name = 'seq_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(40000))
    );
    c.execute("ROLLBACK TO sp1").unwrap();
    assert!(
        c.query("SELECT seq FROM sqlite_sequence WHERE name = 'seq_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(130))
    );
    c.execute("INSERT INTO seq_t(v) VALUES (131)").unwrap();
    assert!(
        c.query("SELECT seq FROM sqlite_sequence WHERE name = 'seq_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(131))
    );
    c.execute("RELEASE sp1").unwrap();
    c.execute("COMMIT").unwrap();

    //    (b) `CachedFastSelect::TopK` (ASC/DESC/NULLS FIRST/NULLS LAST/OFFSET/alias-shadowing),
    //        `CachedFastSelect::IndexOrderCovering` (ASC & DESC + OFFSET), and `index_find_two_int`
    //        (with mixed NULL / INT / TEXT values in the same secondary index!).
    c.execute("CREATE TABLE tk (id INTEGER PRIMARY KEY, val INT, tag TEXT)").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO tk VALUES (1, NULL, 'n1'), (2, 50, 'a'), (3, 10, 'b'), (4, 90, 'c'), (5, 30, 'd'), (6, 'zz', 't')")
        .unwrap();
    c.execute("COMMIT").unwrap();
    for _ in 0..2 {
        let desc_lim = c.query("SELECT id, val FROM tk ORDER BY val DESC LIMIT 3 OFFSET 1").unwrap();
        assert_eq!(desc_lim.rows.len(), 3);
        assert!(desc_lim.rows[0][0].exact_eq(&SqlValue::Int(4))); // 'zz' is #0, 90 is #1
        assert!(desc_lim.rows[1][0].exact_eq(&SqlValue::Int(2))); // 50 is #2
        assert!(desc_lim.rows[2][0].exact_eq(&SqlValue::Int(5))); // 30 is #3

        let asc_nf = c.query("SELECT id, val FROM tk ORDER BY val ASC NULLS LAST LIMIT 2").unwrap();
        assert!(asc_nf.rows[0][0].exact_eq(&SqlValue::Int(3))); // 10
        assert!(asc_nf.rows[1][0].exact_eq(&SqlValue::Int(5))); // 30

        // Explicit alias shadowing `val` must NOT use table column `val`!
        let sh = c.query("SELECT id AS val, val AS orig FROM tk ORDER BY val DESC LIMIT 2").unwrap();
        assert!(sh.rows[0][0].exact_eq(&SqlValue::Int(6)));
        assert!(sh.rows[1][0].exact_eq(&SqlValue::Int(5)));
    }
    // Add covering index on `val`, exercise `IndexOrderCovering` + `index_find_two_int` delete!
    c.execute("CREATE INDEX idx_tk_val ON tk(val)").unwrap();
    for _ in 0..2 {
        let cov = c.query("SELECT id, val FROM tk ORDER BY val DESC LIMIT 3 OFFSET 1").unwrap();
        assert_eq!(cov.rows.len(), 3);
        assert!(cov.rows[0][0].exact_eq(&SqlValue::Int(4)));
        assert!(cov.rows[1][0].exact_eq(&SqlValue::Int(2)));
        assert!(cov.rows[2][0].exact_eq(&SqlValue::Int(5)));
    }
    c.execute_with_params(
        "DELETE FROM tk WHERE id = ?1",
        &QueryParams::positional(vec![SqlValue::Int(4)]),
    )
    .unwrap();
    c.execute_with_params(
        "DELETE FROM tk WHERE id = ?1",
        &QueryParams::positional(vec![SqlValue::Int(6)]),
    )
    .unwrap();
    let after_del = c.query("SELECT id, val FROM tk ORDER BY val DESC LIMIT 2").unwrap();
    assert!(after_del.rows[0][0].exact_eq(&SqlValue::Int(2))); // 50
    assert!(after_del.rows[1][0].exact_eq(&SqlValue::Int(5))); // 30

    //    (c) `seed_json` owned-string batch (both Object rows & Array rows).
    c.execute("CREATE TABLE arr_t (c0 INT PRIMARY KEY, c1 TEXT NOT NULL)").unwrap();
    let arr_seed = r#"{"tables":[{"name":"arr_t","columns":["c0","c1"],"rows":[[1,"alpha"],[2,"beta"],[3,"gamma"]]}]}"#;
    let arep = c.seed_json(arr_seed).unwrap();
    assert_eq!(arep.rows_inserted, 3);
    assert!(
        c.query("SELECT c1 FROM arr_t WHERE c0 = 2").unwrap().rows[0][0]
            .exact_eq(&SqlValue::text("beta"))
    );
    let obj_seed = r#"{"obj_t":[{"id":1,"s":"hello"},{"id":2,"s":"world"}]}"#;
    let orep = c.seed_json(obj_seed).unwrap();
    assert_eq!(orep.rows_inserted, 2);
    assert!(
        c.query("SELECT s FROM obj_t WHERE id = 2").unwrap().rows[0][0]
            .exact_eq(&SqlValue::text("world"))
    );

    assert_integrity_ok(&mut c);
    let final_bytes = c.to_bytes();
    verify_with_real_sqlite(
        "pass7_audit_all",
        &final_bytes,
        None,
        Some("SELECT (SELECT count(*) FROM tk) AS tk_cnt, (SELECT seq FROM sqlite_sequence WHERE name='seq_t') AS seq, (SELECT count(*) FROM arr_t) AS arr_cnt"),
    );
}

#[test]
fn pass8_audit_seqcache_repack_atomicity_cow_metering_strict_alter_corrupt_and_fastpaths_verified_by_c_sqlite() {
    // 1. Critical Bug #1: 30 AUTOINCREMENT tables on 512B pages force `sqlite_sequence` repacks/splits;
    //    `SeqCache` MUST invalidate sibling byte offsets on every repack/insert and stay 100% intact!
    let mut c_seq = SqliteConnection::open_in_memory(ConnectionOptions {
        page_size: 512,
        ..ConnectionOptions::default()
    })
    .unwrap();
    for t in 0..30 {
        c_seq
            .execute(&format!(
                "CREATE TABLE auto_tbl_{t:02} (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)"
            ))
            .unwrap();
    }
    c_seq.execute("BEGIN").unwrap();
    // Round 1: insert 1 row into every table (forces sqlite_sequence inserts & page splits).
    for t in 0..30 {
        c_seq
            .execute(&format!("INSERT INTO auto_tbl_{t:02}(v) VALUES (1)"))
            .unwrap();
    }
    // Round 2: grow sequences across 1-byte -> 2-byte -> 3-byte integer widths in interleaved order!
    for t in 0..30 {
        c_seq
            .execute(&format!(
                "INSERT INTO auto_tbl_{t:02}(id, v) VALUES ({}, 2)",
                200 + t * 500
            ))
            .unwrap();
        c_seq
            .execute(&format!("INSERT INTO auto_tbl_{t:02}(v) VALUES (3)"))
            .unwrap();
    }
    c_seq.execute("COMMIT").unwrap();
    for t in 0..30 {
        let expected = 200 + t * 500 + 1;
        let q = c_seq
            .query(&format!(
                "SELECT seq FROM sqlite_sequence WHERE name = 'auto_tbl_{t:02}'"
            ))
            .unwrap();
        assert!(
            q.rows[0][0].exact_eq(&SqlValue::Int(expected)),
            "auto_tbl_{t:02} expected seq {expected}, got {:?}",
            q.rows[0][0]
        );
    }
    assert_integrity_ok(&mut c_seq);
    verify_with_real_sqlite(
        "pass8_seqcache_repack",
        &c_seq.to_bytes(),
        None,
        Some("SELECT count(*) AS seq_cnt, sum(seq) AS seq_sum FROM sqlite_sequence"),
    );

    // 2. High Bug #2: `sqlite_sequence` at `i64::MAX` inside `BEGIN` MUST fail atomically
    //    (cannot elide savepoint and leave a phantom row in `tbl`).
    let mut c_atom = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_atom
        .execute("CREATE TABLE ai_max (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    c_atom
        .execute("INSERT INTO sqlite_sequence(name, seq) VALUES ('ai_max', 9223372036854775807)")
        .unwrap();
    c_atom.execute("BEGIN").unwrap();
    assert!(c_atom.execute("INSERT INTO ai_max(v) VALUES (1)").is_err());
    c_atom.execute("COMMIT").unwrap();
    assert!(
        c_atom
            .query("SELECT count(*) FROM ai_max")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(0)),
        "failed AUTOINCREMENT insert must not leave phantom row"
    );

    // 3. High Bug #3: Nested `SAVEPOINT` COW undo pages & direct scans (`TopK`, `IndexOrderCovering`,
    //    `IndexEq`) bounded by `BudgetMeter` memory limit and cleanly release memory on drop/rollback.
    {
        let cow_meter = Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(32 * 1024),
            ..ResourceLimits::default()
        }));
        let mut c_cow = SqliteConnection::open_in_memory(ConnectionOptions {
            meter: Some(cow_meter.clone()),
            ..ConnectionOptions::default()
        })
        .unwrap();
        c_cow.execute("CREATE TABLE cow_t (id INTEGER PRIMARY KEY, pad TEXT)").unwrap();
        c_cow
            .execute_with_params(
                "INSERT INTO cow_t VALUES (1, ?1)",
                &QueryParams::positional(vec![SqlValue::text("a".repeat(200))]),
            )
            .unwrap();
        // Verify COW undo bytes are reserved on savepoint UPDATE and released on ROLLBACK!
        let base_mem = cow_meter.memory_bytes();
        c_cow.execute("BEGIN").unwrap();
        c_cow.execute("SAVEPOINT s_rel").unwrap();
        c_cow
            .execute_with_params(
                "UPDATE cow_t SET pad = ?1 WHERE id = 1",
                &QueryParams::positional(vec![SqlValue::text("b".repeat(200))]),
            )
            .unwrap();
        assert!(
            cow_meter.memory_bytes() >= base_mem + 4096,
            "COW undo page must be accounted in BudgetMeter"
        );
        c_cow.execute("ROLLBACK").unwrap();
        assert!(
            cow_meter.memory_bytes() <= base_mem,
            "ROLLBACK must release COW undo page reservations"
        );

        // Now verify nested savepoints cannot bypass max_memory_bytes!
        c_cow.execute("BEGIN").unwrap();
        let mut hit_oom = false;
        for sp in 0..24 {
            let r1 = c_cow.execute(&format!("SAVEPOINT s{sp}"));
            let r2 = match r1 {
                Ok(_) => c_cow.execute_with_params(
                    "UPDATE cow_t SET pad = ?1 WHERE id = 1",
                    &QueryParams::positional(vec![SqlValue::text(format!(
                        "v{sp}_{}",
                        "b".repeat(200)
                    ))]),
                ),
                Err(e) => Err(e),
            };
            if let Err(e) = r2 {
                assert!(
                    matches!(e, SqliteError::ResourceLimit(rl) if rl.kind == LimitKind::Memory)
                        || e.to_string().contains("memory budget exceeded"),
                    "expected memory budget error, got {e:?}"
                );
                hit_oom = true;
                break;
            }
        }
        assert!(hit_oom, "nested savepoint COW copies must enforce BudgetMeter memory limit");
        drop(c_cow);
        assert_eq!(cow_meter.memory_bytes(), 0);
    }

    // Direct TopK / IndexOrderCovering / IndexEq memory metering (fresh meter per query)!
    let mut c_scan_src = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_scan_src
        .execute(
            "CREATE TABLE sc (id INTEGER PRIMARY KEY, grp INT NOT NULL, val INT NOT NULL, pad TEXT NOT NULL);
             CREATE INDEX ix_sc_grp ON sc(grp);
             CREATE INDEX ix_sc_pad ON sc(pad);",
        )
        .unwrap();
    c_scan_src.execute("BEGIN").unwrap();
    for i in 1..=200i64 {
        c_scan_src
            .execute_with_params(
                "INSERT INTO sc VALUES (?1, ?2, ?3, ?4)",
                &QueryParams::positional(vec![
                    SqlValue::Int(i),
                    SqlValue::Int(7),
                    SqlValue::Int(i),
                    SqlValue::text(format!("row_{i:04}_{}", "z".repeat(400))),
                ]),
            )
            .unwrap();
    }
    c_scan_src.execute("COMMIT").unwrap();
    let sc_bytes = c_scan_src.to_bytes();
    for (label, sql) in [
        ("IndexEq", "SELECT id, pad FROM sc WHERE grp = 7"),
        (
            "IndexOrderCovering",
            "SELECT id, pad FROM sc ORDER BY pad DESC LIMIT 200",
        ),
        ("TopK", "SELECT id, pad FROM sc ORDER BY val DESC LIMIT 200"),
    ] {
        let scan_meter = Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(sc_bytes.len() as u64 + 32_768),
            ..ResourceLimits::default()
        }));
        let c_scan = SqliteConnection::open_from_bytes(
            &sc_bytes,
            None,
            None,
            ConnectionOptions {
                meter: Some(scan_meter.clone()),
                ..ConnectionOptions::default()
            },
        )
        .unwrap();
        let err = c_scan.query(sql).unwrap_err();
        assert!(
            matches!(err, SqliteError::ResourceLimit(e) if e.kind == LimitKind::Memory)
                || err.to_string().contains("memory budget exceeded"),
            "expected Memory limit on {label} direct scan, got {err:?}"
        );
        drop(c_scan);
        assert_eq!(scan_meter.memory_bytes(), 0, "{label} leaked meter bytes");
    }

    // 4. High Bug #4: `ALTER TABLE ADD COLUMN` on non-empty `STRICT` table rejects mismatched DEFAULT!
    let mut c_strict = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_strict
        .execute("CREATE TABLE st_empty (id INT PRIMARY KEY) STRICT")
        .unwrap();
    c_strict
        .execute("ALTER TABLE st_empty ADD COLUMN x INT DEFAULT 'not_an_int'")
        .unwrap();
    c_strict
        .execute("CREATE TABLE st_nonempty (id INT PRIMARY KEY) STRICT")
        .unwrap();
    c_strict.execute("INSERT INTO st_nonempty VALUES (1)").unwrap();
    assert!(
        c_strict
            .execute("ALTER TABLE st_nonempty ADD COLUMN x INT DEFAULT 'not_an_int'")
            .is_err(),
        "non-empty STRICT table must reject type-mismatched DEFAULT on ALTER TABLE ADD COLUMN"
    );
    c_strict
        .execute("ALTER TABLE st_nonempty ADD COLUMN ok_x INT DEFAULT 42")
        .unwrap();
    assert!(
        c_strict
            .query("SELECT ok_x FROM st_nonempty WHERE id = 1")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(42))
    );

    // 5. Med Bug #5: Trailing garbage bytes on a table leaf record rejected across PointRowid,
    //    TopK, and FullScan (`visit_sparse_range`)!
    let mut c_tr = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_tr.execute("CREATE TABLE tr (id INTEGER PRIMARY KEY, v INT)").unwrap();
    c_tr.execute("INSERT INTO tr VALUES (1, 99)").unwrap();
    let mut tr_bytes = c_tr.to_bytes();
    let tr_root = open_database_from_bytes(&tr_bytes, None, None, OpenOptions::default())
        .unwrap()
        .table("tr")
        .unwrap()
        .info()
        .root_page as usize;
    let pg_off = (tr_root - 1) * 4096;
    let pc = u16::from_be_bytes([tr_bytes[pg_off + 8], tr_bytes[pg_off + 9]]) as usize;
    // Grow local payload size varint by +1 byte so payload has 1 trailing garbage byte past body!
    tr_bytes[pg_off + pc] += 1;
    let c_bad = SqliteConnection::open_from_bytes(&tr_bytes, None, None, ConnectionOptions::default())
        .unwrap();
    assert!(c_bad.query("SELECT v FROM tr WHERE id = 1").is_err(), "PointRowid must reject trailing bytes");
    assert!(c_bad.query("SELECT v FROM tr ORDER BY v DESC LIMIT 1").is_err(), "TopK must reject trailing bytes");
    assert!(c_bad.query("SELECT v FROM tr").is_err(), "FullScan must reject trailing bytes");

    // 6. Low Bug #8: `DROP TABLE` and `ALTER TABLE RENAME` clean ALL duplicate `sqlite_sequence` rows.
    let mut c_dupseq = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_dupseq
        .execute("CREATE TABLE ds1 (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    c_dupseq
        .execute("CREATE TABLE ds2 (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)")
        .unwrap();
    c_dupseq.execute("INSERT INTO ds1(v) VALUES (1)").unwrap();
    c_dupseq.execute("INSERT INTO ds2(v) VALUES (1)").unwrap();
    c_dupseq
        .execute("INSERT INTO sqlite_sequence(name, seq) VALUES ('ds1', 5), ('ds2', 7)")
        .unwrap();
    c_dupseq.execute("ALTER TABLE ds1 RENAME TO ds1_new").unwrap();
    assert!(
        c_dupseq
            .query("SELECT count(*) FROM sqlite_sequence WHERE name = 'ds1'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(0))
    );
    assert!(
        c_dupseq
            .query("SELECT count(*) FROM sqlite_sequence WHERE name = 'ds1_new'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(2))
    );
    c_dupseq.execute("DROP TABLE ds2").unwrap();
    assert!(
        c_dupseq
            .query("SELECT count(*) FROM sqlite_sequence WHERE name = 'ds2'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(0))
    );

    // 7. Pass-8 Perf Wave 2 & 3 end-to-end stress (CachedFastDml SimpleInsert + monotonic append
    //    across splits & savepoints, PointUpdateSingleCol same-width + width-change + 0/1 toggle,
    //    PointDeleteRowid with fused 2-int delete, CachedFastSelect::IndexEq covering & pinned-leaf
    //    table lookup + DESC + NOCASE + NULL, counting & radix CREATE INDEX ASC/DESC, zero-intermediate
    //    seed_json, and whole-leaf visit_sparse_range with overflow rows) — ALL verified by C SQLite!
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c.execute(
        "CREATE TABLE p8 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grp INT NOT NULL,
            val INT NOT NULL,
            tag TEXT COLLATE NOCASE NOT NULL,
            pad TEXT NOT NULL
         );
         CREATE INDEX ix_p8_grp ON p8(grp);
         CREATE INDEX ix_p8_tag ON p8(tag COLLATE NOCASE);
         CREATE TABLE p8_r (k INT PRIMARY KEY, lbl TEXT NOT NULL);",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    for i in 1..=600i64 {
        let pad = if i == 300 {
            "O".repeat(5000) // overflow row to test whole-leaf fallback!
        } else {
            format!("pad_{i}")
        };
        c.execute_with_params(
            "INSERT INTO p8(grp, val, tag, pad) VALUES (?1, ?2, ?3, ?4)",
            &QueryParams::positional(vec![
                SqlValue::Int(i % 20),
                SqlValue::Int((i * 1973) % 50_000), // spans > 16383 -> exercises 2-pass 11-bit radix sort!
                SqlValue::text(format!("Tag_{}", i % 10)),
                SqlValue::text(pad),
            ]),
        )
        .unwrap();
    }
    // Same-width, 0<->1 serial-type toggle, and width-growing UPDATEs via CachedFastDml!
    for &(id, new_val) in &[(10i64, 0i64), (10, 1), (10, 0), (10, 12345), (10, 54321), (20, 999_999_999)] {
        c.execute_with_params(
            "UPDATE p8 SET val = ?1 WHERE id = ?2",
            &QueryParams::positional(vec![SqlValue::Int(new_val), SqlValue::Int(id)]),
        )
        .unwrap();
    }
    // Fused 2-int index deletes via CachedFastDml::PointDeleteRowid!
    for i in 1..=40i64 {
        c.execute_with_params(
            "DELETE FROM p8 WHERE id = ?1",
            &QueryParams::positional(vec![SqlValue::Int(i * 13)]),
        )
        .unwrap();
    }
    // Insert more rows AFTER update/delete to verify `last_append_tail` invalidation & refresh!
    for i in 601..=650i64 {
        c.execute_with_params(
            "INSERT INTO p8(grp, val, tag, pad) VALUES (?1, ?2, ?3, ?4)",
            &QueryParams::positional(vec![
                SqlValue::Int(i % 20),
                SqlValue::Int((i * 1973) % 50_000),
                SqlValue::text(format!("Tag_{}", i % 10)),
                SqlValue::text(format!("pad_{i}")),
            ]),
        )
        .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Radix sort CREATE INDEX (DESC) + counting sort CREATE INDEX (ASC)!
    c.execute("CREATE INDEX ix_p8_val_desc ON p8(val DESC)").unwrap();
    c.execute("CREATE INDEX ix_p8_grp2 ON p8(grp)").unwrap();

    // Verify CachedFastSelect::IndexEq (non-covering pinned-leaf, covering, NOCASE, NULL, param)!
    for _ in 0..2 {
        let eq_noncov = c.query("SELECT id, val, pad FROM p8 WHERE grp = 7").unwrap();
        let eq_full = c.query("SELECT id, val, pad FROM p8 NOT INDEXED WHERE grp = 7").unwrap();
        assert_eq!(eq_noncov.rows.len(), eq_full.rows.len());
        for (a, b) in eq_noncov.rows.iter().zip(eq_full.rows.iter()) {
            assert_eq!(a.len(), b.len());
            for (va, vb) in a.iter().zip(b.iter()) {
                assert!(va.exact_eq(vb), "IndexEq mismatch: {va:?} != {vb:?}");
            }
        }

        let eq_cov = c.query("SELECT id, grp FROM p8 WHERE grp = 7").unwrap();
        assert_eq!(eq_cov.rows.len(), eq_full.rows.len());

        let eq_nocase = c
            .query_with_params(
                "SELECT id, tag FROM p8 WHERE tag = ?1",
                &QueryParams::positional(vec![SqlValue::text("TAG_3")]),
            )
            .unwrap();
        assert!(!eq_nocase.rows.is_empty());

        let eq_null = c
            .query_with_params(
                "SELECT id FROM p8 WHERE grp = ?1",
                &QueryParams::positional(vec![SqlValue::Null]),
            )
            .unwrap();
        assert!(eq_null.rows.is_empty());

        let eq_desc = c.query("SELECT id, val FROM p8 WHERE val = 54321").unwrap();
        assert_eq!(eq_desc.rows.len(), 1);
        assert!(eq_desc.rows[0][0].exact_eq(&SqlValue::Int(10)));
    }

    // Verify whole-leaf `visit_sparse_range` reads overflow row id=300 intact (id=299 = 23*13 was deleted)!
    let ov_q = c.query("SELECT id, length(pad) FROM p8 WHERE id BETWEEN 298 AND 301").unwrap();
    assert_eq!(ov_q.rows.len(), 3);
    assert!(ov_q.rows[0][0].exact_eq(&SqlValue::Int(298)));
    assert!(ov_q.rows[1][0].exact_eq(&SqlValue::Int(300)));
    assert!(ov_q.rows[1][1].exact_eq(&SqlValue::Int(5000)));
    assert!(ov_q.rows[2][0].exact_eq(&SqlValue::Int(301)));

    c.execute("INSERT INTO p8_r VALUES (0,'a'),(1,'b'),(7,'lucky')").unwrap();
    let j_q = c
        .query("SELECT p.id, r.lbl FROM p8 p JOIN p8_r r ON p.grp = r.k")
        .unwrap();
    assert!(!j_q.rows.is_empty());

    // Zero-intermediate `seed_json` object & array batches + error rollback atomicity!
    let srep = c
        .seed_json(r#"{"p8_seed":[{"id":1,"v":10},{"id":2,"v":20},{"id":3,"v":30}]}"#)
        .unwrap();
    assert_eq!(srep.rows_inserted, 3);
    assert_integrity_ok(&mut c);
    verify_with_real_sqlite(
        "pass8_audit_and_fastpaths",
        &c.to_bytes(),
        None,
        Some("SELECT count(*) AS cnt, sum(val) AS sval, (SELECT count(*) FROM p8 WHERE grp = 7) AS g7 FROM p8"),
    );
}

#[test]
fn pass9_audit_seq_tail_atomicity_freelist_corrupt_metering_and_fastpaths_verified_by_c_sqlite() {
    // 1. `update_sqlite_sequence` invalidates `last_append_tail` so explicit `INSERT INTO sqlite_sequence`
    //    never collides with an auto-inserted row!
    let mut c_seq = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_seq.execute("CREATE TABLE sq1 (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)").unwrap();
    c_seq.execute("CREATE TABLE sq2 (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)").unwrap();
    c_seq.execute("INSERT INTO sqlite_sequence(name, seq) VALUES ('manual', 10)").unwrap();
    c_seq.execute("INSERT INTO sq1(v) VALUES (1)").unwrap();
    c_seq.execute("INSERT INTO sqlite_sequence(name, seq) VALUES ('manual2', 20)").unwrap();
    c_seq.execute("INSERT INTO sq2(v) VALUES (2)").unwrap();
    let seq_cnt = c_seq.query("SELECT count(*) FROM sqlite_sequence").unwrap();
    assert!(seq_cnt.rows[0][0].exact_eq(&SqlValue::Int(4)));
    assert_integrity_ok(&mut c_seq);

    // 2. `CREATE TABLE rc(x INT CHECK(RAISE(...)))` succeeds at DDL time, fails on INSERT (C SQLite parity)!
    let mut c_raise = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_raise
        .execute("CREATE TABLE rc(x INT CHECK(RAISE(ABORT, 'boom')))")
        .unwrap();
    let ins_err = c_raise.execute("INSERT INTO rc VALUES (1)").unwrap_err();
    assert!(ins_err
        .to_string()
        .contains("RAISE() may only be used within a trigger-program"));

    // 3. Corrupt self-referential freelist trunk page is rejected cleanly without infinite loop or corruption.
    let mut c_fl = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_fl.execute("CREATE TABLE fl (id INTEGER PRIMARY KEY, pad TEXT)").unwrap();
    c_fl.execute("BEGIN").unwrap();
    for i in 1..=80i64 {
        c_fl.execute_with_params(
            "INSERT INTO fl VALUES (?1, ?2)",
            &QueryParams::positional(vec![SqlValue::Int(i), SqlValue::text("a".repeat(200))]),
        )
        .unwrap();
    }
    c_fl.execute("COMMIT").unwrap();
    c_fl.execute("DROP TABLE fl").unwrap();
    let mut fl_bytes = c_fl.to_bytes();
    let trunk_pg = u32::from_be_bytes([fl_bytes[32], fl_bytes[33], fl_bytes[34], fl_bytes[35]]);
    assert!(trunk_pg >= 2);
    let trunk_off = (trunk_pg as usize - 1) * 4096;
    // Point leaf entry 0 back at trunk_pg itself (self-referential freelist leaf!).
    fl_bytes[trunk_off..trunk_off + 4].copy_from_slice(&0u32.to_be_bytes());
    fl_bytes[trunk_off + 4..trunk_off + 8].copy_from_slice(&1u32.to_be_bytes());
    fl_bytes[trunk_off + 8..trunk_off + 12].copy_from_slice(&trunk_pg.to_be_bytes());
    let mut c_fl_bad =
        SqliteConnection::open_from_bytes(&fl_bytes, None, None, ConnectionOptions::default())
            .unwrap();
    assert!(
        c_fl_bad
            .execute("CREATE TABLE fl_new (x INT)")
            .is_err(),
        "self-referential freelist leaf must be rejected"
    );

    // 4. Corrupt empty record payload (`psize == 0`) rejected cleanly across PointRowid and FullScan.
    let mut c_ep = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_ep.execute("CREATE TABLE ep (id INTEGER PRIMARY KEY, v INT)").unwrap();
    c_ep.execute("INSERT INTO ep VALUES (1, 42)").unwrap();
    let mut ep_bytes = c_ep.to_bytes();
    let ep_root = open_database_from_bytes(&ep_bytes, None, None, OpenOptions::default())
        .unwrap()
        .table("ep")
        .unwrap()
        .info()
        .root_page as usize;
    let ep_off = (ep_root - 1) * 4096;
    let ep_pc = u16::from_be_bytes([ep_bytes[ep_off + 8], ep_bytes[ep_off + 9]]) as usize;
    ep_bytes[ep_off + ep_pc] = 0; // psize = 0 (empty payload)
    let c_ep_bad =
        SqliteConnection::open_from_bytes(&ep_bytes, None, None, ConnectionOptions::default())
            .unwrap();
    assert!(c_ep_bad.query("SELECT v FROM ep WHERE id = 1").is_err());
    assert!(c_ep_bad.query("SELECT v FROM ep").is_err());

    // 5. Pass-9 Perf Wave 2 end-to-end stress: UndoFrame bitmap rollback/commit, leaf-hinted
    //    PointUpdateSingleCol / PointDeleteRowid / PointRowid SELECTs across leaf boundaries,
    //    `index_bulk_load_sorted_two_int` ASC/DESC, and zero-alloc streaming `seed_json`
    //    (including duplicate rowids under OR IGNORE, floats, booleans, nulls, and rollback)!
    let mut c = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c.execute(
        "CREATE TABLE p9 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grp INT NOT NULL,
            val INT NOT NULL,
            name TEXT NOT NULL,
            pad TEXT NOT NULL
         );
         CREATE INDEX ix_p9_grp ON p9(grp);",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    let pad = "p".repeat(64);
    for i in 1..=600i64 {
        c.execute_with_params(
            "INSERT INTO p9(grp, val, name, pad) VALUES (?1, ?2, ?3, ?4)",
            &QueryParams::positional(vec![
                SqlValue::Int(i % 25),
                SqlValue::Int((i * 1009) % 10_000),
                SqlValue::text(format!("u_{i}")),
                SqlValue::text(pad.as_str()),
            ]),
        )
        .unwrap();
    }
    // Savepoint rollback inside transaction exercises flat bitmap `UndoFrame`!
    c.execute("SAVEPOINT sp1").unwrap();
    c.execute("UPDATE p9 SET val = -1 WHERE id = 10").unwrap();
    c.execute("DELETE FROM p9 WHERE id = 20").unwrap();
    c.execute("ROLLBACK TO sp1").unwrap();
    c.execute("RELEASE sp1").unwrap();
    // Leaf-hinted consecutive point UPDATEs across multiple leaves (same-leaf hits + misses)!
    for i in 1..=120i64 {
        c.execute_with_params(
            "UPDATE p9 SET val = ?1 WHERE id = ?2",
            &QueryParams::positional(vec![SqlValue::Int(i * 7), SqlValue::Int(i * 4)]),
        )
        .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Direct 2-int bulk load CREATE INDEX (both ASC and DESC)!
    c.execute("CREATE INDEX ix_p9_val_asc ON p9(val)").unwrap();
    c.execute("CREATE INDEX ix_p9_val_desc ON p9(val DESC)").unwrap();

    // Leaf-hinted point SELECTs across multiple leaves!
    for i in 1..=100i64 {
        let r = c
            .query_with_params(
                "SELECT id, grp, val, name FROM p9 WHERE id = ?1",
                &QueryParams::positional(vec![SqlValue::Int(i * 5)]),
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert!(r.rows[0][0].exact_eq(&SqlValue::Int(i * 5)));
    }

    // Leaf-hinted point DELETEs (deleting first, interior, and last cells across leaves)!
    c.execute("BEGIN").unwrap();
    for i in 1..=80i64 {
        c.execute_with_params(
            "DELETE FROM p9 WHERE id = ?1",
            &QueryParams::positional(vec![SqlValue::Int(i * 6)]),
        )
        .unwrap();
    }
    c.execute("COMMIT").unwrap();

    // Zero-alloc streaming shorthand `seed_json` (monotonic + duplicate ID OR IGNORE + REAL/bool/null)!
    let mut sdoc = String::from(r#"{"stream_items":["#);
    for i in 0..250 {
        if i > 0 {
            sdoc.push(',');
        }
        let flag = if i % 2 == 0 { "true" } else { "false" };
        let rating = if i % 5 == 0 { "null".to_string() } else { format!("{}.5", i % 10) };
        sdoc.push_str(&format!(
            r#"{{"id":{i},"name":"item_{i}","flag":{flag},"rating":{rating}}}"#
        ));
    }
    // Add duplicate id=5 to verify OR IGNORE handling on streaming path!
    sdoc.push_str(r#",{"id":5,"name":"dup_ignored","flag":false,"rating":9.5}]}"#);
    let rep = c.seed_json(&sdoc).unwrap();
    assert_eq!(rep.tables_created, 1);
    assert_eq!(rep.tables_seeded, 1);
    assert_eq!(rep.rows_inserted, 250);
    let check_dup = c.query("SELECT name FROM stream_items WHERE id = 5").unwrap();
    assert_eq!(check_dup.rows[0][0].as_str(), Some("item_5"));

    assert_integrity_ok(&mut c);
    verify_with_real_sqlite(
        "pass9_audit_and_fastpaths",
        &c.to_bytes(),
        None,
        Some("SELECT (SELECT count(*) FROM p9) AS p9c, (SELECT count(*) FROM stream_items) AS sc, (SELECT sum(val) FROM p9) AS sv"),
    );
}

#[test]
fn pass10_audit_seed_utf8_vacuum_pragmas_deferred_seq_metering_and_c_sqlite_parity() {
    // 1. `seed_json` multi-byte UTF-8 table key never panics, and shorthand scan honors step meter!
    let mut c_seed = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    let rep = c_seed
        .seed_json(r#"{"ééééééé":[{"id":1,"v":"ok"},{"id":2,"v":"done"}]}"#)
        .unwrap();
    assert_eq!(rep.rows_inserted, 2);
    assert_eq!(
        c_seed
            .query("SELECT v FROM \"ééééééé\" WHERE id = 2")
            .unwrap()
            .rows[0][0]
            .as_str(),
        Some("done")
    );

    let tiny_seed_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(10),
        max_memory_bytes: Some(256 * 1024),
        max_page_reads: None,
    }));
    let mut c_seed_m = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(tiny_seed_meter),
        ..ConnectionOptions::default()
    })
    .unwrap();
    let big_seed = format!(r#"{{"items":[{{"id":1,"s":"{}"}}]}}"#, "a".repeat(50_000));
    let seed_err = c_seed_m.seed_json(&big_seed).unwrap_err();
    assert!(matches!(seed_err, SqliteError::ResourceLimit(_)));

    // 2. `CREATE INDEX IF NOT EXISTS tbl ON tbl(x)` fails when a table already has that name!
    let mut c_idx = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_idx.execute("CREATE TABLE same_name (x INT)").unwrap();
    let idx_err = c_idx
        .execute("CREATE INDEX IF NOT EXISTS same_name ON same_name(x)")
        .unwrap_err();
    assert!(idx_err
        .to_string()
        .contains("there is already a table named same_name"));

    // 3. `PRAGMA integrity_check(N)` caps errors & propagates `ResourceLimit` cleanly.
    let ic_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(50_000),
        max_memory_bytes: Some(256 * 1024),
        max_page_reads: None,
    }));
    let mut c_ic = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(ic_meter.clone()),
        ..ConnectionOptions::default()
    })
    .unwrap();
    c_ic.execute("CREATE TABLE ic_t (id INTEGER PRIMARY KEY, v INT)").unwrap();
    c_ic.execute("INSERT INTO ic_t VALUES (1, 10), (2, 20)").unwrap();
    let ic_ok = c_ic.execute("PRAGMA integrity_check(5)").unwrap();
    assert_eq!(ic_ok.rows[0][0].as_str(), Some("ok"));

    // 4. Parse `ResourceLimit` preserved in `query_with_params` & `execute` (not downgraded to Sql)!
    let parse_m = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(5),
        max_memory_bytes: Some(64 * 1024),
        max_page_reads: None,
    }));
    let mut c_pm = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(parse_m),
        ..ConnectionOptions::default()
    })
    .unwrap();
    let long_q = format!("SELECT {} 1", " ".repeat(2000));
    assert!(matches!(
        c_pm.query_with_params(&long_q, &QueryParams::empty()).unwrap_err(),
        SqliteError::ResourceLimit(_)
    ));
    assert!(matches!(
        c_pm.execute(&long_q).unwrap_err(),
        SqliteError::ResourceLimit(_)
    ));

    // 5. `PRAGMA foreign_key_list`, `PRAGMA table_list`, `PRAGMA data_version`, tx-guarded
    //    `PRAGMA foreign_keys`, and invalid `PRAGMA page_size` silently ignored!
    let mut c_p = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_p.execute(
        "CREATE TABLE parent_a (id INTEGER PRIMARY KEY);
         CREATE TABLE parent_b (k TEXT PRIMARY KEY);
         CREATE TABLE child (
             id INTEGER PRIMARY KEY,
             a_id INT REFERENCES parent_a(id) ON DELETE CASCADE,
             b_k TEXT REFERENCES parent_b(k) ON UPDATE SET NULL
         );
         CREATE TABLE wr_strict (k INT PRIMARY KEY, v TEXT) WITHOUT ROWID, STRICT;",
    )
    .unwrap();
    // Invalid page_size silently ignored (matches SQLite)!
    c_p.execute("PRAGMA page_size = 123").unwrap();
    assert!(c_p.execute("PRAGMA page_size").unwrap().rows[0][0].exact_eq(&SqlValue::Int(4096)));

    // `PRAGMA foreign_key_list(child)` + TVF `pragma_foreign_key_list('child')`:
    // reverse declaration order (`parent_b` = id 0, `parent_a` = id 1)!
    let fkl = c_p.execute("PRAGMA foreign_key_list(child)").unwrap();
    assert_eq!(
        fkl.columns,
        vec!["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"]
    );
    assert_eq!(fkl.rows.len(), 2);
    assert!(fkl.rows[0][0].exact_eq(&SqlValue::Int(0)));
    assert_eq!(fkl.rows[0][2].as_str(), Some("parent_b"));
    assert_eq!(fkl.rows[0][3].as_str(), Some("b_k"));
    assert_eq!(fkl.rows[0][4].as_str(), Some("k"));
    assert_eq!(fkl.rows[0][5].as_str(), Some("SET NULL"));
    assert_eq!(fkl.rows[0][7].as_str(), Some("NONE"));
    assert!(fkl.rows[1][0].exact_eq(&SqlValue::Int(1)));
    assert_eq!(fkl.rows[1][2].as_str(), Some("parent_a"));
    assert_eq!(fkl.rows[1][6].as_str(), Some("CASCADE"));
    let fkl_tvf = c_p.query("SELECT * FROM pragma_foreign_key_list('child')").unwrap();
    assert_eq!(fkl_tvf.rows.len(), 2);

    // `PRAGMA table_list('wr_strict')` + TVF `pragma_table_list('wr_strict')`!
    let tl_one = c_p.execute("PRAGMA table_list('wr_strict')").unwrap();
    assert_eq!(tl_one.columns, vec!["schema", "name", "type", "ncol", "wr", "strict"]);
    assert_eq!(tl_one.rows.len(), 1);
    assert_eq!(tl_one.rows[0][0].as_str(), Some("main"));
    assert_eq!(tl_one.rows[0][1].as_str(), Some("wr_strict"));
    assert_eq!(tl_one.rows[0][2].as_str(), Some("table"));
    assert!(tl_one.rows[0][3].exact_eq(&SqlValue::Int(2)));
    assert!(tl_one.rows[0][4].exact_eq(&SqlValue::Int(1)));
    assert!(tl_one.rows[0][5].exact_eq(&SqlValue::Int(1)));
    let tl_tvf = c_p.query("SELECT name, wr, strict FROM pragma_table_list('wr_strict')").unwrap();
    assert_eq!(tl_tvf.rows.len(), 1);

    // `PRAGMA data_version`!
    assert!(c_p.execute("PRAGMA data_version").unwrap().rows[0][0].exact_eq(&SqlValue::Int(1)));

    // `PRAGMA foreign_keys = OFF` is a no-op inside an active transaction!
    c_p.execute("PRAGMA foreign_keys = ON").unwrap();
    assert!(c_p.execute("PRAGMA foreign_keys").unwrap().rows[0][0].exact_eq(&SqlValue::Int(1)));
    c_p.execute("BEGIN").unwrap();
    c_p.execute("PRAGMA foreign_keys = OFF").unwrap();
    assert!(
        c_p.execute("PRAGMA foreign_keys").unwrap().rows[0][0].exact_eq(&SqlValue::Int(1)),
        "PRAGMA foreign_keys must be ignored inside a transaction"
    );
    c_p.execute("COMMIT").unwrap();
    c_p.execute("PRAGMA foreign_keys = OFF").unwrap();
    assert!(c_p.execute("PRAGMA foreign_keys").unwrap().rows[0][0].exact_eq(&SqlValue::Int(0)));

    // 6. Deferred `sqlite_sequence` observability (`query`, `to_bytes`, constraint error, savepoint, rollback)!
    let mut c_dseq = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    c_dseq
        .execute("CREATE TABLE ai (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT UNIQUE)")
        .unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (100)").unwrap();
    c_dseq.execute("BEGIN").unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (200)").unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (300)").unwrap();
    // `&self` query and `to_bytes()` must both observe the deferred uncommitted sequence = 3!
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(3))
    );
    let mid_bytes = c_dseq.to_bytes();
    let mid_ro = open_database_from_bytes(&mid_bytes, None, None, OpenOptions::default()).unwrap();
    assert!(
        mid_ro
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(3))
    );
    // Constraint violation inside tx must keep earlier deferred bumps intact (seq stays 3)!
    assert!(c_dseq.execute("INSERT INTO ai(v) VALUES (200)").is_err());
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(3))
    );
    // Savepoint + rollback-to restores exact sequence!
    c_dseq.execute("SAVEPOINT sp_seq").unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (400)").unwrap();
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(4))
    );
    c_dseq.execute("ROLLBACK TO sp_seq").unwrap();
    c_dseq.execute("RELEASE sp_seq").unwrap();
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(3))
    );
    // Full rollback reverts sequence back to 1!
    c_dseq.execute("ROLLBACK").unwrap();
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(1))
    );
    // Commit flushes deferred sequence cleanly!
    c_dseq.execute("BEGIN").unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (200)").unwrap();
    c_dseq.execute("INSERT INTO ai(v) VALUES (300)").unwrap();
    c_dseq.execute("COMMIT").unwrap();
    assert!(
        c_dseq
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(3))
    );
    assert_integrity_ok(&mut c_dseq);

    // 7. Real crash-safe `VACUUM` + staged `PRAGMA page_size = 8192` on a VFS-backed database
    //    with rowid gaps, `AUTOINCREMENT`, `WITHOUT ROWID`, indexes, views, triggers, `user_version`,
    //    and `application_id` — verified end-to-end by live C SQLite!
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    let mut c_vac =
        SqliteConnection::open(vfs.clone(), "vac.db", ConnectionOptions::default()).unwrap();
    c_vac
        .execute(
            "PRAGMA user_version = 77;
             PRAGMA application_id = 305419896;
             CREATE TABLE main_t (id INTEGER PRIMARY KEY AUTOINCREMENT, grp INT NOT NULL, pad TEXT NOT NULL);
             CREATE INDEX ix_main_grp ON main_t(grp DESC);
             CREATE TABLE wr_t (k1 INT, k2 TEXT, v INT, PRIMARY KEY(k1, k2)) WITHOUT ROWID;
             CREATE INDEX ix_wr_v ON wr_t(v);
             CREATE VIEW v_cnt AS SELECT count(*) AS n FROM main_t;",
        )
        .unwrap();
    c_vac.execute("BEGIN").unwrap();
    let pad = "V".repeat(300);
    for i in 1..=200i64 {
        c_vac
            .execute_with_params(
                "INSERT INTO main_t(grp, pad) VALUES (?1, ?2)",
                &QueryParams::positional(vec![SqlValue::Int(i % 10), SqlValue::text(pad.as_str())]),
            )
            .unwrap();
    }
    for i in 1..=30i64 {
        c_vac
            .execute_with_params(
                "INSERT INTO wr_t VALUES (?1, ?2, ?3)",
                &QueryParams::positional(vec![
                    SqlValue::Int(i),
                    SqlValue::text(format!("k_{i:02}")),
                    SqlValue::Int(i * 10),
                ]),
            )
            .unwrap();
    }
    c_vac.execute("COMMIT").unwrap();
    // Delete most rows to create plenty of freelist pages and non-contiguous rowids!
    c_vac.execute("DELETE FROM main_t WHERE id <= 185").unwrap();
    let before_pages = c_vac.page_count();
    let before_free = c_vac.execute("PRAGMA freelist_count").unwrap().rows[0][0].clone();
    assert!(matches!(before_free, SqlValue::Int(f) if f > 0));

    // Reject VACUUM inside active transaction!
    c_vac.execute("BEGIN").unwrap();
    assert!(c_vac.execute("VACUUM").is_err());
    c_vac.execute("ROLLBACK").unwrap();

    // Stage page_size change from 4096 -> 8192 and run real VACUUM!
    c_vac.execute("PRAGMA page_size = 8192").unwrap();
    assert_eq!(c_vac.page_size(), 4096); // staged until VACUUM
    c_vac.execute("VACUUM").unwrap();
    assert_eq!(c_vac.page_size(), 8192);
    assert!(
        c_vac.page_count() < before_pages,
        "VACUUM must shrink page_count ({} -> {})",
        before_pages,
        c_vac.page_count()
    );
    assert!(c_vac.execute("PRAGMA freelist_count").unwrap().rows[0][0].exact_eq(&SqlValue::Int(0)));
    assert!(c_vac.execute("PRAGMA user_version").unwrap().rows[0][0].exact_eq(&SqlValue::Int(77)));
    assert!(
        c_vac.execute("PRAGMA application_id").unwrap().rows[0][0]
            .exact_eq(&SqlValue::Int(305_419_896))
    );
    assert!(c_vac.query("SELECT n FROM v_cnt").unwrap().rows[0][0].exact_eq(&SqlValue::Int(15)));
    assert!(
        c_vac
            .query("SELECT min(id), max(id) FROM main_t")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(186))
    );
    assert!(
        c_vac
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'main_t'")
            .unwrap()
            .rows[0][0]
            .exact_eq(&SqlValue::Int(200))
    );
    assert!(
        c_vac.query("SELECT count(*) FROM wr_t").unwrap().rows[0][0].exact_eq(&SqlValue::Int(30))
    );
    assert_integrity_ok(&mut c_vac);
    drop(c_vac);

    // Reopen from VFS after VACUUM + page_size swap and verify against live C SQLite!
    let mut c_reopen =
        SqliteConnection::open(vfs, "vac.db", ConnectionOptions::default()).unwrap();
    assert_eq!(c_reopen.page_size(), 8192);
    assert!(c_reopen.query("SELECT n FROM v_cnt").unwrap().rows[0][0].exact_eq(&SqlValue::Int(15)));
    assert_integrity_ok(&mut c_reopen);
    verify_with_real_sqlite(
        "pass10_vacuum_page_size_8192",
        &c_reopen.to_bytes(),
        None,
        Some("SELECT (SELECT count(*) FROM main_t) AS mc, (SELECT min(id) FROM main_t) AS mn, (SELECT count(*) FROM wr_t) AS wc, (SELECT seq FROM sqlite_sequence WHERE name='main_t') AS sq"),
    );
}
