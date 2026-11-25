//! The schema catalog: `sqlite_schema` read and interpreted the way SQLite's
//! schema loader does it (`prepare.c` `sqlite3InitOne` / `sqlite3InitCallback`,
//! and the `db->init.busy` paths of `build.c`, `trigger.c`, and `vtab.c`).

use std::collections::HashMap;
use std::sync::Arc;

use xdb_core::{Affinity, SqlValue, TextEncoding};
use xdb_parser::{
    build_index, build_table, parse_statement, parse_statement_at_metered, parser_tokens, tk,
    BuildOptions, ColumnHidden, CreateIndexStmt, CreateTableStmt, CreateTriggerStmt,
    CreateViewStmt, CreateVirtualTableStmt, Dialect, Expr, IndexDefinition, IndexOrigin, Name,
    ParseOptions, Span, SqlParseError, Statement, TableDefinition,
    TriggerEventKind, TriggerTime,
};

use crate::btree::Btree;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::RawDbHeader;
use crate::schema::default_value::column_default_value;
use crate::schema::text::{ascii_fold, column_text, get_u32, same_text};

/// `SQLITE_MAX_FILE_FORMAT` (`sqliteInt.h`).
pub const MAX_SCHEMA_FILE_FORMAT: u32 = 4;

const SCHEMA_TABLE_SQL: &str =
    "CREATE TABLE x(type text,name text,tbl_name text,rootpage int,sql text)";

/// One `sqlite_schema` row as SQLite's schema loader sees it (every column as text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    pub rowid: i64,
    pub obj_type: Option<String>,
    pub name: Option<String>,
    pub table_name: Option<String>,
    pub rootpage_text: Option<String>,
    pub root_page: u32,
    pub sql: Option<String>,
}

/// `pragma table_xinfo` `hidden`: 0 ordinary, 2 VIRTUAL generated, 3 STORED generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColumnHiddenKind {
    Normal = 0,
    Virtual = 2,
    Stored = 3,
}

/// One column of a table, in declaration order.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub cid: usize,
    pub name: String,
    pub decl_type: String,
    pub affinity: Affinity,
    pub not_null: bool,
    pub default_sql: Option<String>,
    /// 1-based position in PRIMARY KEY, or 0.
    pub pk: usize,
    pub hidden: ColumnHiddenKind,
    pub generated: Option<GeneratedKind>,
    pub collation: String,
    pub rowid_alias: bool,
    /// Record field index (`None` for VIRTUAL generated columns).
    pub storage_index: Option<usize>,
    /// Default value returned for short records (predating `ALTER TABLE ADD COLUMN`).
    pub default_value: SqlValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratedKind {
    Virtual,
    Stored,
}

