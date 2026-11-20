//! Phase 1 exit gate + integration tests against all 25 real SQLite 3.54.1
//! fixture databases and golden JSONs in `E:\repos\xdb\xdb\fixtures\generated\sqlite`.
//!
//! Verifies:
//! - Exact header fields, page count, freelist, and auto-vacuum pointer maps
//! - Exact `sqlite_schema` entries, table metadata, column metadata, and index metadata
//! - Every single cell of every stored column of every row of every table (all 25 fixtures + 3 WAL main-only)
//! - Every single cell of every entry of every index in B-tree order
//! - `TableReader::get(rowid)`, `SearchKey` + `IndexEntryCursor::seek_key` (ASC/DESC, collations, UTF-8/UTF-16)
//! - `VirtualColumnError`, `DatabaseClosed`, and deterministic `BudgetMeter` step/memory halts

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as JsonValue;
use xdb_core::{BudgetMeter, LimitKind, ResourceLimits, SqlValue};
use xdb_sqlite::{
    open_database, open_database_from_bytes, open_snapshot, ColumnHiddenKind, ColumnSpec,
    OpenOptions, ScanOptions, SearchKey, SeekOp, SeekResult, SnapshotOptions, SqliteDatabase,
    SqliteError, WalMode,
};
use xdb_vfs_fs::StdFsVfs;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(r"E:\repos\xdb\xdb\fixtures\generated\sqlite")
}

fn hex_decode(hex: &str) -> Vec<u8> {
    let b = hex.as_bytes();
    assert_eq!(b.len() % 2, 0);
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        let hi = hex_nibble(b[i]);
        let lo = hex_nibble(b[i + 1]);
        out.push((hi << 4) | lo);
    }
    out
}

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("invalid hex byte: {c}"),
    }
}

fn parse_golden_real(s: &str) -> f64 {
    match s {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        "-0" | "-0.0" => -0.0,
        other => other
            .parse::<f64>()
            .unwrap_or_else(|e| panic!("invalid golden real {other:?}: {e}")),
    }
}

fn golden_cell_to_sql(cell: &JsonValue) -> SqlValue {
    match cell["t"].as_str().expect("cell has t") {
        "null" => SqlValue::Null,
        "int" => {
            let v = cell["v"]
                .as_str()
                .expect("int has v")
                .parse::<i64>()
                .expect("valid i64");
            SqlValue::Int(v)
        }
        "real" => SqlValue::real(parse_golden_real(cell["v"].as_str().expect("real has v"))),
        "text" => SqlValue::text(cell["v"].as_str().expect("text has v")),
        "blob" => SqlValue::blob(hex_decode(cell["v"].as_str().expect("blob has v"))),
        other => panic!("unexpected golden cell type: {other}"),
    }
}

#[derive(Default)]
struct Totals {
    fixtures: usize,
    tables: usize,
    rows: usize,
    indexes: usize,
    entries: usize,
    cells_compared: usize,
    virtual_verified: usize,
    catalog_checks: usize,
}

