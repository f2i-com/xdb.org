//! Comprehensive regression & SQLite parity tests for:
//! - `CREATE TRIGGER` (`BEFORE`/`AFTER`/`INSTEAD OF`, `UPDATE OF`, `WHEN`, `OLD`/`NEW`, `RAISE()`,
//!   `PRAGMA recursive_triggers`, `UPSERT`, `OR REPLACE`, `ALTER TABLE`, `VACUUM`)
//! - Foreign Keys (`PRAGMA foreign_keys`, `CASCADE`/`SET NULL`/`SET DEFAULT`/`RESTRICT`/`NO ACTION`,
//!   `DEFERRABLE`, `PRAGMA defer_foreign_keys`, `PRAGMA foreign_key_check`, `DROP TABLE`, `ALTER TABLE`)
//! - `REINDEX`, `ANALYZE` (`sqlite_stat1`), `VACUUM INTO`, `EXPLAIN QUERY PLAN`,
//!   `ALTER TABLE` (`DROP/SET NOT NULL`, `ADD CHECK`, `DROP CONSTRAINT`), and new PRAGMAs
//! - Direct-on-`WritePager` `CachedFastSelect` kernels parity & edge cases

use std::sync::Arc;

use xdb_core::vfs::{MemoryVfs, MemoryVfsOptions};
use xdb_core::{BudgetMeter, ResourceLimits, SqlValue};
use xdb_sqlite::{open_database, ConnectionOptions, OpenOptions, QueryParams, SqliteConnection};

fn i(v: i64) -> SqlValue {
    SqlValue::Int(v)
}

fn s(v: &str) -> SqlValue {
    SqlValue::text(v)
}

fn t(v: &str) -> SqlValue {
    SqlValue::text(v)
}

fn row_exact_eq(a: &[SqlValue], b: &[SqlValue]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.exact_eq(y))
}

fn assert_rows_eq(got: &[Vec<SqlValue>], expected: &[Vec<SqlValue>]) {
    assert_eq!(
        got.len(),
        expected.len(),
        "row count mismatch:\n  got:      {got:?}\n  expected: {expected:?}"
    );
    for (idx, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        assert!(
            row_exact_eq(g, e),
            "row {idx} mismatch:\n  got:      {g:?}\n  expected: {e:?}"
        );
    }
}

#[test]
fn triggers_before_after_insert_update_delete_and_ordering() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, qty INT DEFAULT 1);
         CREATE TABLE log (seq INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT);",
    )
    .unwrap();

    // Two BEFORE INSERT and two AFTER INSERT triggers to verify reverse creation firing order
    // AND verify `NEW.id = -1` in BEFORE INSERT when auto-assigned vs final rowid in AFTER INSERT!
    conn.execute(
        "CREATE TRIGGER bi_1 BEFORE INSERT ON items BEGIN
            INSERT INTO log(msg) VALUES ('bi1:' || NEW.id || ':' || NEW.name || ':' || NEW.qty);
         END;
         CREATE TRIGGER bi_2 BEFORE INSERT ON items BEGIN
            INSERT INTO log(msg) VALUES ('bi2:' || NEW.id || ':' || NEW.name);
         END;
         CREATE TRIGGER ai_1 AFTER INSERT ON items BEGIN
            INSERT INTO log(msg) VALUES ('ai1:' || NEW.id || ':' || NEW.name);
         END;
         CREATE TRIGGER ai_2 AFTER INSERT ON items BEGIN
            INSERT INTO log(msg) VALUES ('ai2:' || NEW.id || ':' || NEW.name);
         END;",
    )
    .unwrap();

    let res = conn
        .execute("INSERT INTO items(name) VALUES ('apple')")
        .unwrap();
    // Outer statement `changes()` and `last_insert_rowid()` must NOT be clobbered by trigger inserts!
    assert_eq!(res.changes, 1);
    assert_eq!(res.last_insert_rowid, 1);
    let q = conn.query("SELECT changes(), last_insert_rowid()").unwrap();
    assert_rows_eq(&q.rows, &[vec![i(1), i(1)]]);

    let logs = conn.query("SELECT msg FROM log ORDER BY seq").unwrap();
    assert_rows_eq(
        &logs.rows,
        &[
            vec![s("bi2:-1:apple")],
            vec![s("bi1:-1:apple:1")],
            vec![s("ai2:1:apple")],
            vec![s("ai1:1:apple")],
        ],
    );

    // Explicit rowid appears as its coerced value even in BEFORE INSERT.
    conn.execute("DELETE FROM log").unwrap();
    conn.execute("INSERT INTO items(id, name, qty) VALUES (42, 'pear', 5)")
        .unwrap();
    let logs = conn.query("SELECT msg FROM log ORDER BY seq").unwrap();
    assert_rows_eq(
        &logs.rows,
        &[
            vec![s("bi2:42:pear")],
            vec![s("bi1:42:pear:5")],
            vec![s("ai2:42:pear")],
            vec![s("ai1:42:pear")],
        ],
    );

    // `UPDATE OF` + `WHEN` clause + `OLD.*` / `NEW.*` + `DELETE` triggers.
    conn.execute(
        "DELETE FROM log;
         CREATE TRIGGER bu_qty BEFORE UPDATE OF qty ON items WHEN NEW.qty > OLD.qty BEGIN
            INSERT INTO log(msg) VALUES ('bu_qty:' || OLD.qty || '->' || NEW.qty);
         END;
         CREATE TRIGGER au_name AFTER UPDATE OF name ON items BEGIN
            INSERT INTO log(msg) VALUES ('au_name:' || OLD.name || '->' || NEW.name);
         END;
         CREATE TRIGGER bd_items BEFORE DELETE ON items BEGIN
            INSERT INTO log(msg) VALUES ('bd:' || OLD.id || ':' || OLD.name);
         END;
         CREATE TRIGGER ad_items AFTER DELETE ON items BEGIN
            INSERT INTO log(msg) VALUES ('ad:' || OLD.id);
         END;",
    )
    .unwrap();

    // Updating only `name` (even to same value!) fires `au_name` but NOT `bu_qty`.
    conn.execute("UPDATE items SET name = 'apple' WHERE id = 1")
        .unwrap();
    // Updating `qty` downwards does NOT pass `WHEN NEW.qty > OLD.qty`.
    conn.execute("UPDATE items SET qty = 2 WHERE id = 42")
        .unwrap();
    // Updating `qty` upwards DOES fire `bu_qty`.
    conn.execute("UPDATE items SET qty = 10 WHERE id = 42")
        .unwrap();
    // Delete fires `bd_items` then `ad_items`.
    conn.execute("DELETE FROM items WHERE id = 1").unwrap();

    let logs = conn.query("SELECT msg FROM log ORDER BY seq").unwrap();
    assert_rows_eq(
        &logs.rows,
        &[
            vec![s("au_name:apple->apple")],
            vec![s("bu_qty:2->10")],
            vec![s("bd:1:apple")],
            vec![s("ad:1")],
        ],
    );

    // Unqualified column name in trigger body must NOT leak `OLD`/`NEW` columns.
    conn.execute(
        "CREATE TRIGGER bad_unq AFTER INSERT ON items BEGIN
            INSERT INTO log(msg) VALUES (name);
         END;",
    )
    .unwrap();
    let err = conn
        .execute("INSERT INTO items(name) VALUES ('x')")
        .unwrap_err();
    assert!(
        err.to_string().contains("no such column: name"),
        "unexpected err: {err}"
    );
    conn.execute("DROP TRIGGER bad_unq").unwrap();

    // Validation errors for CREATE TRIGGER.
    conn.execute("CREATE VIEW v_items AS SELECT id, name FROM items")
        .unwrap();
    assert!(conn
        .execute("CREATE TRIGGER bi_1 BEFORE INSERT ON items BEGIN SELECT 1; END")
        .is_err());
    conn.execute("CREATE TRIGGER IF NOT EXISTS bi_1 BEFORE INSERT ON items BEGIN SELECT 1; END")
        .unwrap();
    let err = conn
        .execute("CREATE TRIGGER bad_v BEFORE INSERT ON v_items BEGIN SELECT 1; END")
        .unwrap_err();
    assert!(err.to_string().contains("cannot create BEFORE trigger on view"));
    let err = conn
        .execute("CREATE TRIGGER bad_t INSTEAD OF INSERT ON items BEGIN SELECT 1; END")
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("cannot create INSTEAD OF trigger on table"));
    let err = conn
        .execute("CREATE TRIGGER bad_sys BEFORE INSERT ON sqlite_master BEGIN SELECT 1; END")
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("cannot create trigger on system table"));
    assert!(conn
        .execute("CREATE TRIGGER bad_missing BEFORE INSERT ON no_such_tbl BEGIN SELECT 1; END")
        .is_err());

    // DROP TABLE drops attached triggers cleanly.
    conn.execute("DROP TABLE items").unwrap();
    let tr_cnt = conn
        .query("SELECT count(*) FROM sqlite_master WHERE type = 'trigger'")
        .unwrap();
    assert_rows_eq(&tr_cnt.rows, &[vec![i(0)]]);
}

#[test]
fn triggers_raise_actions_instead_of_views_recursive_upsert_and_alter() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT);
         CREATE TABLE audit (note TEXT);",
    )
    .unwrap();

    // RAISE() outside triggers is forbidden.
    let err = conn.query("SELECT RAISE(ABORT, 'nope')").unwrap_err();
    assert!(err.to_string().contains("trigger-program"));

    // RAISE(IGNORE), RAISE(ABORT, dynamic expr), RAISE(FAIL, ...), RAISE(ROLLBACK, ...).
    conn.execute(
        "CREATE TRIGGER guard_t BEFORE INSERT ON t BEGIN
            SELECT CASE
                WHEN NEW.v = 0 THEN RAISE(IGNORE)
                WHEN NEW.v = -1 THEN RAISE(ABORT, 'abort:' || NEW.id)
                WHEN NEW.v = -2 THEN RAISE(FAIL, 'fail:' || NEW.id)
                WHEN NEW.v = -3 THEN RAISE(ROLLBACK, 'rollback:' || NEW.id)
            END;
         END;",
    )
    .unwrap();

    // Multi-row INSERT with RAISE(IGNORE) skips ONLY the v=0 row.
    let r = conn
        .execute("INSERT INTO t VALUES (1, 10), (2, 0), (3, 30)")
        .unwrap();
    assert_eq!(r.changes, 2);
    assert_rows_eq(
        &conn.query("SELECT id, v FROM t ORDER BY id").unwrap().rows,
        &[vec![i(1), i(10)], vec![i(3), i(30)]],
    );

    // RAISE(ABORT, expr) inside transaction rolls back statement only, keeps transaction open.
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (4, 40)").unwrap();
    let err = conn
        .execute("INSERT INTO t VALUES (5, 50), (6, -1)")
        .unwrap_err();
    assert!(err.to_string().contains("abort:6"), "err={err}");
    assert!(conn.in_transaction());
    conn.execute("COMMIT").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id FROM t ORDER BY id").unwrap().rows,
        &[vec![i(1)], vec![i(3)], vec![i(4)]],
    );

    // RAISE(FAIL, expr) keeps rows inserted BEFORE the failing row in the same statement!
    let err = conn
        .execute("INSERT INTO t VALUES (7, 70), (8, -2), (9, 90)")
        .unwrap_err();
    assert!(err.to_string().contains("fail:8"), "err={err}");
    assert_rows_eq(
        &conn.query("SELECT id FROM t ORDER BY id").unwrap().rows,
        &[vec![i(1)], vec![i(3)], vec![i(4)], vec![i(7)]],
    );

    // RAISE(ROLLBACK, expr) rolls back the entire enclosing transaction!
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (10, 100)").unwrap();
    let err = conn.execute("INSERT INTO t VALUES (11, -3)").unwrap_err();
    assert!(err.to_string().contains("rollback:11"), "err={err}");
    assert!(!conn.in_transaction());
    assert_rows_eq(
        &conn
            .query("SELECT count(*) FROM t WHERE id = 10")
            .unwrap()
            .rows,
        &[vec![i(0)]],
    );

    // INSTEAD OF INSERT / UPDATE / DELETE on a view (and DROP VIEW cleanup).
    conn.execute(
        "CREATE TABLE base (id INTEGER PRIMARY KEY, name TEXT);
         CREATE VIEW vbase AS SELECT id, name FROM base;
         CREATE TRIGGER v_ins INSTEAD OF INSERT ON vbase BEGIN
            INSERT INTO base(id, name) VALUES (NEW.id, upper(NEW.name));
         END;
         CREATE TRIGGER v_upd INSTEAD OF UPDATE OF name ON vbase BEGIN
            UPDATE base SET name = NEW.name || '!' WHERE id = OLD.id;
         END;
         CREATE TRIGGER v_del INSTEAD OF DELETE ON vbase BEGIN
            DELETE FROM base WHERE id = OLD.id;
            INSERT INTO audit VALUES ('del:' || OLD.name);
         END;",
    )
    .unwrap();
    conn.execute("INSERT INTO vbase VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    conn.execute("UPDATE vbase SET name = 'alicia' WHERE id = 1")
        .unwrap();
    conn.execute("DELETE FROM vbase WHERE id = 2").unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, name FROM base ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), s("alicia!")]],
    );
    assert_rows_eq(
        &conn.query("SELECT note FROM audit").unwrap().rows,
        &[vec![s("del:BOB")]],
    );
    assert!(conn
        .execute("INSERT INTO vbase VALUES (3, 'c') RETURNING id")
        .is_err());
    conn.execute("DROP VIEW vbase").unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name='vbase'")
            .unwrap()
            .rows,
        &[vec![i(0)]],
    );

    // PRAGMA recursive_triggers OFF vs ON + UPSERT DO UPDATE + OR REPLACE delete triggers.
    conn.execute(
        "CREATE TABLE rec (n INT);
         CREATE TRIGGER rec_tr AFTER INSERT ON rec WHEN NEW.n > 1 BEGIN
            INSERT INTO rec VALUES (NEW.n - 1);
         END;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA recursive_triggers").unwrap().rows,
        &[vec![i(0)]],
    );
    // With recursive_triggers = OFF: inserts 4, fires once -> inserts 3, stops!
    conn.execute("INSERT INTO rec VALUES (4)").unwrap();
    assert_rows_eq(
        &conn.query("SELECT n FROM rec ORDER BY n").unwrap().rows,
        &[vec![i(3)], vec![i(4)]],
    );
    // With recursive_triggers = ON: inserts 4 -> 3 -> 2 -> 1!
    conn.execute("DELETE FROM rec; PRAGMA recursive_triggers = ON;")
        .unwrap();
    conn.execute("INSERT INTO rec VALUES (4)").unwrap();
    assert_rows_eq(
        &conn.query("SELECT n FROM rec ORDER BY n").unwrap().rows,
        &[vec![i(1)], vec![i(2)], vec![i(3)], vec![i(4)]],
    );

    // OR REPLACE fires DELETE trigger when recursive_triggers = ON, and UPSERT fires UPDATE trigger!
    conn.execute(
        "CREATE TABLE kv (k INTEGER PRIMARY KEY, val TEXT);
         CREATE TABLE kv_log (ev TEXT);
         CREATE TRIGGER kv_ai AFTER INSERT ON kv BEGIN INSERT INTO kv_log VALUES ('ins:' || NEW.k || ':' || NEW.val); END;
         CREATE TRIGGER kv_au AFTER UPDATE ON kv BEGIN INSERT INTO kv_log VALUES ('upd:' || OLD.val || '->' || NEW.val); END;
         CREATE TRIGGER kv_ad AFTER DELETE ON kv BEGIN INSERT INTO kv_log VALUES ('del:' || OLD.k || ':' || OLD.val); END;",
    )
    .unwrap();
    conn.execute("INSERT INTO kv VALUES (1, 'a')").unwrap();
    conn.execute("INSERT INTO kv VALUES (1, 'b') ON CONFLICT(k) DO UPDATE SET val = excluded.val")
        .unwrap();
    conn.execute("INSERT OR REPLACE INTO kv VALUES (1, 'c')")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT ev FROM kv_log ORDER BY rowid")
            .unwrap()
            .rows,
        &[
            vec![s("ins:1:a")],
            vec![s("upd:a->b")],
            vec![s("del:1:b")],
            vec![s("ins:1:c")],
        ],
    );

    // ALTER TABLE RENAME TABLE + RENAME COLUMN rewrite triggers, and VACUUM preserves them!
    conn.execute("ALTER TABLE kv RENAME COLUMN val TO payload")
        .unwrap();
    conn.execute("ALTER TABLE kv RENAME TO kv2").unwrap();
    conn.execute("VACUUM").unwrap();
    conn.execute("DELETE FROM kv_log").unwrap();
    conn.execute("UPDATE kv2 SET payload = 'd' WHERE k = 1")
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT ev FROM kv_log").unwrap().rows,
        &[vec![s("upd:c->d")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![s("ok")]],
    );
}

#[test]
fn foreign_keys_enforcement_actions_deferred_and_check() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
         CREATE TABLE child_noact (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id));
         CREATE TABLE child_casc (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE ON UPDATE CASCADE);
         CREATE TABLE child_null (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL ON UPDATE SET NULL);
         CREATE TABLE child_dflt (id INTEGER PRIMARY KEY, pid INT DEFAULT 99 REFERENCES parent(id) ON DELETE SET DEFAULT ON UPDATE SET DEFAULT);
         CREATE TABLE child_rest (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE RESTRICT ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED);",
    )
    .unwrap();

    // Default `PRAGMA foreign_keys` is OFF -> orphan insert succeeds, then `PRAGMA foreign_key_check` catches it!
    assert_rows_eq(
        &conn.execute("PRAGMA foreign_keys").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute("INSERT INTO child_noact VALUES (1, 777)")
        .unwrap();
    let fkc = conn.execute("PRAGMA foreign_key_check").unwrap();
    assert_eq!(fkc.columns, vec!["table", "rowid", "parent", "fkid"]);
    assert_rows_eq(&fkc.rows, &[vec![s("child_noact"), i(1), s("parent"), i(0)]]);
    let fkc_tvf = conn
        .query("SELECT * FROM pragma_foreign_key_check('child_noact')")
        .unwrap();
    assert_rows_eq(&fkc_tvf.rows, &fkc.rows);
    conn.execute("DELETE FROM child_noact").unwrap();

    // Enable foreign keys (verify toggling inside a transaction is a no-op!).
    conn.execute("PRAGMA foreign_keys = ON").unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA foreign_keys").unwrap().rows,
        &[vec![i(1)]],
    );
    conn.execute("BEGIN; PRAGMA foreign_keys = OFF; COMMIT;")
        .unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA foreign_keys").unwrap().rows,
        &[vec![i(1)]],
    );

    // Outgoing FK check on INSERT / UPDATE / OR IGNORE / NULL MATCH SIMPLE.
    conn.execute("INSERT INTO parent VALUES (1, 'p1'), (2, 'p2'), (99, 'fallback')")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO child_noact VALUES (10, 404)")
        .is_err());
    // Per SQLite: INSERT OR IGNORE still enforces foreign keys (does not suppress FK errors).
    assert!(conn
        .execute("INSERT OR IGNORE INTO child_noact VALUES (10, 404), (11, 1), (12, NULL)")
        .is_err());
    conn.execute("INSERT INTO child_noact VALUES (11, 1), (12, NULL)")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, pid FROM child_noact ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(11), i(1)], vec![i(12), SqlValue::Null]],
    );

    // ON UPDATE CASCADE, SET NULL, SET DEFAULT!
    conn.execute("INSERT INTO child_casc VALUES (1, 2)")
        .unwrap();
    conn.execute("INSERT INTO child_null VALUES (1, 2)")
        .unwrap();
    conn.execute("INSERT INTO child_dflt VALUES (1, 2)")
        .unwrap();
    conn.execute("UPDATE parent SET id = 20 WHERE id = 2")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT pid FROM child_casc WHERE id = 1")
            .unwrap()
            .rows,
        &[vec![i(20)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT pid FROM child_null WHERE id = 1")
            .unwrap()
            .rows,
        &[vec![SqlValue::Null]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT pid FROM child_dflt WHERE id = 1")
            .unwrap()
            .rows,
        &[vec![i(99)]],
    );

    // ON DELETE CASCADE, SET NULL, SET DEFAULT!
    conn.execute("UPDATE child_null SET pid = 20 WHERE id = 1")
        .unwrap();
    conn.execute("UPDATE child_dflt SET pid = 20 WHERE id = 1")
        .unwrap();
    conn.execute("DELETE FROM parent WHERE id = 20").unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM child_casc").unwrap().rows,
        &[vec![i(0)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT pid FROM child_null WHERE id = 1")
            .unwrap()
            .rows,
        &[vec![SqlValue::Null]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT pid FROM child_dflt WHERE id = 1")
            .unwrap()
            .rows,
        &[vec![i(99)]],
    );

    // Per SQLite: PRAGMA defer_foreign_keys = ON defers ON DELETE RESTRICT until COMMIT!
    conn.execute("INSERT INTO child_rest VALUES (1, 1)")
        .unwrap();
    conn.execute("BEGIN; PRAGMA defer_foreign_keys = ON;")
        .unwrap();
    conn.execute("DELETE FROM parent WHERE id = 1").unwrap();
    let err = conn.execute("COMMIT").unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY constraint failed"));
    conn.execute("ROLLBACK").unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA defer_foreign_keys").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute("DELETE FROM child_rest").unwrap();

    // Deferred FK check: temporary violation inside transaction -> fails COMMIT until fixed!
    conn.execute("BEGIN; PRAGMA defer_foreign_keys = ON;")
        .unwrap();
    conn.execute("INSERT INTO child_noact VALUES (20, 500)")
        .unwrap();
    let err = conn.execute("COMMIT").unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY constraint failed"));
    assert!(conn.in_transaction());
    // Fix violation by inserting parent 500, then COMMIT succeeds!
    conn.execute("INSERT INTO parent VALUES (500, 'p500')")
        .unwrap();
    conn.execute("COMMIT").unwrap();
    assert!(!conn.in_transaction());
    assert_rows_eq(
        &conn.execute("PRAGMA defer_foreign_keys").unwrap().rows,
        &[vec![i(0)]],
    );

    // Self-referencing multi-row insert (child row before parent row in same statement -> succeeds!).
    conn.execute(
        "CREATE TABLE node (id INTEGER PRIMARY KEY, parent_id INT REFERENCES node(id) ON DELETE CASCADE);
         INSERT INTO node VALUES (2, 1), (1, NULL), (3, 2);",
    )
    .unwrap();
    conn.execute("DELETE FROM node WHERE id = 1").unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM node").unwrap().rows,
        &[vec![i(0)]],
    );

    // DROP TABLE parent fails while child references it!
    assert!(conn.execute("DROP TABLE parent").is_err());

    // ALTER TABLE RENAME TABLE updates child REFERENCES DDL; DROP COLUMN on table-level FK fails
    // while inline REFERENCES column drop succeeds (matching C SQLite).
    conn.execute("ALTER TABLE parent RENAME TO parent2")
        .unwrap();
    conn.execute(
        "CREATE TABLE child_tbl_fk (id INTEGER PRIMARY KEY, pid INT, FOREIGN KEY(pid) REFERENCES parent2(id));",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE child_tbl_fk DROP COLUMN pid")
        .is_err());
    assert!(conn
        .execute("INSERT INTO child_noact VALUES (99, 99999)")
        .is_err());
    conn.execute("INSERT INTO parent2 VALUES (99999, 'ok')")
        .unwrap();
    conn.execute("INSERT INTO child_noact VALUES (99, 99999)")
        .unwrap();
    assert!(conn
        .execute("PRAGMA foreign_key_check")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn reindex_analyze_vacuum_into_eqp_alter_and_pragmas() {
    let vfs = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    let mut conn =
        SqliteConnection::open(vfs.clone(), "main.db", ConnectionOptions::default()).unwrap();

    // journal_mode is "delete" for VFS-backed, "memory" for in-memory!
    assert_rows_eq(
        &conn.execute("PRAGMA journal_mode").unwrap().rows,
        &[vec![s("delete")]],
    );
    let mut mem_conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    assert_rows_eq(
        &mem_conn.execute("PRAGMA journal_mode").unwrap().rows,
        &[vec![s("memory")]],
    );

    conn.execute(
        "CREATE TABLE t (
            id INTEGER PRIMARY KEY,
            a INT NOT NULL,
            b TEXT COLLATE NOCASE,
            CONSTRAINT chk_a CHECK(a > 0),
            CONSTRAINT uq_b UNIQUE(b)
         );
         CREATE INDEX idx_t_a ON t(a);
         CREATE INDEX idx_t_ab ON t(a, b);
         CREATE TABLE empty_t (x INT);
         CREATE VIEW vt AS SELECT id, a, b FROM t;
         INSERT INTO t VALUES
            (1, 1, 'x'), (2, 1, 'y'), (3, 1, 'z'),
            (4, 2, 'u'), (5, 2, 'v'), (6, 2, 'w');",
    )
    .unwrap();

    // ANALYZE creates & populates `sqlite_stat1` with exact SQLite prefix averages!
    conn.execute("ANALYZE").unwrap();
    let stats = conn
        .query("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx")
        .unwrap();
    // 6 rows, 2 distinct `a` -> avg 3; 6 distinct `(a, b)` -> avg 1; `uq_b` has 6 distinct `b` -> avg 1!
    assert_eq!(stats.rows.len(), 3);
    assert!(stats
        .rows
        .iter()
        .any(|r| row_exact_eq(r, &[s("t"), s("idx_t_a"), s("6 3")])));
    assert!(stats
        .rows
        .iter()
        .any(|r| row_exact_eq(r, &[s("t"), s("idx_t_ab"), s("6 3 1")])));
    // Empty table must NOT have a sqlite_stat1 row.
    assert!(!stats.rows.iter().any(|r| r[0].exact_eq(&s("empty_t"))));
    assert!(conn.execute("ANALYZE no_such_table").is_err());

    // REINDEX (all, table, index, collation, invalid target).
    conn.execute("REINDEX").unwrap();
    conn.execute("REINDEX t").unwrap();
    conn.execute("REINDEX idx_t_a").unwrap();
    conn.execute("REINDEX NOCASE").unwrap();
    let err = conn.execute("REINDEX no_such_obj").unwrap_err();
    assert!(err
        .to_string()
        .contains("unable to identify the object to be reindexed"));
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![s("ok")]],
    );

    // VACUUM INTO 'backup.db' writes a fresh compacted database file & rejects existing file!
    conn.execute("VACUUM INTO 'backup.db'").unwrap();
    let err = conn.execute("VACUUM INTO 'backup.db'").unwrap_err();
    assert!(err.to_string().contains("output file already exists"));
    assert!(mem_conn.execute("VACUUM INTO 'x.db'").is_err());

    let backup =
        SqliteConnection::open(vfs.clone(), "backup.db", ConnectionOptions::default()).unwrap();
    assert_rows_eq(
        &backup.query("SELECT count(*) FROM t").unwrap().rows,
        &[vec![i(6)]],
    );
    assert_rows_eq(
        &backup
            .query("SELECT count(*) FROM sqlite_stat1")
            .unwrap()
            .rows,
        &[vec![i(3)]],
    );
    drop(backup);

    // EXPLAIN QUERY PLAN (both SqliteConnection and read-only SqliteDatabase!).
    let eqp_ipk = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM t WHERE id = 3")
        .unwrap();
    assert_eq!(eqp_ipk.columns, vec!["id", "parent", "notused", "detail"]);
    assert!(eqp_ipk.rows[0][3]
        .as_str()
        .unwrap()
        .contains("USING INTEGER PRIMARY KEY"));

    let eqp_cov = conn
        .execute("EXPLAIN QUERY PLAN SELECT a FROM t WHERE a = 1")
        .unwrap();
    assert!(eqp_cov.rows[0][3]
        .as_str()
        .unwrap()
        .contains("USING COVERING INDEX idx_t_a"));

    let eqp_cte = conn
        .execute("EXPLAIN QUERY PLAN WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id")
        .unwrap();
    assert!(eqp_cte
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("MATERIALIZE c")));
    assert!(eqp_cte
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USE TEMP B-TREE FOR ORDER BY")));

    // Read-only SqliteDatabase::query also supports EXPLAIN QUERY PLAN!
    let ro_db = open_database(vfs.as_ref(), "backup.db", OpenOptions::default()).unwrap();
    let ro_eqp = ro_db
        .query("EXPLAIN QUERY PLAN SELECT id FROM t WHERE a = 2")
        .unwrap();
    assert_eq!(ro_eqp.columns, vec!["id", "parent", "notused", "detail"]);
    assert!(ro_eqp.rows[0][3]
        .as_str()
        .unwrap()
        .contains("USING COVERING INDEX idx_t_a"));

    // Remaining ALTER TABLE actions: DROP NOT NULL, SET NOT NULL, ADD CHECK, DROP CONSTRAINT.
    conn.execute("ALTER TABLE t ALTER COLUMN a DROP NOT NULL")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (7, NULL, 'k')").unwrap();
    // SET NOT NULL fails while NULL row exists!
    assert!(conn
        .execute("ALTER TABLE t ALTER COLUMN a SET NOT NULL")
        .is_err());
    conn.execute("UPDATE t SET a = 3 WHERE id = 7").unwrap();
    conn.execute("ALTER TABLE t ALTER COLUMN a SET NOT NULL")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO t VALUES (8, NULL, 'm')")
        .is_err());

    // ADD CHECK validates existing rows & enforces on future rows.
    assert!(conn
        .execute("ALTER TABLE t ADD CONSTRAINT chk_small CHECK(a < 2)")
        .is_err());
    conn.execute("ALTER TABLE t ADD CONSTRAINT chk_max CHECK(a < 100)")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO t VALUES (8, 200, 'm')")
        .is_err());

    // DROP CONSTRAINT removes named CHECK and named UNIQUE constraints cleanly!
    conn.execute("ALTER TABLE t DROP CONSTRAINT chk_max")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (8, 200, 'm')").unwrap();
    conn.execute("ALTER TABLE t DROP CONSTRAINT uq_b").unwrap();
    conn.execute("INSERT INTO t VALUES (9, 5, 'm')").unwrap();
    assert!(conn
        .execute("ALTER TABLE t DROP CONSTRAINT no_such_constraint")
        .is_err());
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![s("ok")]],
    );

    // New PRAGMAs: case_sensitive_like, ignore_check_constraints, query_only, list PRAGMAs, view ncol.
    assert_rows_eq(
        &conn.query("SELECT 'abc' LIKE 'A%'").unwrap().rows,
        &[vec![i(1)]],
    );
    conn.execute("PRAGMA case_sensitive_like = ON").unwrap();
    assert_rows_eq(
        &conn.query("SELECT 'abc' LIKE 'A%'").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute("PRAGMA case_sensitive_like = OFF").unwrap();

    conn.execute("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (10, -5, 'neg')")
        .unwrap();
    conn.execute("PRAGMA ignore_check_constraints = OFF")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO t VALUES (11, -5, 'neg2')")
        .is_err());

    conn.execute("PRAGMA query_only = ON").unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA query_only").unwrap().rows,
        &[vec![i(1)]],
    );
    let err = conn.execute("DELETE FROM t WHERE id = 1").unwrap_err();
    assert!(err.to_string().contains("readonly"));
    conn.execute("PRAGMA query_only = OFF").unwrap();

    let colls = conn.execute("PRAGMA collation_list").unwrap();
    assert_eq!(colls.rows.len(), 3);
    assert!(!conn.execute("PRAGMA compile_options").unwrap().rows.is_empty());
    assert!(conn.execute("PRAGMA pragma_list").unwrap().rows.len() >= 25);
    assert!(conn.execute("PRAGMA function_list").unwrap().rows.len() >= 80);

    // `PRAGMA table_list` returns accurate `ncol = 3` for view `vt` (not 0!).
    let vlist = conn.query("SELECT ncol FROM pragma_table_list('vt')").unwrap();
    assert_rows_eq(&vlist.rows, &[vec![i(3)]]);
}

