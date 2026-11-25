//! Comprehensive standalone integration tests for `xdb-sql`.
//!
//! Covers:
//! - Expressions, 3-valued logic, affinity, collations, CAST, CASE, BETWEEN, IN, row values
//! - Core scalar functions (text, printf, like/glob/escape, hex/unhex/unistr/quote, math)
//! - Date & time functions and modifiers
//! - JSON1 functions (`->`, `->>`, `json_extract`, `json_set`, `json_patch`, `json_each`, `json_tree`, `json_group_*`)
//! - Joins (`INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, `NATURAL`, `USING`, hash equi-join & nested loop)
//! - `GROUP BY`, `HAVING`, `FILTER`, `DISTINCT`, aggregate `ORDER BY`, single `min()`/`max()` bare-column rule, `percentile`/`median`
//! - Window functions (`row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`, `ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, named `WINDOW` inheritance)
//! - `WITH` and `WITH RECURSIVE` CTEs, compound `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`
//! - Correlated subqueries (`EXISTS`, scalar subquery, `IN (SELECT ...)`)
//! - XDB dialect (`FROM`-first, `GROUP BY ALL`, `ORDER BY ALL`, `::`, `* EXCLUDE`, `* REPLACE`, `ILIKE`)
//! - Deterministic `BudgetMeter` halts on expensive queries & runaway recursive CTEs

use std::sync::Arc;

use xdb_core::{Affinity, BudgetMeter, LimitKind, ResourceLimits, SqlValue};
use xdb_parser::Dialect;
use xdb_sql::{
    execute_query, ColumnMeta, MemoryCatalog, QueryOptions, QueryParams, QueryResult, ScannedRow,
    SqlError, TableMeta,
};

fn run(cat: &MemoryCatalog, sql: &str) -> QueryResult {
    execute_query(cat, sql, &QueryParams::empty(), &QueryOptions::default())
        .unwrap_or_else(|e| panic!("SQL failed:\n  {sql}\n  error: {e}"))
}

fn run_xdb(cat: &MemoryCatalog, sql: &str) -> QueryResult {
    let opts = QueryOptions {
        dialect: Dialect::Xdb,
        ..QueryOptions::default()
    };
    execute_query(cat, sql, &QueryParams::empty(), &opts)
        .unwrap_or_else(|e| panic!("XDB SQL failed:\n  {sql}\n  error: {e}"))
}

fn scalar(sql: &str) -> SqlValue {
    let cat = MemoryCatalog::new();
    let res = run(&cat, sql);
    assert_eq!(res.rows.len(), 1, "expected 1 row for: {sql}");
    assert_eq!(res.rows[0].len(), 1, "expected 1 col for: {sql}");
    res.rows[0][0].clone()
}

fn assert_eq_val(got: &SqlValue, exp: &SqlValue, label: &str) {
    assert!(
        got.exact_eq(exp),
        "{label}: got {}, expected {}",
        got.describe(),
        exp.describe()
    );
}

fn sample_catalog() -> MemoryCatalog {
    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "dept",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("name", Affinity::Text),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::text("Eng")],
            vec![SqlValue::Int(2), SqlValue::text("Sales")],
            vec![SqlValue::Int(3), SqlValue::text("Ops")],
        ],
    );
    cat.add_table(
        "emp",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("dept_id", Affinity::Integer),
            ColumnMeta::simple("name", Affinity::Text),
            ColumnMeta::simple("salary", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(1), SqlValue::text("Alice"), SqlValue::Int(150)],
            vec![SqlValue::Int(2), SqlValue::Int(1), SqlValue::text("Bob"), SqlValue::Int(120)],
            vec![SqlValue::Int(3), SqlValue::Int(1), SqlValue::text("Cara"), SqlValue::Int(150)],
            vec![SqlValue::Int(4), SqlValue::Int(2), SqlValue::text("Dan"), SqlValue::Int(100)],
            vec![SqlValue::Int(5), SqlValue::Int(2), SqlValue::text("Eve"), SqlValue::Int(130)],
            vec![SqlValue::Int(6), SqlValue::Null, SqlValue::text("Frank"), SqlValue::Int(90)],
        ],
    );
    cat.add_view(
        "high_earners",
        "SELECT name, salary FROM emp WHERE salary >= 130 ORDER BY salary DESC, name ASC",
        None,
    );
    cat
}

#[test]
fn test_expressions_and_scalar_functions() {
    // Arithmetic & exact i64::MIN literal
    assert_eq_val(&scalar("SELECT -9223372036854775808"), &SqlValue::Int(i64::MIN), "i64::MIN");
    assert_eq_val(&scalar("SELECT 1 + 2 * 3"), &SqlValue::Int(7), "precedence");
    assert_eq_val(&scalar("SELECT 10 / 0"), &SqlValue::Null, "div by zero");
    assert_eq_val(&scalar("SELECT 5 || '-' || 6"), &SqlValue::text("5-6"), "concat op");

    // 3-valued logic
    assert_eq_val(&scalar("SELECT 0 AND NULL"), &SqlValue::Int(0), "0 AND NULL");
    assert_eq_val(&scalar("SELECT 1 AND NULL"), &SqlValue::Null, "1 AND NULL");
    assert_eq_val(&scalar("SELECT 1 OR NULL"), &SqlValue::Int(1), "1 OR NULL");
    assert_eq_val(&scalar("SELECT NULL IS NULL"), &SqlValue::Int(1), "IS NULL");
    assert_eq_val(&scalar("SELECT NULL IS NOT 1"), &SqlValue::Int(1), "IS NOT");

    // Text, UTF-8, printf, like/glob/escape, hex/unhex/unistr/quote
    assert_eq_val(&scalar("SELECT length('héllo')"), &SqlValue::Int(5), "utf8 length");
    assert_eq_val(&scalar("SELECT octet_length('héllo')"), &SqlValue::Int(6), "octet_length");
    assert_eq_val(&scalar("SELECT substr('abcdef', -3, 2)"), &SqlValue::text("de"), "substr neg start");
    assert_eq_val(&scalar("SELECT substr('abcdef', 4, -2)"), &SqlValue::text("bc"), "substr neg len");
    assert_eq_val(&scalar("SELECT instr('bananas', 'na')"), &SqlValue::Int(3), "instr");
    assert_eq_val(&scalar("SELECT replace('a-b-c', '-', '::')"), &SqlValue::text("a::b::c"), "replace");
    assert_eq_val(&scalar("SELECT trim('..hello..', '.')"), &SqlValue::text("hello"), "trim");
    assert_eq_val(&scalar("SELECT concat_ws(',', 'a', NULL, 'b', 3)"), &SqlValue::text("a,b,3"), "concat_ws");
    assert_eq_val(&scalar("SELECT printf('%05d %.2f %Q', 42, 3.14159, 'o''b')"), &SqlValue::text("00042 3.14 'o''b'"), "printf");
    assert_eq_val(&scalar("SELECT 'Hello' LIKE 'h%o'"), &SqlValue::Int(1), "like");
    assert_eq_val(&scalar("SELECT '50%' LIKE '50\\%' ESCAPE '\\'"), &SqlValue::Int(1), "like escape");
    assert_eq_val(&scalar("SELECT 'file3.rs' GLOB 'file[0-9].*'"), &SqlValue::Int(1), "glob");
    assert_eq_val(&scalar("SELECT hex(unhex('DE AD BE EF', ' '))"), &SqlValue::text("DEADBEEF"), "hex/unhex");
    assert_eq_val(&scalar("SELECT quote('a''b')"), &SqlValue::text("'a''b'"), "quote");
    assert_eq_val(&scalar("SELECT unistr('A\\u0042')"), &SqlValue::text("AB"), "unistr");
    assert_eq_val(&scalar("SELECT iif(1 > 2, 'no', 3 > 2, 'yes', 'other')"), &SqlValue::text("yes"), "iif");

    // Math
    assert_eq_val(&scalar("SELECT round(2.71828, 2)"), &SqlValue::Real(2.72), "round");
    assert_eq_val(&scalar("SELECT power(2, 10)"), &SqlValue::Real(1024.0), "power");
    assert_eq_val(&scalar("SELECT sqrt(81)"), &SqlValue::Real(9.0), "sqrt");
    assert_eq_val(&scalar("SELECT sqrt(-1)"), &SqlValue::Null, "sqrt neg -> NULL");

    // Collations & row values
    assert_eq_val(&scalar("SELECT 'abc' = 'ABC' COLLATE NOCASE"), &SqlValue::Int(1), "collate nocase");
    assert_eq_val(&scalar("SELECT 'abc   ' = 'abc' COLLATE RTRIM"), &SqlValue::Int(1), "collate rtrim");
    assert_eq_val(&scalar("SELECT (1, 2) < (1, 3)"), &SqlValue::Int(1), "row value <");
    assert_eq_val(&scalar("SELECT (1, 2) IN (VALUES (1, 1), (1, 2))"), &SqlValue::Int(1), "row value IN");
}