fn verify_database_against_golden(
    db: &SqliteDatabase,
    golden: &JsonValue,
    label: &str,
    totals: &mut Totals,
) {
    // 1. Header & page counts
    let exp_hdr = &golden["header"];
    if exp_hdr.is_null() {
        assert!(db.header().is_none(), "{label}: expected null header");
        assert_eq!(db.page_count(), 0, "{label}: expected 0 pages");
    } else {
        let h = db.header().expect("non-empty fixture must have header");
        assert_eq!(db.page_size(), exp_hdr["pageSize"].as_u64().unwrap() as u32, "{label}: pageSize");
        assert_eq!(h.write_version, exp_hdr["writeVersion"].as_u64().unwrap() as u8, "{label}: writeVersion");
        assert_eq!(h.read_version, exp_hdr["readVersion"].as_u64().unwrap() as u8, "{label}: readVersion");
        assert_eq!(h.reserved_bytes, exp_hdr["reservedBytes"].as_u64().unwrap() as u8, "{label}: reservedBytes");
        assert_eq!(h.freelist_count, exp_hdr["freelistCount"].as_u64().unwrap() as u32, "{label}: freelistCount");
        assert_eq!(h.schema_cookie, exp_hdr["schemaCookie"].as_u64().unwrap() as u32, "{label}: schemaCookie");
        assert_eq!(h.schema_format, exp_hdr["schemaFormat"].as_u64().unwrap() as u32, "{label}: schemaFormat");
        assert_eq!(h.text_encoding, exp_hdr["textEncoding"].as_u64().unwrap() as u32, "{label}: textEncoding");
        assert_eq!(h.user_version, exp_hdr["userVersion"].as_i64().unwrap() as i32, "{label}: userVersion");
        assert_eq!(h.application_id, exp_hdr["applicationId"].as_i64().unwrap() as i32, "{label}: applicationId");
        assert_eq!(
            db.page_count(),
            golden["pragmas"]["pageCount"].as_u64().unwrap() as u32,
            "{label}: pageCount"
        );
        totals.catalog_checks += 11;
    }

    // Freelist + ptrmap integrity
    let fl = db
        .btree()
        .freelist()
        .unwrap_or_else(|e| panic!("{label}: freelist() failed: {e}"));
    assert_eq!(fl.total, db.btree().info().freelist_count as usize);
    if db.btree().info().auto_vacuum && db.page_count() >= 3 {
        assert!(db.btree().is_ptrmap_page(2));
        let entry = db.btree().ptrmap_entry(3).expect("valid ptrmap entry");
        assert!(entry.parent <= db.page_count());
    }

    // 2. sqlite_schema entries
    let exp_schema = golden["schema"].as_array().unwrap();
    let entries = &db.schema().entries;
    assert_eq!(entries.len(), exp_schema.len(), "{label}: schema count");
    for (got, exp) in entries.iter().zip(exp_schema.iter()) {
        assert_eq!(got.rowid, exp["rowid"].as_i64().unwrap(), "{label}: schema rowid");
        assert_eq!(got.obj_type.as_deref(), exp["type"].as_str(), "{label}: schema type");
        assert_eq!(got.name.as_deref(), exp["name"].as_str(), "{label}: schema name");
        assert_eq!(got.table_name.as_deref(), exp["tbl_name"].as_str(), "{label}: schema tbl_name");
        assert_eq!(got.root_page, exp["rootpage"].as_u64().unwrap() as u32, "{label}: schema rootpage");
        assert_eq!(got.sql.as_deref(), exp["sql"].as_str(), "{label}: schema sql");
        totals.catalog_checks += 6;
    }

    // 3. Tables metadata
    let exp_tables = golden["tables"].as_array().unwrap();
    let got_table_names: Vec<&str> = db.tables().iter().map(|t| t.name.as_str()).collect();
    let exp_table_names: Vec<&str> = exp_tables
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(got_table_names, exp_table_names, "{label}: table names");

    for gt in exp_tables {
        let tname = gt["name"].as_str().unwrap();
        let ti = db
            .schema()
            .table(tname)
            .unwrap_or_else(|| panic!("{label}: missing table {tname}"));
        assert_eq!(ti.root_page, gt["rootpage"].as_u64().unwrap() as u32, "{label}: {tname} rootpage");
        assert_eq!(ti.without_rowid, gt["withoutRowid"].as_bool().unwrap(), "{label}: {tname} withoutRowid");
        assert_eq!(ti.strict, gt["strict"].as_bool().unwrap(), "{label}: {tname} strict");
        assert_eq!(ti.system, gt["system"].as_bool().unwrap(), "{label}: {tname} system");
        let got_ipk = ti.ipk_column.map(|i| ti.columns[i].name.as_str());
        assert_eq!(got_ipk, gt["ipkColumn"].as_str(), "{label}: {tname} ipkColumn");
        assert_eq!(ti.has_virtual_columns, gt["hasVirtualColumns"].as_bool().unwrap(), "{label}: {tname} hasVirtual");

        let exp_cols = gt["columns"].as_array().unwrap();
        assert_eq!(ti.columns.len(), exp_cols.len(), "{label}: {tname} column count");
        for (c, gc) in ti.columns.iter().zip(exp_cols.iter()) {
            assert_eq!(c.cid, gc["cid"].as_u64().unwrap() as usize);
            assert_eq!(c.name, gc["name"].as_str().unwrap());
            assert_eq!(c.decl_type, gc["declType"].as_str().unwrap(), "{label}: {tname}.{} declType", c.name);
            assert_eq!(c.not_null, gc["notNull"].as_bool().unwrap(), "{label}: {tname}.{} notNull", c.name);
            assert_eq!(c.default_sql.as_deref(), gc["defaultSql"].as_str(), "{label}: {tname}.{} defaultSql", c.name);
            assert_eq!(c.pk, gc["pk"].as_u64().unwrap() as usize, "{label}: {tname}.{} pk", c.name);
            assert_eq!(c.hidden as u8, gc["hidden"].as_u64().unwrap() as u8, "{label}: {tname}.{} hidden", c.name);
            totals.catalog_checks += 7;
        }
    }

    // 4. Indexes metadata
    let exp_indexes = golden["indexes"].as_array().unwrap();
    let mut got_idx_names: Vec<&str> = db.indexes().iter().map(|i| i.name.as_str()).collect();
    let mut exp_idx_names: Vec<&str> = exp_indexes
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    got_idx_names.sort();
    exp_idx_names.sort();
    assert_eq!(got_idx_names, exp_idx_names, "{label}: index names");

    for gi in exp_indexes {
        let iname = gi["name"].as_str().unwrap();
        let ix = db
            .schema()
            .index(iname)
            .unwrap_or_else(|| panic!("{label}: missing index {iname}"));
        assert_eq!(ix.table_name, gi["table"].as_str().unwrap(), "{label}: {iname} table");
        assert_eq!(ix.root_page, gi["rootpage"].as_u64().unwrap() as u32, "{label}: {iname} rootpage");
        assert_eq!(ix.origin.as_str(), gi["origin"].as_str().unwrap(), "{label}: {iname} origin");
        assert_eq!(ix.unique, gi["unique"].as_bool().unwrap(), "{label}: {iname} unique");
        assert_eq!(ix.partial, gi["partial"].as_bool().unwrap(), "{label}: {iname} partial");
        assert_eq!(ix.where_sql.as_deref(), gi["where"].as_str(), "{label}: {iname} where");
        assert_eq!(
            ix.without_rowid_primary_key,
            gi["withoutRowidPrimaryKey"].as_bool().unwrap(),
            "{label}: {iname} withoutRowidPrimaryKey"
        );
        assert_eq!(ix.sql.as_deref(), gi["sql"].as_str(), "{label}: {iname} sql");

        let exp_cols = gi["columns"].as_array().unwrap();
        assert_eq!(ix.columns.len(), exp_cols.len(), "{label}: {iname} column count");
        for (i, (c, gc)) in ix.columns.iter().zip(exp_cols.iter()).enumerate() {
            assert_eq!(i, gc["seqno"].as_u64().unwrap() as usize);
            assert_eq!(c.cid, gc["cid"].as_i64().unwrap() as i32, "{label}: {iname} col {i} cid");
            assert_eq!(c.name.as_deref(), gc["name"].as_str(), "{label}: {iname} col {i} name");
            assert_eq!(c.desc, gc["desc"].as_bool().unwrap(), "{label}: {iname} col {i} desc");
            assert_eq!(c.collation, gc["coll"].as_str().unwrap(), "{label}: {iname} col {i} coll");
            assert_eq!(c.key, gc["key"].as_bool().unwrap(), "{label}: {iname} col {i} key");
            if c.cid == -2 {
                assert_eq!(c.expr.as_deref(), gc["expr"].as_str(), "{label}: {iname} col {i} expr");
            }
            totals.catalog_checks += 6;
        }
    }

    // 5. EVERY SINGLE CELL of EVERY SINGLE TABLE
    for gt in exp_tables {
        let tname = gt["name"].as_str().unwrap();
        let reader = db.table(tname).expect("table reader");
        let rowid_first = !reader.without_rowid();
        let stored_cols: Vec<_> = reader
            .columns()
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .collect();
        let virtual_count = reader.columns().len() - stored_cols.len();
        let golden_pos: Vec<usize> = stored_cols
            .iter()
            .map(|c| usize::from(rowid_first) + c.cid)
            .collect();

        // Confirm projecting a VIRTUAL column raises SqliteError::VirtualColumn.
        if virtual_count > 0 {
            let err = reader.cursor(None).unwrap_err();
            assert!(
                matches!(err, SqliteError::VirtualColumn { .. }),
                "{label}: {tname} expected VirtualColumn error, got {err}"
            );
        }

        let scan_opts = ScanOptions {
            columns: Some(
                stored_cols
                    .iter()
                    .map(|c| ColumnSpec::Cid(c.cid))
                    .collect(),
            ),
            rowid: rowid_first,
        };
        let mut cur = reader.cursor(Some(&scan_opts)).expect("row cursor");
        let exp_rows = gt["rows"].as_array().unwrap();
        let mut row_idx = 0usize;
        let mut sample_rowid: Option<(i64, Vec<SqlValue>)> = None;

        while cur.next().expect("cursor next") {
            let got_row = cur.row().expect("decode row");
            let exp_row = exp_rows[row_idx].as_array().unwrap();
            assert_eq!(got_row.len(), usize::from(rowid_first) + stored_cols.len());

            if rowid_first {
                let want_rid = golden_cell_to_sql(&exp_row[0]);
                assert!(
                    got_row[0].exact_eq(&want_rid),
                    "{label}: table {tname} row {row_idx} rowid: got {}, want {}",
                    got_row[0],
                    want_rid
                );
                totals.cells_compared += 1;
                if row_idx == exp_rows.len() / 2 {
                    if let SqlValue::Int(rid) = got_row[0] {
                        sample_rowid = Some((rid, got_row.clone()));
                    }
                }
            }
            for (k, col) in stored_cols.iter().enumerate() {
                let got_val = &got_row[usize::from(rowid_first) + k];
                let want_val = golden_cell_to_sql(&exp_row[golden_pos[k]]);
                assert!(
                    got_val.exact_eq(&want_val),
                    "{label}: table {tname} row {row_idx} col {}: got {}, want {}",
                    col.name,
                    got_val,
                    want_val
                );
                totals.cells_compared += 1;
            }
            row_idx += 1;
        }
        assert_eq!(row_idx, exp_rows.len(), "{label}: table {tname} row count");
        assert_eq!(row_idx as u64, gt["rowCount"].as_u64().unwrap());
        assert_eq!(reader.count().expect("count"), row_idx as u64);

        // Phase 2: Verify ALL VIRTUAL generated column cells via `SqliteDatabase::query`!
        if virtual_count > 0 {
            let escaped = tname.replace('"', "\"\"");
            let sql = format!("SELECT * FROM \"{escaped}\"");
            let qres = db
                .query(&sql)
                .unwrap_or_else(|e| panic!("{label}: SQL query `{sql}` failed: {e}"));
            assert_eq!(
                qres.rows.len(),
                exp_rows.len(),
                "{label}: SQL {tname} row count"
            );
            assert_eq!(
                qres.columns.len(),
                reader.columns().len(),
                "{label}: SQL {tname} column count"
            );
            for (ridx, (got_row, exp_row_val)) in
                qres.rows.iter().zip(exp_rows.iter()).enumerate()
            {
                let exp_row = exp_row_val.as_array().unwrap();
                for col in reader.columns() {
                    let want = golden_cell_to_sql(&exp_row[usize::from(rowid_first) + col.cid]);
                    let got = &got_row[col.cid];
                    assert!(
                        got.exact_eq(&want),
                        "{label}: SQL {tname} row {ridx} col {} (hidden={:?}): got {}, want {}",
                        col.name,
                        col.hidden,
                        got,
                        want
                    );
                    if col.hidden == ColumnHiddenKind::Virtual {
                        totals.virtual_verified += 1;
                        totals.cells_compared += 1;
                    }
                }
            }
        }

        // Verify TableReader::get(rowid) returns the exact same row!
        if let Some((rid, expected_row)) = sample_rowid {
            let got = reader
                .get(rid, Some(&scan_opts))
                .expect("get")
                .expect("row found");
            assert_eq!(got.len(), expected_row.len());
            for (a, b) in got.iter().zip(expected_row.iter()) {
                assert!(a.exact_eq(b));
            }
        }

        totals.tables += 1;
        totals.rows += row_idx;
    }

    // 6. EVERY SINGLE CELL of EVERY SINGLE INDEX
    for gi in exp_indexes {
        let iname = gi["name"].as_str().unwrap();
        let ix = db.index(iname).expect("index reader");
        let mut cur = ix.cursor().expect("index cursor");
        let exp_entries = gi["entries"].as_array().unwrap();
        let mut n = 0usize;
        let mut mid_entry: Option<Vec<SqlValue>> = None;

        while cur.next().expect("index next") {
            let got = cur.row().expect("index row");
            let exp = exp_entries[n].as_array().unwrap();
            assert_eq!(got.len(), exp.len(), "{label}: index {iname} entry {n} width");
            for (k, (g, e)) in got.iter().zip(exp.iter()).enumerate() {
                let want = golden_cell_to_sql(e);
                assert!(
                    g.exact_eq(&want),
                    "{label}: index {iname} entry {n} col {k}: got {g}, want {want}"
                );
                totals.cells_compared += 1;
            }
            if n == exp_entries.len() / 2 {
                mid_entry = Some(got);
            }
            n += 1;
        }
        assert_eq!(n, exp_entries.len(), "{label}: index {iname} entry count");
        assert_eq!(n as u64, gi["entryCount"].as_u64().unwrap());
        assert_eq!(ix.count().expect("index count"), n as u64);

        // Also exercise SearchKey + IndexEntryCursor::seek_key on a mid entry!
        if let Some(target) = mid_entry {
            if let Ok(ki) = ix.key_info(None) {
                let mut key = SearchKey::new(ki, &target, 0).expect("search key");
                let mut seek_cur = ix.cursor().expect("seek cursor");
                assert!(
                    seek_cur.seek_key(&mut key, SeekOp::Ge).expect("seek_key Ge"),
                    "{label}: index {iname} seek_key(Ge) should find mid entry"
                );
                let found = seek_cur.row().expect("found row");
                for (a, b) in found.iter().zip(target.iter()) {
                    assert!(
                        a.exact_eq(b),
                        "{label}: index {iname} seek_key mismatch: got {a}, want {b}"
                    );
                }
            }
        }

        totals.indexes += 1;
        totals.entries += n;
    }

    totals.fixtures += 1;
}