#[test]
fn direct_fast_scan_kernels_parity_and_edge_cases() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE facts (id INTEGER PRIMARY KEY, grp INT, val INT, note TEXT);
         CREATE TABLE dims (grp INT PRIMARY KEY, label TEXT);
         CREATE TABLE filter_vals (v INT);
         CREATE TABLE empty_tbl (id INTEGER PRIMARY KEY, grp INT, val INT);",
    )
    .unwrap();

    // Populate with NULLs, floats, negative keys, large keys (> 16384), and >4 KiB overflow text!
    let big_text = "Z".repeat(5000);
    conn.execute("BEGIN").unwrap();
    for k in 1..=30i64 {
        let g = match k {
            5 => SqlValue::Null,
            6 => SqlValue::Real(2.0), // Must merge with Int(2) in GROUP BY & JOIN!
            7 => SqlValue::Int(-5),
            8 => SqlValue::Int(50_000),
            _ => SqlValue::Int(k % 4),
        };
        let v = match k {
            10 => SqlValue::Null,
            11 => SqlValue::Real(2.5),
            _ => SqlValue::Int(k * 10),
        };
        let note = if k == 12 {
            SqlValue::text(big_text.as_str())
        } else {
            SqlValue::text(format!("n{k}"))
        };
        conn.execute_with_params(
            "INSERT INTO facts VALUES (?1, ?2, ?3, ?4)",
            &QueryParams::positional(vec![SqlValue::Int(k), g, v, note]),
        )
        .unwrap();
    }
    for (g, lbl) in [
        (0, "zero"),
        (1, "one"),
        (2, "two"),
        (-5, "neg"),
        (50_000, "huge"),
    ] {
        conn.execute_with_params(
            "INSERT INTO dims VALUES (?1, ?2)",
            &QueryParams::positional(vec![SqlValue::Int(g), SqlValue::text(lbl)]),
        )
        .unwrap();
    }
    for fv in [10, 30, 120, 200, 99_999] {
        conn.execute_with_params(
            "INSERT INTO filter_vals VALUES (?1)",
            &QueryParams::positional(vec![SqlValue::Int(fv)]),
        )
        .unwrap();
    }
    conn.execute("INSERT INTO filter_vals VALUES (NULL)")
        .unwrap();
    conn.execute("COMMIT").unwrap();

    // Helper: compare fast-path query against a subselect-wrapped general SQL engine query.
    let assert_parity = |conn: &SqliteConnection, fast_sql: &str, slow_sql: &str, sort: bool| {
        let mut a = conn.query(fast_sql).unwrap().rows;
        let mut b = conn.query(slow_sql).unwrap().rows;
        if sort {
            let key = |r: &Vec<SqlValue>| format!("{r:?}");
            a.sort_by_key(key);
            b.sort_by_key(key);
        }
        assert_rows_eq(&a, &b);
    };

    // 1. RowidRange (BETWEEN, >= AND <=, LIMIT/OFFSET, empty range, overflow payload row 12).
    assert_parity(
        &conn,
        "SELECT id, val, note FROM facts WHERE id BETWEEN 8 AND 15",
        "SELECT id, val, note FROM (SELECT * FROM facts) WHERE id BETWEEN 8 AND 15",
        false,
    );
    assert_parity(
        &conn,
        "SELECT id, val FROM facts WHERE id >= 5 AND id < 12 ORDER BY id LIMIT 4 OFFSET 2",
        "SELECT id, val FROM (SELECT * FROM facts) WHERE id >= 5 AND id < 12 ORDER BY id LIMIT 4 OFFSET 2",
        false,
    );
    assert_parity(
        &conn,
        "SELECT id FROM facts WHERE id BETWEEN 100 AND 50",
        "SELECT id FROM (SELECT * FROM facts) WHERE id BETWEEN 100 AND 50",
        false,
    );

    // 2. GroupBySingleCol (NULL group, Int(2)+Real(2.0) merge, neg/huge keys, float AVG/SUM, empty table).
    assert_parity(
        &conn,
        "SELECT grp, count(*), count(val), sum(val), total(val), avg(val) FROM facts GROUP BY grp",
        "SELECT grp, count(*), count(val), sum(val), total(val), avg(val) FROM (SELECT * FROM facts) GROUP BY grp",
        true,
    );
    assert_parity(
        &conn,
        "SELECT grp, count(*) FROM empty_tbl GROUP BY grp",
        "SELECT grp, count(*) FROM (SELECT * FROM empty_tbl) GROUP BY grp",
        true,
    );

    // 3. CumulativeWindowIpk (NULLs, float promotion on row 11, id <= 20 and unbounded).
    assert_parity(
        &conn,
        "SELECT id, sum(val) OVER (ORDER BY id) FROM facts WHERE id <= 20",
        "SELECT id, sum(val) OVER (ORDER BY id) FROM (SELECT * FROM facts) WHERE id <= 20",
        false,
    );
    assert_parity(
        &conn,
        "SELECT id, val, total(val) OVER (ORDER BY id) FROM facts",
        "SELECT id, val, total(val) OVER (ORDER BY id) FROM (SELECT * FROM facts)",
        false,
    );

    // 4. HashEquiJoin2 (INNER JOIN & LEFT JOIN, NULL keys, Real(2.0)=Int(2), neg/huge keys, overflow rows).
    assert_parity(
        &conn,
        "SELECT f.id, f.val, d.label FROM facts f JOIN dims d ON f.grp = d.grp",
        "SELECT f.id, f.val, d.label FROM (SELECT * FROM facts) f JOIN dims d ON f.grp = d.grp",
        true,
    );
    assert_parity(
        &conn,
        "SELECT f.id, f.note, d.label FROM facts f LEFT JOIN dims d ON f.grp = d.grp",
        "SELECT f.id, f.note, d.label FROM (SELECT * FROM facts) f LEFT JOIN dims d ON f.grp = d.grp",
        true,
    );

    // 5. CountInSubquery (with rowid bound, without rowid bound, NULLs in both tables, empty table).
    assert_parity(
        &conn,
        "SELECT count(*) FROM facts WHERE id <= 20 AND val IN (SELECT v FROM filter_vals)",
        "SELECT count(*) FROM (SELECT * FROM facts) WHERE id <= 20 AND val IN (SELECT v FROM filter_vals)",
        false,
    );
    assert_parity(
        &conn,
        "SELECT count(*) FROM facts WHERE val IN (SELECT v FROM filter_vals)",
        "SELECT count(*) FROM (SELECT * FROM facts) WHERE val IN (SELECT v FROM filter_vals)",
        false,
    );

    // Verify integer overflow detection in direct SUM fast path matches SQLite!
    conn.execute("CREATE TABLE ovf (g INT, x INT); INSERT INTO ovf VALUES (1, 9223372036854775807), (1, 1);")
        .unwrap();
    let err = conn
        .query("SELECT g, sum(x) FROM ovf GROUP BY g")
        .unwrap_err();
    assert!(err.to_string().contains("integer overflow"));

    // Verify sandbox meter halts direct fast scans cleanly!
    conn.execute("CREATE TABLE big_facts (id INTEGER PRIMARY KEY, grp INT, val INT);")
        .unwrap();
    let mut big_sql = String::from("BEGIN; ");
    for i in 1..=300 {
        big_sql.push_str(&format!(
            "INSERT INTO big_facts VALUES ({i}, {}, {});",
            (i % 4) + 1,
            i * 3
        ));
    }
    big_sql.push_str(" COMMIT;");
    conn.execute(&big_sql).unwrap();

    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(200),
        max_memory_bytes: Some(1024 * 1024),
        max_page_reads: Some(1000),
    }));
    let bytes = conn.to_bytes();
    let mut mconn = SqliteConnection::open_from_bytes(
        &bytes,
        None,
        None,
        ConnectionOptions {
            meter: Some(meter),
            ..ConnectionOptions::default()
        },
    )
    .unwrap();
    let err = mconn
        .query("SELECT f.id, f.val, d.label FROM big_facts f JOIN dims d ON f.grp = d.grp")
        .unwrap_err();
    assert!(
        err.to_string().to_ascii_lowercase().contains("limit")
            || err.to_string().to_ascii_lowercase().contains("step"),
        "expected meter error, got: {err}"
    );
    let _ = mconn.close();
}

#[test]
fn c_sqlite_verifies_triggers_fks_analyze_reindex_and_vacuum_images() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE books (
            id INTEGER PRIMARY KEY,
            author_id INT REFERENCES authors(id) ON DELETE CASCADE ON UPDATE CASCADE,
            title TEXT NOT NULL
         );
         CREATE INDEX idx_books_author ON books(author_id);
         CREATE TABLE audit (msg TEXT);
         CREATE TRIGGER tr_book_ins AFTER INSERT ON books BEGIN
            INSERT INTO audit VALUES ('book:' || NEW.title);
         END;
         INSERT INTO authors VALUES (1, 'Ada'), (2, 'Grace');
         INSERT INTO books VALUES (10, 1, 'Notes'), (11, 2, 'COBOL');
         UPDATE authors SET id = 20 WHERE id = 2;
         ANALYZE;
         REINDEX;
         VACUUM;",
    )
    .unwrap();

    let bytes = conn.to_bytes();
    let tmp = std::env::temp_dir().join(format!(
        "xdb_verify_triggers_fk_{}.sqlite",
        std::process::id()
    ));
    std::fs::write(&tmp, &bytes).unwrap();

    // Cross-verify the binary SQLite file with real C SQLite via `node:sqlite` when Node 22+ is available.
    let script = format!(
        r#"const {{ DatabaseSync }} = require('node:sqlite');
const db = new DatabaseSync({});
const ic = db.prepare('PRAGMA integrity_check').all();
if (ic.length !== 1 || ic[0].integrity_check !== 'ok') throw new Error('integrity: ' + JSON.stringify(ic));
const fkc = db.prepare('PRAGMA foreign_key_check').all();
if (fkc.length !== 0) throw new Error('fkc: ' + JSON.stringify(fkc));
db.exec('PRAGMA foreign_keys = ON');
db.exec("INSERT INTO books VALUES (12, 20, 'Compilers')");
const audit = db.prepare('SELECT msg FROM audit ORDER BY rowid').all().map(r => r.msg);
if (JSON.stringify(audit) !== JSON.stringify(['book:Notes','book:COBOL','book:Compilers'])) throw new Error('audit: ' + JSON.stringify(audit));
const stats = db.prepare('SELECT tbl, idx, stat FROM sqlite_stat1').all();
if (stats.length < 1) throw new Error('missing sqlite_stat1');
console.log('C_SQLITE_OK');"#,
        serde_json_escape(&tmp.to_string_lossy())
    );

    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output();
    let _ = std::fs::remove_file(&tmp);

    if let Ok(output) = out {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("C_SQLITE_OK"), "stdout={stdout}");
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("Cannot find module 'node:sqlite'") {
                panic!("C SQLite verification failed: {stderr}");
            }
        }
    }
}

fn serde_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn audit_explain_and_pragma_safety_and_parity() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. EXPLAIN QUERY PLAN: cyclic views return error (no stack overflow!).
    conn.execute("CREATE VIEW v_self AS SELECT * FROM v_self;").unwrap();
    let err = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM v_self")
        .unwrap_err();
    assert!(
        err.to_string().contains("circularly defined"),
        "expected circularly defined, got: {err}"
    );
    conn.execute(
        "CREATE VIEW v_a AS SELECT * FROM v_b;
         CREATE VIEW v_b AS SELECT * FROM v_a;",
    )
    .unwrap();
    assert!(conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM v_a")
        .unwrap_err()
        .to_string()
        .contains("circularly defined"));

    // 2. EXPLAIN QUERY PLAN: view DAG bomb halted cleanly by BudgetMeter.
    let mut dag_builder = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    dag_builder.execute("CREATE VIEW d0 AS SELECT 1 AS a;").unwrap();
    for k in 1..=16 {
        dag_builder
            .execute(&format!(
                "CREATE VIEW d{k} AS SELECT * FROM d{} JOIN d{} AS b;",
                k - 1,
                k - 1
            ))
            .unwrap();
    }
    let dag_bytes = dag_builder.to_bytes();
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(4000),
        max_memory_bytes: Some(2 * 1024 * 1024),
        max_page_reads: Some(2000),
    }));
    let mut mconn = SqliteConnection::open_from_bytes(
        &dag_bytes,
        None,
        None,
        ConnectionOptions {
            meter: Some(meter),
            ..ConnectionOptions::default()
        },
    )
    .unwrap();
    let dag_err = mconn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM d16")
        .unwrap_err();
    assert!(
        dag_err.to_string().to_ascii_lowercase().contains("limit")
            || dag_err.to_string().to_ascii_lowercase().contains("step"),
        "expected meter limit error on view DAG EQP, got: {dag_err}"
    );

    // 3. EXPLAIN QUERY PLAN: CTE scoping, real `oid` column, covering index, INDEXED BY.
    conn.execute(
        "CREATE TABLE real_oid (oid TEXT, a INT, b INT);
         CREATE INDEX idx_a ON real_oid(a);
         CREATE VIEW v_ro AS SELECT a, b FROM real_oid;",
    )
    .unwrap();
    // DML WITH CTE must resolve in EQP.
    conn.execute("EXPLAIN QUERY PLAN WITH c AS (SELECT 'x', 1, 2) INSERT INTO real_oid SELECT * FROM c")
        .unwrap();
    // Qualified `main.real_oid` must not match CTE `real_oid`.
    let eqp_qual = conn
        .execute("EXPLAIN QUERY PLAN WITH real_oid AS (SELECT 1) SELECT * FROM main.real_oid WHERE a = 1")
        .unwrap();
    assert!(
        eqp_qual
            .rows
            .iter()
            .any(|r| r[3].as_str().unwrap_or("").contains("USING INDEX idx_a")),
        "expected main.real_oid to use idx_a, got: {:?}",
        eqp_qual.rows
    );
    // Outer CTE must not shadow table inside view.
    let eqp_view = conn
        .execute("EXPLAIN QUERY PLAN WITH real_oid AS (SELECT 1 AS a, 2 AS b) SELECT * FROM v_ro")
        .unwrap();
    assert!(eqp_view
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap_or("").contains("SCAN real_oid")));
    // Real column named `oid` must NOT report USING INTEGER PRIMARY KEY.
    let eqp_oid = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM real_oid WHERE oid = 'abc'")
        .unwrap();
    assert!(
        !eqp_oid
            .rows
            .iter()
            .any(|r| r[3].as_str().unwrap_or("").contains("INTEGER PRIMARY KEY")),
        "real column oid wrongly reported IPK: {:?}",
        eqp_oid.rows
    );
    // Non-indexed column in WHERE must NOT report COVERING INDEX.
    let eqp_cov = conn
        .execute("EXPLAIN QUERY PLAN SELECT a FROM real_oid WHERE a = 1 AND b = 2")
        .unwrap();
    assert!(
        !eqp_cov
            .rows
            .iter()
            .any(|r| r[3].as_str().unwrap_or("").contains("COVERING")),
        "non-covered WHERE b=2 falsely reported COVERING: {:?}",
        eqp_cov.rows
    );
    // INDEXED BY nonexistent index must error.
    assert!(conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM real_oid INDEXED BY no_such_idx")
        .is_err());

    // 4. PRAGMA table_list: self-referencing pragma_table_list view (no stack overflow!)
    //    and runtime-failing view still reports accurate ncol without materializing!
    conn.execute(
        "CREATE TABLE two_rows (x INT); INSERT INTO two_rows VALUES (1), (2);
         CREATE VIEW v_ptl AS SELECT * FROM pragma_table_list;
         CREATE VIEW v_err AS SELECT (SELECT x FROM two_rows) AS c, 42 AS d;",
    )
    .unwrap();
    let tl = conn.execute("PRAGMA table_list").unwrap();
    let ptl_row = tl
        .rows
        .iter()
        .find(|r| r[1].as_str() == Some("v_ptl"))
        .expect("v_ptl present");
    assert_eq!(ptl_row[3].as_i64(), Some(6));
    let err_row = tl
        .rows
        .iter()
        .find(|r| r[1].as_str() == Some("v_err"))
        .expect("v_err present");
    assert_eq!(err_row[3].as_i64(), Some(2));

    // 5. PRAGMA compile_options matches sqlite_compileoption_get / sqlite_compileoption_used.
    let co = conn.execute("PRAGMA compile_options").unwrap();
    assert!(!co.rows.is_empty());
    for (idx, row) in co.rows.iter().enumerate() {
        let opt = row[0].as_str().unwrap();
        let got = conn
            .query(&format!("SELECT sqlite_compileoption_get({idx}), sqlite_compileoption_used('{opt}')"))
            .unwrap();
        assert_eq!(got.rows[0][0].as_str(), Some(opt));
        assert_eq!(got.rows[0][1].as_i64(), Some(1));
    }

    // 6. PRAGMA function_list & collation_list parity (including custom functions/collations).
    let fl = conn.execute("PRAGMA function_list").unwrap();
    assert!(fl
        .rows
        .iter()
        .any(|r| r[0].as_str() == Some("iif") && r[4].as_i64() == Some(-1)));
    assert!(fl.rows.iter().any(|r| r[0].as_str() == Some("median")));
    assert!(fl.rows.iter().any(|r| r[0].as_str() == Some("unistr")));
    let mut custom_opts = xdb_sql::QueryOptions::default();
    custom_opts.functions.register_scalar(
        "my_double",
        Some(1),
        Arc::new(|args| Ok(SqlValue::Int(args[0].as_i64().unwrap_or(0) * 2))),
    );
    custom_opts
        .collations
        .register("MY_REV", Arc::new(|a, b| b.cmp(a)));
    let view = conn.live_view().unwrap();
    let fl_custom = view
        .query_with_options(
            "SELECT * FROM pragma_function_list",
            &QueryParams::empty(),
            &custom_opts,
        )
        .unwrap();
    assert!(fl_custom
        .rows
        .iter()
        .any(|r| r[0].as_str() == Some("my_double") && r[1].as_i64() == Some(0)));
    let cl_custom = view
        .query_with_options(
            "SELECT * FROM pragma_collation_list",
            &QueryParams::empty(),
            &custom_opts,
        )
        .unwrap();
    assert!(cl_custom.rows.iter().any(|r| r[1].as_str() == Some("MY_REV")));
    drop(view);

    // 7. PRAGMA boolean integer parsing (`2 -> 1`, `-1 -> 0`, `0 -> 0`).
    conn.execute("PRAGMA foreign_keys = 2").unwrap();
    assert_eq!(
        conn.execute("PRAGMA foreign_keys").unwrap().rows[0][0].as_i64(),
        Some(1)
    );
    conn.execute("PRAGMA foreign_keys = -1").unwrap();
    assert_eq!(
        conn.execute("PRAGMA foreign_keys").unwrap().rows[0][0].as_i64(),
        Some(0)
    );

    // 8. PRAGMA foreign_key_check referencing a VIEW returns foreign key mismatch.
    conn.execute(
        "CREATE TABLE pv_base(id INT PRIMARY KEY);
         CREATE VIEW pv_view AS SELECT id FROM pv_base;
         CREATE TABLE fk_to_view(pid INT REFERENCES pv_view(id));
         INSERT INTO fk_to_view VALUES (1);",
    )
    .unwrap();
    let fkc_err = conn
        .execute("PRAGMA foreign_key_check(fk_to_view)")
        .unwrap_err();
    assert!(
        fkc_err.to_string().contains("foreign key mismatch"),
        "expected foreign key mismatch, got: {fkc_err}"
    );
}