#[test]
fn test_date_and_json_functions() {
    assert_eq_val(
        &scalar("SELECT date('2026-01-31', '+1 month', 'floor')"),
        &SqlValue::text("2026-02-28"),
        "date +1 month floor",
    );
    assert_eq_val(
        &scalar("SELECT datetime(1700000000, 'unixepoch')"),
        &SqlValue::text("2023-11-14 22:13:20"),
        "unixepoch",
    );
    assert_eq_val(
        &scalar("SELECT strftime('%Y-%m-%d %H:%M', '2026-06-15 09:30:00', '+2 hours')"),
        &SqlValue::text("2026-06-15 11:30"),
        "strftime",
    );
    assert_eq_val(
        &scalar("SELECT timediff('2026-03-01 12:00:00', '2025-01-01 00:00:00')"),
        &SqlValue::text("+0001-02-00 12:00:00.000"),
        "timediff",
    );

    // JSON1
    assert_eq_val(
        &scalar(r#"SELECT json_extract('{"a":[10,20,{"b":"hi"}]}', '$.a[2].b')"#),
        &SqlValue::text("hi"),
        "json_extract",
    );
    assert_eq_val(
        &scalar(r#"SELECT '{"a":{"b":42}}' -> 'a' ->> 'b'"#),
        &SqlValue::Int(42),
        "json arrows",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_set('{"a":1}', '$.b', json('[2,3]'))"#),
        &SqlValue::text(r#"{"a":1,"b":[2,3]}"#),
        "json_set preserves json subtype",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_patch('{"a":1,"b":2}', '{"b":null,"c":3}')"#),
        &SqlValue::text(r#"{"a":1,"c":3}"#),
        "json_patch",
    );

    let cat = MemoryCatalog::new();
    let each = run(
        &cat,
        r#"SELECT key, value, type FROM json_each('[10, "x", true]') ORDER BY key"#,
    );
    assert_eq!(each.rows.len(), 3);
    assert_eq_val(&each.rows[0][1], &SqlValue::Int(10), "json_each[0]");
    assert_eq_val(&each.rows[1][1], &SqlValue::text("x"), "json_each[1]");
    assert_eq_val(&each.rows[2][2], &SqlValue::text("true"), "json_each[2].type");
}

#[test]
fn test_joins_aggregates_windows_ctes_and_views() {
    let cat = sample_catalog();

    // FULL OUTER JOIN
    let full = run(
        &cat,
        "SELECT d.name, e.name FROM dept d FULL OUTER JOIN emp e ON d.id = e.dept_id ORDER BY d.id NULLS LAST, e.id NULLS LAST",
    );
    assert_eq!(full.rows.len(), 7);
    assert_eq_val(&full.rows[5][0], &SqlValue::text("Ops"), "Ops unmatched");
    assert_eq_val(&full.rows[5][1], &SqlValue::Null, "Ops emp null");
    assert_eq_val(&full.rows[6][0], &SqlValue::Null, "Frank dept null");
    assert_eq_val(&full.rows[6][1], &SqlValue::text("Frank"), "Frank unmatched");

    // GROUP BY + HAVING + FILTER + aggregate ORDER BY + single max() bare-column rule
    let grp = run(
        &cat,
        "SELECT d.name, count(*) AS cnt, sum(e.salary) FILTER (WHERE e.salary >= 120) AS hi_sum, \
         group_concat(e.name, ',' ORDER BY e.name DESC) AS names \
         FROM dept d JOIN emp e ON d.id = e.dept_id \
         GROUP BY d.id, d.name HAVING count(*) >= 2 ORDER BY d.id",
    );
    assert_eq!(grp.rows.len(), 2);
    assert_eq_val(&grp.rows[0][0], &SqlValue::text("Eng"), "Eng");
    assert_eq_val(&grp.rows[0][1], &SqlValue::Int(3), "Eng cnt");
    assert_eq_val(&grp.rows[0][2], &SqlValue::Int(420), "Eng hi_sum");
    assert_eq_val(&grp.rows[0][3], &SqlValue::text("Cara,Bob,Alice"), "Eng ordered group_concat");
    assert_eq_val(&grp.rows[1][2], &SqlValue::Int(130), "Sales hi_sum");

    // SQLite bare-column min()/max() rule
    let top_emp = run(&cat, "SELECT name, max(salary) FROM emp WHERE dept_id = 2");
    assert_eq_val(&top_emp.rows[0][0], &SqlValue::text("Eve"), "bare col from max row");
    assert_eq_val(&top_emp.rows[0][1], &SqlValue::Int(130), "max salary");

    // Window functions + named WINDOW + ROWS frame
    let win = run(
        &cat,
        "SELECT name, salary, \
         row_number() OVER w AS rn, \
         rank() OVER w AS rnk, \
         dense_rank() OVER w AS drnk, \
         sum(salary) OVER (w ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running \
         FROM emp WHERE dept_id = 1 \
         WINDOW w AS (ORDER BY salary DESC) \
         ORDER BY rn",
    );
    assert_eq!(win.rows.len(), 3);
    assert_eq_val(&win.rows[0][3], &SqlValue::Int(1), "rank 1");
    assert_eq_val(&win.rows[1][3], &SqlValue::Int(1), "rank tie 1");
    assert_eq_val(&win.rows[2][3], &SqlValue::Int(3), "rank 3");
    assert_eq_val(&win.rows[2][4], &SqlValue::Int(2), "dense_rank 2");
    assert_eq_val(&win.rows[2][5], &SqlValue::Int(420), "running sum");

    // WITH RECURSIVE Fibonacci + View + Compound INTERSECT
    let fib = run(
        &cat,
        "WITH RECURSIVE f(n, a, b) AS ( \
           SELECT 1, 0, 1 \
           UNION ALL \
           SELECT n + 1, b, a + b FROM f WHERE n < 10 \
         ) SELECT a FROM f ORDER BY n",
    );
    assert_eq!(fib.rows.len(), 10);
    assert_eq_val(&fib.rows[9][0], &SqlValue::Int(34), "fib(10)");

    let view_res = run(&cat, "SELECT name FROM high_earners");
    assert_eq!(view_res.rows.len(), 3);
    assert_eq_val(&view_res.rows[0][0], &SqlValue::text("Alice"), "view row 0");

    // Correlated EXISTS
    let corr = run(
        &cat,
        "SELECT name FROM dept d WHERE EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id AND e.salary > 140)",
    );
    assert_eq!(corr.rows.len(), 1);
    assert_eq_val(&corr.rows[0][0], &SqlValue::text("Eng"), "correlated EXISTS");

    // Parameters
    let params = QueryParams::positional(vec![SqlValue::Int(125)]);
    let pres = execute_query(
        &cat,
        "SELECT count(*) FROM emp WHERE salary > ?",
        &params,
        &QueryOptions::default(),
    )
    .unwrap();
    assert_eq_val(&pres.rows[0][0], &SqlValue::Int(3), "positional param");
}

#[test]
fn test_xdb_dialect_extensions() {
    let cat = sample_catalog();

    // FROM-first, * EXCLUDE, :: cast, ILIKE, GROUP BY ALL, ORDER BY ALL
    let r1 = run_xdb(&cat, "FROM dept WHERE name ILIKE 'e%'");
    assert_eq!(r1.columns, vec!["id", "name"]);
    assert_eq!(r1.rows.len(), 1);

    let r2 = run_xdb(&cat, "SELECT * EXCLUDE (id), salary::TEXT AS sal_txt FROM emp WHERE id = 1");
    assert_eq!(r2.columns, vec!["dept_id", "name", "salary", "sal_txt"]);
    assert_eq_val(&r2.rows[0][3], &SqlValue::text("150"), "::TEXT cast");

    let r3 = run_xdb(
        &cat,
        "SELECT dept_id, count(*) AS n FROM emp WHERE dept_id IS NOT NULL GROUP BY ALL ORDER BY ALL DESC",
    );
    assert_eq!(r3.rows.len(), 2);
    assert_eq_val(&r3.rows[0][0], &SqlValue::Int(2), "ORDER BY ALL DESC first");
    assert_eq_val(&r3.rows[1][0], &SqlValue::Int(1), "ORDER BY ALL DESC second");
}

#[test]
fn test_meter_halts_runaway_recursive_cte() {
    let cat = MemoryCatalog::new();
    let meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(500),
        max_memory_bytes: None,
        max_page_reads: None,
    }));
    let opts = QueryOptions {
        meter: Some(meter.clone()),
        ..QueryOptions::default()
    };
    let err = execute_query(
        &cat,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT * FROM c",
        &QueryParams::empty(),
        &opts,
    )
    .expect_err("infinite recursive CTE must halt on step budget");
    match err {
        SqlError::ResourceLimit(e) => {
            assert_eq!(e.kind, LimitKind::Steps);
            assert!(e.used > 500);
        }
        other => panic!("expected ResourceLimit, got {other:?}"),
    }
}

#[test]
fn test_sql_audit_window_cte_join_json_and_collation_regressions() {
    let cat = MemoryCatalog::new();

    // 1. RANGE offset frame without ORDER BY must return a SQL error (never panic OOB).
    assert!(execute_query(
        &cat,
        "SELECT sum(1) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW)",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());

    // ROWS offset must validate non-negative integers; numeric strings like '5' are accepted.
    assert!(execute_query(
        &cat,
        "SELECT sum(1) OVER (ROWS BETWEEN '-5' PRECEDING AND CURRENT ROW)",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "SELECT sum(1) OVER (ROWS BETWEEN 1.5 PRECEDING AND CURRENT ROW)",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());
    assert_eq_val(
        &scalar("SELECT sum(1) OVER (ROWS BETWEEN '5' PRECEDING AND CURRENT ROW)"),
        &SqlValue::Int(1),
        "ROWS string integer offset",
    );

    // RANGE with DESC, NULL, and non-numeric ORDER BY values matches SQLite.
    let rng = run(
        &cat,
        "WITH d(x) AS (VALUES (NULL), ('a'), ('a'), (10), (8), (5))
         SELECT x, count(*) OVER (ORDER BY x DESC RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING)
         FROM d ORDER BY x DESC",
    );
    // Order DESC: 'a', 'a', 10, 8, 5, NULL
    // 'a' peer group = 2; 10 matches [10, 8] = 2; 8 matches [10, 8] = 2; 5 matches [5] = 1; NULL matches [NULL] = 1
    let counts: Vec<i64> = rng
        .rows
        .iter()
        .map(|r| match r[1] {
            SqlValue::Int(n) => n,
            _ => panic!("expected int"),
        })
        .collect();
    assert_eq!(counts, vec![2, 2, 2, 2, 1, 1]);

    // 2. random() must advance PRNG state across calls.
    let rnd = run(&cat, "SELECT random(), random(), random()");
    assert!(
        !rnd.rows[0][0].exact_eq(&rnd.rows[0][1]) || !rnd.rows[0][1].exact_eq(&rnd.rows[0][2]),
        "random() must advance across calls"
    );

    // 3. WITH RECURSIVE whose second branch does NOT reference the CTE must terminate immediately.
    let non_rec = run(
        &cat,
        "WITH RECURSIVE a(x) AS (SELECT 1 UNION ALL SELECT 2) SELECT x FROM a ORDER BY x",
    );
    assert_eq!(non_rec.rows.len(), 2);
    assert_eq_val(&non_rec.rows[0][0], &SqlValue::Int(1), "non-rec CTE row 0");
    assert_eq_val(&non_rec.rows[1][0], &SqlValue::Int(2), "non-rec CTE row 1");

    // 4. RIGHT JOIN USING: unqualified `a` is coalesced, `t1.a` stays NULL, `t2.*` includes `a`.
    let rj = run(
        &cat,
        "WITH t1(a, x) AS (VALUES (1, 'L')),
              t2(a, y) AS (VALUES (1, 'R1'), (2, 'R2'))
         SELECT a, t1.a, t2.* FROM t1 RIGHT JOIN t2 USING (a) ORDER BY a",
    );
    assert_eq!(rj.rows.len(), 2);
    assert_eq_val(&rj.rows[1][0], &SqlValue::Int(2), "coalesced a");
    assert_eq_val(&rj.rows[1][1], &SqlValue::Null, "t1.a stays NULL");
    assert_eq_val(&rj.rows[1][2], &SqlValue::Int(2), "t2.* includes a");
    assert_eq_val(&rj.rows[1][3], &SqlValue::text("R2"), "t2.y");

    // 5. DISTINCT and UNION honor COLLATE NOCASE; GROUP BY / hash join equate exact large i64 == f64.
    let dist = run(
        &cat,
        "WITH t(x) AS (VALUES ('a'), ('A'), ('b'))
         SELECT DISTINCT x COLLATE NOCASE FROM t ORDER BY 1",
    );
    assert_eq!(dist.rows.len(), 2);

    let grp = run(
        &cat,
        "WITH t(v) AS (VALUES (2251799813685248), (2251799813685248.0))
         SELECT count(*) FROM t GROUP BY v",
    );
    assert_eq!(grp.rows.len(), 1);
    assert_eq_val(&grp.rows[0][0], &SqlValue::Int(2), "large exact int == float");

    // 6. Short-circuiting in iif / coalesce avoids evaluating untaken branch (e.g. error/zero-div side effects).
    assert_eq_val(
        &scalar("SELECT iif(1, 'ok', json('invalid'))"),
        &SqlValue::text("ok"),
        "iif short-circuit",
    );
    assert_eq_val(
        &scalar("SELECT coalesce('ok', json('invalid'))"),
        &SqlValue::text("ok"),
        "coalesce short-circuit",
    );

    // 7. ORDER BY out-of-range ordinal errors even on <= 1 row.
    assert!(execute_query(
        &cat,
        "SELECT 1 ORDER BY 99",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());

    // 8. JSON semantics: non-TEXT keys rejected in json_object; JSON subtype preserved from
    // json_group_array and json_extract; json_group_array(DISTINCT x) preserves NULL.
    assert!(execute_query(
        &cat,
        "SELECT json_object(1, 2)",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());
    assert_eq_val(
        &scalar("WITH t(x) AS (VALUES (1), (NULL), (1), (NULL)) SELECT json_object('arr', json_group_array(DISTINCT x)) FROM t"),
        &SqlValue::text(r#"{"arr":[1,null]}"#),
        "json_group_array(DISTINCT) preserves NULL and JSON subtype",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_object('sub', json_extract('{"a":[1,2]}', '$.a'))"#),
        &SqlValue::text(r#"{"sub":[1,2]}"#),
        "json_extract array preserves JSON subtype",
    );
    // SQLite 3.53.3 accepts valid UTF-8 JSON stored as a BLOB in json()/json_valid()/json_extract().
    assert_eq_val(
        &scalar("SELECT json(X'7b7d')"),
        &SqlValue::text("{}"),
        "json(BLOB) matches SQLite 3.53.3",
    );

    // 9. IN subquery column affinity and exact tuple width check (matching C SQLite).
    let scat = sample_catalog();
    assert_eq_val(
        &run(&scat, "SELECT '01' IN (SELECT id FROM dept)").rows[0][0],
        &SqlValue::Int(1),
        "IN subquery applies INT affinity from RHS column ('01' == 1)",
    );
    assert_eq_val(
        &scalar("WITH t(x) AS (VALUES (CAST('01' AS TEXT))) SELECT 1 IN (SELECT x FROM t)"),
        &SqlValue::Int(0),
        "IN subquery applies TEXT affinity from RHS ('1' != '01')",
    );
    assert!(execute_query(
        &cat,
        "SELECT (1, 2) IN (SELECT 1)",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .is_err());
}

#[test]
fn test_meter_blocks_printf_like_json_and_randomblob_dos() {
    let cat = MemoryCatalog::new();
    let metered = || QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(2_000),
            max_memory_bytes: Some(16_384),
            max_page_reads: None,
        }))),
        ..QueryOptions::default()
    };

    // printf width/precision huge allocation must be caught by memory meter (no OOM).
    let err = execute_query(&cat, "SELECT printf('%999999900f', 1.0)", &QueryParams::empty(), &metered())
        .expect_err("huge printf width must fail under memory meter");
    assert!(matches!(err, SqlError::ResourceLimit(_)));

    let err = execute_query(&cat, "SELECT printf('%,.700000000d', 1)", &QueryParams::empty(), &metered())
        .expect_err("huge printf precision must fail under memory meter");
    assert!(matches!(err, SqlError::ResourceLimit(_)));

    // randomblob must be metered.
    let err = execute_query(&cat, "SELECT randomblob(100000)", &QueryParams::empty(), &metered())
        .expect_err("large randomblob must fail under memory meter");
    assert!(matches!(err, SqlError::ResourceLimit(_)));

    // json_pretty huge indent must be metered.
    let err = execute_query(
        &cat,
        "SELECT json_pretty('[1,2,3]', printf('%100000s', ' '))",
        &QueryParams::empty(),
        &metered(),
    )
    .expect_err("huge json_pretty indent must fail under memory meter");
    assert!(matches!(err, SqlError::ResourceLimit(_)));

    // Expensive LIKE scan on a long string must halt under step budget.
    let tight_steps = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(200),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        }))),
        ..QueryOptions::default()
    };
    let haystack = "a".repeat(5000);
    let pat = format!("{}z", "%a".repeat(40));
    let err = execute_query(
        &cat,
        &format!("SELECT '{haystack}' LIKE '{pat}'"),
        &QueryParams::empty(),
        &tight_steps,
    )
    .expect_err("expensive LIKE scan must halt under step meter");
    assert!(matches!(err, SqlError::ResourceLimit(_)));
}