/// One column of an index record, in record order (`pragma index_xinfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnInfo {
    /// Table column (`>= 0`), `-1` for rowid, `-2` for an expression.
    pub cid: i32,
    pub name: Option<String>,
    pub expr: Option<String>,
    pub desc: bool,
    pub collation: String,
    pub key: bool,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub table_name: String,
    pub root_page: u32,
    pub sql: Option<String>,
    pub origin: IndexOrigin,
    pub unique: bool,
    pub partial: bool,
    pub where_sql: Option<String>,
    pub auto: bool,
    pub without_rowid_primary_key: bool,
    pub key_count: usize,
    pub columns: Vec<IndexColumnInfo>,
    pub definition: IndexDefinition,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub root_page: u32,
    pub sql: Option<String>,
    pub without_rowid: bool,
    pub strict: bool,
    pub system: bool,
    pub ipk_column: Option<usize>,
    pub columns: Vec<ColumnInfo>,
    pub has_virtual_columns: bool,
    pub primary_key: Vec<i32>,
    pub indexes: Vec<Arc<IndexInfo>>,
    pub definition: TableDefinition,
    pub(crate) sql_meta: std::sync::OnceLock<Arc<xdb_sql::TableMeta>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewInfo {
    pub name: String,
    pub sql: String,
    pub column_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerInfo {
    pub name: String,
    pub table_name: String,
    pub sql: String,
    pub time: TriggerTime,
    pub event: TriggerEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTableInfo {
    pub name: String,
    pub module: String,
    pub args: Vec<String>,
    pub sql: String,
    pub root_page: u32,
}

#[derive(Debug, Clone)]
pub enum SchemaObject {
    Table(Arc<TableInfo>),
    Index(Arc<IndexInfo>),
    View(Arc<ViewInfo>),
    Trigger(Arc<TriggerInfo>),
    VirtualTable(Arc<VirtualTableInfo>),
}

/// The loaded schema catalog of one SQLite database.
#[derive(Debug, Clone)]
pub struct SchemaCatalog {
    pub file_format: u32,
    pub encoding: TextEncoding,
    pub entries: Vec<SchemaEntry>,
    pub schema_table: Arc<TableInfo>,
    pub tables: Vec<Arc<TableInfo>>,
    pub indexes: Vec<Arc<IndexInfo>>,
    pub views: Vec<Arc<ViewInfo>>,
    pub triggers: Vec<Arc<TriggerInfo>>,
    pub virtual_tables: Vec<Arc<VirtualTableInfo>>,
    objects: HashMap<String, SchemaObject>,
    index_map: HashMap<String, Arc<IndexInfo>>,
    trigger_map: HashMap<String, Arc<TriggerInfo>>,
}

impl SchemaCatalog {
    /// Look up a table, view, or virtual table by name (ASCII case-insensitive;
    /// `"sqlite_schema"` resolves to `sqlite_master`).
    pub fn object(&self, name: &str) -> Option<&SchemaObject> {
        let key = ascii_fold(name);
        let lookup = if key == "sqlite_schema" {
            "sqlite_master"
        } else {
            key.as_str()
        };
        self.objects.get(lookup)
    }

    pub fn table(&self, name: &str) -> Option<&Arc<TableInfo>> {
        match self.object(name) {
            Some(SchemaObject::Table(t)) => Some(t),
            _ => None,
        }
    }

    pub fn index(&self, name: &str) -> Option<&Arc<IndexInfo>> {
        self.index_map.get(&ascii_fold(name))
    }

    pub fn view(&self, name: &str) -> Option<&Arc<ViewInfo>> {
        match self.object(name) {
            Some(SchemaObject::View(v)) => Some(v),
            _ => None,
        }
    }

    pub fn trigger(&self, name: &str) -> Option<&Arc<TriggerInfo>> {
        self.trigger_map.get(&ascii_fold(name))
    }

    pub fn virtual_table(&self, name: &str) -> Option<&Arc<VirtualTableInfo>> {
        match self.object(name) {
            Some(SchemaObject::VirtualTable(v)) => Some(v),
            _ => None,
        }
    }
}

/// Options for [`load_schema`].
#[derive(Debug, Clone, Default)]
pub struct LoadSchemaOptions<'a> {
    /// Effective database header (`Some(None)` forces empty-db header, `None` uses `bt.info().header`).
    pub header: Option<Option<&'a RawDbHeader>>,
}

/// Read `sqlite_schema` and build the [`SchemaCatalog`].
pub fn load_schema(bt: &Btree, options: &LoadSchemaOptions<'_>) -> SqliteResult<SchemaCatalog> {
    let header = match options.header {
        Some(h) => h,
        None => bt.info().header.as_ref(),
    };
    let enc = bt.info().text_encoding;
    let mut file_format = header.map_or(0, |h| h.schema_format);
    if file_format == 0 {
        file_format = 1;
    }
    if file_format > MAX_SCHEMA_FILE_FORMAT {
        return Err(SqliteError::schema_format("unsupported file format"));
    }
    SchemaLoader::new(bt, enc, file_format).run()
}

// ───────────────────────────── loader implementation ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Table,
    View,
    Virtual,
}

struct IndexRec {
    name: String,
    sql: Option<String>,
    def: IndexDefinition,
    root_page: u32,
    order: usize,
    table_id: usize,
}

struct TableRec {
    kind: TableKind,
    z_name: String,
    root_page: u32,
    sql: Option<String>,
    def: Option<TableDefinition>,
    /// Index IDs in `Table.pIndex` order.
    indexes: Vec<usize>,
    view: Option<ViewInfo>,
    vtab: Option<VirtualTableInfo>,
}

fn is_cr(sql: &str) -> bool {
    let b = sql.as_bytes();
    b.len() >= 2 && (b[0] | 0x20) == b'c' && (b[1] | 0x20) == b'r'
}

fn name_text(sql: &str, span: Span) -> &str {
    &sql[span.start..span.end]
}

fn expr_text<'a>(sql: &'a str, mut e: &'a Expr) -> &'a str {
    while let Expr::Collate { operand, .. } = e {
        e = operand;
    }
    let sp = e.span();
    &sql[sp.start..sp.end]
}

fn has_parameter(sql: &str, span: Span) -> bool {
    parser_tokens(&sql[span.start..span.end], Dialect::Sqlite)
        .iter()
        .any(|t| t.kind == tk::VARIABLE)
}

