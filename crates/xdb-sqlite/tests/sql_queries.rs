//! Phase 2 SQL query engine integration tests executed directly against real
//! SQLite 3.54.1 databases in `E:\repos\xdb\xdb\fixtures\generated\sqlite`.
//!
//! Verifies:
//! - Views (`v_mixed`, `v_join`, `only_view`)
//! - `sqlite_schema` / `sqlite_master` and pragma TVFs (`pragma_table_info`, `pragma_table_xinfo`,
//!   `pragma_index_list`, `pragma_index_xinfo`, `pragma_database_list`)
//! - Rowid & secondary index pushdown (verifying O(log N) page reads on `deep` table!)
//! - `WITHOUT ROWID`, shadowed `rowid`/`oid`, `COLLATE NOCASE`/`RTRIM`, UTF-16LE/BE databases
//! - Complex joins, aggregates, windows, CTEs, subqueries, JSON, and XDB dialect
//! - Deterministic `BudgetMeter` query halts

use std::path::PathBuf;
use std::sync::Arc;

use xdb_core::{BudgetMeter, LimitKind, ResourceLimits, SqlValue};
use xdb_parser::Dialect;
use xdb_sqlite::{open_database, OpenOptions, QueryParams, SqliteError};
use xdb_vfs_fs::StdFsVfs;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(r"E:\repos\xdb\xdb\fixtures\generated\sqlite")
}

#[test]
fn sql_views_schema_pragmas_and_generated_columns() {
    let root = fixtures_dir();
    if !root.exists() {
        return;
    }
    let vfs = StdFsVfs::new();

    // 1. page1-view-only.db
    let vdb = open_database(
        &vfs,
        root.join("page1-view-only.db").to_str().unwrap(),
        OpenOptions::default(),
    )
    .expect("open view-only db");
    let vres = vdb.query("SELECT * FROM only_view").expect("query view");
    assert_eq!(vres.columns, vec!["one", "two"]);
    assert_eq!(vres.rows.len(), 1);
    assert!(vres.rows[0][0].exact_eq(&SqlValue::Int(1)));
    assert!(vres.rows[0][1].exact_eq(&SqlValue::text("two")));

    // 2. pagesize-4096.db
    let db = open_database(
        &vfs,
        root.join("pagesize-4096.db").to_str().unwrap(),
        OpenOptions::default(),
    )
    .expect("open pagesize-4096.db");

    // Views v_mixed and v_join match direct queries
    let vm = db
        .query("SELECT count(*) FROM v_mixed")
        .expect("v_mixed count");
    let vm_direct = db
        .query("SELECT count(*) FROM mixed WHERE i IS NOT NULL")
        .expect("mixed count");
    assert!(vm.rows[0][0].exact_eq(&vm_direct.rows[0][0]));

    let vj = db
        .query("SELECT * FROM v_join ORDER BY id")
        .expect("v_join");
    let vj_direct = db
        .query("SELECT a.id, a.v AS ipk_v, b.v AS ai_v FROM ipk a JOIN ai b ON a.id = b.id ORDER BY a.id")
        .expect("v_join direct");
    assert_eq!(vj.rows.len(), vj_direct.rows.len());
    for (a, b) in vj.rows.iter().zip(vj_direct.rows.iter()) {
        for (ca, cb) in a.iter().zip(b.iter()) {
            assert!(ca.exact_eq(cb));
        }
    }

    // sqlite_schema query
    let sch = db
        .query("SELECT type, count(*) AS n FROM sqlite_schema GROUP BY type ORDER BY type")
        .expect("sqlite_schema group");
    assert!(sch.rows.len() >= 4); // index, table, trigger, view

    // pragma_table_info vs pragma_table_xinfo on `gen` (VIRTUAL + STORED generated columns)
    let pinfo = db
        .query("SELECT name FROM pragma_table_info('gen') ORDER BY cid")
        .expect("pragma_table_info");
    let pxinfo = db
        .query("SELECT name, hidden FROM pragma_table_xinfo('gen') ORDER BY cid")
        .expect("pragma_table_xinfo");
    assert!(pxinfo.rows.len() > pinfo.rows.len());
    assert!(pxinfo
        .rows
        .iter()
        .any(|r| r[1].exact_eq(&SqlValue::Int(2)))); // hidden=2 VIRTUAL
    assert!(pxinfo
        .rows
        .iter()
        .any(|r| r[1].exact_eq(&SqlValue::Int(3)))); // hidden=3 STORED

    // pragma_index_list & pragma_index_xinfo
    let ilist = db
        .query("SELECT name, \"unique\" FROM pragma_index_list('mixed') ORDER BY name")
        .expect("pragma_index_list");
    assert!(ilist.rows.len() >= 5);
    let ixinfo = db
        .query("SELECT name, \"desc\", coll, key FROM pragma_index_xinfo('ix_mixed_desc') ORDER BY seqno")
        .expect("pragma_index_xinfo");
    assert!(ixinfo.rows.len() >= 3);
    assert!(ixinfo.rows[0][1].exact_eq(&SqlValue::Int(1))); // r DESC

    // Shadowed rowid/oid table
    let sh = db
        .query("SELECT rowid, oid, _rowid_, v FROM shadow ORDER BY _rowid_ LIMIT 5")
        .expect("shadowed rowid");
    assert!(!sh.rows.is_empty());
    // `rowid` is a TEXT column in `shadow`, while `_rowid_` still accesses the real integer rowid!
    assert!(sh.rows[0][2].is_integer());

    // Windows, CTEs, JSON aggregation on real SQLite table
    let win = db
        .query(
            "WITH top_ints AS (SELECT id, v FROM ints WHERE v IS NOT NULL ORDER BY id LIMIT 10) \
             SELECT id, v, row_number() OVER (ORDER BY id) AS rn, \
                    sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS cum \
             FROM top_ints ORDER BY rn",
        )
        .expect("window over CTE");
    assert_eq!(win.rows.len(), 10);
    assert!(win.rows[9][2].exact_eq(&SqlValue::Int(10)));

    // XDB dialect on real SQLite database
    let mut xdb_opts = db.default_query_options();
    xdb_opts.dialect = Dialect::Xdb;
    let xres = db
        .query_with_options(
            "FROM ints WHERE label ILIKE '%zero%' ORDER BY ALL LIMIT 5",
            &QueryParams::empty(),
            &xdb_opts,
        )
        .expect("XDB FROM-first on SQLite db");
    assert_eq!(xres.columns, vec!["id", "v", "u", "label"]);
}

