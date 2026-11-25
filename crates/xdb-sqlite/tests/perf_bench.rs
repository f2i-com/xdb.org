//! Release-mode micro-benchmark suite for XDB SQLite hot paths.
//!
//! Run with:
//! `cargo test --release -p xdb-sqlite --test perf_bench -- --ignored --nocapture`

use std::time::Instant;

use xdb_core::{SqlValue, TextEncoding};
use xdb_parser::{parse_statement, parse_statement_at, ParseOptions};
use xdb_sqlite::{ConnectionOptions, QueryParams, SqliteConnection};

fn bench_ms<F: FnMut()>(label: &str, iters: u32, mut f: F) {
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = t0.elapsed();
    let per_iter_ms = elapsed.as_secs_f64() * 1000.0 / f64::from(iters);
    println!("{label:<48} {:>9.2} ms (total {:>8.2} ms, x{iters})", per_iter_ms, elapsed.as_secs_f64() * 1000.0);
}

#[test]
#[ignore = "run explicitly with --release -- --ignored --nocapture"]
fn run_perf_benchmarks() {
    println!("\n=== XDB SQLite Performance Benchmarks ===");

    // 1. Parse multi-row VALUES (1,500 rows)
    let mut values_sql = String::from("INSERT INTO t(a, b) VALUES ");
    for i in 0..1500 {
        if i > 0 {
            values_sql.push(',');
        }
        values_sql.push_str(&format!("({i}, 'row_{i}')"));
    }
    bench_ms("1. parse 1500-row VALUES", 1, || {
        let _ = parse_statement(&values_sql, &ParseOptions::default()).unwrap();
    });

    // 2. Parse 1,500-statement script via parse_statement_at loop
    let mut script_sql = String::new();
    for i in 0..1500 {
        script_sql.push_str(&format!("INSERT INTO t VALUES ({i}, 'val_{i}');\n"));
    }
    bench_ms("2. parse 1500-stmt script", 3, || {
        let opts = ParseOptions::default();
        let mut off = 0;
        while off < script_sql.len() {
            let p = parse_statement_at(&script_sql, off, &opts).unwrap();
            if p.end <= off {
                break;
            }
            off = p.end;
        }
    });

    // 3. Bulk INSERT 5,000 rows (AUTOINCREMENT + secondary index) in one txn
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
    conn.execute(
        "CREATE TABLE big (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grp INT NOT NULL,
            val INT NOT NULL,
            name TEXT NOT NULL,
            pad TEXT NOT NULL
         );
         CREATE INDEX idx_big_grp ON big(grp);
         CREATE TABLE small (v INT PRIMARY KEY);",
    )
    .unwrap();
    let pad = "x".repeat(64);
    bench_ms("3. bulk INSERT 5000 rows (AUTOINC+idx)", 1, || {
        conn.execute("BEGIN").unwrap();
        for i in 1..=5000i64 {
            let p = QueryParams::positional(vec![
                SqlValue::Int(i % 50),
                SqlValue::Int((i * 7919) % 10_000),
                SqlValue::text(format!("user_{i}")),
                SqlValue::text(pad.as_str()),
            ]);
            conn.execute_with_params(
                "INSERT INTO big(grp, val, name, pad) VALUES (?1, ?2, ?3, ?4)",
                &p,
            )
            .unwrap();
        }
        conn.execute("COMMIT").unwrap();
    });
    conn.execute("BEGIN").unwrap();
    for i in 0..200i64 {
        let p = QueryParams::positional(vec![SqlValue::Int(i * 50)]);
        conn.execute_with_params("INSERT INTO small VALUES (?1)", &p)
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    assert_eq!(conn.encoding(), TextEncoding::Utf8);

    // 4. 500 point UPDATEs WHERE id = ?
    bench_ms("4. 500 point UPDATEs (WHERE id = ?)", 1, || {
        conn.execute("BEGIN").unwrap();
        for i in 1..=500i64 {
            let p = QueryParams::positional(vec![SqlValue::Int(i * 3), SqlValue::Int(i * 5)]);
            conn.execute_with_params("UPDATE big SET val = ?1 WHERE id = ?2", &p)
                .unwrap();
        }
        conn.execute("COMMIT").unwrap();
    });

    // 5. SELECT count(*) FROM big
    bench_ms("5. SELECT count(*) FROM big (5000 rows)", 20, || {
        let r = conn.query("SELECT count(*) FROM big").unwrap();
        assert!(r.rows[0][0].exact_eq(&SqlValue::Int(5000)));
    });

    // 6. SELECT id, val FROM big WHERE id BETWEEN 1000 AND 1050
    bench_ms("6. SELECT WHERE id BETWEEN 1000..1050", 20, || {
        let r = conn
            .query("SELECT id, val FROM big WHERE id BETWEEN 1000 AND 1050")
            .unwrap();
        assert_eq!(r.rows.len(), 51);
    });

    // 7. SELECT id, val FROM big WHERE grp = 7 (index equality + projection)
    bench_ms("7. SELECT WHERE grp = 7 (100/5000 rows)", 20, || {
        let r = conn.query("SELECT id, val FROM big WHERE grp = 7").unwrap();
        assert_eq!(r.rows.len(), 100);
    });

    // 8. SELECT id, val FROM big ORDER BY val DESC LIMIT 10
    bench_ms("8. SELECT ORDER BY val DESC LIMIT 10", 10, || {
        let r = conn
            .query("SELECT id, val FROM big ORDER BY val DESC LIMIT 10")
            .unwrap();
        assert_eq!(r.rows.len(), 10);
    });

    // 9. Running window sum OVER (ORDER BY id) on 2,000 rows
    bench_ms("9. window sum(val) OVER (ORDER BY id) 2k", 1, || {
        let r = conn
            .query("SELECT id, sum(val) OVER (ORDER BY id) FROM big WHERE id <= 2000")
            .unwrap();
        assert_eq!(r.rows.len(), 2000);
    });

    // 10. Uncorrelated IN (SELECT v FROM small) over 2,000 outer rows
    bench_ms("10. WHERE val IN (SELECT v FROM small) 2k", 1, || {
        let r = conn
            .query("SELECT count(*) FROM big WHERE id <= 2000 AND val IN (SELECT v FROM small)")
            .unwrap();
        assert_eq!(r.rows.len(), 1);
    });

    // 11. CREATE INDEX on 5,000 rows
    bench_ms("11. CREATE INDEX idx_big_val (5000 rows)", 1, || {
        conn.execute("CREATE INDEX idx_big_val ON big(val)").unwrap();
    });

    // 8b. SELECT id, val FROM big ORDER BY val DESC LIMIT 10 (now indexed via idx_big_val!)
    bench_ms("8b. SELECT ORDER BY val DESC LIMIT 10 (idx)", 50, || {
        let r = conn
            .query("SELECT id, val FROM big ORDER BY val DESC LIMIT 10")
            .unwrap();
        assert_eq!(r.rows.len(), 10);
    });

    // 12. 500 point DELETEs WHERE id = ?
    bench_ms("12. 500 point DELETEs (WHERE id = ?)", 1, || {
        conn.execute("BEGIN").unwrap();
        for i in 1..=500i64 {
            let p = QueryParams::positional(vec![SqlValue::Int(i * 9)]);
            conn.execute_with_params("DELETE FROM big WHERE id = ?1", &p)
                .unwrap();
        }
        conn.execute("COMMIT").unwrap();
    });

    // 14. GROUP BY grp over 4,500 rows -> 50 groups
    bench_ms("14. GROUP BY grp (4500 rows -> 50 grps)", 10, || {
        let r = conn
            .query("SELECT grp, count(*), sum(val), avg(val) FROM big GROUP BY grp")
            .unwrap();
        assert_eq!(r.rows.len(), 50);
    });

    // 15. 500 point SELECTs WHERE id = ?
    bench_ms("15. 500 point SELECTs (WHERE id = ?)", 1, || {
        for i in 1..=500i64 {
            let p = QueryParams::positional(vec![SqlValue::Int(i * 7)]);
            let r = conn
                .query_with_params("SELECT id, grp, val, name FROM big WHERE id = ?1", &p)
                .unwrap();
            assert!(r.rows.len() <= 1);
        }
    });

    // 16. Hash equi-JOIN big (4,500 rows) x grps (50 rows)
    conn.execute("CREATE TABLE grps (grp INT PRIMARY KEY, label TEXT NOT NULL);")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for g in 0..50i64 {
        let p = QueryParams::positional(vec![SqlValue::Int(g), SqlValue::text(format!("g_{g}"))]);
        conn.execute_with_params("INSERT INTO grps VALUES (?1, ?2)", &p)
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    bench_ms("16. JOIN big x grps (4500 rows)", 5, || {
        let r = conn
            .query("SELECT b.id, b.val, g.label FROM big b JOIN grps g ON b.grp = g.grp")
            .unwrap();
        assert_eq!(r.rows.len(), 4500);
    });

    // 13. seed_json 1,000 rows
    let mut seed_doc = String::from(r#"{"items":["#);
    for i in 0..1000 {
        if i > 0 {
            seed_doc.push(',');
        }
        seed_doc.push_str(&format!(r#"{{"id":{i},"name":"item_{i}","score":{}}}"#, i * 3));
    }
    seed_doc.push_str("]}");
    bench_ms("13. seed_json 1000 rows", 1, || {
        let mut sc = SqliteConnection::open_in_memory(ConnectionOptions::default()).unwrap();
        let rep = sc.seed_json(&seed_doc).unwrap();
        assert_eq!(rep.rows_inserted, 1000);
    });

    println!("=========================================\n");
}