fn starts_with_sqlite(name: &str) -> bool {
    name.len() >= 7 && name.as_bytes()[..7].eq_ignore_ascii_case(b"sqlite_")
}

struct SchemaLoader<'a> {
    bt: &'a Btree,
    enc: TextEncoding,
    file_format: u32,
    mx_page: u32,
    table_arena: Vec<TableRec>,
    index_arena: Vec<IndexRec>,
    tables_by_name: HashMap<String, usize>,
    indexes_by_name: HashMap<String, usize>,
    triggers_by_name: HashMap<String, usize>,
    created: Vec<(TableKind, usize)>,
    trigger_list: Vec<TriggerInfo>,
    entries: Vec<SchemaEntry>,
    schema_table_id: usize,
    argv: [Option<String>; 5],
    new_tnum: u32,
    row_order: usize,
}

impl<'a> SchemaLoader<'a> {
    fn new(bt: &'a Btree, enc: TextEncoding, file_format: u32) -> Self {
        let mx_page = bt.page_count();
        let parse_opts = ParseOptions {
            schema: true,
            ..ParseOptions::default()
        };
        let stmt = parse_statement(SCHEMA_TABLE_SQL, &parse_opts).expect("builtin schema SQL");
        let Statement::CreateTable(ct) = stmt else {
            unreachable!();
        };
        let mut def = build_table(
            &ct,
            SCHEMA_TABLE_SQL,
            &BuildOptions {
                schema: true,
                ..BuildOptions::default()
            },
        )
        .expect("builtin schema table");
        def.name = "sqlite_master".to_string();

        let schema_rec = TableRec {
            kind: TableKind::Table,
            z_name: "sqlite_master".to_string(),
            root_page: 1,
            sql: None,
            def: Some(def),
            indexes: Vec::new(),
            view: None,
            vtab: None,
        };
        let mut tables_by_name = HashMap::new();
        tables_by_name.insert("sqlite_master".to_string(), 0);
        Self {
            bt,
            enc,
            file_format,
            mx_page,
            table_arena: vec![schema_rec],
            index_arena: Vec::new(),
            tables_by_name,
            indexes_by_name: HashMap::new(),
            triggers_by_name: HashMap::new(),
            created: Vec::new(),
            trigger_list: Vec::new(),
            entries: Vec::new(),
            schema_table_id: 0,
            argv: [None, None, None, None, None],
            new_tnum: 0,
            row_order: 0,
        }
    }

    fn corrupt(&self, reason: impl Into<String>) -> SqliteError {
        SqliteError::schema_corrupt(self.argv[1].as_deref(), reason)
    }

    fn map_parse_err(&self, err: SqlParseError) -> SqliteError {
        if let Some(rl) = err.resource_limit {
            SqliteError::ResourceLimit(rl)
        } else {
            self.corrupt(err.message)
        }
    }

    fn run(mut self) -> SqliteResult<SchemaCatalog> {
        let mut cur = self.bt.table_cursor(1)?;
        let mut ok = cur.first()?;
        while ok {
            let rowid = cur.rowid()?;
            let enc = self.enc;
            let argv = {
                let rec = cur.record()?;
                [
                    column_text(&rec.value(0)?, enc),
                    column_text(&rec.value(1)?, enc),
                    column_text(&rec.value(2)?, enc),
                    column_text(&rec.value(3)?, enc),
                    column_text(&rec.value(4)?, enc),
                ]
            };
            self.argv = argv;
            self.row_order += 1;
            let root = self.argv[3].as_deref().and_then(get_u32);
            self.entries.push(SchemaEntry {
                rowid,
                obj_type: self.argv[0].clone(),
                name: self.argv[1].clone(),
                table_name: self.argv[2].clone(),
                rootpage_text: self.argv[3].clone(),
                root_page: root.unwrap_or(0),
                sql: self.argv[4].clone(),
            });
            self.process_row()?;
            ok = cur.next()?;
        }
        Ok(self.finish())
    }

