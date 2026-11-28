//! High-level read-only SQLite database reader (`SqliteDatabase`, `TableReader`,
//! `RowCursor`, `IndexReader`, `IndexEntryCursor`).
//!
//! Opens a SQLite database (main file + `-wal` / hot `-journal` overlay) over any
//! [`Vfs`], raw bytes, or a single database image, loads the schema via SQLite's
//! `sqlite3InitOne` rules, and reads logical table rows (with IPK rowid substitution,
//! `REAL` affinity read-back, `ALTER TABLE ADD COLUMN` defaults, and `WITHOUT ROWID`
//! support) and index entries lazily under optional sandbox [`Meter`] limits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use xdb_core::value::collation::CollationRegistry;
use xdb_core::value::apply_affinity;
use xdb_core::{Affinity, Meter, SqlValue, TextEncoding, Vfs};

use crate::access::{key_info_for_index, KeyInfo};
use crate::btree::{
    Btree, BtreeOptions, IndexCursor, RecordKey, RecordView, SeekOp, SeekResult, TableCursor,
};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{
    decode_serial_value, get_payload_size_varint, get_varint, get_varint32, serial_type_length,
    RawDbHeader,
};
use crate::format::page::read_u16_be;
use crate::schema::{
    ascii_fold, load_schema, real_read_back, ColumnHiddenKind, ColumnInfo, IndexInfo,
    LoadSchemaOptions, SchemaCatalog, SchemaObject, TableInfo,
};
use crate::snapshot::{
    open_snapshot, open_snapshot_from_bytes, open_snapshot_from_image, HotJournalMode, JournalInfo,
    Snapshot, SnapshotMode, SnapshotOptions, WalInfo, WalMode, DEFAULT_MAX_PATHNAME,
};

/// Options for [`open_database`], [`open_database_from_bytes`], and [`open_database_from_image`].
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub wal: WalMode,
    pub hot_journal: HotJournalMode,
    pub max_pathname: usize,
    pub cache_pages: Option<usize>,
    pub meter: Option<Arc<dyn Meter>>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            wal: WalMode::Apply,
            hot_journal: HotJournalMode::Overlay,
            max_pathname: DEFAULT_MAX_PATHNAME,
            cache_pages: None,
            meter: None,
        }
    }
}

/// Column selector for [`ScanOptions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnSpec {
    Name(String),
    Cid(usize),
}

impl From<&str> for ColumnSpec {
    fn from(s: &str) -> Self {
        ColumnSpec::Name(s.to_string())
    }
}

impl From<String> for ColumnSpec {
    fn from(s: String) -> Self {
        ColumnSpec::Name(s)
    }
}

impl From<usize> for ColumnSpec {
    fn from(cid: usize) -> Self {
        ColumnSpec::Cid(cid)
    }
}

/// Projection options for table scans and [`TableReader::get`].
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Columns by name (ASCII case-insensitive) or cid, in output order. `None` = all columns.
    pub columns: Option<Vec<ColumnSpec>>,
    /// Prepend the rowid to every row (rowid tables only).
    pub rowid: bool,
}

#[derive(Debug, Clone)]
struct SnapshotMeta {
    path: String,
    mode: SnapshotMode,
    header: Option<RawDbHeader>,
    main_file_size: u64,
    wal: Option<WalInfo>,
    journal: Option<JournalInfo>,
}

impl SnapshotMeta {
    fn from_snapshot(s: &Snapshot) -> Self {
        Self {
            path: s.path.clone(),
            mode: s.mode,
            header: s.header,
            main_file_size: s.main_file_size,
            wal: s.wal.clone(),
            journal: s.journal.clone(),
        }
    }
}

/// Open a SQLite database read-only over `vfs`.
pub fn open_database(
    vfs: &dyn Vfs,
    path: &str,
    options: OpenOptions,
) -> SqliteResult<SqliteDatabase> {
    let snap_opts = SnapshotOptions {
        wal: options.wal,
        hot_journal: options.hot_journal,
        max_pathname: options.max_pathname,
    };
    let snapshot = open_snapshot(vfs, path, snap_opts)?;
    open_from_snapshot(snapshot, None, options.cache_pages, options.meter)
}

/// Open a SQLite database held in memory (main bytes plus optional `-wal` and `-journal`).
pub fn open_database_from_bytes(
    main: &[u8],
    wal: Option<&[u8]>,
    journal: Option<&[u8]>,
    options: OpenOptions,
) -> SqliteResult<SqliteDatabase> {
    open_database_from_owned_bytes(
        main.to_vec(),
        wal.map(|b| b.to_vec()),
        journal.map(|b| b.to_vec()),
        options,
    )
}

/// Open a SQLite database from owned raw byte buffers (avoids cloning `wal`/`journal`).
pub fn open_database_from_owned_bytes(
    main: Vec<u8>,
    wal: Option<Vec<u8>>,
    journal: Option<Vec<u8>>,
    options: OpenOptions,
) -> SqliteResult<SqliteDatabase> {
    let snap_opts = SnapshotOptions {
        wal: options.wal,
        hot_journal: options.hot_journal,
        max_pathname: options.max_pathname,
    };
    let snapshot = open_snapshot_from_bytes(main, wal, journal, snap_opts)?;
    open_from_snapshot(snapshot, None, options.cache_pages, options.meter)
}

/// Open a single raw SQLite main-file image directly (matching `sqlite3_deserialize` /
/// `openDatabaseFromImage` sizing rules; ideal for fuzzing and raw-image inspection).
pub fn open_database_from_image(
    image: Vec<u8>,
    options: OpenOptions,
) -> SqliteResult<SqliteDatabase> {
    let (snapshot, file_pages) = open_snapshot_from_image(image);
    open_from_snapshot(
        snapshot,
        Some(file_pages),
        options.cache_pages,
        options.meter,
    )
}

