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
    execute_query, ColumnMeta, MemoryCatalog, QueryOptions, QueryParams, QueryResult, SqlError,
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