    fn process_row(&mut self) -> SqliteResult<()> {
        let root_text = match self.argv[3].as_deref() {
            Some(s) => s,
            None => return Err(self.corrupt("")),
        };
        if let Some(sql) = self.argv[4].clone() {
            if is_cr(&sql) {
                let t = get_u32(root_text);
                self.new_tnum = t.unwrap_or(0);
                if t.is_none() || (self.new_tnum > self.mx_page && self.mx_page > 0) {
                    return Err(self.corrupt("invalid rootpage"));
                }
                return self.prepare(&sql);
            }
            if !sql.is_empty() {
                return Err(self.corrupt(""));
            }
        }
        let name = match self.argv[1].as_deref() {
            Some(n) => n,
            None => return Err(self.corrupt("")),
        };
        let Some(&idx_id) = self.indexes_by_name.get(&ascii_fold(name)) else {
            return Err(self.corrupt("orphan index"));
        };
        let t = get_u32(root_text);
        let root_page = t.unwrap_or(0);
        self.index_arena[idx_id].root_page = root_page;
        self.index_arena[idx_id].order = self.row_order;
        if t.is_none() || root_page < 2 || root_page > self.mx_page || self.duplicate_root(idx_id) {
            return Err(self.corrupt("invalid rootpage"));
        }
        Ok(())
    }

    fn duplicate_root(&self, idx_id: usize) -> bool {
        let idx = &self.index_arena[idx_id];
        let tbl = &self.table_arena[idx.table_id];
        tbl.indexes
            .iter()
            .any(|&other| other != idx_id && self.index_arena[other].root_page == idx.root_page)
    }

    fn prepare(&mut self, sql: &str) -> SqliteResult<()> {
        let parse_opts = ParseOptions {
            schema: true,
            ..ParseOptions::default()
        };
        let parsed = match parse_statement_at_metered(
            sql,
            0,
            &parse_opts,
            self.bt.pager().meter().as_ref(),
        ) {
            Ok(p) => p,
            Err(err) => {
                if let Some(rl) = err.resource_limit {
                    return Err(SqliteError::ResourceLimit(rl));
                }
                self.header_checks_first(sql, &err)?;
                return Err(self.corrupt(err.message));
            }
        };
        let Some(stmt) = parsed.statement else {
            return Ok(());
        };
        match stmt {
            Statement::CreateTable(ct) => self.create_table(&ct, sql)?,
            Statement::CreateIndex(ci) => self.create_index(&ci, sql)?,
            Statement::CreateView(cv) => self.create_view(&cv, sql)?,
            Statement::CreateTrigger(ctr) => self.create_trigger(&ctr, sql, true)?,
            Statement::CreateVirtualTable(cvt) => self.create_virtual_table(&cvt, sql)?,
            _ => {}
        }
        // prepare.c:162: anything after the statement's terminating ';' is corrupt.
        if parsed.end > 0 && sql.as_bytes()[parsed.end - 1] == b';' && parsed.end < sql.len() {
            return Err(self.corrupt(""));
        }
        Ok(())
    }

    fn check_object_name(&self, z_name: &str, z_type: &str, z_tbl_name: &str) -> SqliteResult<()> {
        if !same_text(Some(z_type), self.argv[0].as_deref())
            || !same_text(Some(z_name), self.argv[1].as_deref())
            || !same_text(Some(z_tbl_name), self.argv[2].as_deref())
        {
            return Err(self.corrupt(""));
        }
        Ok(())
    }

    fn find_table(&self, z_name: &str) -> Option<usize> {
        let key = ascii_fold(z_name);
        let lookup = if key == "sqlite_schema" {
            "sqlite_master"
        } else {
            key.as_str()
        };
        self.tables_by_name.get(lookup).copied()
    }

    fn start_table(
        &self,
        sql: &str,
        schema: Option<&Name>,
        name: &Name,
        if_not_exists: bool,
        is_view: bool,
    ) -> SqliteResult<Option<String>> {
        let (z_name, token) = if self.new_tnum == 1 {
            let tok_span = schema.map_or(name.span, |s| s.span);
            ("sqlite_master".to_string(), name_text(sql, tok_span))
        } else {
            if schema.is_some() {
                return Err(self.corrupt("corrupt database"));
            }
            (name.name.clone(), name_text(sql, name.span))
        };
        self.check_object_name(&z_name, if is_view { "view" } else { "table" }, &z_name)?;
        if let Some(existing_id) = self.find_table(&z_name) {
            if !if_not_exists {
                let kind_word = if self.table_arena[existing_id].kind == TableKind::View {
                    "view"
                } else {
                    "table"
                };
                return Err(self.corrupt(format!("{kind_word} {token} already exists")));
            }
            return Ok(None);
        }
        if self.indexes_by_name.contains_key(&ascii_fold(&z_name)) {
            return Err(self.corrupt(format!("there is already an index named {z_name}")));
        }
        Ok(Some(z_name))
    }