/// Internal constructor used by [`crate::write::SqliteConnection`] to run read-only SQL
/// queries over live uncommitted pages in zero-copy fashion.
#[allow(dead_code)]
pub(crate) fn open_from_page_source(
    source: Box<dyn crate::page_source::PageSource>,
    path: String,
    meter: Option<Arc<dyn Meter>>,
) -> SqliteResult<SqliteDatabase> {
    open_from_page_source_cached(source, path, meter, None)
}

/// Internal constructor reusing a cached [`SchemaCatalog`] when `sqlite_schema` is unchanged.
pub(crate) fn open_from_page_source_cached(
    mut source: Box<dyn crate::page_source::PageSource>,
    path: String,
    meter: Option<Arc<dyn Meter>>,
    cached_schema: Option<Arc<SchemaCatalog>>,
) -> SqliteResult<SqliteDatabase> {
    let ps = source.page_size();
    let pc = source.page_count();
    let header = if pc > 0 {
        let p1 = source.read_page_shared(1)?;
        crate::format::read_raw_header(&p1)
    } else {
        None
    };
    let meta = SnapshotMeta {
        path,
        mode: if pc == 0 {
            SnapshotMode::Empty
        } else {
            SnapshotMode::Rollback
        },
        header,
        main_file_size: u64::from(pc) * u64::from(ps),
        wal: None,
        journal: None,
    };
    let bt_opts = BtreeOptions {
        cache_pages: None,
        file_page_count: Some(pc),
        meter,
    };
    let btree = Btree::open(source, bt_opts)?;
    let schema = match cached_schema {
        Some(s) => s,
        None => {
            let schema_header = if pc == 0 { None } else { meta.header.as_ref() };
            Arc::new(load_schema(
                &btree,
                &LoadSchemaOptions {
                    header: Some(schema_header),
                },
            )?)
        }
    };
    Ok(SqliteDatabase {
        meta,
        btree: Arc::new(btree),
        schema,
        closed: Arc::new(AtomicBool::new(false)),
    })
}

fn open_from_snapshot(
    snapshot: Snapshot,
    file_page_count: Option<u32>,
    cache_pages: Option<usize>,
    meter: Option<Arc<dyn Meter>>,
) -> SqliteResult<SqliteDatabase> {
    let meta = SnapshotMeta::from_snapshot(&snapshot);
    let page_count = snapshot.page_count;
    let empty = snapshot.mode == SnapshotMode::Empty;
    let bt_opts = BtreeOptions {
        cache_pages,
        file_page_count,
        meter,
    };
    let btree = Btree::open(Box::new(snapshot), bt_opts)?;
    let schema_header = if page_count == 0 || empty {
        None
    } else {
        meta.header.as_ref()
    };
    let schema = match load_schema(
        &btree,
        &LoadSchemaOptions {
            header: Some(schema_header),
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = btree.pager().close();
            return Err(e);
        }
    };
    Ok(SqliteDatabase {
        meta,
        btree: Arc::new(btree),
        schema: Arc::new(schema),
        closed: Arc::new(AtomicBool::new(false)),
    })
}

/// A read-only, point-in-time view of one SQLite database.
#[derive(Debug, Clone)]
pub struct SqliteDatabase {
    meta: SnapshotMeta,
    btree: Arc<Btree>,
    schema: Arc<SchemaCatalog>,
    closed: Arc<AtomicBool>,
}

impl SqliteDatabase {
    pub fn path(&self) -> &str {
        &self.meta.path
    }

    pub fn mode(&self) -> SnapshotMode {
        self.meta.mode
    }

    pub fn main_file_size(&self) -> u64 {
        self.meta.main_file_size
    }

    pub fn wal(&self) -> Option<&WalInfo> {
        self.meta.wal.as_ref()
    }

    pub fn journal(&self) -> Option<&JournalInfo> {
        self.meta.journal.as_ref()
    }

    pub fn header(&self) -> Option<&RawDbHeader> {
        self.meta.header.as_ref()
    }

    pub fn page_size(&self) -> u32 {
        self.btree.page_size()
    }

    pub fn page_count(&self) -> u32 {
        self.btree.page_count()
    }

    pub fn encoding(&self) -> TextEncoding {
        self.schema.encoding
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn btree(&self) -> &Btree {
        &self.btree
    }

    pub fn schema(&self) -> &SchemaCatalog {
        &self.schema
    }

    pub(crate) fn schema_arc(&self) -> &Arc<SchemaCatalog> {
        &self.schema
    }

    pub fn tables(&self) -> &[Arc<TableInfo>] {
        &self.schema.tables
    }

    pub fn indexes(&self) -> &[Arc<IndexInfo>] {
        &self.schema.indexes
    }

    fn check_open(&self) -> SqliteResult<()> {
        if self.is_closed() {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    /// Look up a readable table by name (ASCII case-insensitive), including `sqlite_schema`.
    pub fn table(&self, name: &str) -> SqliteResult<TableReader> {
        self.check_open()?;
        match self.schema.object(name) {
            Some(SchemaObject::Table(info)) => Ok(TableReader {
                btree: self.btree.clone(),
                closed: self.closed.clone(),
                info: info.clone(),
            }),
            None => Err(SqliteError::object_not_found(
                format!("no such table: {name}"),
                name,
            )),
            Some(SchemaObject::View(_)) => Err(SqliteError::object_not_found(
                format!("{name} is a view: its rows come from a query, not from the file"),
                name,
            )),
            Some(SchemaObject::VirtualTable(vt)) => Err(SqliteError::object_not_found(
                format!(
                    "{name} is a virtual table (module {}): its rows come from a module, not from the file",
                    vt.module
                ),
                name,
            )),
            Some(_) => Err(SqliteError::object_not_found(
                format!("no such table: {name}"),
                name,
            )),
        }
    }

    /// Look up an index by name (ASCII case-insensitive).
    pub fn index(&self, name: &str) -> SqliteResult<IndexReader> {
        self.check_open()?;
        let Some(info) = self.schema.index(name) else {
            return Err(SqliteError::object_not_found(
                format!("no such index: {name}"),
                name,
            ));
        };
        let table = self
            .schema
            .table(&info.table_name)
            .expect("index table exists")
            .clone();
        Ok(IndexReader::new(
            self.btree.clone(),
            self.closed.clone(),
            info.clone(),
            table,
        ))
    }

    /// Release the SHARED lock and cached pages. Idempotent.
    pub fn close(&self) -> SqliteResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.btree.pager().close()
    }
}

// ───────────────────────────── row decoding ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueSrc {
    Rowid,
    Field,
}

#[derive(Debug, Clone)]
struct ScanPlan {
    columns: Vec<ColumnInfo>,
    src: Vec<ValueSrc>,
    field: Vec<usize>,
    real_back: Vec<bool>,
    dflt: Vec<SqlValue>,
    needs_rowid: bool,
    any_field: bool,
    max_field: usize,
}

/// Handle for reading rows from one SQLite table.
#[derive(Debug, Clone)]
pub struct TableReader {
    btree: Arc<Btree>,
    closed: Arc<AtomicBool>,
    info: Arc<TableInfo>,
}

impl TableReader {
    fn check_open(&self) -> SqliteResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn info(&self) -> &TableInfo {
        &self.info
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn columns(&self) -> &[ColumnInfo] {
        &self.info.columns
    }

    pub fn without_rowid(&self) -> bool {
        self.info.without_rowid
    }

    /// Names of all stored (non-VIRTUAL) columns.
    pub fn stored_columns(&self) -> Vec<&str> {
        self.info
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.name.as_str())
            .collect()
    }

