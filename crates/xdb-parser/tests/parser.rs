use xdb_core::{Affinity, BudgetMeter, LimitKind, ResourceLimits};
use xdb_parser::*;

fn opts() -> ParseOptions {
    ParseOptions::default()
}

fn xdb_opts() -> ParseOptions {
    ParseOptions {
        dialect: Dialect::Xdb,
        ..ParseOptions::default()
    }
}

#[test]
fn tokenizer_handles_bom_unicode_and_numbers() {
    // Leading UTF-8 BOM is whitespace (SPACE)
    let sql = "\u{FEFF}SELECT 1_000, 0x1a_2b, 3.14e-2, x'deadBEEF', \u{FF01}id";
    let toks = parser_tokens(sql, Dialect::Sqlite);
    let kinds: Vec<u16> = toks.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![
            tk::SELECT,
            tk::QNUMBER,
            tk::COMMA,
            tk::QNUMBER,
            tk::COMMA,
            tk::FLOAT,
            tk::COMMA,
            tk::BLOB,
            tk::COMMA,
            tk::ID,
        ]
    );
    assert_eq!(&sql[toks[9].start..toks[9].end], "\u{FF01}id");

    // NUL byte truncates input like SQLite C
    let nul_toks = parser_tokens("SELECT 1\0INVALID ***", Dialect::Sqlite);
    assert_eq!(nul_toks.len(), 2);
}

#[test]
fn parses_complex_select_and_windows() {
    let sql = "WITH RECURSIVE cte(x, y) AS MATERIALIZED (
        SELECT 1, 'a''b'
        UNION ALL
        SELECT x + 1, y || '!' FROM cte WHERE x < 10
    )
    SELECT x, sum(x) FILTER (WHERE x > 2) OVER w2 AS total
    FROM cte
    WINDOW w1 AS (PARTITION BY y ORDER BY x DESC),
           w2 AS (w1 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE TIES)
    ORDER BY x ASC NULLS LAST
    LIMIT 5 OFFSET 1;";
    let stmt = parse_statement(sql, &opts()).expect("should parse");
    let Statement::Select(sel) = stmt else {
        panic!("expected select");
    };
    let with = sel.with.expect("expected WITH");
    assert!(with.recursive);
    assert_eq!(with.ctes.len(), 1);
    assert_eq!(with.ctes[0].materialized, Some(true));
    assert!(sel.limit.is_some());
    assert_eq!(sel.order_by.as_ref().unwrap().len(), 1);
    assert_eq!(sel.order_by.as_ref().unwrap()[0].nulls, Some(NullsOrder::Last));
}

#[test]
fn parses_xdb_dialect_extensions() {
    // :: cast, GROUP BY ALL, ORDER BY ALL, * EXCLUDE/REPLACE, FROM-first, ILIKE
    let sql = "FROM users
        SELECT id::TEXT, * EXCLUDE (secret) REPLACE (lower(name) AS name)
        WHERE email ILIKE '%@example.com'
        GROUP BY ALL
        ORDER BY ALL DESC NULLS FIRST";
    let stmt = parse_statement(sql, &xdb_opts()).expect("xdb dialect parses");
    let Statement::Select(sel) = stmt else {
        panic!("expected select");
    };
    assert!(sel.order_by_all.is_some());
    let SelectBody::Core(core) = sel.body else {
        panic!("expected selectCore");
    };
    assert!(core.from_first);
    assert!(core.group_by_all);
    assert_eq!(core.columns.len(), 2);
    assert!(matches!(
        &core.columns[0],
        ResultColumn::Expr(ResultExpr {
            expr: Expr::Cast { .. },
            ..
        })
    ));
    assert!(matches!(
        &core.columns[1],
        ResultColumn::Star(ResultStar {
            exclude: Some(_),
            replace: Some(_),
            ..
        })
    ));
    assert!(matches!(
        core.where_clause,
        Some(Expr::Like { ilike: true, .. })
    ));

    // In SQLite mode, ILIKE is just a syntax error or alias; in XDB mode, `SELECT a ilike FROM t`
    // keeps `ilike` as an alias (followed by FROM, not an expression starter).
    let alias_stmt = parse_statement("SELECT a ilike FROM t", &xdb_opts()).unwrap();
    let Statement::Select(sel2) = alias_stmt else {
        panic!("expected select");
    };
    let SelectBody::Core(core2) = sel2.body else {
        panic!("expected core");
    };
    match &core2.columns[0] {
        ResultColumn::Expr(re) => {
            assert_eq!(re.alias.as_ref().unwrap().name, "ilike");
        }
        _ => panic!("expected expr"),
    }
}