    fn create_table(&mut self, stmt: &CreateTableStmt, sql: &str) -> SqliteResult<()> {
        let Some(z_name) = self.start_table(
            sql,
            stmt.schema.as_ref(),
            &stmt.name,
            stmt.if_not_exists,
            false,
        )?
        else {
            return Ok(());
        };
        if stmt.as_select.is_some() {
            return Err(self.corrupt(""));
        }
        let build_opts = BuildOptions {
            schema: true,
            file_format: self.file_format,
            ..BuildOptions::default()
        };
        let def = build_table(stmt, sql, &build_opts).map_err(|e| self.map_parse_err(e))?;
        let table_id = self.table_arena.len();
        let mut idx_ids = Vec::with_capacity(def.indexes.len());
        for d in &def.indexes {
            let key = ascii_fold(&d.name);
            if self.indexes_by_name.contains_key(&key) {
                return Err(SqliteError::corrupt(format!(
                    "schema: automatic index {} of table {z_name} collides with an existing index",
                    d.name
                )));
            }
            let idx_id = self.index_arena.len();
            self.index_arena.push(IndexRec {
                name: d.name.clone(),
                sql: None,
                def: d.clone(),
                root_page: if d.without_rowid_primary_key {
                    self.new_tnum
                } else {
                    0
                },
                order: self.row_order,
                table_id,
            });
            idx_ids.push(idx_id);
            self.indexes_by_name.insert(key, idx_id);
        }
        self.table_arena.push(TableRec {
            kind: TableKind::Table,
            z_name: z_name.clone(),
            root_page: self.new_tnum,
            sql: Some(sql.to_string()),
            def: Some(def),
            indexes: idx_ids,
            view: None,
            vtab: None,
        });
        self.tables_by_name.insert(ascii_fold(&z_name), table_id);
        self.created.push((TableKind::Table, table_id));
        Ok(())
    }

    fn create_view(&mut self, stmt: &CreateViewStmt, sql: &str) -> SqliteResult<()> {
        if has_parameter(sql, stmt.select.span) {
            return Err(self.corrupt("parameters are not allowed in views"));
        }
        let Some(z_name) = self.start_table(
            sql,
            stmt.schema.as_ref(),
            &stmt.name,
            stmt.if_not_exists,
            true,
        )?
        else {
            return Ok(());
        };
        if self.new_tnum != 0 {
            return Err(self.corrupt(""));
        }
        let view = ViewInfo {
            name: z_name.clone(),
            sql: sql.to_string(),
            column_names: stmt
                .columns
                .as_ref()
                .map(|cols| cols.iter().map(|c| c.name.name.clone()).collect()),
        };
        let table_id = self.table_arena.len();
        self.table_arena.push(TableRec {
            kind: TableKind::View,
            z_name: z_name.clone(),
            root_page: 0,
            sql: Some(sql.to_string()),
            def: None,
            indexes: Vec::new(),
            view: Some(view),
            vtab: None,
        });
        self.tables_by_name.insert(ascii_fold(&z_name), table_id);
        self.created.push((TableKind::View, table_id));
        Ok(())
    }

    fn create_virtual_table(
        &mut self,
        stmt: &CreateVirtualTableStmt,
        sql: &str,
    ) -> SqliteResult<()> {
        let Some(z_name) = self.start_table(
            sql,
            stmt.schema.as_ref(),
            &stmt.name,
            stmt.if_not_exists,
            false,
        )?
        else {
            return Ok(());
        };
        let vtab = VirtualTableInfo {
            name: z_name.clone(),
            module: stmt.module.name.clone(),
            args: stmt
                .args
                .as_ref()
                .map(|args| args.iter().map(|a| a.text.clone()).collect())
                .unwrap_or_default(),
            sql: sql.to_string(),
            root_page: self.new_tnum,
        };
        let table_id = self.table_arena.len();
        self.table_arena.push(TableRec {
            kind: TableKind::Virtual,
            z_name: z_name.clone(),
            root_page: self.new_tnum,
            sql: Some(sql.to_string()),
            def: None,
            indexes: Vec::new(),
            view: None,
            vtab: Some(vtab),
        });
        self.tables_by_name.insert(ascii_fold(&z_name), table_id);
        self.created.push((TableKind::Virtual, table_id));
        Ok(())
    }