    fn build_plan(&self, options: Option<&ScanOptions>) -> SqliteResult<ScanPlan> {
        let all = &self.info.columns;
        let cols: Vec<ColumnInfo> = match options.and_then(|o| o.columns.as_ref()) {
            None => all.clone(),
            Some(specs) => {
                let mut out = Vec::with_capacity(specs.len());
                for spec in specs {
                    match spec {
                        ColumnSpec::Cid(cid) => {
                            let Some(col) = all.get(*cid) else {
                                return Err(SqliteError::misuse(format!(
                                    "table {} has no column {cid}",
                                    self.info.name
                                )));
                            };
                            out.push(col.clone());
                        }
                        ColumnSpec::Name(name) => {
                            let key = ascii_fold(name);
                            let Some(col) = all.iter().find(|c| ascii_fold(&c.name) == key) else {
                                return Err(SqliteError::object_not_found(
                                    format!("no such column: {name} (table {})", self.info.name),
                                    name,
                                ));
                            };
                            out.push(col.clone());
                        }
                    }
                }
                out
            }
        };

        for c in &cols {
            if c.hidden == ColumnHiddenKind::Virtual {
                return Err(SqliteError::virtual_column(&self.info.name, &c.name));
            }
        }
        let with_rowid = options.is_some_and(|o| o.rowid);
        if with_rowid && self.info.without_rowid {
            return Err(SqliteError::misuse(format!(
                "table {} is a WITHOUT ROWID table: it has no rowid",
                self.info.name
            )));
        }

        let n = cols.len() + usize::from(with_rowid);
        let mut src = Vec::with_capacity(n);
        let mut field = Vec::with_capacity(n);
        let mut real_back = Vec::with_capacity(n);
        let mut dflt = Vec::with_capacity(n);
        let mut needs_rowid = with_rowid;

        if with_rowid {
            src.push(ValueSrc::Rowid);
            field.push(0);
            real_back.push(false);
            dflt.push(SqlValue::Null);
        }
        let mut any_field = false;
        let mut max_field = 0usize;
        for c in &cols {
            if c.rowid_alias {
                src.push(ValueSrc::Rowid);
                field.push(0);
                real_back.push(false);
                dflt.push(SqlValue::Null);
                needs_rowid = true;
            } else {
                let f = c.storage_index.expect("stored column has storage_index");
                src.push(ValueSrc::Field);
                field.push(f);
                real_back.push(c.affinity == Affinity::Real);
                dflt.push(c.default_value.clone());
                any_field = true;
                if f > max_field {
                    max_field = f;
                }
            }
        }

        Ok(ScanPlan {
            columns: cols,
            src,
            field,
            real_back,
            dflt,
            needs_rowid,
            any_field,
            max_field,
        })
    }

    /// Open an internal forward cursor over specific stored column indices `cids` without
    /// cloning `ColumnInfo` strings (used by the SQL scan bridge where `columns()` is unused).
    pub(crate) fn cursor_cids(&self, cids: &[usize]) -> SqliteResult<RowCursor> {
        self.check_open()?;
        let all = &self.info.columns;
        let n = cids.len();
        let mut src = Vec::with_capacity(n);
        let mut field = Vec::with_capacity(n);
        let mut real_back = Vec::with_capacity(n);
        let mut dflt = Vec::with_capacity(n);
        let mut needs_rowid = false;
        let mut any_field = false;
        let mut max_field = 0usize;
        for &cid in cids {
            let Some(c) = all.get(cid) else {
                return Err(SqliteError::misuse(format!(
                    "table {} has no column {cid}",
                    self.info.name
                )));
            };
            if c.hidden == ColumnHiddenKind::Virtual {
                return Err(SqliteError::virtual_column(&self.info.name, &c.name));
            }
            if c.rowid_alias {
                src.push(ValueSrc::Rowid);
                field.push(0);
                real_back.push(false);
                dflt.push(SqlValue::Null);
                needs_rowid = true;
            } else {
                let f = c.storage_index.expect("stored column has storage_index");
                src.push(ValueSrc::Field);
                field.push(f);
                real_back.push(c.affinity == Affinity::Real);
                dflt.push(c.default_value.clone());
                any_field = true;
                if f > max_field {
                    max_field = f;
                }
            }
        }
        let plan = ScanPlan {
            columns: Vec::new(),
            src,
            field,
            real_back,
            dflt,
            needs_rowid,
            any_field,
            max_field,
        };
        let cur = if self.info.without_rowid {
            RowCursorInner::WithoutRowid(Box::new(self.btree.index_cursor(self.info.root_page)?))
        } else {
            RowCursorInner::Rowid(Box::new(self.btree.table_cursor(self.info.root_page)?))
        };
        Ok(RowCursor {
            closed: self.closed.clone(),
            cur,
            plan,
            fast_slots: [(0, 0, 0); 32],
            started: false,
            valid: false,
        })
    }