#[test]
fn audit_triggers_raise_and_dml_corruption_fixes() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. Stale known_max_rowid after BEFORE INSERT RAISE(IGNORE) must NOT overwrite or corrupt index.
    conn.execute(
        "CREATE TABLE t_ign(id INTEGER PRIMARY KEY, x TEXT);
         CREATE INDEX idx_t_ign ON t_ign(x);
         CREATE TRIGGER tr_ign BEFORE INSERT ON t_ign WHEN NEW.x = 'skip' BEGIN
            INSERT INTO t_ign(id, x) VALUES (2, 'from_trig');
            SELECT RAISE(IGNORE);
         END;
         INSERT INTO t_ign(x) VALUES ('row1'), ('skip'), ('row3');",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, x FROM t_ign ORDER BY id").unwrap().rows,
        &[
            vec![i(1), t("row1")],
            vec![i(2), t("from_trig")],
            vec![i(3), t("row3")],
        ],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 2. INSERT OR REPLACE + recursive_triggers = ON + BEFORE DELETE RAISE(IGNORE) must fail UNIQUE,
    //    NOT write a duplicate row / corrupt the UNIQUE index!
    conn.execute(
        "PRAGMA recursive_triggers = ON;
         CREATE TABLE r_uni(id INT PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO r_uni VALUES (1, 10, 'old');
         CREATE TRIGGER tr_r_bd BEFORE DELETE ON r_uni BEGIN SELECT RAISE(IGNORE); END;",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT OR REPLACE INTO r_uni VALUES (2, 10, 'new')")
        .unwrap_err()
        .to_string()
        .contains("UNIQUE"));
    assert_rows_eq(
        &conn.query("SELECT id, u, v FROM r_uni").unwrap().rows,
        &[vec![i(1), i(10), t("old")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );
    conn.execute("PRAGMA recursive_triggers = OFF;").unwrap();

    // 3. BEFORE UPDATE / BEFORE DELETE mutating an indexed column preserves index integrity!
    conn.execute(
        "CREATE TABLE mu(id INTEGER PRIMARY KEY, a INT, y TEXT);
         CREATE INDEX idx_mu_y ON mu(y);
         INSERT INTO mu VALUES (1, 10, 'init');
         CREATE TRIGGER tr_mu_bu BEFORE UPDATE OF a ON mu BEGIN
            UPDATE mu SET y = 'from_bu' WHERE id = OLD.id;
         END;
         UPDATE mu SET a = 20 WHERE id = 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, a, y FROM mu WHERE y = 'from_bu'").unwrap().rows,
        &[vec![i(1), i(20), t("from_bu")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 4. Static RAISE() check outside triggers & non-leakage into views/CHECK/DEFAULT/GENERATED!
    conn.execute("CREATE TABLE empty_r(x INT);").unwrap();
    assert!(conn
        .query("SELECT RAISE(ABORT, 'x') FROM empty_r")
        .unwrap_err()
        .to_string()
        .contains("trigger-program"));
    assert!(conn
        .query("SELECT CASE WHEN 0 THEN RAISE(ABORT, 'x') ELSE 1 END")
        .unwrap_err()
        .to_string()
        .contains("trigger-program"));
    conn.execute(
        "CREATE VIEW v_illegal_raise AS SELECT RAISE(ABORT, 'bad') AS c;
         CREATE TABLE src_vr(x INT);
         CREATE TRIGGER tr_vr AFTER INSERT ON src_vr BEGIN
            SELECT * FROM v_illegal_raise;
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO src_vr VALUES (1)")
        .unwrap_err()
        .to_string()
        .contains("trigger-program"));

    // 5. RAISE(ROLLBACK) & RAISE(IGNORE) inside trigger UPDATE SET, UPDATE ... FROM (>=16 rows!), UPSERT.
    conn.execute(
        "CREATE TABLE tgt_rb(a INT); INSERT INTO tgt_rb VALUES (1);
         CREATE TABLE src_rb(x INT);
         CREATE TRIGGER tr_rb AFTER INSERT ON src_rb BEGIN
            UPDATE tgt_rb SET a = RAISE(ROLLBACK, 'rolled');
         END;",
    )
    .unwrap();
    conn.execute("BEGIN;").unwrap();
    assert!(conn
        .execute("INSERT INTO src_rb VALUES (1)")
        .unwrap_err()
        .to_string()
        .contains("rolled"));
    assert!(!conn.in_transaction(), "RAISE(ROLLBACK) in UPDATE SET must roll back txn");

    // UPDATE ... FROM hash-join (>=16 rows) and UPSERT DO UPDATE see trigger NEW.* scope!
    conn.execute(
        "CREATE TABLE uf_tgt(id INT PRIMARY KEY, v INT); INSERT INTO uf_tgt VALUES (1, 0);
         CREATE TABLE uf_src(id INT, k INT);
         CREATE TABLE uf_ev(x INT);
         CREATE TRIGGER tr_uf AFTER INSERT ON uf_ev BEGIN
            UPDATE uf_tgt SET v = NEW.x + s.k FROM uf_src s WHERE uf_tgt.id = s.id;
         END;",
    )
    .unwrap();
    let mut uf_sql = String::from("BEGIN;");
    for k in 1..=20 {
        uf_sql.push_str(&format!("INSERT INTO uf_src VALUES ({k}, {});", k * 10));
    }
    uf_sql.push_str("COMMIT;");
    conn.execute(&uf_sql).unwrap();
    conn.execute("INSERT INTO uf_ev VALUES (5);").unwrap();
    assert_rows_eq(
        &conn.query("SELECT v FROM uf_tgt WHERE id = 1").unwrap().rows,
        &[vec![i(15)]],
    );

    conn.execute(
        "CREATE TABLE up_tgt(id INT PRIMARY KEY, v INT); INSERT INTO up_tgt VALUES (1, 10);
         CREATE TABLE up_ev(delta INT);
         CREATE TRIGGER tr_up AFTER INSERT ON up_ev BEGIN
            INSERT INTO up_tgt VALUES (1, 0) ON CONFLICT(id) DO UPDATE SET v = up_tgt.v + NEW.delta;
         END;
         INSERT INTO up_ev VALUES (7);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT v FROM up_tgt WHERE id = 1").unwrap().rows,
        &[vec![i(17)]],
    );

    // 6. Outer INSERT OR IGNORE overrides conflict inside triggered statement.
    conn.execute(
        "CREATE TABLE ov_a(x INT);
         CREATE TABLE ov_b(x INT PRIMARY KEY); INSERT INTO ov_b VALUES (1);
         CREATE TRIGGER tr_ov AFTER INSERT ON ov_a BEGIN INSERT INTO ov_b VALUES (NEW.x); END;
         INSERT OR IGNORE INTO ov_a VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT x FROM ov_a").unwrap().rows,
        &[vec![i(1)]],
    );

    // 7. CREATE TRIGGER rejects bind parameters and RETURNING.
    assert!(conn
        .execute("CREATE TRIGGER tr_var AFTER INSERT ON ov_a BEGIN INSERT INTO ov_a VALUES (?); END;")
        .unwrap_err()
        .to_string()
        .contains("variables"));
    assert!(conn
        .execute("CREATE TRIGGER tr_ret AFTER INSERT ON ov_a BEGIN INSERT INTO ov_a VALUES (1) RETURNING x; END;")
        .unwrap_err()
        .to_string()
        .contains("RETURNING"));

    // 8. BEFORE INSERT on STRICT table sees coerced INTEGER value.
    conn.execute(
        "CREATE TABLE st_tbl(a INT) STRICT;
         CREATE TABLE st_log(tp TEXT, val ANY);
         CREATE TRIGGER tr_st BEFORE INSERT ON st_tbl BEGIN
            INSERT INTO st_log VALUES (typeof(NEW.a), NEW.a);
         END;
         INSERT INTO st_tbl VALUES ('123');",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT tp, val FROM st_log").unwrap().rows,
        &[vec![t("integer"), i(123)]],
    );

    // 9. INSTEAD OF view triggers: changes() = 0, no double total_changes, multi-col subquery SET & FROM.
    conn.execute(
        "CREATE TABLE vw_base(id INT PRIMARY KEY, a INT, b INT);
         INSERT INTO vw_base VALUES (1, 10, 20);
         CREATE VIEW vw_ed AS SELECT id, a, b FROM vw_base;
         CREATE TRIGGER tr_vw_u INSTEAD OF UPDATE ON vw_ed BEGIN
            UPDATE vw_base SET a = NEW.a, b = NEW.b WHERE id = OLD.id;
         END;",
    )
    .unwrap();
    let tc_before = conn.total_changes();
    let upd_res = conn
        .execute("UPDATE vw_ed SET (a, b) = (SELECT 100, 200) WHERE id = 1")
        .unwrap();
    assert_eq!(upd_res.changes, 0);
    assert_eq!(conn.changes(), 0);
    assert_eq!(conn.total_changes() - tc_before, 1);
    assert_rows_eq(
        &conn.query("SELECT a, b FROM vw_base WHERE id = 1").unwrap().rows,
        &[vec![i(100), i(200)]],
    );

    // 10. ALTER TABLE RENAME TO/RENAME COLUMN keyword quoting + self-ref FK rewrite!
    conn.execute(
        "CREATE TABLE kw_self(id INT PRIMARY KEY, pid INT REFERENCES kw_self(id));
         CREATE TRIGGER tr_kw AFTER INSERT ON kw_self BEGIN SELECT NEW.pid; END;
         ALTER TABLE kw_self RENAME TO \"select\";
         ALTER TABLE \"select\" RENAME COLUMN pid TO \"order\";",
    )
    .unwrap();
    conn.execute("INSERT INTO \"select\" VALUES (1, NULL), (2, 1)")
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, \"order\" FROM \"select\" ORDER BY id").unwrap().rows,
        &[vec![i(1), SqlValue::Null], vec![i(2), i(1)]],
    );
}

#[test]
fn audit_fks_cascades_deferred_and_virtual_cols() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // Pre-existing orphan in unrelated table before enabling FKs must NOT poison COMMIT!
    conn.execute(
        "CREATE TABLE pre_p(id INT PRIMARY KEY);
         CREATE TABLE pre_c(pid INT REFERENCES pre_p(id));
         INSERT INTO pre_c VALUES (999);
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();

    // 1. Outermost RELEASE SAVEPOINT enforces deferred FKs; ROLLBACK TO restores clean state.
    conn.execute(
        "CREATE TABLE dp(id INT PRIMARY KEY);
         CREATE TABLE dc(pid INT REFERENCES dp(id) DEFERRABLE INITIALLY DEFERRED);",
    )
    .unwrap();
    conn.execute("SAVEPOINT sp_outer; INSERT INTO dc VALUES (777);")
        .unwrap();
    assert!(conn
        .execute("RELEASE sp_outer")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    conn.execute("ROLLBACK TO sp_outer; RELEASE sp_outer;")
        .unwrap();

    // Verify resolving deferred violation in txn succeeds on COMMIT even with unrelated `pre_c` orphan!
    conn.execute(
        "BEGIN;
         INSERT INTO dc VALUES (1);
         INSERT INTO dp VALUES (1);
         COMMIT;",
    )
    .unwrap();

    // 2. ON DELETE CASCADE + child BEFORE DELETE RAISE(IGNORE) must fail FK (no silent orphan!).
    conn.execute(
        "CREATE TABLE cp(id INT PRIMARY KEY);
         CREATE TABLE cc(pid INT REFERENCES cp(id) ON DELETE CASCADE);
         CREATE TRIGGER tr_cc_ign BEFORE DELETE ON cc BEGIN SELECT RAISE(IGNORE); END;
         INSERT INTO cp VALUES (1); INSERT INTO cc VALUES (1);",
    )
    .unwrap();
    assert!(conn
        .execute("DELETE FROM cp WHERE id = 1")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    assert_rows_eq(&conn.query("SELECT id FROM cp").unwrap().rows, &[vec![i(1)]]);

    // 3. Self-referencing multi-row UPDATE CASCADE & DELETE CASCADE (incl. WITHOUT ROWID!).
    conn.execute(
        "CREATE TABLE self_u(id INTEGER PRIMARY KEY, pid INT REFERENCES self_u(id) ON UPDATE CASCADE ON DELETE CASCADE);
         CREATE INDEX idx_self_u_pid ON self_u(pid);
         INSERT INTO self_u VALUES (1, NULL), (2, 1);
         UPDATE self_u SET id = id + 10;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, pid FROM self_u ORDER BY id").unwrap().rows,
        &[vec![i(11), SqlValue::Null], vec![i(12), i(11)]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );
    let del_wr = {
        conn.execute(
            "CREATE TABLE wr_self(id INT PRIMARY KEY, pid INT REFERENCES wr_self(id) ON DELETE CASCADE) WITHOUT ROWID;
             INSERT INTO wr_self VALUES (1, NULL), (2, 1);",
        )
        .unwrap();
        conn.execute("DELETE FROM wr_self").unwrap()
    };
    assert_eq!(del_wr.changes, 1); // row 2 deleted by cascade during row 1's delete!

    // 4. Multiple FKs on the same child table during ON UPDATE CASCADE!
    conn.execute(
        "CREATE TABLE mp(id INT PRIMARY KEY);
         CREATE TABLE mc(
            a INT REFERENCES mp(id) ON UPDATE CASCADE,
            b INT REFERENCES mp(id) ON UPDATE CASCADE
         );
         INSERT INTO mp VALUES (1); INSERT INTO mc VALUES (1, 1);
         UPDATE mp SET id = 2 WHERE id = 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT a, b FROM mc").unwrap().rows,
        &[vec![i(2), i(2)]],
    );

    // 5. VIRTUAL generated columns in child FK & parent UNIQUE-indexed virtual column!
    conn.execute(
        "CREATE TABLE vp(x INT, k INT GENERATED ALWAYS AS (x * 2) VIRTUAL);
         CREATE UNIQUE INDEX idx_vp_k ON vp(k);
         INSERT INTO vp(x) VALUES (5);
         CREATE TABLE vc(v INT, g INT GENERATED ALWAYS AS (v + 1) VIRTUAL REFERENCES vp(k));",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO vc(v) VALUES (99)")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    conn.execute("INSERT INTO vc(v) VALUES (9)").unwrap(); // 9 + 1 == 10 == 5 * 2!
    assert!(conn.execute("PRAGMA foreign_key_check(vc)").unwrap().rows.is_empty());

    // 6. resolve_parent_key_cids rejects duplicate parent cols & collation mismatch!
    conn.execute(
        "CREATE TABLE p_dup(a INT, b INT, UNIQUE(a, b));
         CREATE TABLE c_dup(x INT, y INT, FOREIGN KEY(x, y) REFERENCES p_dup(a, a));",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO c_dup VALUES (1, 1)")
        .unwrap_err()
        .to_string()
        .contains("foreign key mismatch"));
    conn.execute(
        "CREATE TABLE p_coll(k TEXT);
         CREATE UNIQUE INDEX idx_p_coll ON p_coll(k COLLATE NOCASE);
         CREATE TABLE c_coll(k TEXT REFERENCES p_coll(k));",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO c_coll VALUES ('a')")
        .unwrap_err()
        .to_string()
        .contains("foreign key mismatch"));

    // 7. PRAGMA defer_foreign_keys = ON defers RESTRICT until COMMIT (and resets on ROLLBACK).
    conn.execute(
        "CREATE TABLE rp(id INT PRIMARY KEY);
         CREATE TABLE rc(pid INT REFERENCES rp(id) ON DELETE RESTRICT);
         INSERT INTO rp VALUES (1); INSERT INTO rc VALUES (1);",
    )
    .unwrap();
    conn.execute("BEGIN; PRAGMA defer_foreign_keys = ON; DELETE FROM rp WHERE id = 1;")
        .unwrap();
    assert!(conn
        .execute("COMMIT")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    conn.execute("ROLLBACK;").unwrap();
    assert_eq!(
        conn.execute("PRAGMA defer_foreign_keys").unwrap().rows[0][0].as_i64(),
        Some(0)
    );

    // 8. DROP TABLE parent: empty with orphan OK, populated CASCADE OK, populated RESTRICT deferred fails!
    conn.execute("DROP TABLE pre_p").unwrap();
    conn.execute(
        "CREATE TABLE dp2(id INT PRIMARY KEY);
         CREATE TABLE dc2(pid INT REFERENCES dp2(id) ON DELETE CASCADE);
         INSERT INTO dp2 VALUES (1); INSERT INTO dc2 VALUES (1);
         DROP TABLE dp2;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM dc2").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute(
        "CREATE TABLE dp3(id INT PRIMARY KEY);
         CREATE TABLE dc3(pid INT REFERENCES dp3(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO dp3 VALUES (1); INSERT INTO dc3 VALUES (1);
         BEGIN;",
    )
    .unwrap();
    assert!(conn
        .execute("DROP TABLE dp3")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    conn.execute("ROLLBACK;").unwrap();

    // 9. INSERT OR IGNORE self-referencing multi-row & ALTER TABLE ADD COLUMN REFERENCES DEFAULT non-null!
    conn.execute(
        "CREATE TABLE sr_ign(id INT PRIMARY KEY, pid INT REFERENCES sr_ign(id));
         INSERT OR IGNORE INTO sr_ign VALUES (2, 1), (1, NULL);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, pid FROM sr_ign ORDER BY id").unwrap().rows,
        &[vec![i(1), SqlValue::Null], vec![i(2), i(1)]],
    );
    conn.execute("CREATE TABLE add_empty(x INT); ALTER TABLE add_empty ADD COLUMN pid INT REFERENCES dp(id) DEFAULT 1;")
        .unwrap();
    conn.execute("CREATE TABLE add_nonempty(x INT); INSERT INTO add_nonempty VALUES (1);")
        .unwrap();
    assert!(conn
        .execute("ALTER TABLE add_nonempty ADD COLUMN pid INT REFERENCES dp(id) DEFAULT 1")
        .unwrap_err()
        .to_string()
        .contains("non-NULL default"));
}

#[test]
fn audit_fast_scans_ddl_maintenance_and_query_only() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. Parameterized fast-select range queries must NOT cache constant-empty results!
    conn.execute(
        "CREATE TABLE p_t(id INTEGER PRIMARY KEY, g INT, v INT);
         CREATE TABLE p_s(k INT);
         INSERT INTO p_t VALUES (1, 1, 10), (2, 2, 20), (3, 1, 30), (4, 3, 40);
         INSERT INTO p_s VALUES (1), (3);",
    )
    .unwrap();
    let r1 = conn
        .query_with_params(
            "SELECT id, v FROM p_t WHERE id BETWEEN ? AND ?",
            &QueryParams::positional(vec![i(2), i(3)]),
        )
        .unwrap();
    assert_rows_eq(&r1.rows, &[vec![i(2), i(20)], vec![i(3), i(30)]]);
    let r2 = conn
        .query_with_params(
            "SELECT id, sum(v) OVER (ORDER BY id) FROM p_t WHERE id <= ?",
            &QueryParams::positional(vec![i(3)]),
        )
        .unwrap();
    assert_rows_eq(
        &r2.rows,
        &[vec![i(1), i(10)], vec![i(2), i(30)], vec![i(3), i(60)]],
    );
    let r3 = conn
        .query_with_params(
            "SELECT count(*) FROM p_t WHERE id <= ? AND g IN (SELECT k FROM p_s)",
            &QueryParams::positional(vec![i(3)]),
        )
        .unwrap();
    assert_rows_eq(&r3.rows, &[vec![i(2)]]);

    // 2. ORDER BY alias shadowing in RowidRange & CumulativeWindowIpk!
    conn.execute("CREATE TABLE ao(id INTEGER PRIMARY KEY, a INT); INSERT INTO ao VALUES (1, 100), (2, 10);")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT a AS id FROM ao WHERE ao.id BETWEEN 1 AND 2 ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(10)], vec![i(100)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT a AS id, sum(a) OVER (ORDER BY ao.id) FROM ao ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(10), i(110)], vec![i(100), i(100)]],
    );

    // 3. Direct hash equijoin preserves short-record DEFAULT values after ALTER TABLE ADD COLUMN!
    conn.execute(
        "CREATE TABLE jr(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO jr VALUES (1, 'one');
         CREATE TABLE jl(id INTEGER PRIMARY KEY, rk INT); INSERT INTO jl VALUES (10, 1);
         ALTER TABLE jl ADD COLUMN tag TEXT DEFAULT 'fallback';",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT jl.id, jl.tag, jr.name FROM jl JOIN jr ON jl.rk = jr.id")
            .unwrap()
            .rows,
        &[vec![i(10), t("fallback"), t("one")]],
    );

    // 4. KBN float overflow to +Inf stays +Inf (NOT NaN -> NULL/0.0) in GROUP BY & window!
    conn.execute(
        "CREATE TABLE inf_t(id INTEGER PRIMARY KEY, g INT, v REAL);
         INSERT INTO inf_t VALUES (1, 1, 1e308), (2, 1, 1e308);",
    )
    .unwrap();
    let gb_inf = conn
        .query("SELECT g, sum(v), total(v), avg(v) FROM inf_t GROUP BY g")
        .unwrap();
    assert_eq!(gb_inf.rows.len(), 1);
    assert!(matches!(gb_inf.rows[0][1], SqlValue::Real(x) if x.is_infinite() && x.is_sign_positive()));
    assert!(matches!(gb_inf.rows[0][2], SqlValue::Real(x) if x.is_infinite() && x.is_sign_positive()));
    assert!(matches!(gb_inf.rows[0][3], SqlValue::Real(x) if x.is_infinite() && x.is_sign_positive()));
    let win_inf = conn
        .query("SELECT id, sum(v) OVER (ORDER BY id), total(v) OVER (ORDER BY id) FROM inf_t")
        .unwrap();
    assert!(matches!(win_inf.rows[1][1], SqlValue::Real(x) if x.is_infinite() && x.is_sign_positive()));
    assert!(matches!(win_inf.rows[1][2], SqlValue::Real(x) if x.is_infinite() && x.is_sign_positive()));

    // 5. INDEXED BY nonexistent in IN-subquery fast path errors cleanly!
    assert!(conn
        .query("SELECT count(*) FROM p_t WHERE g IN (SELECT k FROM p_s INDEXED BY nope)")
        .is_err());

    // 6. ALTER TABLE DROP CONSTRAINT / DROP NOT NULL strips `CONSTRAINT <name>` cleanly without hijacking,
    //    preserves ADD CHECK ON CONFLICT, and reindexes tables named `nocase`!
    conn.execute(
        "CREATE TABLE dc_t(
            a INT CONSTRAINT nn_a NOT NULL CHECK(a > 0),
            b INT,
            CONSTRAINT c1 CHECK(a < 100),
            CHECK(b > 0)
         );
         ALTER TABLE dc_t DROP CONSTRAINT c1;
         ALTER TABLE dc_t ALTER COLUMN a DROP NOT NULL;",
    )
    .unwrap();
    // Dropping c1 again MUST fail (must NOT have hijacked CHECK(b > 0)!).
    assert!(conn.execute("ALTER TABLE dc_t DROP CONSTRAINT c1").is_err());
    // `CHECK(b > 0)` must STILL be active, while `a` now allows NULL and >= 100!
    conn.execute("INSERT INTO dc_t VALUES (NULL, 5), (500, 5)").unwrap();
    assert!(conn.execute("INSERT INTO dc_t VALUES (1, -1)").is_err());
    // ADD CHECK ON CONFLICT IGNORE preserved in sqlite_schema.sql!
    conn.execute("ALTER TABLE dc_t ADD CHECK(b < 50) ON CONFLICT IGNORE;")
        .unwrap();
    let ddl_row = conn
        .query("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'dc_t'")
        .unwrap();
    assert!(ddl_row.rows[0][0]
        .as_str()
        .unwrap()
        .contains("ON CONFLICT IGNORE"));
    assert!(conn.execute("INSERT INTO dc_t VALUES (1, 999)").is_err());
    conn.execute("INSERT OR IGNORE INTO dc_t VALUES (1, 999)")
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM dc_t WHERE b = 999").unwrap().rows,
        &[vec![i(0)]],
    );
    // Table named `nocase` with 2 UNIQUE constraints: DROP CONSTRAINT rebuilds surviving autoindex!
    conn.execute(
        "CREATE TABLE nocase(a INT, b INT, CONSTRAINT u1 UNIQUE(a), CONSTRAINT u2 UNIQUE(b));
         INSERT INTO nocase VALUES (1, 10), (2, 20);
         ALTER TABLE nocase DROP CONSTRAINT u1;",
    )
    .unwrap();
    conn.execute("INSERT INTO nocase VALUES (1, 30)").unwrap(); // u1 dropped!
    assert!(conn.execute("INSERT INTO nocase VALUES (3, 10)").is_err()); // u2 intact!
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 7. REINDEX main.nocase resolves table, not collation, and REINDEX rebuilds WITHOUT ROWID PK!
    conn.execute(
        "CREATE TABLE wr_re(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
         INSERT INTO wr_re VALUES ('a', 1), ('b', 2);
         REINDEX main.nocase;
         REINDEX wr_re;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT k, v FROM wr_re ORDER BY k").unwrap().rows,
        &[vec![t("a"), i(1)], vec![t("b"), i(2)]],
    );

    // 8. ANALYZE partial-index-only 0-match table emits (tbl, NULL, nrows) and DROP TABLE cleans sqlite_stat1!
    conn.execute(
        "CREATE TABLE part_t(id INTEGER PRIMARY KEY, x INT);
         CREATE INDEX idx_part ON part_t(x) WHERE x > 1000;
         INSERT INTO part_t VALUES (1, 1), (2, 2);
         ANALYZE;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT idx, stat FROM sqlite_stat1 WHERE tbl = 'part_t'")
            .unwrap()
            .rows,
        &[vec![SqlValue::Null, t("2")]],
    );
    conn.execute("DROP TABLE part_t;").unwrap();
    assert!(conn
        .query("SELECT * FROM sqlite_stat1 WHERE tbl = 'part_t'")
        .unwrap()
        .rows
        .is_empty());

    // 8b. DROP TABLE with foreign_keys = ON: rejects NO ACTION children, cascades without parent triggers,
    //     and defers DEFERRABLE INITIALLY DEFERRED violations until COMMIT!
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE dt_p(id INTEGER PRIMARY KEY);
         CREATE TABLE dt_c(id INTEGER PRIMARY KEY, pid INT REFERENCES dt_p(id) ON DELETE CASCADE);
         CREATE TABLE dt_log(msg TEXT);
         CREATE TRIGGER dt_tp BEFORE DELETE ON dt_p BEGIN INSERT INTO dt_log VALUES ('tp'); END;
         CREATE TRIGGER dt_tc BEFORE DELETE ON dt_c BEGIN INSERT INTO dt_log VALUES ('tc'); END;
         INSERT INTO dt_p VALUES (1);
         INSERT INTO dt_c VALUES (10, 1);
         DROP TABLE dt_p;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT msg FROM dt_log").unwrap().rows,
        &[vec![t("tc")]],
    );
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM dt_c").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute(
        "CREATE TABLE dt_p2(id INTEGER PRIMARY KEY);
         CREATE TABLE dt_c2(id INTEGER PRIMARY KEY, pid INT REFERENCES dt_p2(id) DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO dt_p2 VALUES (1);
         INSERT INTO dt_c2 VALUES (10, 1);
         BEGIN;
         DROP TABLE dt_p2;",
    )
    .unwrap();
    let cerr = conn.execute("COMMIT").unwrap_err();
    assert!(cerr.to_string().contains("FOREIGN KEY constraint failed"));
    conn.execute("ROLLBACK").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id FROM dt_p2").unwrap().rows,
        &[vec![i(1)]],
    );
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 9. PRAGMA query_only = ON blocks header-writing PRAGMAs AND seed_json(),
    //    while matching C SQLite by still allowing `PRAGMA page_size = ...` / `PRAGMA encoding = ...`!
    conn.execute("PRAGMA query_only = ON;").unwrap();
    assert!(conn.execute("PRAGMA user_version = 42").is_err());
    assert!(conn.execute("PRAGMA application_id = 99").is_err());
    assert!(conn.execute("PRAGMA page_size = 8192").is_ok());
    assert!(conn.execute("PRAGMA encoding = 'UTF-16'").is_ok());
    assert!(conn.seed_json(r#"{"t_ro": [{"a": 1}]}"#).is_err());
    conn.execute("PRAGMA query_only = OFF;").unwrap();
}

#[test]
fn audit2_triggers_raise_views_and_dml_parity() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. P0 #1: UPDATE OR REPLACE + recursive_triggers + BEFORE DELETE RAISE(FAIL) must NOT lose rows!
    conn.execute(
        "PRAGMA recursive_triggers = ON;
         CREATE TABLE rp_fail(id INTEGER PRIMARY KEY, u INT UNIQUE);
         INSERT INTO rp_fail VALUES (1, 10), (2, 20);
         CREATE TRIGGER rp_bd BEFORE DELETE ON rp_fail BEGIN
            SELECT RAISE(FAIL, 'stop delete');
         END;",
    )
    .unwrap();
    let err = conn
        .execute("UPDATE OR REPLACE rp_fail SET u = 20 WHERE id = 1")
        .unwrap_err();
    assert!(err.to_string().contains("stop delete"));
    assert_rows_eq(
        &conn
            .query("SELECT id, u FROM rp_fail ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(10)], vec![i(2), i(20)]],
    );
    let err2 = conn
        .execute("INSERT OR REPLACE INTO rp_fail VALUES (3, 20)")
        .unwrap_err();
    assert!(err2.to_string().contains("stop delete"));
    assert_rows_eq(
        &conn
            .query("SELECT id, u FROM rp_fail ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(10)], vec![i(2), i(20)]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 2. P0 #2: UPDATE SET rowid = ... on IPK table syncs id, FKs, stored generated cols,
    //    secondary indexes, RETURNING, and UPSERT DO UPDATE SET rowid = ...!
    //    Per C SQLite: `SET rowid = 5` fires `UPDATE OF rowid` (NOT `UPDATE OF id`), while
    //    `SET id = ...` fires `UPDATE OF id`.
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE ipk_p(
            id INTEGER PRIMARY KEY,
            k INT UNIQUE,
            dbl INT GENERATED ALWAYS AS (id * 2) STORED
         );
         CREATE INDEX idx_ipk_p_id ON ipk_p(id);
         CREATE TABLE ipk_c(id INTEGER PRIMARY KEY, pid INT REFERENCES ipk_p(id) ON UPDATE CASCADE);
         CREATE TABLE ipk_log(msg TEXT);
         CREATE TRIGGER ipk_au_id AFTER UPDATE OF id ON ipk_p BEGIN
            INSERT INTO ipk_log VALUES ('id:' || OLD.id || '->' || NEW.id || ':' || NEW.dbl);
         END;
         CREATE TRIGGER ipk_au_rid AFTER UPDATE OF rowid ON ipk_p BEGIN
            INSERT INTO ipk_log VALUES ('rid:' || OLD.id || '->' || NEW.id || ':' || NEW.dbl);
         END;
         INSERT INTO ipk_p(id, k) VALUES (1, 100);
         INSERT INTO ipk_c VALUES (10, 1);",
    )
    .unwrap();
    let ret = conn
        .execute("UPDATE ipk_p SET rowid = 5 WHERE id = 1 RETURNING id, dbl")
        .unwrap();
    assert_rows_eq(&ret.rows, &[vec![i(5), i(10)]]);
    assert_rows_eq(
        &conn.query("SELECT id, dbl FROM ipk_p").unwrap().rows,
        &[vec![i(5), i(10)]],
    );
    assert_rows_eq(
        &conn.query("SELECT pid FROM ipk_c WHERE id = 10").unwrap().rows,
        &[vec![i(5)]],
    );
    assert_rows_eq(
        &conn.query("SELECT msg FROM ipk_log").unwrap().rows,
        &[vec![t("rid:1->5:10")]],
    );
    // UPSERT DO UPDATE SET rowid = ... on IPK table:
    let up_ret = conn
        .execute(
            "INSERT INTO ipk_p(id, k) VALUES (99, 100)
             ON CONFLICT(k) DO UPDATE SET rowid = 7 RETURNING id, dbl",
        )
        .unwrap();
    assert_rows_eq(&up_ret.rows, &[vec![i(7), i(14)]]);
    assert_rows_eq(
        &conn.query("SELECT pid FROM ipk_c WHERE id = 10").unwrap().rows,
        &[vec![i(7)]],
    );
    assert_rows_eq(
        &conn.query("SELECT msg FROM ipk_log ORDER BY rowid").unwrap().rows,
        &[vec![t("rid:1->5:10")], vec![t("rid:5->7:14")]],
    );
    conn.execute("UPDATE ipk_p SET id = 8 WHERE id = 7").unwrap();
    assert_rows_eq(
        &conn.query("SELECT msg FROM ipk_log ORDER BY rowid").unwrap().rows,
        &[
            vec![t("rid:1->5:10")],
            vec![t("rid:5->7:14")],
            vec![t("id:7->8:16")],
        ],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 3. P0 #3: Multi-row UPDATE OR REPLACE + recursive_triggers + AFTER DELETE trigger mutating
    //    subsequent rows keeps secondary indexes and rows consistent!
    conn.execute(
        "CREATE TABLE mr_rep(id INTEGER PRIMARY KEY, u INT UNIQUE, v INT);
         CREATE INDEX idx_mr_rep_v ON mr_rep(v);
         INSERT INTO mr_rep VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300);
         CREATE TRIGGER mr_ad AFTER DELETE ON mr_rep BEGIN
            UPDATE mr_rep SET v = v + 1 WHERE id = 3;
         END;
         UPDATE OR REPLACE mr_rep SET u = u + 10 WHERE id IN (1, 3);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT id, u, v FROM mr_rep ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(20), i(100)], vec![i(3), i(40), i(301)]],
    );

    // 4. Outer ON CONFLICT overrides inner trigger ON CONFLICT (both directions)!
    conn.execute(
        "CREATE TABLE oc_t(x INT);
         CREATE TABLE oc_u(id INT PRIMARY KEY);
         INSERT INTO oc_u VALUES (1);
         CREATE TRIGGER oc_tr_abort AFTER INSERT ON oc_t BEGIN
            INSERT OR ABORT INTO oc_u VALUES (1);
         END;",
    )
    .unwrap();
    conn.execute("INSERT OR IGNORE INTO oc_t VALUES (10)").unwrap();
    assert_rows_eq(
        &conn.query("SELECT x FROM oc_t").unwrap().rows,
        &[vec![i(10)]],
    );
    conn.execute(
        "DROP TRIGGER oc_tr_abort;
         CREATE TRIGGER oc_tr_ign AFTER INSERT ON oc_t BEGIN
            INSERT OR IGNORE INTO oc_u VALUES (1);
         END;",
    )
    .unwrap();
    // Default outer (no explicit OR clause) allows inner OR IGNORE:
    conn.execute("INSERT INTO oc_t VALUES (20)").unwrap();
    // Explicit outer OR ABORT overrides inner OR IGNORE -> fails!
    assert!(conn.execute("INSERT OR ABORT INTO oc_t VALUES (30)").is_err());
    assert_rows_eq(
        &conn.query("SELECT x FROM oc_t ORDER BY x").unwrap().rows,
        &[vec![i(10)], vec![i(20)]],
    );

    // 5. RAISE(IGNORE) inside BEFORE UPDATE skips only that row, whereas RAISE(IGNORE) directly
    //    inside UPSERT DO UPDATE SET aborts the remaining trigger steps & rows!
    conn.execute(
        "CREATE TABLE up_ign_src(x INT);
         CREATE TABLE up_ign_dst(k INT PRIMARY KEY, v INT);
         INSERT INTO up_ign_dst VALUES (1, 10);
         CREATE TRIGGER up_ign_bu BEFORE UPDATE ON up_ign_dst BEGIN
            SELECT RAISE(IGNORE);
         END;
         CREATE TRIGGER up_ign_ai AFTER INSERT ON up_ign_src BEGIN
            INSERT INTO up_ign_dst VALUES (1, 20), (2, 30)
            ON CONFLICT(k) DO UPDATE SET v = excluded.v;
         END;
         INSERT INTO up_ign_src VALUES (1);",
    )
    .unwrap();
    // BEFORE UPDATE RAISE(IGNORE) skips row 1's update, so row 2 IS still inserted:
    assert_rows_eq(
        &conn
            .query("SELECT k, v FROM up_ign_dst ORDER BY k")
            .unwrap()
            .rows,
        &[vec![i(1), i(10)], vec![i(2), i(30)]],
    );
    // Direct RAISE(IGNORE) in DO UPDATE SET aborts the multi-row INSERT AND subsequent trigger steps:
    conn.execute(
        "CREATE TABLE up_ign_src2(x INT);
         CREATE TABLE up_ign_dst2(k INT PRIMARY KEY, v INT);
         INSERT INTO up_ign_dst2 VALUES (1, 10);
         CREATE TRIGGER up_ign_ai2 AFTER INSERT ON up_ign_src2 BEGIN
            INSERT INTO up_ign_dst2 VALUES (1, 20), (2, 30)
            ON CONFLICT(k) DO UPDATE SET v = CASE WHEN excluded.k = 1 THEN RAISE(IGNORE) ELSE excluded.v END;
            INSERT INTO up_ign_dst2 VALUES (3, 40);
         END;
         INSERT INTO up_ign_src2 VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT k, v FROM up_ign_dst2 ORDER BY k")
            .unwrap()
            .rows,
        &[vec![i(1), i(10)]],
    );

    // 6. UPDATE ... FROM inside trigger routes RAISE(IGNORE/FAIL/ROLLBACK) properly!
    conn.execute(
        "CREATE TABLE uf_src(x INT);
         CREATE TABLE uf_dst(id INTEGER PRIMARY KEY, v INT);
         INSERT INTO uf_dst VALUES (1, 10);
         CREATE TRIGGER uf_ai AFTER INSERT ON uf_src BEGIN
            UPDATE uf_dst SET v = s.nv
            FROM (SELECT CASE WHEN NEW.x = 1 THEN RAISE(IGNORE) ELSE 99 END AS nv) AS s
            WHERE uf_dst.id = 1;
         END;
         INSERT INTO uf_src VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT v FROM uf_dst WHERE id = 1").unwrap().rows,
        &[vec![i(10)]],
    );
    conn.execute("INSERT INTO uf_src VALUES (2)").unwrap();
    assert_rows_eq(
        &conn.query("SELECT v FROM uf_dst WHERE id = 1").unwrap().rows,
        &[vec![i(99)]],
    );

    // 7. Immediate FK checked at end of outermost statement across trigger steps!
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE tfk_p(id INTEGER PRIMARY KEY);
         CREATE TABLE tfk_c(id INTEGER PRIMARY KEY, pid INT REFERENCES tfk_p(id));
         CREATE TRIGGER tfk_bi BEFORE INSERT ON tfk_c BEGIN
            INSERT OR IGNORE INTO tfk_p VALUES (NEW.pid);
         END;
         INSERT INTO tfk_c VALUES (1, 42);
         CREATE TRIGGER tfk_ad AFTER DELETE ON tfk_p BEGIN
            DELETE FROM tfk_c WHERE pid = OLD.id;
         END;
         DELETE FROM tfk_p WHERE id = 42;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM tfk_c").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 8. STRICT validation runs BEFORE BEFORE-triggers!
    conn.execute(
        "CREATE TABLE st_t(id INTEGER PRIMARY KEY, x INT) STRICT;
         CREATE TABLE st_log(msg TEXT);
         CREATE TRIGGER st_bi BEFORE INSERT ON st_t BEGIN
            INSERT INTO st_log VALUES ('bi');
         END;
         CREATE TRIGGER st_bu BEFORE UPDATE ON st_t BEGIN
            INSERT INTO st_log VALUES ('bu');
         END;
         INSERT INTO st_t VALUES (1, 10);",
    )
    .unwrap();
    assert!(conn.execute("INSERT INTO st_t VALUES (2, 'bad')").is_err());
    assert!(conn.execute("UPDATE st_t SET x = 'bad' WHERE id = 1").is_err());
    assert_rows_eq(
        &conn.query("SELECT msg FROM st_log").unwrap().rows,
        &[vec![t("bi")]],
    );

    // 9. RETURNING evaluates BEFORE AFTER triggers (INSERT, UPSERT, UPDATE, DELETE)
    //    AND OLD.* in AFTER UPDATE/DELETE & DELETE RETURNING retain pre-BEFORE values!
    conn.execute(
        "CREATE TABLE ro_t(id INTEGER PRIMARY KEY, v INT);
         CREATE TABLE ro_log(msg TEXT);
         CREATE TRIGGER ro_ai AFTER INSERT ON ro_t BEGIN
            UPDATE ro_t SET v = v + 100 WHERE id = NEW.id;
         END;
         CREATE TRIGGER ro_bu BEFORE UPDATE ON ro_t WHEN OLD.v < 50 BEGIN
            UPDATE ro_t SET v = 55 WHERE id = OLD.id;
         END;
         CREATE TRIGGER ro_au AFTER UPDATE ON ro_t BEGIN
            INSERT INTO ro_log VALUES ('au:' || OLD.v || '->' || NEW.v);
            UPDATE ro_t SET v = v + 1000 WHERE id = NEW.id AND v < 500;
         END;
         CREATE TRIGGER ro_bd BEFORE DELETE ON ro_t BEGIN
            UPDATE ro_t SET v = 777 WHERE id = OLD.id;
         END;
         CREATE TRIGGER ro_ad AFTER DELETE ON ro_t BEGIN
            INSERT INTO ro_log VALUES ('ad:' || OLD.v);
         END;",
    )
    .unwrap();
    // Disable recursive_triggers so AFTER triggers don't re-enter themselves infinitely.
    conn.execute("PRAGMA recursive_triggers = OFF;").unwrap();
    let r_ins = conn
        .execute("INSERT INTO ro_t VALUES (1, 10) RETURNING id, v")
        .unwrap();
    assert_rows_eq(&r_ins.rows, &[vec![i(1), i(10)]]);
    // ro_ai added +100 -> fired ro_au which added +1000 -> 1110, while RETURNING saw 10!
    assert_rows_eq(
        &conn.query("SELECT v FROM ro_t WHERE id = 1").unwrap().rows,
        &[vec![i(1110)]],
    );
    // Reset row to v = 20 without triggers, then test BEFORE UPDATE + AFTER UPDATE + RETURNING:
    conn.execute("DROP TRIGGER ro_ai; DROP TRIGGER ro_au; UPDATE ro_t SET v = 20 WHERE id = 1;")
        .unwrap();
    // Note: ro_bu fired and set v=55, then outer SET v=20 wrote 20! Let's recreate clean table for OLD.* test:
    conn.execute(
        "CREATE TABLE old_t(id INTEGER PRIMARY KEY, v INT);
         CREATE TABLE old_log(msg TEXT);
         INSERT INTO old_t VALUES (1, 10);
         CREATE TRIGGER old_bu BEFORE UPDATE ON old_t BEGIN
            UPDATE old_t SET v = 55 WHERE id = OLD.id;
         END;
         CREATE TRIGGER old_au AFTER UPDATE ON old_t BEGIN
            INSERT INTO old_log VALUES ('au:' || OLD.v || '->' || NEW.v);
            UPDATE old_t SET v = 999 WHERE id = NEW.id;
         END;
         CREATE TRIGGER old_bd BEFORE DELETE ON old_t BEGIN
            UPDATE old_t SET v = 888 WHERE id = OLD.id;
         END;
         CREATE TRIGGER old_ad AFTER DELETE ON old_t BEGIN
            INSERT INTO old_log VALUES ('ad:' || OLD.v);
         END;",
    )
    .unwrap();
    let r_upd = conn
        .execute("UPDATE old_t SET v = 20 WHERE id = 1 RETURNING v")
        .unwrap();
    assert_rows_eq(&r_upd.rows, &[vec![i(20)]]);
    assert_rows_eq(
        &conn.query("SELECT v FROM old_t WHERE id = 1").unwrap().rows,
        &[vec![i(999)]],
    );
    let r_del = conn
        .execute("DELETE FROM old_t WHERE id = 1 RETURNING v")
        .unwrap();
    assert_rows_eq(&r_del.rows, &[vec![i(999)]]);
    assert_rows_eq(
        &conn.query("SELECT msg FROM old_log ORDER BY rowid").unwrap().rows,
        &[
            vec![t("au:10->55")],
            vec![t("au:10->20")],
            vec![t("au:999->55")],
            vec![t("au:999->888")],
            vec![t("ad:999")],
        ],
    );

    // 10. INSTEAD OF UPDATE OF uncovered column rejected, UPSERT on view rejected,
    //     and view trigger errors reset changes() to 0!
    conn.execute(
        "CREATE TABLE vt_base(a INT, b INT);
         INSERT INTO vt_base VALUES (1, 2);
         CREATE VIEW vt_v AS SELECT a, b FROM vt_base;
         CREATE TRIGGER vt_io_upd INSTEAD OF UPDATE OF a ON vt_v BEGIN
            UPDATE vt_base SET a = NEW.a;
         END;
         CREATE TRIGGER vt_io_ins INSTEAD OF INSERT ON vt_v BEGIN
            SELECT RAISE(ABORT, 'view fail');
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("UPDATE vt_v SET b = 99")
        .unwrap_err()
        .to_string()
        .contains("cannot modify"));
    conn.execute("UPDATE vt_v SET a = 10").unwrap();
    assert!(conn
        .execute("INSERT INTO vt_v VALUES (1, 2) ON CONFLICT DO NOTHING")
        .unwrap_err()
        .to_string()
        .contains("cannot UPSERT a view"));
    conn.execute("UPDATE vt_base SET a = 11").unwrap();
    assert_eq!(conn.changes(), 1);
    assert!(conn.execute("INSERT INTO vt_v VALUES (1, 2)").is_err());
    assert_eq!(conn.changes(), 0);

    // 11. Multi-row UPDATE re-evaluates SET against live row when earlier row's trigger mutated it!
    conn.execute(
        "CREATE TABLE live_u(id INTEGER PRIMARY KEY, x INT);
         INSERT INTO live_u VALUES (1, 10), (2, 20);
         CREATE TRIGGER live_u_au AFTER UPDATE ON live_u WHEN NEW.id = 1 BEGIN
            UPDATE live_u SET x = 100 WHERE id = 2;
         END;
         UPDATE live_u SET x = x + 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, x FROM live_u ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(11)], vec![i(2), i(101)]],
    );

    // 12. Static RAISE() outside triggers rejected even on 0-row DML!
    conn.execute("CREATE TABLE empty_r(x INT);").unwrap();
    assert!(conn
        .execute("INSERT INTO empty_r SELECT RAISE(IGNORE) WHERE 0")
        .unwrap_err()
        .to_string()
        .contains("RAISE()"));
    assert!(conn
        .execute("UPDATE empty_r SET x = RAISE(IGNORE) WHERE 0")
        .unwrap_err()
        .to_string()
        .contains("RAISE()"));
    assert!(conn
        .execute("DELETE FROM empty_r WHERE RAISE(IGNORE)")
        .unwrap_err()
        .to_string()
        .contains("RAISE()"));
    assert!(conn
        .execute("UPDATE vt_v SET a = RAISE(IGNORE) WHERE 0")
        .unwrap_err()
        .to_string()
        .contains("RAISE()"));

    // 13. changes() / total_changes() on RAISE(FAIL) from FK cascade:
    //     - Immediate FK violated -> rolled back with FOREIGN KEY constraint failed & changes() = 0!
    //     - DEFERRABLE INITIALLY DEFERRED inside BEGIN -> keeps partial changes (changes() = 2) and returns `cascade fail`!
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE ch_p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch_c(id INTEGER PRIMARY KEY, pid INT REFERENCES ch_p(id) ON DELETE CASCADE);
         INSERT INTO ch_p VALUES (1), (2);
         INSERT INTO ch_c VALUES (10, 2);
         CREATE TRIGGER ch_c_bd BEFORE DELETE ON ch_c BEGIN
            SELECT RAISE(FAIL, 'cascade fail');
         END;",
    )
    .unwrap();
    let err_imm_fail = conn.execute("DELETE FROM ch_p").unwrap_err();
    assert!(err_imm_fail.to_string().contains("FOREIGN KEY"));
    assert_eq!(conn.changes(), 0);
    assert_rows_eq(
        &conn.query("SELECT id FROM ch_p ORDER BY id").unwrap().rows,
        &[vec![i(1)], vec![i(2)]],
    );
    conn.execute(
        "CREATE TABLE ch_p2(id INTEGER PRIMARY KEY);
         CREATE TABLE ch_c2(id INTEGER PRIMARY KEY, pid INT REFERENCES ch_p2(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO ch_p2 VALUES (1), (2);
         INSERT INTO ch_c2 VALUES (10, 2);
         CREATE TRIGGER ch_c2_bd BEFORE DELETE ON ch_c2 BEGIN
            SELECT RAISE(FAIL, 'cascade fail');
         END;
         BEGIN;",
    )
    .unwrap();
    let tc_before = conn.total_changes();
    let err_def_fail = conn.execute("DELETE FROM ch_p2").unwrap_err();
    assert!(err_def_fail.to_string().contains("cascade fail"));
    assert_eq!(conn.changes(), 2);
    assert_eq!(conn.total_changes(), tc_before + 2);
    conn.execute("ROLLBACK;").unwrap();
}

#[test]
fn audit2_foreign_key_two_counter_model_and_edge_cases() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute("PRAGMA foreign_keys = ON;").unwrap();

    // 1. INSERT OR FAIL and UPDATE OR FAIL with immediate FK violation roll back the WHOLE statement!
    conn.execute(
        "CREATE TABLE f_p(id INTEGER PRIMARY KEY);
         CREATE TABLE f_c(id INTEGER PRIMARY KEY, pid INT REFERENCES f_p(id));
         INSERT INTO f_p VALUES (1), (2);",
    )
    .unwrap();
    let err_ins = conn
        .execute("INSERT OR FAIL INTO f_c VALUES (10, 1), (11, 999), (12, 2)")
        .unwrap_err();
    assert!(err_ins.to_string().contains("FOREIGN KEY"));
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM f_c").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute("INSERT INTO f_c VALUES (10, 1), (11, 2)").unwrap();
    let err_upd = conn
        .execute("UPDATE OR FAIL f_c SET pid = CASE WHEN id = 10 THEN 2 ELSE 999 END")
        .unwrap_err();
    assert!(err_upd.to_string().contains("FOREIGN KEY"));
    assert_rows_eq(
        &conn
            .query("SELECT id, pid FROM f_c ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(10), i(1)], vec![i(11), i(2)]],
    );

    // 2. ALTER TABLE child RENAME TO child2 inside tx preserves pending deferred FK failure;
    //    DROP TABLE child inside tx clears pending deferred FK violations!
    conn.execute(
        "CREATE TABLE def_p(id INTEGER PRIMARY KEY);
         CREATE TABLE def_c(id INTEGER PRIMARY KEY, pid INT REFERENCES def_p(id) DEFERRABLE INITIALLY DEFERRED);
         BEGIN;
         INSERT INTO def_c VALUES (1, 777);
         ALTER TABLE def_c RENAME TO def_c2;",
    )
    .unwrap();
    assert!(conn
        .execute("COMMIT")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    // Dropping def_c2 inside the transaction resolves its pending deferred violation!
    conn.execute("DROP TABLE def_c2; COMMIT;").unwrap();
    assert!(!conn.in_transaction());

    // 3. Pre-existing FK orphans:
    //    - UPDATE non-FK col succeeds; UPDATE FK col fails;
    //    - ON UPDATE CASCADE on unrelated valid parent succeeds;
    //    - Inserting missing parent for pre-existing orphan does NOT mask a new violation!
    conn.execute(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE orp_p(id INTEGER PRIMARY KEY);
         CREATE TABLE orp_c(id INTEGER PRIMARY KEY, pid INT REFERENCES orp_p(id) ON UPDATE CASCADE, x INT);
         INSERT INTO orp_p VALUES (1);
         INSERT INTO orp_c VALUES (1, 1, 10), (2, 999, 10);
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
    conn.execute("UPDATE orp_c SET x = 20 WHERE id = 2").unwrap();
    assert!(conn
        .execute("UPDATE orp_c SET pid = 888 WHERE id = 2")
        .is_err());
    conn.execute("UPDATE orp_p SET id = 2 WHERE id = 1").unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, pid, x FROM orp_c ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(2), i(10)], vec![i(2), i(999), i(20)]],
    );
    // Clamped counter >= 0: inserting 999 before a fresh orphan in deferred tx still fails COMMIT!
    conn.execute(
        "CREATE TABLE clamp_p(id INTEGER PRIMARY KEY);
         CREATE TABLE clamp_c(id INTEGER PRIMARY KEY, pid INT REFERENCES clamp_p(id) DEFERRABLE INITIALLY DEFERRED);
         PRAGMA foreign_keys = OFF;
         INSERT INTO clamp_c VALUES (1, 500);
         PRAGMA foreign_keys = ON;
         BEGIN;
         INSERT INTO clamp_p VALUES (500);
         INSERT INTO clamp_c VALUES (2, 600);",
    )
    .unwrap();
    assert!(conn
        .execute("COMMIT")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    conn.execute("ROLLBACK;").unwrap();

    // 4. INSERT OR REPLACE on parent fires ON DELETE (CASCADE deletes children,
    //    NO ACTION succeeds when re-inserted, RESTRICT fails)!
    conn.execute(
        "CREATE TABLE rp_p(id INTEGER PRIMARY KEY, v INT);
         CREATE TABLE rp_casc(id INTEGER PRIMARY KEY, pid INT REFERENCES rp_p(id) ON DELETE CASCADE);
         CREATE TABLE rp_noact(id INTEGER PRIMARY KEY, pid INT REFERENCES rp_p(id) ON DELETE NO ACTION);
         INSERT INTO rp_p VALUES (1, 10);
         INSERT INTO rp_casc VALUES (10, 1);
         INSERT INTO rp_noact VALUES (20, 1);
         INSERT OR REPLACE INTO rp_p VALUES (1, 99);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM rp_casc").unwrap().rows,
        &[vec![i(0)]],
    );
    assert_rows_eq(
        &conn.query("SELECT pid FROM rp_noact").unwrap().rows,
        &[vec![i(1)]],
    );
    conn.execute(
        "CREATE TABLE rp_rest(id INTEGER PRIMARY KEY, pid INT REFERENCES rp_p(id) ON DELETE RESTRICT);
         INSERT INTO rp_rest VALUES (30, 1);",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT OR REPLACE INTO rp_p VALUES (1, 100)")
        .is_err());

    // 5. NOCASE parent key 'abc' -> 'ABC' is unchanged -> NO ON UPDATE CASCADE / RESTRICT!
    conn.execute(
        "CREATE TABLE nc_p(k TEXT PRIMARY KEY COLLATE NOCASE);
         CREATE TABLE nc_c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES nc_p(k) ON UPDATE RESTRICT);
         INSERT INTO nc_p VALUES ('abc');
         INSERT INTO nc_c VALUES (1, 'abc');
         UPDATE nc_p SET k = 'ABC' WHERE k = 'abc';",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT k FROM nc_p").unwrap().rows,
        &[vec![t("ABC")]],
    );

    // 6. WITHOUT ROWID PK collation override & shadowed rowid/_rowid_/oid + nullable PK!
    conn.execute(
        "CREATE TABLE wr_p(id INTEGER PRIMARY KEY);
         CREATE TABLE wr_c(
            k TEXT,
            pid INT REFERENCES wr_p(id) ON DELETE CASCADE,
            PRIMARY KEY(k COLLATE NOCASE)
         ) WITHOUT ROWID;
         INSERT INTO wr_p VALUES (1);
         INSERT INTO wr_c VALUES ('Hello', 1);
         DELETE FROM wr_p WHERE id = 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM wr_c").unwrap().rows,
        &[vec![i(0)]],
    );
    conn.execute(
        "CREATE TABLE sh_p(id INTEGER PRIMARY KEY);
         CREATE TABLE sh_c(
            rowid INT PRIMARY KEY,
            _rowid_ INT,
            oid INT,
            pid INT REFERENCES sh_p(id) ON DELETE CASCADE
         );
         INSERT INTO sh_p VALUES (1), (2);
         INSERT INTO sh_c VALUES (NULL, 1, 1, 1), (NULL, 2, 2, 2);
         DELETE FROM sh_p WHERE id = 1;",
    )
    .unwrap();
    // Row 2 with NULL PK must survive!
    assert_rows_eq(
        &conn.query("SELECT _rowid_, pid FROM sh_c").unwrap().rows,
        &[vec![i(2), i(2)]],
    );

    // 7. Outer ON CONFLICT IGNORE does NOT leak into FK cascades!
    conn.execute(
        "CREATE TABLE leak_p(id INTEGER PRIMARY KEY);
         CREATE TABLE leak_c(id INTEGER PRIMARY KEY, pid INT NOT NULL REFERENCES leak_p(id) ON DELETE SET NULL);
         INSERT INTO leak_p VALUES (1);
         INSERT INTO leak_c VALUES (10, 1);",
    )
    .unwrap();
    assert!(conn
        .execute("UPDATE OR IGNORE leak_p SET id = 2 WHERE id = 1")
        .is_err());

    // 8. PRAGMA defer_foreign_keys = ON resets after autocommit statement!
    conn.execute("PRAGMA defer_foreign_keys = ON;").unwrap();
    assert_eq!(
        conn.execute("PRAGMA defer_foreign_keys").unwrap().rows[0][0].as_i64(),
        Some(1)
    );
    conn.execute("INSERT INTO sh_p VALUES (99)").unwrap();
    assert_eq!(
        conn.execute("PRAGMA defer_foreign_keys").unwrap().rows[0][0].as_i64(),
        Some(0)
    );

    // 9. PRAGMA foreign_key_check(view) returns empty, while DML referencing view returns foreign key mismatch!
    conn.execute(
        "CREATE VIEW fk_v AS SELECT id FROM sh_p;
         CREATE TABLE fk_ref_v(id INT, vid INT REFERENCES fk_v(id));",
    )
    .unwrap();
    assert!(conn
        .execute("PRAGMA foreign_key_check(fk_v)")
        .unwrap()
        .rows
        .is_empty());
    let v_err = conn
        .execute("INSERT INTO fk_ref_v VALUES (1, 2)")
        .unwrap_err();
    assert!(v_err.to_string().contains("foreign key mismatch"));

    // 10. PRAGMA foreign_key_check with custom collation, custom function on VIRTUAL FK col, and metering!
    conn.execute(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE cfk_p(k TEXT PRIMARY KEY);
         CREATE TABLE cfk_c(id INTEGER PRIMARY KEY, raw TEXT);
         INSERT INTO cfk_p VALUES ('abc');
         INSERT INTO cfk_c(id, raw) VALUES (1, 'ABC'), (2, 'xyz');
         ALTER TABLE cfk_c ADD COLUMN vkey TEXT GENERATED ALWAYS AS (
             CASE WHEN custom_norm(raw) COLLATE REV_CASE = 'abc' THEN 'abc' ELSE raw END
         ) VIRTUAL REFERENCES cfk_p(k);",
    )
    .unwrap();
    let img = conn.to_bytes();
    let db = xdb_sqlite::open_database_from_bytes(&img, None, None, OpenOptions::default()).unwrap();
    let mut qopts = xdb_sqlite::QueryOptions::default();
    qopts.collations.register(
        "REV_CASE",
        Arc::new(|a: &[u8], b: &[u8]| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())),
    );
    qopts.functions.register_scalar(
        "custom_norm",
        Some(1),
        Arc::new(|args: &[SqlValue]| match &args[0] {
            SqlValue::Text(b) => Ok(SqlValue::text(
                String::from_utf8_lossy(b.as_ref()).to_ascii_lowercase(),
            )),
            other => Ok(other.clone()),
        }),
    );
    let res = db
        .query_with_options(
            "SELECT * FROM pragma_foreign_key_check('cfk_c')",
            &QueryParams::empty(),
            &qopts,
        )
        .unwrap();
    // Row 1 matches 'abc' via custom_norm + REV_CASE; ONLY row 2 is reported!
    assert_rows_eq(&res.rows, &[vec![t("cfk_c"), i(2), t("cfk_p"), i(0)]]);

    // Step metering halts foreign_key_check on tight budget:
    let tight_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(1),
        ..ResourceLimits::default()
    }));
    qopts.meter = Some(tight_meter);
    assert!(db
        .query_with_options(
            "SELECT * FROM pragma_foreign_key_check('cfk_c')",
            &QueryParams::empty(),
            &qopts,
        )
        .is_err());
}