    fn create_index(&mut self, stmt: &CreateIndexStmt, sql: &str) -> SqliteResult<()> {
        if stmt.schema.is_some() {
            return Err(self.corrupt("corrupt database"));
        }
        let Some(table_id) = self.find_table(&stmt.table.name) else {
            return Err(self.corrupt(format!("no such table: main.{}", stmt.table.name)));
        };
        match self.table_arena[table_id].kind {
            TableKind::View => return Err(self.corrupt("views may not be indexed")),
            TableKind::Virtual => return Err(self.corrupt("virtual tables may not be indexed")),
            TableKind::Table => {}
        }
        let z_name = stmt.name.name.clone();
        let t_z_name = self.table_arena[table_id].z_name.clone();
        self.check_object_name(&z_name, "index", &t_z_name)?;
        if self.indexes_by_name.contains_key(&ascii_fold(&z_name)) {
            let msg = if stmt.if_not_exists {
                String::new()
            } else {
                format!("index {z_name} already exists")
            };
            return Err(self.corrupt(msg));
        }
        let build_opts = BuildOptions {
            schema: true,
            file_format: self.file_format,
            ..BuildOptions::default()
        };
        let table_def = self.table_arena[table_id]
            .def
            .as_ref()
            .expect("ordinary table has def");
        let def = build_index(stmt, table_def, &build_opts).map_err(|e| self.map_parse_err(e))?;
        let idx_id = self.index_arena.len();
        self.index_arena.push(IndexRec {
            name: z_name.clone(),
            sql: Some(sql.to_string()),
            def,
            root_page: self.new_tnum,
            order: self.row_order,
            table_id,
        });
        if self.duplicate_root(idx_id) {
            self.index_arena.pop();
            return Err(self.corrupt("invalid rootpage"));
        }
        self.table_arena[table_id].indexes.insert(0, idx_id);
        self.indexes_by_name.insert(ascii_fold(&z_name), idx_id);
        Ok(())
    }

    fn create_trigger(
        &mut self,
        stmt: &CreateTriggerStmt,
        sql: &str,
        register: bool,
    ) -> SqliteResult<()> {
        if stmt.schema.is_some() {
            return Err(self.corrupt("corrupt database"));
        }
        let Some(table_id) = self.find_table(&stmt.table.name) else {
            return Err(self.corrupt(format!("no such table: main.{}", stmt.table.name)));
        };
        if self.table_arena[table_id].kind == TableKind::Virtual {
            return Err(self.corrupt("cannot create triggers on virtual tables"));
        }
        let z_name = stmt.name.name.clone();
        let t_z_name = self.table_arena[table_id].z_name.clone();
        self.check_object_name(&z_name, "trigger", &t_z_name)?;
        if self.triggers_by_name.contains_key(&ascii_fold(&z_name)) {
            let msg = if stmt.if_not_exists {
                String::new()
            } else {
                format!("trigger {} already exists", name_text(sql, stmt.name.span))
            };
            return Err(self.corrupt(msg));
        }
        if starts_with_sqlite(&t_z_name) {
            return Err(self.corrupt("cannot create trigger on system table"));
        }
        let time = stmt.time.unwrap_or(TriggerTime::Before);
        let is_view = self.table_arena[table_id].kind == TableKind::View;
        if is_view && time != TriggerTime::InsteadOf {
            let word = match time {
                TriggerTime::Before => "BEFORE",
                TriggerTime::After => "AFTER",
                TriggerTime::InsteadOf => "INSTEAD OF",
            };
            return Err(self.corrupt(format!(
                "cannot create {word} trigger on view: {}",
                stmt.table.name
            )));
        }
        if !is_view && time == TriggerTime::InsteadOf {
            return Err(self.corrupt(format!(
                "cannot create INSTEAD OF trigger on table: {}",
                stmt.table.name
            )));
        }
        if !register {
            return Ok(());
        }
        let info = TriggerInfo {
            name: z_name.clone(),
            table_name: t_z_name,
            sql: sql.to_string(),
            time,
            event: stmt.event,
        };
        let id = self.trigger_list.len();
        self.trigger_list.push(info);
        self.triggers_by_name.insert(ascii_fold(&z_name), id);
        Ok(())
    }