    /// Open a forward cursor over the table's rows in B-tree order.
    pub fn cursor(&self, options: Option<&ScanOptions>) -> SqliteResult<RowCursor> {
        self.check_open()?;
        let plan = self.build_plan(options)?;
        let cur = if self.info.without_rowid {
            RowCursorInner::WithoutRowid(Box::new(self.btree.index_cursor(self.info.root_page)?))
        } else {
            RowCursorInner::Rowid(Box::new(self.btree.table_cursor(self.info.root_page)?))
        };
        Ok(RowCursor {
            closed: self.closed.clone(),
            cur,
            plan,
            fast_slots: [(0, 0, 0); 32],
            started: false,
            valid: false,
        })
    }

    /// Look up one row by rowid (B-tree seek). Rowid tables only.
    pub fn get(
        &self,
        rowid: i64,
        options: Option<&ScanOptions>,
    ) -> SqliteResult<Option<Vec<SqlValue>>> {
        self.check_open()?;
        if self.info.without_rowid {
            return Err(SqliteError::misuse(format!(
                "table {} is a WITHOUT ROWID table: it has no rowid",
                self.info.name
            )));
        }
        let plan = self.build_plan(options)?;
        let mut cur = self.btree.table_cursor(self.info.root_page)?;
        if cur.seek(rowid)? != SeekResult::Exact {
            return Ok(None);
        }
        let found_rowid = cur.rowid()?;
        let rec = cur.record()?;
        Ok(Some(decode_row(&plan, rec, found_rowid)?))
    }

    /// Count rows by walking B-tree leaves (no record decoding).
    pub fn count(&self) -> SqliteResult<u64> {
        self.check_open()?;
        if self.info.without_rowid {
            self.btree.index_cursor(self.info.root_page)?.count()
        } else {
            self.btree.table_cursor(self.info.root_page)?.count()
        }
    }
}

fn decode_value(
    plan: &ScanPlan,
    j: usize,
    rec: &mut RecordView,
    rowid: i64,
) -> SqliteResult<SqlValue> {
    if plan.src[j] == ValueSrc::Rowid {
        return Ok(SqlValue::Int(rowid));
    }
    let f = plan.field[j];
    let d = &plan.dflt[j];
    if !d.is_null() && rec.serial_type(f)?.is_none() {
        return Ok(d.clone());
    }
    let v = rec.value(f)?;
    Ok(if plan.real_back[j] {
        real_read_back(v)
    } else {
        v
    })
}

fn decode_row(plan: &ScanPlan, rec: &mut RecordView, rowid: i64) -> SqliteResult<Vec<SqlValue>> {
    let n = plan.src.len();
    let mut out = Vec::with_capacity(n);
    for j in 0..n {
        out.push(decode_value(plan, j, rec, rowid)?);
    }
    Ok(out)
}

/// Fast stack-only peek result for single-column key reads (`TopK` heap filtering).
pub(crate) enum FastPeekVal {
    Int(i64),
    Other(SqlValue),
}

#[inline]
fn decode_int_serial_inline(payload: &[u8], st: u32, off: usize) -> Option<i64> {
    match st {
        8 => Some(0),
        9 => Some(1),
        1 => Some(i64::from(payload[off] as i8)),
        2 => Some(i64::from(i16::from_be_bytes([
            payload[off],
            payload[off + 1],
        ]))),
        3 => {
            let v = ((i32::from(payload[off] as i8)) << 16)
                | (i32::from(payload[off + 1]) << 8)
                | i32::from(payload[off + 2]);
            Some(i64::from(v))
        }
        4 => Some(i64::from(i32::from_be_bytes([
            payload[off],
            payload[off + 1],
            payload[off + 2],
            payload[off + 3],
        ]))),
        5 => {
            let v = ((i64::from(payload[off] as i8)) << 40)
                | (i64::from(payload[off + 1]) << 32)
                | (i64::from(payload[off + 2]) << 24)
                | (i64::from(payload[off + 3]) << 16)
                | (i64::from(payload[off + 4]) << 8)
                | i64::from(payload[off + 5]);
            Some(v)
        }
        6 => Some(i64::from_be_bytes([
            payload[off],
            payload[off + 1],
            payload[off + 2],
            payload[off + 3],
            payload[off + 4],
            payload[off + 5],
            payload[off + 6],
            payload[off + 7],
        ])),
        _ => None,
    }
}

#[derive(Debug)]
enum RowCursorInner {
    Rowid(Box<TableCursor>),
    WithoutRowid(Box<IndexCursor>),
}

/// Forward cursor over a table's logical rows.
#[derive(Debug)]
pub struct RowCursor {
    closed: Arc<AtomicBool>,
    cur: RowCursorInner,
    plan: ScanPlan,
    fast_slots: [(u32, u32, u32); 32],
    started: bool,
    valid: bool,
}

impl RowCursor {
    fn check_open(&self) -> SqliteResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn columns(&self) -> &[ColumnInfo] {
        &self.plan.columns
    }

    pub fn width(&self) -> usize {
        self.plan.src.len()
    }