#[test]
fn build_table_and_index_match_sqlite_build_c() {
    let sql = "CREATE TABLE items (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        code TEXT UNIQUE COLLATE NOCASE NOT NULL,
        price REAL DEFAULT 0.0,
        qty INT DEFAULT (1 + 2),
        total REAL GENERATED ALWAYS AS (price * qty) VIRTUAL,
        tag TEXT GENERATED ALWAYS AS (code || '!') STORED,
        CHECK (qty >= 0)
    ) STRICT";
    let stmt = parse_statement(sql, &opts()).unwrap();
    let Statement::CreateTable(ct) = stmt else {
        panic!("expected createTable");
    };
    let def = build_table(&ct, sql, &BuildOptions::default()).unwrap();
    assert_eq!(def.name, "items");
    assert_eq!(def.ipk_column, Some(0));
    assert!(def.autoincrement);
    assert!(def.strict);
    assert!(!def.without_rowid);
    assert!(def.has_virtual_columns);
    assert!(def.has_stored_columns);
    // record_columns skips VIRTUAL column (cid 4)
    assert_eq!(def.record_columns, vec![0, 1, 2, 3, 5]);
    // UNIQUE COLLATE NOCASE updated the autoindex's collation!
    assert_eq!(def.indexes.len(), 1);
    assert_eq!(def.indexes[0].columns[0].collation, "NOCASE");

    // Partial expression index
    let idx_sql = "CREATE INDEX idx_items_code ON items(lower(code) DESC, price) WHERE qty > 0";
    let idx_stmt = parse_statement(idx_sql, &opts()).unwrap();
    let Statement::CreateIndex(ci) = idx_stmt else {
        panic!("expected createIndex");
    };
    let idx = build_index(&ci, &def, &BuildOptions::default()).unwrap();
    assert_eq!(idx.key_count, 2);
    assert!(idx.partial);
    assert_eq!(idx.columns.len(), 3); // 2 keys + rowid
    assert_eq!(idx.columns[0].cid, -2);
    assert!(idx.columns[0].desc);
    assert_eq!(idx.columns[1].cid, 2);
    assert_eq!(idx.columns[2].cid, -1);

    // WITHOUT ROWID table
    let wr_sql = "CREATE TABLE kv (k TEXT, v BLOB, meta ANY, PRIMARY KEY(k DESC)) WITHOUT ROWID, STRICT";
    let Statement::CreateTable(wr_ct) = parse_statement(wr_sql, &opts()).unwrap() else {
        panic!();
    };
    let wr = build_table(&wr_ct, wr_sql, &BuildOptions::default()).unwrap();
    assert!(wr.without_rowid);
    assert!(wr.strict);
    assert_eq!(wr.ipk_column, None);
    assert!(wr.columns[0].not_null);
    assert_eq!(wr.columns[2].affinity, Affinity::Blob); // STRICT ANY -> BLOB
    assert_eq!(wr.record_columns, vec![0, 1, 2]);
    assert!(wr.indexes[0].without_rowid_primary_key);
    assert!(wr.indexes[0].columns[0].desc);
}

#[test]
fn reports_exact_sqlite_errors_and_limits() {
    let err = parse_statement("SELECT FROM", &opts()).unwrap_err();
    assert_eq!(err.message, "near \"FROM\": syntax error");

    let err = parse_statement("SELECT 1 +", &opts()).unwrap_err();
    assert_eq!(err.message, "incomplete input");

    let err = parse_statement("SELECT 1_2__3", &opts()).unwrap_err();
    assert!(err.message.starts_with("unrecognized token:"));

    // Expression depth limit
    let deep = format!("SELECT {}1{}", "(".repeat(25), ")".repeat(25));
    let mut small = opts();
    small.max_expr_depth = 10;
    // Parentheses alone don't add height, unary minus does:
    let deep_minus = format!("SELECT {}1", "- ".repeat(15));
    let err = parse_statement(&deep_minus, &small).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::Limit);
    assert_eq!(err.message, "Expression tree is too large (maximum depth 10)");
    assert!(parse_statement(&deep, &small).is_ok());

    // Build errors
    let bad_wr = "CREATE TABLE t(a INT) WITHOUT ROWID";
    let Statement::CreateTable(ct) = parse_statement(bad_wr, &opts()).unwrap() else {
        panic!();
    };
    let err = build_table(&ct, bad_wr, &BuildOptions::default()).unwrap_err();
    assert_eq!(err.message, "PRIMARY KEY missing on table t");

    // Sandbox BudgetMeter stops parsing deterministically
    let meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(5),
        max_memory_bytes: None,
        max_page_reads: None,
    });
    let err = parse_statement_metered("SELECT 1 + 2 + 3 + 4 + 5", &opts(), &meter).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err.resource_limit.unwrap().kind, LimitKind::Steps);
}

#[test]
fn parses_triggers_views_vtabs_and_expressions() {
    let trig = "CREATE TRIGGER tr AFTER UPDATE OF a, b ON t FOR EACH ROW WHEN NEW.a > 0 BEGIN INSERT INTO log VALUES(NEW.a); UPDATE t SET c = 1 WHERE id = OLD.id; END;";
    let Statement::CreateTrigger(ct) = parse_statement(trig, &opts()).unwrap() else {
        panic!();
    };
    assert_eq!(ct.body.len(), 2);
    assert_eq!(ct.update_of.as_ref().unwrap().len(), 2);

    let vtab = "CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize='porter unicode61')";
    let Statement::CreateVirtualTable(vt) = parse_statement(vtab, &opts()).unwrap() else {
        panic!();
    };
    assert_eq!(vt.module.name, "fts5");
    assert_eq!(vt.args.as_ref().unwrap().len(), 3);

    let expr = parse_expression("coalesce(a, b) + 1", &opts()).unwrap();
    assert!(matches!(
        expr,
        Expr::Binary {
            op: BinaryOp::Add,
            ..
        }
    ));
}