    /// Run `sqlite3StartTable` / `sqlite3BeginTrigger` header checks if a parse error occurred
    /// past the statement header (matching SQLite 3.54.1's error precedence).
    fn header_checks_first(&mut self, sql: &str, err: &SqlParseError) -> SqliteResult<()> {
        let toks = parser_tokens(sql, Dialect::Sqlite);
        let at = |i: usize| toks.get(i).map(|t| t.kind);
        if at(0) != Some(tk::CREATE) {
            return Ok(());
        }
        let mut i = 1usize;
        enum Kind {
            Table,
            Virtual,
            Trigger,
        }
        let kind = if at(i) == Some(tk::VIRTUAL) && at(i + 1) == Some(tk::TABLE) {
            i += 2;
            Kind::Virtual
        } else {
            if at(i) == Some(tk::TEMP) {
                i += 1;
            }
            let k = if at(i) == Some(tk::TABLE) {
                Kind::Table
            } else if at(i) == Some(tk::TRIGGER) {
                Kind::Trigger
            } else {
                return Ok(());
            };
            i += 1;
            k
        };
        let (cut, body) = match kind {
            Kind::Trigger => {
                let mut b = i;
                while b < toks.len() && at(b) != Some(tk::BEGIN) {
                    b += 1;
                }
                if b >= toks.len() {
                    return Ok(());
                }
                (toks[b].start, " BEGIN SELECT 1; END")
            }
            Kind::Virtual | Kind::Table => {
                if at(i) == Some(tk::IF)
                    && at(i + 1) == Some(tk::NOT)
                    && at(i + 2) == Some(tk::EXISTS)
                {
                    i += 3;
                }
                i += 1; // name
                if at(i) == Some(tk::DOT) {
                    i += 2;
                }
                let body = match kind {
                    Kind::Virtual => {
                        if at(i) != Some(tk::USING) {
                            return Ok(());
                        }
                        i += 2;
                        if at(i) != Some(tk::LP) {
                            return Ok(());
                        }
                        ""
                    }
                    Kind::Table => {
                        if at(i) != Some(tk::LP) && at(i) != Some(tk::AS) {
                            return Ok(());
                        }
                        "(x)"
                    }
                    Kind::Trigger => unreachable!(),
                };
                if i >= toks.len() {
                    return Ok(());
                }
                (toks[i].start, body)
            }
        };
        if err.start.is_some_and(|s| s <= cut) {
            return Ok(());
        }
        let head = format!("{}{body}", &sql[..cut]);
        let parse_opts = ParseOptions {
            schema: true,
            ..ParseOptions::default()
        };
        let Ok(parsed) = parse_statement_at_metered(
            &head,
            0,
            &parse_opts,
            self.bt.pager().meter().as_ref(),
        ) else {
            return Ok(());
        };
        match parsed.statement {
            Some(Statement::CreateTable(ct)) => {
                self.start_table(
                    &head,
                    ct.schema.as_ref(),
                    &ct.name,
                    ct.if_not_exists,
                    false,
                )?;
            }
            Some(Statement::CreateVirtualTable(cvt)) => {
                self.start_table(
                    &head,
                    cvt.schema.as_ref(),
                    &cvt.name,
                    cvt.if_not_exists,
                    false,
                )?;
            }
            Some(Statement::CreateTrigger(ctr)) => {
                self.create_trigger(&ctr, &head, false)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> SchemaCatalog {
        let enc = self.enc;
        let built_indexes: Vec<Arc<IndexInfo>> = self
            .index_arena
            .iter()
            .map(|r| {
                let d = &r.def;
                let sql = r.sql.as_deref();
                let where_sql = match (&d.where_clause, sql) {
                    (Some(w), Some(s)) => {
                        let sp = w.span();
                        Some(s[sp.start..sp.end].to_string())
                    }
                    _ => None,
                };
                let columns = d
                    .columns
                    .iter()
                    .map(|c| IndexColumnInfo {
                        cid: c.cid,
                        name: c.name.clone(),
                        expr: match (&c.expr, sql) {
                            (Some(e), Some(s)) => Some(expr_text(s, e).to_string()),
                            _ => None,
                        },
                        desc: c.desc,
                        collation: c.collation.clone(),
                        key: c.key,
                    })
                    .collect();
                Arc::new(IndexInfo {
                    name: r.name.clone(),
                    table_name: self.table_arena[r.table_id].z_name.clone(),
                    root_page: r.root_page,
                    sql: r.sql.clone(),
                    origin: d.origin,
                    unique: d.unique,
                    partial: d.partial,
                    where_sql,
                    auto: d.auto,
                    without_rowid_primary_key: d.without_rowid_primary_key,
                    key_count: d.key_count,
                    columns,
                    definition: d.clone(),
                })
            })
            .collect();

        let to_table_info = |table_id: usize, public_name: &str| -> Arc<TableInfo> {
            let r = &self.table_arena[table_id];
            let def = r.def.as_ref().expect("table has def");
            let columns: Vec<ColumnInfo> = def
                .columns
                .iter()
                .map(|c| {
                    let (hidden, generated) = match c.hidden {
                        ColumnHidden::Virtual => {
                            (ColumnHiddenKind::Virtual, Some(GeneratedKind::Virtual))
                        }
                        ColumnHidden::Stored => {
                            (ColumnHiddenKind::Stored, Some(GeneratedKind::Stored))
                        }
                        _ => (ColumnHiddenKind::Normal, None),
                    };
                    let dflt = c.default_expr.as_ref().or_else(|| {
                        if hidden == ColumnHiddenKind::Stored {
                            c.generated.as_ref()
                        } else {
                            None
                        }
                    });
                    let storage_index = if hidden == ColumnHiddenKind::Virtual {
                        None
                    } else {
                        def.record_columns.iter().position(|&cid| cid == c.cid)
                    };
                    ColumnInfo {
                        cid: c.cid,
                        name: c.name.clone(),
                        decl_type: c.decl_type.clone(),
                        affinity: c.affinity,
                        not_null: c.not_null,
                        default_sql: c.default_text.clone(),
                        pk: c.pk,
                        hidden,
                        generated,
                        collation: c.collation.clone().unwrap_or_else(|| "BINARY".to_string()),
                        rowid_alias: !def.without_rowid && def.ipk_column == Some(c.cid),
                        storage_index,
                        default_value: column_default_value(dflt, c.affinity, enc),
                    }
                })
                .collect();
            let indexes = r
                .indexes
                .iter()
                .map(|&idx_id| built_indexes[idx_id].clone())
                .collect();
            Arc::new(TableInfo {
                name: public_name.to_string(),
                root_page: r.root_page,
                sql: r.sql.clone(),
                without_rowid: def.without_rowid,
                strict: def.strict,
                system: starts_with_sqlite(public_name),
                ipk_column: if def.without_rowid {
                    None
                } else {
                    def.ipk_column
                },
                columns,
                has_virtual_columns: def.has_virtual_columns,
                primary_key: def.primary_key.iter().map(|p| p.cid).collect(),
                indexes,
                definition: def.clone(),
                sql_meta: std::sync::OnceLock::new(),
            })
        };

        let schema_table = to_table_info(self.schema_table_id, "sqlite_schema");
        let mut tables = Vec::new();
        let mut views = Vec::new();
        let mut virtual_tables = Vec::new();
        for &(kind, id) in &self.created {
            match kind {
                TableKind::Table => {
                    let name = self.table_arena[id].z_name.clone();
                    tables.push(to_table_info(id, &name));
                }
                TableKind::View => {
                    if let Some(v) = &self.table_arena[id].view {
                        views.push(Arc::new(v.clone()));
                    }
                }
                TableKind::Virtual => {
                    if let Some(vt) = &self.table_arena[id].vtab {
                        virtual_tables.push(Arc::new(vt.clone()));
                    }
                }
            }
        }

        let mut all_index_pairs: Vec<(usize, Arc<IndexInfo>)> = self
            .indexes_by_name
            .values()
            .map(|&id| (self.index_arena[id].order, built_indexes[id].clone()))
            .collect();
        all_index_pairs.sort_by_key(|(order, _)| *order);
        let indexes: Vec<Arc<IndexInfo>> =
            all_index_pairs.into_iter().map(|(_, info)| info).collect();

        let triggers: Vec<Arc<TriggerInfo>> =
            self.trigger_list.into_iter().map(Arc::new).collect();

        let mut objects = HashMap::new();
        objects.insert(
            "sqlite_master".to_string(),
            SchemaObject::Table(schema_table.clone()),
        );
        for t in &tables {
            objects.insert(ascii_fold(&t.name), SchemaObject::Table(t.clone()));
        }
        for v in &views {
            objects.insert(ascii_fold(&v.name), SchemaObject::View(v.clone()));
        }
        for vt in &virtual_tables {
            objects.insert(ascii_fold(&vt.name), SchemaObject::VirtualTable(vt.clone()));
        }
        let mut index_map = HashMap::new();
        for idx in &indexes {
            index_map.insert(ascii_fold(&idx.name), idx.clone());
        }
        let mut trigger_map = HashMap::new();
        for tr in &triggers {
            trigger_map.insert(ascii_fold(&tr.name), tr.clone());
        }

        SchemaCatalog {
            file_format: self.file_format,
            encoding: enc,
            entries: self.entries,
            schema_table,
            tables,
            indexes,
            views,
            triggers,
            virtual_tables,
            objects,
            index_map,
            trigger_map,
        }
    }
}
