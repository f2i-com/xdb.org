//! Pure safe Rust SQLite 3.54.1 + XDB dialect SQL `SELECT` query engine.
//!
//! Zero unsafe code, zero third-party runtime dependencies, and deterministic
//! sandbox resource metering ([`xdb_core::Meter`]) throughout parsing, expression
//! evaluation, joins, grouping, windowing, CTE recursion, and built-in functions.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod errors;
pub mod eval;
pub mod exec;
pub mod explain;
pub mod functions;
pub mod utf8;

pub use catalog::{
    Bound, ColumnMeta, IndexMeta, MemoryCatalog, QueryOptions, QueryParams, QueryResult,
    ScannedRow, SqlCatalog, TableMeta, TableScanHint, ViewMeta,
};
pub use errors::{SqlError, SqlResult};
pub use eval::{
    eval_expr, eval_generated_expr, expr_contains_raise, from_contains_raise, select_contains_raise,
    EvalEnv, EvalScope, NoSubqueries, ScopeColumn, SubqueryRunner,
};
pub use exec::{
    execute_query, execute_select_stmt, execute_select_stmt_detailed,
    execute_select_stmt_with_outer_with, execute_select_stmt_with_scopes, validate_expr_schema,
    validate_select_stmt_schema, CatalogSubqueryRunner,
};
pub use explain::explain_query_plan;
pub use functions::{
    CustomScalarFn, FunctionContext, FunctionLimits, FunctionRegistry, TableValuedResult,
};
