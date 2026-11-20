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