    /// Advance to the next row (the first on the first call); `false` at EOF.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        if !self.started {
            self.started = true;
            self.valid = match &mut self.cur {
                RowCursorInner::Rowid(c) => c.first()?,
                RowCursorInner::WithoutRowid(c) => c.first()?,
            };
        } else if self.valid {
            self.valid = match &mut self.cur {
                RowCursorInner::Rowid(c) => c.next()?,
                RowCursorInner::WithoutRowid(c) => c.next()?,
            };
        }
        Ok(self.valid)
    }

    /// Position on the last row in B-tree order; `false` if the table is empty.
    pub fn last(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        self.started = true;
        self.valid = match &mut self.cur {
            RowCursorInner::Rowid(c) => c.last()?,
            RowCursorInner::WithoutRowid(c) => c.last()?,
        };
        Ok(self.valid)
    }

    /// Move to the previous row in B-tree order; `false` at BOF.
    pub fn prev(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        if !self.started {
            return self.last();
        }
        if self.valid {
            self.valid = match &mut self.cur {
                RowCursorInner::Rowid(c) => c.prev()?,
                RowCursorInner::WithoutRowid(c) => c.prev()?,
            };
        }
        Ok(self.valid)
    }

    /// Seek to the row with smallest rowid `>= rowid` (rowid tables only).
    pub fn seek(&mut self, rowid: i64) -> SqliteResult<SeekResult> {
        self.check_open()?;
        self.started = true;
        let res = match &mut self.cur {
            RowCursorInner::Rowid(c) => c.seek(rowid)?,
            RowCursorInner::WithoutRowid(_) => {
                return Err(SqliteError::misuse("a WITHOUT ROWID table has no rowid"));
            }
        };
        self.valid = res != SeekResult::Eof;
        Ok(res)
    }

    /// Rowid of the current row (rowid tables only).
    pub fn rowid(&mut self) -> SqliteResult<i64> {
        self.check_open()?;
        match &mut self.cur {
            RowCursorInner::Rowid(c) => c.rowid(),
            RowCursorInner::WithoutRowid(_) => {
                Err(SqliteError::misuse("a WITHOUT ROWID table has no rowid"))
            }
        }
    }

    /// Decode the `j`-th projected value of the current row lazily.
    pub fn value(&mut self, j: usize) -> SqliteResult<SqlValue> {
        self.check_open()?;
        if j >= self.plan.src.len() {
            return Err(SqliteError::misuse(format!(
                "no value {j} (row width {})",
                self.plan.src.len()
            )));
        }
        if matches!(self.plan.src[j], ValueSrc::Rowid) {
            return Ok(SqlValue::Int(self.rowid()?));
        }
        let rec = match &mut self.cur {
            RowCursorInner::Rowid(c) => c.record()?,
            RowCursorInner::WithoutRowid(c) => c.record()?,
        };
        decode_value(&self.plan, j, rec, 0)
    }

    /// Decode all projected values of the current row.
    pub fn row(&mut self) -> SqliteResult<Vec<SqlValue>> {
        self.check_open()?;
        let rowid = if self.plan.needs_rowid {
            self.rowid()?
        } else {
            0
        };
        let rec = match &mut self.cur {
            RowCursorInner::Rowid(c) => c.record()?,
            RowCursorInner::WithoutRowid(c) => c.record()?,
        };
        decode_row(&self.plan, rec, rowid)
    }

    /// Decode projected values directly into a `total_cols`-wide vector at positions `cids`
    /// (single allocation, leaving unrequested slots as `SqlValue::Null`).
    pub(crate) fn row_into_sparse(
        &mut self,
        total_cols: usize,
        cids: &[usize],
    ) -> SqliteResult<Vec<SqlValue>> {
        let mut out = vec![SqlValue::Null; total_cols];
        self.decode_into_sparse_buf(&mut out, cids)?;
        Ok(out)
    }

    /// Advance to the next row and immediately peek key column `j` with a single `check_open()`.
    #[inline]
    pub(crate) fn next_peek_key_fast(&mut self, j: usize) -> SqliteResult<Option<FastPeekVal>> {
        if !self.next()? {
            return Ok(None);
        }
        self.peek_key_fast_checked(j).map(Some)
    }

    /// Fast stack-only single-column key peek for `TopK` scans (bypasses `RecordView` and
    /// `SqlValue` allocation on local rowid-table integer fields!).
    #[inline]
    fn peek_key_fast_checked(&mut self, j: usize) -> SqliteResult<FastPeekVal> {
        if matches!(self.plan.src[j], ValueSrc::Rowid) {
            return Ok(FastPeekVal::Int(self.rowid()?));
        }
        let target_f = self.plan.field[j];
        let real_back = self.plan.real_back[j];
        if let RowCursorInner::Rowid(ref mut tc) = self.cur {
            if let Some((_rid, payload, enc)) = tc.current_local_payload()? {
                if !payload.is_empty() {
                    let b0 = payload[0];
                    let (hdr_len_u32, mut hpos) = if b0 < 0x80 {
                        (u32::from(b0), 1usize)
                    } else {
                        get_varint32(payload, 0)
                    };
                    let hdr_len = hdr_len_u32 as usize;
                    if hdr_len > hpos && hdr_len <= payload.len() {
                        let mut boff = hdr_len;
                        let mut col = 0usize;
                        let mut ok = true;
                        while hpos < hdr_len {
                            let hb = payload[hpos];
                            let (st, n) = if hb < 0x80 {
                                (u32::from(hb), 1usize)
                            } else {
                                get_varint32(payload, hpos)
                            };
                            hpos += n;
                            if hpos > hdr_len {
                                ok = false;
                                break;
                            }
                            let blen = serial_type_length(st);
                            let Some(end) = boff.checked_add(blen) else {
                                ok = false;
                                break;
                            };
                            if end > payload.len() {
                                ok = false;
                                break;
                            }
                            if col == target_f {
                                if hpos == hdr_len && end != payload.len() {
                                    ok = false;
                                    break;
                                }
                                if !real_back {
                                    if let Some(iv) = decode_int_serial_inline(payload, st, boff) {
                                        return Ok(FastPeekVal::Int(iv));
                                    }
                                }
                                let v = decode_serial_value(&payload[boff..end], st, enc);
                                let v = if real_back { real_read_back(v) } else { v };
                                return Ok(match v {
                                    SqlValue::Int(iv) => FastPeekVal::Int(iv),
                                    other => FastPeekVal::Other(other),
                                });
                            }
                            boff = end;
                            col += 1;
                        }
                        if ok && hpos == hdr_len && boff == payload.len() && col <= target_f {
                            // Column past record end -> default value (matches `decode_value`).
                            let v = self.plan.dflt[j].clone();
                            return Ok(match v {
                                SqlValue::Int(iv) => FastPeekVal::Int(iv),
                                other => FastPeekVal::Other(other),
                            });
                        }
                    }
                }
            }
        }
        let v = self.value(j)?;
        Ok(match v {
            SqlValue::Int(iv) => FastPeekVal::Int(iv),
            other => FastPeekVal::Other(other),
        })
    }

    /// Fast inline local-payload decoder writing directly into `buf[cids[j]]` using the cursor's
    /// preallocated `fast_slots` scratch buffer (zero per-row `RecordView` or stack array memset).
    #[inline]
    fn decode_local_payload_into_sparse(
        plan: &ScanPlan,
        slots: &mut [(u32, u32, u32); 32],
        rid: i64,
        payload: &[u8],
        enc: TextEncoding,
        buf: &mut [SqlValue],
        cids: &[usize],
    ) -> bool {
        if !plan.any_field {
            for &cid in cids {
                buf[cid] = SqlValue::Int(rid);
            }
            return true;
        }
        let max_f = plan.max_field;
        if max_f >= 32 || payload.is_empty() || payload.len() > u32::MAX as usize {
            return false;
        }
        let b0 = payload[0];
        let (hdr_len_u32, mut hpos) = if b0 < 0x80 {
            (u32::from(b0), 1usize)
        } else {
            get_varint32(payload, 0)
        };
        let hdr_len = hdr_len_u32 as usize;
        if hdr_len <= hpos || hdr_len > payload.len() {
            return false;
        }
        let mut boff = hdr_len;
        let mut col = 0usize;
        while hpos < hdr_len && col <= max_f {
            let hb = payload[hpos];
            let (st, n) = if hb < 0x80 {
                (u32::from(hb), 1usize)
            } else {
                get_varint32(payload, hpos)
            };
            hpos += n;
            if hpos > hdr_len {
                return false;
            }
            let blen = serial_type_length(st);
            let Some(end) = boff.checked_add(blen) else {
                return false;
            };
            if end > payload.len() {
                return false;
            }
            slots[col] = (st, boff as u32, end as u32);
            boff = end;
            col += 1;
        }
        if hpos == hdr_len && boff != payload.len() {
            return false;
        }
        for (j, &cid) in cids.iter().enumerate() {
            if plan.src[j] == ValueSrc::Rowid {
                buf[cid] = SqlValue::Int(rid);
            } else {
                let f = plan.field[j];
                if f < col {
                    let (st, s_off, e_off) = slots[f];
                    let s_off = s_off as usize;
                    let e_off = e_off as usize;
                    let v = if let Some(iv) = decode_int_serial_inline(payload, st, s_off) {
                        SqlValue::Int(iv)
                    } else {
                        decode_serial_value(&payload[s_off..e_off], st, enc)
                    };
                    buf[cid] = if plan.real_back[j] {
                        real_read_back(v)
                    } else {
                        v
                    };
                } else {
                    buf[cid] = plan.dflt[j].clone();
                }
            }
        }
        true
    }

    /// Decode projected values into an existing `buf` slice at positions `cids` AND return
    /// `Some(rowid)` (for rowid tables) or `None` (for `WITHOUT ROWID`), using a zero-`RecordView`
    /// fast path for inline local rows.
    pub(crate) fn decode_into_sparse_buf_with_rowid(
        &mut self,
        buf: &mut [SqlValue],
        cids: &[usize],
    ) -> SqliteResult<Option<i64>> {
        self.check_open()?;
        if let RowCursorInner::Rowid(ref mut tc) = self.cur {
            if cids.is_empty() {
                return Ok(Some(tc.rowid()?));
            }
            if let Some((rid, payload, enc)) = tc.current_local_payload()? {
                if Self::decode_local_payload_into_sparse(
                    &self.plan,
                    &mut self.fast_slots,
                    rid,
                    payload,
                    enc,
                    buf,
                    cids,
                ) {
                    return Ok(Some(rid));
                }
            }
            let rid = tc.rowid()?;
            let rec = tc.record()?;
            for (j, &cid) in cids.iter().enumerate() {
                buf[cid] = decode_value(&self.plan, j, rec, rid)?;
            }
            return Ok(Some(rid));
        }

        if cids.is_empty() {
            return Ok(None);
        }
        let RowCursorInner::WithoutRowid(ref mut ic) = self.cur else {
            unreachable!();
        };
        let rec = ic.record()?;
        for (j, &cid) in cids.iter().enumerate() {
            buf[cid] = decode_value(&self.plan, j, rec, 0)?;
        }
        Ok(None)
    }

    /// Decode projected values into `buf` only when current `rowid <= hi` (or `hi.is_none()`),
    /// parsing the leaf cell header only ONCE on local rows.
    /// Returns `Ok(Some(rid))` when decoded, or `Ok(None)` when `rid > hi`.
    #[inline]
    pub(crate) fn decode_into_sparse_buf_if_rowid_le(
        &mut self,
        buf: &mut [SqlValue],
        cids: &[usize],
        hi: Option<i64>,
    ) -> SqliteResult<Option<i64>> {
        self.check_open()?;
        if let RowCursorInner::Rowid(ref mut tc) = self.cur {
            if let Some((rid, payload, enc)) = tc.current_local_payload()? {
                if let Some(h) = hi {
                    if rid > h {
                        return Ok(None);
                    }
                }
                if cids.is_empty()
                    || Self::decode_local_payload_into_sparse(
                        &self.plan,
                        &mut self.fast_slots,
                        rid,
                        payload,
                        enc,
                        buf,
                        cids,
                    )
                {
                    return Ok(Some(rid));
                }
            } else {
                let rid = tc.rowid()?;
                if let Some(h) = hi {
                    if rid > h {
                        return Ok(None);
                    }
                }
                if cids.is_empty() {
                    return Ok(Some(rid));
                }
            }
            let rid = tc.rowid()?;
            let rec = tc.record()?;
            for (j, &cid) in cids.iter().enumerate() {
                buf[cid] = decode_value(&self.plan, j, rec, rid)?;
            }
            return Ok(Some(rid));
        }
        Ok(None)
    }

    /// Decode projected values into an existing `buf` slice at positions `cids`
    /// without allocating a new `Vec<SqlValue>`.
    pub(crate) fn decode_into_sparse_buf(
        &mut self,
        buf: &mut [SqlValue],
        cids: &[usize],
    ) -> SqliteResult<()> {
        let _ = self.decode_into_sparse_buf_with_rowid(buf, cids)?;
        Ok(())
    }

    /// Whole-leaf batch scan over a rowid table (full scan or `[lo, hi]` rowid range):
    /// locks onto each leaf page's raw bytes ONCE per page, checks `closed` ONCE per page,
    /// and decodes local inline records directly into `buf[cids[j]]` without per-row
    /// `CellInfo` construction or `CursorCore::next()` overhead. Falls back cleanly per row
    /// for overflow payloads / >32-col tables, and falls back to normal cursor stepping for
    /// `WITHOUT ROWID` tables or metered B-trees.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(crate) fn visit_sparse_range<E>(
        &mut self,
        buf: &mut [SqlValue],
        cids: &[usize],
        lo: Option<i64>,
        hi: Option<i64>,
        meter: Option<&dyn Meter>,
        map_err: fn(SqliteError) -> E,
        map_limit: fn(xdb_core::ResourceLimitError) -> E,
        visit: &mut dyn FnMut(Option<i64>, &[SqlValue]) -> Result<bool, E>,
    ) -> Result<(), E> {
        let positioned = if let Some(l) = lo {
            self.seek(l).map_err(map_err)? != SeekResult::Eof
        } else {
            self.next().map_err(map_err)?
        };
        if !positioned {
            return Ok(());
        }

        if let RowCursorInner::Rowid(ref mut tc) = self.cur {
            let mut batch_ready = tc.current_leaf_batch();
            if batch_ready.is_some() {
                while let Some((page, start_ci, n_cell, usable, limits, enc)) = batch_ready.take() {
                    if self.closed.load(Ordering::Acquire) {
                        return Err(map_err(SqliteError::DatabaseClosed));
                    }
                    let data: &[u8] = &page.data;
                    let bound = (usable as usize).min(data.len());
                    let ptr_off = page.hdr_offset + 8;
                    if ptr_off + 2 * n_cell > bound {
                        return Err(map_err(SqliteError::corrupt(format!(
                            "page {}: leaf cell pointers out of bounds",
                            page.pgno
                        ))));
                    }
                    let max_leaf = limits.max_leaf;
                    for ci in start_ci..n_cell {
                        if let Some(m) = meter {
                            m.charge_steps(1).map_err(map_limit)?;
                        }
                        let pc = usize::from(read_u16_be(data, ptr_off + 2 * ci));
                        if pc >= bound {
                            return Err(map_err(SqliteError::corrupt(format!(
                                "page {}: leaf cell {ci} offset out of bounds",
                                page.pgno
                            ))));
                        }
                        let (psize_u32, n1) = if data[pc] < 0x80 {
                            (u32::from(data[pc]), 1usize)
                        } else {
                            get_payload_size_varint(data, pc)
                        };
                        let rid_off = pc + n1;
                        if rid_off >= bound {
                            return Err(map_err(SqliteError::corrupt(format!(
                                "page {}: leaf cell {ci} truncated before rowid",
                                page.pgno
                            ))));
                        }
                        let (rid, n2) = if data[rid_off] < 0x80 {
                            (i64::from(data[rid_off]), 1usize)
                        } else if rid_off + 1 < bound && data[rid_off + 1] < 0x80 {
                            (
                                (i64::from(data[rid_off] & 0x7f) << 7)
                                    | i64::from(data[rid_off + 1]),
                                2usize,
                            )
                        } else {
                            get_varint(data, rid_off)
                        };
                        let local_start = rid_off + n2;
                        if local_start > bound {
                            return Err(map_err(SqliteError::corrupt(format!(
                                "page {}: leaf cell {ci} rowid varint runs past usable page end",
                                page.pgno
                            ))));
                        }
                        let cell_end = if psize_u32 <= max_leaf {
                            local_start.saturating_add(psize_u32 as usize)
                        } else {
                            let local = crate::format::local_payload_size(
                                psize_u32,
                                limits.max_leaf,
                                limits.min_local,
                                usable,
                            );
                            local_start.saturating_add(local).saturating_add(4)
                        };
                        if cell_end > bound {
                            return Err(map_err(SqliteError::corrupt(format!(
                                "page {}: local payload runs past usable page end",
                                page.pgno
                            ))));
                        }
                        if let Some(h) = hi {
                            if rid > h {
                                return Ok(());
                            }
                        }
                        if cids.is_empty() {
                            if !visit(Some(rid), buf)? {
                                return Ok(());
                            }
                            continue;
                        }
                        let inline_decoded = if psize_u32 <= max_leaf {
                            Self::decode_local_payload_into_sparse(
                                &self.plan,
                                &mut self.fast_slots,
                                rid,
                                &data[local_start..cell_end],
                                enc,
                                buf,
                                cids,
                            )
                        } else {
                            false
                        };
                        let mut emitted_rid = Some(rid);
                        if !inline_decoded {
                            tc.set_leaf_cell_index(ci);
                            let r_rid = tc.rowid().map_err(map_err)?;
                            emitted_rid = Some(r_rid);
                            let rec = tc.record().map_err(map_err)?;
                            for (j, &cid) in cids.iter().enumerate() {
                                buf[cid] =
                                    decode_value(&self.plan, j, rec, r_rid).map_err(map_err)?;
                            }
                        }
                        if !visit(emitted_rid, buf)? {
                            return Ok(());
                        }
                    }
                    drop(page);
                    if !tc.advance_to_next_leaf().map_err(map_err)? {
                        self.valid = false;
                        return Ok(());
                    }
                    batch_ready = tc.current_leaf_batch();
                }
            }
        }

        // Fallback row-by-row path (metered B-tree or WITHOUT ROWID table).
        loop {
            if let Some(m) = meter {
                m.charge_steps(1).map_err(map_limit)?;
            }
            let rid_opt = if hi.is_some() {
                match self
                    .decode_into_sparse_buf_if_rowid_le(buf, cids, hi)
                    .map_err(map_err)?
                {
                    Some(rid) => Some(rid),
                    None => break,
                }
            } else {
                self.decode_into_sparse_buf_with_rowid(buf, cids)
                    .map_err(map_err)?
            };
            if !visit(rid_opt, buf)? {
                break;
            }
            if !self.next().map_err(map_err)? {
                break;
            }
        }
        Ok(())
    }
}