#[test]
fn audit2_ddl_maintenance_pragmas_fast_scans_and_eqp() {
    let vfs = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    let mut conn =
        SqliteConnection::open(vfs.clone(), "audit2.db", ConnectionOptions::default()).unwrap();

    // 1. ALTER TABLE DROP CONSTRAINT:
    //    - Column-level DEFERRABLE FK strips trailing DEFERRABLE without hijacking preceding FK!
    //    - SQL comments around commas do not break DROP CONSTRAINT!
    //    - Dropping UNIQUE constraint removes stale sqlite_stat1 autoindex entry!
    conn.execute(
        "CREATE TABLE p1(id INTEGER PRIMARY KEY);
         CREATE TABLE p2(id INTEGER PRIMARY KEY);
         CREATE TABLE dc_fk(
            a INT CONSTRAINT fk1 REFERENCES p1(id),
            b INT CONSTRAINT fk2 REFERENCES p2(id) DEFERRABLE INITIALLY DEFERRED /* trailing */,
            c INT CONSTRAINT u_c UNIQUE -- comment before comma
            , d INT
         );
         INSERT INTO p1 VALUES (1);
         INSERT INTO p2 VALUES (2);
         INSERT INTO dc_fk VALUES (1, 2, 10, 20);
         ANALYZE;
         ALTER TABLE dc_fk DROP CONSTRAINT fk2;
         ALTER TABLE dc_fk DROP CONSTRAINT u_c;",
    )
    .unwrap();
    let ddl = conn
        .query("SELECT sql FROM sqlite_schema WHERE name = 'dc_fk'")
        .unwrap()
        .rows[0][0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!ddl.to_ascii_uppercase().contains("DEFERRABLE"));
    assert!(ddl.contains("fk1"));
    // Verify stale autoindex row for dropped UNIQUE(c) was removed from sqlite_stat1!
    let stat_rows = conn
        .query("SELECT idx FROM sqlite_stat1 WHERE tbl = 'dc_fk'")
        .unwrap()
        .rows;
    assert!(stat_rows.iter().all(|r| r[0].is_null()));
    // Verify fk1 still enforced and fk2/u_c dropped:
    conn.execute("PRAGMA foreign_keys = ON;").unwrap();
    conn.execute("INSERT INTO dc_fk VALUES (1, 999, 10, 21)")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO dc_fk VALUES (999, 999, 11, 22)")
        .is_err());
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 2. ALTER TABLE ADD COLUMN:
    //    - REFERENCES ... DEFAULT non-null succeeds on non-empty table when fk=OFF, fails when fk=ON!
    //    - Constant DEFAULT (literal) succeeds on non-empty table; non-constant DEFAULT (1 + 2) rejected!
    conn.execute("CREATE TABLE ac_t(x INT); INSERT INTO ac_t VALUES (1);")
        .unwrap();
    assert!(conn
        .execute("ALTER TABLE ac_t ADD COLUMN bad INT DEFAULT (1 + 2)")
        .unwrap_err()
        .to_string()
        .contains("non-constant default"));
    conn.execute("ALTER TABLE ac_t ADD COLUMN y INT DEFAULT (3);")
        .unwrap();
    conn.execute("ALTER TABLE ac_t ADD COLUMN z INT DEFAULT (-5);")
        .unwrap();
    conn.execute("ALTER TABLE ac_t ADD COLUMN pid1 INT REFERENCES p1(id) DEFAULT 1;")
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT x, y, z, pid1 FROM ac_t").unwrap().rows,
        &[vec![i(1), i(3), i(-5), i(1)]],
    );
    conn.execute("PRAGMA foreign_keys = ON;").unwrap();
    assert!(conn
        .execute("ALTER TABLE ac_t ADD COLUMN pid2 INT REFERENCES p1(id) DEFAULT 1")
        .is_err());
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 3. ALTER TABLE DROP COLUMN & RENAME COLUMN & RENAME TABLE edge cases:
    //    - Inline REFERENCES column drop succeeds; table-level FOREIGN KEY fails!
    //    - Case-only RENAME COLUMN a TO A succeeds!
    //    - RENAME COLUMN rejects columns in view JOIN ... USING!
    //    - DROP COLUMN rejects view LIMIT / Compound ORDER BY / JOIN USING & trigger UPDATE ... FROM!
    //    - CTE shadowing does NOT cause false rejection!
    conn.execute("ALTER TABLE ac_t DROP COLUMN pid1;").unwrap();
    conn.execute(
        "CREATE TABLE rc_t(a INT, b INT, c INT);
         CREATE TABLE rc_other(a INT, z INT);
         ALTER TABLE rc_t RENAME COLUMN a TO A;
         CREATE VIEW v_using AS SELECT A, b FROM rc_t JOIN rc_other USING (A);
         CREATE VIEW v_lim AS SELECT b FROM rc_t LIMIT (SELECT c FROM rc_t LIMIT 1);
         CREATE VIEW v_comp AS SELECT b FROM rc_t UNION SELECT z FROM rc_other ORDER BY 1;
         CREATE VIEW v_cte_shadow AS WITH rc_t(b) AS (SELECT 1) SELECT b FROM rc_t;",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE rc_t RENAME COLUMN A TO a2")
        .is_err());
    assert!(conn.execute("ALTER TABLE rc_t DROP COLUMN A").is_err());
    assert!(conn.execute("ALTER TABLE rc_t DROP COLUMN c").is_err());
    conn.execute("DROP VIEW v_using; DROP VIEW v_lim; DROP VIEW v_comp;")
        .unwrap();
    // Only v_cte_shadow remains, where `rc_t` is shadowed by CTE `WITH rc_t(b)` -> DROP COLUMN b succeeds!
    conn.execute("ALTER TABLE rc_t DROP COLUMN b;").unwrap();
    conn.execute("DROP VIEW v_cte_shadow;").unwrap();

    // Trigger RENAME TABLE & RENAME COLUMN: recursive CTE, correlated subquery, alias shadowing, WINDOW, UPDATE FROM!
    conn.execute(
        "CREATE TABLE tr_t(id INT, val INT);
         CREATE TABLE tr_log(x INT);
         CREATE TRIGGER tr_complex AFTER INSERT ON tr_t BEGIN
            WITH RECURSIVE tr_t(id, val) AS (
                SELECT 1, 2 UNION ALL SELECT id + 1, val FROM tr_t WHERE id < 2
            )
            SELECT id FROM tr_t;
            SELECT sum(val) OVER w FROM tr_t WINDOW w AS (PARTITION BY val ORDER BY id);
            UPDATE tr_log SET x = tr_t.val FROM tr_t WHERE tr_t.id = NEW.id;
         END;
         ALTER TABLE tr_t RENAME COLUMN val TO amount;
         ALTER TABLE tr_t RENAME TO tr_t2;
         INSERT INTO tr_log VALUES (0);
         INSERT INTO tr_t2 VALUES (1, 42);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT x FROM tr_log").unwrap().rows,
        &[vec![i(42)]],
    );
    let tr_sql = conn
        .query("SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = 'tr_complex'")
        .unwrap()
        .rows[0][0]
        .as_str()
        .unwrap()
        .to_string();
    // Recursive CTE `WITH RECURSIVE tr_t(id, val)` must NOT be renamed to `tr_t2` / `amount`!
    assert!(tr_sql.contains("WITH RECURSIVE tr_t(id, val)"));
    assert!(tr_sql.contains("PARTITION BY amount"));
    assert!(tr_sql.contains("FROM tr_t2"));

    // 4. ALTER TABLE ALTER COLUMN SET NOT NULL ON CONFLICT IGNORE replaces existing NOT NULL!
    conn.execute(
        "CREATE TABLE nn_t(id INT PRIMARY KEY, x INT NOT NULL);
         ALTER TABLE nn_t ALTER COLUMN x SET NOT NULL ON CONFLICT IGNORE;
         INSERT INTO nn_t VALUES (1, NULL);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT count(*) FROM nn_t").unwrap().rows,
        &[vec![i(0)]],
    );
    let nn_ddl = conn
        .query("SELECT sql FROM sqlite_schema WHERE name = 'nn_t'")
        .unwrap()
        .rows[0][0]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(nn_ddl.matches("NOT NULL").count(), 1);

    // 5. ANALYZE main & single-index ANALYZE idx (preserves sibling index stats)!
    conn.execute(
        "CREATE TABLE an_t(id INTEGER PRIMARY KEY, a INT, b INT);
         CREATE INDEX an_ia ON an_t(a);
         CREATE INDEX an_ib ON an_t(b);
         INSERT INTO an_t VALUES (1, 10, 100), (2, 10, 200), (3, 20, 300);
         ANALYZE main;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT idx, stat FROM sqlite_stat1 WHERE tbl = 'an_t' ORDER BY idx")
            .unwrap()
            .rows,
        &[vec![t("an_ia"), t("3 2")], vec![t("an_ib"), t("3 1")]],
    );
    conn.execute("INSERT INTO an_t VALUES (4, 20, 400); ANALYZE an_ia;")
        .unwrap();
    // an_ia updated to "4 2", while an_ib MUST remain intact ("3 1")!
    assert_rows_eq(
        &conn
            .query("SELECT idx, stat FROM sqlite_stat1 WHERE tbl = 'an_t' ORDER BY idx")
            .unwrap()
            .rows,
        &[vec![t("an_ia"), t("4 2")], vec![t("an_ib"), t("3 1")]],
    );

    // 6. VACUUM & VACUUM INTO preserve reserved_bytes (header byte 20)!
    let mut raw_img = {
        let mut c0 = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        // Initialize an empty page 1 (0 cells) before setting reserved_bytes so the image is valid!
        c0.execute("PRAGMA user_version = 1;").unwrap();
        c0.to_bytes()
    };
    raw_img[20] = 16;
    let mut c_res = SqliteConnection::open_from_bytes(&raw_img, None, None, ConnectionOptions::default())
        .unwrap();
    c_res
        .execute("CREATE TABLE vr(id INTEGER PRIMARY KEY, s TEXT); INSERT INTO vr VALUES (1, 'hi');")
        .unwrap();
    c_res.execute("VACUUM").unwrap();
    let vac_img = c_res.to_bytes();
    assert_eq!(vac_img[20], 16);
    assert_rows_eq(
        &c_res.query("SELECT id, s FROM vr").unwrap().rows,
        &[vec![i(1), t("hi")]],
    );

    // 7. PRAGMAs: table_info(view), table_xinfo(view), integrity_check(table),
    //    pragma_table_info('t', 'bogus'), table_list NATURAL JOIN ncol!
    conn.execute(
        "CREATE TABLE nj_a(id INT, x TEXT);
         CREATE TABLE nj_b(id INT, y REAL);
         CREATE VIEW nj_v AS SELECT * FROM nj_a NATURAL JOIN nj_b;",
    )
    .unwrap();
    let v_info = conn.execute("PRAGMA table_info(nj_v)").unwrap();
    assert_eq!(v_info.rows.len(), 3);
    assert_eq!(v_info.rows[0][1].as_str(), Some("id"));
    assert_eq!(v_info.rows[1][1].as_str(), Some("x"));
    assert_eq!(v_info.rows[2][1].as_str(), Some("y"));
    let v_xinfo = conn.execute("PRAGMA table_xinfo(nj_v)").unwrap();
    assert_eq!(v_xinfo.rows.len(), 3);
    let tlist = conn
        .query("SELECT ncol FROM pragma_table_list('nj_v')")
        .unwrap();
    assert_rows_eq(&tlist.rows, &[vec![i(3)]]);
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check(an_t)").unwrap().rows,
        &[vec![t("ok")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA quick_check(an_t)").unwrap().rows,
        &[vec![t("ok")]],
    );
    assert!(conn
        .execute("PRAGMA integrity_check(no_such_tbl_xyz)")
        .unwrap_err()
        .to_string()
        .contains("no such table"));
    assert!(conn
        .query("SELECT * FROM pragma_table_info('an_t', 'bogus')")
        .unwrap_err()
        .to_string()
        .contains("unknown database"));

    // 8. DirectSumState::finalize_total: +Inf + -Inf -> NULL (NOT 0.0) in GROUP BY & window!
    conn.execute(
        "CREATE TABLE nan_t(id INTEGER PRIMARY KEY, g INT, v REAL);
         INSERT INTO nan_t VALUES (1, 1, 9e999), (2, 1, -9e999);",
    )
    .unwrap();
    let nan_gb = conn
        .query("SELECT g, total(v), sum(v) FROM nan_t GROUP BY g")
        .unwrap();
    assert_rows_eq(&nan_gb.rows, &[vec![i(1), SqlValue::Null, SqlValue::Null]]);
    let nan_win = conn
        .query("SELECT id, total(v) OVER (ORDER BY id) FROM nan_t ORDER BY id")
        .unwrap();
    assert!(nan_win.rows[1][1].is_null());

    // 9. EXPLAIN QUERY PLAN fixes:
    //    - Bare 0-arg pragma_table_list
    //    - WITHOUT ROWID PRIMARY KEY search
    //    - Self-join alias scoping
    //    - COVERING INDEX not falsely claimed when OVER (PARTITION BY unindexed) used
    //    - ORDER BY id on IPK table without TEMP B-TREE
    //    - JOIN combining ON + WHERE & USING/NATURAL JOIN
    //    - INDEXED BY partial index rejected with `cannot use index:`
    conn.execute(
        "CREATE TABLE eq_wr(k INT PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE TABLE eq_t(id INTEGER PRIMARY KEY, a INT, b INT);
         CREATE INDEX eq_ia ON eq_t(a);
         CREATE INDEX eq_part ON eq_t(b) WHERE b > 0;",
    )
    .unwrap();
    let eqp_tvf = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM pragma_table_list")
        .unwrap();
    assert!(!eqp_tvf.rows.is_empty());
    let eqp_wr = conn
        .execute("EXPLAIN QUERY PLAN SELECT v FROM eq_wr WHERE k = 1")
        .unwrap();
    assert!(eqp_wr
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USING PRIMARY KEY")));
    let eqp_self = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM eq_t AS t1 JOIN eq_t AS t2 ON t1.a = t2.a WHERE t2.id = 1")
        .unwrap();
    // t1 has NO filter on t1.id (only t2.id = 1), so t1 must NOT say SEARCH eq_t AS t1 USING INTEGER PRIMARY KEY!
    assert!(!eqp_self
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("SEARCH eq_t AS t1 USING INTEGER PRIMARY KEY")));
    assert!(eqp_self
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("SEARCH eq_t AS t2 USING INTEGER PRIMARY KEY")));
    let eqp_cov = conn
        .execute("EXPLAIN QUERY PLAN SELECT a, count(*) OVER (PARTITION BY b) FROM eq_t WHERE a = 1")
        .unwrap();
    assert!(!eqp_cov
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("COVERING INDEX")));
    let eqp_ord = conn
        .execute("EXPLAIN QUERY PLAN SELECT id, a FROM eq_t ORDER BY id")
        .unwrap();
    assert!(!eqp_ord
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("TEMP B-TREE FOR ORDER BY")));
    let eqp_part = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM eq_t INDEXED BY eq_part WHERE b = 5")
        .unwrap_err();
    assert!(eqp_part.to_string().contains("cannot use index"));
}