fn check_fixture(db_path: &Path, golden_path: &Path, wal_mode: WalMode, totals: &mut Totals) {
    let golden_text = std::fs::read_to_string(golden_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", golden_path.display()));
    let golden: JsonValue = serde_json::from_str(&golden_text).expect("valid golden JSON");

    let vfs = StdFsVfs::new();
    let db = open_database(
        &vfs,
        db_path.to_str().unwrap(),
        OpenOptions {
            wal: wal_mode,
            ..OpenOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("open_database failed for {}: {e}", db_path.display()));

    let label = format!(
        "{}{}",
        db_path.file_name().unwrap().to_string_lossy(),
        if wal_mode == WalMode::Ignore {
            " (wal:ignore)"
        } else {
            ""
        }
    );
    verify_database_against_golden(&db, &golden, &label, totals);
    db.close().expect("close");
    assert!(db.is_closed());
    assert!(matches!(db.table("sqlite_schema"), Err(SqliteError::DatabaseClosed)));
}

#[test]
fn phase1_exit_gate_all_25_fixtures_cell_by_cell() {
    let root = fixtures_dir();
    if !root.exists() {
        eprintln!("skipping fixture tests: {} not found", root.display());
        return;
    }
    let manifest_text = std::fs::read_to_string(root.join("manifest.json")).expect("manifest.json");
    let manifest: JsonValue = serde_json::from_str(&manifest_text).expect("valid manifest");
    let fixtures = manifest["fixtures"].as_array().unwrap();
    assert_eq!(fixtures.len(), 25);

    let mut totals = Totals::default();
    let mut main_only = Totals::default();

    for entry in fixtures {
        let name = entry["name"].as_str().unwrap();
        let db_path = root.join(format!("{name}.db"));
        let golden_path = root.join("goldens").join(format!("{name}.json"));
        check_fixture(&db_path, &golden_path, WalMode::Apply, &mut totals);

        let main_only_golden = root.join("goldens").join(format!("{name}.main-only.json"));
        if main_only_golden.exists() {
            check_fixture(&db_path, &main_only_golden, WalMode::Ignore, &mut main_only);
        }
    }

    assert_eq!(totals.fixtures, 25);
    assert_eq!(main_only.fixtures, 3);
    assert!(totals.tables >= 600, "expected >= 600 tables, got {}", totals.tables);
    assert!(totals.rows >= 120_000, "expected >= 120,000 rows, got {}", totals.rows);
    assert!(totals.indexes >= 600, "expected >= 600 indexes, got {}", totals.indexes);
    assert!(totals.entries >= 200_000, "expected >= 200,000 entries, got {}", totals.entries);

    assert_eq!(totals.virtual_verified, 960, "expected all 960 VIRTUAL generated cells verified");

    eprintln!(
        "Phase 1+2 Rust exit gate PASSED: {}/25 fixtures; {} tables, {} rows, {} indexes, {} entries, \
         {} cells compared (including {} VIRTUAL generated cells, 0 excluded!), {} catalog checks; \
         wal:ignore main-only: {} fixtures, {} tables, {} rows, {} indexes, {} entries, {} cells",
        totals.fixtures,
        totals.tables,
        totals.rows,
        totals.indexes,
        totals.entries,
        totals.cells_compared,
        totals.virtual_verified,
        totals.catalog_checks,
        main_only.fixtures,
        main_only.tables,
        main_only.rows,
        main_only.indexes,
        main_only.entries,
        main_only.cells_compared,
    );
}

#[test]
fn btree_forward_reverse_and_meter_halts() {
    let root = fixtures_dir();
    if !root.exists() {
        return;
    }
    let db_path = root.join("wal-uncheckpointed-p4096.db");
    let wal_path = root.join("wal-uncheckpointed-p4096.db-wal");
    let main_bytes = std::fs::read(&db_path).expect("read main");
    let wal_bytes = std::fs::read(&wal_path).expect("read wal");

    // 1. Forward vs reverse cursor walk + rowid seek parity
    let vfs = StdFsVfs::new();
    let snap = open_snapshot(&vfs, db_path.to_str().unwrap(), SnapshotOptions::default()).unwrap();
    let bt = xdb_sqlite::Btree::open(Box::new(snap), xdb_sqlite::BtreeOptions::default()).unwrap();
    let mut cur = bt.table_cursor(2).unwrap();
    let mut fwd = Vec::new();
    if cur.first().unwrap() {
        loop {
            fwd.push(cur.rowid().unwrap());
            if !cur.next().unwrap() {
                break;
            }
        }
    }
    let mut rev = Vec::new();
    if cur.last().unwrap() {
        loop {
            rev.push(cur.rowid().unwrap());
            if !cur.prev().unwrap() {
                break;
            }
        }
    }
    rev.reverse();
    assert_eq!(fwd, rev);
    for &rid in fwd.iter().step_by(17) {
        assert_eq!(cur.seek(rid).unwrap(), SeekResult::Exact);
        assert_eq!(cur.rowid().unwrap(), rid);
    }

    // 2. Step limit halts deterministically during full SqliteDatabase scan
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(400),
        max_memory_bytes: None,
        max_page_reads: None,
    }));
    let db = open_database_from_bytes(
        &main_bytes,
        Some(&wal_bytes),
        None,
        OpenOptions {
            meter: Some(meter.clone()),
            ..OpenOptions::default()
        },
    )
    .expect("open within 400 steps");
    let mut hit_limit = false;
    'outer: for t in db.tables() {
        let reader = db.table(&t.name).unwrap();
        let mut c = reader.cursor(None).unwrap();
        loop {
            match c.next() {
                Ok(true) => match c.row() {
                    Ok(_) => {}
                    Err(SqliteError::ResourceLimit(e)) if e.kind == LimitKind::Steps => {
                        hit_limit = true;
                        break 'outer;
                    }
                    Err(e) => panic!("unexpected error: {e}"),
                },
                Ok(false) => break,
                Err(SqliteError::ResourceLimit(e)) if e.kind == LimitKind::Steps => {
                    hit_limit = true;
                    break 'outer;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    }
    assert!(hit_limit, "expected step limit to halt metered scan");

    // 3. Tight memory limit rejects caching a 4096-byte page
    let mem_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: None,
        max_memory_bytes: Some(2048),
        max_page_reads: None,
    }));
    let err = open_database_from_bytes(
        &main_bytes,
        Some(&wal_bytes),
        None,
        OpenOptions {
            meter: Some(mem_meter),
            ..OpenOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SqliteError::ResourceLimit(e) if e.kind == LimitKind::Memory),
        "expected Memory limit, got {err}"
    );
}