// ───────────────────────────── index entries ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IxRead {
    Raw,
    Real,
    Affinity,
}

#[derive(Debug, Clone)]
struct IndexPlan {
    read: Vec<IxRead>,
    aff: Vec<Affinity>,
    dflt: Vec<SqlValue>,
}

/// Handle for reading entries from one SQLite index.
#[derive(Debug, Clone)]
pub struct IndexReader {
    btree: Arc<Btree>,
    closed: Arc<AtomicBool>,
    info: Arc<IndexInfo>,
    table: Arc<TableInfo>,
    plan: IndexPlan,
}

impl IndexReader {
    fn new(
        btree: Arc<Btree>,
        closed: Arc<AtomicBool>,
        info: Arc<IndexInfo>,
        table: Arc<TableInfo>,
    ) -> Self {
        let n = info.columns.len();
        let mut read = vec![IxRead::Raw; n];
        let mut aff = vec![Affinity::Blob; n];
        let mut dflt = vec![SqlValue::Null; n];
        for (i, c) in info.columns.iter().enumerate() {
            if c.cid < 0 {
                continue;
            }
            let col = &table.columns[c.cid as usize];
            if col.hidden == ColumnHiddenKind::Virtual {
                read[i] = IxRead::Affinity;
                aff[i] = col.affinity;
            } else {
                if col.affinity == Affinity::Real {
                    read[i] = IxRead::Real;
                }
                dflt[i] = col.default_value.clone();
            }
        }
        Self {
            btree,
            closed,
            info,
            table,
            plan: IndexPlan { read, aff, dflt },
        }
    }

