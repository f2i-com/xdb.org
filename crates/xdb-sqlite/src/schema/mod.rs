//! SQLite schema loader (`sqlite3InitOne` / `sqlite3InitCallback`), catalog types,
//! and `ALTER TABLE ADD COLUMN` default-value evaluation.

pub mod catalog;
pub mod default_value;
pub mod text;

pub use catalog::{
    load_schema, ColumnHiddenKind, ColumnInfo, IndexColumnInfo, IndexInfo, LoadSchemaOptions,
    SchemaCatalog, SchemaEntry, SchemaObject, TableInfo, TriggerInfo, ViewInfo, VirtualTableInfo,
    MAX_SCHEMA_FILE_FORMAT,
};
pub use default_value::{column_default_value, real_read_back, value_from_expr};
pub use text::{ascii_fold, column_text, get_u32, same_text};
