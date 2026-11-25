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

#[test]
fn parser_hardening_utf8_depth_and_expression_boundaries() {
    // Multi-byte UTF-8 after type quotes and in table names must never panic on byte slicing.
    let ct_sql = "CREATE TABLE aaaaaaé (a [int] é, b TEXT ALWAYS, c \"varchar\" é)";
    let Statement::CreateTable(ct) = parse_statement(ct_sql, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let tbl = build_table(&ct, ct_sql, &BuildOptions::default()).unwrap();
    assert_eq!(tbl.columns[0].decl_type, "int");
    assert_eq!(tbl.columns[1].decl_type, "TEXT ALWAYS");
    assert_eq!(tbl.columns[2].decl_type, "varchar");

    let idx_sql = "CREATE INDEX idx_u ON aaaaaaé(a)";
    let Statement::CreateIndex(ci) = parse_statement(idx_sql, &opts()).unwrap() else {
        panic!("expected CreateIndex");
    };
    let idx = build_index(&ci, &tbl, &BuildOptions::default()).unwrap();
    assert_eq!(idx.table, "aaaaaaé");

    // max_expr_depth must apply to CAST, XDB :: casts, AND chains, and empty IN ().
    let mut tight = xdb_opts();
    tight.max_expr_depth = 5;
    let deep_dcolon = format!("SELECT 1{}", "::int".repeat(12));
    assert_eq!(
        parse_statement(&deep_dcolon, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_cast = format!("SELECT {}1{}", "CAST(".repeat(10), " AS INT)".repeat(10));
    assert_eq!(
        parse_statement(&deep_cast, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_and = format!("SELECT 1{}", " AND 1".repeat(12));
    assert_eq!(
        parse_statement(&deep_and, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_in = format!("SELECT 1{}", " IN ()".repeat(12));
    assert_eq!(
        parse_statement(&deep_in, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );

    // parse_expression must reject trailing statements or SELECT clauses smuggled via `)`.
    assert!(parse_expression("1); DROP TABLE x; --", &opts()).is_err());
    assert!(parse_expression("1) ORDER BY (1", &opts()).is_err());
    assert!(parse_expression("1) LIMIT (1", &opts()).is_err());

    // max_parser_depth < 50 must actually be enforced (not silently raised to 50).
    let mut tiny_stack = opts();
    tiny_stack.max_parser_depth = 10;
    let nested_parens = format!("SELECT {}1{}", "(".repeat(20), ")".repeat(20));
    assert_eq!(
        parse_statement(&nested_parens, &tiny_stack).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );

    // Dialect::Xdb tokenizer must classify ILIKE as tk::LIKE_KW.
    let ilike_toks = parser_tokens("a ILIKE b", Dialect::Xdb);
    assert_eq!(ilike_toks.len(), 3);
    assert_eq!(ilike_toks[1].kind, tk::LIKE_KW);
}

#[test]
fn parser_pass2_comment_metering_and_natural_join_rejection() {
    // Huge run of leading comments must be metered in parse_statement_at_metered.
    let meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(25),
        max_memory_bytes: None,
        max_page_reads: None,
    });
    let comments = "/* x */ ".repeat(200);
    let err = parse_statement_at_metered(&comments, 0, &opts(), &meter).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err.resource_limit.unwrap().kind, LimitKind::Steps);

    // Trailing semicolons/comments after a statement must also be metered in parse_statement_metered.
    let meter2 = BudgetMeter::new(ResourceLimits {
        max_steps: Some(30),
        max_memory_bytes: None,
        max_page_reads: None,
    });
    let trailing = format!("SELECT 1{}", "; /* x */ ".repeat(200));
    let err2 = parse_statement_metered(&trailing, &opts(), &meter2).unwrap_err();
    assert_eq!(err2.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err2.resource_limit.unwrap().kind, LimitKind::Steps);

    // SQLite forbids combining NATURAL JOIN with ON or USING.
    assert!(parse_statement("SELECT * FROM a NATURAL JOIN b ON a.id = b.id", &opts()).is_err());
    assert!(parse_statement("SELECT * FROM a NATURAL LEFT JOIN b USING (id)", &opts()).is_err());
    assert!(parse_statement("SELECT * FROM a NATURAL JOIN b", &opts()).is_ok());
}

#[test]
fn parser_pass3_xdb_var_cast_depth_and_first_error() {
    // 1. XDB dialect `:x::int`, `$x::text`, `@x::real` must split variable from `::` cast!
    for sql in [
        "SELECT :x::int",
        "SELECT $x::text",
        "SELECT @x::real",
    ] {
        let stmt = parse_statement(sql, &xdb_opts()).unwrap();
        let Statement::Select(sel) = stmt else {
            panic!("expected Select");
        };
        let SelectBody::Core(core) = sel.body else {
            panic!("expected Core");
        };
        let ResultColumn::Expr(ref re) = core.columns[0] else {
            panic!("expected Expr");
        };
        assert!(
            matches!(
                &re.expr,
                Expr::Cast { operand, .. } if matches!(operand.as_ref(), Expr::Variable(_))
            ),
            "expected Cast(Variable) for `{sql}`, got {:?}",
            re.expr
        );
    }
    // Standard SQLite dialect still treats `:a::b` as a single variable name (TCL-style).
    let stmt = parse_statement("SELECT :a::b", &opts()).unwrap();
    let Statement::Select(sel) = stmt else {
        panic!("expected Select");
    };
    let SelectBody::Core(core) = sel.body else {
        panic!("expected Core");
    };
    let ResultColumn::Expr(ref re) = core.columns[0] else {
        panic!("expected Expr");
    };
    assert!(matches!(&re.expr, Expr::Variable(v) if v.text == ":a::b"));

    // 2. max_expr_depth applies to vectors, unary chains, FILTER, window clauses, and * REPLACE.
    let mut tight = xdb_opts();
    tight.max_expr_depth = 4;
    let deep_vec = format!("SELECT {}1{}", "(1, ".repeat(8), ")".repeat(8));
    assert_eq!(
        parse_statement(&deep_vec, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_unary = format!("SELECT {}1", "- ".repeat(10));
    assert_eq!(
        parse_statement(&deep_unary, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_filter = format!("SELECT count(*) FILTER (WHERE {}1)", "- ".repeat(10));
    assert_eq!(
        parse_statement(&deep_filter, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_win = format!("SELECT sum(x) OVER (PARTITION BY {}1)", "- ".repeat(10));
    assert_eq!(
        parse_statement(&deep_win, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );
    let deep_replace = format!("SELECT * REPLACE ({}1 AS a) FROM t", "- ".repeat(10));
    assert_eq!(
        parse_statement(&deep_replace, &tight).unwrap_err().kind,
        SqlParseErrorKind::Limit
    );

    // 3. parse_statement_at preserves the first syntax error on invalid statements.
    let err = parse_statement_at("SELECT FROM; SELECT 1", 0, &opts()).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::Syntax);
    assert!(
        err.message.contains("FROM"),
        "expected first syntax error near FROM, got: {}",
        err.message
    );
}

#[test]
fn parser_pass4_unary_plus_neg_fold_depth_and_temp_self_ref() {
    use xdb_parser::{build_table, BuildOptions, UnaryOp};

    // 1. `- + 9223372036854775808` and `- + 0x8000000000000000` rewrite inner `TK_UPLUS` in place
    //    to `Unary(Neg, Number(...))` (matching SQLite 3.54.1: `- + 9223372036854775808` -> i64::MIN
    //    and `- + 0x8000000000000000` -> hex literal too big).
    for sql in [
        "SELECT - + 9223372036854775808",
        "SELECT - + 0x8000000000000000",
    ] {
        let Statement::Select(sel) = parse_statement(sql, &opts()).unwrap() else {
            panic!("expected Select");
        };
        let SelectBody::Core(core) = sel.body else {
            panic!("expected Core");
        };
        let ResultColumn::Expr(ref re) = core.columns[0] else {
            panic!("expected Expr");
        };
        assert!(
            matches!(
                &re.expr,
                Expr::Unary {
                    op: UnaryOp::Neg,
                    operand,
                    ..
                } if matches!(operand.as_ref(), Expr::Number(_))
            ),
            "expected in-place Neg(Number) for `{sql}`, got {:?}",
            re.expr
        );
    }

    // 2. max_expr_depth enforced inside VALUES, WITH CTEs, named WINDOW clauses, and FROM subqueries.
    let mut tight = opts();
    tight.max_expr_depth = 4;
    let deep = format!("{}1", "- ".repeat(10));
    for sql in [
        format!("VALUES ({deep})"),
        format!("WITH c AS (SELECT {deep}) SELECT * FROM c"),
        format!("SELECT 1 FROM t WINDOW w AS (PARTITION BY {deep})"),
        format!("SELECT * FROM (SELECT {deep})"),
        format!("SELECT * FROM a JOIN b ON {deep}"),
    ] {
        assert_eq!(
            parse_statement(&sql, &tight).unwrap_err().kind,
            SqlParseErrorKind::Limit,
            "expected max_expr_depth Limit for `{sql}`"
        );
    }

    // 3. `CREATE TEMP TABLE t1(a INT CHECK (temp.t1.a > 0))` resolves self-ref with `temp.` schema!
    let ddl = "CREATE TEMP TABLE t1(a INT CHECK (temp.t1.a > 0))";
    let Statement::CreateTable(ct) = parse_statement(ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let built = build_table(&ct, ddl, &BuildOptions::default()).unwrap();
    assert_eq!(built.name, "t1");
}

#[test]
fn pass5_fold_expr_linear_trigger_parity_meter_and_offset() {
    use xdb_core::{BudgetMeter, LimitKind, ResourceLimits};
    use xdb_parser::{
        build_table, parse_statement_at, parse_statement_metered, BuildOptions, ParseOptions,
    };

    // 1. 40-deep nested `(-((...) ISNULL)) ISNULL` CHECK expression folds in O(N) (not O(2^40)!).
    let mut nested = "a".to_string();
    for _ in 0..40 {
        nested = format!("(-(({nested}) ISNULL))");
    }
    let ddl = format!("CREATE TABLE t_fold (a INT CHECK ({nested} ISNULL))");
    let Statement::CreateTable(ct) = parse_statement(&ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let built = build_table(&ct, &ddl, &BuildOptions::default()).unwrap();
    assert_eq!(built.name, "t_fold");

    // 2. Large AND-chain CHECK validates in linear time.
    let ands = vec!["a = 1"; 400].join(" AND ");
    let ddl_and = format!("CREATE TABLE t_and (a INT CHECK ({ands}))");
    let Statement::CreateTable(ct_and) = parse_statement(&ddl_and, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    assert_eq!(
        build_table(&ct_and, &ddl_and, &BuildOptions::default())
            .unwrap()
            .name,
        "t_and"
    );

    // 3. Parser memory metering enforces `max_memory_bytes` on large ASTs and releases cleanly.
    let meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000_000),
        max_memory_bytes: Some(8 * 1024),
        max_page_reads: None,
    });
    let huge_vals = format!("VALUES {}", vec!["(1,2,3,4)"; 1500].join(","));
    let err = parse_statement_metered(&huge_vals, &opts(), &meter).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err.resource_limit.map(|l| l.kind), Some(LimitKind::Memory));
    assert_eq!(meter.usage().memory_bytes, 0);

    // 4. CREATE TRIGGER parity: reject RETURNING, bind variables, qualified DML, and > max_columns.
    for bad in [
        "CREATE TRIGGER tr1 AFTER INSERT ON t BEGIN INSERT INTO t2 VALUES(1) RETURNING *; END",
        "CREATE TRIGGER tr2 AFTER INSERT ON t BEGIN INSERT INTO t2 VALUES(?1); END",
        "CREATE TRIGGER tr3 AFTER INSERT ON t BEGIN DELETE FROM main.t2 WHERE id = 1; END",
    ] {
        assert!(
            parse_statement(bad, &opts()).is_err(),
            "expected trigger error for `{bad}`"
        );
    }
    // Qualified DML inside trigger is permitted in schema mode (for legacy sqlite_schema reload)
    let schema_opts = ParseOptions {
        schema: true,
        ..opts()
    };
    assert!(parse_statement(
        "CREATE TRIGGER tr3 AFTER INSERT ON t BEGIN DELETE FROM main.t2 WHERE id = 1; END",
        &schema_opts
    )
    .is_ok());

    let mut tight_cols = opts();
    tight_cols.max_columns = 2;
    let tr_err = parse_statement(
        "CREATE TRIGGER tr4 AFTER INSERT ON t BEGIN UPDATE t2 SET a=1, b=2, c=3; END",
        &tight_cols,
    )
    .unwrap_err();
    assert!(
        tr_err.message.contains("too many columns"),
        "expected too many columns error, got: {}",
        tr_err.message
    );

    // 5. `stored_type` strips trailing `always` from generated-column type names (`build.c:1573`).
    let gen_ddl = "CREATE TABLE tg (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED)";
    let Statement::CreateTable(ct_g) = parse_statement(gen_ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let built_g = build_table(&ct_g, gen_ddl, &BuildOptions::default()).unwrap();
    assert_eq!(built_g.columns[1].decl_type, "INT");

    // 6. `parse_statement_at` rejects mid-UTF-8-char offset cleanly and advances `end` to EOF.
    let uni = "SELECT 'é';   ";
    let mid_utf8 = uni.find('é').unwrap() + 1;
    assert_eq!(
        parse_statement_at(uni, mid_utf8, &opts()).unwrap_err().kind,
        SqlParseErrorKind::Syntax
    );
    let eof = parse_statement_at(uni, uni.len() - 3, &opts()).unwrap();
    assert!(eof.statement.is_none());
    assert_eq!(eof.end, uni.len());
}

#[test]
fn pass6_nul_split_wrapped_expr_metering_schema_windows_and_inline_stack() {
    use xdb_core::{BudgetMeter, LimitKind, ResourceLimits};

    // 1. `split_statements` stops cleanly at embedded NUL byte (never includes NUL or trailing text).
    let with_nul = "SELECT 1\0; DROP TABLE x";
    let slices = split_statements(with_nul);
    assert_eq!(slices.len(), 1);
    assert_eq!(&with_nul[slices[0].0..slices[0].1], "SELECT 1");

    // 2. `parse_expression_metered` flushes step/memory meters & reports NUL-clamped spans.
    let expr_meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(100_000),
        max_memory_bytes: Some(1024 * 1024),
        max_page_reads: None,
    });
    assert!(parse_expression_metered("1 + 2", &opts(), &expr_meter).is_ok());
    assert!(expr_meter.usage().steps > 0, "parse_expression_metered must flush steps");
    assert_eq!(expr_meter.usage().memory_bytes, 0);
    let nul_expr_err = parse_expression("1 + \0 trailing", &opts()).unwrap_err();
    assert!(nul_expr_err.end.unwrap_or(usize::MAX) <= 4);

    // 3. Large string & blob literals are metered BEFORE/DURING reduction against `max_memory_bytes`.
    let tight = BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000_000),
        max_memory_bytes: Some(4 * 1024),
        max_page_reads: None,
    });
    let big_str_sql = format!("SELECT '{}'", "a".repeat(16 * 1024));
    let err_s = parse_statement_metered(&big_str_sql, &opts(), &tight).unwrap_err();
    assert_eq!(err_s.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err_s.resource_limit.map(|l| l.kind), Some(LimitKind::Memory));
    assert_eq!(tight.usage().memory_bytes, 0);

    let tight2 = BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000_000),
        max_memory_bytes: Some(4 * 1024),
        max_page_reads: None,
    });
    let big_blob_sql = format!("SELECT X'{}'", "aa".repeat(8 * 1024));
    let err_b = parse_statement_metered(&big_blob_sql, &opts(), &tight2).unwrap_err();
    assert_eq!(err_b.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(err_b.resource_limit.map(|l| l.kind), Some(LimitKind::Memory));
    assert_eq!(tight2.usage().memory_bytes, 0);

    // 4. `build_table` & `build_index` reject window functions (`OVER (...)`) and `CURRENT_*`
    //    in GENERATED columns, index expressions, and partial-index WHERE clauses.
    let bad_gen_win = "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (sum(a) OVER (ORDER BY a)) VIRTUAL)";
    let Statement::CreateTable(ct1) = parse_statement(bad_gen_win, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    assert!(build_table(&ct1, bad_gen_win, &BuildOptions::default()).is_err());

    let bad_gen_ct = "CREATE TABLE t (a INT, b TEXT GENERATED ALWAYS AS (CURRENT_TIMESTAMP) STORED)";
    let Statement::CreateTable(ct2) = parse_statement(bad_gen_ct, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    assert!(build_table(&ct2, bad_gen_ct, &BuildOptions::default()).is_err());

    let base_ddl = "CREATE TABLE t (a INT, b INT)";
    let Statement::CreateTable(ct_base) = parse_statement(base_ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let base_table = build_table(&ct_base, base_ddl, &BuildOptions::default()).unwrap();

    for bad_idx in [
        "CREATE INDEX ix1 ON t(sum(a) OVER (ORDER BY a))",
        "CREATE INDEX ix2 ON t(a) WHERE (sum(a) OVER (ORDER BY a)) > 0",
        "CREATE INDEX ix3 ON t(CURRENT_TIMESTAMP)",
        "CREATE INDEX ix4 ON t(a) WHERE a > CURRENT_DATE",
    ] {
        let Statement::CreateIndex(ci) = parse_statement(bad_idx, &opts()).unwrap() else {
            panic!("expected CreateIndex");
        };
        assert!(
            build_index(&ci, &base_table, &BuildOptions::default()).is_err(),
            "expected build_index error for `{bad_idx}`"
        );
    }

    // 5. Trigger `RETURNING` error reports unified `SqlParseErrorKind::Syntax`.
    let trig_ret_err = parse_statement(
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t VALUES(1) RETURNING *; END",
        &opts(),
    )
    .unwrap_err();
    assert_eq!(trig_ret_err.kind, SqlParseErrorKind::Syntax);

    // 6. Fast `dequote` (no doubled quotes vs doubled quotes) & inline stack spill (> 32 depth).
    assert_eq!(dequote("'hello_world'"), "hello_world");
    assert_eq!(dequote("'it''s ''ok'''"), "it's 'ok'");
    assert_eq!(dequote("[col]"), "col");
    let deep = format!("SELECT {}1{}", "(".repeat(45), ")".repeat(45));
    assert!(parse_statement(&deep, &opts()).is_ok());
}

#[test]
fn pass7_parser_dos_metering_uplus_schema_and_pooled_stacks() {
    use xdb_core::{BudgetMeter, ResourceLimits};

    // 1. Constraint-name clone bomb (`64 KiB` name cloned across many constraints) halted by memory meter.
    let big_name = "c".repeat(65_536);
    let mut bomb_sql = format!("CREATE TABLE t (a INT CONSTRAINT {big_name} ");
    for _ in 0..32 {
        bomb_sql.push_str("NOT NULL ");
    }
    bomb_sql.push(')');
    let tight_mem = BudgetMeter::new(ResourceLimits {
        max_steps: Some(10_000_000),
        max_memory_bytes: Some(256 * 1024),
        max_page_reads: None,
    });
    let err = parse_statement_metered(&bomb_sql, &opts(), &tight_mem).unwrap_err();
    assert_eq!(err.kind, SqlParseErrorKind::ResourceLimit);
    assert_eq!(tight_mem.usage().memory_bytes, 0);

    // 2. Huge comment inside `SELECT` / `CHECK` / `DEFAULT` / `TypeName` slices halted by memory meter.
    let big_comment = format!("/*{}*/", "x".repeat(200_000));
    for slice_sql in [
        format!("SELECT 1 {big_comment} + 2"),
        format!("CREATE TABLE t (a INT CHECK (a {big_comment} > 0))"),
        format!("CREATE TABLE t (a INT DEFAULT (1 {big_comment} + 2))"),
        format!("CREATE TABLE t (a INT {big_comment} (10))"),
    ] {
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(10_000_000),
            max_memory_bytes: Some(64 * 1024),
            max_page_reads: None,
        });
        let e = parse_statement_metered(&slice_sql, &opts(), &m).unwrap_err();
        assert_eq!(
            e.kind,
            SqlParseErrorKind::ResourceLimit,
            "expected memory limit on `{}`",
            &slice_sql[..32]
        );
        assert_eq!(m.usage().memory_bytes, 0);
    }

    // 3. `VtabArgExtend` is O(N) (not O(N^2)) and respects memory meter.
    let mut vtab_sql = String::from("CREATE VIRTUAL TABLE vt USING mod(");
    for _ in 0..4000 {
        vtab_sql.push_str("tok ");
    }
    vtab_sql.push(')');
    let Statement::CreateVirtualTable(vt) = parse_statement(&vtab_sql, &opts()).unwrap() else {
        panic!("expected CreateVirtualTable");
    };
    assert_eq!(vt.args.as_ref().unwrap().len(), 1);
    let m_vtab = BudgetMeter::new(ResourceLimits {
        max_steps: Some(10_000_000),
        max_memory_bytes: Some(4096),
        max_page_reads: None,
    });
    assert_eq!(
        parse_statement_metered(&vtab_sql, &opts(), &m_vtab)
            .unwrap_err()
            .kind,
        SqlParseErrorKind::ResourceLimit
    );
    assert_eq!(m_vtab.usage().memory_bytes, 0);

    // 4. Huge `tk::QNUMBER` pre-charged against memory meter before `dequote_number`.
    let big_qnum = format!("SELECT 1{}", "_0".repeat(40_000));
    let m_qn = BudgetMeter::new(ResourceLimits {
        max_steps: Some(10_000_000),
        max_memory_bytes: Some(8192),
        max_page_reads: None,
    });
    assert_eq!(
        parse_statement_metered(&big_qnum, &opts(), &m_qn)
            .unwrap_err()
            .kind,
        SqlParseErrorKind::ResourceLimit
    );
    assert_eq!(m_qn.usage().memory_bytes, 0);

    // 5. Huge comment charges proportional steps (`>= 1000` steps for 100 KiB comment).
    let step_meter = BudgetMeter::new(ResourceLimits {
        max_steps: Some(500),
        max_memory_bytes: Some(1024 * 1024),
        max_page_reads: None,
    });
    let big_ws_sql = format!("/*{}*/ SELECT 1", "z".repeat(100_000));
    assert_eq!(
        parse_statement_metered(&big_ws_sql, &opts(), &step_meter)
            .unwrap_err()
            .kind,
        SqlParseErrorKind::ResourceLimit
    );

    // 6. 500 `- +` pairs stay within depth 600 (in-place `TK_UPLUS` rewrite -> height 501).
    let mut alt_uplus = String::from("SELECT ");
    for _ in 0..500 {
        alt_uplus.push_str("- + ");
    }
    alt_uplus.push('1');
    let mut d_opts = opts();
    d_opts.max_expr_depth = 600;
    assert!(parse_statement(&alt_uplus, &d_opts).is_ok());

    // 7. `build_index` recognizes `temp.tt1.a` in partial index `WHERE` on `CREATE TEMP TABLE tt1(a)`,
    //    and rejects `.` in index expressions as well as `FILTER`/`ORDER BY`/`*`/`DISTINCT`.
    let temp_ddl = "CREATE TEMP TABLE tt1 (a INT, b INT)";
    let Statement::CreateTable(ct_temp) = parse_statement(temp_ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let temp_def = build_table(&ct_temp, temp_ddl, &BuildOptions::default()).unwrap();
    let idx_temp_sql = "CREATE INDEX ix_tt1 ON tt1(a) WHERE temp.tt1.a > 0";
    let Statement::CreateIndex(ci_temp) = parse_statement(idx_temp_sql, &opts()).unwrap() else {
        panic!("expected CreateIndex");
    };
    assert!(build_index(&ci_temp, &temp_def, &BuildOptions::default()).is_ok());
    let idx_dot_sql = "CREATE INDEX ix_bad ON tt1(temp.tt1.a)";
    let Statement::CreateIndex(ci_dot) = parse_statement(idx_dot_sql, &opts()).unwrap() else {
        panic!("expected CreateIndex");
    };
    assert!(build_index(&ci_dot, &temp_def, &BuildOptions::default()).is_err());

    for bad_gen in [
        "CREATE TABLE t (a INT, g INT GENERATED ALWAYS AS (count(*)) VIRTUAL)",
        "CREATE TABLE t (a INT, g INT GENERATED ALWAYS AS (sum(DISTINCT a)) VIRTUAL)",
        "CREATE TABLE t (a INT, g INT GENERATED ALWAYS AS (sum(a) FILTER (WHERE a > 0)) VIRTUAL)",
        "CREATE TABLE t (a INT, g INT GENERATED ALWAYS AS (group_concat(a ORDER BY a)) VIRTUAL)",
    ] {
        let Statement::CreateTable(ct) = parse_statement(bad_gen, &opts()).unwrap() else {
            panic!("expected CreateTable");
        };
        assert!(
            build_table(&ct, bad_gen, &BuildOptions::default()).is_err(),
            "expected build_table error for `{bad_gen}`"
        );
    }

    // 8. Embedded NUL clamps error spans to `<= sql_end(sql)` in `parse_statement` & `parse_expression`.
    let nul_stmt = "SELECT 1; SELECT 2\0trailing garbage";
    let e_stmt = parse_statement(nul_stmt, &opts()).unwrap_err();
    assert!(e_stmt.end.unwrap_or(0) <= 18);
    let nul_expr = "1 + 2; 3\0trailing";
    let e_expr = parse_expression(nul_expr, &opts()).unwrap_err();
    assert!(e_expr.end.unwrap_or(0) <= 8);

    // 9. Thread-local `ParserStacks` reuse & deep spill (> 64 depth).
    for i in 0..100 {
        let q = format!("SELECT {i}, 'v_{i}' FROM t WHERE id = {i}");
        assert!(parse_statement(&q, &opts()).is_ok());
    }
    let deep80 = format!("SELECT {}1{}", "(".repeat(80), ")".repeat(80));
    assert!(parse_statement(&deep80, &opts()).is_ok());
    assert!(parse_statement("SELECT 42", &opts()).is_ok());
}

#[test]
fn pass8_join_limit_compound_cap_qnumber_linear_gen_loop_and_raise() {
    // 1. 64-table join succeeds; 65-table join fails with SQLite's exact limit message.
    let join64 = format!(
        "SELECT 1 FROM {}",
        (0..64).map(|i| format!("t{i}")).collect::<Vec<_>>().join(", ")
    );
    assert!(parse_statement(&join64, &opts()).is_ok());
    let join65 = format!(
        "SELECT 1 FROM {}",
        (0..65).map(|i| format!("t{i}")).collect::<Vec<_>>().join(", ")
    );
    let e65 = parse_statement(&join65, &opts()).unwrap_err();
    assert!(
        e65.message.contains("at most 64 tables in a join"),
        "expected 64-table join error, got: {}",
        e65.message
    );

    // 2. Deeply nested parenthesized FROM groups bounded by `max_expr_depth`.
    let deep_from = format!("SELECT 1 FROM {}t{}", "(".repeat(1200), ")".repeat(1200));
    assert!(parse_statement(&deep_from, &opts()).is_err());

    // 3. Trailing-VALUES compound SELECT respects SQLite's 600-term allowance while enforcing
    //    the hard cap `max_compound.max(1000)` to prevent stack overflow on drop!
    let vals_600 = (0..600)
        .map(|_| "VALUES(1)")
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    assert!(parse_statement(&vals_600, &opts()).is_ok());
    let vals_1200 = (0..1200)
        .map(|_| "VALUES(1)")
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let e_cmp = parse_statement(&vals_1200, &opts()).unwrap_err();
    assert!(
        e_cmp.message.contains("too many terms in compound SELECT"),
        "expected compound limit error, got: {}",
        e_cmp.message
    );

    // 4. Malformed numeric literal with 50,000 underscores fails in linear time (no O(N^2) churn).
    let bad_qnum = format!("SELECT 1{}2", "_".repeat(50_000));
    assert!(parse_statement(&bad_qnum, &opts()).is_err());

    // 5. Generated column cycle rejected cleanly at `build_table`.
    for cyclic in [
        "CREATE TABLE c1 (a INT GENERATED ALWAYS AS (b) VIRTUAL, b INT GENERATED ALWAYS AS (a) VIRTUAL)",
        "CREATE TABLE c2 (a INT GENERATED ALWAYS AS (a + 1) STORED)",
        "CREATE TABLE c3 (a INT GENERATED ALWAYS AS (b) VIRTUAL, b INT GENERATED ALWAYS AS (c) VIRTUAL, c INT GENERATED ALWAYS AS (a) VIRTUAL)",
    ] {
        let Statement::CreateTable(ct) = parse_statement(cyclic, &opts()).unwrap() else {
            panic!("expected CreateTable");
        };
        let err = build_table(&ct, cyclic, &BuildOptions::default()).unwrap_err();
        assert!(
            err.message.contains("generated column loop"),
            "expected generated column loop for `{cyclic}`, got: {}",
            err.message
        );
    }

    // 6. `RAISE()` rejected in generated columns, index expressions, and partial index WHERE.
    let gen_raise = "CREATE TABLE r1 (a INT, g INT GENERATED ALWAYS AS (RAISE(ABORT, 'x')) VIRTUAL)";
    let Statement::CreateTable(ct_r) = parse_statement(gen_raise, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let e_gr = build_table(&ct_r, gen_raise, &BuildOptions::default()).unwrap_err();
    assert!(e_gr.message.contains("RAISE() may only be used within a trigger-program"));

    let base_ddl = "CREATE TABLE rb (a INT, b INT)";
    let Statement::CreateTable(ct_rb) = parse_statement(base_ddl, &opts()).unwrap() else {
        panic!("expected CreateTable");
    };
    let rb_def = build_table(&ct_rb, base_ddl, &BuildOptions::default()).unwrap();
    for bad_idx in [
        "CREATE INDEX ir1 ON rb((RAISE(ABORT, 'x')))",
        "CREATE INDEX ir2 ON rb(a) WHERE RAISE(ABORT, 'x')",
    ] {
        let Statement::CreateIndex(ci) = parse_statement(bad_idx, &opts()).unwrap() else {
            panic!("expected CreateIndex");
        };
        let e = build_index(&ci, &rb_def, &BuildOptions::default()).unwrap_err();
        assert!(
            e.message.contains("RAISE() may only be used within a trigger-program"),
            "expected RAISE error for `{bad_idx}`, got: {}",
            e.message
        );
    }

    // 7. 256-entry tokenizer LUT edge cases (UTF-8 idents, `$` in idents, floats, hex underscores).
    let edge_sql = "SELECT a$1, café, .5, 1e-3, 0x1_A_f, 'plain' FROM t WHERE x = 123456";
    assert!(parse_statement(edge_sql, &opts()).is_ok());
}

#[test]
fn pass9_flat_values_1005_upsert_order_metered_pass1_gen_chain_and_string_doubled_quotes() {
    use xdb_core::{BudgetMeter, ResourceLimits};
    use xdb_parser::{build_table, parse_statement_metered, BuildOptions, SelectBody};

    // 1. Flat 1,005-row `VALUES` with non-constant expressions succeeds (no false compound cap!),
    //    while 501-term `UNION ALL` still fails compound cap cleanly.
    let mut big_vals = String::from("INSERT INTO t VALUES ");
    for i in 0..1005 {
        if i > 0 {
            big_vals.push(',');
        }
        big_vals.push_str(&format!("(abs({i}), CAST({i} AS TEXT))"));
    }
    let parsed_vals = parse_statement(&big_vals, &opts()).expect("1005-row VALUES must succeed");
    if let Statement::Insert(ins) = parsed_vals {
        let Some(sel) = ins.select else { panic!("expected select") };
        match sel.body {
            SelectBody::Values(v) => assert_eq!(v.rows.len(), 1005),
            other => panic!("expected flat Values, got {other:?}"),
        }
    } else {
        panic!("expected Insert");
    }

    // 2. Chained right-recursive `ON CONFLICT` upserts preserve exact SQL order (O(K) linear build).
    let upsert_sql = "INSERT INTO t VALUES (1, 2) \
        ON CONFLICT(a) DO UPDATE SET b = 10 \
        ON CONFLICT(b) DO NOTHING \
        ON CONFLICT DO UPDATE SET b = 99";
    let parsed_up = parse_statement(upsert_sql, &opts()).unwrap();
    if let Statement::Insert(ins) = parsed_up {
        assert_eq!(ins.upsert.len(), 3);
        assert!(ins.upsert[0].target.is_some());
        assert!(matches!(ins.upsert[0].action, UpsertAction::Update));
        assert!(matches!(ins.upsert[1].action, UpsertAction::Nothing));
        assert!(ins.upsert[2].target.is_none());
        assert!(matches!(ins.upsert[2].action, UpsertAction::Update));
    } else {
        panic!("expected Insert");
    }

    // 3. Pass1/Pass2 zero-alloc fast path under metered parsing doesn't double-charge parenthesized Expr.
    let paren_sql = format!("SELECT {}'payload'{}", "(".repeat(25), ")".repeat(25));
    let m = BudgetMeter::new(ResourceLimits {
        max_steps: Some(100_000),
        max_memory_bytes: Some(16_384),
        max_page_reads: None,
    });
    assert!(parse_statement_metered(&paren_sql, &opts(), &m).is_ok());
    assert!(
        m.usage().peak_memory_bytes < 8_192,
        "Pass1/Pass2 should not double-charge full Expr per paren level, peak={}",
        m.usage().peak_memory_bytes
    );

    // 4. Deeply chained 400-column GENERATED ALWAYS AS dependency chain (iterative 3-color DFS, no stack overflow!)
    //    and cycle rejection.
    let mut chain_ddl = String::from("CREATE TABLE gchain (c0 INT");
    for i in 1..400 {
        chain_ddl.push_str(&format!(", c{i} INT GENERATED ALWAYS AS (c{} + 1) VIRTUAL", i - 1));
    }
    chain_ddl.push(')');
    let chain_stmt = parse_statement(&chain_ddl, &opts()).unwrap();
    if let Statement::CreateTable(ct) = chain_stmt {
        let built = build_table(&ct, &chain_ddl, &BuildOptions::default()).expect("400-col gen chain ok");
        assert_eq!(built.columns.len(), 400);
    }
    let cyc_ddl = "CREATE TABLE gcyc (a INT GENERATED ALWAYS AS (b + 1), b INT GENERATED ALWAYS AS (a + 1))";
    let cyc_stmt = parse_statement(cyc_ddl, &opts()).unwrap();
    if let Statement::CreateTable(ct) = cyc_stmt {
        assert!(build_table(&ct, cyc_ddl, &BuildOptions::default()).is_err());
    }

    // 5. LALR(1) lookahead-safe `has_doubled_quote` on adjacent plain and escaped string literals.
    let q_sql = "SELECT 'plain1', 'it''s', 'plain2', 'a''''b', 'plain3'";
    let q_stmt = parse_statement(q_sql, &opts()).unwrap();
    if let Statement::Select(sel) = q_stmt {
        if let SelectBody::Core(core) = sel.body {
            let vals: Vec<String> = core
                .columns
                .iter()
                .map(|c| match c {
                    ResultColumn::Expr(re) => match &re.expr {
                        Expr::String(s) => s.value.clone(),
                        other => panic!("expected string, got {other:?}"),
                    },
                    _ => panic!("expected expr"),
                })
                .collect();
            assert_eq!(vals, vec!["plain1", "it's", "plain2", "a''b", "plain3"]);
        }
    }
}