#[test]
fn test_pass2_sql_semantics_and_json_fixes() {
    let mut cat = sample_catalog();
    cat.add_table(
        "empty_tbl",
        vec![
            ColumnMeta::simple("a", Affinity::Integer),
            ColumnMeta::simple("b", Affinity::Text),
        ],
        vec![],
    );
    cat.add_table(
        "tx",
        vec![ColumnMeta::simple("code", Affinity::Text)],
        vec![vec![SqlValue::text("01")], vec![SqlValue::text("1")]],
    );
    let q_err = |c: &MemoryCatalog, sql: &str| {
        execute_query(c, sql, &QueryParams::empty(), &QueryOptions::default())
            .expect_err(&format!("expected error for: {sql}"))
    };

    // 1. Zero-row column/window validation (errors even when FROM produces 0 rows).
    q_err(&cat, "SELECT bogus FROM empty_tbl");
    q_err(&cat, "SELECT a FROM empty_tbl WHERE bogus = 1");
    q_err(&cat, "SELECT a FROM empty_tbl GROUP BY bogus");
    q_err(&cat, "SELECT a FROM empty_tbl ORDER BY bogus");
    q_err(
        &cat,
        "SELECT sum(a) OVER w FROM empty_tbl WINDOW w AS (PARTITION BY bogus)",
    );

    // 2. HAVING without GROUP BY or aggregates is an error.
    q_err(&cat, "SELECT id FROM dept HAVING id > 1");

    // 3. Schema qualifier validation (only `main` is valid).
    q_err(&cat, "SELECT bogus.dept.id FROM dept");
    assert_eq!(run(&cat, "SELECT main.dept.id FROM dept ORDER BY id").rows.len(), 3);

    // 4. ORDER BY / GROUP BY ordinal with COLLATE, overflowing ordinal, explicit AS precedence.
    let ob = run(
        &cat,
        "WITH t(x) AS (VALUES ('b'), ('A'), ('a')) SELECT x FROM t ORDER BY 1 COLLATE BINARY DESC",
    );
    assert_eq_val(&ob.rows[0][0], &SqlValue::text("b"), "ORDER BY 1 COLLATE BINARY DESC");
    assert_eq_val(&ob.rows[2][0], &SqlValue::text("A"), "ORDER BY 1 COLLATE BINARY DESC last");

    let gb = run(
        &cat,
        "WITH t(x) AS (VALUES ('a'), ('A'), ('b')) SELECT x, count(*) FROM t GROUP BY 1 COLLATE NOCASE ORDER BY 1 COLLATE NOCASE",
    );
    assert_eq!(gb.rows.len(), 2);
    assert_eq_val(&gb.rows[0][1], &SqlValue::Int(2), "GROUP BY 1 COLLATE NOCASE");

    q_err(&cat, "SELECT 1 ORDER BY 9999999999999999999999");

    // Explicit `AS` alias wins in bare ORDER BY even if FROM has the same column name.
    let alias_ord = run(
        &cat,
        "WITH t(x, y) AS (VALUES (1, 20), (2, 10)) SELECT x AS y, y AS x FROM t ORDER BY x",
    );
    assert_eq_val(&alias_ord.rows[0][0], &SqlValue::Int(2), "explicit AS alias wins ORDER BY");
    assert_eq_val(&alias_ord.rows[0][1], &SqlValue::Int(10), "explicit AS alias wins ORDER BY val");

    // 5. Single min()/max() bare-column rule counts only SELECT aggregates (not HAVING).
    let single_max = run(
        &cat,
        "WITH t(k, v) AS (VALUES ('a', 1), ('b', 9), ('c', 4))
         SELECT k, max(v) FROM t HAVING max(v) > 5",
    );
    assert_eq!(single_max.rows.len(), 1);
    assert_eq_val(&single_max.rows[0][0], &SqlValue::text("b"), "single max + HAVING max");

    // 6. IN affinity (no double affinity), RHS COLLATE, row-value IN list, row-value BETWEEN,
    //    and subquery-vs-subquery row comparisons.
    assert_eq_val(
        &run(&cat, "SELECT code FROM tx WHERE code IN (SELECT 1)").rows[0][0],
        &SqlValue::text("1"),
        "TEXT col IN (SELECT 1) compares as TEXT ('1' matches, '01' does not)",
    );
    assert_eq!(
        run(&cat, "SELECT code FROM tx WHERE code IN (SELECT 1)").rows.len(),
        1
    );
    assert_eq_val(
        &scalar("SELECT 'a' IN ('A' COLLATE NOCASE)"),
        &SqlValue::Int(1),
        "IN list respects RHS COLLATE",
    );
    assert_eq_val(
        &scalar("SELECT 'a' IN (SELECT 'A' COLLATE NOCASE)"),
        &SqlValue::Int(1),
        "IN subquery respects RHS COLLATE",
    );
    assert_eq_val(
        &scalar("SELECT (1, 2) IN ((1, 3), (1, 2))"),
        &SqlValue::Int(1),
        "row-value IN list",
    );
    assert_eq_val(
        &scalar("SELECT (1, 5) BETWEEN (1, 1) AND (1, 10)"),
        &SqlValue::Int(1),
        "row-value BETWEEN",
    );
    assert_eq_val(
        &scalar("SELECT (1, 11) NOT BETWEEN (1, 1) AND (1, 10)"),
        &SqlValue::Int(1),
        "row-value NOT BETWEEN",
    );
    assert_eq_val(
        &scalar("SELECT (SELECT 1, 2) = (SELECT 1, 2)"),
        &SqlValue::Int(1),
        "subquery-vs-subquery row comparison",
    );

    // 7. LIMIT / OFFSET strict integer coercion (matching SQLite 3.53.3).
    q_err(&cat, "SELECT 1 LIMIT NULL");
    q_err(&cat, "SELECT 1 LIMIT 'abc'");
    q_err(&cat, "SELECT 1 LIMIT 1.5");
    q_err(&cat, "SELECT 1 LIMIT 1 OFFSET NULL");

    // 8. Compound ORDER BY leftmost table qualifier + explicit COLLATE merge.
    let cmp_tbl = run(
        &cat,
        "SELECT id FROM dept UNION ALL SELECT id FROM dept ORDER BY dept.id DESC LIMIT 1",
    );
    assert_eq_val(&cmp_tbl.rows[0][0], &SqlValue::Int(3), "compound ORDER BY leftmost table.col");
    let cmp_coll = run(&cat, "SELECT 'a' UNION SELECT 'A' COLLATE NOCASE");
    assert_eq!(cmp_coll.rows.len(), 1);

    // 9. Correlated TVF LEFT JOIN with ON predicate.
    let tvf_on = run(
        &cat,
        "WITH t(id, j) AS (VALUES (1, '[10,20]'), (2, '[5]'))
         SELECT t.id, je.value FROM t LEFT JOIN json_each(t.j) AS je ON je.value > 15 ORDER BY t.id",
    );
    assert_eq!(tvf_on.rows.len(), 2);
    assert_eq_val(&tvf_on.rows[0][1], &SqlValue::Int(20), "correlated TVF LEFT JOIN ON match");
    assert_eq_val(&tvf_on.rows[1][1], &SqlValue::Null, "correlated TVF LEFT JOIN ON null pad");

    // 10. WITH RECURSIVE: multiple recursive terms, 0-row column mismatch, ORDER BY + LIMIT priority queue.
    let multi_rec = run(
        &cat,
        "WITH RECURSIVE c(x) AS (
             SELECT 1
             UNION ALL SELECT x + 1 FROM c WHERE x < 3
             UNION ALL SELECT x + 10 FROM c WHERE x = 1
         ) SELECT x FROM c ORDER BY x",
    );
    let xs: Vec<i64> = multi_rec
        .rows
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect();
    assert_eq!(xs, vec![1, 2, 3, 11]);

    q_err(
        &cat,
        "WITH RECURSIVE c(x, y) AS (SELECT 1 WHERE 0 UNION ALL SELECT x + 1 FROM c) SELECT * FROM c",
    );

    let rec_pq = run(
        &cat,
        "WITH RECURSIVE c(x) AS (
             SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10 ORDER BY 1 DESC LIMIT 3
         ) SELECT x FROM c",
    );
    let pq_xs: Vec<i64> = rec_pq.rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
    assert_eq!(pq_xs, vec![1, 2, 3]);

    // 11. Named WINDOW with aggregate + illegal window-chain override.
    let win_agg = run(
        &cat,
        "WITH t(g, v) AS (VALUES (1, 10), (1, 20), (2, 5))
         SELECT g, sum(v), sum(sum(v)) OVER w FROM t GROUP BY g WINDOW w AS (ORDER BY sum(v)) ORDER BY g",
    );
    assert_eq_val(&win_agg.rows[0][2], &SqlValue::Int(35), "named WINDOW referencing aggregate");

    q_err(
        &cat,
        "SELECT sum(id) OVER w2 FROM dept WINDOW w1 AS (ORDER BY id), w2 AS (w1 ORDER BY name)",
    );

    // 12. JSON1 fixes: json_remove('$') / NULL path, json_tree(doc, path), huge array index, TRUE/FALSE.
    assert_eq_val(
        &scalar(r#"SELECT json_remove('{"a":1}', '$')"#),
        &SqlValue::Null,
        "json_remove root '$'",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_remove('{"a":1}', '$.a', NULL)"#),
        &SqlValue::Null,
        "json_remove NULL path",
    );
    let jt = run(
        &cat,
        r#"SELECT key, fullkey, path FROM json_tree('{"a":[10,20]}', '$.a') LIMIT 1"#,
    );
    assert_eq_val(&jt.rows[0][0], &SqlValue::text("a"), "json_tree root key");
    assert_eq_val(&jt.rows[0][1], &SqlValue::text("$.a"), "json_tree root fullkey");
    assert_eq_val(&jt.rows[0][2], &SqlValue::text("$"), "json_tree root parent path");
    assert_eq_val(
        &scalar("SELECT '[1,2]' -> 5000000000"),
        &SqlValue::Null,
        "json arrow huge index returns NULL",
    );
    assert_eq_val(
        &scalar("SELECT json_extract('[1,2]', '$[99999999999999999999]')"),
        &SqlValue::Null,
        "json_extract overflowing array index returns NULL",
    );
    assert_eq_val(
        &scalar("SELECT json_valid(X'7b7d')"),
        &SqlValue::Int(1),
        "json_valid(BLOB UTF-8 JSON)",
    );
    assert_eq_val(&scalar("SELECT true + false"), &SqlValue::Int(1), "true/false literals");
}

#[test]
fn test_pass2_cumulative_memory_and_step_metering() {
    let cat = sample_catalog();
    let tight_mem = |bytes: u64| QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(500_000),
            max_memory_bytes: Some(bytes),
            max_page_reads: None,
        }))),
        ..QueryOptions::default()
    };

    // 1. Cumulative memory metering catches CROSS JOIN explosion (many small rows).
    let err = execute_query(
        &cat,
        "WITH RECURSIVE a(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM a WHERE x < 100)
         SELECT a1.x, a2.x FROM a AS a1 CROSS JOIN a AS a2",
        &QueryParams::empty(),
        &tight_mem(8_192),
    )
    .expect_err("CROSS JOIN must exceed cumulative memory budget");
    assert!(matches!(err, SqlError::ResourceLimit(ref e) if e.kind == LimitKind::Memory));

    // 2. json_pretty deep indent bomb caught BEFORE allocating output.
    let deep_json = format!("{}1{}", "[".repeat(120), "]".repeat(120));
    let indent = " ".repeat(20);
    let err = execute_query(
        &cat,
        &format!("SELECT json_pretty('{deep_json}', '{indent}')"),
        &QueryParams::empty(),
        &tight_mem(4_096),
    )
    .expect_err("deep json_pretty indent bomb must hit memory budget");
    assert!(matches!(err, SqlError::ResourceLimit(ref e) if e.kind == LimitKind::Memory));

    // 3. group_concat cumulative memory enforcement.
    let err = execute_query(
        &cat,
        "WITH RECURSIVE a(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM a WHERE x < 300)
         SELECT group_concat('01234567890123456789') FROM a",
        &QueryParams::empty(),
        &tight_mem(2_048),
    )
    .expect_err("group_concat must hit cumulative memory budget");
    assert!(matches!(err, SqlError::ResourceLimit(ref e) if e.kind == LimitKind::Memory));

    // 4. instr uses linear KMP and charges steps (halts under tiny step budget, fast otherwise).
    let hay = format!("{}b", "a".repeat(20_000));
    let needle = format!("{}b", "a".repeat(5_000));
    let tight_steps = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_steps: Some(50),
            max_memory_bytes: Some(1024 * 1024),
            max_page_reads: None,
        }))),
        ..QueryOptions::default()
    };
    let err = execute_query(
        &cat,
        &format!("SELECT instr('{hay}', '{needle}')"),
        &QueryParams::empty(),
        &tight_steps,
    )
    .expect_err("large instr must halt under tight step meter");
    assert!(matches!(err, SqlError::ResourceLimit(ref e) if e.kind == LimitKind::Steps));
    // And with ample budget, KMP finishes accurately in microseconds!
    let ok = run(&cat, &format!("SELECT instr('{hay}', '{needle}')"));
    assert_eq_val(&ok.rows[0][0], &SqlValue::Int(15_001), "KMP instr exact match position");
}