#[test]
fn audit3_storage_pager_fast_scans_integrity_and_seed() {
    // 1. WritePager::set_page_size + VACUUM with reserved_bytes: never panics when
    //    page_size - reserved_bytes < 480; preserves reserved_bytes when valid.
    {
        let mut base = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        base.execute("PRAGMA user_version = 1;").unwrap();
        let mut img = base.to_bytes();
        // Set header reserved_bytes (offset 20) to 64 on a clean empty page 1 (valid on 4096-byte
        // page, invalid on 512-byte page since 512-64=448 < 480).
        img[20] = 64;
        // Re-open with reserved_bytes = 64, populate a table, then attempt PRAGMA page_size = 512 + VACUUM
        // (must ignore 512 and NOT panic!).
        let mut conn =
            SqliteConnection::open_from_bytes(&img, None, None, ConnectionOptions::default())
                .unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'ok');")
            .unwrap();
        conn.execute("PRAGMA page_size = 512; VACUUM;").unwrap();
        assert_eq!(conn.page_size(), 4096);
        assert_eq!(conn.to_bytes()[20], 64);
        assert_rows_eq(
            &conn.query("SELECT id, v FROM t").unwrap().rows,
            &[vec![i(1), t("ok")]],
        );
        // Valid page_size = 1024 (1024 - 64 = 960 >= 480) succeeds and preserves reserved_bytes = 64.
        conn.execute("PRAGMA page_size = 1024; VACUUM;").unwrap();
        assert_eq!(conn.page_size(), 1024);
        assert_eq!(conn.to_bytes()[20], 64);
        assert_rows_eq(
            &conn.execute("PRAGMA integrity_check").unwrap().rows,
            &[vec![t("ok")]],
        );
    }

    // 2. Crafted overflow payload OOM DoS in fast_scans rejected cleanly before allocating.
    {
        let mut base = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        base.execute("CREATE TABLE oom(id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        base.execute(&format!(
            "INSERT INTO oom VALUES (1, '{}')",
            "x".repeat(5000)
        ))
        .unwrap();
        let mut img = base.to_bytes();
        // Locate page 2 (table root leaf) and corrupt first cell's payload varint to 0x7FFF_FFFF.
        let p2 = 4096usize;
        assert_eq!(img[p2], 0x0D);
        let cell_off = u16::from_be_bytes([img[p2 + 8], img[p2 + 9]]) as usize;
        // Overwrite the 2-byte payload-len varint (5005 = 0xA7 0x0D) with a 4-byte huge varint?
        // Even simpler: replace first two varint bytes with continuation bits so decoded payload_len >> db size!
        // Encode 0x0F_FF_FF_FF as 4-byte SQLite varint (0xFF 0xFF 0xFF 0x7F) followed by rowid = 1
        // so BOTH full scan and point-lookup (`WHERE id = 1`) hit the crafted cell!
        img[p2 + cell_off] = 0xFF;
        img[p2 + cell_off + 1] = 0xFF;
        img[p2 + cell_off + 2] = 0xFF;
        img[p2 + cell_off + 3] = 0x7F;
        img[p2 + cell_off + 4] = 0x01;
        let opts = ConnectionOptions {
            meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
                max_memory_bytes: Some(2 * 1024 * 1024),
                max_steps: Some(100_000),
                ..ResourceLimits::default()
            }))),
            ..ConnectionOptions::default()
        };
        let conn = SqliteConnection::open_from_bytes(&img, None, None, opts).unwrap();
        // Both full-scan and point-lookup fast paths must return Err without OOM or panic.
        assert!(conn.query("SELECT body FROM oom").is_err());
        assert!(conn.query("SELECT body FROM oom WHERE id = 1").is_err());
    }

    // 3. Failed ROLLBACK TO <nonexistent> preserves deferred sqlite_sequence updates!
    {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute("CREATE TABLE sq(id INTEGER PRIMARY KEY AUTOINCREMENT, v INT);")
            .unwrap();
        conn.execute(
            "BEGIN;
             INSERT INTO sq(v) VALUES (10), (20), (30);",
        )
        .unwrap();
        assert!(conn.execute("ROLLBACK TO no_such_sp").is_err());
        conn.execute("COMMIT;").unwrap();
        assert_rows_eq(
            &conn
                .query("SELECT seq FROM sqlite_sequence WHERE name = 'sq'")
                .unwrap()
                .rows,
            &[vec![i(3)]],
        );
        conn.execute("DELETE FROM sq; INSERT INTO sq(v) VALUES (40);")
            .unwrap();
        assert_rows_eq(
            &conn.query("SELECT id FROM sq").unwrap().rows,
            &[vec![i(4)]],
        );
    }

    // 4. PRAGMA integrity_check & quick_check: CHECK, NOT NULL, STRICT, index consistency,
    //    scoped page-1 child pointer, freeblock gap, and overlapping regions!
    {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute(
            "CREATE TABLE chk(id INTEGER PRIMARY KEY, a INT CHECK(a > 0));
             PRAGMA ignore_check_constraints = ON;
             INSERT INTO chk VALUES (1, -5);
             PRAGMA ignore_check_constraints = OFF;",
        )
        .unwrap();
        let qc = conn.execute("PRAGMA quick_check").unwrap();
        assert!(qc
            .rows
            .iter()
            .any(|r| r[0].as_str().unwrap_or("").contains("CHECK constraint failed")));
        let ic = conn.execute("PRAGMA integrity_check").unwrap();
        assert!(ic
            .rows
            .iter()
            .any(|r| r[0].as_str().unwrap_or("").contains("CHECK constraint failed")));

        // Corrupt index entry mismatch: integrity_check detects it, quick_check skips indexes.
        let mut idx_conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        idx_conn
            .execute(
                "CREATE TABLE it(id INTEGER PRIMARY KEY, a INT);
                 CREATE INDEX idx_it_a ON it(a);
                 INSERT INTO it VALUES (1, 10), (2, 20);",
            )
            .unwrap();
        let mut img = idx_conn.to_bytes();
        // Page 3 is idx_it_a root (page 1 = schema, page 2 = it, page 3 = idx_it_a).
        // Zero its cell count (bytes 3..5 of page 3) to simulate missing index entries.
        let p3 = 2 * 4096usize;
        assert_eq!(img[p3], 0x0A);
        img[p3 + 3] = 0;
        img[p3 + 4] = 0;
        let mut corrupted =
            SqliteConnection::open_from_bytes(&img, None, None, ConnectionOptions::default())
                .unwrap();
        let qc2 = corrupted.execute("PRAGMA quick_check").unwrap();
        assert_rows_eq(&qc2.rows, &[vec![t("ok")]]);
        let ic2 = corrupted.execute("PRAGMA integrity_check").unwrap();
        assert!(ic2.rows.iter().any(|r| {
            let s = r[0].as_str().unwrap_or("");
            s.contains("wrong # of entries in index") || s.contains("row")
        }));

        // Scoped integrity_check catches corrupt interior child pointing to page 1.
        let mut big = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        big.execute("CREATE TABLE pt(id INTEGER PRIMARY KEY, pad TEXT);")
            .unwrap();
        big.execute("BEGIN;").unwrap();
        for k in 1..=80 {
            big.execute(&format!(
                "INSERT INTO pt VALUES ({k}, '{}')",
                "a".repeat(200)
            ))
            .unwrap();
        }
        big.execute("COMMIT;").unwrap();
        let mut bimg = big.to_bytes();
        // Page 2 is pt's interior table root (page type 0x05). Point right_child (bytes 8..12) to page 1.
        let p2 = 4096usize;
        assert_eq!(bimg[p2], 0x05);
        bimg[p2 + 8..p2 + 12].copy_from_slice(&1u32.to_be_bytes());
        let mut cbig =
            SqliteConnection::open_from_bytes(&bimg, None, None, ConnectionOptions::default())
                .unwrap();
        let ic_scoped = cbig.execute("PRAGMA integrity_check(pt)").unwrap();
        assert_ne!(ic_scoped.rows[0][0].as_str().unwrap_or(""), "ok");
    }

    // 5. seed_json strict {"__blob": [...]} validation & BudgetMeter memory enforcement.
    {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.seed_json(
            r#"{
                "items": [
                    {"id": 1, "b": {"__blob": [0, 127, 255]}},
                    {"id": 2, "b": {"__blob": [256, 1]}},
                    {"id": 3, "b": {"__blob": [-1, 2]}},
                    {"id": 4, "b": {"__blob": [1.5, 2]}}
                ]
            }"#,
        )
        .unwrap();
        let rows = conn
            .query("SELECT id, typeof(b), b FROM items ORDER BY id")
            .unwrap()
            .rows;
        assert!(rows[0][1].exact_eq(&t("blob")));
        assert!(rows[0][2].exact_eq(&SqlValue::Blob(vec![0, 127, 255].into_boxed_slice())));
        // Out-of-range / negative / float entries must NOT be silently truncated to bytes!
        assert!(rows[1][1].exact_eq(&t("text")));
        assert!(rows[2][1].exact_eq(&t("text")));
        assert!(rows[3][1].exact_eq(&t("text")));

        // BudgetMeter enforces memory limit on seed_json payloads.
        let meter = Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(128 * 1024),
            ..ResourceLimits::default()
        }));
        let mut mconn = SqliteConnection::open_in_memory(ConnectionOptions {
            meter: Some(meter),
            ..ConnectionOptions::default()
        })
        .unwrap();
        let huge_rows: Vec<String> = (0..500)
            .map(|k| format!(r#"{{"id":{k},"payload":"{}"}}"#, "z".repeat(512)))
            .collect();
        let huge_json = format!(r#"{{"big":[{}]}}"#, huge_rows.join(","));
        assert!(mconn.seed_json(&huge_json).is_err());
    }

    // 6. CollationRegistry::with_encoding preserves custom collations & PRAGMA encoding switches cleanly!
    {
        let mut reg = xdb_core::value::CollationRegistry::new(xdb_core::TextEncoding::Utf8);
        reg.register("REV", Arc::new(|a: &[u8], b: &[u8]| b.cmp(a)));
        let reg16 = reg.with_encoding(xdb_core::TextEncoding::Utf16Le);
        let rev = reg16.find("REV").expect("custom collation preserved");
        assert_eq!(
            rev.compare_utf8(b"apple", b"cherry"),
            std::cmp::Ordering::Greater
        );

        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute("PRAGMA encoding = 'UTF-16le';").unwrap();
        assert_eq!(conn.encoding(), xdb_core::TextEncoding::Utf16Le);
        conn.execute(
            "CREATE TABLE ct(a TEXT);
             INSERT INTO ct VALUES ('apple'), ('cherry'), ('banana');",
        )
        .unwrap();
        assert_rows_eq(
            &conn.query("SELECT a FROM ct ORDER BY a").unwrap().rows,
            &[vec![t("apple")], vec![t("banana")], vec![t("cherry")]],
        );
    }
}

#[test]
fn audit3_fk_three_counter_ddl_analyze_and_view_pragmas() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute("PRAGMA foreign_keys = ON;").unwrap();

    // 1. Self-referencing FK UPDATE parity: NO ACTION / RESTRICT fail; CASCADE / SET NULL / deferred work!
    conn.execute(
        "CREATE TABLE sr_na(id INT PRIMARY KEY, pid INT REFERENCES sr_na(id));
         INSERT INTO sr_na VALUES (1, 1);
         CREATE TABLE sr_rs(id INT PRIMARY KEY, pid INT REFERENCES sr_rs(id) ON UPDATE RESTRICT);
         INSERT INTO sr_rs VALUES (1, 1);
         CREATE TABLE sr_ca(id INT PRIMARY KEY, pid INT REFERENCES sr_ca(id) ON UPDATE CASCADE);
         INSERT INTO sr_ca VALUES (1, 1);
         CREATE TABLE sr_sn(id INT PRIMARY KEY, pid INT REFERENCES sr_sn(id) ON UPDATE SET NULL);
         INSERT INTO sr_sn VALUES (1, 1);
         CREATE TABLE sr_df(id INT PRIMARY KEY, pid INT REFERENCES sr_df(id) DEFERRABLE INITIALLY DEFERRED);",
    )
    .unwrap();
    assert!(conn.execute("UPDATE sr_na SET id = 2 WHERE id = 1").is_err());
    assert!(conn.execute("UPDATE sr_rs SET id = 2 WHERE id = 1").is_err());
    conn.execute("UPDATE sr_ca SET id = 2 WHERE id = 1").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, pid FROM sr_ca").unwrap().rows,
        &[vec![i(2), i(2)]],
    );
    conn.execute("UPDATE sr_sn SET id = 2 WHERE id = 1").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, pid FROM sr_sn").unwrap().rows,
        &[vec![i(2), SqlValue::Null]],
    );
    conn.execute(
        "BEGIN;
         INSERT INTO sr_df VALUES (1, 999);
         UPDATE sr_df SET id = 999 WHERE id = 1;
         COMMIT;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, pid FROM sr_df").unwrap().rows,
        &[vec![i(999), i(999)]],
    );

    // 2. CachedFastDml::SimpleInsert & PointDeleteRowid respect incoming/outgoing FKs!
    conn.execute(
        "CREATE TABLE fd_p(id INTEGER PRIMARY KEY);
         CREATE TABLE fd_c(id INTEGER PRIMARY KEY, pid INT REFERENCES fd_p(id) DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO fd_p VALUES (1);
         INSERT INTO fd_c VALUES (10, 1);",
    )
    .unwrap();
    // Repeated parameterized fast-DML statements should NEVER bypass FK checks.
    conn.execute("BEGIN;").unwrap();
    conn.execute_with_params(
        "INSERT INTO fd_c(id, pid) VALUES (?, ?)",
        &QueryParams::positional(vec![i(11), i(999)]),
    )
    .unwrap();
    assert!(conn.execute("COMMIT;").is_err());
    conn.execute("ROLLBACK;").unwrap();

    conn.execute("BEGIN;").unwrap();
    conn.execute_with_params(
        "DELETE FROM fd_p WHERE id = ?",
        &QueryParams::positional(vec![i(1)]),
    )
    .unwrap();
    assert!(conn.execute("COMMIT;").is_err());
    conn.execute("ROLLBACK;").unwrap();

    // 3. Strict 3-counter FK segregation & mid-tx defer_foreign_keys ON -> OFF!
    conn.execute(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE s3_p(id INT PRIMARY KEY);
         CREATE TABLE s3_c_imm(id INT PRIMARY KEY, pid INT REFERENCES s3_p(id));
         CREATE TABLE s3_c_def(id INT PRIMARY KEY, pid INT REFERENCES s3_p(id) DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO s3_c_imm VALUES (1, 777);
         INSERT INTO s3_c_def VALUES (1, 888);
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
    // Resolving pre-existing immediate orphan must NOT mask a deferred FK violation!
    conn.execute(
        "BEGIN;
         INSERT INTO s3_c_def VALUES (2, 555);
         DELETE FROM s3_c_imm WHERE id = 1;",
    )
    .unwrap();
    assert!(conn.execute("COMMIT;").is_err());
    conn.execute("ROLLBACK;").unwrap();

    // Resolving pre-existing deferred orphan must NOT mask an immediate FK violation!
    conn.execute("BEGIN;").unwrap();
    conn.execute("DELETE FROM s3_c_def WHERE id = 1;").unwrap();
    assert!(conn
        .execute("INSERT INTO s3_c_imm VALUES (2, 444);")
        .is_err());
    conn.execute("ROLLBACK;").unwrap();

    // Toggling defer_foreign_keys ON -> insert immediate orphan -> OFF -> insert parent -> COMMIT succeeds!
    conn.execute(
        "BEGIN;
         PRAGMA defer_foreign_keys = ON;
         INSERT INTO s3_c_imm VALUES (3, 333);
         PRAGMA defer_foreign_keys = OFF;
         INSERT INTO s3_p VALUES (333);
         COMMIT;",
    )
    .unwrap();

    // 4. DROP TABLE parent; DROP TABLE child; COMMIT inside deferred FK tx!
    conn.execute(
        "CREATE TABLE dp(id INT PRIMARY KEY);
         CREATE TABLE dc(pid INT REFERENCES dp(id) DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO dp VALUES (1);
         INSERT INTO dc VALUES (1);
         BEGIN;
         DROP TABLE dp;
         DROP TABLE dc;
         COMMIT;",
    )
    .unwrap();

    // 5. INSERT NULL child rejects missing/mismatched parent; UPDATE unrelated parent col succeeds!
    conn.execute(
        "CREATE TABLE mm_p(a INT, other TEXT);
         INSERT INTO mm_p VALUES (1, 'x');
         CREATE TABLE mm_c(pid INT REFERENCES mm_p(a));
         CREATE TABLE miss_c(pid INT REFERENCES no_such_parent(id));",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO mm_c VALUES (NULL)")
        .unwrap_err()
        .to_string()
        .contains("foreign key mismatch"));
    assert!(conn
        .execute("INSERT INTO miss_c VALUES (NULL)")
        .unwrap_err()
        .to_string()
        .contains("no such table"));
    // Updating unrelated parent column `other` must succeed despite mismatched FK on `mm_p(a)`!
    conn.execute("UPDATE mm_p SET other = 'y' WHERE a = 1")
        .unwrap();
    conn.execute("PRAGMA foreign_keys = OFF;").unwrap();

    // 6. ALTER TABLE RENAME TO rewrites table's own CHECK(t.a > 0);
    //    RENAME COLUMN rejects broken view-over-view & trigger JOIN USING, rewrites SELECT * EXCLUDE/REPLACE;
    //    ADD COLUMN DEFAULT CURRENT_TIMESTAMP / (1+2) allowed on empty table, rejected on non-empty.
    conn.execute(
        "CREATE TABLE rc_t(a INT CHECK(rc_t.a > 0));
         ALTER TABLE rc_t RENAME TO rc_t2;
         INSERT INTO rc_t2 VALUES (5);",
    )
    .unwrap();
    assert!(conn.execute("INSERT INTO rc_t2 VALUES (-1)").is_err());

    conn.execute(
        "CREATE TABLE vv_t(a INT, b INT);
         CREATE VIEW vv_1 AS SELECT a, b FROM vv_t;
         CREATE VIEW vv_2 AS SELECT a FROM vv_1;",
    )
    .unwrap();
    let vv_err = conn
        .execute("ALTER TABLE vv_t RENAME COLUMN a TO a2")
        .unwrap_err();
    assert!(vv_err.to_string().contains("error in view vv_2"));
    conn.execute("DROP VIEW vv_2; DROP VIEW vv_1;").unwrap();

    conn.execute(
        "CREATE TABLE uj_a(k INT, x INT);
         CREATE TABLE uj_b(k INT, y INT);
         CREATE TRIGGER tr_uj AFTER INSERT ON uj_a BEGIN
            SELECT * FROM uj_a JOIN uj_b USING (k);
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE uj_a RENAME COLUMN k TO k2")
        .unwrap_err()
        .to_string()
        .contains("cannot join using column"));
    conn.execute("DROP TRIGGER tr_uj;").unwrap();

    {
        let mut xconn = SqliteConnection::open_in_memory(ConnectionOptions {
            dialect: xdb_parser::Dialect::Xdb,
            ..ConnectionOptions::default()
        })
        .unwrap();
        xconn
            .execute(
                "CREATE TABLE er_t(a INT, b INT, c INT);
                 INSERT INTO er_t VALUES (1, 2, 3);
                 CREATE VIEW er_v AS SELECT * EXCLUDE(b) REPLACE(a + 10 AS a) FROM er_t;",
            )
            .unwrap();
        assert!(xconn.execute("ALTER TABLE er_t DROP COLUMN b").is_err());
        xconn
            .execute("ALTER TABLE er_t RENAME COLUMN a TO a_new;")
            .unwrap();
        assert_rows_eq(
            &xconn.query("SELECT * FROM er_v").unwrap().rows,
            &[vec![i(11), i(3)]],
        );
    }

    // ADD COLUMN non-constant default: allowed on empty table, rejected on non-empty table.
    conn.execute(
        "CREATE TABLE empty_add(id INT);
         ALTER TABLE empty_add ADD COLUMN ts TEXT DEFAULT CURRENT_TIMESTAMP;
         ALTER TABLE empty_add ADD COLUMN expr_c INT DEFAULT (1 + 2);
         INSERT INTO empty_add(id) VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, expr_c FROM empty_add").unwrap().rows,
        &[vec![i(1), i(3)]],
    );
    assert!(conn
        .execute("ALTER TABLE empty_add ADD COLUMN ts2 TEXT DEFAULT CURRENT_TIMESTAMP")
        .is_err());
    assert!(conn
        .execute("ALTER TABLE empty_add ADD COLUMN expr2 INT DEFAULT (1 + 2)")
        .is_err());

    // 7. ANALYZE partial-index-only emits BOTH (tbl, NULL, nrows) and partial index stat;
    //    DROP INDEX deletes stale sqlite_stat1 row; ANALYZE respects BudgetMeter step limit!
    conn.execute(
        "CREATE TABLE an_p(id INTEGER PRIMARY KEY, v INT);
         CREATE INDEX idx_an_part ON an_p(v) WHERE v > 0;
         INSERT INTO an_p VALUES (1, -1), (2, -2), (3, -3), (4, 10), (5, 20);
         ANALYZE an_p;",
    )
    .unwrap();
    let st = conn
        .query("SELECT tbl, idx, stat FROM sqlite_stat1 WHERE tbl = 'an_p' ORDER BY idx")
        .unwrap()
        .rows;
    assert_rows_eq(
        &st,
        &[
            vec![t("an_p"), SqlValue::Null, t("5")],
            vec![t("an_p"), t("idx_an_part"), t("2 1")],
        ],
    );
    conn.execute("DROP INDEX idx_an_part;").unwrap();
    let st_after = conn
        .query("SELECT tbl, idx FROM sqlite_stat1 WHERE tbl = 'an_p'")
        .unwrap()
        .rows;
    assert_rows_eq(&st_after, &[vec![t("an_p"), SqlValue::Null]]);

    let mut mconn = SqliteConnection::open_in_memory(ConnectionOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(200),
            ..ResourceLimits::default()
        }))),
        ..ConnectionOptions::default()
    })
    .unwrap();
    mconn
        .execute("CREATE TABLE big_an(id INTEGER PRIMARY KEY, v INT);")
        .unwrap();
    // Insert with unlimited connection, then reopen with tiny step budget to test ANALYZE metering.
    let mut uconn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    uconn
        .execute("CREATE TABLE big_an(id INTEGER PRIMARY KEY, v INT); BEGIN;")
        .unwrap();
    for k in 1..=300 {
        uconn
            .execute(&format!("INSERT INTO big_an VALUES ({k}, {k})"))
            .unwrap();
    }
    uconn.execute("COMMIT;").unwrap();
    let mut tight = SqliteConnection::open_from_bytes(
        &uconn.to_bytes(),
        None,
        None,
        ConnectionOptions {
            meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
                max_steps: Some(50),
                ..ResourceLimits::default()
            }))),
            ..ConnectionOptions::default()
        },
    )
    .unwrap();
    assert!(tight.execute("ANALYZE big_an").is_err());

    // 8. PRAGMA table_info(view), table_xinfo(view), table_list parity!
    conn.execute(
        "CREATE TABLE vp_t(MyCol INTEGER, B TEXT);
         CREATE VIEW vp_dup AS SELECT MyCol, MyCol, CAST(B AS INTEGER) AS c_int FROM vp_t;
         CREATE TABLE vp_dead(x INT);
         CREATE VIEW vp_broken(c1, c2) AS SELECT x, x FROM vp_dead;
         DROP TABLE vp_dead;",
    )
    .unwrap();
    let ti = conn
        .query("SELECT name, type FROM pragma_table_info('vp_dup') ORDER BY cid")
        .unwrap()
        .rows;
    assert_rows_eq(
        &ti,
        &[
            vec![t("MyCol"), t("INTEGER")],
            vec![t("MyCol:1"), t("INTEGER")],
            vec![t("c_int"), t("INTEGER")],
        ],
    );
    // Broken view errors in table_info/table_xinfo and reports ncol = 0 in table_list!
    assert!(conn
        .query("SELECT * FROM pragma_table_info('vp_broken')")
        .is_err());
    assert!(conn
        .query("SELECT * FROM pragma_table_xinfo('vp_broken')")
        .is_err());
    let tl = conn
        .query("SELECT ncol FROM pragma_table_list WHERE name = 'vp_broken'")
        .unwrap()
        .rows;
    assert_rows_eq(&tl, &[vec![i(0)]]);
}