    fn check_open(&self) -> SqliteResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn info(&self) -> &IndexInfo {
        &self.info
    }

    pub fn table(&self) -> &TableInfo {
        &self.table
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Build a [`KeyInfo`] for seeking this index with [`crate::access::SearchKey`].
    pub fn key_info(&self, registry: Option<&CollationRegistry>) -> SqliteResult<KeyInfo> {
        key_info_for_index(&self.info, self.btree.info().text_encoding, registry)
    }

    /// Open a forward cursor over index entries in B-tree order.
    pub fn cursor(&self) -> SqliteResult<IndexEntryCursor> {
        self.check_open()?;
        Ok(IndexEntryCursor {
            closed: self.closed.clone(),
            cur: self.btree.index_cursor(self.info.root_page)?,
            plan: self.plan.clone(),
            started: false,
            valid: false,
        })
    }

    /// Number of entries in this index.
    pub fn count(&self) -> SqliteResult<u64> {
        self.check_open()?;
        self.btree.index_cursor(self.info.root_page)?.count()
    }
}

/// Forward/seekable cursor over logical index entries.
#[derive(Debug)]
pub struct IndexEntryCursor {
    closed: Arc<AtomicBool>,
    cur: IndexCursor,
    plan: IndexPlan,
    started: bool,
    valid: bool,
}

impl IndexEntryCursor {
    fn check_open(&self) -> SqliteResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn width(&self) -> usize {
        self.plan.read.len()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        if !self.started {
            self.started = true;
            self.valid = self.cur.first()?;
        } else if self.valid {
            self.valid = self.cur.next()?;
        }
        Ok(self.valid)
    }