#[test]
fn test_pass3_functions_eval_subqueries_windows_and_joins() {
    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "t_int",
        vec![ColumnMeta::simple("x", Affinity::Integer)],
        vec![vec![SqlValue::Int(1)]],
    );
    let mut nc_col = ColumnMeta::simple("x", Affinity::Text);
    nc_col.collation = "NOCASE".into();
    cat.add_table("t_nc", vec![nc_col], vec![vec![SqlValue::text("A")]]);
    cat.add_table(
        "empty_t",
        vec![ColumnMeta::simple("a", Affinity::Integer)],
        vec![],
    );
    // Table for TEXT-affinity hash equijoin (>= 16 rows on RHS to trigger hash path).
    let t_left_rows = vec![vec![SqlValue::Int(1)], vec![SqlValue::text("2")]];
    let mut t_right_rows = vec![vec![SqlValue::Real(1.0)], vec![SqlValue::Int(2)]];
    for i in 3..=25i64 {
        t_right_rows.push(vec![SqlValue::text(format!("k{i}"))]);
    }
    cat.add_table(
        "j_left",
        vec![ColumnMeta::simple("k", Affinity::Text)],
        t_left_rows,
    );
    cat.add_table(
        "j_right",
        vec![ColumnMeta::simple("k", Affinity::None)],
        t_right_rows,
    );

    let eval_scalar = |sql: &str| run(&cat, sql).rows[0][0].clone();

    // 1. unicode(char(0)) -> NULL; NUL-safe linear replace; json_remove first-match-only.
    assert_eq_val(&eval_scalar("SELECT unicode(char(0))"), &SqlValue::Null, "unicode(char(0))");
    // SQLite `replaceFunc` (`func.c:1515`) returns input unchanged if pattern starts with NUL,
    // while still replacing embedded NULs inside patterns starting with a non-NUL byte.
    assert_eq_val(
        &eval_scalar("SELECT replace('a' || char(0) || 'b', char(0), 'X')"),
        &SqlValue::text("a\0b"),
        "NUL-leading replace pattern returns input unchanged (SQLite func.c:1515)",
    );
    assert_eq_val(
        &eval_scalar("SELECT replace('a' || char(0) || 'b', 'a' || char(0), 'X')"),
        &SqlValue::text("Xb"),
        "embedded NUL inside non-NUL-leading replace pattern",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_remove('{\"a\":1,\"a\":2,\"b\":3}', '$.a')"),
        &SqlValue::text("{\"a\":2,\"b\":3}"),
        "json_remove removes first matching duplicate key only",
    );

    // 2. printf %#Q large ASCII string succeeds cleanly.
    let big = "a".repeat(2000);
    let q_out = eval_scalar(&format!("SELECT length(printf('%#Q', '{big}'))"));
    assert_eq_val(&q_out, &SqlValue::Int(2002), "printf %#Q exact length");

    // 3. nth_value positive integer validation + RANGE frames with NULL/TEXT/NULLS LAST + sliding window.
    for bad_n in ["0", "-1", "1.9"] {
        assert!(execute_query(
            &cat,
            &format!("WITH d(v) AS (VALUES (10), (20), (30)) SELECT nth_value(v, {bad_n}) OVER () FROM d"),
            &QueryParams::empty(),
            &QueryOptions::default()
        )
        .is_err());
    }
    let nv = run(
        &cat,
        "WITH d(v) AS (VALUES (10), (20), (30))
         SELECT nth_value(v, NULL) OVER (), nth_value(v, 2) OVER (ORDER BY v) FROM d",
    );
    assert_eq_val(&nv.rows[0][0], &SqlValue::Null, "nth_value(NULL) -> NULL");
    assert_eq_val(&nv.rows[1][1], &SqlValue::Int(20), "nth_value(2) -> 20");

    let rng_null = run(
        &cat,
        "WITH d(v) AS (VALUES (NULL), (1), (2), (10), ('abc'))
         SELECT v, sum(1) OVER (ORDER BY v ASC NULLS LAST RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING)
         FROM d ORDER BY v ASC NULLS LAST",
    );
    // Ordering under ASC NULLS LAST: 1 (matches 1,2 -> 2), 2 (matches 1,2 -> 2), 10 (matches 10 -> 1), 'abc' (peer 'abc' -> 1), NULL (peer NULL -> 1).
    assert_eq_val(&rng_null.rows[0][1], &SqlValue::Int(2), "RANGE around 1");
    assert_eq_val(&rng_null.rows[1][1], &SqlValue::Int(2), "RANGE around 2");
    assert_eq_val(&rng_null.rows[2][1], &SqlValue::Int(1), "RANGE around 10");
    assert_eq_val(&rng_null.rows[3][1], &SqlValue::Int(1), "RANGE on TEXT peer");
    assert_eq_val(&rng_null.rows[4][1], &SqlValue::Int(1), "RANGE on NULL peer");

    // Sliding window O(N) incremental SUM/COUNT/AVG verification.
    let slide = run(
        &cat,
        "WITH d(id, v) AS (VALUES (1, 10), (2, 20), (3, NULL), (4, 40), (5, 50))
         SELECT id,
                sum(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING),
                count(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)
         FROM d ORDER BY id",
    );
    assert_eq_val(&slide.rows[0][1], &SqlValue::Int(30), "row 1 sum");
    assert_eq_val(&slide.rows[1][1], &SqlValue::Int(30), "row 2 sum");
    assert_eq_val(&slide.rows[2][1], &SqlValue::Int(60), "row 3 sum");
    assert_eq_val(&slide.rows[3][1], &SqlValue::Int(90), "row 4 sum");
    assert_eq_val(&slide.rows[4][1], &SqlValue::Int(90), "row 5 sum");
    assert_eq_val(&slide.rows[2][2], &SqlValue::Int(2), "row 3 non-null count");

    // 4. CASE NULL evaluates WHEN expressions; FILTER on non-aggregate errors; REAL IN (2^53+1) is 0.
    assert!(execute_query(
        &cat,
        "SELECT CASE NULL WHEN json('{') THEN 1 END",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "SELECT abs(1) FILTER (WHERE 1)",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert_eq_val(
        &eval_scalar("SELECT CAST(9007199254740992.0 AS REAL) IN (9007199254740993)"),
        &SqlValue::Int(0),
        "REAL IN (9007199254740993) uses comparison affinity without lossy cast",
    );

    // 5. Subquery affinity vs collation propagation, CAST/binary explicit COLLATE, compound collation merge.
    assert_eq_val(
        &eval_scalar("SELECT '01' = (SELECT x FROM t_int)"),
        &SqlValue::Int(1),
        "1-col subquery propagates column affinity",
    );
    assert_eq_val(
        &eval_scalar("SELECT '01' = (SELECT x FROM t_int UNION SELECT 2)"),
        &SqlValue::Int(0),
        "compound subquery drops column affinity",
    );
    assert_eq_val(
        &eval_scalar("SELECT (SELECT 'a') = (SELECT x FROM t_nc)"),
        &SqlValue::Int(0),
        "1-col scalar subquery drops implicit collation",
    );
    assert_eq_val(
        &eval_scalar("SELECT (SELECT 'a', 'b') = (SELECT x, 'b' FROM t_nc)"),
        &SqlValue::Int(1),
        "multi-col row-value subquery retains collation",
    );
    assert_eq_val(
        &eval_scalar("SELECT CAST(x AS TEXT) = 'a' FROM t_nc"),
        &SqlValue::Int(1),
        "CAST preserves column collation",
    );
    assert_eq_val(
        &eval_scalar("SELECT ('b' || ('a' COLLATE NOCASE)) = 'BA'"),
        &SqlValue::Int(1),
        "explicit COLLATE propagates across binary ||",
    );
    assert_eq_val(
        &eval_scalar("SELECT (x || 'b') = 'ab' FROM t_nc"),
        &SqlValue::Int(0),
        "implicit column collation does NOT propagate across ||",
    );
    let comp = run(&cat, "SELECT 'a' UNION SELECT x FROM t_nc");
    assert_eq!(comp.rows.len(), 1, "uncollated 'a' yields to NOCASE in UNION");

    // 6. Duplicate columns in single subquery vs self-join ambiguity, row-value BETWEEN subquery.
    assert_eq_val(
        &eval_scalar("SELECT x FROM (SELECT 1 AS x, 2 AS x) AS s"),
        &SqlValue::Int(1),
        "duplicate col inside single subquery resolves to first",
    );
    let star_dup = run(&cat, "SELECT * FROM (SELECT 1 AS x, 2 AS x) AS s");
    assert_eq!(star_dup.rows[0].len(), 2);
    assert_eq_val(&star_dup.rows[0][0], &SqlValue::Int(1), "star col 0");
    assert_eq_val(&star_dup.rows[0][1], &SqlValue::Int(2), "star col 1");
    assert!(execute_query(
        &cat,
        "SELECT t_int.x FROM t_int JOIN t_int",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert_eq_val(
        &eval_scalar("SELECT (SELECT 1, 2) BETWEEN (1, 1) AND (1, 3)"),
        &SqlValue::Int(1),
        "multi-col subquery in BETWEEN",
    );

    // 7. 0-row aggregate/window rejection in WHERE/GROUP BY; recursive CTE rejects EXCEPT/INTERSECT.
    assert!(execute_query(
        &cat,
        "SELECT a FROM empty_t WHERE count(*) > 0",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "SELECT a FROM empty_t GROUP BY sum(a)",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "SELECT a FROM empty_t WHERE row_number() OVER () = 1",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "WITH RECURSIVE r(x) AS (SELECT 1 EXCEPT SELECT x+1 FROM r WHERE x < 3) SELECT * FROM r",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "WITH RECURSIVE r(x) AS (SELECT 1 INTERSECT SELECT x+1 FROM r WHERE x < 3) SELECT * FROM r",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());

    // 8. Hash equijoin under TEXT affinity matches BOTH `1 = 1.0` and `'2' = 2`.
    let hj = run(
        &cat,
        "SELECT j_left.k, j_right.k FROM j_left JOIN j_right ON j_left.k = j_right.k ORDER BY j_left.rowid",
    );
    assert_eq!(hj.rows.len(), 2, "TEXT hash equijoin must match 1=1.0 and '2'=2");
    assert_eq_val(&hj.rows[0][0], &SqlValue::Int(1), "num=num match");
    assert_eq_val(&hj.rows[1][0], &SqlValue::text("2"), "text=num match");

    // 9. Uncorrelated IN (SELECT ...) cache preserves 3-valued NULL logic for NOT IN.
    let in_res = run(
        &cat,
        "WITH vals(v) AS (VALUES (1), (2), (3)), s(x) AS (VALUES (1), (NULL))
         SELECT v,
                v IN (SELECT x FROM s),
                v NOT IN (SELECT x FROM s)
         FROM vals ORDER BY v",
    );
    assert_eq_val(&in_res.rows[0][1], &SqlValue::Int(1), "1 IN (1, NULL) -> 1");
    assert_eq_val(&in_res.rows[0][2], &SqlValue::Int(0), "1 NOT IN (1, NULL) -> 0");
    assert_eq_val(&in_res.rows[1][1], &SqlValue::Null, "2 IN (1, NULL) -> NULL");
    assert_eq_val(&in_res.rows[1][2], &SqlValue::Null, "2 NOT IN (1, NULL) -> NULL");
}

#[test]
fn test_pass4_audit_and_perf_pushdown_regressions() {
    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "nums",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("grp", Affinity::Integer),
            ColumnMeta::simple("r", Affinity::Real),
            ColumnMeta {
                name: "s".into(),
                affinity: Affinity::Text,
                collation: "NOCASE".into(),
                is_generated: false,
                is_hidden: false,
            },
        ],
        vec![
            vec![
                SqlValue::Int(1),
                SqlValue::Int(10),
                SqlValue::Real(-9223372036854775808.0),
                SqlValue::text("Alpha"),
            ],
            vec![
                SqlValue::Int(2),
                SqlValue::Int(10),
                SqlValue::Real(9007199254740992.0),
                SqlValue::text("beta"),
            ],
            vec![
                SqlValue::Int(3),
                SqlValue::Int(20),
                SqlValue::Null,
                SqlValue::text("GAMMA"),
            ],
        ],
    );
    cat.add_view("v_rand", "SELECT random() AS x", None);
    cat.add_view("v_ids", "SELECT id FROM nums", None);
    let eval_scalar = |sql: &str| run(&cat, sql).rows[0][0].clone();

    // 1. Exact i64::MIN / 2^53+1 on REAL index without lossy coercion or overflow.
    let q_min = run(&cat, "SELECT id FROM nums WHERE r = -9223372036854775808");
    assert_eq!(q_min.rows.len(), 1);
    assert_eq_val(&q_min.rows[0][0], &SqlValue::Int(1), "i64::MIN matches -2^63 REAL");
    let q_big = run(&cat, "SELECT id FROM nums WHERE r = '9007199254740993'");
    assert_eq!(q_big.rows.len(), 0, "2^53+1 must NOT match 2^53 REAL");

    // 2. Sliding window sum(5e18, 4e18, 5e18) uses inverse-before-step & i128 without overflow!
    let w_ov = run(
        &cat,
        "WITH w(id, v) AS (VALUES (1, 5000000000000000000), (2, 4000000000000000000), (3, 5000000000000000000))
         SELECT sum(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
    );
    assert_eq_val(&w_ov.rows[1][0], &SqlValue::Int(9_000_000_000_000_000_000), "5e18+4e18");
    assert_eq_val(&w_ov.rows[2][0], &SqlValue::Int(9_000_000_000_000_000_000), "4e18+5e18 no overflow");

    // 3. RANGE +Inf offset cancellation and exact i64::MAX + 1.0 boundary.
    let w_inf = run(
        &cat,
        "WITH w(x) AS (VALUES (1e309), (-1e309), (1.0))
         SELECT x, count(*) OVER (ORDER BY x RANGE BETWEEN 1e309 PRECEDING AND CURRENT ROW) FROM w ORDER BY x",
    );
    assert_eq_val(&w_inf.rows[2][1], &SqlValue::Int(3), "+Inf - +Inf includes all <= +Inf");
    let w_imax = run(
        &cat,
        "WITH w(x) AS (VALUES (9223372036854775806), (9223372036854775807))
         SELECT x, count(*) OVER (ORDER BY x RANGE BETWEEN CURRENT ROW AND 1.0 FOLLOWING) FROM w ORDER BY x",
    );
    assert_eq_val(&w_imax.rows[0][1], &SqlValue::Int(2), "i64::MAX-1 + 1.0 includes i64::MAX");

    // 4. Correlated WINDOW frame bound & parenthesized FROM alias `(nums) AS g`.
    let corr_win = run(
        &cat,
        "WITH o(k) AS (VALUES (0), (1)), v(x) AS (VALUES (1), (10))
         SELECT k, (SELECT max(s) FROM (
            SELECT sum(x) OVER (ORDER BY x ROWS BETWEEN o.k PRECEDING AND CURRENT ROW) AS s
            FROM v
         )) FROM o ORDER BY k",
    );
    assert_eq_val(&corr_win.rows[0][1], &SqlValue::Int(10), "k=0 window");
    assert_eq_val(&corr_win.rows[1][1], &SqlValue::Int(11), "k=1 window");
    let grp_from = run(&cat, "SELECT g.id FROM (nums) AS g WHERE g.id = 2");
    assert_eq_val(&grp_from.rows[0][0], &SqlValue::Int(2), "parenthesized FROM alias");

    // 5. Non-deterministic view is NOT cached across outer rows; `1 IN v_ids` view fallback works.
    let rnd_v = run(
        &cat,
        "SELECT count(DISTINCT (SELECT x FROM v_rand)) FROM nums",
    );
    assert_eq_val(&rnd_v.rows[0][0], &SqlValue::Int(3), "non-deterministic view not cached");
    let in_v = run(&cat, "SELECT 2 IN v_ids, 99 IN v_ids");
    assert_eq_val(&in_v.rows[0][0], &SqlValue::Int(1), "2 IN view");
    assert_eq_val(&in_v.rows[0][1], &SqlValue::Int(0), "99 IN view");

    // 6. ORDER BY validation runs even with LIMIT 0, including stacked COLLATE chains.
    assert!(execute_query(
        &cat,
        "SELECT id FROM nums ORDER BY nonexistent LIMIT 0",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    assert!(execute_query(
        &cat,
        "SELECT id FROM nums ORDER BY s COLLATE no_such_coll COLLATE BINARY LIMIT 0",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());

    // 7. CASE COLLATE propagation, single- vs multi-item IN COLLATE, and BETWEEN COLLATE.
    assert_eq_val(
        &eval_scalar("SELECT (CASE 1 WHEN 1 THEN 'a' COLLATE NOCASE END) = 'A'"),
        &SqlValue::Int(1),
        "CASE explicit COLLATE propagates",
    );
    assert_eq_val(
        &eval_scalar("SELECT 'a' IN ('A' COLLATE NOCASE)"),
        &SqlValue::Int(1),
        "1-item IN applies RHS COLLATE",
    );
    assert_eq_val(
        &eval_scalar("SELECT 'a' IN ('A' COLLATE NOCASE, 'B')"),
        &SqlValue::Int(0),
        "multi-item IN ignores RHS item COLLATE",
    );
    assert_eq_val(
        &eval_scalar("SELECT 'b' BETWEEN ('A' COLLATE NOCASE) AND ('C' COLLATE NOCASE)"),
        &SqlValue::Int(1),
        "BETWEEN respects operand COLLATE",
    );

    // 8. JSON intermediate containers (`json_set`/`json_insert` vs `json_replace`), `$[#-0]`, and `±Inf`.
    assert_eq_val(
        &eval_scalar("SELECT json_set('{}', '$.a.b', 1)"),
        &SqlValue::text(r#"{"a":{"b":1}}"#),
        "json_set creates missing intermediate object",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_replace('{}', '$.a.b', 1)"),
        &SqlValue::text("{}"),
        "json_replace does NOT create missing intermediate object",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_insert('[1,2]', '$[#-0]', 3)"),
        &SqlValue::text("[1,2,3]"),
        "json_insert $[#-0] appends",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_array(1e309, -1e309)"),
        &SqlValue::text("[9.0e+999,-9.0e+999]"),
        "json_array preserves ±Inf as ±9.0e+999",
    );

    // 9. Online single-pass GROUP BY + bitset/sorted IN (SELECT ...) + JOIN column pruning.
    let gb = run(
        &cat,
        "SELECT grp, count(*), sum(id), avg(id) FROM nums GROUP BY grp ORDER BY grp",
    );
    assert_eq!(gb.rows.len(), 2);
    assert_eq_val(&gb.rows[0][1], &SqlValue::Int(2), "grp 10 count");
    assert_eq_val(&gb.rows[0][2], &SqlValue::Int(3), "grp 10 sum");
    assert_eq_val(&gb.rows[0][3], &SqlValue::Real(1.5), "grp 10 avg");
    let in_float = run(
        &cat,
        "WITH s(v) AS (VALUES (1), (5), (100)) SELECT 5.0 IN (SELECT v FROM s), 5.5 IN (SELECT v FROM s)",
    );
    assert_eq_val(&in_float.rows[0][0], &SqlValue::Int(1), "5.0 IN bitset(1,5,100)");
    assert_eq_val(&in_float.rows[0][1], &SqlValue::Int(0), "5.5 NOT IN bitset(1,5,100)");
    let in_sparse = run(
        &cat,
        "WITH s(v) AS (VALUES (-1000000000000), (1000000000000)) SELECT 1000000000000 IN (SELECT v FROM s), 0 IN (SELECT v FROM s)",
    );
    assert_eq_val(&in_sparse.rows[0][0], &SqlValue::Int(1), "sparse IntSorted hit");
    assert_eq_val(&in_sparse.rows[0][1], &SqlValue::Int(0), "sparse IntSorted miss");
}

#[test]
fn test_pass5_sql_audit_json_zero_row_and_streaming_fastpaths() {
    use std::sync::Arc;
    use xdb_core::{BudgetMeter, LimitKind, ResourceLimits};

    let mut cat = MemoryCatalog::new();
    let eval_scalar = |sql: &str| -> SqlValue {
        let empty = MemoryCatalog::new();
        run(&empty, sql).rows[0][0].clone()
    };
    cat.add_table(
        "t",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("grp", Affinity::Integer),
            ColumnMeta::simple("val", Affinity::Integer),
            ColumnMeta::simple("j", Affinity::Text),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(10), SqlValue::Int(5), SqlValue::text("[1]")],
            vec![SqlValue::Int(2), SqlValue::Int(10), SqlValue::Int(20), SqlValue::text("[2]")],
            vec![SqlValue::Int(3), SqlValue::Int(20), SqlValue::Int(10), SqlValue::text("[3]")],
            vec![SqlValue::Int(4), SqlValue::Int(20), SqlValue::Null, SqlValue::text("[4]")],
        ],
    );
    cat.add_table(
        "g",
        vec![
            ColumnMeta::simple("grp", Affinity::Integer),
            ColumnMeta::simple("label", Affinity::Text),
        ],
        vec![
            vec![SqlValue::Int(10), SqlValue::text("ten")],
            vec![SqlValue::Int(20), SqlValue::text("twenty")],
        ],
    );
    cat.add_table(
        "small",
        vec![ColumnMeta::simple("v", Affinity::Integer)],
        vec![vec![SqlValue::Int(5)], vec![SqlValue::Int(10)]],
    );
    cat.add_table(
        "empty_t",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("j", Affinity::Text),
        ],
        vec![],
    );

    // 1. XDB `SELECT * REPLACE (...)` must NOT be bypassed by TopK/ORDER LIMIT pushdown!
    let xdb_opts = QueryOptions {
        dialect: xdb_parser::Dialect::Xdb,
        ..QueryOptions::default()
    };
    let rep_ord = execute_query(
        &cat,
        "SELECT * REPLACE (-val AS val) FROM t ORDER BY val DESC LIMIT 1",
        &QueryParams::empty(),
        &xdb_opts,
    )
    .unwrap();
    assert_eq_val(&rep_ord.rows[0][0], &SqlValue::Int(1), "* REPLACE(-val) DESC picks id=1 (-5)");
    let rep_agg = execute_query(
        &cat,
        "SELECT * REPLACE (sum(val) AS val) FROM t LIMIT 1",
        &QueryParams::empty(),
        &xdb_opts,
    )
    .unwrap();
    assert_eq_val(&rep_agg.rows[0][2], &SqlValue::Int(35), "* REPLACE(sum(val)) sums all rows");

    // 2. LIMIT 0 OFFSET N returns 0 rows cleanly.
    let lim0 = run(&cat, "SELECT id FROM t ORDER BY id LIMIT 0 OFFSET 10");
    assert!(lim0.rows.is_empty());

    // 3. Deep JSON path rejects cleanly instead of stack-overflowing.
    let deep_path = format!("${}", ".a".repeat(2000));
    let deep_sql = format!("SELECT json_extract('{{}}', '{deep_path}')");
    assert!(execute_query(&cat, &deep_sql, &QueryParams::empty(), &QueryOptions::default()).is_err());

    // 4. `json_set` / `json_insert` `$[len]` appends to array; `$[len+1]` is a no-op!
    assert_eq_val(
        &eval_scalar("SELECT json_set('[1,2]', '$[2]', 3)"),
        &SqlValue::text("[1,2,3]"),
        "json_set $[len] appends",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_insert('[1,2]', '$[2]', 3)"),
        &SqlValue::text("[1,2,3]"),
        "json_insert $[len] appends",
    );
    assert_eq_val(
        &eval_scalar("SELECT json_set('[1,2]', '$[3]', 4)"),
        &SqlValue::text("[1,2]"),
        "json_set $[len+1] is no-op",
    );
    assert_eq_val(
        &eval_scalar(r#"SELECT json_extract('{"a":1,"b":2}', '$.a', '$.b')"#),
        &SqlValue::text("[1,2]"),
        "json_extract multi-path",
    );
    // Large json_patch exercises O(N+M) HashMap-indexed merge
    let big_obj = (0..64)
        .map(|i| format!(r#""k{i}":{i}"#))
        .collect::<Vec<_>>()
        .join(",");
    let patch_sql = format!(r#"SELECT json_extract(json_patch('{{{big_obj}}}', '{{"k63":999,"k0":null}}'), '$.k63', '$.k0')"#);
    assert_eq_val(
        &eval_scalar(&patch_sql),
        &SqlValue::text("[999,null]"),
        "large json_patch merge",
    );

    // 5. Parse-time & JSON-aggregate `ResourceLimit` propagates as `SqlError::ResourceLimit`!
    let tiny_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000_000),
        max_memory_bytes: Some(4 * 1024),
        max_page_reads: None,
    }));
    let tiny_opts = QueryOptions {
        meter: Some(tiny_meter.clone()),
        ..QueryOptions::default()
    };
    let huge_vals = format!("VALUES {}", vec!["(1,2,3,4)"; 1500].join(","));
    let parse_lim = execute_query(&cat, &huge_vals, &QueryParams::empty(), &tiny_opts).unwrap_err();
    assert!(matches!(
        parse_lim,
        SqlError::ResourceLimit(r) if r.kind == LimitKind::Memory
    ));
    assert_eq!(tiny_meter.usage().memory_bytes, 0);

    // 6. 0-row column validation on JOIN ON, correlated TVF JOIN ON, aggregate ORDER BY, and WINDOWs!
    for bad in [
        "SELECT * FROM empty_t a JOIN empty_t b ON a.bogus = b.id",
        "SELECT * FROM empty_t LEFT JOIN json_each(empty_t.j) AS je ON bogus_col = 1",
        "SELECT count(*) FROM empty_t ORDER BY sum(bogus_col)",
        "SELECT id FROM empty_t WINDOW w AS (PARTITION BY bogus_col)",
        "SELECT sum(id) OVER (PARTITION BY bogus_col) FROM empty_t",
    ] {
        assert!(
            execute_query(&cat, bad, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected 0-row column validation error for `{bad}`"
        );
    }

    // 7. QueryParams::resolve normalizes NaN -> NULL.
    let nan_params = QueryParams::positional(vec![SqlValue::Real(f64::NAN)]);
    let nan_res = execute_query(&cat, "SELECT ?1 IS NULL", &nan_params, &QueryOptions::default()).unwrap();
    assert_eq_val(&nan_res.rows[0][0], &SqlValue::Int(1), "NaN param becomes NULL");

    // 8. Phase-B streaming fast paths: count(*) WHERE + IN, streaming GROUP BY, streaming JOIN, direct window!
    let c_in = run(&cat, "SELECT count(*) FROM t WHERE id <= 4 AND val IN (SELECT v FROM small)");
    assert_eq_val(&c_in.rows[0][0], &SqlValue::Int(2), "streaming count(*) WHERE IN");
    let c_not_in = run(&cat, "SELECT count(*) FROM t WHERE val NOT IN (SELECT v FROM small)");
    assert_eq_val(&c_not_in.rows[0][0], &SqlValue::Int(1), "streaming count(*) WHERE NOT IN (excludes NULL)");
    let c_empty_in = run(&cat, "SELECT count(*) FROM t WHERE val NOT IN (SELECT id FROM empty_t)");
    assert_eq_val(&c_empty_in.rows[0][0], &SqlValue::Int(4), "NOT IN empty subquery is true even for NULL");
    let c_bet = run(&cat, "SELECT count(*) FROM t WHERE id BETWEEN 2 AND 3");
    assert_eq_val(&c_bet.rows[0][0], &SqlValue::Int(2), "streaming count(*) WHERE BETWEEN");

    let s_gb = run(&cat, "SELECT grp, count(*), sum(val), avg(val) FROM t WHERE id >= 1 GROUP BY grp ORDER BY grp");
    assert_eq!(s_gb.rows.len(), 2);
    assert_eq_val(&s_gb.rows[0][2], &SqlValue::Int(25), "streaming GROUP BY grp=10 sum");
    assert_eq_val(&s_gb.rows[1][2], &SqlValue::Int(10), "streaming GROUP BY grp=20 sum (skips NULL)");

    let s_join = run(&cat, "SELECT t.id, t.val, g.label AS lbl FROM t JOIN g ON t.grp = g.grp");
    assert_eq!(s_join.rows.len(), 4);
    assert_eq!(s_join.columns, vec!["id", "val", "lbl"]);
    assert_eq_val(&s_join.rows[0][2], &SqlValue::text("ten"), "streaming JOIN row 0");
    assert_eq_val(&s_join.rows[3][2], &SqlValue::text("twenty"), "streaming JOIN row 3");

    let s_win = run(
        &cat,
        "SELECT id, sum(val) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
    );
    assert_eq_val(&s_win.rows[0][1], &SqlValue::Int(5), "sliding window row 0");
    assert_eq_val(&s_win.rows[1][1], &SqlValue::Int(25), "sliding window row 1");
    assert_eq_val(&s_win.rows[2][1], &SqlValue::Int(30), "sliding window row 2");
    assert_eq_val(&s_win.rows[3][1], &SqlValue::Int(10), "sliding window row 3 (with NULL)");
}

#[test]
fn test_pass6_sql_parity_validation_metering_and_perf_paths() {
    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "t",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("grp", Affinity::Integer),
            ColumnMeta::simple("val", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(10), SqlValue::Int(5)],
            vec![SqlValue::Int(2), SqlValue::Int(10), SqlValue::Int(20)],
            vec![SqlValue::Int(3), SqlValue::Int(20), SqlValue::Int(10)],
            vec![SqlValue::Int(4), SqlValue::Int(20), SqlValue::Null],
        ],
    );
    cat.add_table(
        "empty_t",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("val", Affinity::Integer),
        ],
        vec![],
    );
    cat.add_table(
        "s",
        vec![ColumnMeta::simple("v", Affinity::Integer)],
        vec![vec![SqlValue::Int(5)], vec![SqlValue::Int(10)]],
    );
    cat.add_table(
        "ovf",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("x", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(i64::MAX)],
            vec![SqlValue::Int(2), SqlValue::Int(1)],
            vec![SqlValue::Int(3), SqlValue::Int(-10)],
        ],
    );
    cat.add_table(
        "mix",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("x", Affinity::None),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Real(1.5)],
            vec![SqlValue::Int(2), SqlValue::Int(2)],
            vec![SqlValue::Int(3), SqlValue::Int(3)],
        ],
    );

    // H1: Unconditional column/subquery width validation even on 0-row tables & short-circuited AND.
    for bad in [
        "SELECT * FROM empty_t WHERE id IN (SELECT 1, 2)",
        "SELECT * FROM t WHERE 0 AND id IN (SELECT 1, 2)",
        "SELECT * FROM empty_t WHERE id IN (SELECT bogus FROM s)",
        "SELECT * FROM t WHERE 0 AND id IN (SELECT bogus FROM s)",
        "SELECT 0 AND bogus FROM t",
        "SELECT coalesce(1, bogus) FROM t",
        "SELECT NULL IN (bogus) FROM t",
        "SELECT * FROM empty_t WHERE bogus = 1",
    ] {
        assert!(
            execute_query(&cat, bad, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected validation error for `{bad}`"
        );
    }
    // Valid row-value subquery comparison must still succeed!
    assert_eq_val(
        &scalar("SELECT (SELECT 1, 2) = (SELECT 1, 2)"),
        &SqlValue::Int(1),
        "row-value subquery comparison",
    );

    // H2: ByteAccum step exhaustion does not leak reserved memory; CompiledRowPred & RowDedup meter.
    let step_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(20),
        max_memory_bytes: Some(16 * 1024 * 1024),
        max_page_reads: None,
    }));
    let step_opts = QueryOptions {
        meter: Some(step_meter.clone()),
        ..QueryOptions::default()
    };
    let err = execute_query(
        &cat,
        "SELECT printf('%100000s', 'x')",
        &QueryParams::empty(),
        &step_opts,
    )
    .unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit(r) if r.kind == LimitKind::Steps));
    assert_eq!(step_meter.usage().memory_bytes, 0, "ByteAccum must release memory on step limit");

    let dedup_meter = Arc::new(BudgetMeter::new(ResourceLimits {
        max_steps: Some(1_000_000),
        max_memory_bytes: Some(8 * 1024),
        max_page_reads: None,
    }));
    let dedup_opts = QueryOptions {
        meter: Some(dedup_meter.clone()),
        ..QueryOptions::default()
    };
    let err_d = execute_query(
        &cat,
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r WHERE x < 1000) SELECT DISTINCT printf('%200d', x) FROM r",
        &QueryParams::empty(),
        &dedup_opts,
    )
    .unwrap_err();
    assert!(matches!(err_d, SqlError::ResourceLimit(r) if r.kind == LimitKind::Memory));
    assert_eq!(dedup_meter.usage().memory_bytes, 0);

    // H3: `abs()` returns REAL for TEXT/BLOB (including `-9223372036854775808`), overflows only INT.
    assert_eq_val(&scalar("SELECT abs('-5')"), &SqlValue::Real(5.0), "abs('-5') -> REAL");
    assert_eq_val(&scalar("SELECT abs(X'2d35')"), &SqlValue::Real(5.0), "abs(blob) -> REAL");
    assert_eq_val(&scalar("SELECT abs('abc')"), &SqlValue::Real(0.0), "abs('abc') -> 0.0");
    assert!(matches!(
        scalar("SELECT abs('-9223372036854775808')"),
        SqlValue::Real(r) if r > 9.2e18
    ));
    assert!(execute_query(
        &cat,
        "SELECT abs(-9223372036854775808)",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());

    // H4: `sum()` uses strict numeric affinity on TEXT (never BLOB/partial), sticky approx, ovrfl.
    assert_eq_val(&scalar("SELECT sum('abc')"), &SqlValue::Real(0.0), "sum('abc') -> 0.0");
    assert_eq_val(&scalar("SELECT sum('5a')"), &SqlValue::Real(5.0), "sum('5a') -> 5.0");
    assert_eq_val(&scalar("SELECT sum(X'35')"), &SqlValue::Real(5.0), "sum(X'35') -> 5.0");
    assert!(execute_query(
        &cat,
        "SELECT sum(x) FROM ovf",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());
    let sticky_win = run(
        &cat,
        "SELECT id, sum(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW) FROM mix ORDER BY id",
    );
    assert_eq_val(&sticky_win.rows[0][1], &SqlValue::Real(1.5), "window row 0 real");
    assert_eq_val(&sticky_win.rows[1][1], &SqlValue::Real(2.0), "window row 1 stays REAL (sticky approx)");
    assert_eq_val(&sticky_win.rows[2][1], &SqlValue::Real(3.0), "window row 2 stays REAL (sticky approx)");

    // H5: Math functions & `sign` return NULL on BLOB.
    assert_eq_val(&scalar("SELECT sign(X'31')"), &SqlValue::Null, "sign(blob) -> NULL");
    assert_eq_val(&scalar("SELECT sqrt(X'34')"), &SqlValue::Null, "sqrt(blob) -> NULL");
    assert_eq_val(&scalar("SELECT ceil(X'31')"), &SqlValue::Null, "ceil(blob) -> NULL");

    // H6: `percentile_disc` floor formula & Inf/non-numeric rejection across all 4 percentile kinds.
    assert_eq_val(
        &scalar("WITH d(v) AS (VALUES (10),(20),(30),(40)) SELECT percentile_disc(v, 0.5) FROM d"),
        &SqlValue::Real(20.0),
        "percentile_disc floor((4-1)*0.5) = idx 1 -> 20.0",
    );
    for bad_pct in [
        "WITH d(v) AS (VALUES ('abc')) SELECT median(v) FROM d",
        "WITH d(v) AS (VALUES (X'31')) SELECT percentile(v, 50) FROM d",
        "WITH d(v) AS (VALUES (9e999)) SELECT percentile_cont(v, 0.5) FROM d",
        "WITH d(v) AS (VALUES (9e999)) SELECT percentile_disc(v, 0.5) FROM d",
    ] {
        assert!(
            execute_query(&cat, bad_pct, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected percentile error for `{bad_pct}`"
        );
    }

    // H7: `WITH RECURSIVE` LIMIT 0 OFFSET N halts immediately; trailing non-recursive branch errors.
    let zero_lim = run(
        &cat,
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r LIMIT 0 OFFSET 1000000) SELECT * FROM r",
    );
    assert!(zero_lim.rows.is_empty());
    assert!(execute_query(
        &cat,
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r WHERE x < 3 UNION ALL SELECT 99) SELECT * FROM r",
        &QueryParams::empty(),
        &QueryOptions::default()
    )
    .is_err());

    // H8: Compound collation uses leftmost non-None collation (`NOCASE` beats right `BINARY`).
    let cmp_coll = run(
        &cat,
        "SELECT 'a' COLLATE NOCASE UNION SELECT 'A' COLLATE BINARY",
    );
    assert_eq!(cmp_coll.rows.len(), 1, "leftmost NOCASE collation must dedupe 'a' and 'A'");

    // M1: JSON `->` treats REAL as object key and supports `'[N]'` shorthand.
    assert_eq_val(
        &scalar(r#"SELECT json('{"1.0":"obj"}') -> 1.0"#),
        &SqlValue::text("\"obj\""),
        "JSON -> 1.0 is object key",
    );
    assert_eq_val(
        &scalar("SELECT json('[10,20,30]') -> '[1]'"),
        &SqlValue::text("20"),
        "JSON -> '[1]' array index shorthand",
    );

    // M2: NUL handling in `substr`/`trim`/`replace` & strict float literal in `likelihood`.
    assert_eq_val(
        &scalar("SELECT substr('a' || char(0) || 'b', 1, 5)"),
        &SqlValue::text("a"),
        "substr stops at NUL",
    );
    assert_eq_val(
        &scalar("SELECT trim('abz', 'a' || char(0) || 'z')"),
        &SqlValue::text("bz"),
        "trim cutset stops at NUL",
    );
    assert_eq_val(
        &scalar("SELECT likelihood(42, 0.5)"),
        &SqlValue::Int(42),
        "likelihood valid float literal",
    );
    for bad_like in [
        "SELECT likelihood(42, 1)",
        "SELECT likelihood(42, 1.5)",
        "SELECT likelihood(42, 0.2 + 0.3)",
    ] {
        assert!(
            execute_query(&cat, bad_like, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected likelihood error for `{bad_like}`"
        );
    }

    // M3: 0-row window validation (bad name, wrong arity, unknown collation, RANGE >1 ORDER BY).
    for bad_win in [
        "SELECT abs(id) OVER () FROM empty_t",
        "SELECT row_number(1) OVER () FROM empty_t",
        "SELECT sum(id) OVER (ORDER BY id COLLATE no_such_coll) FROM empty_t",
        "SELECT sum(id) OVER (ORDER BY id, val RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM empty_t",
        "SELECT sum(id) OVER (ORDER BY id ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM t",
    ] {
        assert!(
            execute_query(&cat, bad_win, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected 0-row window error for `{bad_win}`"
        );
    }

    // L3: `GROUP BY` aggregate alias reports SQLite error.
    let gb_err = execute_query(
        &cat,
        "SELECT sum(val) AS s FROM t GROUP BY s",
        &QueryParams::empty(),
        &QueryOptions::default(),
    )
    .unwrap_err();
    assert!(gb_err.to_string().contains("aggregate functions are not allowed in the GROUP BY"));

    // Pass-6 performance paths: streaming SELECT WHERE, columnar cumulative window, shared SumAcc.
    let sw = run(&cat, "SELECT id, val FROM t WHERE grp = 10");
    assert_eq!(sw.rows.len(), 2);
    assert_eq_val(&sw.rows[0][1], &SqlValue::Int(5), "streaming select row 0");
    assert_eq_val(&sw.rows[1][1], &SqlValue::Int(20), "streaming select row 1");

    let cw = run(&cat, "SELECT id, sum(val) OVER (ORDER BY id) FROM t");
    assert_eq!(cw.rows.len(), 4);
    assert_eq_val(&cw.rows[0][1], &SqlValue::Int(5), "columnar cumulative win 0");
    assert_eq_val(&cw.rows[1][1], &SqlValue::Int(25), "columnar cumulative win 1");
    assert_eq_val(&cw.rows[2][1], &SqlValue::Int(35), "columnar cumulative win 2");
    assert_eq_val(&cw.rows[3][1], &SqlValue::Int(35), "columnar cumulative win 3");

    let shared_gb = run(
        &cat,
        "SELECT grp, count(*), sum(val), avg(val), total(val) FROM t GROUP BY grp ORDER BY grp",
    );
    assert_eq!(shared_gb.rows.len(), 2);
    assert_eq_val(&shared_gb.rows[0][2], &SqlValue::Int(25), "shared SumAcc sum");
    assert_eq_val(&shared_gb.rows[0][3], &SqlValue::Real(12.5), "shared SumAcc avg");
    assert_eq_val(&shared_gb.rows[0][4], &SqlValue::Real(25.0), "shared SumAcc total");
}

#[test]
fn test_pass7_audit_validation_json5_unhex_and_streaming_fastpaths() {
    use xdb_sql::{ScannedRow, TableMeta};

    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "u",
        vec![ColumnMeta::simple("a", Affinity::Integer)],
        vec![vec![SqlValue::Int(10)], vec![SqlValue::Int(20)]],
    );
    cat.add_table(
        "empty_u",
        vec![ColumnMeta::simple("a", Affinity::Integer)],
        vec![],
    );
    // Table with explicit IPK (`ipk_column: Some(0)`) to exercise streaming cumulative window fast path.
    cat.add_table_with_meta(
        TableMeta {
            name: "wtab".to_string(),
            columns: vec![
                ColumnMeta::simple("id", Affinity::Integer),
                ColumnMeta::simple("grp", Affinity::Integer),
                ColumnMeta::simple("val", Affinity::Integer),
            ],
            has_rowid: true,
            ipk_column: Some(0),
            indexes: Vec::new(),
        },
        vec![
            ScannedRow {
                rowid: Some(1),
                values: vec![SqlValue::Int(1), SqlValue::Int(1), SqlValue::Int(10)],
            },
            ScannedRow {
                rowid: Some(2),
                values: vec![SqlValue::Int(2), SqlValue::Int(2), SqlValue::Int(25)],
            },
            ScannedRow {
                rowid: Some(3),
                values: vec![SqlValue::Int(3), SqlValue::Int(1), SqlValue::Null],
            },
            ScannedRow {
                rowid: Some(4),
                values: vec![SqlValue::Int(4), SqlValue::Int(3), SqlValue::Int(5)],
            },
        ],
    );
    cat.add_table(
        "rdup",
        vec![
            ColumnMeta::simple("k", Affinity::Integer),
            ColumnMeta::simple("lbl", Affinity::Text),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::text("g1_a")],
            vec![SqlValue::Int(1), SqlValue::text("g1_b")],
            vec![SqlValue::Int(2), SqlValue::text("g2_only")],
        ],
    );
    cat.add_table(
        "small_v",
        vec![ColumnMeta::simple("v", Affinity::Integer)],
        vec![vec![SqlValue::Int(10)], vec![SqlValue::Int(5)]],
    );

    // Bug #1: `build_from_schema_only` exposes synthetic `rowid`/`_rowid_`/`oid` for subqueries on 0-row tables.
    for q in [
        "SELECT 1 FROM empty_u WHERE (SELECT rowid FROM u LIMIT 1) = 1",
        "SELECT 1 FROM empty_u WHERE (SELECT _rowid_ FROM u LIMIT 1) = 1",
        "SELECT 1 FROM empty_u WHERE (SELECT oid FROM u LIMIT 1) = 1",
        "SELECT 1 FROM empty_u WHERE EXISTS (SELECT 1 FROM u WHERE rowid = 1)",
    ] {
        let r = run(&cat, q);
        assert!(r.rows.is_empty(), "query `{q}` must validate and return 0 rows");
    }

    // Bug #2: Lazy `CompiledRowPred` `IN (SELECT ...)` never triggers runtime errors on 0-row tables
    // or short-circuited `AND` branches, while compile-time width mismatches STILL error on 0-row tables!
    let lazy_zero = run(
        &cat,
        "SELECT count(*) FROM empty_u WHERE a IN (SELECT json('{bad}') FROM u)",
    );
    assert_eq_val(&lazy_zero.rows[0][0], &SqlValue::Int(0), "lazy IN on 0-row count");
    let lazy_sc = run(
        &cat,
        "SELECT count(*) FROM u WHERE a = 999 AND a IN (SELECT json('{bad}') FROM u)",
    );
    assert_eq_val(
        &lazy_sc.rows[0][0],
        &SqlValue::Int(0),
        "lazy IN short-circuited by false AND",
    );
    assert!(
        execute_query(
            &cat,
            "SELECT count(*) FROM empty_u WHERE a IN (SELECT 1, 2 FROM u)",
            &QueryParams::empty(),
            &QueryOptions::default()
        )
        .is_err(),
        "sub-select width mismatch must still error on 0-row table"
    );

    // Bug #4: `try_streaming_group_by` validates non-direct SELECT / ORDER BY expressions on empty & non-empty tables.
    for bad_gb in [
        "SELECT a, count(*), bogus_col FROM empty_u GROUP BY a",
        "SELECT a, count(*) FROM empty_u GROUP BY a ORDER BY bogus_col",
        "SELECT a, count(*), (SELECT 1, 2 FROM u) FROM u GROUP BY a",
    ] {
        assert!(
            execute_query(&cat, bad_gb, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "streaming GROUP BY must validate `{bad_gb}`"
        );
    }

    // Bug #5: XDB `* REPLACE (window_fn AS col)` duplicate-span columnar window must not panic or null out!
    let rep = run_xdb(
        &cat,
        "SELECT * REPLACE (sum(val) OVER (ORDER BY id) AS val) FROM wtab",
    );
    assert_eq!(rep.rows.len(), 4);
    assert_eq_val(&rep.rows[0][2], &SqlValue::Int(10), "* REPLACE win row 0");
    assert_eq_val(&rep.rows[1][2], &SqlValue::Int(35), "* REPLACE win row 1");
    assert_eq_val(&rep.rows[2][2], &SqlValue::Int(35), "* REPLACE win row 2");
    assert_eq_val(&rep.rows[3][2], &SqlValue::Int(40), "* REPLACE win row 3");

    // Bug #6: `eval_single_aggregate` DISTINCT/ORDER BY enforces `BudgetMeter` memory/step limits.
    let big_rows: Vec<Vec<SqlValue>> = (0..1000)
        .map(|i| vec![SqlValue::text(format!("val_{i:06}_payload_padding_bytes"))])
        .collect();
    cat.add_table("agg_m", vec![ColumnMeta::simple("s", Affinity::Text)], big_rows);
    let tight_mem = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(2048),
            ..ResourceLimits::default()
        }))),
        ..QueryOptions::default()
    };
    let err = execute_query(
        &cat,
        "SELECT count(DISTINCT s) FROM agg_m",
        &QueryParams::empty(),
        &tight_mem,
    )
    .unwrap_err();
    assert!(
        matches!(err, SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory),
        "expected Memory limit on DISTINCT aggregate, got {err:?}"
    );

    // Bug #7: 0-row `Between`, `InList`, and stray `Vector` row-value misuse validation (while valid passes).
    for bad_rv in [
        "SELECT 1 FROM empty_u WHERE (a, a) BETWEEN 1 AND 2",
        "SELECT 1 FROM empty_u WHERE (a, a) IN (1, 2)",
        "SELECT 1 FROM empty_u WHERE (1, 2) + 1",
    ] {
        assert!(
            execute_query(&cat, bad_rv, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected row-value misuse error for `{bad_rv}`"
        );
    }
    let valid_rv = run(&cat, "SELECT (1, 2) IN ((1, 2), (3, 4))");
    assert_eq_val(&valid_rv.rows[0][0], &SqlValue::Int(1), "valid row-value IN list");

    // Bug #9: JSON5 case-insensitive `NaN`/`QNaN`/`SNaN` and `[+|-]Inf[inity]`.
    assert_eq_val(&scalar("SELECT json('NaN')"), &SqlValue::text("null"), "JSON5 NaN");
    assert_eq_val(&scalar("SELECT json('nan')"), &SqlValue::text("null"), "JSON5 nan");
    assert_eq_val(&scalar("SELECT json('QNaN')"), &SqlValue::text("null"), "JSON5 QNaN");
    assert_eq_val(&scalar("SELECT json('SNaN')"), &SqlValue::text("null"), "JSON5 SNaN");
    assert_eq_val(&scalar("SELECT json('Inf')"), &SqlValue::text("9.0e+999"), "JSON5 Inf");
    assert_eq_val(&scalar("SELECT json('+Infinity')"), &SqlValue::text("9.0e+999"), "JSON5 +Infinity");
    assert_eq_val(&scalar("SELECT json('-inf')"), &SqlValue::text("-9.0e+999"), "JSON5 -inf");
    assert_eq_val(&scalar("SELECT json('-INFINITY')"), &SqlValue::text("-9.0e+999"), "JSON5 -INFINITY");
    assert_eq_val(&scalar("SELECT json_extract('Inf', '$')"), &SqlValue::Real(f64::INFINITY), "JSON5 extract Inf");
    assert_eq_val(&scalar("SELECT json_extract('-INFINITY', '$')"), &SqlValue::Real(f64::NEG_INFINITY), "JSON5 extract -INFINITY");
    assert_eq_val(&scalar("SELECT json_valid('NaN', 1)"), &SqlValue::Int(0), "RFC8259 rejects NaN");
    assert_eq_val(&scalar("SELECT json_valid('NaN', 2)"), &SqlValue::Int(1), "JSON5 accepts NaN");
    assert_eq_val(&scalar("SELECT json_valid('+NaN', 2)"), &SqlValue::Int(0), "JSON5 rejects +NaN");
    assert_eq_val(&scalar("SELECT json_valid('-NaN', 2)"), &SqlValue::Int(0), "JSON5 rejects -NaN");

    // Bug #11: `unhex` stops at first embedded NUL byte.
    assert_eq_val(
        &scalar("SELECT hex(unhex('4142' || char(0) || '4344'))"),
        &SqlValue::text("4142"),
        "unhex stops at NUL",
    );

    // Perf Wave 3 fast paths:
    // 1. Single-pass `try_streaming_cumulative_window` (`sum`, `count`, `avg`, `total` + WHERE).
    let swin = run(
        &cat,
        "SELECT id, sum(val) OVER (ORDER BY id), count(val) OVER (ORDER BY id), avg(val) OVER (ORDER BY id) FROM wtab WHERE id <= 4",
    );
    assert_eq!(swin.rows.len(), 4);
    assert_eq_val(&swin.rows[0][1], &SqlValue::Int(10), "stream win sum 0");
    assert_eq_val(&swin.rows[1][1], &SqlValue::Int(35), "stream win sum 1");
    assert_eq_val(&swin.rows[2][1], &SqlValue::Int(35), "stream win sum 2 (null skipped)");
    assert_eq_val(&swin.rows[2][2], &SqlValue::Int(2), "stream win count 2");
    assert_eq_val(&swin.rows[3][1], &SqlValue::Int(40), "stream win sum 3");
    assert_eq_val(&swin.rows[3][2], &SqlValue::Int(3), "stream win count 3");

    // 2. Tagged flat small-int `try_streaming_hash_join` (unique + duplicate keys + LEFT JOIN).
    let hj = run(
        &cat,
        "SELECT w.id, r.lbl FROM wtab w LEFT JOIN rdup r ON w.grp = r.k",
    );
    assert_eq!(hj.rows.len(), 6); // id=1 -> 2 matches, id=2 -> 1 match, id=3 -> 2 matches, id=4 -> 1 NULL match
    assert_eq_val(&hj.rows[5][0], &SqlValue::Int(4), "LEFT JOIN unmatched id=4");
    assert_eq_val(&hj.rows[5][1], &SqlValue::Null, "LEFT JOIN unmatched NULL");

    // 3. Direct `try_streaming_group_by` emission (no ORDER BY) + hoisted `ReadyIntBitset` + fast `SELECT v FROM small_v`.
    let gb_dir = run(
        &cat,
        "SELECT grp, count(*), sum(val) FROM wtab GROUP BY grp",
    );
    assert_eq!(gb_dir.rows.len(), 3);
    let in_cnt = run(
        &cat,
        "SELECT count(*) FROM wtab WHERE id <= 4 AND val IN (SELECT v FROM small_v)",
    );
    assert_eq_val(&in_cnt.rows[0][0], &SqlValue::Int(2), "hoisted ReadyIntBitset IN count");

    // 4. Fast `JsonParser::parse_string` (plain ASCII + UTF-8 + escaped).
    assert_eq_val(
        &scalar(r#"SELECT json_extract('{"plain":"hello_world","esc":"a\nb\"c","u":"héllo"}', '$.plain')"#),
        &SqlValue::text("hello_world"),
        "fast plain JSON string",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_extract('{"plain":"hello_world","esc":"a\nb\"c","u":"héllo"}', '$.esc')"#),
        &SqlValue::text("a\nb\"c"),
        "escaped JSON string",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_extract('{"plain":"hello_world","esc":"a\nb\"c","u":"héllo"}', '$.u')"#),
        &SqlValue::text("héllo"),
        "UTF-8 JSON string",
    );
}

#[test]
fn test_pass8_audit_parity_metering_and_streaming_fastpaths() {
    use xdb_core::{BudgetMeter, LimitKind, ResourceLimits};

    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "g",
        vec![
            ColumnMeta::simple("grp", Affinity::Integer),
            ColumnMeta::simple("val", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(10)],
            vec![SqlValue::Int(1), SqlValue::Int(20)],
            vec![SqlValue::Int(2), SqlValue::Int(5)],
            vec![SqlValue::Int(3), SqlValue::Int(50)],
        ],
    );
    cat.add_table("empty_t", vec![ColumnMeta::simple("x", Affinity::Integer)], vec![]);

    // 1. Streaming `GROUP BY ... ORDER BY <output alias>` works cleanly!
    let gb_ord = run(
        &cat,
        "SELECT grp, count(*) AS cnt, sum(val) AS tot FROM g GROUP BY grp ORDER BY tot DESC",
    );
    assert_eq!(gb_ord.rows.len(), 3);
    assert_eq_val(&gb_ord.rows[0][0], &SqlValue::Int(3), "top group");
    assert_eq_val(&gb_ord.rows[0][2], &SqlValue::Int(50), "top sum");
    assert_eq_val(&gb_ord.rows[1][0], &SqlValue::Int(1), "second group");
    assert_eq_val(&gb_ord.rows[2][0], &SqlValue::Int(2), "third group");

    // 2. Unsorted / duplicate IPK in `MemoryCatalog` falls back cleanly in cumulative window!
    let uns_meta = TableMeta {
        name: "uns_w".to_string(),
        columns: vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("v", Affinity::Integer),
        ],
        has_rowid: true,
        ipk_column: Some(0),
        indexes: Vec::new(),
    };
    cat.add_table_with_meta(
        uns_meta,
        vec![
            ScannedRow {
                rowid: Some(3),
                values: vec![SqlValue::Int(3), SqlValue::Int(30)],
            },
            ScannedRow {
                rowid: Some(1),
                values: vec![SqlValue::Int(1), SqlValue::Int(10)],
            },
            ScannedRow {
                rowid: Some(2),
                values: vec![SqlValue::Int(2), SqlValue::Int(20)],
            },
            ScannedRow {
                rowid: Some(2),
                values: vec![SqlValue::Int(2), SqlValue::Int(5)],
            },
        ],
    );
    let wres = run(
        &cat,
        "SELECT id, sum(v) OVER (ORDER BY id) FROM uns_w",
    );
    assert_eq!(wres.rows.len(), 4);
    let mut wpairs: Vec<(i64, i64)> = wres
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (SqlValue::Int(id), SqlValue::Int(s)) => (*id, *s),
            other => panic!("unexpected window row {other:?}"),
        })
        .collect();
    wpairs.sort_by_key(|&(id, _)| id);
    // Peers at id=2 share default RANGE frame total = 10 + 20 + 5 = 35; id=3 gets 65!
    assert_eq!(wpairs, vec![(1, 10), (2, 35), (2, 35), (3, 65)]);

    // 3. `temp.tbl.col` qualification works in SELECT, WHERE, JOIN, and CompiledRowPred.
    let tq = run(&cat, "SELECT temp.g.grp, temp.g.val FROM g WHERE temp.g.grp = 2");
    assert_eq!(tq.rows.len(), 1);
    assert_eq_val(&tq.rows[0][1], &SqlValue::Int(5), "temp.g.val");

    // 4. `ORDER BY` rejected in scalar functions AND window functions.
    assert!(
        execute_query(&cat, "SELECT abs(-5 ORDER BY 1)", &QueryParams::empty(), &QueryOptions::default())
            .is_err()
    );
    assert!(
        execute_query(
            &cat,
            "SELECT sum(val ORDER BY val) OVER () FROM g",
            &QueryParams::empty(),
            &QueryOptions::default()
        )
        .is_err()
    );

    // 5. Multi-path `json_extract` with NULL path returns SQL NULL; `->` / `json_extract`
    //    path validation parity with C SQLite 3.53.3!
    assert_eq_val(
        &scalar(r#"SELECT json_extract('{"a":1,"b":2}', '$.a', NULL)"#),
        &SqlValue::Null,
        "multi-path json_extract NULL",
    );
    assert_eq_val(
        &scalar(r#"SELECT '{"a":1}' -> '$[foo]'"#),
        &SqlValue::Null,
        "object -> '$[foo]' returns NULL",
    );
    assert_eq_val(
        &scalar(r#"SELECT '{"a":1}' -> '[foo]'"#),
        &SqlValue::Null,
        "object -> '[foo]' returns NULL",
    );
    for bad_jp in [
        r#"SELECT '[1,2]' -> '$[foo]'"#,
        r#"SELECT json_extract('{"a":1}', '$a')"#,
        r#"SELECT json_extract('{"a":1}', '$.')"#,
    ] {
        assert!(
            execute_query(&cat, bad_jp, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected bad JSON path error for `{bad_jp}`"
        );
    }

    // 6. `QueryOptions::case_sensitive_like` propagated to `like()` scalar function.
    let cs_opts = QueryOptions {
        case_sensitive_like: true,
        ..QueryOptions::default()
    };
    let cs_no = execute_query(&cat, "SELECT like('a', 'A')", &QueryParams::empty(), &cs_opts).unwrap();
    assert_eq_val(&cs_no.rows[0][0], &SqlValue::Int(0), "case_sensitive like('a','A')");
    let cs_yes = execute_query(&cat, "SELECT like('a', 'a')", &QueryParams::empty(), &cs_opts).unwrap();
    assert_eq_val(&cs_yes.rows[0][0], &SqlValue::Int(1), "case_sensitive like('a','a')");

    // 7. Duplicate CTE name rejected.
    let dup_cte = execute_query(
        &cat,
        "WITH a AS (SELECT 1), a AS (SELECT 2) SELECT * FROM a",
        &QueryParams::empty(),
        &QueryOptions::default(),
    );
    assert!(dup_cte.is_err());

    // 8. Single-element `IN (col)` collation matches C SQLite 3.53.3!
    cat.add_table(
        "nc",
        vec![ColumnMeta {
            name: "a".to_string(),
            affinity: Affinity::Text,
            collation: "NOCASE".to_string(),
            is_generated: false,
            is_hidden: false,
        }],
        vec![vec![SqlValue::text("a")]],
    );
    let in_col = run(
        &cat,
        "SELECT 'A' IN (a), 'A' IN (a COLLATE NOCASE), 'A' IN ('a' COLLATE NOCASE), a IN ('A') FROM nc",
    );
    assert_eq_val(&in_col.rows[0][0], &SqlValue::Int(0), "'A' IN (a)");
    assert_eq_val(&in_col.rows[0][1], &SqlValue::Int(0), "'A' IN (a COLLATE NOCASE)");
    assert_eq_val(&in_col.rows[0][2], &SqlValue::Int(1), "'A' IN ('a' COLLATE NOCASE)");
    assert_eq_val(&in_col.rows[0][3], &SqlValue::Int(1), "a IN ('A')");

    // 9. `json_valid(X, FLAGS)` enforces 1..=15; `json_error_position` returns UTF-8 char offset.
    assert!(
        execute_query(&cat, "SELECT json_valid('1', 0)", &QueryParams::empty(), &QueryOptions::default())
            .is_err()
    );
    assert!(
        execute_query(&cat, "SELECT json_valid('1', 16)", &QueryParams::empty(), &QueryOptions::default())
            .is_err()
    );
    assert!(
        execute_query(&cat, "SELECT json_valid('1', -1)", &QueryParams::empty(), &QueryOptions::default())
            .is_err()
    );
    assert_eq_val(&scalar("SELECT json_valid('1', 15)"), &SqlValue::Int(1), "json_valid flags=15");
    assert_eq_val(
        &scalar(r#"SELECT json_error_position('{"é": ]}')"#),
        &SqlValue::Int(7),
        "json_error_position 2-byte UTF-8 char offset",
    );
    assert_eq_val(
        &scalar(r#"SELECT json_error_position('{"😀": ]}')"#),
        &SqlValue::Int(7),
        "json_error_position 4-byte UTF-8 char offset",
    );

    // 10. `json_patch` null removes ONLY the first matching duplicate key (small & >32-key objects).
    assert_eq_val(
        &scalar(r#"SELECT json_patch('{"a":1,"a":2}', '{"a":null}')"#),
        &SqlValue::text(r#"{"a":2}"#),
        "json_patch dup key small",
    );
    let mut large_obj = String::from(r#"{"dup":1,"dup":2"#);
    for i in 0..40 {
        large_obj.push_str(&format!(r#","k{i}":{i}"#));
    }
    large_obj.push('}');
    let patch_large_q = format!("SELECT json_extract(json_patch('{large_obj}', '{{\"dup\":null}}'), '$.dup')");
    assert_eq_val(&scalar(&patch_large_q), &SqlValue::Int(2), "json_patch dup key large HashMap");

    // 11. 0-row / `FILTER (WHERE 0)` compile-time aggregate/window validation.
    for bad_zero in [
        "SELECT sum(count(*)) FROM empty_t",
        "SELECT sum(x) FROM empty_t GROUP BY 1",
        "SELECT sum(x) FROM empty_t GROUP BY x HAVING sum(x) OVER () > 0",
        "SELECT sum(row_number() OVER ()) OVER () FROM empty_t",
        "SELECT sum(count(*)) FILTER (WHERE 0) FROM g",
    ] {
        assert!(
            execute_query(&cat, bad_zero, &QueryParams::empty(), &QueryOptions::default()).is_err(),
            "expected compile-time agg/window error for `{bad_zero}`"
        );
    }

    // 12. 65+ argument `json_array(..., json('[1,2]'))` preserves JSON subtype past arg 63.
    let args66 = (0..65)
        .map(|i| i.to_string())
        .chain(std::iter::once("json('[1,2]')".to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let arr66_sql = format!("SELECT json_extract(json_array({args66}), '$[65][1]')");
    assert_eq_val(&scalar(&arr66_sql), &SqlValue::Int(2), "json_array arg 65 subtype preserved");

    // 13. Memory metering on `StreamedGroupBy::Direct`, `trim`, and no-op `replace`.
    let many_grps: Vec<Vec<SqlValue>> = (0..500)
        .map(|i| vec![SqlValue::Int(i), SqlValue::text("p".repeat(64))])
        .collect();
    cat.add_table(
        "mg",
        vec![
            ColumnMeta::simple("k", Affinity::Integer),
            ColumnMeta::simple("s", Affinity::Text),
        ],
        many_grps,
    );
    let tight_gb = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(4096),
            ..ResourceLimits::default()
        }))),
        ..QueryOptions::default()
    };
    let gb_err = execute_query(
        &cat,
        "SELECT k, count(*) FROM mg GROUP BY k",
        &QueryParams::empty(),
        &tight_gb,
    )
    .unwrap_err();
    assert!(matches!(gb_err, SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory));

    let tight_str = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(512),
            ..ResourceLimits::default()
        }))),
        ..QueryOptions::default()
    };
    let trim_sql = format!("SELECT trim('{}')", "a".repeat(2000));
    let repl_sql = format!("SELECT replace('{}', 'z', 'y')", "a".repeat(2000));
    assert!(matches!(
        execute_query(&cat, &trim_sql, &QueryParams::empty(), &tight_str).unwrap_err(),
        SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory
    ));
    assert!(matches!(
        execute_query(&cat, &repl_sql, &QueryParams::empty(), &tight_str).unwrap_err(),
        SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory
    ));

    // 14. `SumAcc` `i_sum_64 + i_sum_hi` overflow, sliding window inverse, and `total`/`avg` accuracy!
    cat.add_table(
        "ov",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("x", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(i64::MAX)],
            vec![SqlValue::Int(2), SqlValue::Int(10)],
            vec![SqlValue::Int(3), SqlValue::Int(5)],
        ],
    );
    assert!(
        execute_query(&cat, "SELECT sum(x) FROM ov", &QueryParams::empty(), &QueryOptions::default())
            .is_err()
    );
    let tot_ov = run(&cat, "SELECT total(x), avg(x) FROM ov");
    assert!(matches!(tot_ov.rows[0][0], SqlValue::Real(r) if r > 9.0e18));
    assert!(matches!(tot_ov.rows[0][1], SqlValue::Real(r) if r > 3.0e18));
    // Sliding window frame whose 2nd window overflows (i64::MAX + 10) and 3rd window slides
    // past i64::MAX back to 10 + 5 = 15 (tests `inverse_value` hi-borrow cancellation!).
    let sw_ov = run(
        &cat,
        "SELECT id, total(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM ov",
    );
    assert_eq_val(&sw_ov.rows[2][1], &SqlValue::Real(15.0), "sliding window inverse exact after overflow");
}

#[test]
fn test_pass9_audit_having_in_collation_kbn_utf16_hash_metering_and_subq_validation() {
    let mut cat = MemoryCatalog::new();
    cat.add_table(
        "t",
        vec![
            ColumnMeta::simple("id", Affinity::Integer),
            ColumnMeta::simple("x", Affinity::Integer),
            ColumnMeta {
                name: "nc".to_string(),
                affinity: Affinity::Text,
                collation: "NOCASE".to_string(),
                is_hidden: false,
                is_generated: false,
            },
            ColumnMeta::simple("true", Affinity::Integer),
        ],
        vec![
            vec![SqlValue::Int(1), SqlValue::Int(10), SqlValue::text("abc"), SqlValue::Int(7)],
            vec![SqlValue::Int(2), SqlValue::Int(20), SqlValue::text("XYZ"), SqlValue::Int(8)],
        ],
    );
    cat.add_table(
        "empty_t",
        vec![ColumnMeta::simple("x", Affinity::Integer)],
        vec![],
    );

    // 1. `HAVING` without `GROUP BY` rejected when aggregate only appears in `HAVING` or `ORDER BY`,
    //    and 0-row `HAVING` validates explicit SELECT aliases cleanly.
    assert!(
        execute_query(&cat, "SELECT 1 FROM t HAVING count(*) = 2", &QueryParams::empty(), &QueryOptions::default()).is_err()
    );
    assert!(
        execute_query(&cat, "SELECT 1 FROM t HAVING x > 0 ORDER BY count(*)", &QueryParams::empty(), &QueryOptions::default()).is_err()
    );
    let hav_empty = run(&cat, "SELECT count(*) AS c FROM empty_t HAVING c > 0");
    assert!(hav_empty.rows.is_empty());

    // 2. Row-value `IN` collation matches C SQLite 3.53.3: 1-item inherits RHS collation, 2-item does not!
    assert_eq_val(&run(&cat, "SELECT ('ABC', 1) IN ((nc, 1)) FROM t WHERE id = 1").rows[0][0], &SqlValue::Int(1), "1-item row-value IN inherits column collation");
    assert_eq_val(&run(&cat, "SELECT ('ABC', 1) IN (('abc' COLLATE NOCASE, 1))").rows[0][0], &SqlValue::Int(1), "1-item row-value IN inherits explicit collation");
    assert_eq_val(&run(&cat, "SELECT ('ABC', 1) IN ((nc, 1), ('z', 2)) FROM t WHERE id = 1").rows[0][0], &SqlValue::Int(0), "2-item row-value IN does not inherit RHS column collation");
    assert_eq_val(&run(&cat, "SELECT ('ABC', 1) IN (('abc' COLLATE NOCASE, 1), ('z', 2))").rows[0][0], &SqlValue::Int(0), "2-item row-value IN does not inherit RHS explicit collation");

    // 3. Unquoted column named `true` resolved inside `IN (true)`!
    assert_eq_val(&run(&cat, "SELECT 7 IN (\"true\") FROM t WHERE id = 1").rows[0][0], &SqlValue::Int(1), "quoted true col");
    assert_eq_val(&run(&cat, "SELECT 7 IN (true) FROM t WHERE id = 1").rows[0][0], &SqlValue::Int(1), "unquoted true col in IN");

    // 4. `RAISE()` outside trigger returns exact SQLite error during evaluation.
    let raise_err = execute_query(&cat, "SELECT RAISE(ABORT, 'boom')", &QueryParams::empty(), &QueryOptions::default()).unwrap_err();
    assert!(raise_err.to_string().contains("RAISE() may only be used within a trigger-program"));

    // 5. `SumAcc` KBN folds exact integer chunks into compensated `r_sum` before canceling reals!
    let kbn1 = run(&cat, "WITH v(x) AS (VALUES (1.0), (1000000000000000000), (-1e18)) SELECT sum(x) FROM v");
    assert_eq_val(&kbn1.rows[0][0], &SqlValue::Real(1.0), "KBN int + canceling real == 1.0");
    let kbn2 = run(&cat, "WITH v(x) AS (VALUES (100.0), (9223372036854775807), (-9223372036854775808.0)) SELECT sum(x) FROM v");
    assert_eq_val(&kbn2.rows[0][0], &SqlValue::Real(99.0), "KBN i64::MAX + -2^63 + 100.0 == 99.0");

    // 6. `eval_binary` and `eval_hash_equijoin` under `TEXT` comparison affinity (`Text` vs `None`):
    //    Text("10") == Int(10) ("10" == "10") -> match, Text("10") != Real(10.0) ("10" != "10.0") -> no match!
    cat.add_table("jt1", vec![ColumnMeta::simple("k", Affinity::Text)], vec![vec![SqlValue::text("10")]]);
    cat.add_table("jt2", vec![ColumnMeta::simple("k", Affinity::None)], vec![vec![SqlValue::Real(10.0)]]);
    cat.add_table("jt3", vec![ColumnMeta::simple("k", Affinity::None)], vec![vec![SqlValue::Int(10)]]);
    assert!(run(&cat, "SELECT jt1.k FROM jt1 JOIN jt2 ON jt1.k = jt2.k").rows.is_empty());
    assert_eq!(run(&cat, "SELECT jt1.k FROM jt1 JOIN jt3 ON jt1.k = jt3.k").rows.len(), 1);
    assert_eq_val(&run(&cat, "SELECT CAST(10 AS TEXT) = 10, CAST(10 AS TEXT) = 10.0").rows[0][0], &SqlValue::Int(1), "TEXT aff '10' == 10");
    assert_eq_val(&run(&cat, "SELECT CAST(10 AS TEXT) = 10, CAST(10 AS TEXT) = 10.0").rows[0][1], &SqlValue::Int(0), "TEXT aff '10' != 10.0");

    // 7. `json_error_position` on invalid UTF-8 bytes returns valid prefix error or char offset!
    assert_eq_val(
        &run(&cat, "SELECT json_error_position(CAST(X'7B2261223AFF7D' AS TEXT))").rows[0][0],
        &SqlValue::Int(6),
        "json_error_position invalid utf8 after valid prefix",
    );
    assert_eq_val(
        &run(&cat, "SELECT json_error_position(CAST(X'7B2261222C3AFF7D' AS TEXT))").rows[0][0],
        &SqlValue::Int(5),
        "json_error_position syntax error before invalid utf8",
    );

    // 8. 0-row subquery validation catches missing column inside subquery on empty outer table.
    assert!(
        execute_query(
            &cat,
            "SELECT (SELECT nope FROM t) FROM empty_t",
            &QueryParams::empty(),
            &QueryOptions::default(),
        )
        .is_err()
    );

    // 9. Cumulative CTE, aggregate, `json_array`, `printf`, and `unhex` metering!
    let tight_mem = QueryOptions {
        meter: Some(Arc::new(BudgetMeter::new(ResourceLimits {
            max_memory_bytes: Some(2048),
            max_steps: Some(1_000_000),
            ..ResourceLimits::default()
        }))),
        ..QueryOptions::default()
    };
    assert!(matches!(
        execute_query(&cat, "SELECT printf('%5000s', 'x')", &QueryParams::empty(), &tight_mem).unwrap_err(),
        SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory
    ));
    let mut big_jarr = String::from("SELECT json_array(");
    for i in 0..80 {
        if i > 0 {
            big_jarr.push(',');
        }
        big_jarr.push_str(&format!("'{}'", "z".repeat(40)));
    }
    big_jarr.push(')');
    assert!(matches!(
        execute_query(&cat, &big_jarr, &QueryParams::empty(), &tight_mem).unwrap_err(),
        SqlError::ResourceLimit(e) if e.kind == LimitKind::Memory
    ));

    // 10. Specialized streaming cumulative window + `(CountStar, Sum)` GROUP BY + streaming hash join!
    let wres = run(&cat, "SELECT id, sum(x) OVER (ORDER BY id) FROM t WHERE id <= 2");
    assert_eq!(wres.rows.len(), 2);
    assert_eq_val(&wres.rows[0][1], &SqlValue::Int(10), "window sum row 0");
    assert_eq_val(&wres.rows[1][1], &SqlValue::Int(30), "window sum row 1");
    let gres = run(&cat, "SELECT id, count(*), sum(x), avg(x) FROM t GROUP BY id ORDER BY id");
    assert_eq!(gres.rows.len(), 2);
    assert_eq_val(&gres.rows[1][2], &SqlValue::Int(20), "group sum");
    assert_eq_val(&gres.rows[1][3], &SqlValue::Real(20.0), "group avg");
}