#[test]
fn audit3_triggers_upsert_views_dml_indexed_by_and_eqp() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. Stale cached_max_rowid after live re-eval rowid change in multi-row UPDATE!
    conn.execute(
        "CREATE TABLE live_rid(id INTEGER PRIMARY KEY, x INT);
         INSERT INTO live_rid VALUES (1, 10), (2, 20);
         CREATE TRIGGER tr_lr AFTER UPDATE ON live_rid WHEN OLD.id = 1 BEGIN
            UPDATE live_rid SET x = 100 WHERE id = 2;
         END;
         UPDATE live_rid SET id = CASE WHEN x = 100 THEN 50 ELSE id END, x = x + 1;",
    )
    .unwrap();
    // Auto-rowid INSERT must NOT collide with rowid 50!
    conn.execute("INSERT INTO live_rid(x) VALUES (999);")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, x FROM live_rid ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(1), i(11)], vec![i(50), i(101)], vec![i(51), i(999)]],
    );

    // 2. Stale known_max_rowid / autoinc_seq_cache after UPSERT triggers & AUTOINCREMENT DO UPDATE SET id = 200!
    conn.execute(
        "CREATE TABLE ai_up(id INTEGER PRIMARY KEY AUTOINCREMENT, k TEXT UNIQUE, v INT);
         INSERT INTO ai_up(k, v) VALUES ('a', 1);
         INSERT INTO ai_up(k, v) VALUES ('a', 2) ON CONFLICT(k) DO UPDATE SET id = 200, v = excluded.v;",
    )
    .unwrap();
    // Per C SQLite: sqlite_sequence.seq is 2 (pre-allocated candidate), NOT 200!
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_up'")
            .unwrap()
            .rows,
        &[vec![i(2)]],
    );
    assert_rows_eq(
        &conn.query("SELECT id, k, v FROM ai_up").unwrap().rows,
        &[vec![i(200), t("a"), i(2)]],
    );
    // ON CONFLICT DO NOTHING also advances AUTOINCREMENT sequence for the allocated candidate rowid.
    conn.execute("INSERT INTO ai_up(k, v) VALUES ('a', 3) ON CONFLICT(k) DO NOTHING;")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_up'")
            .unwrap()
            .rows,
        &[vec![i(201)]],
    );

    // UPSERT BEFORE UPDATE RAISE(IGNORE) mutating table refreshes known_max_rowid & autoinc_seq_cache!
    conn.execute(
        "CREATE TABLE up_ign(id INTEGER PRIMARY KEY AUTOINCREMENT, k TEXT UNIQUE);
         CREATE TRIGGER tr_up_ign BEFORE UPDATE ON up_ign BEGIN
            INSERT INTO up_ign(id, k) VALUES (500, 'from_trig');
            SELECT RAISE(IGNORE);
         END;
         INSERT INTO up_ign(k) VALUES ('x');
         INSERT INTO up_ign(k) VALUES ('x') ON CONFLICT(k) DO UPDATE SET k = 'x2';
         INSERT INTO up_ign(k) VALUES ('y');",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, k FROM up_ign ORDER BY id")
            .unwrap()
            .rows,
        &[
            vec![i(1), t("x")],
            vec![i(500), t("from_trig")],
            vec![i(501), t("y")],
        ],
    );

    // 3. OR REPLACE + recursive_triggers = ON + AFTER DELETE reinserting conflicting row!
    conn.execute(
        "PRAGMA recursive_triggers = ON;
         CREATE TABLE rpu(id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO rpu VALUES (1, 10, 'old');
         CREATE TRIGGER tr_rpu AFTER DELETE ON rpu WHEN OLD.id = 1 BEGIN
            INSERT INTO rpu VALUES (2, 10, 'reborn');
         END;
         INSERT OR REPLACE INTO rpu VALUES (1, 10, 'new');",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, u, v FROM rpu ORDER BY id").unwrap().rows,
        &[vec![i(1), i(10), t("new")]],
    );
    // Reinserting the exact same rowid fails with UNIQUE constraint failed!
    conn.execute(
        "CREATE TABLE rpr(id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO rpr VALUES (1, 10, 'old');
         CREATE TRIGGER tr_rpr AFTER DELETE ON rpr WHEN OLD.u = 10 BEGIN
            INSERT INTO rpr VALUES (1, 99, 'reborn');
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT OR REPLACE INTO rpr VALUES (1, 10, 'new')")
        .unwrap_err()
        .to_string()
        .contains("UNIQUE constraint failed"));
    conn.execute("PRAGMA recursive_triggers = OFF;").unwrap();

    // 4. UPSERT DO UPDATE mandatory ConflictAction::Abort (overrides OR IGNORE/REPLACE, schema ON CONFLICT,
    //    outer trigger override, AND inner trigger INSERT OR IGNORE)!
    conn.execute(
        "CREATE TABLE uab(
            id INT PRIMARY KEY,
            u INT UNIQUE ON CONFLICT IGNORE,
            nn INT NOT NULL ON CONFLICT REPLACE DEFAULT 7
         );
         INSERT INTO uab VALUES (1, 10, 1), (2, 20, 2);",
    )
    .unwrap();
    // INSERT OR IGNORE / OR REPLACE + DO UPDATE colliding on secondary UNIQUE or NOT NULL MUST ABORT!
    assert!(conn
        .execute(
            "INSERT OR IGNORE INTO uab VALUES (1, 99, 9)
             ON CONFLICT(id) DO UPDATE SET u = 20",
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT OR REPLACE INTO uab VALUES (1, 99, 9)
             ON CONFLICT(id) DO UPDATE SET u = 20",
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT OR REPLACE INTO uab VALUES (1, 99, 9)
             ON CONFLICT(id) DO UPDATE SET nn = NULL",
        )
        .is_err());
    // Inner AFTER UPDATE trigger's `INSERT OR IGNORE` is overridden to ABORT during DO UPDATE!
    conn.execute(
        "CREATE TABLE uab_log(k INT UNIQUE);
         INSERT INTO uab_log VALUES (1);
         CREATE TRIGGER tr_uab_au AFTER UPDATE ON uab BEGIN
            INSERT OR IGNORE INTO uab_log VALUES (1);
         END;",
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT OR IGNORE INTO uab VALUES (1, 10, 1)
             ON CONFLICT(id) DO UPDATE SET nn = 5",
        )
        .is_err());
    conn.execute("DROP TRIGGER tr_uab_au;").unwrap();

    // 5. excluded.rowid & ON CONFLICT(rowid) on BOTH non-IPK and IPK rowid tables!
    conn.execute(
        "CREATE TABLE erid(a INT UNIQUE, b INT);
         INSERT INTO erid(rowid, a, b) VALUES (1, 10, 0);
         INSERT INTO erid(rowid, a, b) VALUES (99, 10, 5)
            ON CONFLICT(a) DO UPDATE SET b = excluded.rowid;
         INSERT INTO erid(rowid, a, b) VALUES (1, 20, 77)
            ON CONFLICT(rowid) DO UPDATE SET b = excluded.b;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT rowid, a, b FROM erid").unwrap().rows,
        &[vec![i(1), i(10), i(77)]],
    );

    // 6. UPDATE ... FROM s where s is shadowed by a WITH CTE!
    conn.execute(
        "CREATE TABLE s(x INT);
         CREATE TABLE ut(k INT, v INT);
         INSERT INTO ut VALUES (1, 0);
         WITH s(k, v) AS (VALUES (1, 42))
         UPDATE ut SET v = s.v FROM s WHERE ut.k = s.k;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT k, v FROM ut").unwrap().rows,
        &[vec![i(1), i(42)]],
    );

    // 7. Static RAISE() in dead CASE branch on single-row INSERT fast path rejected outside triggers!
    assert!(conn
        .execute("INSERT INTO ut VALUES (2, CASE WHEN 0 THEN RAISE(ABORT, 'x') ELSE 1 END)")
        .unwrap_err()
        .to_string()
        .contains("RAISE() may only be used within a trigger-program"));

    // 8. Multi-row trigger UPDATE/DELETE hitting RAISE(IGNORE) and RAISE(FAIL) keeps earlier rows!
    conn.execute(
        "CREATE TABLE mr_u(id INTEGER PRIMARY KEY, v INT);
         INSERT INTO mr_u VALUES (1, 1), (2, 2), (3, 3);
         CREATE TABLE mr_drv(x INT);
         CREATE TRIGGER tr_mru_ign AFTER INSERT ON mr_drv WHEN NEW.x = 1 BEGIN
            UPDATE mr_u SET v = CASE WHEN id = 2 THEN RAISE(IGNORE) ELSE v + 10 END;
         END;
         CREATE TRIGGER tr_mru_fail AFTER INSERT ON mr_drv WHEN NEW.x = 2 BEGIN
            UPDATE mr_u SET v = CASE WHEN id = 2 THEN RAISE(FAIL, 'stop2') ELSE v + 100 END;
         END;
         INSERT INTO mr_drv VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, v FROM mr_u ORDER BY id").unwrap().rows,
        &[vec![i(1), i(11)], vec![i(2), i(2)], vec![i(3), i(3)]],
    );
    assert!(conn
        .execute("INSERT INTO mr_drv VALUES (2)")
        .unwrap_err()
        .to_string()
        .contains("stop2"));
    assert_rows_eq(
        &conn.query("SELECT id, v FROM mr_u ORDER BY id").unwrap().rows,
        &[vec![i(1), i(111)], vec![i(2), i(2)], vec![i(3), i(3)]],
    );
    conn.execute(
        "CREATE TABLE mr_d(id INTEGER PRIMARY KEY);
         INSERT INTO mr_d VALUES (1), (2), (3);
         CREATE TRIGGER tr_mrd_ign AFTER INSERT ON mr_drv WHEN NEW.x = 3 BEGIN
            DELETE FROM mr_d WHERE CASE WHEN id = 2 THEN RAISE(IGNORE) ELSE 1 END;
         END;
         INSERT INTO mr_drv VALUES (3);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id FROM mr_d ORDER BY id").unwrap().rows,
        &[vec![i(2)], vec![i(3)]],
    );

    // 9. Multi-row UPDATE with correlated SET subquery + trigger re-evaluates subquery per row!
    conn.execute(
        "CREATE TABLE cs(id INTEGER PRIMARY KEY, v INT, s INT);
         INSERT INTO cs VALUES (1, 10, 0), (2, 20, 0);
         CREATE TRIGGER tr_cs AFTER UPDATE ON cs BEGIN SELECT 1; END;
         UPDATE cs SET v = v + 1, s = (SELECT sum(v) FROM cs);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, v, s FROM cs ORDER BY id").unwrap().rows,
        &[vec![i(1), i(11), i(30)], vec![i(2), i(21), i(31)]],
    );

    // 10. UPDATE OR REPLACE SET id = id + 1 matches C SQLite ([(3, 'a')])!
    conn.execute(
        "CREATE TABLE uor(id INTEGER PRIMARY KEY, v TEXT);
         INSERT INTO uor VALUES (1, 'a'), (2, 'b');
         UPDATE OR REPLACE uor SET id = id + 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, v FROM uor ORDER BY id").unwrap().rows,
        &[vec![i(3), t("a")]],
    );

    // 11. View INSTEAD OF: duplicate INSERT target keeps FIRST; UPDATE applies affinities & supports FROM s.rowid!
    conn.execute(
        "CREATE TABLE vb(a INT, b REAL, c TEXT);
         INSERT INTO vb VALUES (1, 1.0, 'init');
         CREATE VIEW vv AS SELECT a, b, c FROM vb;
         CREATE TABLE vlog(k TEXT, a ANY, b ANY, c ANY);
         CREATE TRIGGER tr_v_ins INSTEAD OF INSERT ON vv BEGIN
            INSERT INTO vlog VALUES ('ins', NEW.a, NEW.b, NEW.c);
         END;
         CREATE TRIGGER tr_v_upd INSTEAD OF UPDATE ON vv BEGIN
            INSERT INTO vlog VALUES ('upd', typeof(NEW.a) || ':' || NEW.a, typeof(NEW.b) || ':' || NEW.b, typeof(NEW.c) || ':' || NEW.c);
         END;
         INSERT INTO vv(a, a) VALUES (1, 2);
         CREATE TABLE vs(x INT);
         INSERT INTO vs(rowid, x) VALUES (77, 1);
         UPDATE vv SET a = '42', b = 7, c = vs.rowid FROM vs WHERE vv.a = vs.x;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT k, a, b, c FROM vlog ORDER BY rowid").unwrap().rows,
        &[
            vec![t("ins"), i(1), SqlValue::Null, SqlValue::Null],
            vec![t("upd"), t("integer:42"), t("real:7.0"), t("text:77")],
        ],
    );

    // 12. trigger.rs rewrites: RENAME COLUMN with subquery/TVF & ORDER BY alias; RENAME TABLE with FROM t AS t!
    conn.execute(
        "CREATE TABLE rt_t(a INT, b INT);
         CREATE TABLE rt_log(x INT);
         CREATE TRIGGER tr_rt1 AFTER INSERT ON rt_t BEGIN
            INSERT INTO rt_log SELECT a FROM rt_t, json_each('[1]') ORDER BY a;
         END;
         CREATE TRIGGER tr_rt2 AFTER INSERT ON rt_t BEGIN
            INSERT INTO rt_log SELECT b AS a FROM rt_t ORDER BY a;
         END;
         CREATE TRIGGER tr_rt3 AFTER INSERT ON rt_t BEGIN
            INSERT INTO rt_log SELECT rt_t.b FROM rt_t AS rt_t;
         END;
         ALTER TABLE rt_t RENAME COLUMN a TO a_ren;
         ALTER TABLE rt_t RENAME TO rt_t_ren;
         INSERT INTO rt_t_ren VALUES (10, 20);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT x FROM rt_log ORDER BY rowid").unwrap().rows,
        &[vec![i(20)], vec![i(20)], vec![i(10)]],
    );

    // 13. Upfront validation on 0-row DML (UPSERT SET, RETURNING agg/window/col, direct-point WHERE)!
    assert!(conn
        .execute("INSERT INTO uor VALUES (99, 'z') ON CONFLICT(id) DO UPDATE SET bogus = 1")
        .is_err());
    assert!(conn
        .execute("DELETE FROM uor WHERE 0 RETURNING sum(id)")
        .unwrap_err()
        .to_string()
        .contains("misuse of aggregate"));
    assert!(conn
        .execute("DELETE FROM uor WHERE 0 RETURNING row_number() OVER ()")
        .unwrap_err()
        .to_string()
        .contains("misuse of window"));
    assert!(conn
        .execute("DELETE FROM uor WHERE 0 RETURNING no_such_col")
        .is_err());
    assert!(conn
        .execute("UPDATE uor AS u SET v = 'x' WHERE 0 RETURNING u.v")
        .is_err());
    assert!(conn
        .execute("UPDATE uor SET v = 'x' WHERE id = 999 AND no_such_col = 1")
        .is_err());
    assert!(conn
        .execute("DELETE FROM uor WHERE id = 999 AND no_such_col = 1")
        .is_err());

    // RETURNING memory metering via BudgetMeter:
    {
        let mut mconn = SqliteConnection::open_in_memory(ConnectionOptions {
            meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
                max_memory_bytes: Some(256 * 1024),
                ..ResourceLimits::default()
            }))),
            ..ConnectionOptions::default()
        })
        .unwrap();
        mconn.execute("CREATE TABLE rm(id INT, s TEXT);").unwrap();
        assert!(mconn
            .execute(
                "WITH RECURSIVE c(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM c WHERE n < 200)
                 INSERT INTO rm SELECT n, printf('%.*c', 4000, 'a') FROM c RETURNING s",
            )
            .is_err());
    }

    // 14. INDEXED BY validation on UPDATE, DELETE, SELECT (partial index, CTE, view, TVF)!
    conn.execute(
        "CREATE TABLE ib_t(id INTEGER PRIMARY KEY, a INT, b INT);
         CREATE INDEX ib_full ON ib_t(a);
         CREATE INDEX ib_part ON ib_t(b) WHERE b > 0;
         CREATE VIEW ib_v AS SELECT * FROM ib_t;",
    )
    .unwrap();
    assert!(conn
        .execute("UPDATE ib_t INDEXED BY missing_idx SET a = 1")
        .unwrap_err()
        .to_string()
        .contains("no such index"));
    assert!(conn
        .execute("UPDATE ib_t INDEXED BY ib_part SET a = 1")
        .unwrap_err()
        .to_string()
        .contains("cannot use index"));
    assert!(conn
        .execute("DELETE FROM ib_t INDEXED BY missing_idx WHERE id = 1")
        .unwrap_err()
        .to_string()
        .contains("no such index"));
    assert!(conn
        .execute("DELETE FROM ib_t INDEXED BY ib_part WHERE b = 1")
        .unwrap_err()
        .to_string()
        .contains("cannot use index"));
    assert!(conn
        .query("SELECT * FROM ib_t INDEXED BY ib_part WHERE b = 5")
        .unwrap_err()
        .to_string()
        .contains("cannot use index"));
    assert!(conn
        .query("WITH c AS (SELECT 1) SELECT * FROM c INDEXED BY ib_full")
        .unwrap_err()
        .to_string()
        .contains("no such index"));
    assert!(conn
        .query("SELECT * FROM ib_v INDEXED BY ib_full")
        .unwrap_err()
        .to_string()
        .contains("no such index"));
    assert!(conn
        .query("SELECT * FROM pragma_table_list INDEXED BY ib_full")
        .unwrap_err()
        .to_string()
        .contains("no such index"));

    // 15. EXPLAIN QUERY PLAN fixes: IN(1,2) ORDER BY, ORDER BY alias shadowing,
    //     LEFT JOIN NULL pushdown, RHS COLLATE precedence, window frame subquery!
    conn.execute(
        "CREATE TABLE eq3(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT);
         CREATE INDEX eq3_ab ON eq3(a, b);
         CREATE INDEX eq3_s ON eq3(s);",
    )
    .unwrap();
    let eqp_in2 = conn
        .execute("EXPLAIN QUERY PLAN SELECT a, b FROM eq3 WHERE a IN (1, 2) ORDER BY b")
        .unwrap();
    assert!(eqp_in2
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("TEMP B-TREE FOR ORDER BY")));
    let eqp_in1 = conn
        .execute("EXPLAIN QUERY PLAN SELECT a, b FROM eq3 WHERE a IN (1) ORDER BY b")
        .unwrap();
    assert!(!eqp_in1
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("TEMP B-TREE FOR ORDER BY")));
    let eqp_alias = conn
        .execute("EXPLAIN QUERY PLAN SELECT b AS id FROM eq3 ORDER BY id")
        .unwrap();
    assert!(eqp_alias
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("TEMP B-TREE FOR ORDER BY")));
    let eqp_lj = conn
        .execute("EXPLAIN QUERY PLAN SELECT l.id FROM eq3 l LEFT JOIN eq3 r ON l.a = r.a WHERE r.b IS NULL")
        .unwrap();
    // Must not use `eq3_ab` for `r.b IS NULL` (only `r.a = ?` from ON, or SCAN r).
    assert!(!eqp_lj
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("b=?") || r[3].as_str().unwrap().contains("b IS NULL")));
    let eqp_coll = conn
        .execute("EXPLAIN QUERY PLAN SELECT id FROM eq3 WHERE s = 'abc' COLLATE NOCASE")
        .unwrap();
    assert!(!eqp_coll
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USING INDEX eq3_s")));
    let eqp_wf = conn
        .execute(
            "EXPLAIN QUERY PLAN SELECT sum(a) OVER (ORDER BY id ROWS BETWEEN (SELECT 1) PRECEDING AND CURRENT ROW) FROM eq3",
        )
        .unwrap();
    assert!(eqp_wf
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("SCALAR SUBQUERY")));
}