    /// Position on the last entry in index B-tree order; `false` if the index is empty.
    pub fn last(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        self.started = true;
        self.valid = self.cur.last()?;
        Ok(self.valid)
    }

    /// Move to the previous entry in index B-tree order; `false` at BOF.
    pub fn prev(&mut self) -> SqliteResult<bool> {
        self.check_open()?;
        if !self.started {
            return self.last();
        }
        if self.valid {
            self.valid = self.cur.prev()?;
        }
        Ok(self.valid)
    }

    /// Seek to the entry matching `op` (`Lt`, `Le`, `Ge`, `Gt`) against `key`.
    pub fn seek_key(&mut self, key: &mut dyn RecordKey, op: SeekOp) -> SqliteResult<bool> {
        self.check_open()?;
        self.started = true;
        self.valid = self.cur.seek_key(key, op)?;
        Ok(self.valid)
    }

    /// Decode the `i`-th column of the current index entry (record order).
    pub fn value(&mut self, i: usize) -> SqliteResult<SqlValue> {
        self.check_open()?;
        if i >= self.plan.read.len() {
            return Err(SqliteError::misuse(format!("no index column {i}")));
        }
        let rec = self.cur.record()?;
        let d = &self.plan.dflt[i];
        if !d.is_null() && rec.serial_type(i)?.is_none() {
            return Ok(d.clone());
        }
        let v = rec.value(i)?;
        Ok(match self.plan.read[i] {
            IxRead::Real => real_read_back(v),
            IxRead::Affinity => apply_affinity(v, self.plan.aff[i]),
            IxRead::Raw => v,
        })
    }

    /// Decode all columns of the current index entry.
    pub fn row(&mut self) -> SqliteResult<Vec<SqlValue>> {
        self.check_open()?;
        let n = self.plan.read.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.value(i)?);
        }
        Ok(out)
    }
}