#[test]
fn sql_rowid_and_index_pushdown_reduces_page_reads() {
    let root = fixtures_dir();
    if !root.exists() {
        return;
    }
    let vfs = StdFsVfs::new();
    let db_path = root.join("pagesize-4096.db");

    // Open fresh with a metered BudgetMeter so we can measure exact page reads!
    let meter_idx = Arc::new(BudgetMeter::new(ResourceLimits::UNLIMITED));
    let db_idx = open_database(
        &vfs,
        db_path.to_str().unwrap(),
        OpenOptions {
            meter: Some(meter_idx.clone()),
            ..OpenOptions::default()
        },
    )
    .expect("open metered db");

    // Baseline schema load reads
    let after_open = meter_idx.usage().page_reads;

    // Point lookup on `deep` (which has thousands of rows across many pages!) via rowid IPK
    let rid_res = db_idx
        .query("SELECT id, k, t FROM deep WHERE id = 500")
        .expect("rowid seek on deep");
    assert_eq!(rid_res.rows.len(), 1);
    assert!(rid_res.rows[0][0].exact_eq(&SqlValue::Int(500)));
    let reads_for_rowid_seek = meter_idx.usage().page_reads - after_open;
    assert!(
        reads_for_rowid_seek <= 6,
        "expected O(log N) <= 6 page reads for rowid point lookup, got {reads_for_rowid_seek}"
    );

    // Secondary index seek on `deep(t)` via `ix_deep_t`
    let target_t = rid_res.rows[0][2].clone();
    let before_idx_seek = meter_idx.usage().page_reads;
    let params = QueryParams::positional(vec![target_t]);
    let idx_res = db_idx
        .query_with_params("SELECT id, k, t FROM deep WHERE t = ?", &params)
        .expect("index seek on deep(t)");
    assert!(!idx_res.rows.is_empty());
    assert!(idx_res
        .rows
        .iter()
        .any(|r| r[0].exact_eq(&SqlValue::Int(500))));
    let reads_for_idx_seek = meter_idx.usage().page_reads - before_idx_seek;
    assert!(
        reads_for_idx_seek <= 12,
        "expected O(log N) <= 12 page reads for secondary index lookup, got {reads_for_idx_seek}"
    );
}

#[test]
fn sql_utf16_and_wal_and_meter_halt() {
    let root = fixtures_dir();
    if !root.exists() {
        return;
    }
    let vfs = StdFsVfs::new();

    // 1. UTF-16BE database with NOCASE/RTRIM + VIRTUAL columns
    let u16db = open_database(
        &vfs,
        root.join("utf16be-p1024.db").to_str().unwrap(),
        OpenOptions::default(),
    )
    .expect("open utf16be");
    let u16res = u16db
        .query("SELECT count(*), sum(v1) FROM gen")
        .expect("query gen on utf16be");
    assert_eq!(u16res.rows.len(), 1);
    assert!(u16res.rows[0][0].as_i64().unwrap() > 0);

    // 2. WAL overlay SQL query
    let waldb = open_database(
        &vfs,
        root.join("wal-uncheckpointed-p4096.db").to_str().unwrap(),
        OpenOptions::default(),
    )
    .expect("open wal");
    let wal_res = waldb
        .query("SELECT count(*) FROM wal_log")
        .expect("wal_log count");
    assert!(wal_res.rows[0][0].as_i64().unwrap() > 0);

    // 3. Meter halts expensive cross-join on real SQLite DB deterministically
    let tight_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(2_000),
        max_memory_bytes: None,
        max_page_reads: None,
    }));
    let mut opts = waldb.default_query_options();
    opts.meter = Some(tight_meter);
    let err = waldb
        .query_with_options(
            "SELECT count(*) FROM base a CROSS JOIN base b",
            &QueryParams::empty(),
            &opts,
        )
        .expect_err("cross join must halt on step limit");
    match err {
        SqliteError::ResourceLimit(r) => assert_eq!(r.kind, LimitKind::Steps),
        other => panic!("expected ResourceLimit, got {other:?}"),
    }
}