#[test]
fn audit4_triggers_replace_autoinc_views_returning_and_dml() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // S1 #1 (P0): INSERT OR REPLACE with recursive_triggers must probe secondary UNIQUE indexes
    // LIVE after PK delete triggers run, never using stale row snapshots!
    conn.execute(
        "PRAGMA recursive_triggers = ON;
         CREATE TABLE s1_t(id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         CREATE INDEX s1_iv ON s1_t(v);
         INSERT INTO s1_t VALUES (1, 10, 'old1'), (2, 20, 'old2');
         CREATE TABLE s1_log(msg TEXT);
         CREATE TRIGGER s1_ad AFTER DELETE ON s1_t WHEN OLD.id = 1 BEGIN
            UPDATE s1_t SET v = 'mut2' WHERE id = 2;
            INSERT INTO s1_log VALUES ('del1');
         END;
         INSERT OR REPLACE INTO s1_t VALUES (1, 20, 'new1');",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, u, v FROM s1_t ORDER BY id").unwrap().rows,
        &[vec![i(1), i(20), t("new1")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // If PK delete trigger already deleted row 2, BEFORE/AFTER DELETE must NOT spuriously fire on row 2!
    conn.execute(
        "CREATE TABLE s1_t2(id INTEGER PRIMARY KEY, u INT UNIQUE);
         INSERT INTO s1_t2 VALUES (1, 10), (2, 20);
         CREATE TABLE s1_log2(id INT);
         CREATE TRIGGER s1_bd2 BEFORE DELETE ON s1_t2 BEGIN
            INSERT INTO s1_log2 VALUES (OLD.id);
         END;
         CREATE TRIGGER s1_ad2 AFTER DELETE ON s1_t2 WHEN OLD.id = 1 BEGIN
            DELETE FROM s1_t2 WHERE id = 2;
         END;
         INSERT OR REPLACE INTO s1_t2 VALUES (1, 20);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id FROM s1_log2 ORDER BY rowid").unwrap().rows,
        &[vec![i(1)], vec![i(2)]], // fired once for row 1, once inside trigger DELETE for row 2, NOT a second time!
    );

    // S1 #2a: AFTER DELETE trigger re-inserting same UNIQUE key on secondary UNIQUE conflict MUST fail!
    conn.execute(
        "CREATE TABLE s1_2a(id INTEGER PRIMARY KEY, u INT UNIQUE, x INT);
         INSERT INTO s1_2a VALUES (1, 100, 10);
         CREATE TRIGGER s1_2a_tr AFTER DELETE ON s1_2a WHEN OLD.id = 1 BEGIN
            INSERT INTO s1_2a VALUES (2, 100, 99);
         END;",
    )
    .unwrap();
    let err_2a = conn
        .execute("INSERT OR REPLACE INTO s1_2a VALUES (10, 100, 20)")
        .unwrap_err();
    assert!(err_2a.to_string().contains("UNIQUE constraint failed"));

    // S1 #2b: AFTER DELETE on PK conflict inserts row with same UNIQUE key -> secondary check deletes it -> succeeds!
    conn.execute(
        "CREATE TABLE s1_2b(id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         INSERT INTO s1_2b VALUES (1, 10, 'old');
         CREATE TRIGGER s1_2b_tr AFTER DELETE ON s1_2b WHEN OLD.id = 1 BEGIN
            INSERT INTO s1_2b VALUES (2, 10, 'reborn');
         END;
         INSERT OR REPLACE INTO s1_2b VALUES (1, 10, 'new');",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, u, v FROM s1_2b").unwrap().rows,
        &[vec![i(1), i(10), t("new")]],
    );

    // S1 #3: AUTOINCREMENT advances sqlite_sequence for candidate rowid even on DO NOTHING / DO UPDATE / OR IGNORE!
    conn.execute(
        "CREATE TABLE ai_a(id INTEGER PRIMARY KEY AUTOINCREMENT, u INT UNIQUE);
         INSERT INTO ai_a VALUES (1, 10);
         INSERT INTO ai_a(id, u) VALUES (50, 10) ON CONFLICT(u) DO NOTHING;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_a'")
            .unwrap()
            .rows,
        &[vec![i(50)]],
    );
    conn.execute(
        "CREATE TABLE ai_b(id INTEGER PRIMARY KEY AUTOINCREMENT, u INT UNIQUE);
         INSERT INTO ai_b VALUES (1, 10);
         INSERT INTO ai_b(id, u) VALUES (50, 10) ON CONFLICT(u) DO UPDATE SET id = 200;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_b'")
            .unwrap()
            .rows,
        &[vec![i(50)]],
    );
    conn.execute(
        "CREATE TABLE ai_c(id INTEGER PRIMARY KEY AUTOINCREMENT, u INT UNIQUE, x INT NOT NULL CHECK (x > 0));
         INSERT INTO ai_c VALUES (1, 10, 5);
         INSERT OR IGNORE INTO ai_c(u, x) VALUES (10, 5);
         INSERT OR IGNORE INTO ai_c(u, x) VALUES (20, NULL);
         INSERT OR IGNORE INTO ai_c(u, x) VALUES (30, -1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_c'")
            .unwrap()
            .rows,
        &[vec![i(4)]],
    );

    // S1 #4: XDB-dialect triggers survive ALTER TABLE RENAME TO and RENAME COLUMN!
    let mut xconn = SqliteConnection::open_in_memory(ConnectionOptions {
        dialect: xdb_parser::Dialect::Xdb,
        ..ConnectionOptions::default()
    })
    .unwrap();
    xconn
        .execute(
            "CREATE TABLE xt(a INT, b INT);
             CREATE TABLE xlog(b INT);
             CREATE TRIGGER xtr AFTER INSERT ON xt BEGIN
                INSERT INTO xlog SELECT * EXCLUDE (a) FROM xt WHERE xt.a = NEW.a;
             END;
             ALTER TABLE xt RENAME TO xt2;
             ALTER TABLE xt2 RENAME COLUMN a TO a2;
             INSERT INTO xt2 VALUES (7, 42);",
        )
        .unwrap();
    assert_rows_eq(
        &xconn.query("SELECT b FROM xlog").unwrap().rows,
        &[vec![i(42)]],
    );

    // S1 #5: ALTER TABLE RENAME COLUMN rejects broken INSTEAD OF view trigger & broken USING in trigger CTE/subquery!
    conn.execute(
        "CREATE TABLE src_io(a INT, b INT);
         CREATE VIEW v_io_src AS SELECT * FROM src_io;
         CREATE TABLE dst_io(v INT);
         CREATE TRIGGER tr_io_src INSTEAD OF INSERT ON v_io_src BEGIN
            INSERT INTO dst_io VALUES (NEW.a);
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE src_io RENAME COLUMN a TO a2")
        .is_err());
    conn.execute("DROP TRIGGER tr_io_src; DROP VIEW v_io_src;").unwrap();

    conn.execute(
        "CREATE TABLE ju_a(a INT);
         CREATE TABLE ju_b(a INT);
         CREATE TABLE ju_ev(x INT);
         CREATE TRIGGER tr_ju AFTER INSERT ON ju_ev BEGIN
            SELECT a FROM (SELECT * FROM ju_a) JOIN ju_b USING (a);
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE ju_a RENAME COLUMN a TO a2")
        .is_err());
    conn.execute("DROP TRIGGER tr_ju;").unwrap();

    // S1 #6: View INSTEAD OF UPDATE/DELETE defers RAISE(IGNORE) / RAISE(FAIL) until after earlier matched rows!
    conn.execute(
        "CREATE TABLE vr_base(id INT PRIMARY KEY, v INT);
         INSERT INTO vr_base VALUES (1, 10), (2, 20), (3, 30);
         CREATE VIEW vr_v AS SELECT id, v FROM vr_base ORDER BY id;
         CREATE TABLE vr_log(id INT);
         CREATE TRIGGER vr_io_u INSTEAD OF UPDATE ON vr_v BEGIN
            INSERT INTO vr_log VALUES (NEW.id);
         END;
         CREATE TRIGGER vr_io_d INSTEAD OF DELETE ON vr_v BEGIN
            INSERT INTO vr_log VALUES (-OLD.id);
         END;
         CREATE TABLE vr_ev(k INT);
         CREATE TRIGGER vr_ev_tr AFTER INSERT ON vr_ev BEGIN
            UPDATE vr_v SET v = CASE WHEN id = 2 THEN RAISE(IGNORE) ELSE v + 1 END;
            DELETE FROM vr_v WHERE CASE WHEN id = 2 THEN RAISE(IGNORE) ELSE 1 END;
         END;
         INSERT INTO vr_ev VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT id FROM vr_log ORDER BY rowid").unwrap().rows,
        &[vec![i(1)]],
    );

    // S1 #7: Multi-row VALUES inside trigger preserves earlier rows on RAISE(IGNORE) / RAISE(FAIL)!
    conn.execute(
        "CREATE TABLE mv_log(x INT);
         CREATE TABLE mv_ev(k INT);
         CREATE TRIGGER mv_tr AFTER INSERT ON mv_ev BEGIN
            INSERT INTO mv_log VALUES (10), (CASE WHEN NEW.k = 1 THEN RAISE(IGNORE) ELSE RAISE(FAIL, 'stop2') END), (30);
         END;
         INSERT INTO mv_ev VALUES (1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT x FROM mv_log ORDER BY rowid").unwrap().rows,
        &[vec![i(10)]],
    );
    assert!(conn.execute("INSERT INTO mv_ev VALUES (2)").is_err());
    assert_rows_eq(
        &conn.query("SELECT x FROM mv_log ORDER BY rowid").unwrap().rows,
        &[vec![i(10)], vec![i(10)]],
    );

    // S1 #8: changes() resets to 0 after a failed statement inside BEGIN (savepoint-elided path)!
    conn.execute(
        "CREATE TABLE ch_t(id INTEGER PRIMARY KEY, x INT NOT NULL);
         BEGIN;
         INSERT INTO ch_t VALUES (1, 10);",
    )
    .unwrap();
    assert_eq!(conn.changes(), 1);
    assert!(conn.execute("INSERT INTO ch_t VALUES (2, NULL)").is_err());
    assert_eq!(conn.changes(), 0);
    conn.execute("COMMIT;").unwrap();

    // S1 #9: Zero-row column validation on UPDATE / DELETE / UPSERT / aliased table / views!
    conn.execute("CREATE TABLE zr(id INT PRIMARY KEY, a INT);").unwrap();
    assert!(conn.execute("UPDATE zr SET a = nonexistent").is_err());
    assert!(conn.execute("DELETE FROM zr WHERE nonexistent = 1").is_err());
    assert!(conn
        .execute("INSERT INTO zr VALUES (1, 10) ON CONFLICT(id) DO UPDATE SET a = nonexistent")
        .is_err());
    assert!(conn.execute("UPDATE zr SET a = nonexistent WHERE 0").is_err());
    assert!(conn
        .execute("UPDATE zr AS u SET a = zr.a WHERE 0")
        .is_err());
    conn.execute(
        "CREATE VIEW zr_v AS SELECT id, a FROM zr WHERE 0;
         CREATE TRIGGER zr_vu INSTEAD OF UPDATE ON zr_v BEGIN SELECT 1; END;
         CREATE TRIGGER zr_vd INSTEAD OF DELETE ON zr_v BEGIN SELECT 1; END;",
    )
    .unwrap();
    assert!(conn.execute("UPDATE zr_v SET a = nonexistent").is_err());
    assert!(conn.execute("DELETE FROM zr_v WHERE nonexistent = 1").is_err());

    // S1 #10: RETURNING rejects schema-qualified columns, FILTER on non-aggregates, wrong-arity aggregates!
    assert!(conn
        .execute("INSERT INTO zr VALUES (2, 20) RETURNING main.zr.a")
        .is_err());
    assert!(conn
        .execute("INSERT INTO zr VALUES (3, 30) RETURNING abs(a) FILTER (WHERE a > 0)")
        .is_err());
    assert!(conn
        .execute("INSERT INTO zr VALUES (4, 40) RETURNING count(1, 2)")
        .is_err());
    let ret_ok = conn
        .execute("INSERT INTO zr VALUES (5, 50) RETURNING min(a, 100)")
        .unwrap();
    assert_rows_eq(&ret_ok.rows, &[vec![i(50)]]);

    // S1 #11: Variables in named WINDOW clause of trigger rejected; RAISE in WITH CTE of view INSERT rejected!
    assert!(conn
        .execute(
            "CREATE TRIGGER bad_win_var AFTER INSERT ON zr BEGIN
                SELECT sum(a) OVER w FROM zr WINDOW w AS (PARTITION BY ?1);
             END;",
        )
        .is_err());
    conn.execute(
        "CREATE TRIGGER zr_vi INSTEAD OF INSERT ON zr_v BEGIN SELECT 1; END;",
    )
    .unwrap();
    assert!(conn
        .execute("WITH c AS (SELECT RAISE(ABORT, 'x'), 1) INSERT INTO zr_v SELECT * FROM c")
        .is_err());
}

#[test]
fn audit4_fk_ddl_views_maintenance_and_pragmas() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // S2 #1: DEFERRABLE INITIALLY DEFERRED ON DELETE/UPDATE RESTRICT + defer_foreign_keys = ON!
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE fp(id INT PRIMARY KEY);
         CREATE TABLE fc(pid INT REFERENCES fp(id) ON DELETE RESTRICT ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO fp VALUES (1);
         INSERT INTO fc VALUES (1);
         BEGIN;
         PRAGMA defer_foreign_keys = ON;
         DELETE FROM fp WHERE id = 1;
         INSERT INTO fp VALUES (1);
         COMMIT;",
    )
    .unwrap();
    conn.execute(
        "BEGIN;
         PRAGMA defer_foreign_keys = ON;
         UPDATE fp SET id = 2 WHERE id = 1;
         INSERT INTO fp VALUES (1);
         COMMIT;",
    )
    .unwrap();

    // S2 #2: Pending deferred FK violation on table A does not trigger foreign key mismatch on unrelated parent B!
    conn.execute(
        "CREATE TABLE p_ok(id INT PRIMARY KEY);
         CREATE TABLE c_def(pid INT REFERENCES p_ok(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TABLE p_broken(x INT);
         CREATE TABLE c_broken(px INT REFERENCES p_broken(x));
         BEGIN;
         INSERT INTO c_def VALUES (99);
         INSERT INTO p_broken VALUES (1);
         INSERT INTO p_ok VALUES (99);
         COMMIT;",
    )
    .unwrap();

    // S2 #3: Unrelated view with LIMIT 1 and runtime-failing json('invalid') must not block ALTER TABLE!
    conn.execute(
        "CREATE VIEW v_bad_json AS SELECT json('invalid') AS j LIMIT 1;
         CREATE TABLE alt_t(a INT, b INT);
         ALTER TABLE alt_t RENAME COLUMN a TO a2;",
    )
    .unwrap();

    // S2 #4: ORDER BY alias shadowed by SELECT b AS a is NOT rewritten on RENAME COLUMN a TO a2!
    conn.execute(
        "CREATE TABLE ord_t(a INT, b INT);
         INSERT INTO ord_t VALUES (1, 20), (2, 10);
         CREATE VIEW v_ord_alias AS SELECT b AS a FROM ord_t ORDER BY a;
         ALTER TABLE ord_t RENAME COLUMN a TO a2;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT a FROM v_ord_alias").unwrap().rows,
        &[vec![i(10)], vec![i(20)]],
    );

    // S2 #5: DROP COLUMN rejects broken indirect view-over-view AND explicit column-list views!
    conn.execute(
        "CREATE TABLE dc_t(a INT, b INT);
         CREATE VIEW dc_v1 AS SELECT * FROM dc_t;
         CREATE VIEW dc_v2 AS SELECT b FROM dc_v1;",
    )
    .unwrap();
    assert!(conn.execute("ALTER TABLE dc_t DROP COLUMN b").is_err());
    conn.execute("DROP VIEW dc_v2; DROP VIEW dc_v1;").unwrap();
    conn.execute("CREATE VIEW dc_v3(x, y) AS SELECT * FROM dc_t;").unwrap();
    assert!(conn.execute("ALTER TABLE dc_t DROP COLUMN b").is_err());
    conn.execute("DROP VIEW dc_v3;").unwrap();

    // S2 #6: NATURAL JOIN RHS unqualified column not rewritten when LHS owns it; qualified * EXCLUDE/REPLACE!
    conn.execute(
        "CREATE TABLE nj_l(a INT, l INT);
         CREATE TABLE nj_r(a INT, r INT);
         INSERT INTO nj_l VALUES (1, 10);
         INSERT INTO nj_r VALUES (1, 20);
         CREATE VIEW v_nj AS SELECT a, l, r FROM nj_l NATURAL JOIN nj_r;
         ALTER TABLE nj_r RENAME COLUMN a TO a2;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.query("SELECT a, l, r FROM v_nj").unwrap().rows,
        &[vec![i(1), i(10), i(20)]],
    );

    let mut xconn = SqliteConnection::open_in_memory(ConnectionOptions {
        dialect: xdb_parser::Dialect::Xdb,
        ..ConnectionOptions::default()
    })
    .unwrap();
    xconn
        .execute(
            "CREATE TABLE xr_u(a INT, x INT);
             CREATE TABLE xr_t(a INT, y INT);
             CREATE VIEW v_excl AS SELECT xr_u.* EXCLUDE (a), xr_t.y FROM xr_u JOIN xr_t ON xr_u.x = xr_t.y;
             CREATE VIEW v_rep AS SELECT * REPLACE (xr_t.y + 1 AS y) FROM xr_t;
             ALTER TABLE xr_t RENAME COLUMN a TO a2;
             ALTER TABLE xr_t DROP COLUMN a2;
             ALTER TABLE xr_t RENAME TO xr_t2;
             INSERT INTO xr_u VALUES (1, 5);
             INSERT INTO xr_t2 VALUES (5);",
        )
        .unwrap();
    assert_rows_eq(
        &xconn.query("SELECT x, y FROM v_excl").unwrap().rows,
        &[vec![i(5), i(5)]],
    );
    assert_rows_eq(
        &xconn.query("SELECT y FROM v_rep").unwrap().rows,
        &[vec![i(6)]],
    );

    // S2 #10 & S2 #12: VACUUM & PRAGMA table_info on XDB view under Dialect::Sqlite!
    let xdb_bytes = xconn.to_bytes();
    let mut sconn = SqliteConnection::open_from_bytes(
        &xdb_bytes,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    let ti = sconn.execute("PRAGMA table_info(v_excl)").unwrap();
    assert_eq!(ti.rows.len(), 2);
    sconn.execute("VACUUM;").unwrap();
    assert_rows_eq(
        &sconn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // S2 #7: WITHOUT ROWID DROP COLUMN preserves NOCASE PK ordering!
    conn.execute(
        "CREATE TABLE wr_dc(k TEXT COLLATE NOCASE PRIMARY KEY, drop_me INT, keep_me INT) WITHOUT ROWID;
         INSERT INTO wr_dc VALUES ('Alpha', 1, 10), ('beta', 2, 20);
         ALTER TABLE wr_dc DROP COLUMN drop_me;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT k, keep_me FROM wr_dc WHERE k = 'ALPHA'")
            .unwrap()
            .rows,
        &[vec![t("Alpha"), i(10)]],
    );

    // S2 #8 & S2 #9: PRAGMA integrity_check strict storage class, CHECK DQS, ignore_check_constraints, and row-index cross-check!
    // (a) Legacy on-disk CHECK with double-quoted string literal (`CHECK (s != "bad")`):
    let mut chk_db = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    chk_db
        .execute(
            "CREATE TABLE ic_chk(s TEXT CHECK (s != 'bad'));
             INSERT INTO ic_chk VALUES ('good');",
        )
        .unwrap();
    let mut chk_bytes = chk_db.to_bytes();
    let needle = b"'bad'";
    let pos = chk_bytes[..chk_db.page_size() as usize]
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("CHECK literal in sqlite_schema");
    chk_bytes[pos..pos + needle.len()].copy_from_slice(b"\"bad\"");
    let mut chk_loaded = SqliteConnection::open_from_bytes(
        &chk_bytes,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    assert_rows_eq(
        &chk_loaded
            .execute("PRAGMA integrity_check(ic_chk)")
            .unwrap()
            .rows,
        &[vec![t("ok")]],
    );
    // (b) Corrupt STRICT INT column storing TEXT '123' must be flagged as non-INT (not silently coerced)!
    let mut st_db = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    st_db
        .execute("CREATE TABLE ic_st(x INT) STRICT; INSERT INTO ic_st VALUES (123);")
        .unwrap();
    let mut st_bytes = st_db.to_bytes();
    let st_ps = st_db.page_size() as usize;
    // Patch table leaf page 2: replace stored row record [hdr=2, st=1 (1-byte int), 123]
    // with [hdr=2, st=19 (3-byte TEXT), b'1', b'2', b'3'] by rewriting cell 0 cleanly.
    let p2 = &mut st_bytes[st_ps..2 * st_ps];
    let new_cell: [u8; 7] = [5, 1, 2, 19, b'1', b'2', b'3']; // payload_len=5, rowid=1, hdr_len=2, st=19 (len 3 TEXT), "123"
    let new_off = st_ps - new_cell.len();
    p2[new_off..st_ps].copy_from_slice(&new_cell);
    p2[5..7].copy_from_slice(&(new_off as u16).to_be_bytes());
    p2[8..10].copy_from_slice(&(new_off as u16).to_be_bytes());
    let mut st_bad = SqliteConnection::open_from_bytes(
        &st_bytes,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    let st_ic = st_bad.execute("PRAGMA integrity_check(ic_st)").unwrap();
    assert!(
        st_ic.rows[0][0].as_str().unwrap().contains("non-INT value"),
        "expected non-INT value error, got {:?}",
        st_ic.rows
    );
    conn.execute(
        "CREATE TABLE ic_ign(x INT CHECK (x > 0));
         PRAGMA ignore_check_constraints = ON;
         INSERT INTO ic_ign VALUES (-5);",
    )
    .unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check(ic_ign)").unwrap().rows,
        &[vec![t("ok")]],
    );
    conn.execute("PRAGMA ignore_check_constraints = OFF;").unwrap();
    let ic_err = conn.execute("PRAGMA integrity_check(ic_ign)").unwrap();
    assert!(ic_err.rows[0][0]
        .as_str()
        .unwrap()
        .contains("CHECK constraint failed"));
    conn.execute("DROP TABLE ic_ign;").unwrap();

    // Corrupt index value while keeping exact entry count -> integrity_check must report missing row!
    let mut ic_conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    ic_conn
        .execute(
            "CREATE TABLE ic_ix(id INTEGER PRIMARY KEY, v INT);
             CREATE INDEX ic_ix_v ON ic_ix(v);
             INSERT INTO ic_ix VALUES (1, 11111), (2, 22222);",
        )
        .unwrap();
    let mut raw = ic_conn.to_bytes();
    let ps = ic_conn.page_size() as usize;
    // Mutate value 22222 (0x56CE) -> 22223 (0x56CF) ONLY on the index page (page 3), not table page 2!
    let ix_page = &mut raw[2 * ps..3 * ps];
    let pos = ix_page
        .windows(2)
        .position(|w| w == [0x56, 0xCE])
        .expect("index value 22222 encoded in index leaf");
    ix_page[pos + 1] = 0xCF;
    let mut ic_bad = SqliteConnection::open_from_bytes(
        &raw,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    let msgs = ic_bad.execute("PRAGMA integrity_check").unwrap();
    assert!(
        msgs.rows
            .iter()
            .any(|r| r[0].as_str().unwrap().contains("missing from index ic_ix_v")),
        "expected missing from index ic_ix_v, got {:?}",
        msgs.rows
    );

    // S2 #11: PRAGMA integrity_check(-1), integrity_check(view), auto_vacuum, pragma_list!
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check(-1)").unwrap().rows,
        &[vec![t("ok")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check(v_nj)").unwrap().rows,
        &[vec![t("ok")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA auto_vacuum").unwrap().rows,
        &[vec![i(0)]],
    );
    let plist = conn.execute("PRAGMA pragma_list").unwrap();
    assert!(plist.rows.iter().any(|r| r[0].as_str() == Some("auto_vacuum")));
}

#[test]
fn audit4_sql_engine_eqp_utf16_json_and_printf() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // S3 #1: INDEXED BY partial index succeeds when WHERE implies partial predicate (SELECT, EQP, UPDATE, DELETE)!
    conn.execute(
        "CREATE TABLE pi_t(a INT, b INT);
         CREATE INDEX pi_idx ON pi_t(a) WHERE b > 0;
         INSERT INTO pi_t VALUES (1, 5), (2, -3);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT a, b FROM pi_t INDEXED BY pi_idx WHERE b > 0 AND a = 1")
            .unwrap()
            .rows,
        &[vec![i(1), i(5)]],
    );
    let eqp_pi = conn
        .execute("EXPLAIN QUERY PLAN SELECT a FROM pi_t INDEXED BY pi_idx WHERE b > 0 AND a = 1")
        .unwrap();
    assert!(eqp_pi
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USING INDEX pi_idx")));
    assert!(conn
        .query("SELECT a FROM pi_t INDEXED BY pi_idx WHERE a = 1")
        .is_err());
    conn.execute("UPDATE pi_t INDEXED BY pi_idx SET a = 10 WHERE b > 0 AND a = 1")
        .unwrap();
    conn.execute("DELETE FROM pi_t INDEXED BY pi_idx WHERE b > 0 AND a = 10")
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT a, b FROM pi_t").unwrap().rows,
        &[vec![i(2), i(-3)]],
    );

    // S3 #2: Window aggregate FILTER skips evaluating args on filtered-out rows!
    conn.execute(
        "CREATE TABLE wf_t(ok INT, s TEXT);
         INSERT INTO wf_t VALUES (1, '10'), (0, 'not json');",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT sum(json(s)) FILTER (WHERE ok = 1) OVER () FROM wf_t")
            .unwrap()
            .rows,
        &[vec![i(10)], vec![i(10)]],
    );

    // S3 #3: ntile(n) evaluates bucket count once on first row of partition!
    conn.execute(
        "CREATE TABLE nt_t(id INT, n INT);
         INSERT INTO nt_t VALUES (1, 2), (2, 100), (3, 100), (4, 100);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, ntile(n) OVER (ORDER BY id) FROM nt_t ORDER BY id")
            .unwrap()
            .rows,
        &[
            vec![i(1), i(1)],
            vec![i(2), i(1)],
            vec![i(3), i(2)],
            vec![i(4), i(2)],
        ],
    );

    // S3 #4 & S3 #5: UTF-16 encoding parity for octet_length, hex, WHERE/iif blob truthiness, and aggregate BINARY collation!
    let mut u16 = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    u16.execute(
        "PRAGMA encoding = 'UTF-16le';
         CREATE TABLE u16t(s TEXT);
         INSERT INTO u16t VALUES (char(65280)), (char(65536));",
    )
    .unwrap();
    assert_rows_eq(
        &u16.query("SELECT octet_length('a'), hex('A')").unwrap().rows,
        &[vec![i(2), t("4100")]],
    );
    assert_rows_eq(
        &u16.query("SELECT iif(X'3100', 'yes', 'no'), (SELECT 'ok' WHERE X'3100')")
            .unwrap()
            .rows,
        &[vec![t("yes"), t("ok")]],
    );
    // In UTF-16 BINARY, U+FF00 (0xFF00) > U+10000 (surrogate 0xD800 0xDC00), opposite of UTF-8!
    assert_rows_eq(
        &u16.query("SELECT unicode(max(s)) FROM u16t").unwrap().rows,
        &[vec![i(65280)]],
    );

    // S3 #6: Collation propagates out of scalar function args!
    assert_rows_eq(
        &conn
            .query("SELECT coalesce('a' COLLATE NOCASE, 'x') = 'A'")
            .unwrap()
            .rows,
        &[vec![i(1)]],
    );

    // S3 #7..#11: EXPLAIN QUERY PLAN fixes (UPDATE/DELETE CTE ignore, RIGHT JOIN ON, window+ORDER BY, LHS join collation, RAISE outside trigger)!
    conn.execute(
        "CREATE TABLE eq4_a(id INT PRIMARY KEY, x INT);
         CREATE TABLE eq4_b(id INT PRIMARY KEY, x INT);
         CREATE INDEX eq4_bx ON eq4_b(x);
         CREATE TABLE eq4_j1(a TEXT COLLATE NOCASE);
         CREATE TABLE eq4_j2(a TEXT);
         CREATE INDEX eq4_j2a ON eq4_j2(a);",
    )
    .unwrap();
    let eqp_cte_del = conn
        .execute("EXPLAIN QUERY PLAN WITH eq4_a AS (SELECT 1 AS id) DELETE FROM eq4_a WHERE id = 1")
        .unwrap();
    assert!(eqp_cte_del
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("SEARCH eq4_a USING")));
    assert!(conn
        .execute("EXPLAIN QUERY PLAN WITH ghost_cte AS (SELECT 1 AS id) DELETE FROM ghost_cte")
        .is_err());
    assert!(conn
        .execute("EXPLAIN QUERY PLAN WITH ghost_cte AS (SELECT 1 AS id) UPDATE ghost_cte SET id = 2")
        .is_err());
    let eqp_rj = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM eq4_a RIGHT JOIN eq4_b ON eq4_a.id = eq4_b.id AND eq4_b.x = 5")
        .unwrap();
    assert!(!eqp_rj
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USING INDEX eq4_bx")));
    let eqp_win_ord = conn
        .execute("EXPLAIN QUERY PLAN SELECT id, row_number() OVER (ORDER BY x) FROM eq4_a ORDER BY id")
        .unwrap();
    assert!(eqp_win_ord
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("TEMP B-TREE FOR ORDER BY")));
    let eqp_jcoll = conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM eq4_j1 JOIN eq4_j2 ON eq4_j1.a = eq4_j2.a")
        .unwrap();
    assert!(!eqp_jcoll
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap().contains("USING INDEX eq4_j2a")));
    assert!(conn
        .execute("EXPLAIN QUERY PLAN SELECT RAISE(ABORT, 'x')")
        .is_err());

    // S3 #12: ORDER BY validates columns before LIMIT 0, skips eval on LIMIT 0, still evaluates on OFFSET > rows!
    assert!(conn
        .query("SELECT 1 FROM (SELECT 'not json' AS a) ORDER BY json(a) LIMIT 0")
        .unwrap()
        .rows
        .is_empty());
    assert!(conn
        .query("SELECT 1 FROM (SELECT 1 AS a) ORDER BY bogus LIMIT 0")
        .is_err());
    assert!(conn
        .query("SELECT 1 FROM (SELECT 'not json' AS a) ORDER BY json(a) LIMIT 1 OFFSET 10")
        .is_err());

    // S3 #13: Recursive CTE ORDER BY resolves base/recursive column names and rejects expressions!
    assert_rows_eq(
        &conn
            .query(
                "WITH RECURSIVE r(a) AS (
                    SELECT 1
                    UNION ALL
                    SELECT a + 1 AS b FROM r WHERE a < 3
                    ORDER BY b DESC
                 ) SELECT a FROM r",
            )
            .unwrap()
            .rows,
        &[vec![i(1)], vec![i(2)], vec![i(3)]],
    );
    assert!(conn
        .query(
            "WITH RECURSIVE r(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM r WHERE a < 3 ORDER BY a + 1) SELECT a FROM r",
        )
        .is_err());

    // S3 #14: Deep 100-term UNION ALL executes and explains cleanly!
    let mut big_union = String::from("SELECT 1");
    for k in 2..=100 {
        big_union.push_str(&format!(" UNION ALL SELECT {k}"));
    }
    assert_eq!(conn.query(&big_union).unwrap().rows.len(), 100);
    assert!(conn
        .execute(&format!("EXPLAIN QUERY PLAN {big_union}"))
        .is_ok());

    // S3 #15: Row-value vector keeps runtime column affinity of subquery element!
    conn.execute("CREATE TABLE rv_t(a INT); INSERT INTO rv_t VALUES (1);")
        .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT ((SELECT a FROM rv_t), 1) = ('1', 1)")
            .unwrap()
            .rows,
        &[vec![i(1)]],
    );

    // S3 #16: IN list validates explicit collation even when LHS is NULL!
    assert!(conn.query("SELECT NULL IN ('a' COLLATE bogus)").is_err());

    // S3 #17: JSON5 decimal escapes rejected, json('-0') preserved, raw non-UTF-8 blob bytes preserved!
    assert_rows_eq(
        &conn
            .query("SELECT json_valid('''\\1''', 2), json('-0'), json_type('-0'), hex(json(X'22FF22')), json_type(X'22FF22')")
            .unwrap()
            .rows,
        &[vec![i(0), t("-0"), t("integer"), t("22FF22"), t("text")]],
    );

    // S3 #18: printf('') and printf('%k', 1) return NULL, while printf('%s', '') returns ''!
    assert_rows_eq(
        &conn
            .query("SELECT printf(''), printf('%k', 1), printf('x%k', 1), printf('%s', ''), printf('%s', NULL)")
            .unwrap()
            .rows,
        &[vec![
            SqlValue::Null,
            SqlValue::Null,
            t("x"),
            t(""),
            t(""),
        ]],
    );
}

#[test]
fn audit4_storage_wal_fingerprint_overflow_and_fast_scans() {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use xdb_core::vfs::{FileKind, LockLevel, MemoryVfs, MemoryVfsOptions, OpenFlags, Vfs, VfsFile};
    use xdb_core::IoResult;

    // S4 #1 (P0): Full-content CompanionFingerprint detects mid-file WAL frame rewrites
    // even when file length + first 32 bytes + last 32 bytes are 100% identical!
    let mut base = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    base.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'orig');")
        .unwrap();
    let ps = base.page_size() as usize;
    let mut main_bytes = base.to_bytes();
    main_bytes[18] = 2;
    main_bytes[19] = 2; // WAL mode

    base.execute("UPDATE t SET v = 'wal_v1' WHERE id = 1").unwrap();
    let p2_v1 = base.to_bytes()[ps..2 * ps].to_vec();
    base.execute("UPDATE t SET v = 'wal_v2' WHERE id = 1").unwrap();
    let p2_v2 = base.to_bytes()[ps..2 * ps].to_vec();

    let wal_ck = |bytes: &[u8], s: &mut [u32; 2]| {
        for chunk in bytes.chunks_exact(8) {
            let w0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let w1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            s[0] = s[0].wrapping_add(w0).wrapping_add(s[1]);
            s[1] = s[1].wrapping_add(w1).wrapping_add(s[0]);
        }
    };
    let build_wal = |p2: &[u8]| -> Vec<u8> {
        let salt1 = 0x1111_2222u32;
        let salt2 = 0x3333_4444u32;
        let mut wal = Vec::with_capacity(32 + 2 * (24 + ps));
        let mut hdr = [0u8; 32];
        hdr[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
        hdr[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
        hdr[8..12].copy_from_slice(&(ps as u32).to_be_bytes());
        hdr[12..16].copy_from_slice(&0u32.to_be_bytes());
        hdr[16..20].copy_from_slice(&salt1.to_be_bytes());
        hdr[20..24].copy_from_slice(&salt2.to_be_bytes());
        let mut ck = [0u32; 2];
        wal_ck(&hdr[0..24], &mut ck);
        hdr[24..28].copy_from_slice(&ck[0].to_be_bytes());
        hdr[28..32].copy_from_slice(&ck[1].to_be_bytes());
        wal.extend_from_slice(&hdr);

        // Frame 0 (valid committed frame for page 2, db_size = 2)
        let mut fh = [0u8; 24];
        fh[0..4].copy_from_slice(&2u32.to_be_bytes());
        fh[4..8].copy_from_slice(&2u32.to_be_bytes());
        fh[8..12].copy_from_slice(&salt1.to_be_bytes());
        fh[12..16].copy_from_slice(&salt2.to_be_bytes());
        wal_ck(&fh[0..8], &mut ck);
        wal_ck(p2, &mut ck);
        fh[16..20].copy_from_slice(&ck[0].to_be_bytes());
        fh[20..24].copy_from_slice(&ck[1].to_be_bytes());
        wal.extend_from_slice(&fh);
        wal.extend_from_slice(p2);

        // Frame 1 (stale trailing frame with mismatched salt — identical across v1 and v2!)
        let mut stale = vec![0xA5u8; 24 + ps];
        stale[8..12].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        wal.extend_from_slice(&stale);
        wal
    };

    let wal_v1 = build_wal(&p2_v1);
    let wal_v2 = build_wal(&p2_v2);
    assert_eq!(wal_v1.len(), wal_v2.len());
    assert_eq!(&wal_v1[..32], &wal_v2[..32]);
    assert_eq!(&wal_v1[wal_v1.len() - 32..], &wal_v2[wal_v2.len() - 32..]);

    let inner = Arc::new(MemoryVfs::new(MemoryVfsOptions::default()));
    {
        let mut f = inner
            .open("wal_fp.db", OpenFlags::create(FileKind::MainDb))
            .unwrap();
        f.write(&main_bytes, 0).unwrap();
        f.close().unwrap();
        let mut wf = inner
            .open("wal_fp.db-wal", OpenFlags::create(FileKind::Wal))
            .unwrap();
        wf.write(&wal_v1, 0).unwrap();
        wf.close().unwrap();
    }

    #[derive(Debug)]
    struct MutatingFile {
        inner_file: Box<dyn VfsFile>,
        vfs: Arc<MemoryVfs>,
        mutated: Arc<AtomicBool>,
        wal_v2: Arc<Vec<u8>>,
        is_main: bool,
    }
    impl VfsFile for MutatingFile {
        fn path(&self) -> &str {
            self.inner_file.path()
        }
        fn is_readonly(&self) -> bool {
            self.inner_file.is_readonly()
        }
        fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<usize> {
            self.inner_file.read(buf, offset)
        }
        fn write(&mut self, data: &[u8], offset: u64) -> IoResult<()> {
            self.inner_file.write(data, offset)
        }
        fn truncate(&mut self, size: u64) -> IoResult<()> {
            self.inner_file.truncate(size)
        }
        fn sync(&mut self, data_only: bool) -> IoResult<()> {
            self.inner_file.sync(data_only)
        }
        fn size(&self) -> IoResult<u64> {
            self.inner_file.size()
        }
        fn lock(&mut self, level: LockLevel) -> IoResult<bool> {
            let acquired = self.inner_file.lock(level)?;
            if acquired
                && self.is_main
                && level == LockLevel::Exclusive
                && !self.mutated.swap(true, AtomicOrdering::SeqCst)
            {
                let mut wf = self
                    .vfs
                    .open("wal_fp.db-wal", OpenFlags::create(FileKind::Wal))?;
                wf.write(&self.wal_v2, 0)?;
                wf.close()?;
            }
            Ok(acquired)
        }
        fn unlock(&mut self, level: LockLevel) -> IoResult<()> {
            self.inner_file.unlock(level)
        }
        fn check_reserved_lock(&self) -> IoResult<bool> {
            self.inner_file.check_reserved_lock()
        }
        fn lock_level(&self) -> LockLevel {
            self.inner_file.lock_level()
        }
        fn close(&mut self) -> IoResult<()> {
            self.inner_file.close()
        }
    }
    #[derive(Debug)]
    struct MutatingVfs {
        inner: Arc<MemoryVfs>,
        mutated: Arc<AtomicBool>,
        wal_v2: Arc<Vec<u8>>,
    }
    impl Vfs for MutatingVfs {
        fn open(&self, path: &str, flags: OpenFlags) -> IoResult<Box<dyn VfsFile>> {
            let f = self.inner.open(path, flags)?;
            Ok(Box::new(MutatingFile {
                inner_file: f,
                vfs: Arc::clone(&self.inner),
                mutated: Arc::clone(&self.mutated),
                wal_v2: Arc::clone(&self.wal_v2),
                is_main: path == "wal_fp.db",
            }))
        }
        fn delete(&self, path: &str, sync_dir: bool) -> IoResult<()> {
            self.inner.delete(path, sync_dir)
        }
        fn exists(&self, path: &str) -> IoResult<bool> {
            self.inner.exists(path)
        }
    }

    let mutated = Arc::new(AtomicBool::new(false));
    let mvfs: Arc<dyn Vfs> = Arc::new(MutatingVfs {
        inner: Arc::clone(&inner),
        mutated: Arc::clone(&mutated),
        wal_v2: Arc::new(wal_v2),
    });
    let c_wal = SqliteConnection::open(mvfs, "wal_fp.db", ConnectionOptions::default()).unwrap();
    assert!(mutated.load(AtomicOrdering::SeqCst));
    assert_rows_eq(
        &c_wal.query("SELECT v FROM t WHERE id = 1").unwrap().rows,
        &[vec![t("wal_v2")]],
    );

    // S4 #2: Direct GROUP BY fast path applies real_read_back to integer DEFAULT on REAL column!
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE gb_real(g INT);
         INSERT INTO gb_real VALUES (1), (1), (2);
         ALTER TABLE gb_real ADD COLUMN r REAL DEFAULT 1;",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT g, typeof(min(r)), typeof(max(r)), min(r) FROM gb_real GROUP BY g ORDER BY g")
            .unwrap()
            .rows,
        &[
            vec![i(1), t("real"), t("real"), SqlValue::Real(1.0)],
            vec![i(2), t("real"), t("real"), SqlValue::Real(1.0)],
        ],
    );

    // S4 #3: Corrupt overflow pointer pointing to page 1 (DB header page) is rejected on read AND delete!
    let mut ov = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    ov.execute("CREATE TABLE ov_t(id INTEGER PRIMARY KEY, big BLOB);")
        .unwrap();
    let big_hex = "AB".repeat(5000);
    ov.execute(&format!("INSERT INTO ov_t VALUES (1, X'{big_hex}');"))
        .unwrap();
    let mut bytes = ov.to_bytes();
    let ps = ov.page_size() as usize;
    // Page 2 is ov_t leaf; its last 4 bytes of cell 0 store the first overflow page number (page 3).
    // Patch the overflow page number (0x00000003) in page 2's cell area to page 1 (0x00000001)!
    let p2 = &mut bytes[ps..2 * ps];
    let cell_off = u16::from_be_bytes([p2[8], p2[9]]) as usize;
    let mut patched = false;
    for idx in (cell_off..ps.saturating_sub(3)).rev() {
        if p2[idx..idx + 4] == 3u32.to_be_bytes() {
            p2[idx..idx + 4].copy_from_slice(&1u32.to_be_bytes());
            patched = true;
            break;
        }
    }
    assert!(patched, "failed to locate overflow page pointer in page 2");
    let mut ov_bad = SqliteConnection::open_from_bytes(
        &bytes,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    assert!(ov_bad.query("SELECT length(big) FROM ov_t").is_err());
    assert!(ov_bad.execute("DELETE FROM ov_t WHERE id = 1").is_err());

    // S4 #4: Rowid range scan direct fast path cleanly rejects corrupt truncated cell bounds!
    let mut rs = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    rs.execute("CREATE TABLE rs_t(id INTEGER PRIMARY KEY, x INT);")
        .unwrap();
    for k in 1..=10 {
        rs.execute(&format!("INSERT INTO rs_t VALUES ({k}, {k});"))
            .unwrap();
    }
    assert_rows_eq(
        &rs.query("SELECT id, x FROM rs_t WHERE id BETWEEN 3 AND 7 ORDER BY id LIMIT 2 OFFSET 1")
            .unwrap()
            .rows,
        &[vec![i(4), i(4)], vec![i(5), i(5)]],
    );
    let mut rs_bytes = rs.to_bytes();
    let rps = rs.page_size() as usize;
    // Corrupt cell pointer 0 on page 2 to point 1 byte before page end (truncated cell).
    let rp2 = &mut rs_bytes[rps..2 * rps];
    let bad_off = (rps as u16) - 1;
    rp2[8..10].copy_from_slice(&bad_off.to_be_bytes());
    let rs_bad = SqliteConnection::open_from_bytes(
        &rs_bytes,
        None,
        None,
        ConnectionOptions::default(),
    )
    .unwrap();
    assert!(rs_bad
        .query("SELECT id, x FROM rs_t WHERE id BETWEEN 1 AND 5 LIMIT 2 OFFSET 1")
        .is_err());
}

#[test]
fn audit5_batch1_storage_pager_btree_fast_scans_and_access() {
    use xdb_core::value::CollationRegistry;
    use xdb_core::TextEncoding;
    use xdb_sqlite::{
        key_info_for_index, open_database_from_bytes, resolve_collation, WritePager,
    };

    // 1. P0 cow_overlay_with_flushed_seq: multi-table dirty sqlite_sequence overlay stays consistent!
    {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute(
            "CREATE TABLE seq_a(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
             CREATE TABLE seq_b(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);",
        )
        .unwrap();
        // Insert across multiple AUTOINCREMENT tables so multiple dirty sequence rows must flush cleanly.
        for k in 1..=25 {
            conn.execute(&format!(
                "INSERT INTO seq_a(v) VALUES ('a{k}');
                 INSERT INTO seq_b(v) VALUES ('b{k}');"
            ))
            .unwrap();
        }
        // Reading sqlite_sequence and exporting via to_bytes() must both see exact sequences (25, 25).
        let seq_rows = conn
            .query("SELECT name, seq FROM sqlite_sequence ORDER BY name")
            .unwrap()
            .rows;
        assert_rows_eq(&seq_rows, &[vec![t("seq_a"), i(25)], vec![t("seq_b"), i(25)]]);
        let exported = conn.to_bytes();
        let ro = open_database_from_bytes(&exported, None, None, OpenOptions::default()).unwrap();
        let ro_seq = ro
            .query("SELECT name, seq FROM sqlite_sequence ORDER BY name")
            .unwrap()
            .rows;
        assert_rows_eq(&ro_seq, &[vec![t("seq_a"), i(25)], vec![t("seq_b"), i(25)]]);
        assert_rows_eq(
            &conn.execute("PRAGMA integrity_check").unwrap().rows,
            &[vec![t("ok")]],
        );
    }

    // 2. UTF-16le & UTF-16be sum/total/avg on BLOBs across direct fast-scan, GROUP BY, SumAcc, and windows!
    for (enc_pragma, hex_10, hex_25) in [
        ("UTF-16le", "31003000", "32003500"),
        ("UTF-16be", "00310030", "00320035"),
    ] {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute(&format!(
            "PRAGMA encoding = '{enc_pragma}';
             CREATE TABLE u16b(id INTEGER PRIMARY KEY, g INT, b BLOB);
             INSERT INTO u16b VALUES (1, 1, X'{hex_10}'), (2, 1, X'{hex_25}');"
        ))
        .unwrap();
        assert_rows_eq(
            &conn
                .query("SELECT sum(b), total(b), avg(b) FROM u16b")
                .unwrap()
                .rows,
            &[vec![SqlValue::Real(35.0), SqlValue::Real(35.0), SqlValue::Real(17.5)]],
        );
        assert_rows_eq(
            &conn
                .query("SELECT g, sum(b), total(b), avg(b) FROM u16b GROUP BY g")
                .unwrap()
                .rows,
            &[vec![
                i(1),
                SqlValue::Real(35.0),
                SqlValue::Real(35.0),
                SqlValue::Real(17.5),
            ]],
        );
        assert_rows_eq(
            &conn
                .query(
                    "SELECT id, sum(b) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)
                     FROM u16b ORDER BY id",
                )
                .unwrap()
                .rows,
            &[
                vec![i(1), SqlValue::Real(10.0)],
                vec![i(2), SqlValue::Real(35.0)],
            ],
        );
    }

    // 3. resolve_collation checks custom registry BEFORE BuiltinCollation!
    {
        let mut reg = CollationRegistry::new(TextEncoding::Utf8);
        reg.register("BINARY", Arc::new(|a, b| b.cmp(a)));
        let coll = resolve_collation("BINARY", TextEncoding::Utf8, Some(&reg))
            .unwrap()
            .expect("custom BINARY override must return Some(Custom)");
        assert_eq!(
            coll.compare_utf8(b"a", b"b"),
            std::cmp::Ordering::Greater,
            "custom BINARY override must take precedence over built-in BINARY"
        );
    }

    // 4. WITHOUT ROWID non-PK custom collation: key_info_for_index / VACUUM / REINDEX / ANALYZE / integrity_check!
    {
        let mut base = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        base.execute(
            "CREATE TABLE wr_c(k INT PRIMARY KEY, v TEXT COLLATE NOCASE) WITHOUT ROWID;
             INSERT INTO wr_c VALUES (1, 'Alpha'), (2, 'beta');",
        )
        .unwrap();
        let mut bytes = base.to_bytes();
        // Patch the 6-byte collation name "NOCASE" in sqlite_schema to unregistered "MYCOLL".
        let needle = b"NOCASE";
        let pos = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("NOCASE in sqlite_schema");
        bytes[pos..pos + 6].copy_from_slice(b"MYCOLL");
        let ro = open_database_from_bytes(&bytes, None, None, OpenOptions::default()).unwrap();
        let wr_tbl = ro.schema().table("wr_c").unwrap();
        let pk_idx = wr_tbl
            .indexes
            .iter()
            .find(|ix| ix.without_rowid_primary_key)
            .unwrap();
        let ki = key_info_for_index(pk_idx, TextEncoding::Utf8, None).unwrap();
        assert_eq!(ki.n_key_field, 1);
        assert_eq!(ki.n_all_field, 2);

        let mut rw =
            SqliteConnection::open_from_bytes(&bytes, None, None, ConnectionOptions::default())
                .unwrap();
        assert_rows_eq(
            &rw.execute("PRAGMA integrity_check").unwrap().rows,
            &[vec![t("ok")]],
        );
        rw.execute("REINDEX; ANALYZE; VACUUM;").unwrap();
        assert_rows_eq(
            &rw.query("SELECT k, v FROM wr_c ORDER BY k").unwrap().rows,
            &[vec![i(1), t("Alpha")], vec![i(2), t("beta")]],
        );
    }

    // 5. WritePager::db_info on an empty (page_count == 0) UTF-16 database populates usable_size & encoding!
    {
        let mut wp = WritePager::new_in_memory(4096, None).unwrap();
        wp.set_encoding(TextEncoding::Utf16Le);
        assert_eq!(wp.page_count(), 0);
        let info = wp.db_info();
        assert_eq!(info.usable_size, 4096);
        assert_eq!(info.text_encoding, TextEncoding::Utf16Le);
        assert!(info.limits.max_leaf > 0);
    }

    // 6. usable_size cell bounds enforcement rejects a cell extending into reserved_bytes!
    {
        let mut c0 = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        c0.execute("CREATE TABLE ub(id INTEGER PRIMARY KEY, s TEXT); INSERT INTO ub VALUES (1, 'hi');")
            .unwrap();
        let mut corrupt_img = c0.to_bytes();
        // Page 1 already placed cell 0 at the end of the 4096-byte page; setting reserved_bytes = 32
        // makes cell 0 run past usable_size (4064), which MUST be rejected cleanly as Corrupt!
        corrupt_img[20] = 32;
        let bad = open_database_from_bytes(&corrupt_img, None, None, OpenOptions::default());
        assert!(bad.is_err(), "cell running into reserved_bytes must be rejected");
    }

    // 7. RowCursor::visit_sparse_range always yields Some(rowid) on rowid tables!
    {
        let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        conn.execute(
            "CREATE TABLE sr(a INT, b TEXT);
             INSERT INTO sr(rowid, a, b) VALUES (10, 1, 'x'), (20, 2, 'y'), (30, 3, 'z');",
        )
        .unwrap();
        assert_rows_eq(
            &conn
                .query("SELECT rowid, b FROM sr WHERE rowid BETWEEN 15 AND 30 ORDER BY rowid")
                .unwrap()
                .rows,
            &[vec![i(20), t("y")], vec![i(30), t("z")]],
        );
    }
}

#[test]
fn audit5_batch2_sql_engine_eqp_and_builtin_functions() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. JSON5 `\0` followed by ASCII digit rejected (`json_valid`), bare `\0` accepted;
    //    `json()` canonicalization respects meter on large strings; `->`/`->>` on non-UTF-8 blob.
    assert_rows_eq(
        &conn
            .query("SELECT json_valid('\"\\01\"', 2), json_valid('\"\\0\"', 2)")
            .unwrap()
            .rows,
        &[vec![i(0), i(1)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT hex(CAST(json(X'228022') AS BLOB)), X'7B2280223A317D' -> 'a'")
            .unwrap()
            .rows,
        &[vec![t("228022"), SqlValue::Null]],
    );
    {
        let metered = SqliteConnection::open_in_memory(ConnectionOptions {
            meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
                max_memory_bytes: Some(32 * 1024),
                ..ResourceLimits::default()
            }))),
            ..ConnectionOptions::default()
        })
        .unwrap();
        assert!(metered
            .query("SELECT json(quote(replace(hex(zeroblob(10000)), '0', '\"')))")
            .is_err());
        assert!(metered
            .query("SELECT (X'7B2261223A22' || zeroblob(50000) || X'80227D') -> 'a'")
            .is_err());
    }

    // 2. lag()/lead() offset parsing with prefix strings & BLOBs matching C SQLite!
    conn.execute(
        "CREATE TABLE win_t(id INT, v TEXT);
         INSERT INTO win_t VALUES (1, 'a'), (2, 'b'), (3, 'c');",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query(
                "SELECT id,
                        lag(v, '1abc', 'none') OVER (ORDER BY id),
                        lead(v, '1abc', 'none') OVER (ORDER BY id),
                        lag(v, 'abc', 'none') OVER (ORDER BY id),
                        lag(v, '-1abc', 'none') OVER (ORDER BY id)
                 FROM win_t ORDER BY id",
            )
            .unwrap()
            .rows,
        &[
            vec![i(1), t("none"), t("b"), t("a"), t("b")],
            vec![i(2), t("a"), t("c"), t("b"), t("c")],
            vec![i(3), t("b"), t("none"), t("c"), t("none")],
        ],
    );
    assert_rows_eq(
        &conn
            .query(
                "SELECT id,
                        lag(v, '1.5abc', 'none') OVER (ORDER BY id),
                        lag(v, X'312E35', 'none') OVER (ORDER BY id),
                        lag(v, '2.0abc', 'none') OVER (ORDER BY id)
                 FROM win_t ORDER BY id",
            )
            .unwrap()
            .rows,
        &[
            vec![i(1), t("none"), t("none"), t("none")],
            vec![i(2), t("none"), t("none"), t("none")],
            vec![i(3), t("none"), t("none"), t("a")],
        ],
    );

    // 3. instr() counts bytes on BLOBs and UTF-8 lead bytes on invalid UTF-8 TEXT!
    assert_rows_eq(
        &conn
            .query("SELECT instr(X'618062', X'62'), instr(CAST(X'618062' AS TEXT), 'b')")
            .unwrap()
            .rows,
        &[vec![i(3), i(2)]],
    );

    // 4. IN list row-value misuse rejected BEFORE NULL early-return or short-circuit!
    assert!(conn
        .query("SELECT NULL IN ((1, 2))")
        .unwrap_err()
        .to_string()
        .contains("row value misused"));
    assert!(conn
        .query("SELECT 1 IN (1, (1, 2))")
        .unwrap_err()
        .to_string()
        .contains("row value misused"));

    // 5. Aggregate inside InTable args rejected!
    assert!(conn
        .query("SELECT 1 IN json_each(count(*))")
        .is_err());

    // 6. Partial index EQP: self-join alias scoping & asymmetric collation flip!
    conn.execute(
        "CREATE TABLE pi(a TEXT COLLATE NOCASE, b TEXT COLLATE BINARY);
         CREATE INDEX pi_idx ON pi(a) WHERE a = b;",
    )
    .unwrap();
    // Self-join: `p1` cannot use `pi_idx` when only `p2` satisfies `p2.a = p2.b`!
    assert!(conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM pi p1 INDEXED BY pi_idx JOIN pi p2 ON p1.a = p2.a WHERE p2.a = p2.b")
        .unwrap_err()
        .to_string()
        .contains("cannot use index"));
    // Reversed operands `b = a` has BINARY collation (left operand wins), NOT NOCASE (`a = b`), so partial index must NOT match!
    assert!(conn
        .execute("EXPLAIN QUERY PLAN SELECT * FROM pi INDEXED BY pi_idx WHERE b = a")
        .unwrap_err()
        .to_string()
        .contains("cannot use index"));
    // Matching order `a = b` DOES match!
    conn.execute("EXPLAIN QUERY PLAN SELECT * FROM pi INDEXED BY pi_idx WHERE a = b")
        .unwrap();

    // 7. Duplicate named WINDOW definitions & chained base reference!
    assert_rows_eq(
        &conn
            .query(
                "SELECT id, sum(id) OVER w FROM win_t
                 WINDOW w AS (ORDER BY id), w AS (ORDER BY id DESC)
                 ORDER BY id",
            )
            .unwrap()
            .rows,
        &[vec![i(1), i(6)], vec![i(2), i(5)], vec![i(3), i(3)]],
    );

    // 8. Compound ORDER BY: valid branch table.col accepted, invalid qualifiers rejected!
    conn.execute(
        "CREATE TABLE co1(a INT, b INT); INSERT INTO co1 VALUES (1, 10), (3, 30);
         CREATE TABLE co2(a INT, c INT); INSERT INTO co2 VALUES (2, 20), (4, 40);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT a FROM co1 UNION ALL SELECT a FROM co2 ORDER BY co2.a DESC")
            .unwrap()
            .rows,
        &[vec![i(4)], vec![i(3)], vec![i(2)], vec![i(1)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT a AS x FROM co1 UNION ALL SELECT a AS x FROM co2 ORDER BY co1.a DESC")
            .unwrap()
            .rows,
        &[vec![i(4)], vec![i(3)], vec![i(2)], vec![i(1)]],
    );
    assert_rows_eq(
        &conn
            .query("SELECT a FROM co1 AS u UNION ALL SELECT a FROM co2 ORDER BY u.a DESC")
            .unwrap()
            .rows,
        &[vec![i(4)], vec![i(3)], vec![i(2)], vec![i(1)]],
    );
    for bad_q in [
        "SELECT a AS x FROM co1 UNION ALL SELECT a AS x FROM co2 ORDER BY co1.x DESC",
        "SELECT a FROM co1 AS u UNION ALL SELECT a FROM co2 ORDER BY co1.a DESC",
        "SELECT 1 AS a UNION ALL SELECT 2 ORDER BY co1.a",
        "SELECT a FROM co1 UNION ALL SELECT a FROM co1 ORDER BY co1.b DESC",
    ] {
        assert!(
            conn.query(bad_q)
                .unwrap_err()
                .to_string()
                .contains("ORDER BY term does not match"),
            "expected ORDER BY error for: {bad_q}"
        );
    }

    // 9. WITH RECURSIVE: >1 ref, aggregate/GROUP BY, window function, and LIMIT 1 arity mismatch rejected!
    for bad_cte in [
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT a.x + b.x FROM r a JOIN r b) SELECT * FROM r LIMIT 5",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT sum(x) FROM r) SELECT * FROM r LIMIT 5",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x FROM r GROUP BY x) SELECT * FROM r LIMIT 5",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT row_number() OVER () FROM r) SELECT * FROM r LIMIT 5",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x, x + 1 FROM r LIMIT 1) SELECT * FROM r",
    ] {
        assert!(conn.query(bad_cte).is_err(), "expected error for: {bad_cte}");
    }

    // 10. GROUP BY aliased window function rejected!
    assert!(conn
        .query("SELECT row_number() OVER () AS rn FROM win_t GROUP BY rn")
        .unwrap_err()
        .to_string()
        .contains("misuse of aliased window function"));

    // 11. Correlated TVF on empty left table still validates nonexistent columns!
    conn.execute("CREATE TABLE empty_l(a INT);").unwrap();
    assert!(conn
        .query("SELECT * FROM empty_l JOIN json_each(empty_l.no_such_col)")
        .is_err());

    // 12. Aliased parenthesized FULL OUTER JOIN USING drops using_hidden so `g.id` resolves!
    conn.execute(
        "CREATE TABLE fj_a(id INT, va TEXT); INSERT INTO fj_a VALUES (1, 'a1');
         CREATE TABLE fj_b(id INT, vb TEXT); INSERT INTO fj_b VALUES (1, 'b1'), (2, 'b2');",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT g.id, g.va, g.vb FROM (fj_a FULL OUTER JOIN fj_b USING (id)) AS g ORDER BY g.id")
            .unwrap()
            .rows,
        &[
            vec![i(1), t("a1"), t("b1")],
            vec![i(2), SqlValue::Null, t("b2")],
        ],
    );

    // 13. Nested window function inside frame bound rejected!
    assert!(conn
        .query("SELECT sum(id) OVER (ORDER BY id ROWS BETWEEN row_number() OVER () PRECEDING AND CURRENT ROW) FROM win_t")
        .is_err());

    // 14. EXPLAIN QUERY PLAN for UPDATE ... FROM includes target table & rejects RAISE() outside trigger!
    conn.execute("CREATE TABLE eq_u(id INT, v INT); CREATE TABLE eq_s(id INT, v INT);")
        .unwrap();
    let eqp_u = conn
        .execute("EXPLAIN QUERY PLAN UPDATE eq_u SET v = eq_s.v FROM eq_s WHERE eq_u.id = eq_s.id")
        .unwrap();
    assert!(eqp_u
        .rows
        .iter()
        .any(|r| r[3].as_str().unwrap_or("").contains("eq_u")));
    assert!(conn
        .execute("EXPLAIN QUERY PLAN UPDATE eq_u SET v = 1 FROM (SELECT RAISE(ABORT, 'x'))")
        .unwrap_err()
        .to_string()
        .contains("trigger-program"));
}

#[test]
fn audit5_batch3_fk_ddl_maintenance_pragmas_and_catalog() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. P0 token-gluing DDL rewrite (CREATE TABLE"t"("a"INT), REFERENCES"p"(id), SELECT"a"FROM"t")!
    conn.execute(
        r#"CREATE TABLE"p"("id"INT PRIMARY KEY);
           CREATE TABLE"t"("a"INT REFERENCES"p"("id"));
           CREATE VIEW"v"AS SELECT"a"FROM"t";
           CREATE TRIGGER"tr"AFTER INSERT ON"t"BEGIN SELECT"a"FROM"t";END;
           INSERT INTO"p"VALUES(1);
           INSERT INTO"t"VALUES(1);"#,
    )
    .unwrap();
    conn.execute(r#"ALTER TABLE"p"RENAME TO parent_tbl;"#).unwrap();
    conn.execute(r#"ALTER TABLE"t"RENAME TO child_tbl;"#).unwrap();
    conn.execute(r#"ALTER TABLE child_tbl RENAME COLUMN"a"TO col_a;"#)
        .unwrap();
    assert_rows_eq(
        &conn.query("SELECT col_a FROM v").unwrap().rows,
        &[vec![i(1)]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 2. P0 VACUUM skips unindexed VIRTUAL columns (even if evaluating them would error)!
    conn.execute(
        "CREATE TABLE vcol_t(id INTEGER PRIMARY KEY, raw TEXT);
         INSERT INTO vcol_t(id, raw) VALUES (1, 'not_json');
         ALTER TABLE vcol_t ADD COLUMN bad TEXT GENERATED ALWAYS AS (json(raw)) VIRTUAL;",
    )
    .unwrap();
    conn.execute("VACUUM;").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, raw FROM vcol_t").unwrap().rows,
        &[vec![i(1), t("not_json")]],
    );
    conn.execute("DROP TABLE vcol_t;").unwrap();

    // 3. ALTER TABLE RENAME TO rejects rename when a schema view or trigger is broken!
    conn.execute(
        "CREATE TABLE rt_dead(x INT);
         CREATE VIEW rt_broken_v AS SELECT x FROM rt_dead;
         DROP TABLE rt_dead;
         CREATE TABLE rt_ok(a INT);",
    )
    .unwrap();
    assert!(conn
        .execute("ALTER TABLE rt_ok RENAME TO rt_ok2")
        .unwrap_err()
        .to_string()
        .contains("rt_broken_v"));
    conn.execute("DROP VIEW rt_broken_v;").unwrap();

    // 4. ALTER TABLE RENAME COLUMN succeeds cleanly with unrelated runtime-failing views (schema-only validation)!
    conn.execute(
        "CREATE VIEW v_bad_json AS VALUES (json('invalid'));
         CREATE VIEW v_bad_je AS SELECT value FROM json_each('invalid');
         CREATE TABLE rc_t(old_c INT);
         ALTER TABLE rc_t RENAME COLUMN old_c TO new_c;",
    )
    .unwrap();
    conn.execute("DROP VIEW v_bad_json; DROP VIEW v_bad_je;").unwrap();

    // 5. ALTER TABLE DROP COLUMN rejects dropping column referenced in view GROUP BY/ORDER BY alias or NATURAL JOIN!
    conn.execute(
        "CREATE TABLE dc_a(id INT, k INT, x INT);
         CREATE TABLE dc_b(id INT, y INT);
         CREATE VIEW dc_v1 AS SELECT id, k AS kk FROM dc_a GROUP BY kk;
         CREATE VIEW dc_v2 AS SELECT * FROM dc_a NATURAL JOIN dc_b;",
    )
    .unwrap();
    assert!(conn.execute("ALTER TABLE dc_a DROP COLUMN k").is_err());
    assert!(conn.execute("ALTER TABLE dc_a DROP COLUMN id").is_err());
    conn.execute("DROP VIEW dc_v1; DROP VIEW dc_v2;").unwrap();

    // 6. FK reverse declaration order (CASCADE + RESTRICT vs RESTRICT + CASCADE)!
    conn.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE fkp(id INT PRIMARY KEY);
         CREATE TABLE fkc1(pid INT REFERENCES fkp(id) ON DELETE CASCADE,
                           pid2 INT REFERENCES fkp(id) ON DELETE RESTRICT);
         INSERT INTO fkp VALUES (1);
         INSERT INTO fkc1 VALUES (1, 1);",
    )
    .unwrap();
    // Second FK (RESTRICT) runs first in reverse order -> blocks delete!
    assert!(conn
        .execute("DELETE FROM fkp WHERE id = 1")
        .unwrap_err()
        .to_string()
        .contains("FOREIGN KEY"));
    // Reverse order: CASCADE declared AFTER SET NULL -> CASCADE deletes child row first, so SET NULL sees no row!
    conn.execute(
        "CREATE TABLE fkp2(id INT PRIMARY KEY);
         CREATE TABLE fkc2(pid INT REFERENCES fkp2(id) ON DELETE RESTRICT,
                           pid2 INT REFERENCES fkp2(id) ON DELETE CASCADE);
         INSERT INTO fkp2 VALUES (1);
         INSERT INTO fkc2 VALUES (1, 1);
         DELETE FROM fkp2 WHERE id = 1;",
    )
    .unwrap();
    assert_rows_eq(&conn.query("SELECT * FROM fkc2").unwrap().rows, &[]);

    // 7. CREATE TABLE and ALTER TABLE ADD COLUMN reject unknown collations!
    assert!(conn
        .execute("CREATE TABLE bad_coll(a TEXT COLLATE no_such_coll_xyz)")
        .unwrap_err()
        .to_string()
        .contains("no such collation"));
    conn.execute("CREATE TABLE ok_coll(a INT);").unwrap();
    assert!(conn
        .execute("ALTER TABLE ok_coll ADD COLUMN b TEXT COLLATE no_such_coll_xyz")
        .unwrap_err()
        .to_string()
        .contains("no such collation"));

    // 8. REINDEX <collation> on WITHOUT ROWID secondary index whose PK tail uses <collation>!
    conn.execute(
        "CREATE TABLE wr_ri(k TEXT COLLATE NOCASE PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE INDEX wr_ri_v ON wr_ri(v);
         INSERT INTO wr_ri VALUES ('a', 1), ('B', 1), ('c', 2);
         REINDEX NOCASE;",
    )
    .unwrap();
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 9. PRAGMA user_version / application_id / busy_timeout reset to 0 on unparseable integer arg!
    conn.execute(
        "PRAGMA user_version = 42;
         PRAGMA application_id = 1234;
         PRAGMA busy_timeout = 5000;",
    )
    .unwrap();
    conn.execute(
        "PRAGMA user_version = 'abc';
         PRAGMA application_id = 'abc';
         PRAGMA busy_timeout = 'abc';",
    )
    .unwrap();
    assert_rows_eq(&conn.execute("PRAGMA user_version").unwrap().rows, &[vec![i(0)]]);
    assert_rows_eq(&conn.execute("PRAGMA application_id").unwrap().rows, &[vec![i(0)]]);
    assert_rows_eq(&conn.execute("PRAGMA busy_timeout").unwrap().rows, &[vec![i(0)]]);

    // 10. PRAGMA table_info(view): COLLATE preserves type; circular view, compound right-branch
    //     missing table, and invalid WHERE fail cleanly!
    conn.execute(
        "CREATE TABLE ti_src(a INTEGER, b TEXT);
         CREATE VIEW ti_coll AS SELECT a COLLATE NOCASE AS ac, b COLLATE RTRIM AS bc FROM ti_src;
         CREATE VIEW ti_circ AS SELECT * FROM ti_circ;
         CREATE VIEW ti_bad_rhs AS SELECT a FROM ti_src UNION ALL SELECT x FROM ti_missing_tbl;
         CREATE VIEW ti_bad_where AS SELECT a FROM ti_src WHERE no_such_col = 1;",
    )
    .unwrap();
    let ti_c = conn
        .query("SELECT name, type FROM pragma_table_info('ti_coll') ORDER BY cid")
        .unwrap()
        .rows;
    assert_rows_eq(
        &ti_c,
        &[vec![t("ac"), t("INTEGER")], vec![t("bc"), t("TEXT")]],
    );
    assert!(conn
        .query("SELECT * FROM pragma_table_info('ti_circ')")
        .unwrap_err()
        .to_string()
        .contains("circularly defined"));
    assert!(conn
        .query("SELECT * FROM pragma_table_info('ti_bad_rhs')")
        .is_err());
    assert!(conn
        .query("SELECT * FROM pragma_table_info('ti_bad_where')")
        .is_err());

    // 11. ANALYZE <view> is a clean no-op!
    conn.execute("ANALYZE ti_coll;").unwrap();

    // 12. WITH t AS (...) UPDATE t SET a = t.a: CTE must NOT shadow UPDATE target qualifier!
    conn.execute(
        "CREATE TABLE cte_u(a INT);
         INSERT INTO cte_u VALUES (10);
         WITH cte_u AS (SELECT 999 AS a) UPDATE cte_u SET a = cte_u.a + 1;",
    )
    .unwrap();
    assert_rows_eq(&conn.query("SELECT a FROM cte_u").unwrap().rows, &[vec![i(11)]]);
}

#[test]
fn audit5_batch4_dml_triggers_savepoints_and_seed_json() {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();

    // 1. P0 OR REPLACE where BEFORE DELETE trigger deletes the conflicting row itself (`reload_live_row == None`)!
    conn.execute(
        "PRAGMA recursive_triggers = ON;
         CREATE TABLE del_self(id INTEGER PRIMARY KEY, u INT UNIQUE, v TEXT);
         CREATE TABLE del_self_fired(x INT);
         INSERT INTO del_self VALUES (1, 10, 'old');
         CREATE TRIGGER tr_ds_bd BEFORE DELETE ON del_self WHEN NOT EXISTS (SELECT 1 FROM del_self_fired) BEGIN
            INSERT INTO del_self_fired VALUES (1);
            DELETE FROM del_self WHERE id = 1;
         END;
         INSERT OR REPLACE INTO del_self VALUES (2, 10, 'new');
         INSERT INTO del_self(u, v) VALUES (20, 'next');",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT id, u, v FROM del_self ORDER BY id")
            .unwrap()
            .rows,
        &[vec![i(2), i(10), t("new")], vec![i(3), i(20), t("next")]],
    );
    assert_rows_eq(
        &conn.execute("PRAGMA integrity_check").unwrap().rows,
        &[vec![t("ok")]],
    );

    // 2. UPDATE OR REPLACE + recursive_triggers = ON with BEFORE DELETE RAISE(FAIL) & AFTER DELETE RAISE(FAIL)!
    {
        let mut c1 = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        c1.execute(
            "PRAGMA recursive_triggers = ON;
             CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE log(msg TEXT);
             INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c');
             CREATE TRIGGER bd BEFORE DELETE ON t BEGIN
                INSERT INTO log VALUES ('bd_' || OLD.id);
                SELECT RAISE(FAIL, 'stop_bd');
             END;",
        )
        .unwrap();
        assert!(c1
            .execute("UPDATE OR REPLACE t SET id = 2 WHERE id = 1")
            .unwrap_err()
            .to_string()
            .contains("stop_bd"));
        assert_rows_eq(
            &c1.query("SELECT id, v FROM t ORDER BY id").unwrap().rows,
            &[
                vec![i(1), t("a")],
                vec![i(2), t("b")],
                vec![i(3), t("c")],
            ],
        );
        assert_rows_eq(
            &c1.query("SELECT msg FROM log").unwrap().rows,
            &[vec![t("bd_2")]],
        );

        let mut c2 = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        c2.execute(
            "PRAGMA recursive_triggers = ON;
             CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE log(msg TEXT);
             INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c');
             CREATE TRIGGER ad AFTER DELETE ON t BEGIN
                INSERT INTO log VALUES ('ad_' || OLD.id);
                SELECT RAISE(FAIL, 'stop_ad');
             END;",
        )
        .unwrap();
        assert!(c2
            .execute("UPDATE OR REPLACE t SET id = 2 WHERE id = 1")
            .unwrap_err()
            .to_string()
            .contains("stop_ad"));
        assert_rows_eq(
            &c2.query("SELECT id, v FROM t ORDER BY id").unwrap().rows,
            &[vec![i(1), t("a")], vec![i(3), t("c")]],
        );
        assert_rows_eq(
            &c2.query("SELECT msg FROM log").unwrap().rows,
            &[vec![t("ad_2")]],
        );
    }

    // 3. seed_json on INSTEAD OF INSERT view, OR IGNORE AUTOINCREMENT, duplicate IPK, recursive DELETE triggers!
    {
        let mut sc = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        sc.execute(
            "PRAGMA recursive_triggers = ON;
             CREATE TABLE base_v(a INT, b TEXT);
             CREATE VIEW ins_v AS SELECT a, b FROM base_v;
             CREATE TRIGGER tr_iv INSTEAD OF INSERT ON ins_v BEGIN
                INSERT INTO base_v VALUES (NEW.a * 10, NEW.b);
             END;
             CREATE TABLE ai_s(id INTEGER PRIMARY KEY AUTOINCREMENT, k TEXT UNIQUE);
             INSERT INTO ai_s(k) VALUES ('dup');
             CREATE TABLE rep_s(id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE rep_log(deleted_id INT);
             INSERT INTO rep_s VALUES (1, 'old');
             CREATE TRIGGER tr_rep_d AFTER DELETE ON rep_s BEGIN
                INSERT INTO rep_log VALUES (OLD.id);
             END;",
        )
        .unwrap();
        sc.seed_json(
            r#"{
                "tables": [
                    { "name": "ins_v", "rows": [{ "a": 3, "b": "ok" }] },
                    { "name": "ai_s", "mode": "ignore", "rows": [{ "k": "dup" }] },
                    { "name": "rep_s", "mode": "replace", "rows": [{ "id": 1, "v": "replaced" }] }
                ]
            }"#,
        )
        .unwrap();
        assert_rows_eq(
            &sc.query("SELECT a, b FROM base_v").unwrap().rows,
            &[vec![i(30), t("ok")]],
        );
        // OR IGNORE advanced sqlite_sequence to 2!
        assert_rows_eq(
            &sc.query("SELECT seq FROM sqlite_sequence WHERE name = 'ai_s'")
                .unwrap()
                .rows,
            &[vec![i(2)]],
        );
        // Recursive DELETE trigger fired during seed_json OR REPLACE!
        assert_rows_eq(&sc.query("SELECT deleted_id FROM rep_log").unwrap().rows, &[vec![i(1)]]);
        assert_rows_eq(
            &sc.query("SELECT id, v FROM rep_s").unwrap().rows,
            &[vec![i(1), t("replaced")]],
        );
    }

    // 4. Outer WITH + inner WITH in INSERT ... SELECT preserves BOTH CTEs!
    conn.execute(
        "CREATE TABLE w_dst(x INT, y INT);
         WITH a AS (SELECT 10 AS x)
         INSERT INTO w_dst
         WITH b AS (SELECT 20 AS y)
         SELECT a.x, b.y FROM a, b;",
    )
    .unwrap();
    assert_rows_eq(&conn.query("SELECT x, y FROM w_dst").unwrap().rows, &[vec![i(10), i(20)]]);

    // 5. 0-row DML / RETURNING / UPSERT DO UPDATE validates unknown functions, aggregates, windows upfront!
    conn.execute("CREATE TABLE z_dml(id INT PRIMARY KEY, v INT);")
        .unwrap();
    assert!(conn
        .execute("UPDATE z_dml SET v = 1 RETURNING no_such_fn_xyz(v)")
        .is_err());
    assert!(conn
        .execute("DELETE FROM z_dml RETURNING count(*)")
        .is_err());
    assert!(conn
        .execute("DELETE FROM z_dml RETURNING row_number() OVER ()")
        .is_err());
    assert!(conn
        .execute("INSERT INTO z_dml SELECT 1, 2 WHERE 0 ON CONFLICT(id) DO UPDATE SET v = no_such_fn_xyz(v)")
        .is_err());
    assert!(conn
        .execute("INSERT INTO z_dml SELECT 1, 2 WHERE 0 ON CONFLICT(id) DO UPDATE SET v = count(*)")
        .is_err());

    // 6. UPDATE ... FROM with RAISE() outside trigger rejected!
    assert!(conn
        .execute("UPDATE z_dml SET v = 1 FROM (SELECT RAISE(ABORT, 'bad'))")
        .unwrap_err()
        .to_string()
        .contains("trigger-program"));

    // 7. CHECK (RAISE(ABORT, 'from_check')) fires when executed inside a trigger!
    conn.execute(
        "CREATE TABLE chk_r(x INT CHECK(CASE WHEN x < 0 THEN RAISE(ABORT, 'from_check') ELSE 1 END));
         CREATE TABLE chk_ev(v INT);
         CREATE TRIGGER tr_chk_ev AFTER INSERT ON chk_ev BEGIN
            INSERT INTO chk_r VALUES (NEW.v);
         END;",
    )
    .unwrap();
    assert!(conn
        .execute("INSERT INTO chk_ev VALUES (-1)")
        .unwrap_err()
        .to_string()
        .contains("from_check"));

    // 8. Negative sqlite_sequence.seq = -10 updated to -5 (not clamped to 0), while brand-new row clamps to 0!
    conn.execute(
        "CREATE TABLE neg_seq(id INTEGER PRIMARY KEY AUTOINCREMENT, v INT);
         INSERT INTO neg_seq(id, v) VALUES (-5, 1);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'neg_seq'")
            .unwrap()
            .rows,
        &[vec![i(0)]],
    );
    conn.execute(
        "UPDATE sqlite_sequence SET seq = -10 WHERE name = 'neg_seq';
         INSERT INTO neg_seq(id, v) VALUES (-4, 2);",
    )
    .unwrap();
    assert_rows_eq(
        &conn
            .query("SELECT seq FROM sqlite_sequence WHERE name = 'neg_seq'")
            .unwrap()
            .rows,
        &[vec![i(-4)]],
    );

    // 9. ROLLBACK outside transaction fails cleanly without clobbering caches!
    conn.execute("CREATE TABLE rb_c(id INTEGER PRIMARY KEY AUTOINCREMENT, v INT); INSERT INTO rb_c(v) VALUES (1);")
        .unwrap();
    assert!(conn.execute("ROLLBACK").is_err());
    conn.execute("INSERT INTO rb_c(v) VALUES (2);").unwrap();
    assert_rows_eq(
        &conn.query("SELECT id, v FROM rb_c ORDER BY id").unwrap().rows,
        &[vec![i(1), i(1)], vec![i(2), i(2)]],
    );
}
