//! Full read-write SQLite 3 connection (`SqliteConnection`) supporting DDL, DML,
//! `UPSERT`, `RETURNING`, constraints (`NOT NULL`, `UNIQUE`, `PRIMARY KEY`, `CHECK`,
//! `STRICT`, `GENERATED`), transactions & nested savepoints, `PRAGMA`, and crash-safe
//! rollback journal commits.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use xdb_core::value::{
    apply_affinity, apply_numeric_affinity, boolean_value, comparison_affinity,
    comparison_collation_name, int_value, integer_literal_value, mem_compare, must_be_int,
    real_literal_value, real_to_exact_i64, BuiltinCollation, Collation, CollationRegistry,
    OperandCollation,
};
use xdb_core::{Affinity, Meter, SqlValue, StorageClass, TextEncoding, Vfs};
use xdb_parser::{
    build_index, build_table, parse_expression, parse_statement, parse_statement_metered,
    AlterTableAction, AlterTableStmt, BinaryOp, BuildOptions, ColumnConstraint, ConflictAction,
    CreateIndexStmt, CreateTableStmt, CreateViewStmt, DefaultForm, DeleteStmt, Dialect,
    DropObjectType, DropStmt, Expr, GeneratedStorage, InsertStmt, OnError, ParseOptions, Parser,
    PragmaStmt, ResultColumn, SelectBody, SelectCore, SelectStmt, SetClause, Statement,
    TableConstraint, UpdateStmt, UpsertAction,
};
use xdb_sql::eval::{expr_affinity, expr_collation};
use xdb_sql::{
    eval_expr, execute_select_stmt_detailed, CatalogSubqueryRunner, EvalEnv, EvalScope,
    FunctionLimits, FunctionRegistry, NoSubqueries, QueryOptions, QueryParams, QueryResult,
    ScopeColumn,
};

use crate::access::{key_info_for_index, make_key_info, KeyFieldSpec, KeyInfo, SearchKey};
use crate::btree::{SeekOp, SeekResult};
use crate::database::{open_from_page_source_cached, ColumnSpec, ScanOptions, SqliteDatabase};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::BtreePageType;
use crate::schema::catalog::{GeneratedKind, SchemaCatalog};
use crate::schema::{ColumnHiddenKind, IndexInfo, SchemaObject, TableInfo};
use crate::write::btree::{
    append_index_cell_body, check_integrity_metered, compare_index_values, create_btree, drop_btree,
    index_bulk_load_sorted, index_bulk_load_sorted_flat, index_bulk_load_sorted_two_int,
    index_contains_prefix, index_delete, index_delete_two_int_asc, index_eq_scan_direct,
    index_insert, index_insert_two_int_asc, index_insert_with_values,
    index_order_scan_covering_direct, table_append_monotonic, table_contains_rowid,
    table_count_rows, table_delete, table_delete_and_read_sparse_fields_into,
    table_finish_delete_located, table_get_record_fields, table_get_record_sparse_fields,
    table_get_record_sparse_fields_into_hinted, table_insert,
    table_locate_and_read_sparse_fields_into_hinted, table_max_rowid,
    table_scan_single_stored_col_direct, table_topk_scan_direct, table_update_int_field_cached,
    table_update_single_field, table_update_single_field_hinted, DirectIndexEqMode,
    DirectIndexSlot, DirectTableLookupSlot, DirectTableSlot, FastFieldUpdateOutcome,
    FastIndexScan,
};
use crate::write::pager::WritePager;
use crate::write::record::{
    encode_record_refs_into, encode_record_with_format, encode_record_with_format_into,
    encode_two_int_record_stack,
};

/// Options for opening a read-write [`SqliteConnection`].
#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    /// Initial page size when creating a brand-new database (default `4096`).
    pub page_size: u32,
    /// SQL dialect (`Dialect::Sqlite` or `Dialect::Xdb`).
    pub dialect: Dialect,
    /// Optional sandbox resource meter.
    pub meter: Option<Arc<dyn Meter>>,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            page_size: 4096,
            dialect: Dialect::Sqlite,
            meter: None,
        }
    }
}

/// Output of [`SqliteConnection::execute`] / [`SqliteConnection::execute_with_params`].
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    /// Output column names (populated for `SELECT`, `PRAGMA`, and `RETURNING`).
    pub columns: Vec<String>,
    /// Output rows (populated for `SELECT`, `PRAGMA`, and `RETURNING`).
    pub rows: Vec<Vec<SqlValue>>,
    /// Number of rows inserted, updated, or deleted by the last DML statement.
    pub changes: i64,
    /// Rowid of the most recently inserted rowid-table row.
    pub last_insert_rowid: i64,
}

const STMT_CACHE_MAX_ENTRIES: usize = 64;
const STMT_CACHE_MAX_SQL_LEN: usize = 8192;

#[derive(Debug, Clone)]
enum FastValSrc {
    Param { index: usize, text: String },
    Const(SqlValue),
}

#[derive(Debug, Clone)]
enum FastInsertColSrc {
    IpkAuto,
    Default(SqlValue),
    Param { index: usize, text: String },
    Const(SqlValue),
}

#[derive(Debug, Clone)]
enum FastDeleteIndex {
    TwoIntAsc {
        root_page: u32,
        want_slot: Option<usize>,
        storage_idx: usize,
        default_val: SqlValue,
        real_back: bool,
        key_info: KeyInfo,
    },
    General {
        ix_idx: usize,
    },
}

#[derive(Debug)]
enum CachedFastDml {
    NotEligible {
        schema_gen: u64,
    },
    SimpleInsert {
        schema_gen: u64,
        dml: Arc<DmlTableState>,
        col_srcs: Vec<FastInsertColSrc>,
        ipk_src: Option<FastValSrc>,
        n_params_steps: u64,
        /// `(ix_idx, Some(col_cid))` when secondary index is a 2-column `(col, rowid)` ASC index.
        sec_indexes: Vec<(usize, Option<usize>)>,
    },
    PointUpdateSingleCol {
        schema_gen: u64,
        dml: Arc<DmlTableState>,
        target_sidx: usize,
        target_affinity: Affinity,
        target_not_null: bool,
        val_src: FastValSrc,
        rid_src: FastKeySrc,
    },
    PointDeleteRowid {
        schema_gen: u64,
        dml: Arc<DmlTableState>,
        rid_src: FastKeySrc,
        indexes: Vec<FastDeleteIndex>,
    },
}

impl CachedFastDml {
    #[inline]
    fn schema_gen(&self) -> u64 {
        match self {
            Self::NotEligible { schema_gen }
            | Self::SimpleInsert { schema_gen, .. }
            | Self::PointUpdateSingleCol { schema_gen, .. }
            | Self::PointDeleteRowid { schema_gen, .. } => *schema_gen,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedParsedStmt {
    stmt: Arc<Statement>,
    raw_start: usize,
    raw_end: usize,
    param_count: usize,
}

#[derive(Debug, Clone)]
struct CachedScript {
    sql: String,
    dialect: Dialect,
    stmts: Arc<[CachedParsedStmt]>,
    fast_dml: Option<Arc<CachedFastDml>>,
}

type SeqRowEntry = (String, SqlValue, i64, i64, Option<(usize, u32)>, bool);
type SeqCache = (u32, Vec<SeqRowEntry>);

#[derive(Debug, Clone)]
enum FastKeySrc {
    Const(Option<i64>),
    Param { index: usize, text: String },
}

#[derive(Debug, Clone)]
enum FastSelectSlot {
    Rowid,
    Field {
        want_pos: usize,
        storage_idx: usize,
        default_val: SqlValue,
        real_back: bool,
    },
}

#[derive(Debug)]
enum FastIndexEqPlan {
    Covering(Vec<DirectIndexSlot>),
    TableLookup {
        table_root: u32,
        rid_idx_pos: usize,
        want_fields: Vec<usize>,
        slots: Vec<DirectTableLookupSlot>,
    },
}

#[derive(Debug)]
enum CachedFastSelect {
    NotEligible {
        schema_gen: u64,
    },
    CountStar {
        schema_gen: u64,
        root_page: u32,
        out_col: String,
    },
    PointRowid {
        schema_gen: u64,
        root_page: u32,
        out_cols: Vec<String>,
        want_fields: Vec<usize>,
        slots: Vec<FastSelectSlot>,
        key_src: FastKeySrc,
    },
    IndexEq {
        schema_gen: u64,
        idx_root: u32,
        lead_desc: bool,
        lead_coll: Option<Collation>,
        col_affinity: Affinity,
        val_src: FastValSrc,
        out_cols: Vec<String>,
        plan: FastIndexEqPlan,
    },
    IndexOrderCovering {
        schema_gen: u64,
        idx_root: u32,
        desc: bool,
        limit: usize,
        offset: usize,
        out_cols: Vec<String>,
        slots: Vec<DirectIndexSlot>,
    },
    TopK {
        schema_gen: u64,
        root_page: u32,
        key_storage_idx: usize,
        key_default: SqlValue,
        key_real_back: bool,
        desc: bool,
        nulls_first: bool,
        collation: Option<Collation>,
        limit: usize,
        offset: usize,
        out_cols: Vec<String>,
        slots: Vec<DirectTableSlot>,
    },
}

impl CachedFastSelect {
    #[inline]
    fn schema_gen(&self) -> u64 {
        match self {
            Self::NotEligible { schema_gen }
            | Self::CountStar { schema_gen, .. }
            | Self::PointRowid { schema_gen, .. }
            | Self::IndexEq { schema_gen, .. }
            | Self::IndexOrderCovering { schema_gen, .. }
            | Self::TopK { schema_gen, .. } => *schema_gen,
        }
    }
}

#[derive(Debug)]
struct CachedSelectEntry {
    dialect: Dialect,
    sql: String,
    stmt: Arc<SelectStmt>,
    fast: Arc<CachedFastSelect>,
    /// Cached `(mut_gen, leaf_pgno, min_rid, max_rid)` for `PointRowid` leaf-hint lookups.
    last_leaf: Option<(u64, u32, i64, i64)>,
}

/// Read-write SQLite 3 database connection with ACID rollback journal & nested savepoints.
#[derive(Debug)]
pub struct SqliteConnection {
    pager: WritePager,
    dialect: Dialect,
    changes: i64,
    total_changes: i64,
    last_insert_rowid: i64,
    foreign_keys: bool,
    busy_timeout: i64,
    synchronous: i64,
    closed: bool,
    pending_error_conflict: Option<ConflictAction>,
    cached_schema: std::sync::Mutex<Option<(u64, Arc<SchemaCatalog>)>>,
    /// Warm read-only view keyed by `pager.mut_gen()`, immediately cleared before any write
    /// so `WritePager::pages` stays uniquely owned during mutations while consecutive SELECTs
    /// reuse parsed `BtreePage`s with zero page copies!
    cached_view: std::sync::Mutex<Option<(u64, Arc<SqliteDatabase>)>>,
    warm_view_populated: AtomicBool,
    /// Per-table DML state (`KeyInfo`, `ScopeColumn`s) cached across statements by `schema_gen`.
    cached_dml: (u64, Vec<(String, Arc<DmlTableState>)>),
    /// Cached built-in `CollationRegistry` for the current database text encoding.
    cached_collations: (TextEncoding, CollationRegistry),
    /// Cached default `FunctionRegistry` to avoid rebuilding on every `query_options()` call.
    cached_functions: FunctionRegistry,
    /// Cached `sqlite_sequence` `(root_page, [(table_name, name_val, rowid, seq, loc, dirty)])`,
    /// invalidated on rollback, schema change, or direct DML on `sqlite_sequence`.
    cached_seq: Option<SeqCache>,
    /// True when one or more `cached_seq` entries have deferred in-tx sequence bumps not yet
    /// written to `sqlite_sequence` pages (flushed on COMMIT, SAVEPOINT, DDL, or read overlay).
    seq_dirty: bool,
    /// Cached exact `table_max_rowid` per rowid table `root_page` (invalidated on DELETE,
    /// rowid-changing UPDATE, DDL, or rollback) to avoid redundant B-tree descents on bulk INSERT.
    cached_max_rowid: HashMap<u32, Option<i64>>,
    /// Single-entry hot front cache for monotonic sequential appends:
    /// `(table_root_page, max_rowid, rightmost_leaf_pgno)`. Invalidated on any rollback,
    /// DDL, DELETE, UPDATE, or non-monotonic write on that table.
    last_append_tail: Option<(u32, i64, u32)>,
    /// Single-entry leaf-range hint for point `UPDATE`/`DELETE` loops:
    /// `(table_root_page, leaf_pgno, min_rid, max_rid)`. Invalidated on any structural mutation,
    /// insert, rollback, DDL, or leaf split on that table.
    last_point_leaf: Option<(u32, u32, i64, i64)>,
    /// Reusable scratch buffer for encoding binary SQLite records in DML hot paths.
    record_scratch: Vec<u8>,
    /// Reusable scratch row buffer for single-row INSERT hot paths.
    row_scratch: Vec<SqlValue>,
    /// Bounded LRU cache of parsed statements keyed by `(sql, dialect)`.
    stmt_cache: Vec<CachedScript>,
    /// Bounded LRU cache of parsed read-only `SELECT` statements + compiled fast plans.
    select_cache: std::sync::Mutex<Vec<CachedSelectEntry>>,
}

impl SqliteConnection {
    /// Open a brand-new empty in-memory read-write SQLite database.
    pub fn open_in_memory(options: ConnectionOptions) -> SqliteResult<Self> {
        let pager = WritePager::new_in_memory(options.page_size, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    /// Load a raw SQLite `.db` byte image (plus optional `-wal`/`-journal`) into a writable
    /// in-memory connection.
    pub fn open_from_bytes(
        main_bytes: &[u8],
        wal_bytes: Option<&[u8]>,
        journal_bytes: Option<&[u8]>,
        options: ConnectionOptions,
    ) -> SqliteResult<Self> {
        let pager = WritePager::from_bytes(main_bytes, wal_bytes, journal_bytes, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    /// Load an owned SQLite `.db` byte image (plus optional `-wal`/`-journal`) into a writable
    /// in-memory connection without cloning the input buffers.
    pub fn open_from_owned_bytes(
        main_bytes: Vec<u8>,
        wal_bytes: Option<Vec<u8>>,
        journal_bytes: Option<Vec<u8>>,
        options: ConnectionOptions,
    ) -> SqliteResult<Self> {
        let pager =
            WritePager::from_owned_bytes(main_bytes, wal_bytes, journal_bytes, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    /// Return the current parsed schema catalog (used by hosts such as `xdb-wasm`).
    pub fn schema(&self) -> SqliteResult<Arc<SchemaCatalog>> {
        self.schema_arc()
    }

    /// Open (or create) a durable read-write SQLite database over `vfs` at `path`.
    pub fn open(
        vfs: Arc<dyn Vfs>,
        path: &str,
        options: ConnectionOptions,
    ) -> SqliteResult<Self> {
        let pager = WritePager::open_vfs(vfs, path, options.page_size, options.meter)?;
        Ok(Self::from_pager(pager, options.dialect))
    }

    fn from_pager(pager: WritePager, dialect: Dialect) -> Self {
        let enc = pager.encoding();
        Self {
            pager,
            dialect,
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            foreign_keys: false,
            busy_timeout: 0,
            synchronous: 2,
            closed: false,
            pending_error_conflict: None,
            cached_schema: std::sync::Mutex::new(None),
            cached_view: std::sync::Mutex::new(None),
            warm_view_populated: AtomicBool::new(false),
            cached_dml: (0, Vec::new()),
            cached_collations: (enc, CollationRegistry::new(enc)),
            cached_functions: FunctionRegistry::new(),
            cached_seq: None,
            seq_dirty: false,
            cached_max_rowid: HashMap::new(),
            last_append_tail: None,
            last_point_leaf: None,
            record_scratch: Vec::with_capacity(256),
            row_scratch: Vec::with_capacity(16),
            stmt_cache: Vec::new(),
            select_cache: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn constraint_err(&mut self, effective: ConflictAction, msg: String) -> SqliteError {
        self.pending_error_conflict = Some(effective);
        SqliteError::Sql(msg)
    }

    pub(super) fn check_open(&self) -> SqliteResult<()> {
        if self.closed {
            Err(SqliteError::DatabaseClosed)
        } else {
            Ok(())
        }
    }

    pub fn page_size(&self) -> u32 {
        self.pager.page_size()
    }

    pub fn page_count(&self) -> u32 {
        self.pager.page_count()
    }

    pub fn encoding(&self) -> TextEncoding {
        self.pager.encoding()
    }

    pub fn changes(&self) -> i64 {
        self.changes
    }

    pub fn total_changes(&self) -> i64 {
        self.total_changes
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    pub fn in_transaction(&self) -> bool {
        self.pager.in_transaction()
    }

    pub fn meter(&self) -> &Arc<dyn Meter> {
        self.pager.meter()
    }

    /// Export the live SQLite 3 binary database image (readable directly by C SQLite).
    pub fn to_bytes(&self) -> Vec<u8> {
        if let Some(ov) = self.cow_overlay_with_flushed_seq() {
            return ov.to_bytes();
        }
        self.pager.to_bytes()
    }

    /// Returns `true` if this connection has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close this connection (rolling back any uncommitted transaction and releasing pages).
    pub fn close(&mut self) -> SqliteResult<()> {
        if self.closed {
            return Ok(());
        }
        self.invalidate_warm_view();
        if self.pager.in_transaction() {
            let _ = self.pager.rollback();
        }
        self.pager.close();
        if let Ok(mut guard) = self.cached_schema.lock() {
            *guard = None;
        }
        self.cached_dml.1.clear();
        self.cached_seq = None;
        self.seq_dirty = false;
        self.invalidate_max_rowid_cache();
        self.stmt_cache.clear();
        if let Ok(mut guard) = self.select_cache.lock() {
            guard.clear();
        }
        self.closed = true;
        Ok(())
    }

    #[inline]
    fn invalidate_max_rowid_cache(&mut self) {
        self.cached_max_rowid.clear();
        self.last_append_tail = None;
        self.last_point_leaf = None;
    }

    #[inline]
    fn invalidate_max_rowid_for(&mut self, root_page: u32) {
        self.invalidate_max_rowid_only_for(root_page);
        if self.last_point_leaf.is_some_and(|(r, _, _, _)| r == root_page) {
            self.last_point_leaf = None;
        }
    }

    #[inline]
    fn invalidate_max_rowid_only_for(&mut self, root_page: u32) {
        if !self.cached_max_rowid.is_empty() {
            self.cached_max_rowid.remove(&root_page);
        }
        if self.last_append_tail.is_some_and(|(r, _, _)| r == root_page) {
            self.last_append_tail = None;
        }
    }

    #[inline]
    fn clear_append_leaf_only_for(&mut self, root_page: u32) {
        if let Some((r, m, _)) = self.last_append_tail {
            if r == root_page {
                self.cached_max_rowid.insert(r, Some(m));
                self.last_append_tail = None;
            }
        }
    }

    #[inline]
    fn clear_append_leaf_for(&mut self, root_page: u32) {
        self.clear_append_leaf_only_for(root_page);
        if self.last_point_leaf.is_some_and(|(r, _, _, _)| r == root_page) {
            self.last_point_leaf = None;
        }
    }

    #[inline]
    fn get_known_max_rowid(&self, root_page: u32) -> Option<Option<i64>> {
        if let Some((r, m, _)) = self.last_append_tail {
            if r == root_page {
                return Some(Some(m));
            }
        }
        self.cached_max_rowid.get(&root_page).copied()
    }

    #[inline]
    fn get_known_max_and_leaf(&self, root_page: u32) -> (Option<Option<i64>>, Option<u32>) {
        if let Some((r, m, leaf_pg)) = self.last_append_tail {
            if r == root_page {
                return (Some(Some(m)), Some(leaf_pg));
            }
        }
        (self.cached_max_rowid.get(&root_page).copied(), None)
    }

    #[inline]
    fn set_max_rowid_only(&mut self, root_page: u32, max_opt: Option<i64>) {
        if self.last_append_tail.is_some_and(|(r, _, _)| r == root_page) {
            self.last_append_tail = None;
        }
        if self.last_point_leaf.is_some_and(|(r, _, _, _)| r == root_page) {
            self.last_point_leaf = None;
        }
        self.cached_max_rowid.insert(root_page, max_opt);
    }

    #[inline]
    fn set_append_tail(&mut self, root_page: u32, max_rid: i64, hint_leaf: Option<u32>) {
        if self.last_point_leaf.is_some_and(|(r, _, _, _)| r == root_page) {
            self.last_point_leaf = None;
        }
        if let Some(leaf_pg) = hint_leaf {
            if let Some((old_r, old_m, _)) = self.last_append_tail {
                if old_r != root_page {
                    self.cached_max_rowid.insert(old_r, Some(old_m));
                }
            }
            self.last_append_tail = Some((root_page, max_rid, leaf_pg));
        } else {
            self.set_max_rowid_only(root_page, Some(max_rid));
        }
    }

    #[inline]
    fn invalidate_warm_view(&self) {
        if self.warm_view_populated.swap(false, AtomicOrdering::AcqRel) {
            if let Ok(mut guard) = self.cached_view.lock() {
                *guard = None;
            }
        }
    }

    /// Return a cached warm [`SqliteDatabase`] handle keyed by `pager.mut_gen()` for internal
    /// read-only queries, keeping the parsed `BtreePage` cache hot across consecutive `SELECT`s
    /// with a single `Arc::clone` (zero `SnapshotMeta` / `AtomicBool` allocations).
    fn warm_view(&self) -> SqliteResult<Arc<SqliteDatabase>> {
        self.check_open()?;
        let mgen = self.pager.mut_gen();
        if !self.seq_dirty && self.warm_view_populated.load(AtomicOrdering::Acquire) {
            if let Ok(guard) = self.cached_view.lock() {
                if let Some((cg, ref db)) = *guard {
                    if cg == mgen && !db.is_closed() {
                        return Ok(Arc::clone(db));
                    }
                }
            }
        }
        let view = Arc::new(self.live_view()?);
        if !self.seq_dirty {
            if let Ok(mut guard) = self.cached_view.lock() {
                *guard = Some((mgen, Arc::clone(&view)));
                self.warm_view_populated.store(true, AtomicOrdering::Release);
            }
        }
        Ok(view)
    }

    /// Return the current [`SchemaCatalog`] in O(1) without constructing a `SqliteDatabase`
    /// whenever the schema cache is valid.
    pub(crate) fn schema_arc(&self) -> SqliteResult<Arc<SchemaCatalog>> {
        self.check_open()?;
        let gen = self.pager.schema_gen();
        if let Ok(guard) = self.cached_schema.lock() {
            if let Some((cg, ref s)) = *guard {
                if cg == gen {
                    return Ok(Arc::clone(s));
                }
            }
        }
        let view = self.live_view()?;
        Ok(Arc::clone(view.schema_arc()))
    }

    fn cached_dml_for(&self, name: &str) -> Option<&Arc<DmlTableState>> {
        if self.cached_dml.0 != self.pager.schema_gen() {
            return None;
        }
        self.cached_dml
            .1
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, st)| st)
    }

    fn get_dml_state_by_name(&mut self, name: &str) -> SqliteResult<Arc<DmlTableState>> {
        let gen = self.pager.schema_gen();
        if self.cached_dml.0 != gen {
            self.cached_dml.0 = gen;
            self.cached_dml.1.clear();
        }
        if let Some((_, st)) = self
            .cached_dml
            .1
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            return Ok(Arc::clone(st));
        }
        let schema = self.schema_arc()?;
        let Some(table_ref) = schema.table(name) else {
            return Err(SqliteError::Sql(format!("no such table: {name}")));
        };
        let table = table_ref.clone();
        drop(schema);
        let enc = self.pager.encoding();
        let st = Arc::new(DmlTableState::build(table, enc)?);
        self.cached_dml
            .1
            .push((st.table.name.clone(), Arc::clone(&st)));
        Ok(st)
    }

    /// Build a live read-only [`SqliteDatabase`] view over the current uncommitted pager state.
    pub fn live_view(&self) -> SqliteResult<SqliteDatabase> {
        self.check_open()?;
        let gen = self.pager.schema_gen();
        let cached = self
            .cached_schema
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|(cg, s)| (*cg == gen).then(|| Arc::clone(s))));
        let hit = cached.is_some();
        let page_source = if let Some(ov) = self.cow_overlay_with_flushed_seq() {
            ov.snapshot_page_source()
        } else {
            self.pager.snapshot_page_source()
        };
        let view = open_from_page_source_cached(
            page_source,
            self.pager.path().to_string(),
            Some(self.pager.meter().clone()),
            cached,
        )?;
        if !hit {
            if let Ok(mut guard) = self.cached_schema.lock() {
                *guard = Some((gen, Arc::clone(view.schema_arc())));
            }
        }
        Ok(view)
    }

    fn query_options(&self, dialect: Dialect) -> QueryOptions {
        let enc = self.pager.encoding();
        let collations = if self.cached_collations.0 == enc {
            self.cached_collations.1.clone()
        } else {
            CollationRegistry::new(enc)
        };
        let meter = if self.pager.is_noop_meter() {
            None
        } else {
            Some(self.pager.meter().clone())
        };
        QueryOptions {
            dialect,
            limits: FunctionLimits::default(),
            functions: self.cached_functions.clone(),
            collations,
            meter,
            now_julian_ms: None,
            local_tz_offset_minutes: 0,
            dqs: false,
            case_sensitive_like: false,
            max_cte_iterations: 100_000,
            changes: self.changes,
            total_changes: self.total_changes,
            last_insert_rowid: self.last_insert_rowid,
        }
    }

    fn exec_fast_select(
        &self,
        fast: &CachedFastSelect,
        params: &QueryParams,
        leaf_hint: &mut Option<(u32, i64, i64)>,
    ) -> SqliteResult<QueryResult> {
        match fast {
            CachedFastSelect::NotEligible { .. } => unreachable!(),
            CachedFastSelect::CountStar {
                root_page, out_col, ..
            } => {
                let cnt = table_count_rows(&self.pager, *root_page)?;
                Ok(QueryResult {
                    columns: vec![out_col.clone()],
                    rows: vec![vec![SqlValue::Int(cnt as i64)]],
                })
            }
            CachedFastSelect::PointRowid {
                root_page,
                out_cols,
                want_fields,
                slots,
                key_src,
                ..
            } => {
                let target_rid = match key_src {
                    FastKeySrc::Const(opt) => *opt,
                    FastKeySrc::Param { index, text } => {
                        let v = params.resolve_ref(*index, text);
                        match coerce_rowid_eq_lookup(v) {
                            RowidEqLookup::Exact(r) => Some(r),
                            RowidEqLookup::Impossible => None,
                        }
                    }
                };
                let Some(rid) = target_rid else {
                    return Ok(QueryResult {
                        columns: out_cols.clone(),
                        rows: Vec::new(),
                    });
                };
                let enc = self.pager.encoding();
                let mut stack_dec = [const { SqlValue::Null }; 32];
                let mut heap_dec: Vec<SqlValue> = Vec::new();
                let n_fields = if want_fields.is_empty() {
                    if !table_contains_rowid(&self.pager, *root_page, rid)? {
                        return Ok(QueryResult {
                            columns: out_cols.clone(),
                            rows: Vec::new(),
                        });
                    }
                    0
                } else if want_fields.len() <= 32 {
                    match table_get_record_sparse_fields_into_hinted(
                        &self.pager,
                        *root_page,
                        rid,
                        enc,
                        want_fields,
                        &mut stack_dec[..want_fields.len()],
                        leaf_hint,
                    )? {
                        Some(n) => n,
                        None => {
                            return Ok(QueryResult {
                                columns: out_cols.clone(),
                                rows: Vec::new(),
                            });
                        }
                    }
                } else {
                    heap_dec.resize(want_fields.len(), SqlValue::Null);
                    match table_get_record_sparse_fields_into_hinted(
                        &self.pager,
                        *root_page,
                        rid,
                        enc,
                        want_fields,
                        &mut heap_dec,
                        leaf_hint,
                    )? {
                        Some(n) => n,
                        None => {
                            return Ok(QueryResult {
                                columns: out_cols.clone(),
                                rows: Vec::new(),
                            });
                        }
                    }
                };
                let dec_slice: &mut [SqlValue] = if want_fields.len() <= 32 {
                    &mut stack_dec[..want_fields.len()]
                } else {
                    &mut heap_dec
                };
                let mut row = Vec::with_capacity(slots.len());
                for slot in slots {
                    match slot {
                        FastSelectSlot::Rowid => row.push(SqlValue::Int(rid)),
                        FastSelectSlot::Field {
                            want_pos,
                            storage_idx,
                            default_val,
                            real_back,
                        } => {
                            let val = if *storage_idx < n_fields {
                                std::mem::replace(&mut dec_slice[*want_pos], SqlValue::Null)
                            } else {
                                default_val.clone()
                            };
                            row.push(if *real_back {
                                crate::schema::real_read_back(val)
                            } else {
                                val
                            });
                        }
                    }
                }
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows: vec![row],
                })
            }
            CachedFastSelect::IndexEq {
                idx_root,
                lead_desc,
                lead_coll,
                col_affinity,
                val_src,
                out_cols,
                plan,
                ..
            } => {
                let raw_ref: &SqlValue = match val_src {
                    FastValSrc::Const(v) => v,
                    FastValSrc::Param { index, text } => params.resolve_ref(*index, text),
                };
                if raw_ref.is_null() || matches!(raw_ref, SqlValue::Real(r) if r.is_nan()) {
                    return Ok(QueryResult {
                        columns: out_cols.clone(),
                        rows: Vec::new(),
                    });
                }
                let owned_key: Option<SqlValue> = if *col_affinity >= Affinity::Numeric {
                    if let SqlValue::Text(b) = raw_ref {
                        apply_numeric_affinity(b, false)
                    } else {
                        None
                    }
                } else if *col_affinity == Affinity::Text {
                    if matches!(raw_ref, SqlValue::Int(_) | SqlValue::Real(_)) {
                        Some(apply_affinity(raw_ref.clone(), Affinity::Text))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let lookup_key = owned_key.as_ref().unwrap_or(raw_ref);
                let enc = self.pager.encoding();
                let mode = match plan {
                    FastIndexEqPlan::Covering(slots) => DirectIndexEqMode::Covering(slots),
                    FastIndexEqPlan::TableLookup {
                        table_root,
                        rid_idx_pos,
                        want_fields,
                        slots,
                    } => DirectIndexEqMode::TableLookup {
                        table_root: *table_root,
                        rid_idx_pos: *rid_idx_pos,
                        want_fields,
                        slots,
                    },
                };
                let rows = index_eq_scan_direct(
                    &self.pager,
                    *idx_root,
                    lookup_key,
                    *lead_desc,
                    lead_coll.as_ref(),
                    enc,
                    mode,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
            CachedFastSelect::IndexOrderCovering {
                idx_root,
                desc,
                limit,
                offset,
                out_cols,
                slots,
                ..
            } => {
                let enc = self.pager.encoding();
                let rows = index_order_scan_covering_direct(
                    &self.pager,
                    *idx_root,
                    *desc,
                    *limit,
                    *offset,
                    slots,
                    enc,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
            CachedFastSelect::TopK {
                root_page,
                key_storage_idx,
                key_default,
                key_real_back,
                desc,
                nulls_first,
                collation,
                limit,
                offset,
                out_cols,
                slots,
                ..
            } => {
                let enc = self.pager.encoding();
                let rows = table_topk_scan_direct(
                    &self.pager,
                    *root_page,
                    *key_storage_idx,
                    key_default,
                    *key_real_back,
                    *desc,
                    *nulls_first,
                    collation.as_ref(),
                    *limit,
                    *offset,
                    slots,
                    enc,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
        }
    }

    fn try_build_fast_select(&self, sel: &SelectStmt) -> Option<CachedFastSelect> {
        if sel.with.is_some() || sel.order_by_all.is_some() {
            return None;
        }
        let SelectBody::Core(core) = &sel.body else {
            return None;
        };
        if core.distinct.is_some()
            || core.group_by.is_some()
            || core.group_by_all
            || core.having.is_some()
            || core.windows.is_some()
            || core.from_first
        {
            return None;
        }
        let Some(xdb_parser::FromSource::Table(ft)) = &core.from else {
            return None;
        };
        if ft.indexed_by.is_some() {
            return None;
        }
        if let Some(ref sc) = ft.schema {
            if !sc.name.eq_ignore_ascii_case("main") {
                return None;
            }
        }
        let sgen = self.pager.schema_gen();
        let schema = self.schema_arc().ok()?;
        let table = schema.table(&ft.name.name)?;
        if table.without_rowid || table.has_virtual_columns || table.system {
            return None;
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();

        // 0. `SELECT <cols> FROM tbl ORDER BY <col> [ASC|DESC] [NULLS FIRST|LAST] LIMIT <k> [OFFSET <o>]`
        //    (no WHERE): covering-index fast path OR direct-on-WritePager Top-K scan!
        if sel.order_by.is_some() || sel.limit.is_some() {
            if core.where_clause.is_some() {
                return None;
            }
            let ob = sel.order_by.as_ref()?;
            let lim = sel.limit.as_ref()?;
            if ob.len() != 1 {
                return None;
            }
            let lim_val = try_eval_simple_const_expr(&lim.count, &QueryParams::empty()).ok()??;
            let lim_i64 = lim_val.as_i64()?;
            if lim_i64 < 0 {
                return None;
            }
            let limit = usize::try_from(lim_i64).ok()?;
            let offset = if let Some(ref off_e) = lim.offset {
                let ov = try_eval_simple_const_expr(off_e, &QueryParams::empty()).ok()??;
                let oi = ov.as_i64()?;
                if oi < 0 {
                    return None;
                }
                usize::try_from(oi).ok()?
            } else {
                0
            };
            let cap = limit.checked_add(offset)?;
            if cap > 1024 {
                return None;
            }

            // Extract ORDER BY column & optional explicit COLLATE.
            let ot = &ob[0];
            let (ob_inner, explicit_coll) = match &ot.expr {
                Expr::Collate {
                    operand, collation, ..
                } => (operand.as_ref(), Some(collation.name.as_str())),
                other => (other, None),
            };
            let Expr::Column(ob_c) = ob_inner else {
                return None;
            };
            if let Some(ref sc) = ob_c.schema {
                if !sc.name.eq_ignore_ascii_case("main") {
                    return None;
                }
            }
            if let Some(ref tq) = ob_c.table {
                if !tq.name.eq_ignore_ascii_case(alias) {
                    return None;
                }
            } else if core.columns.iter().any(|rc| {
                matches!(
                    rc,
                    ResultColumn::Expr(re)
                        if re
                            .alias
                            .as_ref()
                            .is_some_and(|a| a.name.eq_ignore_ascii_case(&ob_c.column.name))
                )
            }) {
                // Unqualified ORDER BY name matches an explicit SELECT alias (shadows table column).
                return None;
            }
            let ob_cid = table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&ob_c.column.name))?;
            // Let `sql.rs` handle `ORDER BY <ipk>` in O(limit) directly on rowid B-tree.
            if Some(ob_cid) == table.ipk_column {
                return None;
            }
            let ob_col = &table.columns[ob_cid];
            if ob_col.hidden == ColumnHiddenKind::Virtual {
                return None;
            }
            let req_desc = ot.order == Some(xdb_parser::SortOrder::Desc);
            let req_nulls_first = match ot.nulls {
                Some(xdb_parser::NullsOrder::First) => true,
                Some(xdb_parser::NullsOrder::Last) => false,
                None => !req_desc,
            };
            let eff_coll_name = explicit_coll.unwrap_or(ob_col.collation.as_str());

            // Resolve projected columns once (shared between IndexOrderCovering & TopK).
            let mut out_cols = Vec::with_capacity(core.columns.len());
            let mut proj_cids: Vec<Option<usize>> = Vec::with_capacity(core.columns.len()); // None => rowid
            for rc in &core.columns {
                match rc {
                    ResultColumn::Star(st) => {
                        if st.exclude.is_some() || st.replace.is_some() {
                            return None;
                        }
                        if let Some(ref tq) = st.table {
                            if !tq.name.eq_ignore_ascii_case(alias) {
                                return None;
                            }
                        }
                        for (cid, col) in table.columns.iter().enumerate() {
                            if col.hidden == ColumnHiddenKind::Virtual {
                                return None;
                            }
                            out_cols.push(col.name.clone());
                            proj_cids.push(if Some(cid) == table.ipk_column {
                                None
                            } else {
                                Some(cid)
                            });
                        }
                    }
                    ResultColumn::Expr(re) => {
                        let Expr::Column(c) = &re.expr else {
                            return None;
                        };
                        if let Some(ref sc) = c.schema {
                            if !sc.name.eq_ignore_ascii_case("main") {
                                return None;
                            }
                        }
                        if let Some(ref tq) = c.table {
                            if !tq.name.eq_ignore_ascii_case(alias) {
                                return None;
                            }
                        }
                        if let Some(cid) = table
                            .columns
                            .iter()
                            .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
                        {
                            let col = &table.columns[cid];
                            if col.hidden == ColumnHiddenKind::Virtual {
                                return None;
                            }
                            out_cols.push(
                                re.alias
                                    .as_ref()
                                    .map(|a| a.name.clone())
                                    .unwrap_or_else(|| col.name.clone()),
                            );
                            proj_cids.push(if Some(cid) == table.ipk_column {
                                None
                            } else {
                                Some(cid)
                            });
                        } else if matches!(
                            c.column.name.to_ascii_lowercase().as_str(),
                            "rowid" | "_rowid_" | "oid"
                        ) {
                            out_cols.push(
                                re.alias
                                    .as_ref()
                                    .map(|a| a.name.clone())
                                    .unwrap_or_else(|| c.column.name.clone()),
                            );
                            proj_cids.push(None);
                        } else {
                            return None;
                        }
                    }
                }
            }

            // Check if a non-partial covering index matches `(ob_cid, eff_coll_name)` and default NULLs order!
            if !ft.not_indexed && req_nulls_first != req_desc {
                for ix in &table.indexes {
                    if ix.without_rowid_primary_key || ix.partial || ix.key_count == 0 {
                        continue;
                    }
                    let Some(lead) = ix.columns.first() else {
                        continue;
                    };
                    if lead.cid != ob_cid as i32
                        || !lead.collation.eq_ignore_ascii_case(eff_coll_name)
                    {
                        continue;
                    }
                    // Check every projected column is present in `ix.columns`.
                    let rid_idx_pos = ix.columns.len().checked_sub(1)?;
                    if ix.columns[rid_idx_pos].cid != -1 {
                        continue;
                    }
                    let mut idx_slots = Vec::with_capacity(proj_cids.len());
                    let mut covered = true;
                    for &pc in &proj_cids {
                        match pc {
                            None => idx_slots.push(DirectIndexSlot {
                                idx_pos: rid_idx_pos,
                                real_back: false,
                            }),
                            Some(cid) => {
                                if let Some(pos) = ix
                                    .columns
                                    .iter()
                                    .take(ix.key_count)
                                    .position(|ic| ic.cid == cid as i32)
                                {
                                    idx_slots.push(DirectIndexSlot {
                                        idx_pos: pos,
                                        real_back: table.columns[cid].affinity == Affinity::Real,
                                    });
                                } else {
                                    covered = false;
                                    break;
                                }
                            }
                        }
                    }
                    if covered {
                        return Some(CachedFastSelect::IndexOrderCovering {
                            schema_gen: sgen,
                            idx_root: ix.root_page,
                            desc: req_desc ^ lead.desc,
                            limit,
                            offset,
                            out_cols,
                            slots: idx_slots,
                        });
                    }
                }
            }

            // If ANY non-covering index exists on `ob_cid`, let `sql.rs` `try_index_order_scan`
            // use it in O(limit * log N) instead of a full Top-K table scan!
            if !ft.not_indexed && req_nulls_first != req_desc {
                for ix in &table.indexes {
                    if ix.without_rowid_primary_key || ix.partial || ix.key_count == 0 {
                        continue;
                    }
                    if let Some(lead) = ix.columns.first() {
                        if lead.cid == ob_cid as i32
                            && lead.collation.eq_ignore_ascii_case(eff_coll_name)
                        {
                            return None;
                        }
                    }
                }
            }

            // Direct-on-WritePager Top-K table scan!
            let key_storage_idx = ob_col.storage_index?;
            let enc = self.pager.encoding();
            let collation = if eff_coll_name.eq_ignore_ascii_case("BINARY") {
                if enc == TextEncoding::Utf8 {
                    None
                } else {
                    Some(Collation::builtin(BuiltinCollation::Binary, enc))
                }
            } else {
                let bi = BuiltinCollation::from_name(eff_coll_name)?;
                Some(Collation::builtin(bi, enc))
            };
            let mut tbl_slots = Vec::with_capacity(proj_cids.len());
            for pc in proj_cids {
                match pc {
                    None => tbl_slots.push(DirectTableSlot::Rowid),
                    Some(cid) => {
                        let col = &table.columns[cid];
                        tbl_slots.push(DirectTableSlot::Field {
                            storage_idx: col.storage_index?,
                            default_val: col.default_value.clone(),
                            real_back: col.affinity == Affinity::Real,
                        });
                    }
                }
            }
            return Some(CachedFastSelect::TopK {
                schema_gen: sgen,
                root_page: table.root_page,
                key_storage_idx,
                key_default: ob_col.default_value.clone(),
                key_real_back: ob_col.affinity == Affinity::Real,
                desc: req_desc,
                nulls_first: req_nulls_first,
                collation,
                limit,
                offset,
                out_cols,
                slots: tbl_slots,
            });
        }

        // 1. `SELECT count(*) [AS alias] FROM tbl`
        if core.where_clause.is_none() && core.columns.len() == 1 {
            if let ResultColumn::Expr(re) = &core.columns[0] {
                if let Expr::Function(f) = &re.expr {
                    if f.name.name.eq_ignore_ascii_case("count")
                        && (f.star || f.args.is_empty())
                        && f.filter.is_none()
                        && f.over.is_none()
                        && f.order_by.is_none()
                        && f.quantifier.is_none()
                    {
                        let out_col = match &re.alias {
                            Some(a) => a.name.clone(),
                            None => re.text.trim().to_string(),
                        };
                        return Some(CachedFastSelect::CountStar {
                            schema_gen: sgen,
                            root_page: table.root_page,
                            out_col,
                        });
                    }
                }
            }
            return None;
        }

        // 2. `SELECT <cols> FROM tbl WHERE <ipk|rowid|indexed_col> = <param|const>`
        let we = core.where_clause.as_ref()?;
        let Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            ..
        } = we
        else {
            return None;
        };

        // 2a. Rowid / IPK equality point lookup.
        let rowid_rhs_opt = if is_dml_rowid_ref(left, table, alias) {
            Some(right.as_ref())
        } else if is_dml_rowid_ref(right, table, alias) {
            Some(left.as_ref())
        } else {
            None
        };

        if let Some(rhs_expr) = rowid_rhs_opt {
            let key_src = match rhs_expr {
                Expr::Variable(v) => FastKeySrc::Param {
                    index: v.index,
                    text: v.text.clone(),
                },
                Expr::Number(_)
                | Expr::String(_)
                | Expr::Blob(_)
                | Expr::Null { .. }
                | Expr::Boolean { .. }
                | Expr::Unary { .. } => {
                    let sv = try_eval_simple_const_expr(rhs_expr, &QueryParams::empty()).ok()??;
                    match coerce_rowid_eq_lookup(&sv) {
                        RowidEqLookup::Exact(r) => FastKeySrc::Const(Some(r)),
                        RowidEqLookup::Impossible => FastKeySrc::Const(None),
                    }
                }
                _ => return None,
            };

            let mut out_cols = Vec::with_capacity(core.columns.len());
            let mut want_fields: Vec<usize> = Vec::new();
            let mut slots: Vec<FastSelectSlot> = Vec::with_capacity(core.columns.len());

            for rc in &core.columns {
                match rc {
                    ResultColumn::Star(st) => {
                        if st.exclude.is_some() || st.replace.is_some() {
                            return None;
                        }
                        if let Some(ref tbl_qual) = st.table {
                            if !tbl_qual.name.eq_ignore_ascii_case(alias) {
                                return None;
                            }
                        }
                        for (cid, col) in table.columns.iter().enumerate() {
                            if col.hidden == ColumnHiddenKind::Virtual {
                                return None;
                            }
                            out_cols.push(col.name.clone());
                            if Some(cid) == table.ipk_column {
                                slots.push(FastSelectSlot::Rowid);
                            } else {
                                let sidx = col.storage_index?;
                                let want_pos = match want_fields.iter().position(|&f| f == sidx) {
                                    Some(p) => p,
                                    None => {
                                        let p = want_fields.len();
                                        want_fields.push(sidx);
                                        p
                                    }
                                };
                                slots.push(FastSelectSlot::Field {
                                    want_pos,
                                    storage_idx: sidx,
                                    default_val: col.default_value.clone(),
                                    real_back: col.affinity == Affinity::Real,
                                });
                            }
                        }
                    }
                    ResultColumn::Expr(re) => {
                        let Expr::Column(c) = &re.expr else {
                            return None;
                        };
                        if let Some(ref sc) = c.schema {
                            if !sc.name.eq_ignore_ascii_case("main") {
                                return None;
                            }
                        }
                        if let Some(ref tbl_qual) = c.table {
                            if !tbl_qual.name.eq_ignore_ascii_case(alias) {
                                return None;
                            }
                        }
                        if let Some(cid) = table
                            .columns
                            .iter()
                            .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
                        {
                            let col = &table.columns[cid];
                            if col.hidden == ColumnHiddenKind::Virtual {
                                return None;
                            }
                            out_cols.push(
                                re.alias
                                    .as_ref()
                                    .map(|a| a.name.clone())
                                    .unwrap_or_else(|| col.name.clone()),
                            );
                            if Some(cid) == table.ipk_column {
                                slots.push(FastSelectSlot::Rowid);
                            } else {
                                let sidx = col.storage_index?;
                                let want_pos = match want_fields.iter().position(|&f| f == sidx) {
                                    Some(p) => p,
                                    None => {
                                        let p = want_fields.len();
                                        want_fields.push(sidx);
                                        p
                                    }
                                };
                                slots.push(FastSelectSlot::Field {
                                    want_pos,
                                    storage_idx: sidx,
                                    default_val: col.default_value.clone(),
                                    real_back: col.affinity == Affinity::Real,
                                });
                            }
                        } else if matches!(
                            c.column.name.to_ascii_lowercase().as_str(),
                            "rowid" | "_rowid_" | "oid"
                        ) {
                            out_cols.push(
                                re.alias
                                    .as_ref()
                                    .map(|a| a.name.clone())
                                    .unwrap_or_else(|| c.column.name.clone()),
                            );
                            slots.push(FastSelectSlot::Rowid);
                        } else {
                            return None;
                        }
                    }
                }
            }

            // Duplicate references to the same field slot could steal `std::mem::replace` above;
            // bail cleanly if any `want_pos` appears more than once in `slots`!
            let mut used_pos = vec![false; want_fields.len()];
            for sl in &slots {
                if let FastSelectSlot::Field { want_pos, .. } = sl {
                    if used_pos[*want_pos] {
                        return None;
                    }
                    used_pos[*want_pos] = true;
                }
            }

            return Some(CachedFastSelect::PointRowid {
                schema_gen: sgen,
                root_page: table.root_page,
                out_cols,
                want_fields,
                slots,
                key_src,
            });
        }

        // 2b. Secondary-index equality scan (`WHERE <indexed_col> = <param|const>`):
        //     direct-on-WritePager index seek + covering or pinned-table-leaf lookup!
        if ft.not_indexed || table.indexes.is_empty() {
            return None;
        }
        fn extract_col_and_coll<'a>(
            e: &'a Expr,
            table: &TableInfo,
            alias: &str,
        ) -> Option<(usize, Option<&'a str>)> {
            let (inner, exp_coll) = match e {
                Expr::Collate {
                    operand, collation, ..
                } => (operand.as_ref(), Some(collation.name.as_str())),
                other => (other, None),
            };
            let Expr::Column(c) = inner else {
                return None;
            };
            if let Some(ref sc) = c.schema {
                if !sc.name.eq_ignore_ascii_case("main") {
                    return None;
                }
            }
            if let Some(ref tq) = c.table {
                if !tq.name.eq_ignore_ascii_case(alias) {
                    return None;
                }
            }
            let cid = table
                .columns
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))?;
            if table.columns[cid].hidden == ColumnHiddenKind::Virtual
                || Some(cid) == table.ipk_column
            {
                return None;
            }
            Some((cid, exp_coll))
        }
        let (filter_cid, exp_coll, rhs_expr) =
            if let Some((cid, ec)) = extract_col_and_coll(left, table, alias) {
                (cid, ec, right.as_ref())
            } else if let Some((cid, ec)) = extract_col_and_coll(right, table, alias) {
                (cid, ec, left.as_ref())
            } else {
                return None;
            };
        let val_src = compile_fast_val_src(rhs_expr)?;
        let filter_col = &table.columns[filter_cid];
        let eff_coll_name = exp_coll.unwrap_or(filter_col.collation.as_str());
        let enc = self.pager.encoding();
        let lead_coll = if eff_coll_name.eq_ignore_ascii_case("BINARY") {
            if enc == TextEncoding::Utf8 {
                None
            } else {
                Some(Collation::builtin(BuiltinCollation::Binary, enc))
            }
        } else {
            let bi = BuiltinCollation::from_name(eff_coll_name)?;
            Some(Collation::builtin(bi, enc))
        };

        // Resolve projected columns.
        let mut out_cols = Vec::with_capacity(core.columns.len());
        let mut proj_cids: Vec<Option<usize>> = Vec::with_capacity(core.columns.len());
        for rc in &core.columns {
            match rc {
                ResultColumn::Star(st) => {
                    if st.exclude.is_some() || st.replace.is_some() {
                        return None;
                    }
                    if let Some(ref tq) = st.table {
                        if !tq.name.eq_ignore_ascii_case(alias) {
                            return None;
                        }
                    }
                    for (cid, col) in table.columns.iter().enumerate() {
                        if col.hidden == ColumnHiddenKind::Virtual {
                            return None;
                        }
                        out_cols.push(col.name.clone());
                        proj_cids.push(if Some(cid) == table.ipk_column {
                            None
                        } else {
                            Some(cid)
                        });
                    }
                }
                ResultColumn::Expr(re) => {
                    let Expr::Column(c) = &re.expr else {
                        return None;
                    };
                    if let Some(ref sc) = c.schema {
                        if !sc.name.eq_ignore_ascii_case("main") {
                            return None;
                        }
                    }
                    if let Some(ref tq) = c.table {
                        if !tq.name.eq_ignore_ascii_case(alias) {
                            return None;
                        }
                    }
                    if let Some(cid) = table
                        .columns
                        .iter()
                        .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
                    {
                        let col = &table.columns[cid];
                        if col.hidden == ColumnHiddenKind::Virtual {
                            return None;
                        }
                        out_cols.push(
                            re.alias
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| col.name.clone()),
                        );
                        proj_cids.push(if Some(cid) == table.ipk_column {
                            None
                        } else {
                            Some(cid)
                        });
                    } else if matches!(
                        c.column.name.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ) {
                        out_cols.push(
                            re.alias
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| c.column.name.clone()),
                        );
                        proj_cids.push(None);
                    } else {
                        return None;
                    }
                }
            }
        }

        // Prefer a covering index matching `(filter_cid, eff_coll_name)`, else first matching index.
        let mut best_non_covering: Option<&IndexInfo> = None;
        for ix in &table.indexes {
            if ix.without_rowid_primary_key || ix.partial || ix.key_count == 0 {
                continue;
            }
            let Some(lead) = ix.columns.first() else {
                continue;
            };
            if lead.cid != filter_cid as i32 || !lead.collation.eq_ignore_ascii_case(eff_coll_name)
            {
                continue;
            }
            let rid_idx_pos = ix.columns.len().checked_sub(1)?;
            if ix.columns[rid_idx_pos].cid != -1 {
                continue;
            }
            let mut cov_slots = Vec::with_capacity(proj_cids.len());
            let mut covered = true;
            for &pc in &proj_cids {
                match pc {
                    None => cov_slots.push(DirectIndexSlot {
                        idx_pos: rid_idx_pos,
                        real_back: false,
                    }),
                    Some(cid) => {
                        if let Some(pos) = ix
                            .columns
                            .iter()
                            .take(ix.key_count)
                            .position(|ic| ic.cid == cid as i32)
                        {
                            cov_slots.push(DirectIndexSlot {
                                idx_pos: pos,
                                real_back: table.columns[cid].affinity == Affinity::Real,
                            });
                        } else {
                            covered = false;
                            break;
                        }
                    }
                }
            }
            if covered {
                return Some(CachedFastSelect::IndexEq {
                    schema_gen: sgen,
                    idx_root: ix.root_page,
                    lead_desc: lead.desc,
                    lead_coll,
                    col_affinity: filter_col.affinity,
                    val_src,
                    out_cols,
                    plan: FastIndexEqPlan::Covering(cov_slots),
                });
            }
            if best_non_covering.is_none() {
                best_non_covering = Some(ix);
            }
        }

        let ix = best_non_covering?;
        let rid_idx_pos = ix.columns.len().checked_sub(1)?;
        let mut want_fields: Vec<usize> = Vec::new();
        let mut slots: Vec<DirectTableLookupSlot> = Vec::with_capacity(proj_cids.len());
        for &pc in &proj_cids {
            match pc {
                None => slots.push(DirectTableLookupSlot::Rowid),
                Some(cid) => {
                    let col = &table.columns[cid];
                    let sidx = col.storage_index?;
                    let want_pos = match want_fields.iter().position(|&f| f == sidx) {
                        Some(p) => p,
                        None => {
                            let p = want_fields.len();
                            want_fields.push(sidx);
                            p
                        }
                    };
                    slots.push(DirectTableLookupSlot::Field {
                        want_pos,
                        storage_idx: sidx,
                        default_val: col.default_value.clone(),
                        real_back: col.affinity == Affinity::Real,
                    });
                }
            }
        }
        // Bail cleanly if any `want_pos` appears more than once (since `std::mem::replace` moves it).
        let mut used_pos = vec![false; want_fields.len()];
        for sl in &slots {
            if let DirectTableLookupSlot::Field { want_pos, .. } = sl {
                if used_pos[*want_pos] {
                    return None;
                }
                used_pos[*want_pos] = true;
            }
        }

        Some(CachedFastSelect::IndexEq {
            schema_gen: sgen,
            idx_root: ix.root_page,
            lead_desc: ix.columns[0].desc,
            lead_coll,
            col_affinity: filter_col.affinity,
            val_src,
            out_cols,
            plan: FastIndexEqPlan::TableLookup {
                table_root: table.root_page,
                rid_idx_pos,
                want_fields,
                slots,
            },
        })
    }

    /// Run a read-only `SELECT` query against the live connection state.
    pub fn query(&self, sql: &str) -> SqliteResult<QueryResult> {
        self.query_with_params(sql, &QueryParams::empty())
    }

    /// Run a parameterized read-only `SELECT` query against the live connection state.
    pub fn query_with_params(&self, sql: &str, params: &QueryParams) -> SqliteResult<QueryResult> {
        self.check_open()?;
        if !self.seq_dirty && sql.len() <= STMT_CACHE_MAX_SQL_LEN {
            let sgen = self.pager.schema_gen();
            let mgen = self.pager.mut_gen();
            let cached = self.select_cache.lock().ok().and_then(|mut cache| {
                let pos = cache
                    .iter()
                    .rposition(|e| e.dialect == self.dialect && e.sql == sql)?;
                let last = cache.len() - 1;
                if pos != last {
                    let entry = cache.remove(pos);
                    cache.push(entry);
                }
                let entry = &cache[last];
                let fast = Arc::clone(&entry.fast);
                let fast_ready = fast.schema_gen() == sgen
                    && !matches!(fast.as_ref(), CachedFastSelect::NotEligible { .. });
                let stmt = if fast_ready {
                    None
                } else {
                    Some(Arc::clone(&entry.stmt))
                };
                let leaf_hint = entry
                    .last_leaf
                    .and_then(|(g, lpg, lo, hi)| (g == mgen).then_some((lpg, lo, hi)));
                Some((stmt, fast, leaf_hint))
            });
            if let Some((stmt_opt, fast, orig_leaf_hint)) = cached {
                if !self.pager.is_noop_meter() {
                    self.pager.meter().charge_steps(1)?;
                }
                if stmt_opt.is_none() {
                    let mut leaf_hint = orig_leaf_hint;
                    let res = self.exec_fast_select(&fast, params, &mut leaf_hint)?;
                    if leaf_hint != orig_leaf_hint {
                        if let Ok(mut cache) = self.select_cache.lock() {
                            if let Some(entry) = cache
                                .iter_mut()
                                .rev()
                                .find(|e| e.dialect == self.dialect && e.sql == sql)
                            {
                                entry.last_leaf =
                                    leaf_hint.map(|(lpg, lo, hi)| (mgen, lpg, lo, hi));
                            }
                        }
                    }
                    return Ok(res);
                }
                let stmt = stmt_opt.expect("populated when fast plan unavailable or stale");
                let effective_fast = if fast.schema_gen() == sgen {
                    fast
                } else {
                    let rebuilt = Arc::new(
                        self.try_build_fast_select(&stmt)
                            .unwrap_or(CachedFastSelect::NotEligible { schema_gen: sgen }),
                    );
                    if let Ok(mut cache) = self.select_cache.lock() {
                        if let Some(entry) = cache
                            .iter_mut()
                            .rev()
                            .find(|e| e.dialect == self.dialect && e.sql == sql)
                        {
                            entry.fast = Arc::clone(&rebuilt);
                            entry.last_leaf = None;
                        }
                    }
                    rebuilt
                };
                if !matches!(effective_fast.as_ref(), CachedFastSelect::NotEligible { .. }) {
                    let mut leaf_hint = None;
                    let res = self.exec_fast_select(&effective_fast, params, &mut leaf_hint)?;
                    if let Some((lpg, lo, hi)) = leaf_hint {
                        if let Ok(mut cache) = self.select_cache.lock() {
                            if let Some(entry) = cache
                                .iter_mut()
                                .rev()
                                .find(|e| e.dialect == self.dialect && e.sql == sql)
                            {
                                entry.last_leaf = Some((mgen, lpg, lo, hi));
                            }
                        }
                    }
                    return Ok(res);
                }
                let view = self.warm_view()?;
                let opts = self.query_options(self.dialect);
                return view.query_parsed_with_options(&stmt, params, &opts);
            }
            let parse_opts = ParseOptions {
                dialect: self.dialect,
                ..ParseOptions::default()
            };
            let parsed = if self.pager.is_noop_meter() {
                parse_statement(sql, &parse_opts)
            } else {
                parse_statement_metered(sql, &parse_opts, self.pager.meter().as_ref())
            };
            if let Err(ref pe) = parsed {
                if let Some(rl) = pe.resource_limit {
                    return Err(SqliteError::ResourceLimit(rl));
                }
            }
            if let Ok(Statement::Select(sel)) = parsed {
                let arc_sel: Arc<SelectStmt> = Arc::from(sel);
                let fast = Arc::new(
                    self.try_build_fast_select(&arc_sel)
                        .unwrap_or(CachedFastSelect::NotEligible { schema_gen: sgen }),
                );
                let mut leaf_hint = None;
                let fast_res = if !matches!(fast.as_ref(), CachedFastSelect::NotEligible { .. }) {
                    Some(self.exec_fast_select(&fast, params, &mut leaf_hint)?)
                } else {
                    None
                };
                if let Ok(mut cache) = self.select_cache.lock() {
                    if cache.len() >= STMT_CACHE_MAX_ENTRIES {
                        cache.remove(0);
                    }
                    cache.push(CachedSelectEntry {
                        dialect: self.dialect,
                        sql: sql.to_string(),
                        stmt: Arc::clone(&arc_sel),
                        fast: Arc::clone(&fast),
                        last_leaf: leaf_hint.map(|(lpg, lo, hi)| (mgen, lpg, lo, hi)),
                    });
                }
                if let Some(res) = fast_res {
                    return Ok(res);
                }
                let view = self.warm_view()?;
                let opts = self.query_options(self.dialect);
                return view.query_parsed_with_options(&arc_sel, params, &opts);
            }
        }
        let view = self.warm_view()?;
        let opts = self.query_options(self.dialect);
        view.query_with_options(sql, params, &opts)
    }

    /// Execute one or more `;`-separated SQL statements (DDL, DML, transactions, PRAGMA, SELECT).
    pub fn execute(&mut self, sql: &str) -> SqliteResult<ExecResult> {
        self.execute_with_params(sql, &QueryParams::empty())
    }

    /// Execute one or more `;`-separated SQL statements with bound parameters.
    pub fn execute_with_params(
        &mut self,
        sql: &str,
        params: &QueryParams,
    ) -> SqliteResult<ExecResult> {
        self.execute_with_dialect(sql, params, self.dialect)
    }

    /// Execute one or more `;`-separated SQL statements using an explicit SQL dialect.
    pub fn execute_with_dialect(
        &mut self,
        sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        self.check_open()?;
        let mut last = ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        };

        // Fast statement-parse cache hit (reverse scan hits MRU at back on check 0!).
        if sql.len() <= STMT_CACHE_MAX_SQL_LEN {
            if let Some(pos) = self
                .stmt_cache
                .iter()
                .rposition(|c| c.dialect == dialect && c.sql == sql)
            {
                if !self.pager.is_noop_meter() {
                    self.pager.meter().charge_steps(1)?;
                }
                // Move to MRU end if not already last.
                let last_idx = self.stmt_cache.len() - 1;
                if pos != last_idx {
                    let entry = self.stmt_cache.remove(pos);
                    self.stmt_cache.push(entry);
                }
                // Pre-compiled single-statement DML fast path (checked BEFORE cloning `stmts`
                // so hot INSERT/UPDATE/DELETE loops avoid extra atomic refcount traffic!).
                if self.stmt_cache[last_idx].stmts.len() == 1 {
                    let sgen = self.pager.schema_gen();
                    let cached_fast = self.stmt_cache[last_idx]
                        .fast_dml
                        .as_ref()
                        .filter(|f| f.schema_gen() == sgen)
                        .cloned();
                    let fast = match cached_fast {
                        Some(f) => f,
                        None => {
                            let stmt0 = Arc::clone(&self.stmt_cache[last_idx].stmts[0].stmt);
                            let built = Arc::new(self.try_build_fast_dml(&stmt0));
                            self.stmt_cache[last_idx].fast_dml = Some(Arc::clone(&built));
                            built
                        }
                    };
                    if !matches!(fast.as_ref(), CachedFastDml::NotEligible { .. }) {
                        if let Some(res) = self.try_exec_fast_dml(&fast, params)? {
                            return Ok(res);
                        }
                    }
                }
                let stmts = Arc::clone(&self.stmt_cache[last_idx].stmts);
                let mut anon_param_offset = 0usize;
                for item in stmts.iter() {
                    let raw_stmt_sql = &sql[item.raw_start..item.raw_end];
                    let shifted_params;
                    let effective_params =
                        if anon_param_offset == 0 || params.positional.is_empty() {
                            params
                        } else {
                            let tail = if anon_param_offset < params.positional.len() {
                                params.positional[anon_param_offset..].to_vec()
                            } else {
                                Vec::new()
                            };
                            shifted_params = QueryParams {
                                positional: tail,
                                named: params.named.clone(),
                            };
                            &shifted_params
                        };
                    last = self.exec_one_stmt(
                        &item.stmt,
                        sql,
                        raw_stmt_sql,
                        effective_params,
                        dialect,
                    )?;
                    if !params.positional.is_empty() {
                        anon_param_offset = anon_param_offset.saturating_add(item.param_count);
                    }
                }
                return Ok(last);
            }
        }

        let parse_opts = ParseOptions {
            dialect,
            ..ParseOptions::default()
        };
        let mut offset = 0usize;
        let mut anon_param_offset = 0usize;
        let want_cache = sql.len() <= STMT_CACHE_MAX_SQL_LEN;
        let mut parsed_list: Vec<CachedParsedStmt> = Vec::new();
        let parser_meter = if self.pager.is_noop_meter() {
            None
        } else {
            Some(Arc::clone(self.pager.meter()))
        };
        let mut parser = Parser::new(sql, &parse_opts, parser_meter.as_deref());

        while offset < sql.len() {
            let parsed = parser.parse_at(offset).map_err(|e| {
                if let Some(rl) = e.resource_limit {
                    SqliteError::ResourceLimit(rl)
                } else {
                    SqliteError::Sql(e.to_string())
                }
            })?;
            let stmt_end = parsed.end;
            if let Some(stmt) = parsed.statement {
                let slice = &sql[offset..stmt_end];
                let trim_start = slice.len() - slice.trim_start().len();
                let trimmed = slice.trim().trim_end_matches(';').trim();
                let raw_start = offset + trim_start;
                let raw_end = raw_start + trimmed.len();
                let raw_stmt_sql = &sql[raw_start..raw_end];
                let shifted_params;
                let effective_params = if anon_param_offset == 0 || params.positional.is_empty() {
                    params
                } else {
                    let tail = if anon_param_offset < params.positional.len() {
                        params.positional[anon_param_offset..].to_vec()
                    } else {
                        Vec::new()
                    };
                    shifted_params = QueryParams {
                        positional: tail,
                        named: params.named.clone(),
                    };
                    &shifted_params
                };
                let pcount = if (!params.positional.is_empty() && stmt_end < sql.len()) || want_cache
                {
                    count_stmt_positional_params(raw_stmt_sql, dialect)
                } else {
                    0
                };
                let stmt_arc = Arc::new(stmt);
                if want_cache {
                    parsed_list.push(CachedParsedStmt {
                        stmt: Arc::clone(&stmt_arc),
                        raw_start,
                        raw_end,
                        param_count: pcount,
                    });
                }
                last = self.exec_one_stmt(&stmt_arc, sql, raw_stmt_sql, effective_params, dialect)?;
                // Advance positional parameter offset for subsequent statements in a script
                // using the exact same `n_var` numbering rule as `xdb_parser`.
                if !params.positional.is_empty() && stmt_end < sql.len() {
                    anon_param_offset = anon_param_offset.saturating_add(pcount);
                }
            }
            if stmt_end <= offset {
                break;
            }
            offset = stmt_end;
        }
        if want_cache && !parsed_list.is_empty() {
            if self.stmt_cache.len() >= STMT_CACHE_MAX_ENTRIES {
                self.stmt_cache.remove(0);
            }
            self.stmt_cache.push(CachedScript {
                sql: sql.to_string(),
                dialect,
                stmts: Arc::from(parsed_list),
                fast_dml: None,
            });
        }
        Ok(last)
    }

    /// Compile a single DML AST into a schema-pinned [`CachedFastDml`] plan (rebuilt
    /// automatically on `schema_gen` change) so repeated `INSERT`/`UPDATE`/`DELETE` loops
    /// bypass AST walking, column-name scans, and `Vec` savepoint-check allocations.
    fn try_build_fast_dml(&mut self, stmt: &Statement) -> CachedFastDml {
        let sgen = self.pager.schema_gen();
        let not_elig = || CachedFastDml::NotEligible { schema_gen: sgen };
        match stmt {
            Statement::Insert(ins) => {
                if ins
                    .table
                    .schema
                    .as_ref()
                    .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
                {
                    return not_elig();
                }
                if ins.replace_into
                    || ins.or_conflict == Some(ConflictAction::Replace)
                    || !ins.upsert.is_empty()
                    || ins.returning.is_some()
                    || ins.with.is_some()
                    || ins.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
                {
                    return not_elig();
                }
                let Ok(dml) = self.get_dml_state_by_name(&ins.table.name.name) else {
                    return not_elig();
                };
                let table = &dml.table;
                if table.system
                    || table.without_rowid
                    || table.columns.len() > 64
                    || !dml.gen_order.is_empty()
                    || !table.definition.checks.is_empty()
                    || dml.any_default_expr
                    || table.definition.strict
                    || !dml.simple_delete_elidable
                    || dml.any_unique_secondary
                {
                    return not_elig();
                }
                let Some(ref sel) = ins.select else {
                    return not_elig();
                };
                let Some(vals) = direct_values_fast_path(ins, sel) else {
                    return not_elig();
                };
                if vals.rows.len() != 1 {
                    return not_elig();
                }
                let vrow = &vals.rows[0];
                enum Tgt {
                    Col(usize),
                    Rowid,
                }
                let mut targets: Vec<Tgt> = Vec::with_capacity(vrow.items.len());
                if let Some(ref cols) = ins.columns {
                    if cols.len() != vrow.items.len() {
                        return not_elig();
                    }
                    for c in cols {
                        if let Some(pos) = table
                            .columns
                            .iter()
                            .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                        {
                            if table.columns[pos].generated.is_some() {
                                return not_elig();
                            }
                            targets.push(Tgt::Col(pos));
                        } else if c.name.eq_ignore_ascii_case("rowid")
                            || c.name.eq_ignore_ascii_case("_rowid_")
                            || c.name.eq_ignore_ascii_case("oid")
                        {
                            targets.push(Tgt::Rowid);
                        } else {
                            return not_elig();
                        }
                    }
                } else {
                    for c in &table.columns {
                        if c.generated.is_none() {
                            targets.push(Tgt::Col(c.cid));
                        }
                    }
                    if targets.len() != vrow.items.len() {
                        return not_elig();
                    }
                }
                let mut col_srcs: Vec<FastInsertColSrc> = table
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(cid, col)| {
                        if Some(cid) == table.ipk_column {
                            FastInsertColSrc::IpkAuto
                        } else {
                            FastInsertColSrc::Default(col.default_value.clone())
                        }
                    })
                    .collect();
                let mut assigned_mask = 0u64;
                let mut ipk_src: Option<FastValSrc> = None;
                for (tgt, item) in targets.iter().zip(vrow.items.iter()) {
                    let Some(src) = compile_fast_val_src(item) else {
                        return not_elig();
                    };
                    match *tgt {
                        Tgt::Col(cid) => {
                            if Some(cid) == table.ipk_column {
                                if ipk_src.is_some() {
                                    return not_elig();
                                }
                                ipk_src = Some(src);
                            } else {
                                let bit = 1u64 << cid;
                                if (assigned_mask & bit) == 0 {
                                    col_srcs[cid] = match src {
                                        FastValSrc::Param { index, text } => {
                                            FastInsertColSrc::Param { index, text }
                                        }
                                        FastValSrc::Const(v) => FastInsertColSrc::Const(v),
                                    };
                                    assigned_mask |= bit;
                                }
                            }
                        }
                        Tgt::Rowid => {
                            if ipk_src.is_some() {
                                return not_elig();
                            }
                            ipk_src = Some(src);
                        }
                    }
                }
                let mut sec_indexes = Vec::with_capacity(table.indexes.len());
                for (ix_idx, ix) in table.indexes.iter().enumerate() {
                    if ix.without_rowid_primary_key {
                        continue;
                    }
                    let Some(ref ki) = dml.index_key_infos[ix_idx] else {
                        return not_elig();
                    };
                    let two_int_cid = if ix.columns.len() == 2
                        && ix.columns[0].cid >= 0
                        && ix.columns[1].cid == -1
                        && ki.is_two_int_asc()
                    {
                        Some(ix.columns[0].cid as usize)
                    } else {
                        None
                    };
                    sec_indexes.push((ix_idx, two_int_cid));
                }
                CachedFastDml::SimpleInsert {
                    schema_gen: sgen,
                    dml,
                    col_srcs,
                    ipk_src,
                    n_params_steps: vrow.items.len() as u64,
                    sec_indexes,
                }
            }
            Statement::Update(upd) => {
                if upd
                    .table
                    .schema
                    .as_ref()
                    .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
                {
                    return not_elig();
                }
                if upd.from.is_some()
                    || upd.with.is_some()
                    || upd.returning.is_some()
                    || upd.or_conflict == Some(ConflictAction::Replace)
                    || upd.set.len() != 1
                    || upd.set[0].columns.len() != 1
                    || upd.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
                {
                    return not_elig();
                }
                let Ok(dml) = self.get_dml_state_by_name(&upd.table.name.name) else {
                    return not_elig();
                };
                let table = &dml.table;
                if table.system
                    || table.without_rowid
                    || table.columns.len() > 64
                    || !dml.gen_order.is_empty()
                    || !table.definition.checks.is_empty()
                    || table.definition.strict
                {
                    return not_elig();
                }
                let alias = upd
                    .table
                    .alias
                    .as_ref()
                    .unwrap_or(&upd.table.name)
                    .name
                    .as_str();
                let Some(ref we) = upd.where_clause else {
                    return not_elig();
                };
                let Some(rid_src) = compile_fast_rowid_eq_src(we, table, alias) else {
                    return not_elig();
                };
                let cname = &upd.set[0].columns[0].name;
                let Some(cid) = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(cname))
                else {
                    return not_elig();
                };
                if Some(cid) == table.ipk_column || (dml.indexed_col_mask & (1u64 << cid)) != 0 {
                    return not_elig();
                }
                let col = &table.columns[cid];
                let Some(sidx) = col.storage_index else {
                    return not_elig();
                };
                let Some(val_src) = compile_fast_val_src(&upd.set[0].expr) else {
                    return not_elig();
                };
                CachedFastDml::PointUpdateSingleCol {
                    schema_gen: sgen,
                    target_sidx: sidx,
                    target_affinity: col.affinity,
                    target_not_null: col.not_null,
                    val_src,
                    rid_src,
                    dml,
                }
            }
            Statement::Delete(del) => {
                if del
                    .table
                    .schema
                    .as_ref()
                    .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
                {
                    return not_elig();
                }
                if del.with.is_some()
                    || del.returning.is_some()
                    || del.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
                {
                    return not_elig();
                }
                let Ok(dml) = self.get_dml_state_by_name(&del.table.name.name) else {
                    return not_elig();
                };
                let table = &dml.table;
                if table.system || table.without_rowid || !dml.simple_delete_elidable {
                    return not_elig();
                }
                let alias = del
                    .table
                    .alias
                    .as_ref()
                    .unwrap_or(&del.table.name)
                    .name
                    .as_str();
                let Some(ref we) = del.where_clause else {
                    return not_elig();
                };
                let Some(rid_src) = compile_fast_rowid_eq_src(we, table, alias) else {
                    return not_elig();
                };
                let mut indexes = Vec::with_capacity(table.indexes.len());
                for (ix_idx, ix) in table.indexes.iter().enumerate() {
                    if ix.without_rowid_primary_key {
                        continue;
                    }
                    let Some(ref ki) = dml.index_key_infos[ix_idx] else {
                        return not_elig();
                    };
                    if ix.columns.len() == 2
                        && ix.columns[0].cid >= 0
                        && ix.columns[1].cid == -1
                        && ki.is_two_int_asc()
                    {
                        let cid = ix.columns[0].cid as usize;
                        let (want_slot, storage_idx) = if Some(cid) == table.ipk_column {
                            (None, 0)
                        } else {
                            let Some(slot) =
                                dml.simple_delete_want_cids.iter().position(|&c| c == cid)
                            else {
                                return not_elig();
                            };
                            (Some(slot), dml.simple_delete_want_fields[slot])
                        };
                        let col = &table.columns[cid];
                        indexes.push(FastDeleteIndex::TwoIntAsc {
                            root_page: ix.root_page,
                            want_slot,
                            storage_idx,
                            default_val: col.default_value.clone(),
                            real_back: col.affinity == Affinity::Real,
                            key_info: ki.clone(),
                        });
                    } else {
                        indexes.push(FastDeleteIndex::General { ix_idx });
                    }
                }
                CachedFastDml::PointDeleteRowid {
                    schema_gen: sgen,
                    dml,
                    rid_src,
                    indexes,
                }
            }
            _ => not_elig(),
        }
    }

    /// Execute a pre-compiled [`CachedFastDml`]. Returns `Ok(Some(res))` if executed directly,
    /// or `Ok(None)` to fall back cleanly to `exec_one_stmt` (e.g. constraint violation or
    /// non-monotonic explicit rowid).
    fn try_exec_fast_dml(
        &mut self,
        fast: &CachedFastDml,
        params: &QueryParams,
    ) -> SqliteResult<Option<ExecResult>> {
        match fast {
            CachedFastDml::NotEligible { .. } => Ok(None),
            CachedFastDml::SimpleInsert {
                dml,
                col_srcs,
                ipk_src,
                n_params_steps,
                sec_indexes,
                ..
            } => {
                let table = &dml.table;
                let n_cols = table.columns.len();
                debug_assert!(n_cols <= 64);

                // Resolve & apply column affinity using borrowed `&SqlValue` slots whenever
                // unchanged — zero `Box<[u8]>` heap clones for TEXT/BLOB parameters!
                // Use compact 8-slot stack arrays for typical tables (`n_cols <= 8`).
                let mut owned_small: [Option<SqlValue>; 8] = [const { None }; 8];
                let mut raw_small: [&SqlValue; 8] = [&NULL_SQL_VAL; 8];
                let mut refs_small: [&SqlValue; 8] = [&NULL_SQL_VAL; 8];
                let mut owned_big: Vec<Option<SqlValue>> = Vec::new();
                let mut raw_big: Vec<&SqlValue> = Vec::new();
                let mut refs_big: Vec<&SqlValue> = Vec::new();
                let (owned_vals, raw_refs, col_refs_mut): (
                    &mut [Option<SqlValue>],
                    &mut [&SqlValue],
                    &mut [&SqlValue],
                ) = if n_cols <= 8 {
                    (
                        &mut owned_small[..n_cols],
                        &mut raw_small[..n_cols],
                        &mut refs_small[..n_cols],
                    )
                } else {
                    owned_big.resize_with(n_cols, || None);
                    raw_big.resize(n_cols, &NULL_SQL_VAL);
                    refs_big.resize(n_cols, &NULL_SQL_VAL);
                    (&mut owned_big, &mut raw_big, &mut refs_big)
                };

                for (cid, (col, src)) in table.columns.iter().zip(col_srcs.iter()).enumerate() {
                    if Some(cid) == table.ipk_column {
                        continue;
                    }
                    let raw_ref: &SqlValue = match src {
                        FastInsertColSrc::IpkAuto => &NULL_SQL_VAL,
                        FastInsertColSrc::Default(v) | FastInsertColSrc::Const(v) => v,
                        FastInsertColSrc::Param { index, text } => {
                            params.resolve_ref(*index, text)
                        }
                    };
                    if let Some(coerced) = apply_affinity_if_changed(raw_ref, col.affinity) {
                        if col.not_null && coerced.is_null() {
                            return Ok(None);
                        }
                        owned_vals[cid] = Some(coerced);
                    } else {
                        if col.not_null && raw_ref.is_null() {
                            return Ok(None);
                        }
                        raw_refs[cid] = raw_ref;
                    }
                }
                for cid in 0..n_cols {
                    col_refs_mut[cid] = owned_vals[cid].as_ref().unwrap_or(raw_refs[cid]);
                }
                let col_refs: &[&SqlValue] = &*col_refs_mut;

                // Resolve rowid (must be strictly greater than existing max for monotonic append).
                let autoinc_seq_info: Option<Option<(usize, i64)>> =
                    if table.definition.autoincrement {
                        Some(self.get_sqlite_sequence_indexed(&table.name)?)
                    } else {
                        None
                    };
                let autoinc_seq_cache: Option<i64> =
                    autoinc_seq_info.map(|opt| opt.map_or(0, |(_, s)| s));
                let (known_max_rowid, mut hint_leaf) =
                    self.get_known_max_and_leaf(table.root_page);
                let mut rowid_guaranteed_new = false;
                let rid: i64 = if let Some(ref isrc) = ipk_src {
                    let cand: &SqlValue = match isrc {
                        FastValSrc::Const(v) => v,
                        FastValSrc::Param { index, text } => params.resolve_ref(*index, text),
                    };
                    if cand.is_null() {
                        let cur_max = match known_max_rowid {
                            Some(m) => m,
                            None => table_max_rowid(&self.pager, table.root_page)?,
                        };
                        let next = self.allocate_next_rowid_with_max(
                            table,
                            cur_max,
                            autoinc_seq_cache,
                        )?;
                        if cur_max.is_none_or(|m| next > m) {
                            rowid_guaranteed_new = true;
                        }
                        next
                    } else {
                        let Ok(r) = coerce_ipk_value(cand) else {
                            return Ok(None);
                        };
                        if let Some(cur_max) = known_max_rowid {
                            if cur_max.is_none_or(|m| r > m) {
                                rowid_guaranteed_new = true;
                            }
                        }
                        r
                    }
                } else {
                    let cur_max = match known_max_rowid {
                        Some(m) => m,
                        None => table_max_rowid(&self.pager, table.root_page)?,
                    };
                    let next = self.allocate_next_rowid_with_max(
                        table,
                        cur_max,
                        autoinc_seq_cache,
                    )?;
                    if cur_max.is_none_or(|m| next > m) {
                        rowid_guaranteed_new = true;
                    }
                    next
                };

                if !rowid_guaranteed_new {
                    return Ok(None);
                }

                let elide_sp = self.pager.in_transaction()
                    && self.pager.is_noop_meter()
                    && dml.simple_insert_elidable
                    && autoinc_seq_info.is_none_or(|opt| opt.is_some());

                let do_write = |conn: &mut Self| -> SqliteResult<ExecResult> {
                    if !conn.pager.is_noop_meter() {
                        conn.pager.meter().charge_steps(*n_params_steps)?;
                    }
                    let enc = conn.pager.encoding();
                    let fmt = conn.pager.schema_format();
                    encode_table_record_refs_into(
                        table,
                        &col_refs[..n_cols],
                        enc,
                        fmt,
                        &mut conn.record_scratch,
                    );
                    table_append_monotonic(
                        &mut conn.pager,
                        table.root_page,
                        &mut hint_leaf,
                        rid,
                        &conn.record_scratch,
                    )?;
                    let undo_partial_insert = |conn: &mut Self, count: usize| {
                        conn.invalidate_max_rowid_for(table.root_page);
                        for &(ix_idx, two_int_cid) in &sec_indexes[..count] {
                            let ix = &table.indexes[ix_idx];
                            if let Some(ki) = dml.index_key_infos[ix_idx].as_ref() {
                                if let Some(col_cid) = two_int_cid {
                                    if Some(col_cid) == table.ipk_column {
                                        let _ = index_delete_two_int_asc(
                                            &mut conn.pager,
                                            ix.root_page,
                                            rid,
                                            rid,
                                            ki,
                                        );
                                        continue;
                                    }
                                    let v0 = col_refs[col_cid];
                                    if let SqlValue::Int(k0) = *v0 {
                                        let _ = index_delete_two_int_asc(
                                            &mut conn.pager,
                                            ix.root_page,
                                            k0,
                                            rid,
                                            ki,
                                        );
                                        continue;
                                    }
                                    let kvals = [v0.clone(), SqlValue::Int(rid)];
                                    let _ = index_delete(&mut conn.pager, ix.root_page, &kvals, ki);
                                } else {
                                    let mut kvals = Vec::with_capacity(ix.columns.len());
                                    for ic in &ix.columns {
                                        if ic.cid == -1
                                            || Some(ic.cid as usize) == table.ipk_column
                                        {
                                            kvals.push(SqlValue::Int(rid));
                                        } else {
                                            kvals.push(col_refs[ic.cid as usize].clone());
                                        }
                                    }
                                    let _ = index_delete(&mut conn.pager, ix.root_page, &kvals, ki);
                                }
                            }
                        }
                        let _ = table_delete(&mut conn.pager, table.root_page, rid);
                    };
                    for (inserted_sec, &(ix_idx, two_int_cid)) in sec_indexes.iter().enumerate() {
                        let ix = &table.indexes[ix_idx];
                        let Some(ki) = dml.index_key_infos[ix_idx].as_ref() else {
                            if elide_sp {
                                undo_partial_insert(conn, inserted_sec);
                            }
                            return Err(SqliteError::corrupt(
                                "missing KeyInfo for secondary index",
                            ));
                        };
                        let ins_res = if let Some(col_cid) = two_int_cid {
                            if Some(col_cid) == table.ipk_column {
                                index_insert_two_int_asc(
                                    &mut conn.pager,
                                    ix.root_page,
                                    rid,
                                    rid,
                                    fmt,
                                    ki,
                                    false,
                                )
                            } else {
                                let v0 = col_refs[col_cid];
                                if let SqlValue::Int(k0) = *v0 {
                                    index_insert_two_int_asc(
                                        &mut conn.pager,
                                        ix.root_page,
                                        k0,
                                        rid,
                                        fmt,
                                        ki,
                                        false,
                                    )
                                } else {
                                    let rid_v = SqlValue::Int(rid);
                                    let refs = [v0, &rid_v];
                                    encode_record_refs_into(
                                        &refs,
                                        enc,
                                        fmt,
                                        &mut conn.record_scratch,
                                    );
                                    let kvals = [v0.clone(), rid_v];
                                    index_insert_with_values(
                                        &mut conn.pager,
                                        ix.root_page,
                                        &kvals,
                                        &conn.record_scratch,
                                        ki,
                                        false,
                                    )
                                }
                            }
                        } else if ix.columns.len() == 2
                            && ix.columns[0].cid >= 0
                            && ix.columns[1].cid == -1
                        {
                            let col_cid = ix.columns[0].cid as usize;
                            let rid_v = SqlValue::Int(rid);
                            let v0: &SqlValue = if Some(col_cid) == table.ipk_column {
                                &rid_v
                            } else {
                                col_refs[col_cid]
                            };
                            let refs = [v0, &rid_v];
                            encode_record_refs_into(&refs, enc, fmt, &mut conn.record_scratch);
                            let kvals = [v0.clone(), rid_v];
                            index_insert_with_values(
                                &mut conn.pager,
                                ix.root_page,
                                &kvals,
                                &conn.record_scratch,
                                ki,
                                false,
                            )
                        } else {
                            let mut kvals = Vec::with_capacity(ix.columns.len());
                            for ic in &ix.columns {
                                if ic.cid == -1 || Some(ic.cid as usize) == table.ipk_column {
                                    kvals.push(SqlValue::Int(rid));
                                } else {
                                    kvals.push(col_refs[ic.cid as usize].clone());
                                }
                            }
                            encode_record_with_format_into(
                                &kvals,
                                enc,
                                fmt,
                                &mut conn.record_scratch,
                            );
                            index_insert_with_values(
                                &mut conn.pager,
                                ix.root_page,
                                &kvals,
                                &conn.record_scratch,
                                ki,
                                false,
                            )
                        };
                        if let Err(e) = ins_res {
                            if elide_sp {
                                undo_partial_insert(conn, inserted_sec);
                            }
                            return Err(e);
                        }
                    }
                    if let Some(seq_opt) = autoinc_seq_info {
                        let seq_res = if let Some((seq_idx, _)) = seq_opt {
                            conn.update_sqlite_sequence_at(&table.name, seq_idx, rid.max(0))
                        } else {
                            conn.update_sqlite_sequence(&table.name, rid.max(0))
                        };
                        if let Err(e) = seq_res {
                            if elide_sp {
                                undo_partial_insert(conn, sec_indexes.len());
                            }
                            return Err(e);
                        }
                    }
                    conn.set_append_tail(table.root_page, rid, hint_leaf);
                    conn.last_insert_rowid = rid;
                    conn.changes = 1;
                    conn.total_changes += 1;
                    Ok(ExecResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        changes: 1,
                        last_insert_rowid: rid,
                    })
                };

                let res = if elide_sp {
                    self.run_mutating_stmt_no_savepoint(do_write)?
                } else {
                    self.run_mutating_stmt(do_write)?
                };
                Ok(Some(res))
            }
            CachedFastDml::PointUpdateSingleCol {
                dml,
                target_sidx,
                target_affinity,
                target_not_null,
                val_src,
                rid_src,
                ..
            } => {
                let target_rid = match rid_src {
                    FastKeySrc::Const(opt) => *opt,
                    FastKeySrc::Param { index, text } => {
                        match coerce_rowid_eq_lookup(params.resolve_ref(*index, text)) {
                            RowidEqLookup::Exact(r) => Some(r),
                            RowidEqLookup::Impossible => None,
                        }
                    }
                };
                let Some(rid) = target_rid else {
                    self.changes = 0;
                    return Ok(Some(ExecResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        changes: 0,
                        last_insert_rowid: self.last_insert_rowid,
                    }));
                };
                let raw_ref: &SqlValue = match val_src {
                    FastValSrc::Const(v) => v,
                    FastValSrc::Param { index, text } => params.resolve_ref(*index, text),
                };
                let owned_val = apply_affinity_if_changed(raw_ref, *target_affinity);
                let val_ref = owned_val.as_ref().unwrap_or(raw_ref);
                if *target_not_null && val_ref.is_null() {
                    return Ok(None);
                }
                let root_page = dml.table.root_page;
                let elide_sp = self.pager.in_transaction() && self.pager.is_noop_meter();
                let mut fell_back = false;
                let do_update = |conn: &mut Self| -> SqliteResult<ExecResult> {
                    if !conn.pager.is_noop_meter() {
                        conn.pager.meter().charge_steps(2)?;
                    }
                    conn.clear_append_leaf_only_for(root_page);
                    let mut leaf_hint = conn
                        .last_point_leaf
                        .and_then(|(r, lpg, lo, hi)| (r == root_page).then_some((lpg, lo, hi)));
                    let outcome = table_update_single_field_hinted(
                        &mut conn.pager,
                        root_page,
                        rid,
                        *target_sidx,
                        val_ref,
                        &mut conn.record_scratch,
                        &mut leaf_hint,
                    )?;
                    conn.last_point_leaf =
                        leaf_hint.map(|(lpg, lo, hi)| (root_page, lpg, lo, hi));
                    match outcome {
                        FastFieldUpdateOutcome::Updated => {
                            conn.changes = 1;
                            conn.total_changes += 1;
                            Ok(ExecResult {
                                columns: Vec::new(),
                                rows: Vec::new(),
                                changes: 1,
                                last_insert_rowid: conn.last_insert_rowid,
                            })
                        }
                        FastFieldUpdateOutcome::NotFound => {
                            conn.changes = 0;
                            Ok(ExecResult {
                                columns: Vec::new(),
                                rows: Vec::new(),
                                changes: 0,
                                last_insert_rowid: conn.last_insert_rowid,
                            })
                        }
                        FastFieldUpdateOutcome::Fallback => {
                            fell_back = true;
                            Ok(conn.empty_exec_result())
                        }
                    }
                };
                let res = if elide_sp {
                    self.run_mutating_stmt_no_savepoint(do_update)?
                } else {
                    self.run_mutating_stmt(do_update)?
                };
                if fell_back {
                    Ok(None)
                } else {
                    Ok(Some(res))
                }
            }
            CachedFastDml::PointDeleteRowid {
                dml,
                rid_src,
                indexes,
                ..
            } => {
                let target_rid = match rid_src {
                    FastKeySrc::Const(opt) => *opt,
                    FastKeySrc::Param { index, text } => {
                        match coerce_rowid_eq_lookup(params.resolve_ref(*index, text)) {
                            RowidEqLookup::Exact(r) => Some(r),
                            RowidEqLookup::Impossible => None,
                        }
                    }
                };
                let Some(rid) = target_rid else {
                    self.changes = 0;
                    return Ok(Some(ExecResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        changes: 0,
                        last_insert_rowid: self.last_insert_rowid,
                    }));
                };
                let table = &dml.table;
                let elide_sp = self.pager.in_transaction() && self.pager.is_noop_meter();
                let do_delete = |conn: &mut Self| -> SqliteResult<ExecResult> {
                    if !conn.pager.is_noop_meter() {
                        conn.pager.meter().charge_steps(2)?;
                    }
                    let enc = conn.pager.encoding();
                    let fmt = conn.pager.schema_format();
                    let want = &dml.simple_delete_want_fields;
                    let mut stack_dec = [const { SqlValue::Null }; 8];
                    let mut heap_dec: Vec<SqlValue> = Vec::new();
                    let mut leaf_hint = conn
                        .last_point_leaf
                        .and_then(|(r, lpg, lo, hi)| (r == table.root_page).then_some((lpg, lo, hi)));
                    let located = if want.len() <= 8 {
                        table_locate_and_read_sparse_fields_into_hinted(
                            &conn.pager,
                            table.root_page,
                            rid,
                            enc,
                            want,
                            &mut stack_dec[..want.len()],
                            &mut leaf_hint,
                        )?
                    } else {
                        heap_dec.resize(want.len(), SqlValue::Null);
                        table_locate_and_read_sparse_fields_into_hinted(
                            &conn.pager,
                            table.root_page,
                            rid,
                            enc,
                            want,
                            &mut heap_dec,
                            &mut leaf_hint,
                        )?
                    };
                    if let Some(loc) = located {
                        let post_hint = loc.post_delete_hint;
                        let n_fields = loc.n_fields;
                        let decoded: &[SqlValue] = if want.len() <= 8 {
                            &stack_dec[..want.len()]
                        } else {
                            &heap_dec
                        };
                        let undo_deleted_indexes = |conn: &mut Self, count: usize| {
                            for ix_plan in &indexes[..count] {
                                match ix_plan {
                                    FastDeleteIndex::TwoIntAsc {
                                        root_page,
                                        want_slot,
                                        storage_idx,
                                        default_val,
                                        real_back,
                                        key_info,
                                    } => {
                                        if let Some(slot) = *want_slot {
                                            let raw_ref: &SqlValue = if *storage_idx < n_fields {
                                                &decoded[slot]
                                            } else {
                                                default_val
                                            };
                                            if !*real_back {
                                                if let SqlValue::Int(k0) = *raw_ref {
                                                    let _ = index_insert_two_int_asc(
                                                        &mut conn.pager,
                                                        *root_page,
                                                        k0,
                                                        rid,
                                                        fmt,
                                                        key_info,
                                                        false,
                                                    );
                                                    continue;
                                                }
                                            }
                                            let val = if *real_back {
                                                crate::schema::real_read_back(raw_ref.clone())
                                            } else {
                                                raw_ref.clone()
                                            };
                                            let kvals = [val, SqlValue::Int(rid)];
                                            encode_record_with_format_into(
                                                &kvals,
                                                enc,
                                                fmt,
                                                &mut conn.record_scratch,
                                            );
                                            let _ = index_insert_with_values(
                                                &mut conn.pager,
                                                *root_page,
                                                &kvals,
                                                &conn.record_scratch,
                                                key_info,
                                                false,
                                            );
                                        } else {
                                            let _ = index_insert_two_int_asc(
                                                &mut conn.pager,
                                                *root_page,
                                                rid,
                                                rid,
                                                fmt,
                                                key_info,
                                                false,
                                            );
                                        }
                                    }
                                    FastDeleteIndex::General { ix_idx } => {
                                        let ix = &table.indexes[*ix_idx];
                                        if let Some(ki) = dml.index_key_infos[*ix_idx].as_ref() {
                                            let mut kvals = Vec::with_capacity(ix.columns.len());
                                            for ic in &ix.columns {
                                                if ic.cid == -1
                                                    || Some(ic.cid as usize) == table.ipk_column
                                                {
                                                    kvals.push(SqlValue::Int(rid));
                                                } else {
                                                    let cid = ic.cid as usize;
                                                    let val = if let Some(slot) = dml
                                                        .simple_delete_want_cids
                                                        .iter()
                                                        .position(|&c| c == cid)
                                                    {
                                                        let sidx =
                                                            dml.simple_delete_want_fields[slot];
                                                        let raw = if sidx < n_fields {
                                                            decoded[slot].clone()
                                                        } else {
                                                            table.columns[cid].default_value.clone()
                                                        };
                                                        if table.columns[cid].affinity
                                                            == Affinity::Real
                                                        {
                                                            crate::schema::real_read_back(raw)
                                                        } else {
                                                            raw
                                                        }
                                                    } else {
                                                        SqlValue::Null
                                                    };
                                                    kvals.push(val);
                                                }
                                            }
                                            encode_record_with_format_into(
                                                &kvals,
                                                enc,
                                                fmt,
                                                &mut conn.record_scratch,
                                            );
                                            let _ = index_insert_with_values(
                                                &mut conn.pager,
                                                ix.root_page,
                                                &kvals,
                                                &conn.record_scratch,
                                                ki,
                                                false,
                                            );
                                        }
                                    }
                                }
                            }
                        };
                        // Delete secondary indexes FIRST while the table row is still intact so
                        // any corrupt index page error leaves the table untouched (and we cleanly
                        // restore any earlier index entries).
                        for (del_pos, ix_plan) in indexes.iter().enumerate() {
                            let del_res = match ix_plan {
                                FastDeleteIndex::TwoIntAsc {
                                    root_page,
                                    want_slot,
                                    storage_idx,
                                    default_val,
                                    real_back,
                                    key_info,
                                } => {
                                    if let Some(slot) = *want_slot {
                                        let raw_ref: &SqlValue = if *storage_idx < n_fields {
                                            &decoded[slot]
                                        } else {
                                            default_val
                                        };
                                        if !*real_back {
                                            if let SqlValue::Int(k0) = *raw_ref {
                                                index_delete_two_int_asc(
                                                    &mut conn.pager,
                                                    *root_page,
                                                    k0,
                                                    rid,
                                                    key_info,
                                                )
                                            } else {
                                                let kvals = [raw_ref.clone(), SqlValue::Int(rid)];
                                                index_delete(
                                                    &mut conn.pager,
                                                    *root_page,
                                                    &kvals,
                                                    key_info,
                                                )
                                            }
                                        } else {
                                            let val =
                                                crate::schema::real_read_back(raw_ref.clone());
                                            let kvals = [val, SqlValue::Int(rid)];
                                            index_delete(
                                                &mut conn.pager,
                                                *root_page,
                                                &kvals,
                                                key_info,
                                            )
                                        }
                                    } else {
                                        index_delete_two_int_asc(
                                            &mut conn.pager,
                                            *root_page,
                                            rid,
                                            rid,
                                            key_info,
                                        )
                                    }
                                }
                                FastDeleteIndex::General { ix_idx } => {
                                    let ix = &table.indexes[*ix_idx];
                                    let Some(ki) = dml.index_key_infos[*ix_idx].as_ref() else {
                                        if elide_sp {
                                            undo_deleted_indexes(conn, del_pos);
                                        }
                                        return Err(SqliteError::corrupt(
                                            "missing KeyInfo for secondary index",
                                        ));
                                    };
                                    let mut kvals = Vec::with_capacity(ix.columns.len());
                                    for ic in &ix.columns {
                                        if ic.cid == -1
                                            || Some(ic.cid as usize) == table.ipk_column
                                        {
                                            kvals.push(SqlValue::Int(rid));
                                        } else {
                                            let cid = ic.cid as usize;
                                            let val = if let Some(slot) = dml
                                                .simple_delete_want_cids
                                                .iter()
                                                .position(|&c| c == cid)
                                            {
                                                let sidx = dml.simple_delete_want_fields[slot];
                                                let raw = if sidx < n_fields {
                                                    decoded[slot].clone()
                                                } else {
                                                    table.columns[cid].default_value.clone()
                                                };
                                                if table.columns[cid].affinity == Affinity::Real {
                                                    crate::schema::real_read_back(raw)
                                                } else {
                                                    raw
                                                }
                                            } else {
                                                SqlValue::Null
                                            };
                                            kvals.push(val);
                                        }
                                    }
                                    index_delete(&mut conn.pager, ix.root_page, &kvals, ki)
                                }
                            };
                            if let Err(e) = del_res {
                                if elide_sp {
                                    undo_deleted_indexes(conn, del_pos);
                                }
                                return Err(e);
                            }
                        }
                        if let Err(e) = table_finish_delete_located(&mut conn.pager, loc) {
                            if elide_sp {
                                undo_deleted_indexes(conn, indexes.len());
                            }
                            return Err(e);
                        }
                        conn.invalidate_max_rowid_only_for(table.root_page);
                        conn.last_point_leaf =
                            post_hint.map(|(lpg, lo, hi)| (table.root_page, lpg, lo, hi));
                        conn.changes = 1;
                        conn.total_changes += 1;
                        return Ok(ExecResult {
                            columns: Vec::new(),
                            rows: Vec::new(),
                            changes: 1,
                            last_insert_rowid: conn.last_insert_rowid,
                        });
                    }
                    conn.last_point_leaf =
                        leaf_hint.map(|(lpg, lo, hi)| (table.root_page, lpg, lo, hi));
                    conn.changes = 0;
                    Ok(ExecResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        changes: 0,
                        last_insert_rowid: conn.last_insert_rowid,
                    })
                };
                let res = if elide_sp {
                    self.run_mutating_stmt_no_savepoint(do_delete)?
                } else {
                    self.run_mutating_stmt(do_delete)?
                };
                Ok(Some(res))
            }
        }
    }

    fn exec_one_stmt(
        &mut self,
        stmt: &Statement,
        full_sql: &str,
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        match stmt {
            Statement::Select(sel) => {
                self.flush_seq_cache()?;
                let view = self.warm_view()?;
                let opts = self.query_options(dialect);
                let res = self.eval_select_stmt(&view, sel, params, &opts)?;
                Ok(ExecResult {
                    columns: res.columns,
                    rows: res.rows,
                    changes: self.changes,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            Statement::Begin { .. } => {
                self.pager.begin()?;
                Ok(self.empty_exec_result())
            }
            Statement::Commit { .. } => {
                self.flush_seq_cache()?;
                self.invalidate_warm_view();
                self.pager.commit()?;
                Ok(self.empty_exec_result())
            }
            Statement::Rollback { savepoint, .. } => {
                self.invalidate_warm_view();
                self.cached_seq = None;
                self.seq_dirty = false;
                self.invalidate_max_rowid_cache();
                if let Some(sp) = savepoint {
                    self.pager.rollback_to_savepoint(&sp.name)?;
                } else {
                    self.pager.rollback()?;
                }
                Ok(self.empty_exec_result())
            }
            Statement::Savepoint { name, .. } => {
                self.flush_seq_cache()?;
                self.pager.push_savepoint(Some(name.name.clone()));
                Ok(self.empty_exec_result())
            }
            Statement::Release { name, .. } => {
                self.flush_seq_cache()?;
                self.invalidate_warm_view();
                self.pager.release_savepoint(&name.name)?;
                Ok(self.empty_exec_result())
            }
            Statement::Pragma(p) => {
                self.flush_seq_cache()?;
                self.invalidate_warm_view();
                self.exec_pragma(p)
            }
            Statement::CreateTable(ct) => self.run_mutating_stmt(|conn| {
                conn.flush_seq_cache()?;
                conn.exec_create_table(ct, full_sql, raw_sql, params, dialect)
            }),
            Statement::CreateIndex(ci) => self.run_mutating_stmt(|conn| {
                conn.flush_seq_cache()?;
                conn.exec_create_index(ci, raw_sql)
            }),
            Statement::CreateView(cv) => self.run_mutating_stmt(|conn| {
                conn.flush_seq_cache()?;
                conn.exec_create_view(cv, raw_sql)
            }),
            Statement::Drop(dr) => self.run_mutating_stmt(|conn| {
                conn.flush_seq_cache()?;
                conn.cached_seq = None;
                conn.seq_dirty = false;
                conn.invalidate_max_rowid_cache();
                conn.exec_drop(dr)
            }),
            Statement::AlterTable(al) => self.run_mutating_stmt(|conn| {
                conn.flush_seq_cache()?;
                conn.cached_seq = None;
                conn.seq_dirty = false;
                conn.invalidate_max_rowid_cache();
                conn.exec_alter_table(al, full_sql)
            }),
            Statement::Insert(ins) => {
                if self.can_elide_insert_savepoint(ins) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_insert(ins, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_insert(ins, params, dialect))
                }
            }
            Statement::Update(upd) => {
                if self.can_elide_update_savepoint(upd) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_update(upd, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_update(upd, params, dialect))
                }
            }
            Statement::Delete(del) => {
                if self.can_elide_delete_savepoint(del) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_delete(del, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_delete(del, params, dialect))
                }
            }
            Statement::Vacuum { into, .. } => {
                if self.pager.in_transaction() {
                    return Err(SqliteError::Sql(
                        "cannot VACUUM from within a transaction".to_string(),
                    ));
                }
                if into.is_some() {
                    return Err(SqliteError::Sql(
                        "VACUUM INTO is not yet supported".to_string(),
                    ));
                }
                self.exec_vacuum()
            }
            Statement::Analyze { .. } | Statement::Reindex { .. } => {
                Ok(self.empty_exec_result())
            }
            other => Err(SqliteError::Sql(format!(
                "unsupported statement in Phase 3 engine: {other:?}"
            ))),
        }
    }

    fn can_elide_insert_savepoint(&self, ins: &InsertStmt) -> bool {
        if !self.pager.in_transaction() || !self.pager.is_noop_meter() {
            return false;
        }
        if ins.replace_into
            || ins.or_conflict == Some(ConflictAction::Replace)
            || !ins.upsert.is_empty()
            || ins.with.is_some()
            || ins.returning.is_some()
            || ins.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
        {
            return false;
        }
        let single_row = match &ins.select {
            None => true, // DEFAULT VALUES (single row)
            Some(sel) => {
                sel.with.is_none()
                    && sel.order_by.is_none()
                    && sel.order_by_all.is_none()
                    && sel.limit.is_none()
                    && matches!(&sel.body, SelectBody::Values(v) if v.rows.len() <= 1)
            }
        };
        if !single_row {
            return false;
        }
        if let Some(dml) = self.cached_dml_for(&ins.table.name.name) {
            return dml.simple_insert_elidable
                && dml.table.indexes.is_empty()
                && !dml.table.definition.autoincrement;
        }
        let Ok(schema) = self.schema_arc() else {
            return false;
        };
        let Some(tbl) = schema.table(&ins.table.name.name) else {
            return false;
        };
        if tbl.without_rowid
            || tbl.has_virtual_columns
            || tbl.definition.ipk_on_error == OnError::Replace
            || !tbl.indexes.is_empty()
            || tbl.definition.autoincrement
        {
            return false;
        }
        true
    }

    fn can_elide_delete_savepoint(&self, del: &DeleteStmt) -> bool {
        if !self.pager.in_transaction() || !self.pager.is_noop_meter() {
            return false;
        }
        if del.returning.is_some()
            || del.with.is_some()
            || del.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
        {
            return false;
        }
        let alias = del
            .table
            .alias
            .as_ref()
            .unwrap_or(&del.table.name)
            .name
            .as_str();
        if let Some(dml) = self.cached_dml_for(&del.table.name.name) {
            return dml.simple_delete_elidable
                && dml.table.indexes.is_empty()
                && del
                    .where_clause
                    .as_ref()
                    .is_some_and(|we| has_direct_rowid_eq_conjunct(we, &dml.table, alias));
        }
        let Ok(schema) = self.schema_arc() else {
            return false;
        };
        let Some(tbl) = schema.table(&del.table.name.name) else {
            return false;
        };
        if tbl.without_rowid || tbl.has_virtual_columns || !tbl.indexes.is_empty() {
            return false;
        }
        del.where_clause
            .as_ref()
            .is_some_and(|we| has_direct_rowid_eq_conjunct(we, tbl, alias))
    }

    fn can_elide_update_savepoint(&self, upd: &UpdateStmt) -> bool {
        if !self.pager.in_transaction() || !self.pager.is_noop_meter() {
            return false;
        }
        if upd.from.is_some()
            || upd.with.is_some()
            || upd.or_conflict == Some(ConflictAction::Replace)
            || upd.returning.is_some()
            || upd.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
        {
            return false;
        }
        // Check cached DML state first (zero Mutex lock / SchemaCatalog lookup on hot UPDATE loops).
        let schema_opt;
        let tbl: &TableInfo = if let Some(dml) = self.cached_dml_for(&upd.table.name.name) {
            &dml.table
        } else {
            let Ok(schema) = self.schema_arc() else {
                return false;
            };
            schema_opt = schema;
            let Some(t) = schema_opt.table(&upd.table.name.name) else {
                return false;
            };
            t
        };
        if tbl.without_rowid || tbl.has_virtual_columns {
            return false;
        }
        if tbl.columns.iter().any(|c| c.generated.is_some()) {
            return false;
        }
        // Check every modified column is NOT the IPK and NOT referenced by any index.
        let mut modified_cids: Vec<usize> = Vec::new();
        for sc in &upd.set {
            for c in &sc.columns {
                if let Some(pos) = tbl
                    .columns
                    .iter()
                    .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                {
                    if Some(pos) == tbl.ipk_column {
                        return false;
                    }
                    modified_cids.push(pos);
                } else {
                    return false;
                }
            }
        }
        for ix in &tbl.indexes {
            if ix.definition.where_clause.is_some() || index_needs_expr_scope(ix) {
                return false;
            }
            if ix.columns[..ix.key_count]
                .iter()
                .any(|ic| ic.cid >= 0 && modified_cids.contains(&(ic.cid as usize)))
            {
                return false;
            }
        }
        // Safe for single-row WHERE (e.g. `WHERE id = ?`); multi-row UPDATE could fail CHECK/NOT NULL
        // on row 2 after writing row 1, so require a simple rowid/IPK equality conjunct!
        let alias = upd
            .table
            .alias
            .as_ref()
            .unwrap_or(&upd.table.name)
            .name
            .as_str();
        upd.where_clause
            .as_ref()
            .is_some_and(|we| has_direct_rowid_eq_conjunct(we, tbl, alias))
    }

    /// Execute a single-row validated mutating statement inside an explicit transaction without
    /// pushing an extra statement savepoint (eliminates per-statement 4 KiB COW page copies!).
    fn run_mutating_stmt_no_savepoint<F>(&mut self, f: F) -> SqliteResult<ExecResult>
    where
        F: FnOnce(&mut Self) -> SqliteResult<ExecResult>,
    {
        self.invalidate_warm_view();
        self.pending_error_conflict = None;
        match f(self) {
            Ok(res) => Ok(res),
            Err(e) => {
                self.invalidate_max_rowid_cache();
                if self.pending_error_conflict.take() == Some(ConflictAction::Rollback)
                    && self.pager.in_transaction()
                {
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    let _ = self.pager.rollback();
                } else {
                    let _ = self.flush_seq_cache();
                    self.cached_seq = None;
                }
                Err(e)
            }
        }
    }

    fn empty_exec_result(&self) -> ExecResult {
        ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        }
    }

    /// Execute a mutating statement inside an automatic statement savepoint with proper
    /// `ON CONFLICT ABORT`/`FAIL`/`ROLLBACK` and auto-commit handling.
    fn run_mutating_stmt<F>(&mut self, f: F) -> SqliteResult<ExecResult>
    where
        F: FnOnce(&mut Self) -> SqliteResult<ExecResult>,
    {
        self.flush_seq_cache()?;
        self.invalidate_warm_view();
        self.pending_error_conflict = None;
        let prev_changes = self.changes;
        let prev_total_changes = self.total_changes;
        let prev_last_insert_rowid = self.last_insert_rowid;
        self.pager.push_savepoint(None);
        match f(self) {
            Ok(res) => {
                if let Err(e) = self.pager.commit_statement_and_autocommit() {
                    self.invalidate_warm_view();
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    self.invalidate_max_rowid_cache();
                    self.changes = prev_changes;
                    self.total_changes = prev_total_changes;
                    self.last_insert_rowid = prev_last_insert_rowid;
                    return Err(e);
                }
                Ok(res)
            }
            Err(e) => {
                self.invalidate_warm_view();
                self.cached_seq = None;
                self.seq_dirty = false;
                self.invalidate_max_rowid_cache();
                let eff = self.pending_error_conflict.take();
                match eff {
                    Some(ConflictAction::Rollback) if self.pager.in_transaction() => {
                        let _ = self.pager.rollback();
                    }
                    Some(ConflictAction::Fail) => {
                        if self.pager.commit_statement_and_autocommit().is_err() {
                            self.changes = prev_changes;
                            self.total_changes = prev_total_changes;
                            self.last_insert_rowid = prev_last_insert_rowid;
                        }
                    }
                    _ => {
                        self.pager.rollback_statement_savepoint();
                    }
                }
                Err(e)
            }
        }
    }

    // ───────────────────────────── sqlite_schema helpers ─────────────────────────────

    fn insert_schema_row(
        &mut self,
        obj_type: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: Option<&str>,
    ) -> SqliteResult<i64> {
        self.pager.ensure_initialized()?;
        let next_rid = table_max_rowid(&self.pager, 1)?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| SqliteError::Sql("database or disk is full".into()))?;
        let vals = vec![
            SqlValue::text(obj_type),
            SqlValue::text(name),
            SqlValue::text(tbl_name),
            SqlValue::Int(i64::from(root_page)),
            sql.map_or(SqlValue::Null, SqlValue::text),
        ];
        let payload = encode_record_with_format(
            &vals,
            self.pager.encoding(),
            self.pager.schema_format(),
        );
        table_insert(&mut self.pager, 1, next_rid, &payload, false)?;
        self.pager.invalidate_schema_cache();
        Ok(next_rid)
    }

    fn update_schema_row(
        &mut self,
        rowid: i64,
        obj_type: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: Option<&str>,
    ) -> SqliteResult<()> {
        let vals = vec![
            SqlValue::text(obj_type),
            SqlValue::text(name),
            SqlValue::text(tbl_name),
            SqlValue::Int(i64::from(root_page)),
            sql.map_or(SqlValue::Null, SqlValue::text),
        ];
        let payload = encode_record_with_format(
            &vals,
            self.pager.encoding(),
            self.pager.schema_format(),
        );
        table_insert(&mut self.pager, 1, rowid, &payload, true)?;
        self.pager.invalidate_schema_cache();
        Ok(())
    }

    fn flush_seq_cache(&mut self) -> SqliteResult<()> {
        if !self.seq_dirty {
            return Ok(());
        }
        let Some((seq_root, mut list)) = self.cached_seq.take() else {
            self.seq_dirty = false;
            return Ok(());
        };
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let flush_res: SqliteResult<()> = (|| {
            for idx in 0..list.len() {
                if !list[idx].5 {
                    continue;
                }
                let rid = list[idx].2;
                let clamped = list[idx].3;
                let mut loc = list[idx].4;
                let (outcome, repacked) = table_update_int_field_cached(
                    &mut self.pager,
                    seq_root,
                    rid,
                    1,
                    clamped,
                    &mut loc,
                    &mut self.record_scratch,
                )?;
                if matches!(outcome, FastFieldUpdateOutcome::Updated) {
                    if repacked {
                        for entry in &mut list {
                            entry.4 = None;
                        }
                    }
                    list[idx].4 = loc;
                    list[idx].5 = false;
                } else {
                    let seq_val = SqlValue::Int(clamped);
                    let refs = [&list[idx].1, &seq_val];
                    encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
                    table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, true)?;
                    for entry in &mut list {
                        entry.4 = None;
                    }
                    list[idx].5 = false;
                }
            }
            Ok(())
        })();
        self.seq_dirty = list.iter().any(|e| e.5);
        self.cached_seq = Some((seq_root, list));
        flush_res
    }

    fn cow_overlay_with_flushed_seq(&self) -> Option<WritePager> {
        if !self.seq_dirty {
            return None;
        }
        let (seq_root, ref list) = *self.cached_seq.as_ref()?;
        let mut ov = self.pager.clone_cow_overlay();
        let enc = ov.encoding();
        let fmt = ov.schema_format();
        let mut scratch = Vec::with_capacity(64);
        for (_, name_val, rid, clamped, loc_init, dirty) in list {
            if !*dirty {
                continue;
            }
            let mut loc = *loc_init;
            let ok = matches!(
                table_update_int_field_cached(&mut ov, seq_root, *rid, 1, *clamped, &mut loc, &mut scratch),
                Ok((FastFieldUpdateOutcome::Updated, _))
            );
            if !ok {
                let seq_val = SqlValue::Int(*clamped);
                let refs = [name_val, &seq_val];
                encode_record_refs_into(&refs, enc, fmt, &mut scratch);
                let _ = table_insert(&mut ov, seq_root, *rid, &scratch, true);
            }
        }
        Some(ov)
    }

    fn ensure_sqlite_sequence_cache(&mut self) -> SqliteResult<Option<u32>> {
        if let Some((root, _)) = self.cached_seq {
            return Ok(Some(root));
        }
        let schema = self.schema_arc()?;
        let Some(seq_tbl) = schema.table("sqlite_sequence") else {
            return Ok(None);
        };
        let seq_root = seq_tbl.root_page;
        let view = self.live_view()?;
        let reader = view.table(&seq_tbl.name)?;
        let opts = ScanOptions {
            columns: None,
            rowid: false,
        };
        let mut cur = reader.cursor(Some(&opts))?;
        let mut list: Vec<SeqRowEntry> = Vec::new();
        while cur.next()? {
            let rid = cur.rowid()?;
            let r = cur.row()?;
            if let Some(name) = r.first().and_then(SqlValue::as_str) {
                if !list.iter().any(|(n, _, _, _, _, _)| n.eq_ignore_ascii_case(name)) {
                    let seq = r.get(1).map_or(0, int_value);
                    list.push((name.to_string(), SqlValue::text(name), rid, seq, None, false));
                }
            }
        }
        drop(cur);
        drop(reader);
        drop(view);
        self.cached_seq = Some((seq_root, list));
        Ok(Some(seq_root))
    }

    fn ensure_sqlite_sequence(&mut self) -> SqliteResult<u32> {
        if let Some(root) = self.ensure_sqlite_sequence_cache()? {
            return Ok(root);
        }
        let root = create_btree(&mut self.pager, BtreePageType::TableLeaf)?;
        self.insert_schema_row(
            "table",
            "sqlite_sequence",
            "sqlite_sequence",
            root,
            Some("CREATE TABLE sqlite_sequence(name,seq)"),
        )?;
        self.cached_seq = Some((root, Vec::new()));
        Ok(root)
    }

    fn get_sqlite_sequence(&mut self, table_name: &str) -> SqliteResult<Option<(i64, i64)>> {
        if self.ensure_sqlite_sequence_cache()?.is_none() {
            return Ok(None);
        }
        Ok(self.cached_seq.as_ref().and_then(|(_, list)| {
            list.iter()
                .find(|(n, _, _, _, _, _)| n.eq_ignore_ascii_case(table_name))
                .map(|(_, _, rid, seq, _, _)| (*rid, *seq))
        }))
    }

    fn get_sqlite_sequence_indexed(
        &mut self,
        table_name: &str,
    ) -> SqliteResult<Option<(usize, i64)>> {
        if self.ensure_sqlite_sequence_cache()?.is_none() {
            return Ok(None);
        }
        Ok(self.cached_seq.as_ref().and_then(|(_, list)| {
            list.iter()
                .enumerate()
                .find(|(_, (n, _, _, _, _, _))| n.eq_ignore_ascii_case(table_name))
                .map(|(idx, (_, _, _, seq, _, _))| (idx, *seq))
        }))
    }

    fn update_sqlite_sequence_at(
        &mut self,
        table_name: &str,
        idx: usize,
        new_seq: i64,
    ) -> SqliteResult<()> {
        let clamped = new_seq.max(0);
        let Some((seq_root, rid, old_seq, mut loc)) =
            self.cached_seq.as_ref().and_then(|(root, list)| {
                list.get(idx)
                    .map(|(_, _, rid, seq, loc, _)| (*root, *rid, *seq, *loc))
            })
        else {
            return self.update_sqlite_sequence(table_name, new_seq);
        };
        if clamped <= old_seq {
            return Ok(());
        }
        // Inside an explicit transaction with no nested savepoints and noop meter, defer
        // physical `sqlite_sequence` page writes until COMMIT / SAVEPOINT / DDL / read overlay.
        if self.pager.in_transaction() && !self.pager.has_savepoints() && self.pager.is_noop_meter()
        {
            if let Some((_, ref mut list)) = self.cached_seq {
                list[idx].3 = clamped;
                list[idx].5 = true;
            }
            self.seq_dirty = true;
            return Ok(());
        }
        let (outcome, repacked) = table_update_int_field_cached(
            &mut self.pager,
            seq_root,
            rid,
            1,
            clamped,
            &mut loc,
            &mut self.record_scratch,
        )?;
        if matches!(outcome, FastFieldUpdateOutcome::Updated) {
            if let Some((_, ref mut list)) = self.cached_seq {
                if repacked {
                    for entry in list.iter_mut() {
                        entry.4 = None;
                    }
                }
                list[idx].3 = clamped;
                list[idx].4 = loc;
                list[idx].5 = false;
            }
            return Ok(());
        }
        self.invalidate_max_rowid_for(seq_root);
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let seq_val = SqlValue::Int(clamped);
        if let Some((_, ref list)) = self.cached_seq {
            let refs = [&list[idx].1, &seq_val];
            encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
        }
        table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, true)?;
        if let Some((_, ref mut list)) = self.cached_seq {
            for entry in list.iter_mut() {
                entry.4 = None;
            }
            list[idx].3 = clamped;
            list[idx].5 = false;
        }
        Ok(())
    }

    fn update_sqlite_sequence(&mut self, table_name: &str, new_seq: i64) -> SqliteResult<()> {
        let clamped = new_seq.max(0);
        let seq_root = self.ensure_sqlite_sequence()?;
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let existing = self.cached_seq.as_ref().and_then(|(_, list)| {
            list.iter()
                .enumerate()
                .find(|(_, (n, _, _, _, _, _))| n.eq_ignore_ascii_case(table_name))
                .map(|(idx, (_, _, rid, seq, loc, _))| (idx, *rid, *seq, *loc))
        });
        if let Some((idx, rid, old_seq, mut loc)) = existing {
            if clamped > old_seq {
                if self.pager.in_transaction()
                    && !self.pager.has_savepoints()
                    && self.pager.is_noop_meter()
                {
                    if let Some((_, ref mut list)) = self.cached_seq {
                        list[idx].3 = clamped;
                        list[idx].5 = true;
                    }
                    self.seq_dirty = true;
                    return Ok(());
                }
                let (outcome, repacked) = table_update_int_field_cached(
                    &mut self.pager,
                    seq_root,
                    rid,
                    1,
                    clamped,
                    &mut loc,
                    &mut self.record_scratch,
                )?;
                if matches!(outcome, FastFieldUpdateOutcome::Updated) {
                    if let Some((_, ref mut list)) = self.cached_seq {
                        if repacked {
                            for entry in list.iter_mut() {
                                entry.4 = None;
                            }
                        }
                        list[idx].3 = clamped;
                        list[idx].4 = loc;
                        list[idx].5 = false;
                    }
                    return Ok(());
                }
                self.invalidate_max_rowid_for(seq_root);
                let seq_val = SqlValue::Int(clamped);
                if let Some((_, ref list)) = self.cached_seq {
                    let refs = [&list[idx].1, &seq_val];
                    encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
                }
                table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, true)?;
                if let Some((_, ref mut list)) = self.cached_seq {
                    for entry in list.iter_mut() {
                        entry.4 = None;
                    }
                    list[idx].3 = clamped;
                    list[idx].5 = false;
                }
            }
        } else {
            self.invalidate_max_rowid_for(seq_root);
            let rid = table_max_rowid(&self.pager, seq_root)?
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".into()))?;
            let name_val = SqlValue::text(table_name);
            let seq_val = SqlValue::Int(clamped);
            let refs = [&name_val, &seq_val];
            encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
            table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, false)?;
            if let Some((_, ref mut list)) = self.cached_seq {
                for entry in list.iter_mut() {
                    entry.4 = None;
                }
                list.push((table_name.to_string(), name_val, rid, clamped, None, false));
            }
        }
        Ok(())
    }

    // ───────────────────────────── VACUUM ─────────────────────────────

    fn exec_vacuum(&mut self) -> SqliteResult<ExecResult> {
        self.flush_seq_cache()?;
        if self.pager.page_count() == 0 {
            if let Some(ps) = self.pager.pending_page_size() {
                self.pager.set_page_size(ps)?;
            }
            return Ok(self.empty_exec_result());
        }

        let target_ps = self
            .pager
            .pending_page_size()
            .unwrap_or_else(|| self.pager.page_size());
        let enc = self.pager.encoding();
        let sf = self.pager.schema_format();

        let mut tmp = SqliteConnection::open_in_memory(ConnectionOptions {
            page_size: target_ps,
            dialect: self.dialect,
            meter: Some(Arc::clone(self.pager.meter())),
        })?;
        tmp.pager.set_encoding(enc);
        tmp.pager.set_schema_format(sf);
        tmp.cached_collations = self.cached_collations.clone();
        tmp.cached_functions = self.cached_functions.clone();
        tmp.foreign_keys = false;
        tmp.pager.ensure_initialized()?;

        let view = self.live_view()?;
        let mut table_ddls: Vec<String> = Vec::new();
        let mut index_ddls: Vec<String> = Vec::new();
        let mut view_ddls: Vec<String> = Vec::new();
        let mut trigger_rows: Vec<(String, String, String)> = Vec::new();

        for entry in &view.schema().entries {
            let Some(ref otype) = entry.obj_type else {
                continue;
            };
            let Some(ref sql) = entry.sql else {
                continue;
            };
            let name = entry.name.as_deref().unwrap_or("");
            let tbl_name = entry.table_name.as_deref().unwrap_or(name);
            match otype.as_str() {
                "table" => {
                    if !name.eq_ignore_ascii_case("sqlite_sequence")
                        && !name.eq_ignore_ascii_case("sqlite_master")
                        && !name.eq_ignore_ascii_case("sqlite_schema")
                    {
                        table_ddls.push(sql.clone());
                    }
                }
                "index" => index_ddls.push(sql.clone()),
                "view" => view_ddls.push(sql.clone()),
                "trigger" => {
                    trigger_rows.push((name.to_string(), tbl_name.to_string(), sql.clone()));
                }
                _ => {}
            }
        }

        // 1. Recreate all user tables cleanly (creating fresh root pages & autoindexes).
        for ddl in &table_ddls {
            tmp.execute(ddl)?;
        }

        // 2. Copy all user table rows compactly into `tmp`, preserving exact rowids & WITHOUT ROWID PKs.
        let qopts = tmp.query_options(self.dialect);
        for src_tbl in &view.schema().tables {
            if src_tbl.system || src_tbl.name.eq_ignore_ascii_case("sqlite_sequence") {
                continue;
            }
            let mut rows_mem = ConnMemGuard::new(&self.pager);
            let rows = self.read_all_table_rows(&view, src_tbl, &mut rows_mem)?;
            let dst_tbl = {
                let tv = tmp.live_view()?;
                tv.schema()
                    .table(&src_tbl.name)
                    .cloned()
                    .ok_or_else(|| {
                        SqliteError::Sql(format!("VACUUM missing recreated table {}", src_tbl.name))
                    })?
            };
            if !dst_tbl.without_rowid {
                let mut hint_leaf: Option<u32> = None;
                let mut rec_vals: Vec<SqlValue> =
                    Vec::with_capacity(dst_tbl.definition.record_columns.len());
                for (rid_opt, full_row) in &rows {
                    let rid = rid_opt.ok_or_else(|| {
                        SqliteError::corrupt(format!("table {} row missing rowid", dst_tbl.name))
                    })?;
                    rec_vals.clear();
                    for &cid in &dst_tbl.definition.record_columns {
                        if Some(cid) == dst_tbl.ipk_column {
                            rec_vals.push(SqlValue::Null);
                        } else {
                            rec_vals.push(full_row[cid].clone());
                        }
                    }
                    encode_record_with_format_into(&rec_vals, enc, sf, &mut tmp.record_scratch);
                    table_append_monotonic(
                        &mut tmp.pager,
                        dst_tbl.root_page,
                        &mut hint_leaf,
                        rid,
                        &tmp.record_scratch,
                    )?;
                }
            } else {
                let pk_idx = dst_tbl
                    .indexes
                    .iter()
                    .find(|ix| ix.without_rowid_primary_key)
                    .ok_or_else(|| {
                        SqliteError::corrupt(format!(
                            "WITHOUT ROWID table {} missing primary key index",
                            dst_tbl.name
                        ))
                    })?;
                let ki = key_info_for_index(pk_idx, enc, Some(&tmp.cached_collations.1))?;
                let mut wr_entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                for (_, full_row) in &rows {
                    let mut rec = Vec::with_capacity(pk_idx.columns.len());
                    for ic in &pk_idx.columns {
                        rec.push(full_row[ic.cid as usize].clone());
                    }
                    wr_entries.push(rec);
                }
                wr_entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                let payloads: Vec<Vec<u8>> = wr_entries
                    .iter()
                    .map(|e| encode_record_with_format(e, enc, sf))
                    .collect();
                index_bulk_load_sorted(&mut tmp.pager, dst_tbl.root_page, &payloads)?;
            }

            // Populate any `sqlite_autoindex_*` B-trees created alongside the table.
            let expr_scope_cols = build_plain_scope_columns(&dst_tbl);
            let where_scope_cols = build_table_scope_columns(&dst_tbl, &dst_tbl.name);
            for ix in &dst_tbl.indexes {
                if ix.without_rowid_primary_key {
                    continue;
                }
                let ki = key_info_for_index(ix, enc, Some(&tmp.cached_collations.1))?;
                let mut idx_entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                for (rid_opt, full_row) in &rows {
                    if let Some(entry_vals) = build_index_entry_values_fast(
                        &dst_tbl,
                        ix,
                        *rid_opt,
                        full_row,
                        &qopts,
                        &expr_scope_cols,
                        &where_scope_cols,
                    )? {
                        idx_entries.push(entry_vals);
                    }
                }
                idx_entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                let payloads: Vec<Vec<u8>> = idx_entries
                    .iter()
                    .map(|e| encode_record_with_format(e, enc, sf))
                    .collect();
                index_bulk_load_sorted(&mut tmp.pager, ix.root_page, &payloads)?;
            }
        }

        // 3. Preserve `sqlite_sequence` rows if present.
        if let Some(src_seq) = view.schema().table("sqlite_sequence") {
            let seq_root = tmp.ensure_sqlite_sequence()?;
            let mut seq_mem = ConnMemGuard::new(&self.pager);
            let seq_rows = self.read_all_table_rows(&view, src_seq, &mut seq_mem)?;
            let mut hint_leaf: Option<u32> = None;
            for (rid_opt, full_row) in &seq_rows {
                let rid = rid_opt.unwrap_or(1);
                encode_record_with_format_into(full_row, enc, sf, &mut tmp.record_scratch);
                table_append_monotonic(
                    &mut tmp.pager,
                    seq_root,
                    &mut hint_leaf,
                    rid,
                    &tmp.record_scratch,
                )?;
            }
        }

        // 4. Rebuild explicit indexes, views, and preserve trigger definitions.
        for ddl in &index_ddls {
            tmp.execute(ddl)?;
        }
        for ddl in &view_ddls {
            tmp.execute(ddl)?;
        }
        for (tname, tbl_name, tsql) in &trigger_rows {
            tmp.insert_schema_row("trigger", tname, tbl_name, 0, Some(tsql))?;
        }

        drop(view);
        self.pager.replace_from_vacuum(tmp.pager)?;
        self.invalidate_warm_view();
        if let Ok(mut guard) = self.cached_schema.lock() {
            *guard = None;
        }
        self.cached_dml.1.clear();
        self.cached_seq = None;
        self.seq_dirty = false;
        self.invalidate_max_rowid_cache();
        if let Ok(mut guard) = self.select_cache.lock() {
            guard.clear();
        }
        Ok(self.empty_exec_result())
    }

    // ───────────────────────────── DDL ─────────────────────────────

    fn exec_create_table(
        &mut self,
        stmt: &CreateTableStmt,
        full_sql: &str,
        raw_sql: &str,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        if stmt.temp {
            return Err(SqliteError::Sql(
                "TEMP tables are not supported in single-database mode".to_string(),
            ));
        }
        self.pager.ensure_initialized()?;
        let tname = &stmt.name.name;
        if tname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {tname}"
            )));
        }
        {
            let view = self.live_view()?;
            if view.schema().index(tname).is_some() {
                return Err(SqliteError::Sql(format!(
                    "there is already an index named {tname}"
                )));
            }
            if view.schema().object(tname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("table {tname} already exists")));
            }
        }

        // Handle `CREATE TABLE t AS SELECT ...`
        if let Some(ref sel) = stmt.as_select {
            let view = self.live_view()?;
            let opts = self.query_options(dialect);
            let (qcols, qrows) = self.eval_select_stmt_detailed(&view, sel, params, &opts)?;
            drop(view);

            let mut used_names: HashSet<String> = HashSet::with_capacity(qcols.len());
            let mut col_defs = Vec::with_capacity(qcols.len());
            for sc in &qcols {
                let cname = &sc.name;
                let mut unique_name = cname.clone();
                if !used_names.insert(unique_name.to_ascii_lowercase()) {
                    let mut suffix = 1u64;
                    loop {
                        let cand = format!("{cname}:{suffix}");
                        if used_names.insert(cand.to_ascii_lowercase()) {
                            unique_name = cand;
                            break;
                        }
                        suffix += 1;
                    }
                }
                let col_type = match sc.affinity {
                    Affinity::Integer => "INT",
                    Affinity::Text => "TEXT",
                    Affinity::Real => "REAL",
                    Affinity::Numeric | Affinity::Flexnum => "NUM",
                    Affinity::Blob | Affinity::None => "",
                };
                let escaped = unique_name.replace('"', "\"\"");
                if col_type.is_empty() {
                    col_defs.push(format!("\"{escaped}\""));
                } else {
                    col_defs.push(format!("\"{escaped}\" {col_type}"));
                }
            }
            let escaped_tname = tname.replace('"', "\"\"");
            let synth_sql = format!("CREATE TABLE \"{escaped_tname}\"({})", col_defs.join(","));
            let root_pg = create_btree(&mut self.pager, BtreePageType::TableLeaf)?;
            self.insert_schema_row("table", tname, tname, root_pg, Some(&synth_sql))?;
            self.pager.bump_schema_cookie()?;

            let enc = self.pager.encoding();
            let fmt = self.pager.schema_format();
            let mut rid = 0i64;
            for row in qrows {
                rid += 1;
                let payload = encode_record_with_format(&row, enc, fmt);
                table_insert(&mut self.pager, root_pg, rid, &payload, false)?;
            }
            return Ok(self.empty_exec_result());
        }

        // Validate table definition with SQLite's exact `build_table` (AST spans index `full_sql`)!
        let build_opts = BuildOptions {
            schema: false,
            file_format: self.pager.schema_format().clamp(1, 4),
            ..BuildOptions::default()
        };
        let def = build_table(stmt, full_sql, &build_opts)
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        let _ = generated_eval_order_from_defs(&def.name, &def.columns)?;

        let stored_sql =
            normalize_stored_ddl(raw_sql, stmt.if_not_exists, stmt.schema.is_some());

        let root_kind = if def.without_rowid {
            BtreePageType::IndexLeaf
        } else {
            BtreePageType::TableLeaf
        };
        let table_root = create_btree(&mut self.pager, root_kind)?;
        self.insert_schema_row("table", tname, tname, table_root, Some(&stored_sql))?;

        // Create `sqlite_autoindex_*` B-trees for UNIQUE / non-IPK PRIMARY KEY constraints.
        for idx_def in &def.indexes {
            if idx_def.without_rowid_primary_key {
                continue; // WITHOUT ROWID PK shares `table_root`!
            }
            let idx_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
            self.insert_schema_row("index", &idx_def.name, tname, idx_root, None)?;
        }

        // Create `sqlite_sequence` if table uses `AUTOINCREMENT`.
        if def.autoincrement {
            self.ensure_sqlite_sequence()?;
        }

        self.pager.bump_schema_cookie()?;
        let _ = self.live_view()?;
        Ok(self.empty_exec_result())
    }

    fn exec_create_index(
        &mut self,
        stmt: &CreateIndexStmt,
        raw_sql: &str,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        self.pager.ensure_initialized()?;
        let iname = &stmt.name.name;
        if iname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {iname}"
            )));
        }
        let tname = &stmt.table.name;
        let table_name_canon: String;
        {
            let view = self.live_view()?;
            if let Some(existing) = view.schema().object(iname) {
                let kind = match existing {
                    SchemaObject::Table(_) | SchemaObject::VirtualTable(_) => Some("table"),
                    SchemaObject::View(_) => Some("view"),
                    SchemaObject::Trigger(_) => Some("trigger"),
                    SchemaObject::Index(_) => None,
                };
                if let Some(k) = kind {
                    return Err(SqliteError::Sql(format!(
                        "there is already a {k} named {iname}"
                    )));
                }
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("index {iname} already exists")));
            }
            if view.schema().index(iname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("index {iname} already exists")));
            }
            let Some(tbl) = view.schema().table(tname) else {
                return Err(SqliteError::Sql(format!("no such table: main.{tname}")));
            };
            if tbl.system {
                return Err(SqliteError::Sql(format!(
                    "table {} may not be indexed",
                    tbl.name
                )));
            }
            // Validate columns, expressions, NULLS FIRST/LAST, subqueries, parameters, and
            // non-deterministic functions upfront BEFORE allocating pages or mutating sqlite_schema.
            let build_opts = BuildOptions {
                schema: false,
                file_format: self.pager.schema_format().clamp(1, 4),
                ..BuildOptions::default()
            };
            let idx_def = build_index(stmt, &tbl.definition, &build_opts)
                .map_err(|e| SqliteError::Sql(e.to_string()))?;
            for col in &idx_def.columns {
                if let Some(ref expr) = col.expr {
                    validate_index_expr_deterministic(expr, "index expressions")?;
                }
            }
            if let Some(ref where_expr) = idx_def.where_clause {
                validate_index_expr_deterministic(where_expr, "partial index WHERE clauses")?;
            }
            table_name_canon = tbl.name.clone();
        }

        let stored_sql =
            normalize_stored_ddl(raw_sql, stmt.if_not_exists, stmt.schema.is_some());
        let idx_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
        self.insert_schema_row(
            "index",
            iname,
            &table_name_canon,
            idx_root,
            Some(&stored_sql),
        )?;
        self.pager.bump_schema_cookie()?;

        // Populate index with all existing rows of the table in sorted key order!
        let view = self.live_view()?;
        let table = view
            .schema()
            .table(&table_name_canon)
            .expect("table exists")
            .clone();
        let idx_info = view
            .schema()
            .index(iname)
            .expect("newly created index exists")
            .clone();
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let colls = if self.cached_collations.0 == enc {
            self.cached_collations.1.clone()
        } else {
            CollationRegistry::new(enc)
        };
        let key_info = key_info_for_index(&idx_info, enc, Some(&colls))?;
        let idx_has_exprs = index_needs_expr_scope(&idx_info);
        let simple_stored_cols = !table.without_rowid
            && !idx_has_exprs
            && idx_info.definition.where_clause.is_none()
            && idx_info.columns[..idx_info.key_count].iter().all(|ic| {
                ic.cid >= 0
                    && table.columns[ic.cid as usize].hidden != ColumnHiddenKind::Virtual
            });

        // Ultra-fast flat single-column CREATE INDEX path: walks table leaves directly on
        // `&self.pager` (bypassing `live_view()`, `Pager` Mutex, and `RecordView`), sorting a
        // compact 16-byte `Vec<(i64, i64)>` with stack 2-int record encoding when all keys are ints!
        if simple_stored_cols && idx_info.key_count == 1 {
            let col_cid = idx_info.columns[0].cid as usize;
            let col = &table.columns[col_cid];
            let is_desc = key_info.desc[0];
            let mut mem = ConnMemGuard::new(&self.pager);
            let scanned = if Some(col_cid) != table.ipk_column {
                if let Some(sidx) = col.storage_index {
                    drop(view);
                    table_scan_single_stored_col_direct(
                        &self.pager,
                        table.root_page,
                        sidx,
                        &col.default_value,
                        col.affinity == Affinity::Real,
                        enc,
                        &mut |b| mem.reserve(b),
                    )?
                } else {
                    let reader = view.table(&table.name)?;
                    let scan_opts = ScanOptions {
                        columns: Some(vec![ColumnSpec::Cid(col_cid)]),
                        rowid: false,
                    };
                    let mut cur = reader.cursor(Some(&scan_opts))?;
                    let mut entries: Vec<(SqlValue, i64)> = Vec::new();
                    while cur.next()? {
                        let rid = cur.rowid()?;
                        let val = cur.value(0)?;
                        if mem.meter.is_some() {
                            mem.reserve(estimate_sql_val_bytes(&val).saturating_add(16))?;
                        }
                        entries.push((val, rid));
                    }
                    drop(view);
                    FastIndexScan::Mixed(entries)
                }
            } else {
                let reader = view.table(&table.name)?;
                let scan_opts = ScanOptions {
                    columns: Some(vec![ColumnSpec::Cid(col_cid)]),
                    rowid: false,
                };
                let mut cur = reader.cursor(Some(&scan_opts))?;
                let mut entries: Vec<(SqlValue, i64)> = Vec::new();
                while cur.next()? {
                    let rid = cur.rowid()?;
                    let val = cur.value(0)?;
                    if mem.meter.is_some() {
                        mem.reserve(estimate_sql_val_bytes(&val).saturating_add(16))?;
                    }
                    entries.push((val, rid));
                }
                drop(view);
                FastIndexScan::Mixed(entries)
            };

            let metered = !self.pager.is_noop_meter();
            match scanned {
                FastIndexScan::AllInts(mut int_entries) => {
                    // `table_scan_single_stored_col_direct` visits rows in strictly ascending
                    // rowid order (`a.1 < b.1`), so any stable sort on `k` automatically preserves
                    // ascending rowid order on ties without comparing `a.1`!
                    sort_int_index_entries(&mut int_entries, is_desc, &mut mem)?;
                    if idx_info.unique {
                        for pair in int_entries.windows(2) {
                            if pair[0].0 == pair[1].0 {
                                return Err(SqliteError::Sql(format!(
                                    "UNIQUE constraint failed: index {iname}"
                                )));
                            }
                        }
                    }
                    index_bulk_load_sorted_two_int(
                        &mut self.pager,
                        idx_info.root_page,
                        &int_entries,
                        fmt,
                    )?;
                    drop(mem);
                    return Ok(self.empty_exec_result());
                }
                FastIndexScan::Mixed(mut entries) => {
                    let binary_fallback = (enc != TextEncoding::Utf8)
                        .then(|| Collation::builtin(BuiltinCollation::Binary, enc));
                    let lead_coll = key_info.collations[0].as_ref().or(binary_fallback.as_ref());
                    entries.sort_unstable_by(|a, b| {
                        let mut o = match (&a.0, &b.0) {
                            (SqlValue::Int(x), SqlValue::Int(y)) => x.cmp(y),
                            _ => mem_compare(&a.0, &b.0, lead_coll),
                        };
                        if is_desc {
                            o = o.reverse();
                        }
                        o.then_with(|| a.1.cmp(&b.1))
                    });
                    if idx_info.unique {
                        for pair in entries.windows(2) {
                            if !pair[0].0.is_null()
                                && !pair[1].0.is_null()
                                && mem_compare(&pair[0].0, &pair[1].0, lead_coll)
                                    == std::cmp::Ordering::Equal
                            {
                                return Err(SqliteError::Sql(format!(
                                    "UNIQUE constraint failed: index {iname}"
                                )));
                            }
                        }
                    }
                    let init_buf_cap = entries.len().saturating_mul(16);
                    let init_ranges_bytes =
                        entries.len().saturating_mul(std::mem::size_of::<(usize, usize)>());
                    let mut reserved_buf = 0u64;
                    if metered {
                        let pre = (init_buf_cap as u64).saturating_add(init_ranges_bytes as u64);
                        if pre > 0 {
                            self.pager.meter().reserve_memory(pre)?;
                            reserved_buf = pre;
                        }
                    }
                    let mut scratch = Vec::with_capacity(64);
                    let mut cell_buf = Vec::with_capacity(init_buf_cap);
                    let mut cell_ranges = Vec::with_capacity(entries.len());
                    let mut used_buf_bytes = 0u64;
                    let build_res: SqliteResult<()> = (|| {
                        for (k, rid) in entries {
                            let pair = [k, SqlValue::Int(rid)];
                            encode_record_with_format_into(&pair, enc, fmt, &mut scratch);
                            let before = cell_buf.len();
                            let range =
                                append_index_cell_body(&mut self.pager, &scratch, &mut cell_buf)?;
                            if metered {
                                used_buf_bytes = used_buf_bytes.saturating_add(
                                    (cell_buf.len() - before
                                        + std::mem::size_of::<(usize, usize)>())
                                        as u64,
                                );
                                if used_buf_bytes > reserved_buf {
                                    let delta = used_buf_bytes - reserved_buf;
                                    self.pager.meter().reserve_memory(delta)?;
                                    reserved_buf = used_buf_bytes;
                                }
                            }
                            cell_ranges.push(range);
                        }
                        drop(mem);
                        index_bulk_load_sorted_flat(
                            &mut self.pager,
                            idx_info.root_page,
                            &cell_buf,
                            &cell_ranges,
                        )
                    })();
                    if reserved_buf > 0 {
                        self.pager.meter().release_memory(reserved_buf);
                    }
                    build_res?;
                    return Ok(self.empty_exec_result());
                }
            }
        }

        let mut entries_mem = ConnMemGuard::new(&self.pager);
        let mut entries: Vec<Vec<SqlValue>> = if simple_stored_cols {
            // Fast projected scan: decode ONLY the indexed columns (skips unused TEXT/BLOB cols)!
            let reader = view.table(&table.name)?;
            let specs: Vec<ColumnSpec> = idx_info.columns[..idx_info.key_count]
                .iter()
                .map(|ic| ColumnSpec::Cid(ic.cid as usize))
                .collect();
            let scan_opts = ScanOptions {
                columns: Some(specs),
                rowid: false,
            };
            let mut cur = reader.cursor(Some(&scan_opts))?;
            let mut out = Vec::new();
            while cur.next()? {
                let rid = cur.rowid()?;
                let mut vals = cur.row()?;
                vals.push(SqlValue::Int(rid));
                if entries_mem.meter.is_some() {
                    entries_mem.reserve(estimate_sql_row_bytes(&vals))?;
                }
                out.push(vals);
            }
            out
        } else {
            let mut rows_mem = ConnMemGuard::new(&self.pager);
            let existing_rows = self.read_all_table_rows(&view, &table, &mut rows_mem)?;
            let qopts = self.query_options(self.dialect);
            let expr_scope_cols = if idx_has_exprs {
                build_plain_scope_columns(&table)
            } else {
                Vec::new()
            };
            let where_scope_cols = if idx_info.definition.where_clause.is_some() {
                build_table_scope_columns(&table, &table.name)
            } else {
                Vec::new()
            };
            let mut out = Vec::with_capacity(existing_rows.len());
            for (rowid, full_row) in existing_rows {
                if let Some(entry_vals) = build_index_entry_values_fast(
                    &table,
                    &idx_info,
                    rowid,
                    &full_row,
                    &qopts,
                    &expr_scope_cols,
                    &where_scope_cols,
                )? {
                    if entries_mem.meter.is_some() {
                        entries_mem.reserve(estimate_sql_row_bytes(&entry_vals))?;
                    }
                    out.push(entry_vals);
                }
            }
            out
        };
        // Drop read snapshot BEFORE mutating pages so `WritePager::pages` stays uniquely owned!
        drop(view);

        // Sort in memory by exact index key order (O(N) UNIQUE check + O(N) bottom-up B-tree build!).
        entries.sort_by(|a, b| compare_index_values(a, b, &key_info));
        if idx_info.unique {
            for pair in entries.windows(2) {
                let a_prefix = &pair[0][..idx_info.key_count];
                let b_prefix = &pair[1][..idx_info.key_count];
                if !prefix_has_null(a_prefix)
                    && !prefix_has_null(b_prefix)
                    && key_tuples_equal(a_prefix, b_prefix, &key_info)
                {
                    return Err(SqliteError::Sql(format!(
                        "UNIQUE constraint failed: index {iname}"
                    )));
                }
            }
        }
        let metered = !self.pager.is_noop_meter();
        let meter = self.pager.meter().clone();
        let mut payload_bytes: u64 = 0;
        let mut sorted_payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
        let build_res: SqliteResult<()> = (|| {
            for entry_vals in &entries {
                let enc_rec = encode_record_with_format(entry_vals, enc, fmt);
                if metered {
                    let b = (enc_rec.len() as u64).saturating_add(24);
                    meter.reserve_memory(b)?;
                    payload_bytes = payload_bytes.saturating_add(b);
                }
                sorted_payloads.push(enc_rec);
            }
            drop(entries);
            drop(entries_mem);
            index_bulk_load_sorted(&mut self.pager, idx_info.root_page, &sorted_payloads)
        })();
        if payload_bytes > 0 {
            meter.release_memory(payload_bytes);
        }
        build_res?;
        Ok(self.empty_exec_result())
    }

    fn exec_create_view(&mut self, stmt: &CreateViewStmt, raw_sql: &str) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        if stmt.temp {
            return Err(SqliteError::Sql(
                "TEMP views are not supported in single-database mode".to_string(),
            ));
        }
        if select_has_variables(&stmt.select) {
            return Err(SqliteError::Sql(
                "parameters are not allowed in views".to_string(),
            ));
        }
        self.pager.ensure_initialized()?;
        let vname = &stmt.name.name;
        if vname.to_ascii_lowercase().starts_with("sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {vname}"
            )));
        }
        {
            let view = self.live_view()?;
            if view.schema().index(vname).is_some() {
                return Err(SqliteError::Sql(format!(
                    "there is already an index named {vname}"
                )));
            }
            if view.schema().object(vname).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!("view {vname} already exists")));
            }
        }
        let stored_sql =
            normalize_stored_ddl(raw_sql, stmt.if_not_exists, stmt.schema.is_some());
        // Verify stored view DDL parses cleanly under standard SQLite schema rules.
        xdb_parser::parse_statement(&stored_sql, &ParseOptions::default())
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        self.insert_schema_row("view", vname, vname, 0, Some(&stored_sql))?;
        self.pager.bump_schema_cookie()?;
        let _ = self.live_view()?;
        Ok(self.empty_exec_result())
    }

    fn exec_drop(&mut self, stmt: &DropStmt) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        let name = &stmt.name.name;
        let view = self.live_view()?;

        match stmt.object_type {
            DropObjectType::Table => {
                let tbl = match view.schema().object(name) {
                    Some(SchemaObject::Table(tbl)) => tbl,
                    Some(SchemaObject::View(_)) => {
                        return Err(SqliteError::Sql(format!(
                            "use DROP VIEW to delete view {name}"
                        )));
                    }
                    _ => {
                        if stmt.if_exists {
                            return Ok(self.empty_exec_result());
                        }
                        return Err(SqliteError::Sql(format!("no such table: {name}")));
                    }
                };
                if tbl.system {
                    return Err(SqliteError::Sql(format!(
                        "table {} may not be dropped",
                        tbl.name
                    )));
                }
                let tbl_name = tbl.name.clone();
                let tbl_root = tbl.root_page;
                let idx_roots: Vec<u32> = tbl
                    .indexes
                    .iter()
                    .filter(|ix| !ix.without_rowid_primary_key)
                    .map(|ix| ix.root_page)
                    .collect();
                let schema_rowids: Vec<i64> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.table_name
                            .as_deref()
                            .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl_name))
                    })
                    .map(|e| e.rowid)
                    .collect();
                let seq_root = view.schema().table("sqlite_sequence").map(|s| s.root_page);
                let mut seq_del_rids: Vec<i64> = Vec::new();
                if let Some(seq_tbl) = view.schema().table("sqlite_sequence") {
                    if let Ok(reader) = view.table(&seq_tbl.name) {
                        let opts = ScanOptions {
                            columns: None,
                            rowid: false,
                        };
                        let mut cur = reader.cursor(Some(&opts))?;
                        while cur.next()? {
                            let rid = cur.rowid()?;
                            let r = cur.row()?;
                            if r.first()
                                .and_then(SqlValue::as_str)
                                .is_some_and(|n| n.eq_ignore_ascii_case(&tbl_name))
                            {
                                seq_del_rids.push(rid);
                            }
                        }
                    }
                }
                drop(view);

                for r in idx_roots {
                    drop_btree(&mut self.pager, r)?;
                }
                drop_btree(&mut self.pager, tbl_root)?;
                for srid in schema_rowids {
                    table_delete(&mut self.pager, 1, srid)?;
                }
                if let Some(sr) = seq_root {
                    for rid in seq_del_rids {
                        table_delete(&mut self.pager, sr, rid)?;
                    }
                }
                self.cached_seq = None;
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::Index => {
                let Some(ix) = view.schema().index(name) else {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such index: {name}")));
                };
                if ix.auto {
                    return Err(SqliteError::Sql(
                        "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped"
                            .to_string(),
                    ));
                }
                let ix_root = ix.root_page;
                let ix_name = ix.name.clone();
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&ix_name))
                    })
                    .map(|e| e.rowid);
                drop(view);

                drop_btree(&mut self.pager, ix_root)?;
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::View => {
                let v = match view.schema().object(name) {
                    Some(SchemaObject::View(v)) => v,
                    Some(SchemaObject::Table(_)) | Some(SchemaObject::VirtualTable(_)) => {
                        return Err(SqliteError::Sql(format!(
                            "use DROP TABLE to delete table {name}"
                        )));
                    }
                    _ => {
                        if stmt.if_exists {
                            return Ok(self.empty_exec_result());
                        }
                        return Err(SqliteError::Sql(format!("no such view: {name}")));
                    }
                };
                let vname = v.name.clone();
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("view")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&vname))
                    })
                    .map(|e| e.rowid);
                drop(view);
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
            DropObjectType::Trigger => {
                let srid = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("trigger")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .map(|e| e.rowid);
                if srid.is_none() {
                    if stmt.if_exists {
                        return Ok(self.empty_exec_result());
                    }
                    return Err(SqliteError::Sql(format!("no such trigger: {name}")));
                }
                drop(view);
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                self.pager.bump_schema_cookie()?;
                Ok(self.empty_exec_result())
            }
        }
    }

    fn exec_alter_table(
        &mut self,
        stmt: &AlterTableStmt,
        full_sql: &str,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        let view = self.live_view()?;
        let Some(tbl) = view.schema().table(&stmt.table.name).cloned() else {
            return Err(SqliteError::Sql(format!(
                "no such table: {}",
                stmt.table.name
            )));
        };
        if tbl.system {
            return Err(SqliteError::Sql(format!(
                "table {} may not be altered",
                tbl.name
            )));
        }
        let table_entry = view
            .schema()
            .entries
            .iter()
            .find(|e| {
                e.obj_type.as_deref() == Some("table")
                    && e.name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
            })
            .cloned()
            .ok_or_else(|| SqliteError::corrupt("missing sqlite_schema entry for table"))?;
        let old_sql = table_entry
            .sql
            .as_deref()
            .ok_or_else(|| SqliteError::corrupt("missing CREATE TABLE sql"))?
            .to_string();

        match &stmt.action {
            AlterTableAction::RenameTable { new_name, .. } => {
                if new_name.name.to_ascii_lowercase().starts_with("sqlite_") {
                    return Err(SqliteError::Sql(format!(
                        "object name reserved for internal use: {}",
                        new_name.name
                    )));
                }
                if view.schema().object(&new_name.name).is_some()
                    || view.schema().index(&new_name.name).is_some()
                {
                    return Err(SqliteError::Sql(format!(
                        "there is already another table or index with this name: {}",
                        new_name.name
                    )));
                }
                let updated_sql = rewrite_create_table_name(&old_sql, &new_name.name)?;
                let dep_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.rowid != table_entry.rowid
                            && e.table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
                let view_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| e.obj_type.as_deref() == Some("view") && e.sql.is_some())
                    .cloned()
                    .collect();
                let seq_root = view.schema().table("sqlite_sequence").map(|s| s.root_page);
                let mut seq_entries: Vec<(i64, i64)> = Vec::new();
                if let Some(seq_tbl) = view.schema().table("sqlite_sequence") {
                    if let Ok(reader) = view.table(&seq_tbl.name) {
                        let opts = ScanOptions {
                            columns: None,
                            rowid: false,
                        };
                        let mut cur = reader.cursor(Some(&opts))?;
                        while cur.next()? {
                            let rid = cur.rowid()?;
                            let r = cur.row()?;
                            if r.first()
                                .and_then(SqlValue::as_str)
                                .is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
                            {
                                seq_entries.push((rid, r.get(1).map_or(0, int_value)));
                            }
                        }
                    }
                }
                drop(view);

                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &new_name.name,
                    &new_name.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                let old_auto_prefix = format!("sqlite_autoindex_{}_", tbl.name);
                for dep in dep_entries {
                    let dep_name_orig = dep.name.as_deref().unwrap_or("");
                    let dep_name_new = if dep.sql.is_none()
                        && dep_name_orig
                            .as_bytes()
                            .get(..old_auto_prefix.len())
                            .is_some_and(|p| p.eq_ignore_ascii_case(old_auto_prefix.as_bytes()))
                    {
                        format!(
                            "sqlite_autoindex_{}_{}",
                            new_name.name,
                            &dep_name_orig[old_auto_prefix.len()..]
                        )
                    } else {
                        dep_name_orig.to_string()
                    };
                    let dep_sql = dep
                        .sql
                        .as_deref()
                        .map(|s| rewrite_on_table_name(s, &tbl.name, &new_name.name));
                    self.update_schema_row(
                        dep.rowid,
                        dep.obj_type.as_deref().unwrap_or("index"),
                        &dep_name_new,
                        &new_name.name,
                        dep.root_page,
                        dep_sql.as_deref(),
                    )?;
                }
                for ve in view_entries {
                    if let Some(ref vsql) = ve.sql {
                        let new_vsql =
                            rewrite_table_name_in_view_ddl(vsql, &tbl.name, &new_name.name);
                        if new_vsql != *vsql {
                            let vname = ve.name.as_deref().unwrap_or("");
                            self.update_schema_row(
                                ve.rowid,
                                "view",
                                vname,
                                vname,
                                ve.root_page,
                                Some(&new_vsql),
                            )?;
                        }
                    }
                }
                if let Some(sr) = seq_root {
                    for (seq_rid, seq_val) in seq_entries {
                        let vals = vec![SqlValue::text(&new_name.name), SqlValue::Int(seq_val)];
                        let payload = encode_record_with_format(
                            &vals,
                            self.pager.encoding(),
                            self.pager.schema_format(),
                        );
                        table_insert(&mut self.pager, sr, seq_rid, &payload, true)?;
                    }
                }
                self.cached_seq = None;
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::AddColumn { column, .. } => {
                if tbl
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&column.name.name))
                {
                    return Err(SqliteError::Sql(format!(
                        "duplicate column name: {}",
                        column.name.name
                    )));
                }
                let mut has_not_null = false;
                let mut has_non_null_default = false;
                let mut has_check = false;
                let mut is_generated = false;
                for c in &column.constraints {
                    match c {
                        ColumnConstraint::PrimaryKey { .. } => {
                            return Err(SqliteError::Sql(
                                "Cannot add a PRIMARY KEY column".to_string(),
                            ));
                        }
                        ColumnConstraint::Unique { .. } => {
                            return Err(SqliteError::Sql(
                                "Cannot add a UNIQUE column".to_string(),
                            ));
                        }
                        ColumnConstraint::Generated { storage, .. } => {
                            if *storage == Some(GeneratedStorage::Stored) {
                                return Err(SqliteError::Sql(
                                    "cannot add a STORED column".to_string(),
                                ));
                            }
                            is_generated = true;
                        }
                        ColumnConstraint::NotNull { .. } => {
                            has_not_null = true;
                        }
                        ColumnConstraint::Check(_) => {
                            has_check = true;
                        }
                        ColumnConstraint::Default { form, expr, .. } => {
                            if *form == DefaultForm::Paren
                                || matches!(expr, Expr::CurrentTime { .. })
                            {
                                return Err(SqliteError::Sql(
                                    "Cannot add a column with non-constant default".to_string(),
                                ));
                            }
                            let is_null_dflt = match expr {
                                Expr::Null { .. } => true,
                                Expr::Unary {
                                    op: xdb_parser::UnaryOp::Pos | xdb_parser::UnaryOp::Neg,
                                    operand,
                                    ..
                                } => matches!(operand.as_ref(), Expr::Null { .. }),
                                _ => false,
                            };
                            has_non_null_default = !is_null_dflt;
                        }
                        _ => {}
                    }
                }
                if has_not_null && !is_generated && !has_non_null_default {
                    return Err(SqliteError::Sql(
                        "Cannot add a NOT NULL column with default value NULL".to_string(),
                    ));
                }
                let col_def_sql = full_sql[column.span.start..column.span.end].trim();
                let updated_sql = append_column_to_create_table(&old_sql, col_def_sql)?;
                // Verify the new CREATE TABLE SQL parses cleanly via `build_table`!
                let p_opts = ParseOptions::default();
                let parsed = xdb_parser::parse_statement(&updated_sql, &p_opts)
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::CreateTable(ct) = parsed {
                    let b_opts = BuildOptions {
                        schema: false,
                        file_format: self.pager.schema_format().clamp(1, 4),
                        ..BuildOptions::default()
                    };
                    let def = build_table(&ct, &updated_sql, &b_opts)
                        .map_err(|e| SqliteError::Sql(e.to_string()))?;
                    let _ = generated_eval_order_from_defs(&def.name, &def.columns)?;
                }
                drop(view);
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                self.pager.bump_schema_cookie()?;
                let new_view = self.live_view()?;
                // Validate new CHECK constraints, VIRTUAL NOT NULL constraints, and STRICT DEFAULT
                // types against existing rows (SQLite rejects mistyped DEFAULT on non-empty STRICT).
                if has_check || (is_generated && has_not_null) || tbl.definition.strict {
                    if let Some(new_tbl) = new_view.schema().table(&tbl.name).cloned() {
                        let mut _rows_mem = ConnMemGuard::new(&self.pager);
                        let existing_rows =
                            self.read_all_table_rows(&new_view, &new_tbl, &mut _rows_mem)?;
                        drop(new_view);
                        let qopts = self.query_options(self.dialect);
                        let empty_params = QueryParams::empty();
                        let dml = DmlTableState::build(Arc::clone(&new_tbl), self.pager.encoding())?;
                        for (rid, mut row) in existing_rows {
                            self.finalize_and_validate_row(
                                &new_tbl,
                                &dml,
                                rid,
                                &mut row,
                                None,
                                &empty_params,
                                &qopts,
                            )?;
                        }
                    }
                }
                Ok(self.empty_exec_result())
            }
            AlterTableAction::RenameColumn {
                column, new_name, ..
            } => {
                let Some(cid) = tbl
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column.name))
                else {
                    return Err(SqliteError::Sql(format!(
                        "no such column: \"{}\"",
                        column.name
                    )));
                };
                if tbl
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&new_name.name))
                {
                    return Err(SqliteError::Sql(format!(
                        "duplicate column name: {}",
                        new_name.name
                    )));
                }
                let old_col_name = tbl.columns[cid].name.clone();
                let updated_sql = rewrite_column_name_in_ddl(
                    &old_sql,
                    &tbl.name,
                    cid,
                    &old_col_name,
                    &new_name.name,
                )?;
                let dep_indexes: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.sql.is_some()
                            && e.table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
                let view_rewrites: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| e.obj_type.as_deref() == Some("view") && e.sql.is_some())
                    .filter_map(|ve| {
                        let vsql = ve.sql.as_deref()?;
                        let new_vsql = rewrite_column_name_in_view_ddl(
                            vsql,
                            &tbl.name,
                            &old_col_name,
                            &new_name.name,
                            view.schema(),
                        );
                        if new_vsql != vsql {
                            Some((ve.clone(), new_vsql))
                        } else {
                            None
                        }
                    })
                    .collect();
                drop(view);
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                for dep in dep_indexes {
                    if let Some(ref dep_sql) = dep.sql {
                        let new_dep_sql = rewrite_column_name_in_index_ddl(
                            dep_sql,
                            &tbl.name,
                            &old_col_name,
                            &new_name.name,
                        )?;
                        if new_dep_sql != *dep_sql {
                            self.update_schema_row(
                                dep.rowid,
                                "index",
                                dep.name.as_deref().unwrap_or(""),
                                &tbl.name,
                                dep.root_page,
                                Some(&new_dep_sql),
                            )?;
                        }
                    }
                }
                for (ve, new_vsql) in view_rewrites {
                    let vname = ve.name.as_deref().unwrap_or("");
                    self.update_schema_row(
                        ve.rowid,
                        "view",
                        vname,
                        vname,
                        ve.root_page,
                        Some(&new_vsql),
                    )?;
                }
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::DropColumn { column, .. } => {
                let Some(cid) = tbl
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column.name))
                else {
                    return Err(SqliteError::Sql(format!(
                        "no such column: \"{}\"",
                        column.name
                    )));
                };
                if tbl.columns.len() <= 1 {
                    return Err(SqliteError::Sql(
                        "cannot drop the only column of a table".to_string(),
                    ));
                }
                if tbl.columns[cid].pk > 0 {
                    return Err(SqliteError::Sql(format!(
                        "cannot drop PRIMARY KEY column \"{}\"",
                        column.name
                    )));
                }
                for ix in &tbl.indexes {
                    if ix
                        .columns
                        .iter()
                        .take(ix.key_count)
                        .any(|ic| ic.cid == cid as i32)
                    {
                        return Err(SqliteError::Sql(format!(
                            "error in index {} after drop column: no such column: {}",
                            ix.name, column.name
                        )));
                    }
                }
                let old_col_name = tbl.columns[cid].name.clone();
                check_column_unreferenced_in_ddl(&old_sql, &tbl.name, cid, &old_col_name)?;
                for dep in &view.schema().entries {
                    if dep.obj_type.as_deref() == Some("index")
                        && dep
                            .table_name
                            .as_deref()
                            .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    {
                        if let Some(ref dep_sql) = dep.sql {
                            check_column_unreferenced_in_index_ddl(
                                dep_sql,
                                dep.name.as_deref().unwrap_or(""),
                                &tbl.name,
                                &old_col_name,
                            )?;
                        }
                    }
                }
                for v in &view.schema().views {
                    check_column_unreferenced_in_view_ddl(
                        &v.sql,
                        &v.name,
                        &tbl.name,
                        &old_col_name,
                        view.schema(),
                    )?;
                }
                for tr in &view.schema().triggers {
                    check_column_unreferenced_in_trigger_ddl(
                        &tr.sql,
                        &tr.name,
                        &tbl.name,
                        &old_col_name,
                        view.schema(),
                    )?;
                }
                let mut _rows_mem = ConnMemGuard::new(&self.pager);
                let existing = self.read_all_table_rows(&view, &tbl, &mut _rows_mem)?;
                let updated_sql = remove_column_from_ddl(&old_sql, cid)?;
                drop(view);

                // Update schema first, then rewrite stored table rows without the dropped column!
                self.update_schema_row(
                    table_entry.rowid,
                    "table",
                    &tbl.name,
                    &tbl.name,
                    table_entry.root_page,
                    Some(&updated_sql),
                )?;
                self.pager.bump_schema_cookie()?;

                let new_view = self.live_view()?;
                if tbl.columns[cid].hidden != ColumnHiddenKind::Virtual {
                    let new_tbl = new_view
                        .schema()
                        .table(&tbl.name)
                        .expect("table exists")
                        .clone();
                    drop(new_view);
                    let enc = self.pager.encoding();
                    let fmt = self.pager.schema_format();
                    if !tbl.without_rowid {
                        for (rid, mut row) in existing {
                            row.remove(cid);
                            let mut rec = Vec::new();
                            encode_table_record_into(&new_tbl, &row, enc, fmt, &mut rec);
                            table_insert(
                                &mut self.pager,
                                new_tbl.root_page,
                                rid.unwrap(),
                                &rec,
                                true,
                            )?;
                        }
                    } else if let Some(pk_idx) =
                        new_tbl.indexes.iter().find(|ix| ix.without_rowid_primary_key)
                    {
                        let colls = CollationRegistry::new(enc);
                        let ki = key_info_for_without_rowid_pk(pk_idx, enc, &colls)?;
                        for (_, mut row) in existing {
                            row.remove(cid);
                            let rec_vals = build_without_rowid_record_values(&new_tbl, &row);
                            let payload = encode_record_with_format(&rec_vals, enc, fmt);
                            index_insert(&mut self.pager, new_tbl.root_page, &payload, &ki, true)?;
                        }
                    }
                }
                Ok(self.empty_exec_result())
            }
            other => Err(SqliteError::Sql(format!(
                "unsupported ALTER TABLE action: {other:?}"
            ))),
        }
    }

    // ───────────────────────────── DML: INSERT / UPSERT ─────────────────────────────

    fn exec_insert(
        &mut self,
        stmt: &InsertStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let dml = self.get_dml_state_by_name(&stmt.table.name.name)?;
        let table = &dml.table;
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }
        if table.name.eq_ignore_ascii_case("sqlite_sequence") {
            self.flush_seq_cache()?;
            self.cached_seq = None;
        }

        let stmt_conflict = if stmt.replace_into {
            Some(ConflictAction::Replace)
        } else {
            stmt.or_conflict
        };
        let target_alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();

        // Determine target column indices (stack-allocated for <= 64 columns to avoid per-INSERT heap alloc).
        #[derive(Clone, Copy)]
        enum TargetCol {
            Col(usize),
            Rowid,
        }
        let mut target_cols_buf = [TargetCol::Rowid; 64];
        let mut target_cols_heap: Vec<TargetCol> = Vec::new();
        let target_cols_len: usize;
        if let Some(ref cols) = stmt.columns {
            if cols.len() > 64 {
                target_cols_heap.reserve(cols.len());
            }
            let mut n = 0usize;
            for c in cols {
                let tc = if let Some(pos) = table
                    .columns
                    .iter()
                    .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                {
                    if table.columns[pos].generated.is_some() {
                        return Err(SqliteError::Sql(format!(
                            "cannot INSERT into generated column \"{}\"",
                            c.name
                        )));
                    }
                    TargetCol::Col(pos)
                } else if !table.without_rowid
                    && (c.name.eq_ignore_ascii_case("rowid")
                        || c.name.eq_ignore_ascii_case("_rowid_")
                        || c.name.eq_ignore_ascii_case("oid"))
                {
                    TargetCol::Rowid
                } else {
                    return Err(SqliteError::Sql(format!(
                        "table {} has no column named {}",
                        table.name, c.name
                    )));
                };
                if cols.len() <= 64 {
                    target_cols_buf[n] = tc;
                } else {
                    target_cols_heap.push(tc);
                }
                n += 1;
            }
            target_cols_len = n;
        } else {
            let mut n = 0usize;
            for c in &table.columns {
                if c.generated.is_none() {
                    if n < 64 && target_cols_heap.is_empty() {
                        target_cols_buf[n] = TargetCol::Col(c.cid);
                    } else {
                        if target_cols_heap.is_empty() {
                            target_cols_heap.extend_from_slice(&target_cols_buf[..n]);
                        }
                        target_cols_heap.push(TargetCol::Col(c.cid));
                    }
                    n += 1;
                }
            }
            target_cols_len = n;
        }
        let target_cols: &[TargetCol] = if target_cols_heap.is_empty() {
            &target_cols_buf[..target_cols_len]
        } else {
            &target_cols_heap
        };

        // Ultra-fast zero-qopts single-row `INSERT INTO t(...) VALUES(params/consts)` path:
        // skips `query_options()` cloning, reuses `self.row_scratch`, and stack-encodes 1-col
        // integer secondary index entries directly.
        if !table.without_rowid
            && table.columns.len() <= 64
            && stmt.upsert.is_empty()
            && stmt.returning.is_none()
            && dml.gen_order.is_empty()
            && table.definition.checks.is_empty()
            && !dml.any_default_expr
            && !table.definition.strict
            && dml.simple_delete_elidable
        {
            if let Some(ref sel) = stmt.select {
                if let Some(vals) = direct_values_fast_path(stmt, sel) {
                    if vals.rows.len() == 1 {
                        let vrow = &vals.rows[0];
                        if vrow.items.len() != target_cols.len() {
                            return Err(SqliteError::Sql(format!(
                                "table {} has {} columns but {} values were supplied",
                                table.name,
                                target_cols.len(),
                                vrow.items.len()
                            )));
                        }
                        if !self.pager.is_noop_meter() {
                            self.pager.meter().charge_steps(vrow.items.len() as u64)?;
                        }
                        let mut target_col_mask = 0u64;
                        for tcol in target_cols {
                            if let TargetCol::Col(cid) = *tcol {
                                target_col_mask |= 1u64 << cid;
                            }
                        }
                        let mut row = std::mem::take(&mut self.row_scratch);
                        row.clear();
                        row.reserve(table.columns.len());
                        for (cid, col) in table.columns.iter().enumerate() {
                            if (target_col_mask & (1u64 << cid)) != 0
                                || Some(cid) == table.ipk_column
                            {
                                row.push(SqlValue::Null);
                            } else {
                                row.push(col.default_value.clone());
                            }
                        }
                        let mut all_simple = true;
                        let mut assigned_mask = 0u64;
                        let mut ipk_candidate: Option<SqlValue> = None;
                        for (tcol, item) in target_cols.iter().zip(vrow.items.iter()) {
                            let Some(val) = try_eval_simple_const_expr(item, params)? else {
                                all_simple = false;
                                break;
                            };
                            match *tcol {
                                TargetCol::Col(cid) => {
                                    if Some(cid) == table.ipk_column {
                                        ipk_candidate = Some(val.clone());
                                        row[cid] = val;
                                    } else {
                                        let bit = 1u64 << cid;
                                        if (assigned_mask & bit) == 0 {
                                            row[cid] = val;
                                            assigned_mask |= bit;
                                        }
                                    }
                                }
                                TargetCol::Rowid => {
                                    ipk_candidate = Some(val);
                                }
                            }
                        }
                        if all_simple {
                            // Apply affinities and verify NOT NULL (fall back if violated so
                            // `ON CONFLICT` default-substitution/IGNORE/FAIL semantics run).
                            let mut not_null_ok = true;
                            for (cid, col) in table.columns.iter().enumerate() {
                                if Some(cid) == table.ipk_column {
                                    continue;
                                }
                                let v = std::mem::replace(&mut row[cid], SqlValue::Null);
                                let coerced = apply_affinity(v, col.affinity);
                                if col.not_null && coerced.is_null() {
                                    not_null_ok = false;
                                    break;
                                }
                                row[cid] = coerced;
                            }
                            if not_null_ok {
                                let autoinc_seq_cache: Option<i64> =
                                    if table.definition.autoincrement {
                                        Some(
                                            self.get_sqlite_sequence(&table.name)?
                                                .map_or(0, |(_, s)| s),
                                        )
                                    } else {
                                        None
                                    };
                                let (mut known_max_rowid, mut hint_leaf) =
                                    self.get_known_max_and_leaf(table.root_page);
                                let mut rowid_guaranteed_new = false;
                                let rid_res: SqliteResult<i64> = (|| {
                                    if let Some(ipk) = table.ipk_column {
                                        let candidate = ipk_candidate.as_ref().unwrap_or(&row[ipk]);
                                        if candidate.is_null() {
                                            let cur_max = match known_max_rowid {
                                                Some(m) => m,
                                                None => {
                                                    let m = table_max_rowid(
                                                        &self.pager,
                                                        table.root_page,
                                                    )?;
                                                    known_max_rowid = Some(m);
                                                    m
                                                }
                                            };
                                            let next = self.allocate_next_rowid_with_max(
                                                table,
                                                cur_max,
                                                autoinc_seq_cache,
                                            )?;
                                            if cur_max.is_none_or(|m| next > m) {
                                                rowid_guaranteed_new = true;
                                            }
                                            row[ipk] = SqlValue::Int(next);
                                            Ok(next)
                                        } else {
                                            let rid = coerce_ipk_value(candidate)?;
                                            if let Some(cur_max) = known_max_rowid {
                                                if cur_max.is_none_or(|m| rid > m) {
                                                    rowid_guaranteed_new = true;
                                                }
                                            }
                                            row[ipk] = SqlValue::Int(rid);
                                            Ok(rid)
                                        }
                                    } else if let Some(ref er) = ipk_candidate {
                                        if er.is_null() {
                                            let cur_max = match known_max_rowid {
                                                Some(m) => m,
                                                None => {
                                                    let m = table_max_rowid(
                                                        &self.pager,
                                                        table.root_page,
                                                    )?;
                                                    known_max_rowid = Some(m);
                                                    m
                                                }
                                            };
                                            let next = self.allocate_next_rowid_with_max(
                                                table,
                                                cur_max,
                                                autoinc_seq_cache,
                                            )?;
                                            if cur_max.is_none_or(|m| next > m) {
                                                rowid_guaranteed_new = true;
                                            }
                                            Ok(next)
                                        } else {
                                            let rid = coerce_ipk_value(er)?;
                                            if let Some(cur_max) = known_max_rowid {
                                                if cur_max.is_none_or(|m| rid > m) {
                                                    rowid_guaranteed_new = true;
                                                }
                                            }
                                            Ok(rid)
                                        }
                                    } else {
                                        let cur_max = match known_max_rowid {
                                            Some(m) => m,
                                            None => {
                                                let m =
                                                    table_max_rowid(&self.pager, table.root_page)?;
                                                known_max_rowid = Some(m);
                                                m
                                            }
                                        };
                                        let next = self.allocate_next_rowid_with_max(
                                            table,
                                            cur_max,
                                            autoinc_seq_cache,
                                        )?;
                                        if cur_max.is_none_or(|m| next > m) {
                                            rowid_guaranteed_new = true;
                                        }
                                        Ok(next)
                                    }
                                })();
                                let rid = match rid_res {
                                    Ok(r) => r,
                                    Err(e) => {
                                        row.clear();
                                        self.row_scratch = row;
                                        return Err(e);
                                    }
                                };
                                if rowid_guaranteed_new && !dml.any_unique_secondary {
                                    let enc = self.pager.encoding();
                                    let fmt = self.pager.schema_format();
                                    encode_table_record_into(
                                        table,
                                        &row,
                                        enc,
                                        fmt,
                                        &mut self.record_scratch,
                                    );
                                    let write_res: SqliteResult<()> = (|| {
                                        table_append_monotonic(
                                            &mut self.pager,
                                            table.root_page,
                                            &mut hint_leaf,
                                            rid,
                                            &self.record_scratch,
                                        )?;
                                        for (ix_idx, ix) in table.indexes.iter().enumerate() {
                                            if ix.without_rowid_primary_key {
                                                continue;
                                            }
                                            let ki = dml.index_key_infos[ix_idx]
                                                .as_ref()
                                                .ok_or_else(|| {
                                                    SqliteError::corrupt(
                                                        "missing KeyInfo for secondary index",
                                                    )
                                                })?;
                                            if ix.columns.len() == 2
                                                && ix.columns[0].cid >= 0
                                                && ix.columns[1].cid == -1
                                            {
                                                let v0 = &row[ix.columns[0].cid as usize];
                                                if let SqlValue::Int(k0) = *v0 {
                                                    if ki.is_two_int_asc() {
                                                        index_insert_two_int_asc(
                                                            &mut self.pager,
                                                            ix.root_page,
                                                            k0,
                                                            rid,
                                                            fmt,
                                                            ki,
                                                            false,
                                                        )?;
                                                        continue;
                                                    }
                                                    let mut idx_buf = [0u8; 20];
                                                    let ilen = encode_two_int_record_stack(
                                                        k0,
                                                        rid,
                                                        fmt,
                                                        &mut idx_buf,
                                                    );
                                                    let kvals =
                                                        [SqlValue::Int(k0), SqlValue::Int(rid)];
                                                    index_insert_with_values(
                                                        &mut self.pager,
                                                        ix.root_page,
                                                        &kvals,
                                                        &idx_buf[..ilen],
                                                        ki,
                                                        false,
                                                    )?;
                                                    continue;
                                                }
                                                let rid_v = SqlValue::Int(rid);
                                                let refs = [v0, &rid_v];
                                                encode_record_refs_into(
                                                    &refs,
                                                    enc,
                                                    fmt,
                                                    &mut self.record_scratch,
                                                );
                                                let kvals = [v0.clone(), rid_v];
                                                index_insert_with_values(
                                                    &mut self.pager,
                                                    ix.root_page,
                                                    &kvals,
                                                    &self.record_scratch,
                                                    ki,
                                                    false,
                                                )?;
                                            } else {
                                                let mut kvals =
                                                    Vec::with_capacity(ix.columns.len());
                                                for ic in &ix.columns {
                                                    if ic.cid == -1 {
                                                        kvals.push(SqlValue::Int(rid));
                                                    } else {
                                                        kvals.push(row[ic.cid as usize].clone());
                                                    }
                                                }
                                                encode_record_with_format_into(
                                                    &kvals,
                                                    enc,
                                                    fmt,
                                                    &mut self.record_scratch,
                                                );
                                                index_insert_with_values(
                                                    &mut self.pager,
                                                    ix.root_page,
                                                    &kvals,
                                                    &self.record_scratch,
                                                    ki,
                                                    false,
                                                )?;
                                            }
                                        }
                                        Ok(())
                                    })();
                                    row.clear();
                                    self.row_scratch = row;
                                    write_res?;
                                    self.set_append_tail(table.root_page, rid, hint_leaf);
                                    self.last_insert_rowid = rid;
                                    if table.definition.autoincrement {
                                        self.update_sqlite_sequence(&table.name, rid.max(0))?;
                                    }
                                    self.changes = 1;
                                    self.total_changes += 1;
                                    return Ok(ExecResult {
                                        columns: Vec::new(),
                                        rows: Vec::new(),
                                        changes: 1,
                                        last_insert_rowid: rid,
                                    });
                                }
                            }
                        }
                        row.clear();
                        self.row_scratch = row;
                    }
                }
            }
        }

        let qopts = self.query_options(dialect);

        // Single-row `INSERT INTO t(...) VALUES(...)` fast path: avoids allocating
        // `source_rows: Vec<Vec<SqlValue>>` and `assigned_cols: Vec<bool>`.
        if !table.without_rowid
            && table.columns.len() <= 64
            && stmt.upsert.is_empty()
            && stmt.returning.is_none()
        {
            if let Some(ref sel) = stmt.select {
                if let Some(vals) = direct_values_fast_path(stmt, sel) {
                    if vals.rows.len() == 1 {
                        let vrow = &vals.rows[0];
                        if vrow.items.len() != target_cols.len() {
                            return Err(SqliteError::Sql(format!(
                                "table {} has {} columns but {} values were supplied",
                                table.name,
                                target_cols.len(),
                                vrow.items.len()
                            )));
                        }
                        if let Some(ref m) = qopts.meter {
                            m.charge_steps(vrow.items.len() as u64)?;
                        }
                        let mut eval_env_opt: Option<EvalEnv<'_>> = None;
                        let mut target_col_mask = 0u64;
                        for tcol in target_cols {
                            if let TargetCol::Col(cid) = *tcol {
                                target_col_mask |= 1u64 << cid;
                            }
                        }
                        let mut row = Vec::with_capacity(table.columns.len());
                        for (cid, col) in table.columns.iter().enumerate() {
                            if (target_col_mask & (1u64 << cid)) != 0
                                || col.generated.is_some()
                                || Some(cid) == table.ipk_column
                            {
                                row.push(SqlValue::Null);
                            } else if let Some(ref dflt_expr) =
                                table.definition.columns[cid].default_expr
                            {
                                let v = eval_standalone_expr(dflt_expr, params, &qopts)?;
                                row.push(v);
                            } else {
                                row.push(col.default_value.clone());
                            }
                        }
                        let mut assigned_mask = 0u64;
                        let mut ipk_candidate: Option<SqlValue> = None;
                        for (tcol, item) in target_cols.iter().zip(vrow.items.iter()) {
                            let val = if let Some(sv) = try_eval_simple_const_expr(item, params)? {
                                sv
                            } else {
                                let env = eval_env_opt.get_or_insert_with(|| {
                                    EvalEnv::new(&qopts, params, &NoSubqueries)
                                });
                                eval_expr(item, &[], env)?
                            };
                            match *tcol {
                                TargetCol::Col(cid) => {
                                    if Some(cid) == table.ipk_column {
                                        ipk_candidate = Some(val.clone());
                                        row[cid] = val;
                                    } else {
                                        let bit = 1u64 << cid;
                                        if (assigned_mask & bit) == 0 {
                                            row[cid] = val;
                                            assigned_mask |= bit;
                                        }
                                    }
                                }
                                TargetCol::Rowid => {
                                    ipk_candidate = Some(val);
                                }
                            }
                        }
                        let autoinc_seq_cache: Option<i64> = if table.definition.autoincrement {
                            Some(
                                self.get_sqlite_sequence(&table.name)?
                                    .map_or(0, |(_, s)| s),
                            )
                        } else {
                            None
                        };
                        let mut known_max_rowid: Option<Option<i64>> =
                            self.get_known_max_rowid(table.root_page);
                        let mut rowid_guaranteed_new = false;
                        let mut rowid: Option<i64> = if let Some(ipk) = table.ipk_column {
                            let candidate = ipk_candidate.as_ref().unwrap_or(&row[ipk]);
                            if candidate.is_null() {
                                let cur_max = match known_max_rowid {
                                    Some(m) => m,
                                    None => {
                                        let m = table_max_rowid(&self.pager, table.root_page)?;
                                        known_max_rowid = Some(m);
                                        m
                                    }
                                };
                                let next = self.allocate_next_rowid_with_max(
                                    table,
                                    cur_max,
                                    autoinc_seq_cache,
                                )?;
                                if cur_max.is_none_or(|m| next > m) {
                                    rowid_guaranteed_new = true;
                                }
                                row[ipk] = SqlValue::Int(next);
                                Some(next)
                            } else {
                                let rid = coerce_ipk_value(candidate)?;
                                if let Some(cur_max) = known_max_rowid {
                                    if cur_max.is_none_or(|m| rid > m) {
                                        rowid_guaranteed_new = true;
                                    }
                                }
                                row[ipk] = SqlValue::Int(rid);
                                Some(rid)
                            }
                        } else if let Some(ref er) = ipk_candidate {
                            if er.is_null() {
                                let cur_max = match known_max_rowid {
                                    Some(m) => m,
                                    None => {
                                        let m = table_max_rowid(&self.pager, table.root_page)?;
                                        known_max_rowid = Some(m);
                                        m
                                    }
                                };
                                let next = self.allocate_next_rowid_with_max(
                                    table,
                                    cur_max,
                                    autoinc_seq_cache,
                                )?;
                                if cur_max.is_none_or(|m| next > m) {
                                    rowid_guaranteed_new = true;
                                }
                                Some(next)
                            } else {
                                let rid = coerce_ipk_value(er)?;
                                if let Some(cur_max) = known_max_rowid {
                                    if cur_max.is_none_or(|m| rid > m) {
                                        rowid_guaranteed_new = true;
                                    }
                                }
                                Some(rid)
                            }
                        } else {
                            let cur_max = match known_max_rowid {
                                Some(m) => m,
                                None => {
                                    let m = table_max_rowid(&self.pager, table.root_page)?;
                                    known_max_rowid = Some(m);
                                    m
                                }
                            };
                            let next = self.allocate_next_rowid_with_max(
                                table,
                                cur_max,
                                autoinc_seq_cache,
                            )?;
                            if cur_max.is_none_or(|m| next > m) {
                                rowid_guaranteed_new = true;
                            }
                            Some(next)
                        };

                        match self.finalize_and_validate_row(
                            table,
                            &dml,
                            rowid,
                            &mut row,
                            stmt_conflict,
                            params,
                            &qopts,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                self.changes = 0;
                                return Ok(ExecResult {
                                    columns: Vec::new(),
                                    rows: Vec::new(),
                                    changes: 0,
                                    last_insert_rowid: self.last_insert_rowid,
                                });
                            }
                            Err(e) => {
                                if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                    self.changes = 0;
                                }
                                return Err(e);
                            }
                        }

                        let mut inserted_count = 0i64;
                        let mut pending_autoinc_max: Option<i64> = None;
                        match self.insert_row_with_constraints(
                            table,
                            &dml,
                            target_alias,
                            &mut rowid,
                            &mut row,
                            stmt_conflict,
                            &stmt.upsert,
                            stmt.with.as_ref(),
                            params,
                            &qopts,
                            None,
                            Some(&mut pending_autoinc_max),
                            rowid_guaranteed_new,
                            false,
                        ) {
                            Ok(RowWriteOutcome::Written {
                                final_rowid,
                                was_insert,
                                deleted_conflict,
                                ..
                            }) => {
                                inserted_count = 1;
                                if let Some(rid) = final_rowid {
                                    if let Some(ref mut cur_max) = known_max_rowid {
                                        if cur_max.is_none_or(|m| rid > m) {
                                            *cur_max = Some(rid);
                                        }
                                    }
                                    if was_insert {
                                        self.last_insert_rowid = rid;
                                    }
                                    if table.definition.autoincrement {
                                        let clamped = rid.max(0);
                                        pending_autoinc_max = Some(
                                            pending_autoinc_max
                                                .map_or(clamped, |prev| prev.max(clamped)),
                                        );
                                    }
                                }
                                if !was_insert
                                    || deleted_conflict
                                    || stmt_conflict == Some(ConflictAction::Replace)
                                {
                                    known_max_rowid = None;
                                }
                            }
                            Ok(RowWriteOutcome::Ignored) => {}
                            Err(e) => {
                                if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                    self.changes = 0;
                                }
                                return Err(e);
                            }
                        }
                        if let Some(max_opt) = known_max_rowid {
                            self.set_max_rowid_only(table.root_page, max_opt);
                        } else {
                            self.invalidate_max_rowid_for(table.root_page);
                        }
                        if let Some(max_rid) = pending_autoinc_max {
                            self.update_sqlite_sequence(&table.name, max_rid)?;
                        }
                        self.changes = inserted_count;
                        self.total_changes += inserted_count;
                        return Ok(ExecResult {
                            columns: Vec::new(),
                            rows: Vec::new(),
                            changes: inserted_count,
                            last_insert_rowid: self.last_insert_rowid,
                        });
                    }
                }
            }
        }

        // Evaluate source rows (fast direct VALUES path avoids `live_view()` & `QueryExec`!).
        let source_rows: Vec<Vec<SqlValue>> = if let Some(ref sel) = stmt.select {
            if let Some(vals) = direct_values_fast_path(stmt, sel) {
                let mut eval_env_opt: Option<EvalEnv<'_>> = None;
                let mut rows = Vec::with_capacity(vals.rows.len());
                for vrow in &vals.rows {
                    if vrow.items.len() != target_cols.len() {
                        return Err(SqliteError::Sql(format!(
                            "table {} has {} columns but {} values were supplied",
                            table.name,
                            target_cols.len(),
                            vrow.items.len()
                        )));
                    }
                    let mut r = Vec::with_capacity(vrow.items.len());
                    if let Some(ref m) = qopts.meter {
                        m.charge_steps(vrow.items.len() as u64)?;
                    }
                    for item in &vrow.items {
                        let val = if let Some(sv) = try_eval_simple_const_expr(item, params)? {
                            sv
                        } else {
                            let env = eval_env_opt
                                .get_or_insert_with(|| EvalEnv::new(&qopts, params, &NoSubqueries));
                            eval_expr(item, &[], env)?
                        };
                        r.push(val);
                    }
                    rows.push(r);
                }
                rows
            } else {
                let view = self.live_view()?;
                let mut sel_with = (**sel).clone();
                if stmt.with.is_some() && sel_with.with.is_none() {
                    sel_with.with = stmt.with.clone();
                }
                let qres = self.eval_select_stmt(&view, &sel_with, params, &qopts)?;
                if qres.columns.len() != target_cols.len() {
                    return Err(SqliteError::Sql(format!(
                        "table {} has {} columns but {} values were supplied",
                        table.name,
                        target_cols.len(),
                        qres.columns.len()
                    )));
                }
                qres.rows
            }
        } else {
            // `INSERT INTO t DEFAULT VALUES`
            vec![Vec::new()]
        };

        // Validate UPSERT targets and RETURNING clauses upfront (even when 0 rows or 0 conflicts).
        validate_upsert_targets(table, &stmt.upsert)?;
        let returning_cols = match stmt.returning.as_deref() {
            Some(rc) => returning_column_names(table, rc)?,
            None => Vec::new(),
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);
        let mut autoinc_seq_cache: Option<i64> = if !table.without_rowid && table.definition.autoincrement {
            Some(
                self.get_sqlite_sequence(&table.name)?
                    .map_or(0, |(_, s)| s),
            )
        } else {
            None
        };
        let mut pending_autoinc_max: Option<i64> = None;
        // Track table's current max rowid (seeded from connection cache) so strictly monotonic
        // inserts skip redundant `table_max_rowid` AND `table_contains_rowid` B-tree descents!
        let mut known_max_rowid: Option<Option<i64>> = if table.without_rowid {
            None
        } else {
            self.get_known_max_rowid(table.root_page)
        };

        let mut inserted_count = 0i64;
        let mut returning_rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut targeted_cols = vec![false; table.columns.len()];
        if stmt.select.is_some() {
            for tcol in target_cols {
                if let TargetCol::Col(cid) = *tcol {
                    targeted_cols[cid] = true;
                }
            }
        }
        let mut assigned_cols = vec![false; table.columns.len()];

        for src in source_rows {
            // Build candidate row with defaults (skip DEFAULT expressions for targeted columns).
            let mut row = Vec::with_capacity(table.columns.len());
            for (cid, col) in table.columns.iter().enumerate() {
                if targeted_cols[cid]
                    || col.generated.is_some()
                    || (Some(cid) == table.ipk_column && !table.without_rowid)
                {
                    row.push(SqlValue::Null);
                } else if let Some(ref dflt_expr) = table.definition.columns[cid].default_expr {
                    let v = eval_standalone_expr(dflt_expr, params, &qopts)?;
                    row.push(v);
                } else {
                    row.push(col.default_value.clone());
                }
            }

            let mut ipk_candidate: Option<SqlValue> = None;
            assigned_cols.fill(false);
            for (tcol, val) in target_cols.iter().zip(src.into_iter()) {
                match *tcol {
                    TargetCol::Col(cid) => {
                        if Some(cid) == table.ipk_column && !table.without_rowid {
                            ipk_candidate = Some(val.clone());
                            row[cid] = val;
                        } else if !assigned_cols[cid] {
                            row[cid] = val;
                            assigned_cols[cid] = true;
                        }
                    }
                    TargetCol::Rowid => {
                        ipk_candidate = Some(val);
                    }
                }
            }

            // Determine rowid for rowid tables (and whether it is guaranteed strictly fresh).
            let mut rowid_guaranteed_new = false;
            let mut rowid: Option<i64> = if table.without_rowid {
                None
            } else if let Some(ipk) = table.ipk_column {
                let candidate = ipk_candidate.as_ref().unwrap_or(&row[ipk]);
                if candidate.is_null() {
                    let cur_max = match known_max_rowid {
                        Some(m) => m,
                        None => {
                            let m = table_max_rowid(&self.pager, table.root_page)?;
                            known_max_rowid = Some(m);
                            m
                        }
                    };
                    let next = self.allocate_next_rowid_with_max(table, cur_max, autoinc_seq_cache)?;
                    if cur_max.is_none_or(|m| next > m) {
                        rowid_guaranteed_new = true;
                    }
                    row[ipk] = SqlValue::Int(next);
                    Some(next)
                } else {
                    let rid = coerce_ipk_value(candidate)?;
                    if let Some(cur_max) = known_max_rowid {
                        if cur_max.is_none_or(|m| rid > m) {
                            rowid_guaranteed_new = true;
                        }
                    }
                    row[ipk] = SqlValue::Int(rid);
                    Some(rid)
                }
            } else if let Some(ref er) = ipk_candidate {
                if er.is_null() {
                    let cur_max = match known_max_rowid {
                        Some(m) => m,
                        None => {
                            let m = table_max_rowid(&self.pager, table.root_page)?;
                            known_max_rowid = Some(m);
                            m
                        }
                    };
                    let next = self.allocate_next_rowid_with_max(table, cur_max, autoinc_seq_cache)?;
                    if cur_max.is_none_or(|m| next > m) {
                        rowid_guaranteed_new = true;
                    }
                    Some(next)
                } else {
                    let rid = coerce_ipk_value(er)?;
                    if let Some(cur_max) = known_max_rowid {
                        if cur_max.is_none_or(|m| rid > m) {
                            rowid_guaranteed_new = true;
                        }
                    }
                    Some(rid)
                }
            } else {
                let cur_max = match known_max_rowid {
                    Some(m) => m,
                    None => {
                        let m = table_max_rowid(&self.pager, table.root_page)?;
                        known_max_rowid = Some(m);
                        m
                    }
                };
                let next = self.allocate_next_rowid_with_max(table, cur_max, autoinc_seq_cache)?;
                if cur_max.is_none_or(|m| next > m) {
                    rowid_guaranteed_new = true;
                }
                Some(next)
            };

            // Apply affinities, STRICT checks, evaluate generated columns, NOT NULL, CHECK!
            match self.finalize_and_validate_row(
                table,
                &dml,
                rowid,
                &mut row,
                stmt_conflict,
                params,
                &qopts,
            ) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        if let Some(max_rid) = pending_autoinc_max.take() {
                            let _ = self.update_sqlite_sequence(&table.name, max_rid);
                        }
                        self.changes = inserted_count;
                        self.total_changes += inserted_count;
                    }
                    return Err(e);
                }
            }

            // Check PRIMARY KEY / UNIQUE constraints & handle ON CONFLICT / UPSERT!
            match self.insert_row_with_constraints(
                table,
                &dml,
                target_alias,
                &mut rowid,
                &mut row,
                stmt_conflict,
                &stmt.upsert,
                stmt.with.as_ref(),
                params,
                &qopts,
                None,
                Some(&mut pending_autoinc_max),
                rowid_guaranteed_new,
                stmt.returning.is_some(),
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    was_insert,
                    deleted_conflict,
                }) => {
                    inserted_count += 1;
                    if let Some(rid) = final_rowid {
                        if let Some(ref mut cur_max) = known_max_rowid {
                            if cur_max.is_none_or(|m| rid > m) {
                                *cur_max = Some(rid);
                            }
                        }
                        if was_insert {
                            self.last_insert_rowid = rid;
                        }
                        if table.definition.autoincrement {
                            let clamped = rid.max(0);
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if clamped > *cached {
                                    *cached = clamped;
                                }
                            }
                            pending_autoinc_max = Some(
                                pending_autoinc_max.map_or(clamped, |prev| prev.max(clamped)),
                            );
                        }
                    }
                    // If an OR REPLACE (statement- or schema-level) or UPSERT DO UPDATE modified/deleted rows, reset max cache.
                    if !was_insert || deleted_conflict || stmt_conflict == Some(ConflictAction::Replace) {
                        known_max_rowid = None;
                    }
                    if let Some(ref ret) = stmt.returning {
                        if returning_needs_subq {
                            if let Some(max_rid) = pending_autoinc_max.take() {
                                self.update_sqlite_sequence(&table.name, max_rid)?;
                            }
                        }
                        returning_rows.push(self.eval_returning(
                            table,
                            final_rowid,
                            &final_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?);
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {}
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        if let Some(max_rid) = pending_autoinc_max.take() {
                            let _ = self.update_sqlite_sequence(&table.name, max_rid);
                        }
                        self.changes = inserted_count;
                        self.total_changes += inserted_count;
                    }
                    return Err(e);
                }
            }
        }

        if !table.without_rowid {
            if let Some(max_opt) = known_max_rowid {
                self.set_max_rowid_only(table.root_page, max_opt);
            } else {
                self.invalidate_max_rowid_for(table.root_page);
            }
        }
        if let Some(max_rid) = pending_autoinc_max {
            self.update_sqlite_sequence(&table.name, max_rid)?;
        }

        self.changes = inserted_count;
        self.total_changes += inserted_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: inserted_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    /// Direct batch row insertion for `seed_json`: converts `JsonVal` rows directly into
    /// `self.row_scratch` on the fly (eliminating intermediate `Vec<Vec<SqlValue>>` allocations),
    /// uses monotonic rightmost-leaf table appends + fused 2-int index inserts, and elides the
    /// redundant inner statement savepoint when `seed_json` opened the outer transaction itself.
    /// Returns `Ok(Some(inserted_count))` if handled directly, or `Ok(None)` without mutating
    /// `json_rows` to fall back cleanly to SQL.
    pub(crate) fn try_insert_seed_json_batch_direct(
        &mut self,
        table_name: &str,
        col_names: Option<&[&str]>,
        json_rows: &mut [xdb_sql::functions::json::JsonVal],
        is_objects: bool,
        stmt_conflict: Option<ConflictAction>,
        elide_stmt_savepoint: bool,
    ) -> SqliteResult<Option<usize>> {
        use xdb_sql::functions::json::JsonVal;
        if json_rows.is_empty() {
            return Ok(Some(0));
        }
        let dml = match self.get_dml_state_by_name(table_name) {
            Ok(d) => d,
            Err(_) => return Ok(None),
        };
        let table = &dml.table;
        if table.system
            || table.without_rowid
            || table.columns.len() > 64
            || !dml.gen_order.is_empty()
            || !table.definition.checks.is_empty()
            || dml.any_default_expr
            || table.definition.strict
            || !dml.simple_delete_elidable
        {
            return Ok(None);
        }

        #[derive(Clone, Copy)]
        enum SeedTarget {
            Col(usize),
            Rowid,
        }
        let target_cols: Vec<SeedTarget> = if let Some(cols) = col_names {
            let mut v = Vec::with_capacity(cols.len());
            for &cname in cols {
                if let Some(pos) = table
                    .columns
                    .iter()
                    .position(|tc| tc.name.eq_ignore_ascii_case(cname))
                {
                    if table.columns[pos].generated.is_some() {
                        return Ok(None);
                    }
                    v.push(SeedTarget::Col(pos));
                } else if cname.eq_ignore_ascii_case("rowid")
                    || cname.eq_ignore_ascii_case("_rowid_")
                    || cname.eq_ignore_ascii_case("oid")
                {
                    v.push(SeedTarget::Rowid);
                } else {
                    return Ok(None);
                }
            }
            v
        } else {
            table
                .columns
                .iter()
                .filter(|c| c.generated.is_none())
                .map(|c| SeedTarget::Col(c.cid))
                .collect()
        };

        let expected_len = target_cols.len();
        let all_match = json_rows.iter().all(|r| match (is_objects, r) {
            (true, JsonVal::Object(f)) => f.len() == expected_len,
            (false, JsonVal::Array(a)) => a.len() == expected_len,
            _ => false,
        });
        if !all_match {
            return Ok(None);
        }

        let mut target_col_mask = 0u64;
        for tcol in &target_cols {
            if let SeedTarget::Col(cid) = *tcol {
                target_col_mask |= 1u64 << cid;
            }
        }

        let do_batch = |conn: &mut Self| -> SqliteResult<ExecResult> {
            let table = &dml.table;
            let enc = conn.pager.encoding();
            let fmt = conn.pager.schema_format();
            let autoinc_seq_cache: Option<i64> = if table.definition.autoincrement {
                Some(conn.get_sqlite_sequence(&table.name)?.map_or(0, |(_, s)| s))
            } else {
                None
            };
            let (cached_max_opt, mut hint_leaf) = conn.get_known_max_and_leaf(table.root_page);
            let mut known_max_rowid: Option<Option<i64>> = Some(match cached_max_opt {
                Some(m) => m,
                None => table_max_rowid(&conn.pager, table.root_page)?,
            });
            let mut pending_autoinc_max: Option<i64> = None;
            let mut inserted_count = 0i64;
            let mut lazy_qopts: Option<QueryOptions> = None;
            let mut row = std::mem::take(&mut conn.row_scratch);
            let metered = !conn.pager.is_noop_meter();

            let loop_res: SqliteResult<()> = (|| {
                for src_item in json_rows.iter_mut() {
                    if metered {
                        conn.pager
                            .meter()
                            .charge_steps(expected_len.max(1) as u64)?;
                    }
                    row.clear();
                    row.reserve(table.columns.len());
                    for (cid, col) in table.columns.iter().enumerate() {
                        if (target_col_mask & (1u64 << cid)) != 0
                            || Some(cid) == table.ipk_column
                        {
                            row.push(SqlValue::Null);
                        } else {
                            row.push(col.default_value.clone());
                        }
                    }
                    let mut assigned_mask = 0u64;
                    let mut ipk_candidate: Option<SqlValue> = None;
                    match std::mem::replace(src_item, JsonVal::Null) {
                        JsonVal::Object(fields) => {
                            for (tcol, (_, jv)) in target_cols.iter().zip(fields.into_iter()) {
                                let val = crate::write::seed::seed_json_to_sql_value_owned(jv);
                                match *tcol {
                                    SeedTarget::Col(cid) => {
                                        if Some(cid) == table.ipk_column {
                                            if ipk_candidate.is_none() {
                                                ipk_candidate = Some(val.clone());
                                                row[cid] = val;
                                            }
                                        } else {
                                            let bit = 1u64 << cid;
                                            if (assigned_mask & bit) == 0 {
                                                row[cid] = val;
                                                assigned_mask |= bit;
                                            }
                                        }
                                    }
                                    SeedTarget::Rowid => {
                                        if ipk_candidate.is_none() {
                                            ipk_candidate = Some(val);
                                        }
                                    }
                                }
                            }
                        }
                        JsonVal::Array(cells) => {
                            for (tcol, jv) in target_cols.iter().zip(cells.into_iter()) {
                                let val = crate::write::seed::seed_json_to_sql_value_owned(jv);
                                match *tcol {
                                    SeedTarget::Col(cid) => {
                                        if Some(cid) == table.ipk_column {
                                            if ipk_candidate.is_none() {
                                                ipk_candidate = Some(val.clone());
                                                row[cid] = val;
                                            }
                                        } else {
                                            let bit = 1u64 << cid;
                                            if (assigned_mask & bit) == 0 {
                                                row[cid] = val;
                                                assigned_mask |= bit;
                                            }
                                        }
                                    }
                                    SeedTarget::Rowid => {
                                        if ipk_candidate.is_none() {
                                            ipk_candidate = Some(val);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                    // Apply affinities and verify NOT NULL constraints.
                    let mut need_full_validate = false;
                    for (cid, col) in table.columns.iter().enumerate() {
                        if Some(cid) == table.ipk_column {
                            continue;
                        }
                        let v = std::mem::replace(&mut row[cid], SqlValue::Null);
                        let coerced = apply_affinity(v, col.affinity);
                        if col.not_null && coerced.is_null() {
                            need_full_validate = true;
                        }
                        row[cid] = coerced;
                    }

                    let mut rowid_guaranteed_new = false;
                    let rid = if let Some(ipk) = table.ipk_column {
                        let candidate = ipk_candidate.as_ref().unwrap_or(&row[ipk]);
                        if candidate.is_null() {
                            let cur_max = match known_max_rowid {
                                Some(m) => m,
                                None => {
                                    let m = table_max_rowid(&conn.pager, table.root_page)?;
                                    known_max_rowid = Some(m);
                                    m
                                }
                            };
                            let eff_seq = match (autoinc_seq_cache, pending_autoinc_max) {
                                (Some(a), Some(b)) => Some(a.max(b)),
                                (Some(a), None) => Some(a),
                                (None, b) => b,
                            };
                            let next =
                                conn.allocate_next_rowid_with_max(table, cur_max, eff_seq)?;
                            if cur_max.is_none_or(|m| next > m) {
                                rowid_guaranteed_new = true;
                            }
                            row[ipk] = SqlValue::Int(next);
                            next
                        } else {
                            let r = coerce_ipk_value(candidate)?;
                            if let Some(cur_max) = known_max_rowid {
                                if cur_max.is_none_or(|m| r > m) {
                                    rowid_guaranteed_new = true;
                                }
                            }
                            row[ipk] = SqlValue::Int(r);
                            r
                        }
                    } else if let Some(ref er) = ipk_candidate {
                        if er.is_null() {
                            let cur_max = match known_max_rowid {
                                Some(m) => m,
                                None => {
                                    let m = table_max_rowid(&conn.pager, table.root_page)?;
                                    known_max_rowid = Some(m);
                                    m
                                }
                            };
                            let eff_seq = match (autoinc_seq_cache, pending_autoinc_max) {
                                (Some(a), Some(b)) => Some(a.max(b)),
                                (Some(a), None) => Some(a),
                                (None, b) => b,
                            };
                            let next =
                                conn.allocate_next_rowid_with_max(table, cur_max, eff_seq)?;
                            if cur_max.is_none_or(|m| next > m) {
                                rowid_guaranteed_new = true;
                            }
                            next
                        } else {
                            let r = coerce_ipk_value(er)?;
                            if let Some(cur_max) = known_max_rowid {
                                if cur_max.is_none_or(|m| r > m) {
                                    rowid_guaranteed_new = true;
                                }
                            }
                            r
                        }
                    } else {
                        let cur_max = match known_max_rowid {
                            Some(m) => m,
                            None => {
                                let m = table_max_rowid(&conn.pager, table.root_page)?;
                                known_max_rowid = Some(m);
                                m
                            }
                        };
                        let eff_seq = match (autoinc_seq_cache, pending_autoinc_max) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (Some(a), None) => Some(a),
                            (None, b) => b,
                        };
                        let next = conn.allocate_next_rowid_with_max(table, cur_max, eff_seq)?;
                        if cur_max.is_none_or(|m| next > m) {
                            rowid_guaranteed_new = true;
                        }
                        next
                    };

                    if !need_full_validate && rowid_guaranteed_new && !dml.any_unique_secondary {
                        encode_table_record_into(table, &row, enc, fmt, &mut conn.record_scratch);
                        table_append_monotonic(
                            &mut conn.pager,
                            table.root_page,
                            &mut hint_leaf,
                            rid,
                            &conn.record_scratch,
                        )?;
                        for (ix_idx, ix) in table.indexes.iter().enumerate() {
                            if ix.without_rowid_primary_key {
                                continue;
                            }
                            let ki = dml.index_key_infos[ix_idx]
                                .as_ref()
                                .ok_or_else(|| {
                                    SqliteError::corrupt("missing KeyInfo for secondary index")
                                })?;
                            if ix.columns.len() == 2
                                && ix.columns[0].cid >= 0
                                && ix.columns[1].cid == -1
                            {
                                let v0 = &row[ix.columns[0].cid as usize];
                                if let SqlValue::Int(k0) = *v0 {
                                    if ki.is_two_int_asc() {
                                        index_insert_two_int_asc(
                                            &mut conn.pager,
                                            ix.root_page,
                                            k0,
                                            rid,
                                            fmt,
                                            ki,
                                            false,
                                        )?;
                                        continue;
                                    }
                                    let mut idx_buf = [0u8; 20];
                                    let ilen =
                                        encode_two_int_record_stack(k0, rid, fmt, &mut idx_buf);
                                    let kvals = [SqlValue::Int(k0), SqlValue::Int(rid)];
                                    index_insert_with_values(
                                        &mut conn.pager,
                                        ix.root_page,
                                        &kvals,
                                        &idx_buf[..ilen],
                                        ki,
                                        false,
                                    )?;
                                    continue;
                                }
                                let rid_v = SqlValue::Int(rid);
                                let refs = [v0, &rid_v];
                                encode_record_refs_into(&refs, enc, fmt, &mut conn.record_scratch);
                                let kvals = [v0.clone(), rid_v];
                                index_insert_with_values(
                                    &mut conn.pager,
                                    ix.root_page,
                                    &kvals,
                                    &conn.record_scratch,
                                    ki,
                                    false,
                                )?;
                            } else {
                                let mut kvals = Vec::with_capacity(ix.columns.len());
                                for ic in &ix.columns {
                                    if ic.cid == -1 {
                                        kvals.push(SqlValue::Int(rid));
                                    } else {
                                        kvals.push(row[ic.cid as usize].clone());
                                    }
                                }
                                encode_record_with_format_into(
                                    &kvals,
                                    enc,
                                    fmt,
                                    &mut conn.record_scratch,
                                );
                                index_insert_with_values(
                                    &mut conn.pager,
                                    ix.root_page,
                                    &kvals,
                                    &conn.record_scratch,
                                    ki,
                                    false,
                                )?;
                            }
                        }
                        inserted_count += 1;
                        conn.last_insert_rowid = rid;
                        known_max_rowid = Some(Some(rid));
                        if table.definition.autoincrement {
                            let clamped = rid.max(0);
                            pending_autoinc_max = Some(
                                pending_autoinc_max.map_or(clamped, |prev| prev.max(clamped)),
                            );
                        }
                        continue;
                    }

                    hint_leaf = None;
                    let qopts = lazy_qopts.get_or_insert_with(|| conn.query_options(conn.dialect));
                    if need_full_validate
                        && !conn.finalize_and_validate_row(
                            table,
                            &dml,
                            Some(rid),
                            &mut row,
                            stmt_conflict,
                            &QueryParams::empty(),
                            qopts,
                        )?
                    {
                        continue;
                    }
                    let mut rid_opt = Some(rid);
                    match conn.insert_row_with_constraints(
                        table,
                        &dml,
                        &table.name,
                        &mut rid_opt,
                        &mut row,
                        stmt_conflict,
                        &[],
                        None,
                        &QueryParams::empty(),
                        qopts,
                        None,
                        Some(&mut pending_autoinc_max),
                        rowid_guaranteed_new,
                        false,
                    )? {
                        RowWriteOutcome::Written {
                            final_rowid,
                            was_insert,
                            deleted_conflict,
                            ..
                        } => {
                            inserted_count += 1;
                            if let Some(r) = final_rowid {
                                if was_insert {
                                    conn.last_insert_rowid = r;
                                }
                                if let Some(cur_max) = known_max_rowid {
                                    if cur_max.is_none_or(|m| r > m) {
                                        known_max_rowid = Some(Some(r));
                                    }
                                }
                                if table.definition.autoincrement {
                                    let clamped = r.max(0);
                                    pending_autoinc_max = Some(
                                        pending_autoinc_max
                                            .map_or(clamped, |prev| prev.max(clamped)),
                                    );
                                }
                            }
                            if !was_insert
                                || deleted_conflict
                                || stmt_conflict == Some(ConflictAction::Replace)
                            {
                                known_max_rowid = None;
                            }
                        }
                        RowWriteOutcome::Ignored => {}
                    }
                }
                Ok(())
            })();

            row.clear();
            conn.row_scratch = row;
            if let Err(e) = loop_res {
                if stmt_conflict == Some(ConflictAction::Fail) {
                    if let Some(max_opt) = known_max_rowid {
                        conn.set_max_rowid_only(table.root_page, max_opt);
                    } else {
                        conn.invalidate_max_rowid_for(table.root_page);
                    }
                    if let Some(max_rid) = pending_autoinc_max {
                        let _ = conn.update_sqlite_sequence(&table.name, max_rid);
                    }
                    if inserted_count > 0 {
                        conn.changes = inserted_count;
                        conn.total_changes += inserted_count;
                    }
                }
                return Err(e);
            }

            if let Some(Some(max_rid)) = known_max_rowid {
                conn.set_append_tail(table.root_page, max_rid, hint_leaf);
            } else if let Some(None) = known_max_rowid {
                conn.set_max_rowid_only(table.root_page, None);
            } else {
                conn.invalidate_max_rowid_for(table.root_page);
            }
            if let Some(max_rid) = pending_autoinc_max {
                conn.update_sqlite_sequence(&table.name, max_rid)?;
            }
            conn.changes = inserted_count;
            conn.total_changes += inserted_count;
            Ok(ExecResult {
                columns: Vec::new(),
                rows: Vec::new(),
                changes: inserted_count,
                last_insert_rowid: conn.last_insert_rowid,
            })
        };

        let res = if elide_stmt_savepoint
            && self.pager.in_transaction()
            && self.pager.is_noop_meter()
            && stmt_conflict != Some(ConflictAction::Fail)
        {
            self.run_mutating_stmt_no_savepoint(do_batch)?
        } else {
            self.run_mutating_stmt(do_batch)?
        };
        Ok(Some(res.changes.max(0) as usize))
    }

    /// Zero-allocation streaming insert for newly auto-created UTF-8 shorthand `seed_json` tables
    /// (guaranteed empty, no secondary indexes, no CHECK/STRICT/GENERATED/AUTOINCREMENT/NOT NULL).
    /// Encodes borrowed scalar slices directly into `self.record_scratch` and appends to the leaf.
    pub(super) fn stream_insert_new_seed_table<F>(
        &mut self,
        table_name: &str,
        owned_outer_tx: bool,
        mut drive_rows: F,
    ) -> SqliteResult<usize>
    where
        F: FnMut(
            &mut dyn FnMut(&mut [crate::write::record::BorrowedRecordVal<'_>]) -> SqliteResult<()>,
        ) -> SqliteResult<()>,
    {
        use crate::write::record::{encode_borrowed_record_utf8_into, BorrowedRecordVal};

        let dml = self.get_dml_state_by_name(table_name)?;
        let table = &dml.table;
        debug_assert!(!table.without_rowid && table.indexes.is_empty());
        let ipk_col = table.ipk_column;
        let root_page = table.root_page;
        let n_cols = table.columns.len();
        let mut col_is_real = [false; 16];
        for (cid, col) in table.columns.iter().enumerate().take(16) {
            col_is_real[cid] = col.affinity == Affinity::Real;
        }

        let do_stream = |conn: &mut Self| -> SqliteResult<ExecResult> {
            let fmt = conn.pager.schema_format();
            let metered = !conn.pager.is_noop_meter();
            let mut cur_max: Option<i64> = None;
            let mut hint_leaf: Option<u32> = Some(root_page);
            let mut inserted_count = 0i64;

            let mut emit = |vals: &mut [BorrowedRecordVal<'_>]| -> SqliteResult<()> {
                if metered {
                    conn.pager.meter().charge_steps(n_cols.max(1) as u64)?;
                }
                let rid: i64 = if let Some(ipk) = ipk_col {
                    let v = vals[ipk];
                    vals[ipk] = BorrowedRecordVal::Null;
                    match v {
                        BorrowedRecordVal::Int(r) => r,
                        BorrowedRecordVal::Null => {
                            conn.allocate_next_rowid_with_max(table, cur_max, None)?
                        }
                        _ => {
                            return Err(SqliteError::Sql("datatype mismatch".to_string()));
                        }
                    }
                } else {
                    conn.allocate_next_rowid_with_max(table, cur_max, None)?
                };
                for (cid, v) in vals.iter_mut().enumerate() {
                    if col_is_real[cid] {
                        if let BorrowedRecordVal::Int(i) = *v {
                            *v = BorrowedRecordVal::Real(i as f64);
                        }
                    }
                }
                if cur_max.is_none_or(|m| rid > m) {
                    encode_borrowed_record_utf8_into(vals, fmt, &mut conn.record_scratch);
                    table_append_monotonic(
                        &mut conn.pager,
                        root_page,
                        &mut hint_leaf,
                        rid,
                        &conn.record_scratch,
                    )?;
                    cur_max = Some(rid);
                    inserted_count += 1;
                    conn.last_insert_rowid = rid;
                } else {
                    // Duplicate or out-of-order rowid under default `INSERT OR IGNORE`:
                    hint_leaf = None;
                    if !table_contains_rowid(&conn.pager, root_page, rid)? {
                        encode_borrowed_record_utf8_into(vals, fmt, &mut conn.record_scratch);
                        table_insert(
                            &mut conn.pager,
                            root_page,
                            rid,
                            &conn.record_scratch,
                            false,
                        )?;
                        inserted_count += 1;
                        conn.last_insert_rowid = rid;
                    }
                }
                Ok(())
            };

            drive_rows(&mut emit)?;

            if let Some(m) = cur_max {
                conn.set_append_tail(root_page, m, hint_leaf);
            } else {
                conn.set_max_rowid_only(root_page, None);
            }
            conn.changes = inserted_count;
            conn.total_changes += inserted_count;
            Ok(ExecResult {
                columns: Vec::new(),
                rows: Vec::new(),
                changes: inserted_count,
                last_insert_rowid: conn.last_insert_rowid,
            })
        };

        let res = if owned_outer_tx && self.pager.in_transaction() && self.pager.is_noop_meter() {
            self.run_mutating_stmt_no_savepoint(do_stream)?
        } else {
            self.run_mutating_stmt(do_stream)?
        };
        Ok(res.changes.max(0) as usize)
    }

    // ───────────────────────────── DML: UPDATE ─────────────────────────────

    fn exec_update(
        &mut self,
        stmt: &UpdateStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let dml = self.get_dml_state_by_name(&stmt.table.name.name)?;
        let table = &dml.table;
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }
        if table.name.eq_ignore_ascii_case("sqlite_sequence") {
            self.flush_seq_cache()?;
            self.cached_seq = None;
        }
        validate_set_targets(table, &stmt.set)?;
        let stmt_conflict = stmt.or_conflict;
        let target_alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();

        // Ultra-fast single-descent point UPDATE (`UPDATE t SET col = ? WHERE id = ?` on a
        // non-IPK, non-indexed, non-generated column with no CHECK/STRICT/RETURNING/FROM):
        // splices ONLY the modified column in-place without decoding untouched TEXT/BLOBs or
        // constructing `QueryOptions`!
        if !table.without_rowid
            && stmt.from.is_none()
            && stmt.with.is_none()
            && stmt.returning.is_none()
            && stmt.set.len() == 1
            && stmt.set[0].columns.len() == 1
            && dml.gen_order.is_empty()
            && table.definition.checks.is_empty()
            && !table.definition.strict
            && table.columns.len() <= 64
        {
            if let Some(target_rid) = stmt
                .where_clause
                .as_ref()
                .and_then(|w| extract_fast_exact_rowid_eq_no_qopts(w, table, target_alias, params))
            {
                let cname = &stmt.set[0].columns[0].name;
                if let Some(cid) = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(cname))
                {
                    if Some(cid) != table.ipk_column
                        && (dml.indexed_col_mask & (1u64 << cid)) == 0
                    {
                        if let Some(sidx) = table.columns[cid].storage_index {
                            if let Some(raw_val) =
                                try_eval_simple_const_expr(&stmt.set[0].expr, params)?
                            {
                                let val = apply_affinity(raw_val, table.columns[cid].affinity);
                                if !(table.columns[cid].not_null && val.is_null()) {
                                    if !self.pager.is_noop_meter() {
                                        self.pager.meter().charge_steps(2)?;
                                    }
                                    self.clear_append_leaf_for(table.root_page);
                                    match table_update_single_field(
                                        &mut self.pager,
                                        table.root_page,
                                        target_rid,
                                        sidx,
                                        &val,
                                        &mut self.record_scratch,
                                    )? {
                                        FastFieldUpdateOutcome::Updated => {
                                            self.changes = 1;
                                            self.total_changes += 1;
                                            return Ok(ExecResult {
                                                columns: Vec::new(),
                                                rows: Vec::new(),
                                                changes: 1,
                                                last_insert_rowid: self.last_insert_rowid,
                                            });
                                        }
                                        FastFieldUpdateOutcome::NotFound => {
                                            self.changes = 0;
                                            return Ok(ExecResult {
                                                columns: Vec::new(),
                                                rows: Vec::new(),
                                                changes: 0,
                                                last_insert_rowid: self.last_insert_rowid,
                                            });
                                        }
                                        FastFieldUpdateOutcome::Fallback => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let qopts = self.query_options(dialect);
        let returning_cols = match stmt.returning.as_deref() {
            Some(rc) => returning_column_names(table, rc)?,
            None => Vec::new(),
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);

        type PendingUpdate = (Option<i64>, Vec<SqlValue>, Option<i64>, Vec<SqlValue>);
        let mut updates: Vec<PendingUpdate> = Vec::new();

        // Direct point-rowid fast path (`UPDATE t SET ... WHERE id = ?` with no FROM/WITH/subqueries):
        // bypasses `live_view()`, `SqliteDatabase`, `Pager`, and `CatalogSubqueryRunner` completely!
        let can_direct_point = stmt.from.is_none()
            && stmt.with.is_none()
            && !table.without_rowid
            && stmt
                .where_clause
                .as_ref()
                .is_some_and(|w| !expr_has_subquery_or_intable(w))
            && stmt.set.iter().all(|sc| !expr_has_subquery_or_intable(&sc.expr));
        let direct_rid = if can_direct_point {
            stmt.where_clause
                .as_ref()
                .and_then(|w| extract_direct_rowid_eq(w, table, target_alias, params, &qopts))
        } else {
            None
        };

        if let Some((target_rid, exact_where_only)) = direct_rid {
            if let Some(old_row) = self.fetch_rowid_row_direct(table, target_rid, &qopts)? {
                let fast_applied = if exact_where_only {
                    try_apply_simple_set_clauses(table, &old_row, &stmt.set, params, &qopts)?
                } else {
                    None
                };
                if let Some(new_row) = fast_applied {
                    updates.push((Some(target_rid), old_row, Some(target_rid), new_row));
                } else {
                    let base_scope_cols = build_table_scope_columns(table, target_alias);
                    let eval_env = EvalEnv::new(&qopts, params, &NoSubqueries);
                    let mut full_vals = old_row.clone();
                    let rid_v = SqlValue::Int(target_rid);
                    while full_vals.len() < base_scope_cols.len() {
                        full_vals.push(rid_v.clone());
                    }
                    let scope = [EvalScope::simple(&base_scope_cols, &full_vals)];
                    let matched = if exact_where_only {
                        true
                    } else {
                        match &stmt.where_clause {
                            Some(we) => boolean_value(&eval_expr(we, &scope, &eval_env)?, false),
                            None => true,
                        }
                    };
                    if matched {
                        let (new_rid, new_row) = apply_set_clauses(
                            table,
                            Some(target_rid),
                            &old_row,
                            &stmt.set,
                            &scope,
                            &eval_env,
                        )?;
                        updates.push((Some(target_rid), old_row, new_rid, new_row));
                    }
                }
            }
        } else {
            let view = self.live_view()?;
            // Read candidate rows matching WHERE (using fast point/range/index seek when no FROM!).
            let mut _existing_mem = ConnMemGuard::new(&self.pager);
            let existing_rows = if stmt.from.is_none() {
                self.read_candidate_table_rows(
                    &view,
                    table,
                    target_alias,
                    stmt.where_clause.as_ref(),
                    params,
                    &qopts,
                    &mut _existing_mem,
                )?
            } else {
                self.read_all_table_rows(&view, table, &mut _existing_mem)?
            };

            let from_rows_extra = if let Some(ref from_src) = stmt.from {
                // Expose rowid/_rowid_/oid for single rowid-table FROM sources too.
                let single_from_rowid_alias: Option<(String, Vec<&'static str>)> =
                    if let xdb_parser::FromSource::Table(ft) = from_src {
                        if ft.schema.is_none()
                            || ft
                                .schema
                                .as_ref()
                                .is_some_and(|s| s.name.eq_ignore_ascii_case("main"))
                        {
                            if let Some(ftbl) = view.schema().table(&ft.name.name) {
                                if !ftbl.without_rowid {
                                    let falias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();
                                    let mut synth = Vec::new();
                                    for rname in ["rowid", "_rowid_", "oid"] {
                                        if !ftbl
                                            .columns
                                            .iter()
                                            .any(|c| c.name.eq_ignore_ascii_case(rname))
                                        {
                                            synth.push(rname);
                                        }
                                    }
                                    if !synth.is_empty() {
                                        Some((falias, synth))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                let mut cols_select = vec![ResultColumn::Star(xdb_parser::ResultStar {
                    span: from_src.span(),
                    table: None,
                    exclude: None,
                    replace: None,
                })];
                if let Some((ref falias, ref synth)) = single_from_rowid_alias {
                    let rcol = synth[0];
                    cols_select.push(ResultColumn::Expr(xdb_parser::ResultExpr {
                        span: from_src.span(),
                        expr: Expr::Column(xdb_parser::ColumnRef {
                            span: from_src.span(),
                            schema: None,
                            table: Some(xdb_parser::Name {
                                span: from_src.span(),
                                name: falias.clone(),
                                quote: xdb_parser::QuoteStyle::None,
                            }),
                            column: xdb_parser::Name {
                                span: from_src.span(),
                                name: rcol.to_string(),
                                quote: xdb_parser::QuoteStyle::None,
                            },
                        }),
                        alias: None,
                        text: rcol.to_string(),
                    }));
                }
                let from_select = SelectStmt {
                    span: from_src.span(),
                    with: stmt.with.clone(),
                    body: SelectBody::Core(Box::new(SelectCore {
                        span: from_src.span(),
                        distinct: None,
                        columns: cols_select,
                        from: Some(from_src.clone()),
                        where_clause: None,
                        group_by: None,
                        group_by_all: false,
                        having: None,
                        windows: None,
                        from_first: false,
                    })),
                    order_by: None,
                    order_by_all: None,
                    limit: None,
                };
                let (mut ecols, mut erows) =
                    self.eval_select_stmt_detailed(&view, &from_select, params, &qopts)?;
                if let Some((falias, synth)) = single_from_rowid_alias {
                    // Replace the single appended `rowid` column with all missing rowid aliases.
                    ecols.pop();
                    for rname in &synth {
                        ecols.push(ScopeColumn {
                            table: Some(falias.clone()),
                            name: (*rname).to_string(),
                            affinity: Affinity::Integer,
                            collation: "BINARY".to_string(),
                            is_rowid_alias: true,
                        });
                    }
                    let extra_copies = synth.len().saturating_sub(1);
                    if extra_copies > 0 {
                        for er in &mut erows {
                            if let Some(rv) = er.last().cloned() {
                                for _ in 0..extra_copies {
                                    er.push(rv.clone());
                                }
                            }
                        }
                    }
                }
                Some((ecols, erows))
            } else {
                None
            };

            // Snapshot all rows to update first (SQLite Halloween problem protection!).
            let mut updates_mem = ConnMemGuard::new(&self.pager);
            let base_scope_cols = build_table_scope_columns(table, target_alias);
            let catalog = crate::sql::live_catalog_for(&view, &qopts);
            let sub_runner =
                CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
            let eval_env = EvalEnv::new(&qopts, params, &sub_runner);

            if let Some((ref extra_cols, ref extra_rows)) = from_rows_extra {
                let mut combined_cols = base_scope_cols.clone();
                combined_cols.extend(extra_cols.iter().cloned());

                // Optional O(N+M) hash-join index for `UPDATE t SET ... FROM s WHERE t.k = s.k [AND ...]`
                // when `extra_rows` is non-trivial and keys use BINARY collation.
                let hash_plan = if extra_rows.len() >= 16 {
                    stmt.where_clause.as_ref().and_then(|we| {
                        detect_update_from_equijoin(&base_scope_cols, extra_cols, we)
                    })
                } else {
                    None
                };
                let mut hash_mem = ConnMemGuard::new(&self.pager);
                let hash_buckets: Option<HashMap<Vec<u8>, Vec<usize>>> = match hash_plan {
                    Some((_, ek, aff))
                        if !(aff == Affinity::Text
                            && extra_rows
                                .iter()
                                .any(|r| matches!(r[ek], SqlValue::Int(_) | SqlValue::Real(_)))) =>
                    {
                        hash_mem.reserve((extra_rows.len() as u64).saturating_mul(64))?;
                        let mut m: HashMap<Vec<u8>, Vec<usize>> =
                            HashMap::with_capacity(extra_rows.len());
                        for (eidx, erow) in extra_rows.iter().enumerate() {
                            if let Some(k) = update_join_hash_key(&erow[ek], aff) {
                                hash_mem.reserve((k.len() as u64).saturating_add(16))?;
                                m.entry(k).or_default().push(eidx);
                            }
                        }
                        Some(m)
                    }
                    _ => None,
                };

                for (old_rowid, old_row) in existing_rows {
                    let mut full_vals = old_row.clone();
                    if !table.without_rowid {
                        let rid_v = old_rowid.map_or(SqlValue::Null, SqlValue::Int);
                        while full_vals.len() < base_scope_cols.len() {
                            full_vals.push(rid_v.clone());
                        }
                    }
                    let use_hash_for_row = match (hash_plan, &hash_buckets) {
                        (Some((tk, _, aff)), Some(_)) => {
                            !(aff == Affinity::Text
                                && matches!(full_vals[tk], SqlValue::Int(_) | SqlValue::Real(_)))
                        }
                        _ => false,
                    };
                    if use_hash_for_row {
                        let (tk, _, aff) = hash_plan.expect("checked");
                        let buckets = hash_buckets.as_ref().expect("checked");
                        if let Some(k) = update_join_hash_key(&full_vals[tk], aff) {
                            if let Some(cands) = buckets.get(&k) {
                                for &eidx in cands {
                                    let erow = &extra_rows[eidx];
                                    let mut combined_vals = full_vals.clone();
                                    combined_vals.extend_from_slice(erow);
                                    let scope = [EvalScope::simple(&combined_cols, &combined_vals)];
                                    if let Some(ref we) = stmt.where_clause {
                                        let cond = eval_expr(we, &scope, &eval_env)?;
                                        if !boolean_value(&cond, false) {
                                            continue;
                                        }
                                    }
                                    let (new_rid, new_row) = apply_set_clauses(
                                        table,
                                        old_rowid,
                                        &old_row,
                                        &stmt.set,
                                        &scope,
                                        &eval_env,
                                    )?;
                                    updates_mem.reserve(
                                        estimate_sql_row_bytes(&old_row)
                                            .saturating_add(estimate_sql_row_bytes(&new_row))
                                            .saturating_add(32),
                                    )?;
                                    updates.push((old_rowid, old_row.clone(), new_rid, new_row));
                                    break;
                                }
                            }
                        }
                    } else {
                        for erow in extra_rows {
                            let mut combined_vals = full_vals.clone();
                            combined_vals.extend_from_slice(erow);
                            let scope = [EvalScope::simple(&combined_cols, &combined_vals)];
                            if let Some(ref we) = stmt.where_clause {
                                let cond = eval_expr(we, &scope, &eval_env)?;
                                if !boolean_value(&cond, false) {
                                    continue;
                                }
                            }
                            let (new_rid, new_row) = apply_set_clauses(
                                table,
                                old_rowid,
                                &old_row,
                                &stmt.set,
                                &scope,
                                &eval_env,
                            )?;
                            updates_mem.reserve(
                                estimate_sql_row_bytes(&old_row)
                                    .saturating_add(estimate_sql_row_bytes(&new_row))
                                    .saturating_add(32),
                            )?;
                            updates.push((old_rowid, old_row.clone(), new_rid, new_row));
                            break; // SQLite UPDATE FROM updates each target row at most once
                        }
                    }
                }
            } else {
                for (old_rowid, old_row) in existing_rows {
                    let mut full_vals = old_row.clone();
                    if !table.without_rowid {
                        let rid_v = old_rowid.map_or(SqlValue::Null, SqlValue::Int);
                        while full_vals.len() < base_scope_cols.len() {
                            full_vals.push(rid_v.clone());
                        }
                    }
                    let scope = [EvalScope::simple(&base_scope_cols, &full_vals)];
                    if let Some(ref we) = stmt.where_clause {
                        let cond = eval_expr(we, &scope, &eval_env)?;
                        if !boolean_value(&cond, false) {
                            continue;
                        }
                    }
                    let (new_rid, new_row) = apply_set_clauses(
                        table,
                        old_rowid,
                        &old_row,
                        &stmt.set,
                        &scope,
                        &eval_env,
                    )?;
                    updates_mem.reserve(
                        estimate_sql_row_bytes(&old_row)
                            .saturating_add(estimate_sql_row_bytes(&new_row))
                            .saturating_add(32),
                    )?;
                    updates.push((old_rowid, old_row, new_rid, new_row));
                }
            }
            drop(eval_env);
            drop(sub_runner);
            drop(view);
        }

        if !table.without_rowid {
            if stmt_conflict == Some(ConflictAction::Replace)
                || updates
                    .iter()
                    .any(|(old_rid, _, new_rid, _)| old_rid != new_rid)
            {
                self.invalidate_max_rowid_for(table.root_page);
            } else if !updates.is_empty() {
                self.clear_append_leaf_for(table.root_page);
            }
        }
        let wr_pk_idx = if table.without_rowid {
            table.indexes.iter().find(|ix| ix.without_rowid_primary_key)
        } else {
            None
        };
        let needs_row_savepoint = matches!(
            stmt_conflict,
            Some(ConflictAction::Ignore | ConflictAction::Fail)
        ) || table_has_ignore_or_fail_conflict(table);

        let mut updated_count = 0i64;
        let mut returning_rows = Vec::new();
        let mut replaced = ReplacedTracker::default();
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();

        for (old_rowid, old_row, mut new_rowid, mut new_row) in updates {
            // Skip pending updates whose original target row was already deleted by an earlier
            // `OR REPLACE` conflict resolution in this same UPDATE statement.
            if !table.without_rowid {
                if old_rowid.is_some_and(|r| replaced.rowids.contains(&r)) {
                    continue;
                }
            } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                let old_pk: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                    .iter()
                    .map(|ic| old_row[ic.cid as usize].clone())
                    .collect();
                if replaced.contains_wr_pk(&old_pk, pk_ki) {
                    continue;
                }
            }

            match self.finalize_and_validate_row(
                table,
                &dml,
                new_rowid,
                &mut new_row,
                stmt_conflict,
                params,
                &qopts,
            ) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(e);
                }
            }

            // Fast in-place UPDATE path for rowid tables when rowid did NOT change and NO
            // secondary index entry changed: skips ALL index delete/re-insert & mini-savepoints!
            if !table.without_rowid && new_rowid == old_rowid {
                let mut any_index_changed = false;
                for ix in &table.indexes {
                    if ix.without_rowid_primary_key {
                        continue;
                    }
                    if index_may_have_changed(table, ix, old_rowid, &old_row, &new_row, &qopts, &dml)? {
                        any_index_changed = true;
                        break;
                    }
                }
                if !any_index_changed {
                    let rid = new_rowid.expect("rowid table has rowid");
                    encode_table_record_into(table, &new_row, enc, fmt, &mut self.record_scratch);
                    table_insert(&mut self.pager, table.root_page, rid, &self.record_scratch, true)?;
                    updated_count += 1;
                    if let Some(ref ret) = stmt.returning {
                        returning_rows.push(self.eval_returning(
                            table,
                            new_rowid,
                            &new_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?);
                    }
                    continue;
                }
            }

            if needs_row_savepoint {
                self.pager.push_savepoint(None);
            }

            if let Err(e) = self.delete_row_and_indexes(table, &dml, old_rowid, &old_row, &qopts) {
                if needs_row_savepoint {
                    self.pager.rollback_statement_savepoint();
                }
                return Err(e);
            }

            let mut row_replaced = ReplacedTracker::default();
            match self.insert_row_with_constraints(
                table,
                &dml,
                target_alias,
                &mut new_rowid,
                &mut new_row,
                stmt_conflict,
                &[],
                stmt.with.as_ref(),
                params,
                &qopts,
                Some(&mut row_replaced),
                None,
                false,
                stmt.returning.is_some(),
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    deleted_conflict,
                    ..
                }) => {
                    if needs_row_savepoint {
                        self.pager.commit_statement_savepoint();
                    }
                    if deleted_conflict && !table.without_rowid {
                        self.invalidate_max_rowid_for(table.root_page);
                    }
                    replaced.extend(row_replaced);
                    updated_count += 1;
                    if let Some(ref ret) = stmt.returning {
                        returning_rows.push(self.eval_returning(
                            table,
                            final_rowid,
                            &final_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?);
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {
                    if needs_row_savepoint {
                        self.pager.rollback_statement_savepoint();
                    }
                }
                Err(e) => {
                    if needs_row_savepoint {
                        self.pager.rollback_statement_savepoint();
                    }
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(e);
                }
            }
        }

        self.changes = updated_count;
        self.total_changes += updated_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: updated_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    // ───────────────────────────── DML: DELETE ─────────────────────────────

    fn exec_delete(
        &mut self,
        stmt: &DeleteStmt,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.table.schema.as_ref())?;
        let dml = self.get_dml_state_by_name(&stmt.table.name.name)?;
        let table = &dml.table;
        if table.system && !table.name.eq_ignore_ascii_case("sqlite_sequence") {
            return Err(SqliteError::Sql(format!(
                "table {} may not be modified",
                table.name
            )));
        }
        if table.name.eq_ignore_ascii_case("sqlite_sequence") {
            self.flush_seq_cache()?;
            self.cached_seq = None;
        }

        let alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();

        // Ultra-fast single-descent point DELETE (`DELETE FROM t WHERE id = ?` with no
        // RETURNING/WITH/complex indexes): reads ONLY indexed columns and removes the row in ONE
        // B-tree descent without constructing `QueryOptions`!
        if stmt.with.is_none() && stmt.returning.is_none() && dml.simple_delete_elidable {
            if let Some(target_rid) = stmt
                .where_clause
                .as_ref()
                .and_then(|w| extract_fast_exact_rowid_eq_no_qopts(w, table, alias, params))
            {
                if !self.pager.is_noop_meter() {
                    self.pager.meter().charge_steps(2)?;
                }
                let enc = self.pager.encoding();
                let want = &dml.simple_delete_want_fields;
                let mut stack_dec = [const { SqlValue::Null }; 8];
                let mut heap_dec: Vec<SqlValue> = Vec::new();
                let found_n_fields = if want.len() <= 8 {
                    table_delete_and_read_sparse_fields_into(
                        &mut self.pager,
                        table.root_page,
                        target_rid,
                        enc,
                        want,
                        &mut stack_dec[..want.len()],
                    )?
                } else {
                    heap_dec.resize(want.len(), SqlValue::Null);
                    table_delete_and_read_sparse_fields_into(
                        &mut self.pager,
                        table.root_page,
                        target_rid,
                        enc,
                        want,
                        &mut heap_dec,
                    )?
                };
                if let Some(n_fields) = found_n_fields {
                    let decoded: &[SqlValue] = if want.len() <= 8 {
                        &stack_dec[..want.len()]
                    } else {
                        &heap_dec
                    };
                    self.invalidate_max_rowid_for(table.root_page);
                    if !table.indexes.is_empty() {
                        let get_col_val = |cid: usize| -> SqlValue {
                            if Some(cid) == table.ipk_column {
                                return SqlValue::Int(target_rid);
                            }
                            if let Some(slot) =
                                dml.simple_delete_want_cids.iter().position(|&c| c == cid)
                            {
                                let sidx = dml.simple_delete_want_fields[slot];
                                let raw = if sidx < n_fields {
                                    decoded[slot].clone()
                                } else {
                                    table.columns[cid].default_value.clone()
                                };
                                if table.columns[cid].affinity == Affinity::Real {
                                    crate::schema::real_read_back(raw)
                                } else {
                                    raw
                                }
                            } else {
                                SqlValue::Null
                            }
                        };
                        for (ix_idx, ix) in table.indexes.iter().enumerate() {
                            if ix.without_rowid_primary_key {
                                continue;
                            }
                            let ki = dml.index_key_infos[ix_idx]
                                .as_ref()
                                .ok_or_else(|| {
                                    SqliteError::corrupt("missing KeyInfo for secondary index")
                                })?;
                            if ix.columns.len() == 2
                                && ix.columns[0].cid >= 0
                                && ix.columns[1].cid == -1
                            {
                                let kvals = [
                                    get_col_val(ix.columns[0].cid as usize),
                                    SqlValue::Int(target_rid),
                                ];
                                index_delete(&mut self.pager, ix.root_page, &kvals, ki)?;
                            } else {
                                let mut kvals = Vec::with_capacity(ix.columns.len());
                                for ic in &ix.columns {
                                    if ic.cid == -1 {
                                        kvals.push(SqlValue::Int(target_rid));
                                    } else {
                                        kvals.push(get_col_val(ic.cid as usize));
                                    }
                                }
                                index_delete(&mut self.pager, ix.root_page, &kvals, ki)?;
                            }
                        }
                    }
                    self.changes = 1;
                    self.total_changes += 1;
                    return Ok(ExecResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        changes: 1,
                        last_insert_rowid: self.last_insert_rowid,
                    });
                }
                self.changes = 0;
                return Ok(ExecResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                });
            }
        }

        let qopts = self.query_options(dialect);
        let returning_cols = match stmt.returning.as_deref() {
            Some(rc) => returning_column_names(table, rc)?,
            None => Vec::new(),
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);

        let mut to_delete = Vec::new();
        let can_direct_point = stmt.with.is_none()
            && !table.without_rowid
            && stmt
                .where_clause
                .as_ref()
                .is_some_and(|w| !expr_has_subquery_or_intable(w));
        let direct_rid = if can_direct_point {
            stmt.where_clause
                .as_ref()
                .and_then(|w| extract_direct_rowid_eq(w, table, alias, params, &qopts))
        } else {
            None
        };

        if let Some((target_rid, exact_where_only)) = direct_rid {
            let simple_sparse_delete = exact_where_only
                && stmt.returning.is_none()
                && !table.has_virtual_columns
                && !table
                    .indexes
                    .iter()
                    .any(|ix| ix.definition.where_clause.is_some() || index_needs_expr_scope(ix));
            if simple_sparse_delete {
                // Decode ONLY indexed columns needed to remove index entries (skips unused TEXT/BLOBs!).
                let mut want_fields: Vec<usize> = Vec::new();
                let mut want_cids: Vec<usize> = Vec::new();
                for ix in &table.indexes {
                    for ic in &ix.columns[..ix.key_count] {
                        if ic.cid >= 0 {
                            let cid = ic.cid as usize;
                            if Some(cid) != table.ipk_column {
                                if let Some(sidx) = table.columns[cid].storage_index {
                                    if !want_fields.contains(&sidx) {
                                        want_fields.push(sidx);
                                        want_cids.push(cid);
                                    }
                                }
                            }
                        }
                    }
                }
                let enc = self.pager.encoding();
                let found = if want_fields.is_empty() {
                    table_contains_rowid(&self.pager, table.root_page, target_rid)?
                        .then(|| (Vec::new(), 0))
                } else {
                    table_get_record_sparse_fields(
                        &self.pager,
                        table.root_page,
                        target_rid,
                        enc,
                        &want_fields,
                    )?
                };
                if let Some((mut decoded, n_fields)) = found {
                    let mut row = vec![SqlValue::Null; table.columns.len()];
                    if let Some(ipk) = table.ipk_column {
                        row[ipk] = SqlValue::Int(target_rid);
                    }
                    for (slot, (&cid, &sidx)) in
                        want_cids.iter().zip(want_fields.iter()).enumerate()
                    {
                        let raw = if sidx < n_fields {
                            std::mem::replace(&mut decoded[slot], SqlValue::Null)
                        } else {
                            table.columns[cid].default_value.clone()
                        };
                        row[cid] = if table.columns[cid].affinity == Affinity::Real {
                            crate::schema::real_read_back(raw)
                        } else {
                            raw
                        };
                    }
                    to_delete.push((Some(target_rid), row));
                }
            } else if let Some(row) = self.fetch_rowid_row_direct(table, target_rid, &qopts)? {
                let matched = if exact_where_only {
                    true
                } else {
                    let scope_cols = build_table_scope_columns(table, alias);
                    let eval_env = EvalEnv::new(&qopts, params, &NoSubqueries);
                    let mut vals = row.clone();
                    let rid_v = SqlValue::Int(target_rid);
                    while vals.len() < scope_cols.len() {
                        vals.push(rid_v.clone());
                    }
                    let scope = [EvalScope::simple(&scope_cols, &vals)];
                    match &stmt.where_clause {
                        Some(we) => boolean_value(&eval_expr(we, &scope, &eval_env)?, false),
                        None => true,
                    }
                };
                if matched {
                    to_delete.push((Some(target_rid), row));
                }
            }
        } else {
            let view = self.live_view()?;
            let mut _existing_mem = ConnMemGuard::new(&self.pager);
            let existing_rows = self.read_candidate_table_rows(
                &view,
                table,
                alias,
                stmt.where_clause.as_ref(),
                params,
                &qopts,
                &mut _existing_mem,
            )?;
            let scope_cols = build_table_scope_columns(table, alias);
            let catalog = crate::sql::live_catalog_for(&view, &qopts);
            let sub_runner =
                CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
            let eval_env = EvalEnv::new(&qopts, params, &sub_runner);

            for (rid, row) in existing_rows {
                if let Some(ref we) = stmt.where_clause {
                    let mut vals = row.clone();
                    if !table.without_rowid {
                        let rid_v = rid.map_or(SqlValue::Null, SqlValue::Int);
                        while vals.len() < scope_cols.len() {
                            vals.push(rid_v.clone());
                        }
                    }
                    let scope = [EvalScope::simple(&scope_cols, &vals)];
                    let cond = eval_expr(we, &scope, &eval_env)?;
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }
                to_delete.push((rid, row));
            }
            drop(eval_env);
            drop(sub_runner);
            drop(view);
        }

        if !table.without_rowid && !to_delete.is_empty() {
            self.invalidate_max_rowid_for(table.root_page);
        }
        let mut deleted_count = 0i64;
        let mut returning_rows = Vec::new();

        for (rid, row) in to_delete {
            self.delete_row_and_indexes(table, &dml, rid, &row, &qopts)?;
            deleted_count += 1;
            if let Some(ref ret) = stmt.returning {
                returning_rows.push(self.eval_returning(
                    table,
                    rid,
                    &row,
                    ret,
                    stmt.with.as_ref(),
                    params,
                    dialect,
                    returning_needs_subq,
                )?);
            }
        }

        self.changes = deleted_count;
        self.total_changes += deleted_count;
        Ok(ExecResult {
            columns: returning_cols,
            rows: returning_rows,
            changes: deleted_count,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    // ───────────────────────────── Row & Index write primitives ─────────────────────────────

    fn allocate_next_rowid_with_max(
        &mut self,
        table: &TableInfo,
        cur_max: Option<i64>,
        autoinc_seq_cache: Option<i64>,
    ) -> SqliteResult<i64> {
        let mut max_id = cur_max.unwrap_or(0);
        if table.definition.autoincrement {
            max_id = max_id.max(0);
            let seq = match autoinc_seq_cache {
                Some(s) => Some(s),
                None => self.get_sqlite_sequence(&table.name)?.map(|(_, s)| s),
            };
            if let Some(seq) = seq {
                if seq > max_id {
                    max_id = seq;
                }
            }
            return max_id
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".to_string()));
        }
        if let Some(next) = max_id.checked_add(1) {
            return Ok(next);
        }
        // Non-AUTOINCREMENT table has a row with rowid == i64::MAX: probe up to 100
        // pseudorandom positive rowids (matching SQLite's OP_NewRowid algorithm).
        let mut seed: u64 = (u64::from(table.root_page) << 32)
            ^ u64::from(self.pager.page_count())
            ^ (self.total_changes as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ 0x243f_6a88_85a3_08d3;
        for _ in 0..100 {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            let cand = (z as i64) & i64::MAX;
            if cand == 0 {
                continue;
            }
            if !table_contains_rowid(&self.pager, table.root_page, cand)? {
                return Ok(cand);
            }
        }
        Err(SqliteError::Sql("database or disk is full".to_string()))
    }

    /// Returns `Ok(true)` if the row is valid, `Ok(false)` if a `NOT NULL` or `CHECK`
    /// constraint was violated under `ConflictAction::Ignore`, or `Err` on `STRICT`
    /// datatype errors or non-ignored constraint failures.
    #[allow(clippy::too_many_arguments)]
    fn finalize_and_validate_row(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        rowid: Option<i64>,
        row: &mut [SqlValue],
        stmt_conflict: Option<ConflictAction>,
        params: &QueryParams,
        qopts: &QueryOptions,
    ) -> SqliteResult<bool> {
        // 1. Apply column affinity & STRICT type validation on non-generated columns.
        // Note: SQLite ALWAYS rejects STRICT datatype mismatches even under `OR IGNORE`.
        for (cid, col) in table.columns.iter().enumerate() {
            if col.generated.is_some() {
                continue;
            }
            if table.strict {
                let raw = std::mem::replace(&mut row[cid], SqlValue::Null);
                row[cid] =
                    validate_strict_value(raw, &col.decl_type, &table.name, &col.name)?;
            } else {
                // Fast-path no-op affinities without moving/cloning `Text`/`Blob` values.
                let needs_conv = !matches!(
                    (&row[cid], col.affinity),
                    (SqlValue::Null, _)
                        | (_, Affinity::Blob | Affinity::None)
                        | (SqlValue::Int(_), Affinity::Integer | Affinity::Numeric)
                        | (SqlValue::Text(_), Affinity::Text)
                        | (SqlValue::Blob(_), _)
                );
                if needs_conv {
                    let raw = std::mem::replace(&mut row[cid], SqlValue::Null);
                    row[cid] = apply_affinity(raw, col.affinity);
                }
            }
        }

        // 2. Evaluate STORED & VIRTUAL generated columns in topological dependency order
        //    (detecting cycles and enforcing STRICT type validation on generated columns too).
        if !dml.gen_order.is_empty() {
            for &cid in &dml.gen_order {
                if let Some(ref expr) = table.definition.columns[cid].generated {
                    row[cid] = eval_generated_column_value(
                        table,
                        cid,
                        expr,
                        &dml.gen_scope_cols,
                        row,
                        qopts,
                    )?;
                }
            }
        }

        // 3. NOT NULL constraints (including WITHOUT ROWID PK columns & ON CONFLICT REPLACE DEFAULT!).
        let mut substituted_default = false;
        for (cid, col) in table.columns.iter().enumerate() {
            if (col.not_null || (table.without_rowid && col.pk > 0)) && row[cid].is_null() {
                let eff = resolve_conflict(stmt_conflict, table.definition.columns[cid].not_null_on_error);
                match eff {
                    ConflictAction::Ignore => return Ok(false),
                    ConflictAction::Replace => {
                        if col.generated.is_none() {
                            let dflt_val = if let Some(ref dflt_expr) =
                                table.definition.columns[cid].default_expr
                            {
                                eval_standalone_expr(dflt_expr, params, qopts)?
                            } else {
                                col.default_value.clone()
                            };
                            if !dflt_val.is_null() {
                                let coerced = if table.strict {
                                    validate_strict_value(
                                        dflt_val,
                                        &col.decl_type,
                                        &table.name,
                                        &col.name,
                                    )?
                                } else {
                                    apply_affinity(dflt_val, col.affinity)
                                };
                                if !coerced.is_null() {
                                    row[cid] = coerced;
                                    substituted_default = true;
                                    continue;
                                }
                            }
                        }
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                        ));
                    }
                    ConflictAction::Abort | ConflictAction::Fail | ConflictAction::Rollback => {
                        return Err(self.constraint_err(
                            eff,
                            format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                        ));
                    }
                }
            }
        }
        // Re-evaluate generated columns (and re-check their NOT NULL constraints) if any
        // non-generated column received a default substitution via ON CONFLICT REPLACE.
        if substituted_default && !dml.gen_order.is_empty() {
            for &cid in &dml.gen_order {
                if let Some(ref expr) = table.definition.columns[cid].generated {
                    row[cid] = eval_generated_column_value(
                        table,
                        cid,
                        expr,
                        &dml.gen_scope_cols,
                        row,
                        qopts,
                    )?;
                }
            }
            for &cid in &dml.gen_order {
                let col = &table.columns[cid];
                if col.not_null && row[cid].is_null() {
                    let eff = resolve_conflict(
                        stmt_conflict,
                        table.definition.columns[cid].not_null_on_error,
                    );
                    if eff == ConflictAction::Ignore {
                        return Ok(false);
                    }
                    let err_eff = if eff == ConflictAction::Replace {
                        ConflictAction::Abort
                    } else {
                        eff
                    };
                    return Err(self.constraint_err(
                        err_eff,
                        format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                    ));
                }
            }
        }

        // 4. CHECK constraints (`NULL` passes CHECK in SQLite, only explicit false `0` fails!).
        //    Include synthetic `rowid`/`_rowid_`/`oid` aliases in CHECK scope for rowid tables.
        if !table.definition.checks.is_empty() {
            let mut check_vals = row.to_vec();
            if !table.without_rowid {
                let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
                while check_vals.len() < dml.check_scope_cols.len() {
                    check_vals.push(rid_v.clone());
                }
            }
            let no_sub = NoSubqueries;
            let empty_params = QueryParams::empty();
            let mut env = EvalEnv::new(qopts, &empty_params, &no_sub);
            env.dqs = true;
            let scope = [EvalScope::simple(&dml.check_scope_cols, &check_vals)];
            let eff = stmt_conflict.unwrap_or(ConflictAction::Abort);
            for chk in &table.definition.checks {
                let res = eval_expr(&chk.expr, &scope, &env)?;
                if !res.is_null() && !boolean_value(&res, true) {
                    if eff == ConflictAction::Ignore {
                        return Ok(false);
                    }
                    let cname = if chk.name.is_empty() {
                        table.name.as_str()
                    } else {
                        chk.name.as_str()
                    };
                    let err_eff = if eff == ConflictAction::Replace {
                        ConflictAction::Abort
                    } else {
                        eff
                    };
                    return Err(self.constraint_err(
                        err_eff,
                        format!("CHECK constraint failed: {cname}"),
                    ));
                }
            }
        }

        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row_with_constraints(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        target_alias: &str,
        rowid: &mut Option<i64>,
        row: &mut [SqlValue],
        stmt_conflict: Option<ConflictAction>,
        upserts: &[xdb_parser::UpsertClause],
        with_clause: Option<&xdb_parser::WithClause>,
        params: &QueryParams,
        qopts: &QueryOptions,
        mut replaced: Option<&mut ReplacedTracker>,
        mut pending_autoinc_max: Option<&mut Option<i64>>,
        rowid_guaranteed_new: bool,
        need_returning: bool,
    ) -> SqliteResult<RowWriteOutcome> {
        // Detect any conflict (rowid PK, WITHOUT ROWID PK, or UNIQUE secondary index),
        // retaining each constraint's schema-level `OnError` action.
        let mut conflicting_rows: Vec<(Option<i64>, Vec<SqlValue>, String, OnError)> = Vec::new();

        let wr_pk_idx = if table.without_rowid {
            table.indexes.iter().find(|ix| ix.without_rowid_primary_key)
        } else {
            None
        };

        if !table.without_rowid {
            let rid = rowid.expect("rowid table has rowid");
            if !rowid_guaranteed_new && table_contains_rowid(&self.pager, table.root_page, rid)? {
                let old_row = self.fetch_rowid_row(table, rid, qopts)?;
                let col_label = table
                    .ipk_column
                    .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                    .unwrap_or_else(|| format!("{}.rowid", table.name));
                conflicting_rows.push((
                    Some(rid),
                    old_row,
                    col_label,
                    table.definition.ipk_on_error,
                ));
            }
        } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
            // WITHOUT ROWID primary key index shares `table.root_page`!
            let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| row[ic.cid as usize].clone())
                .collect();
            if let Some(hit_entry) =
                index_contains_prefix(&self.pager, table.root_page, &pk_vals, ki)?
            {
                let old_row = decode_without_rowid_full_row(table, &hit_entry, qopts)?;
                conflicting_rows.push((
                    None,
                    old_row,
                    format!("{}.PRIMARY KEY", table.name),
                    pk_idx.definition.on_error,
                ));
            }
        }

        // Check UNIQUE secondary indexes
        for (ix_idx, ix) in table.indexes.iter().enumerate() {
            if !ix.unique || ix.without_rowid_primary_key {
                continue;
            }
            let Some(entry_vals) = build_index_entry_values_fast(
                table,
                ix,
                *rowid,
                row,
                qopts,
                &dml.index_expr_scope_cols,
                &dml.index_where_scope_cols,
            )?
            else {
                continue;
            };
            let key_prefix = &entry_vals[..ix.key_count];
            if prefix_has_null(key_prefix) {
                continue; // NULLs are always distinct in SQLite UNIQUE indexes
            }
            let ki = dml.index_key_infos[ix_idx]
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
            if let Some(hit_entry) =
                index_contains_prefix(&self.pager, ix.root_page, key_prefix, ki)?
            {
                if !table.without_rowid {
                    let hit_rid = hit_entry.last().and_then(SqlValue::as_i64);
                    if let Some(hrid) = hit_rid {
                        let old_row = self.fetch_rowid_row(table, hrid, qopts)?;
                        conflicting_rows.push((
                            Some(hrid),
                            old_row,
                            format!("{}.{}", table.name, ix.name),
                            ix.definition.on_error,
                        ));
                    } else {
                        conflicting_rows.push((
                            None,
                            Vec::new(),
                            format!("{}.{}", table.name, ix.name),
                            ix.definition.on_error,
                        ));
                    }
                } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                    let mut pk_vals = Vec::with_capacity(pk_idx.key_count);
                    let mut complete = true;
                    for pk_col in &pk_idx.columns[..pk_idx.key_count] {
                        if let Some(pos) = ix.columns.iter().position(|ic| ic.cid == pk_col.cid) {
                            if let Some(v) = hit_entry.get(pos) {
                                pk_vals.push(v.clone());
                                continue;
                            }
                        }
                        complete = false;
                        break;
                    }
                    let old_row = if complete {
                        if let Some(full_entry) =
                            index_contains_prefix(&self.pager, table.root_page, &pk_vals, pk_ki)?
                        {
                            decode_without_rowid_full_row(table, &full_entry, qopts)?
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };
                    conflicting_rows.push((
                        None,
                        old_row,
                        format!("{}.{}", table.name, ix.name),
                        ix.definition.on_error,
                    ));
                } else {
                    conflicting_rows.push((
                        None,
                        Vec::new(),
                        format!("{}.{}", table.name, ix.name),
                        ix.definition.on_error,
                    ));
                }
            }
        }

        let mut deleted_conflict = false;
        if !conflicting_rows.is_empty() {
            // Check UPSERT (`ON CONFLICT ...`) first!
            if !upserts.is_empty() {
                for up in upserts {
                    if let Some(cidx) = upsert_target_matches(table, up, &conflicting_rows) {
                        match up.action {
                            UpsertAction::Nothing => return Ok(RowWriteOutcome::Ignored),
                            UpsertAction::Update => {
                                let (old_rid, old_row, _, _) = conflicting_rows.remove(cidx);
                                let mut upd_row = old_row.clone();
                                let mut upd_rid = old_rid;
                                let scope_cols = build_upsert_scope_columns(table, target_alias);
                                let mut scope_vals = old_row.clone();
                                if !table.without_rowid {
                                    let rv = old_rid.map_or(SqlValue::Null, SqlValue::Int);
                                    for _ in 0..count_synth_rowid_aliases(table) {
                                        scope_vals.push(rv.clone());
                                    }
                                }
                                let base_len = scope_vals.len();
                                // Append `excluded.*` values (hidden from unqualified resolution so
                                // unqualified column names resolve to the existing target table row!).
                                scope_vals.extend(row.iter().cloned());
                                let mut hidden_unq = Vec::with_capacity(scope_cols.len());
                                hidden_unq.resize(base_len, false);
                                hidden_unq.resize(scope_cols.len(), true);

                                let should_update = {
                                    // Flush deferred `sqlite_sequence` updates before UPSERT DO UPDATE
                                    // evaluates WHERE / SET subqueries against `self.live_view()`.
                                    if let Some(max_rid) =
                                        pending_autoinc_max.as_deref_mut().and_then(Option::take)
                                    {
                                        self.update_sqlite_sequence(&table.name, max_rid)?;
                                    }
                                    let view = self.live_view()?;
                                    let catalog = crate::sql::live_catalog_for(&view, qopts);
                                    let sub_runner = CatalogSubqueryRunner::with_ctes(
                                        &catalog,
                                        qopts,
                                        params,
                                        with_clause,
                                    )?;
                                    let eval_env = EvalEnv::new(qopts, params, &sub_runner);
                                    let mut sc = EvalScope::simple(&scope_cols, &scope_vals);
                                    sc.hidden_unqualified = Some(&hidden_unq);
                                    let scope = [sc];

                                    if let Some(ref we) = up.where_clause {
                                        let cond = eval_expr(we, &scope, &eval_env)?;
                                        if !boolean_value(&cond, false) {
                                            false
                                        } else {
                                            if let Some(ref sets) = up.set {
                                                let (nr, nrow) = apply_set_clauses(
                                                    table,
                                                    old_rid,
                                                    &old_row,
                                                    sets,
                                                    &scope,
                                                    &eval_env,
                                                )?;
                                                upd_rid = nr;
                                                upd_row = nrow;
                                            }
                                            true
                                        }
                                    } else {
                                        if let Some(ref sets) = up.set {
                                            let (nr, nrow) = apply_set_clauses(
                                                table,
                                                old_rid,
                                                &old_row,
                                                sets,
                                                &scope,
                                                &eval_env,
                                            )?;
                                            upd_rid = nr;
                                            upd_row = nrow;
                                        }
                                        true
                                    }
                                };
                                if !should_update {
                                    return Ok(RowWriteOutcome::Ignored);
                                }
                                if !self.finalize_and_validate_row(
                                    table,
                                    dml,
                                    upd_rid,
                                    &mut upd_row,
                                    stmt_conflict,
                                    params,
                                    qopts,
                                )? {
                                    return Ok(RowWriteOutcome::Ignored);
                                }
                                self.pager.push_savepoint(None);
                                if let Err(e) =
                                    self.delete_row_and_indexes(table, dml, old_rid, &old_row, qopts)
                                {
                                    self.pager.rollback_statement_savepoint();
                                    return Err(e);
                                }
                                // Re-check all remaining PK/UNIQUE constraints on the updated row!
                                match self.insert_row_with_constraints(
                                    table,
                                    dml,
                                    target_alias,
                                    &mut upd_rid,
                                    &mut upd_row,
                                    stmt_conflict,
                                    &[],
                                    with_clause,
                                    params,
                                    qopts,
                                    None,
                                    None,
                                    false,
                                    need_returning,
                                ) {
                                    Ok(RowWriteOutcome::Written {
                                        final_rowid,
                                        final_row,
                                        deleted_conflict,
                                        ..
                                    }) => {
                                        self.pager.commit_statement_savepoint();
                                        return Ok(RowWriteOutcome::Written {
                                            final_rowid,
                                            final_row,
                                            was_insert: false,
                                            deleted_conflict,
                                        });
                                    }
                                    Ok(RowWriteOutcome::Ignored) => {
                                        self.pager.rollback_statement_savepoint();
                                        return Ok(RowWriteOutcome::Ignored);
                                    }
                                    Err(e) => {
                                        self.pager.rollback_statement_savepoint();
                                        return Err(e);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Resolve non-UPSERT conflicts in order using statement-level or schema-level ON CONFLICT.
            for (_, _, label, on_err) in &conflicting_rows {
                let eff = resolve_conflict(stmt_conflict, *on_err);
                match eff {
                    ConflictAction::Ignore => return Ok(RowWriteOutcome::Ignored),
                    ConflictAction::Replace => {}
                    ConflictAction::Abort | ConflictAction::Fail | ConflictAction::Rollback => {
                        return Err(self.constraint_err(
                            eff,
                            format!("UNIQUE constraint failed: {label}"),
                        ));
                    }
                }
            }

            // All conflicts resolve to REPLACE: delete each distinct conflicting row once.
            let mut deleted_rids: HashSet<i64> = HashSet::new();
            let mut deleted_wr_pks: Vec<Vec<SqlValue>> = Vec::new();
            for (crid, crow, _, _) in conflicting_rows {
                if crow.is_empty() {
                    continue;
                }
                if !table.without_rowid {
                    if let Some(rid) = crid {
                        if !deleted_rids.insert(rid) {
                            continue;
                        }
                        self.delete_row_and_indexes(table, dml, Some(rid), &crow, qopts)?;
                        deleted_conflict = true;
                        if let Some(ref mut tr) = replaced {
                            tr.rowids.insert(rid);
                        }
                    }
                } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                    let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                        .iter()
                        .map(|ic| crow[ic.cid as usize].clone())
                        .collect();
                    if deleted_wr_pks
                        .iter()
                        .any(|p| key_tuples_equal(p, &pk_vals, pk_ki))
                    {
                        continue;
                    }
                    self.delete_row_and_indexes(table, dml, None, &crow, qopts)?;
                    deleted_conflict = true;
                    deleted_wr_pks.push(pk_vals.clone());
                    if let Some(ref mut tr) = replaced {
                        tr.wr_pks.push(pk_vals);
                    }
                }
            }
        }

        self.write_row_and_indexes(table, dml, *rowid, row, qopts)?;
        Ok(RowWriteOutcome::Written {
            final_rowid: *rowid,
            final_row: if need_returning {
                row.to_owned()
            } else {
                Vec::new()
            },
            was_insert: true,
            deleted_conflict,
        })
    }

    fn write_row_and_indexes(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        rowid: Option<i64>,
        row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();

        if table.without_rowid {
            let ki = dml
                .wr_pk_ki
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("WITHOUT ROWID table missing PK index"))?;
            let rec_vals = build_without_rowid_record_values(table, row);
            encode_record_with_format_into(&rec_vals, enc, fmt, &mut self.record_scratch);
            index_insert_with_values(
                &mut self.pager,
                table.root_page,
                &rec_vals,
                &self.record_scratch,
                ki,
                true,
            )?;
        } else {
            let rid = rowid.expect("rowid table has rowid");
            encode_table_record_into(table, row, enc, fmt, &mut self.record_scratch);
            table_insert(&mut self.pager, table.root_page, rid, &self.record_scratch, true)?;
        }

        for (ix_idx, ix) in table.indexes.iter().enumerate() {
            if ix.without_rowid_primary_key {
                continue;
            }
            let ki = dml.index_key_infos[ix_idx]
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
            if !table.without_rowid
                && ix.definition.where_clause.is_none()
                && ix.columns.len() == 2
                && ix.columns[0].cid >= 0
                && ix.columns[1].cid == -1
                && !index_needs_expr_scope(ix)
            {
                let rid_val = rowid.map_or(SqlValue::Null, SqlValue::Int);
                let k0 = &row[ix.columns[0].cid as usize];
                let refs = [k0, &rid_val];
                encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
                let stack_vals = [k0.clone(), rid_val];
                index_insert_with_values(
                    &mut self.pager,
                    ix.root_page,
                    &stack_vals,
                    &self.record_scratch,
                    ki,
                    true,
                )?;
                continue;
            }
            if let Some(entry_vals) = build_index_entry_values_fast(
                table,
                ix,
                rowid,
                row,
                qopts,
                &dml.index_expr_scope_cols,
                &dml.index_where_scope_cols,
            )? {
                encode_record_with_format_into(&entry_vals, enc, fmt, &mut self.record_scratch);
                index_insert_with_values(
                    &mut self.pager,
                    ix.root_page,
                    &entry_vals,
                    &self.record_scratch,
                    ki,
                    true,
                )?;
            }
        }
        Ok(())
    }

    fn delete_row_and_indexes(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        rowid: Option<i64>,
        row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        for (ix_idx, ix) in table.indexes.iter().enumerate() {
            if ix.without_rowid_primary_key {
                continue;
            }
            let ki = dml.index_key_infos[ix_idx]
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
            if !table.without_rowid
                && ix.definition.where_clause.is_none()
                && ix.columns.len() == 2
                && ix.columns[0].cid >= 0
                && ix.columns[1].cid == -1
                && !index_needs_expr_scope(ix)
            {
                let stack_vals = [
                    row[ix.columns[0].cid as usize].clone(),
                    rowid.map_or(SqlValue::Null, SqlValue::Int),
                ];
                index_delete(&mut self.pager, ix.root_page, &stack_vals, ki)?;
                continue;
            }
            if let Some(entry_vals) = build_index_entry_values_fast(
                table,
                ix,
                rowid,
                row,
                qopts,
                &dml.index_expr_scope_cols,
                &dml.index_where_scope_cols,
            )? {
                index_delete(&mut self.pager, ix.root_page, &entry_vals, ki)?;
            }
        }

        if table.without_rowid {
            let ki = dml
                .wr_pk_ki
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("WITHOUT ROWID table missing PK index"))?;
            let rec_vals = build_without_rowid_record_values(table, row);
            index_delete(&mut self.pager, table.root_page, &rec_vals, ki)?;
        } else {
            let rid = rowid.expect("rowid table has rowid");
            table_delete(&mut self.pager, table.root_page, rid)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_returning(
        &self,
        table: &TableInfo,
        rowid: Option<i64>,
        row: &[SqlValue],
        ret: &[ResultColumn],
        with_clause: Option<&xdb_parser::WithClause>,
        params: &QueryParams,
        dialect: Dialect,
        needs_subq: bool,
    ) -> SqliteResult<Vec<SqlValue>> {
        let qopts = self.query_options(dialect);
        let scope_cols = build_table_scope_columns(table, &table.name);
        let mut vals = row.to_vec();
        if !table.without_rowid {
            let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
            while vals.len() < scope_cols.len() {
                vals.push(rid_v.clone());
            }
        }
        let scope = [EvalScope::simple(&scope_cols, &vals)];
        let mut out = Vec::new();
        if needs_subq {
            let view = self.live_view()?;
            let catalog = crate::sql::live_catalog_for(&view, &qopts);
            let sub_runner =
                CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, with_clause)?;
            let env = EvalEnv::new(&qopts, params, &sub_runner);
            for rc in ret {
                match rc {
                    ResultColumn::Star(_) => out.extend_from_slice(row),
                    ResultColumn::Expr(re) => out.push(eval_expr(&re.expr, &scope, &env)?),
                }
            }
        } else {
            let no_sub = NoSubqueries;
            let env = EvalEnv::new(&qopts, params, &no_sub);
            for rc in ret {
                match rc {
                    ResultColumn::Star(_) => out.extend_from_slice(row),
                    ResultColumn::Expr(re) => out.push(eval_expr(&re.expr, &scope, &env)?),
                }
            }
        }
        Ok(out)
    }

    fn fetch_rowid_row_direct(
        &self,
        table: &TableInfo,
        rowid: i64,
        qopts: &QueryOptions,
    ) -> SqliteResult<Option<Vec<SqlValue>>> {
        let enc = self.pager.encoding();
        let Some(rec_vals) = table_get_record_fields(&self.pager, table.root_page, rowid, enc)?
        else {
            return Ok(None);
        };
        let mut full = Vec::with_capacity(table.columns.len());
        let mut rec_idx = 0usize;
        for (cid, col) in table.columns.iter().enumerate() {
            if col.hidden == ColumnHiddenKind::Virtual {
                full.push(SqlValue::Null);
            } else if Some(cid) == table.ipk_column {
                rec_idx += 1;
                full.push(SqlValue::Int(rowid));
            } else {
                let raw = rec_vals
                    .get(rec_idx)
                    .cloned()
                    .unwrap_or_else(|| col.default_value.clone());
                rec_idx += 1;
                let v = if col.affinity == Affinity::Real {
                    if let SqlValue::Int(i) = raw {
                        SqlValue::Real(i as f64)
                    } else {
                        raw
                    }
                } else {
                    raw
                };
                full.push(v);
            }
        }
        if table.has_virtual_columns {
            eval_virtual_columns(table, &mut full, qopts)?;
        }
        Ok(Some(full))
    }

    fn fetch_rowid_row(
        &self,
        table: &TableInfo,
        rowid: i64,
        qopts: &QueryOptions,
    ) -> SqliteResult<Vec<SqlValue>> {
        self.fetch_rowid_row_direct(table, rowid, qopts)?
            .ok_or_else(|| SqliteError::corrupt(format!("missing rowid {rowid}")))
    }

    fn read_all_table_rows(
        &self,
        view: &SqliteDatabase,
        table: &TableInfo,
        mem: &mut ConnMemGuard,
    ) -> SqliteResult<Vec<(Option<i64>, Vec<SqlValue>)>> {
        let reader = view.table(&table.name)?;
        let stored_cids: Vec<usize> = table
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let has_rowid = !table.without_rowid;
        let full_stored_identity = !table.has_virtual_columns && stored_cids.len() == table.columns.len();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let mut cur = reader.cursor(Some(&scan_opts))?;
        let qopts = self.query_options(self.dialect);
        let virt_plan = if table.has_virtual_columns {
            Some((
                generated_eval_order_from_defs(&table.name, &table.definition.columns)?,
                build_plain_scope_columns(table),
            ))
        } else {
            None
        };
        let mut out = Vec::new();
        while cur.next()? {
            let rid = if has_rowid { Some(cur.rowid()?) } else { None };
            let raw = cur.row()?;
            let mut full = if full_stored_identity {
                raw
            } else {
                let mut v = vec![SqlValue::Null; table.columns.len()];
                for (&cid, val) in stored_cids.iter().zip(raw) {
                    v[cid] = val;
                }
                v
            };
            if let Some((ref gen_order, ref scope_cols)) = virt_plan {
                eval_virtual_columns_with_plan(table, gen_order, scope_cols, &mut full, &qopts)?;
            }
            if mem.meter.is_some() {
                mem.reserve(estimate_sql_row_bytes(&full))?;
            }
            out.push((rid, full));
        }
        Ok(out)
    }

    /// Fast candidate-row lookup for `UPDATE` / `DELETE` when `WHERE` constrains rowid/IPK
    /// (`=`, `IN`, `BETWEEN`, `<`/`<=`/`>`/`>=`) or an indexed column (`=`).
    /// Always falls back to `read_all_table_rows` whenever a pushdown is not strictly sound.
    #[allow(clippy::too_many_arguments)]
    fn read_candidate_table_rows(
        &self,
        view: &SqliteDatabase,
        table: &TableInfo,
        alias: &str,
        where_clause: Option<&Expr>,
        params: &QueryParams,
        qopts: &QueryOptions,
        mem: &mut ConnMemGuard,
    ) -> SqliteResult<Vec<(Option<i64>, Vec<SqlValue>)>> {
        let Some(we) = where_clause else {
            return self.read_all_table_rows(view, table, mem);
        };
        if table.without_rowid {
            return self.read_all_table_rows(view, table, mem);
        }

        let mut conjuncts = Vec::new();
        collect_dml_and_conjuncts(we, &mut conjuncts);

        let reader = view.table(&table.name)?;
        let stored_cids: Vec<usize> = table
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let full_stored_identity = !table.has_virtual_columns && stored_cids.len() == table.columns.len();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let virt_plan = if table.has_virtual_columns {
            Some((
                generated_eval_order_from_defs(&table.name, &table.definition.columns)?,
                build_plain_scope_columns(table),
            ))
        } else {
            None
        };
        let assemble = |raw: Vec<SqlValue>| -> SqliteResult<Vec<SqlValue>> {
            let mut full = if full_stored_identity {
                raw
            } else {
                let mut v = vec![SqlValue::Null; table.columns.len()];
                for (&cid, val) in stored_cids.iter().zip(raw) {
                    v[cid] = val;
                }
                v
            };
            if let Some((ref gen_order, ref scope_cols)) = virt_plan {
                eval_virtual_columns_with_plan(table, gen_order, scope_cols, &mut full, qopts)?;
            }
            Ok(full)
        };

        let no_sub = NoSubqueries;
        let const_env = EvalEnv::new(qopts, params, &no_sub);

        // 1. Check rowid / IPK point equality, IN-list, or range bounds.
        let mut rid_lo: Option<i64> = None;
        let mut rid_hi: Option<i64> = None;
        let mut rid_empty = false;
        let mut rid_in: Option<Vec<i64>> = None;

        for conj in &conjuncts {
            match conj {
                Expr::Binary {
                    op, left, right, ..
                } if matches!(
                    op,
                    BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                ) =>
                {
                    let side = if is_dml_rowid_ref(left, table, alias) && is_dml_const_expr(right) {
                        Some((right.as_ref(), *op))
                    } else if is_dml_rowid_ref(right, table, alias) && is_dml_const_expr(left) {
                        Some((left.as_ref(), flip_dml_cmp(*op)))
                    } else {
                        None
                    };
                    if let Some((val_expr, eff_op)) = side {
                        if let Ok(val) = eval_expr(val_expr, &[], &const_env) {
                            match eff_op {
                                BinaryOp::Eq => match coerce_rowid_eq_lookup(&val) {
                                    RowidEqLookup::Exact(rid) => {
                                        // Single O(log N) point seek!
                                        return match reader.get(rid, Some(&scan_opts))? {
                                            Some(raw) => Ok(vec![(Some(rid), assemble(raw)?)]),
                                            None => Ok(Vec::new()),
                                        };
                                    }
                                    RowidEqLookup::Impossible => return Ok(Vec::new()),
                                },
                                BinaryOp::Ge => {
                                    if let Some(b) = coerce_rowid_lower_bound(&val, true) {
                                        match b {
                                            Some(lo) => rid_lo = Some(rid_lo.map_or(lo, |p| p.max(lo))),
                                            None => rid_empty = true,
                                        }
                                    }
                                }
                                BinaryOp::Gt => {
                                    if let Some(b) = coerce_rowid_lower_bound(&val, false) {
                                        match b {
                                            Some(lo) => rid_lo = Some(rid_lo.map_or(lo, |p| p.max(lo))),
                                            None => rid_empty = true,
                                        }
                                    }
                                }
                                BinaryOp::Le => {
                                    if let Some(b) = coerce_rowid_upper_bound(&val, true) {
                                        match b {
                                            Some(hi) => rid_hi = Some(rid_hi.map_or(hi, |p| p.min(hi))),
                                            None => rid_empty = true,
                                        }
                                    }
                                }
                                BinaryOp::Lt => {
                                    if let Some(b) = coerce_rowid_upper_bound(&val, false) {
                                        match b {
                                            Some(hi) => rid_hi = Some(rid_hi.map_or(hi, |p| p.min(hi))),
                                            None => rid_empty = true,
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Expr::Between {
                    not: false,
                    operand,
                    low,
                    high,
                    ..
                } if is_dml_rowid_ref(operand, table, alias)
                    && is_dml_const_expr(low)
                    && is_dml_const_expr(high) =>
                {
                    if let (Ok(lv), Ok(hv)) = (
                        eval_expr(low, &[], &const_env),
                        eval_expr(high, &[], &const_env),
                    ) {
                        if let Some(b) = coerce_rowid_lower_bound(&lv, true) {
                            match b {
                                Some(lo) => rid_lo = Some(rid_lo.map_or(lo, |p| p.max(lo))),
                                None => rid_empty = true,
                            }
                        }
                        if let Some(b) = coerce_rowid_upper_bound(&hv, true) {
                            match b {
                                Some(hi) => rid_hi = Some(rid_hi.map_or(hi, |p| p.min(hi))),
                                None => rid_empty = true,
                            }
                        }
                    }
                }
                Expr::InList {
                    not: false,
                    operand,
                    items,
                    ..
                } if rid_in.is_none()
                    && is_dml_rowid_ref(operand, table, alias)
                    && !items.is_empty()
                    && items.iter().all(is_dml_const_expr) =>
                {
                    let mut vals = Vec::with_capacity(items.len());
                    let mut ok = true;
                    for it in items {
                        match eval_expr(it, &[], &const_env) {
                            Ok(v) => {
                                if let RowidEqLookup::Exact(rid) = coerce_rowid_eq_lookup(&v) {
                                    vals.push(rid);
                                }
                            }
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        vals.sort_unstable();
                        vals.dedup();
                        rid_in = Some(vals);
                    }
                }
                _ => {}
            }
        }

        if rid_empty {
            return Ok(Vec::new());
        }
        if let Some(rids) = rid_in {
            let mut rcur = reader.cursor(Some(&scan_opts))?;
            let mut out = Vec::with_capacity(rids.len().min(256));
            for rid in rids {
                if let Some(lo) = rid_lo {
                    if rid < lo {
                        continue;
                    }
                }
                if let Some(hi) = rid_hi {
                    if rid > hi {
                        continue;
                    }
                }
                if rcur.seek(rid)? == SeekResult::Exact {
                    let row = assemble(rcur.row()?)?;
                    if mem.meter.is_some() {
                        mem.reserve(estimate_sql_row_bytes(&row))?;
                    }
                    out.push((Some(rid), row));
                }
            }
            return Ok(out);
        }
        if rid_lo.is_some() || rid_hi.is_some() {
            if let (Some(lo), Some(hi)) = (rid_lo, rid_hi) {
                if lo > hi {
                    return Ok(Vec::new());
                }
                if lo == hi {
                    return match reader.get(lo, Some(&scan_opts))? {
                        Some(raw) => {
                            let row = assemble(raw)?;
                            if mem.meter.is_some() {
                                mem.reserve(estimate_sql_row_bytes(&row))?;
                            }
                            Ok(vec![(Some(lo), row)])
                        }
                        None => Ok(Vec::new()),
                    };
                }
            }
            let mut rcur = reader.cursor(Some(&scan_opts))?;
            let positioned = if let Some(lo) = rid_lo {
                rcur.seek(lo)? != SeekResult::Eof
            } else {
                rcur.next()?
            };
            let mut out = Vec::new();
            if positioned {
                loop {
                    let rid = rcur.rowid()?;
                    if let Some(hi) = rid_hi {
                        if rid > hi {
                            break;
                        }
                    }
                    let row = assemble(rcur.row()?)?;
                    if mem.meter.is_some() {
                        mem.reserve(estimate_sql_row_bytes(&row))?;
                    }
                    out.push((Some(rid), row));
                    if !rcur.next()? {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        // 2. Check non-partial secondary index equality on first key column (`col = <const>`).
        for ix in &table.indexes {
            if ix.without_rowid_primary_key || ix.partial || ix.key_count == 0 {
                continue;
            }
            let first_ic = &ix.columns[0];
            if first_ic.cid < 0 {
                continue;
            }
            let cid = first_ic.cid as usize;
            let col_meta = &table.columns[cid];
            for conj in &conjuncts {
                let Expr::Binary {
                    op: BinaryOp::Eq,
                    left,
                    right,
                    ..
                } = conj
                else {
                    continue;
                };
                let matched = if is_dml_table_col_ref(left, table, alias, cid)
                    && is_dml_const_expr(right)
                {
                    Some((right.as_ref(), true))
                } else if is_dml_table_col_ref(right, table, alias, cid) && is_dml_const_expr(left)
                {
                    Some((left.as_ref(), false))
                } else {
                    None
                };
                let Some((val_expr, col_on_left)) = matched else {
                    continue;
                };
                // Verify effective collation and affinity match the index column!
                let val_aff = expr_affinity(val_expr, &[]);
                if comparison_affinity(Some(col_meta.affinity), val_aff) != col_meta.affinity {
                    continue;
                }
                let val_coll = expr_collation(val_expr, &[]);
                let col_op_coll = Some(OperandCollation {
                    name: Some(&col_meta.collation),
                    explicit: false,
                });
                let eff_coll = if col_on_left {
                    comparison_collation_name(col_op_coll, val_coll.as_operand())
                } else {
                    comparison_collation_name(val_coll.as_operand(), col_op_coll)
                }
                .unwrap_or("BINARY");
                if !first_ic.collation.eq_ignore_ascii_case(eff_coll) {
                    continue;
                }
                let Ok(raw_val) = eval_expr(val_expr, &[], &const_env) else {
                    continue;
                };
                if raw_val.is_null() {
                    return Ok(Vec::new());
                }
                let target_val = apply_affinity(raw_val, col_meta.affinity);
                let Ok(idx_reader) = view.index(&ix.name) else {
                    continue;
                };
                let Ok(ki) = idx_reader.key_info(Some(&qopts.collations)) else {
                    continue;
                };
                let prefix = [target_val];
                let Ok(mut sk) = SearchKey::new(ki.clone(), &prefix, 1) else {
                    continue;
                };
                let mut icur = idx_reader.cursor()?;
                if !icur.seek_key(&mut sk, SeekOp::Ge)? {
                    return Ok(Vec::new());
                }
                let mut rids_mem = ConnMemGuard::new(&self.pager);
                let mut rids = Vec::new();
                loop {
                    let entry = icur.row()?;
                    if entry.len() < 2 {
                        break;
                    }
                    let coll = ki.collations[0].as_ref();
                    if mem_compare(&entry[0], &prefix[0], coll) != std::cmp::Ordering::Equal {
                        break;
                    }
                    if let Some(rid) = entry.last().and_then(SqlValue::as_i64) {
                        if rids_mem.meter.is_some() {
                            rids_mem.reserve(8)?;
                        }
                        rids.push(rid);
                    } else {
                        // Unexpected non-integer rowid tail -> fall back safely.
                        drop(rids);
                        drop(rids_mem);
                        return self.read_all_table_rows(view, table, mem);
                    }
                    if !icur.next()? {
                        break;
                    }
                }
                rids.sort_unstable();
                rids.dedup();
                let mut rcur = reader.cursor(Some(&scan_opts))?;
                let mut out = Vec::with_capacity(rids.len().min(256));
                for rid in rids {
                    if rcur.seek(rid)? == SeekResult::Exact {
                        let row = assemble(rcur.row()?)?;
                        if mem.meter.is_some() {
                            mem.reserve(estimate_sql_row_bytes(&row))?;
                        }
                        out.push((Some(rid), row));
                    }
                }
                return Ok(out);
            }
        }

        self.read_all_table_rows(view, table, mem)
    }

    fn eval_select_stmt(
        &self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<QueryResult> {
        let catalog = crate::sql::live_catalog_for(view, options);
        xdb_sql::execute_select_stmt(&catalog, stmt, params, options).map_err(SqliteError::from)
    }

    fn eval_select_stmt_detailed(
        &self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        let catalog = crate::sql::live_catalog_for(view, options);
        execute_select_stmt_detailed(&catalog, stmt, params, options).map_err(SqliteError::from)
    }

    // ───────────────────────────── PRAGMA ─────────────────────────────

    fn exec_pragma(&mut self, stmt: &PragmaStmt) -> SqliteResult<ExecResult> {
        if let Some(ref sc) = stmt.schema {
            if !sc.name.eq_ignore_ascii_case("main") {
                return Ok(self.empty_exec_result());
            }
        }
        let name = stmt.name.name.to_ascii_lowercase();
        let val_text = stmt.value.as_ref().map(|v| {
            let t = v.text.trim();
            xdb_parser::dequote(t)
        });

        let single_row = |col: &str, v: SqlValue| ExecResult {
            columns: vec![col.to_string()],
            rows: vec![vec![v]],
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        };

        match name.as_str() {
            "page_size" => {
                if let Some(v) = val_text {
                    if let Ok(ps) = v.parse::<u32>() {
                        self.pager.set_page_size(ps)?;
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row("page_size", SqlValue::Int(i64::from(self.pager.page_size()))))
            }
            "page_count" => Ok(single_row(
                "page_count",
                SqlValue::Int(i64::from(self.pager.page_count())),
            )),
            "freelist_count" => Ok(single_row(
                "freelist_count",
                SqlValue::Int(i64::from(self.pager.freelist_count())),
            )),
            "schema_version" => Ok(single_row(
                "schema_version",
                SqlValue::Int(i64::from(self.pager.schema_cookie())),
            )),
            "user_version" => {
                if let Some(v) = val_text {
                    if let Some(uv) = parse_pragma_i32(&v) {
                        return self.run_mutating_stmt(|conn| {
                            conn.pager.set_user_version(uv)?;
                            Ok(conn.empty_exec_result())
                        });
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "user_version",
                    SqlValue::Int(i64::from(self.pager.user_version())),
                ))
            }
            "application_id" => {
                if let Some(v) = val_text {
                    if let Some(aid) = parse_pragma_i32(&v) {
                        return self.run_mutating_stmt(|conn| {
                            conn.pager.set_application_id(aid)?;
                            Ok(conn.empty_exec_result())
                        });
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "application_id",
                    SqlValue::Int(i64::from(self.pager.application_id())),
                ))
            }
            "encoding" => {
                if let Some(v) = val_text {
                    let enc = match v.trim().to_ascii_lowercase().as_str() {
                        "utf-8" | "utf8" => Some(TextEncoding::Utf8),
                        "utf-16" | "utf16" | "utf-16le" | "utf16le" => Some(TextEncoding::Utf16Le),
                        "utf-16be" | "utf16be" => Some(TextEncoding::Utf16Be),
                        _ => None,
                    };
                    if let Some(e) = enc {
                        self.invalidate_warm_view();
                        self.pager.set_encoding(e);
                        let actual = self.pager.encoding();
                        self.cached_collations = (actual, CollationRegistry::new(actual));
                    }
                    return Ok(self.empty_exec_result());
                }
                let s = match self.pager.encoding() {
                    TextEncoding::Utf8 => "UTF-8",
                    TextEncoding::Utf16Le => "UTF-16le",
                    TextEncoding::Utf16Be => "UTF-16be",
                };
                Ok(single_row("encoding", SqlValue::text(s)))
            }
            "synchronous" => {
                if let Some(v) = val_text {
                    match v.trim().to_ascii_lowercase().as_str() {
                        "0" | "off" => self.synchronous = 0,
                        "1" | "normal" => self.synchronous = 1,
                        "2" | "full" => self.synchronous = 2,
                        "3" | "extra" => self.synchronous = 3,
                        _ => {}
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row("synchronous", SqlValue::Int(self.synchronous)))
            }
            "journal_mode" => Ok(single_row("journal_mode", SqlValue::text("delete"))),
            "busy_timeout" => {
                if let Some(v) = val_text {
                    if let Ok(ms) = v.parse::<i64>() {
                        self.busy_timeout = ms.max(0);
                    }
                    return Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)));
                }
                Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)))
            }
            "foreign_keys" => {
                if let Some(v) = val_text {
                    // SQLite treats `PRAGMA foreign_keys = ON/OFF` as a no-op inside an active transaction.
                    if !self.pager.in_transaction() {
                        self.foreign_keys =
                            matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes");
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "foreign_keys",
                    SqlValue::Int(i64::from(self.foreign_keys)),
                ))
            }
            "data_version" => Ok(single_row("data_version", SqlValue::Int(1))),
            "integrity_check" | "quick_check" => {
                let max_errors = val_text
                    .as_deref()
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(100);
                let view = self.live_view()?;
                let mut roots = vec![1u32];
                for t in view.tables() {
                    if t.root_page > 1 {
                        roots.push(t.root_page);
                    }
                }
                for ix in view.indexes() {
                    if !ix.without_rowid_primary_key && ix.root_page > 1 {
                        roots.push(ix.root_page);
                    }
                }
                let msgs = check_integrity_metered(&self.pager, &roots, max_errors)?;
                Ok(ExecResult {
                    columns: vec![name],
                    rows: msgs.into_iter().map(|m| vec![SqlValue::text(m)]).collect(),
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            "table_info" | "table_xinfo" | "index_list" | "index_info" | "index_xinfo"
            | "database_list" | "foreign_key_list" | "table_list" => {
                let view = self.live_view()?;
                let sql = match val_text {
                    Some(arg) => {
                        let esc = arg.replace('\'', "''");
                        format!("SELECT * FROM pragma_{name}('{esc}')")
                    }
                    None => format!("SELECT * FROM pragma_{name}"),
                };
                let qres = view.query(&sql)?;
                Ok(ExecResult {
                    columns: qres.columns,
                    rows: qres.rows,
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            _ => Ok(self.empty_exec_result()),
        }
    }
}

// ───────────────────────────── Helper functions ─────────────────────────────

fn parse_pragma_i32(s: &str) -> Option<i32> {
    let s = s.trim();
    if let Ok(v) = s.parse::<i32>() {
        return Some(v);
    }
    let (neg, rest) = if let Some(r) = s.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else {
        (false, s)
    };
    let hex = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))?;
    let u = u64::from_str_radix(hex, 16).ok()?;
    let signed = if neg {
        (u as i64).wrapping_neg()
    } else {
        u as i64
    };
    Some(signed as i32)
}

fn validate_main_schema(schema: Option<&xdb_parser::Name>) -> SqliteResult<()> {
    if let Some(s) = schema {
        if !s.name.eq_ignore_ascii_case("main") {
            return Err(SqliteError::Sql(format!("unknown database {}", s.name)));
        }
    }
    Ok(())
}

fn resolve_conflict(
    stmt_conflict: Option<ConflictAction>,
    schema_on_error: OnError,
) -> ConflictAction {
    if let Some(c) = stmt_conflict {
        return c;
    }
    match schema_on_error {
        OnError::Rollback => ConflictAction::Rollback,
        OnError::Abort | OnError::Default | OnError::None => ConflictAction::Abort,
        OnError::Fail => ConflictAction::Fail,
        OnError::Ignore => ConflictAction::Ignore,
        OnError::Replace => ConflictAction::Replace,
    }
}

#[derive(Default)]
struct ReplacedTracker {
    rowids: HashSet<i64>,
    wr_pks: Vec<Vec<SqlValue>>,
}

impl ReplacedTracker {
    fn extend(&mut self, other: Self) {
        self.rowids.extend(other.rowids);
        self.wr_pks.extend(other.wr_pks);
    }

    fn contains_wr_pk(&self, pk: &[SqlValue], ki: &KeyInfo) -> bool {
        self.wr_pks.iter().any(|p| key_tuples_equal(p, pk, ki))
    }
}

fn key_tuples_equal(a: &[SqlValue], b: &[SqlValue], ki: &KeyInfo) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let default_bin =
        xdb_core::value::Collation::builtin(xdb_core::value::BuiltinCollation::Binary, ki.encoding);
    for (i, (va, vb)) in a.iter().zip(b.iter()).enumerate() {
        let coll = ki.collations.get(i).and_then(|c| c.as_ref());
        let effective = match coll {
            Some(c) => Some(c),
            None if ki.encoding != TextEncoding::Utf8 => Some(&default_bin),
            None => None,
        };
        if mem_compare(va, vb, effective) != std::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}

fn validate_index_expr_deterministic(expr: &Expr, ctx: &str) -> SqliteResult<()> {
    match expr {
        Expr::CurrentTime { .. } => {
            return Err(SqliteError::Sql(format!(
                "non-deterministic functions prohibited in {ctx}"
            )));
        }
        Expr::Function(f) => {
            let lower = f.name.name.to_ascii_lowercase();
            let non_det = match lower.as_str() {
                "random" | "randomblob" | "changes" | "total_changes" | "last_insert_rowid" => true,
                "date" | "time" | "datetime" | "julianday" | "unixepoch" => {
                    f.args.is_empty()
                        || f.args.iter().any(|a| {
                            matches!(a, Expr::String(s) if s.value.eq_ignore_ascii_case("now")
                                || s.value.eq_ignore_ascii_case("localtime")
                                || s.value.eq_ignore_ascii_case("utc"))
                        })
                }
                "strftime" => {
                    f.args.len() < 2
                        || f.args.iter().skip(1).any(|a| {
                            matches!(a, Expr::String(s) if s.value.eq_ignore_ascii_case("now")
                                || s.value.eq_ignore_ascii_case("localtime")
                                || s.value.eq_ignore_ascii_case("utc"))
                        })
                }
                _ => false,
            };
            if non_det {
                return Err(SqliteError::Sql(format!(
                    "non-deterministic functions prohibited in {ctx}"
                )));
            }
        }
        _ => {}
    }
    for child in xdb_parser::expr_children(expr) {
        validate_index_expr_deterministic(child, ctx)?;
    }
    Ok(())
}

fn collect_generated_col_deps(
    expr: &Expr,
    table_name: &str,
    cols: &[xdb_parser::ColumnDefinition],
    deps: &mut Vec<usize>,
) {
    if let Expr::Column(c) = expr {
        let tbl_ok = c.table.is_none()
            || c.table
                .as_ref()
                .is_some_and(|t| t.name.eq_ignore_ascii_case(table_name));
        if tbl_ok {
            if let Some(pos) = cols
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))
            {
                if !deps.contains(&pos) {
                    deps.push(pos);
                }
            }
        }
    }
    for child in xdb_parser::expr_children(expr) {
        collect_generated_col_deps(child, table_name, cols, deps);
    }
}

fn generated_eval_order_from_defs(
    table_name: &str,
    cols: &[xdb_parser::ColumnDefinition],
) -> SqliteResult<Vec<usize>> {
    let n = cols.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut has_gen = false;
    for (cid, col) in cols.iter().enumerate() {
        if let Some(ref expr) = col.generated {
            has_gen = true;
            collect_generated_col_deps(expr, table_name, cols, &mut deps[cid]);
        }
    }
    if !has_gen {
        return Ok(Vec::new());
    }
    // 0 = unvisited, 1 = visiting, 2 = done
    let mut state = vec![0u8; n];
    let mut order = Vec::new();
    fn dfs(
        u: usize,
        cols: &[xdb_parser::ColumnDefinition],
        deps: &[Vec<usize>],
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> SqliteResult<()> {
        state[u] = 1;
        for &v in &deps[u] {
            if cols[v].generated.is_none() {
                continue;
            }
            match state[v] {
                1 => {
                    return Err(SqliteError::Sql(format!(
                        "generated column loop on \"{}\"",
                        cols[v].name
                    )));
                }
                0 => dfs(v, cols, deps, state, order)?,
                _ => {}
            }
        }
        state[u] = 2;
        order.push(u);
        Ok(())
    }
    for (cid, col) in cols.iter().enumerate() {
        if col.generated.is_some() && state[cid] == 0 {
            dfs(cid, cols, &deps, &mut state, &mut order)?;
        }
    }
    Ok(order)
}

fn eval_generated_column_value(
    table: &TableInfo,
    cid: usize,
    expr: &Expr,
    scope_cols: &[ScopeColumn],
    row: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<SqlValue> {
    let params = QueryParams::empty();
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(qopts, &params, &no_sub);
    env.dqs = true;
    env.fn_ctx.pure_context = Some("a generated column");

    let mut scope = EvalScope::simple(scope_cols, row);
    scope.no_rowid_aliases = true;
    let scopes = [scope];
    let raw = eval_expr(expr, &scopes, &env)?;
    let col = &table.columns[cid];
    if table.strict {
        validate_strict_value(raw, &col.decl_type, &table.name, &col.name)
    } else {
        Ok(apply_affinity(raw, col.affinity))
    }
}

enum RowWriteOutcome {
    Written {
        final_rowid: Option<i64>,
        final_row: Vec<SqlValue>,
        was_insert: bool,
        deleted_conflict: bool,
    },
    Ignored,
}

struct ConnMemGuard {
    meter: Option<Arc<dyn xdb_core::Meter>>,
    reserved: u64,
}

impl ConnMemGuard {
    fn new(pager: &WritePager) -> Self {
        Self {
            meter: (!pager.is_noop_meter()).then(|| pager.meter().clone()),
            reserved: 0,
        }
    }

    #[inline]
    fn reserve(&mut self, bytes: u64) -> SqliteResult<()> {
        if bytes > 0 {
            if let Some(ref m) = self.meter {
                m.reserve_memory(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
            }
        }
        Ok(())
    }
}

impl Drop for ConnMemGuard {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(ref m) = self.meter {
                m.release_memory(self.reserved);
            }
        }
    }
}

#[inline]
fn estimate_sql_val_bytes(v: &SqlValue) -> u64 {
    match v {
        SqlValue::Text(b) | SqlValue::Blob(b) => 24u64.saturating_add(b.len() as u64),
        _ => 24,
    }
}

fn estimate_sql_row_bytes(row: &[SqlValue]) -> u64 {
    let mut total = 32u64;
    for v in row {
        total = total.saturating_add(estimate_sql_val_bytes(v));
    }
    total
}

static NULL_SQL_VAL: SqlValue = SqlValue::Null;

fn encode_table_record_refs_into(
    table: &TableInfo,
    row: &[&SqlValue],
    enc: TextEncoding,
    schema_format: u32,
    out: &mut Vec<u8>,
) {
    let n_cols = table.columns.len();
    if n_cols <= 32 {
        let mut refs: [&SqlValue; 32] = [&NULL_SQL_VAL; 32];
        let mut count = 0usize;
        for (cid, col) in table.columns.iter().enumerate() {
            if col.storage_index.is_none() {
                continue;
            }
            if Some(cid) == table.ipk_column && !table.without_rowid {
                refs[count] = &NULL_SQL_VAL;
            } else {
                refs[count] = row[cid];
            }
            count += 1;
        }
        encode_record_refs_into(&refs[..count], enc, schema_format, out);
        return;
    }
    let mut refs: Vec<&SqlValue> = Vec::with_capacity(n_cols);
    for (cid, col) in table.columns.iter().enumerate() {
        if col.storage_index.is_none() {
            continue;
        }
        if Some(cid) == table.ipk_column && !table.without_rowid {
            refs.push(&NULL_SQL_VAL);
        } else {
            refs.push(row[cid]);
        }
    }
    encode_record_refs_into(&refs, enc, schema_format, out);
}

/// Apply SQLite column affinity to `v` only when the value actually changes, returning `None`
/// so the caller can keep borrowing `&SqlValue` with zero `Box<[u8]>` heap allocation!
#[inline]
fn apply_affinity_if_changed(v: &SqlValue, aff: Affinity) -> Option<SqlValue> {
    match v {
        SqlValue::Null | SqlValue::Blob(_) => None,
        SqlValue::Int(_) => match aff {
            Affinity::Text | Affinity::Real => Some(apply_affinity(v.clone(), aff)),
            _ => None,
        },
        SqlValue::Text(_) => {
            if xdb_core::value::is_numeric_affinity(Some(aff)) {
                Some(apply_affinity(v.clone(), aff))
            } else {
                None
            }
        }
        SqlValue::Real(r) => {
            if r.is_nan() {
                Some(SqlValue::Null)
            } else if matches!(aff, Affinity::Blob | Affinity::None) {
                None
            } else {
                Some(apply_affinity(v.clone(), aff))
            }
        }
    }
}

fn compile_fast_val_src(expr: &Expr) -> Option<FastValSrc> {
    match expr {
        Expr::Variable(v) => Some(FastValSrc::Param {
            index: v.index,
            text: v.text.clone(),
        }),
        Expr::Null { .. } => Some(FastValSrc::Const(SqlValue::Null)),
        Expr::Boolean { value, .. } => Some(FastValSrc::Const(SqlValue::Int(i64::from(*value)))),
        Expr::String(s) => Some(FastValSrc::Const(SqlValue::text(s.value.as_str()))),
        Expr::Number(n) if !n.is_float => {
            integer_literal_value(n.text.as_bytes(), false).map(FastValSrc::Const)
        }
        Expr::Number(n) => Some(FastValSrc::Const(real_literal_value(n.text.as_bytes(), false))),
        Expr::Blob(b) => Some(FastValSrc::Const(SqlValue::Blob(b.value.clone().into_boxed_slice()))),
        Expr::Unary { op, operand, .. }
            if matches!(op, xdb_parser::UnaryOp::Neg | xdb_parser::UnaryOp::Pos) =>
        {
            let neg = *op == xdb_parser::UnaryOp::Neg;
            if let Expr::Number(n) = operand.as_ref() {
                if !n.is_float {
                    integer_literal_value(n.text.as_bytes(), neg).map(FastValSrc::Const)
                } else {
                    Some(FastValSrc::Const(real_literal_value(n.text.as_bytes(), neg)))
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn compile_fast_rowid_eq_src(where_expr: &Expr, table: &TableInfo, alias: &str) -> Option<FastKeySrc> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
        ..
    } = where_expr
    else {
        return None;
    };
    let val_expr = if is_dml_rowid_ref(left, table, alias) {
        right.as_ref()
    } else if is_dml_rowid_ref(right, table, alias) {
        left.as_ref()
    } else {
        return None;
    };
    match compile_fast_val_src(val_expr)? {
        FastValSrc::Param { index, text } => Some(FastKeySrc::Param { index, text }),
        FastValSrc::Const(sv) => match coerce_rowid_eq_lookup(&sv) {
            RowidEqLookup::Exact(r) => Some(FastKeySrc::Const(Some(r))),
            RowidEqLookup::Impossible => Some(FastKeySrc::Const(None)),
        },
    }
}

/// Fast O(N) stable counting / 2-pass 11-bit LSD radix sort for integer secondary-index
/// entries `(key, rowid)` that arrive in strictly ascending `rowid` order.
fn sort_int_index_entries(
    entries: &mut Vec<(i64, i64)>,
    is_desc: bool,
    mem: &mut ConnMemGuard,
) -> SqliteResult<()> {
    let n = entries.len();
    if n <= 1 {
        return Ok(());
    }
    // Check if already sorted and find (min_k, max_k) in one linear pass.
    let mut min_k = entries[0].0;
    let mut max_k = min_k;
    let mut already_sorted = true;
    let mut prev_k = min_k;
    for &(k, _) in &entries[1..] {
        if k < min_k {
            min_k = k;
        }
        if k > max_k {
            max_k = k;
        }
        if if is_desc { k > prev_k } else { k < prev_k } {
            already_sorted = false;
        }
        prev_k = k;
    }
    if already_sorted {
        return Ok(());
    }
    if n < 256 {
        if is_desc {
            entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        } else {
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        }
        return Ok(());
    }

    let span_u128 = (max_k as i128 - min_k as i128) as u128;
    // Single-pass stable counting sort when integer range <= 16,383.
    if span_u128 <= 16_383 {
        let span = (span_u128 as usize) + 1;
        let scratch_bytes = ((n * std::mem::size_of::<(i64, i64)>())
            + (span * std::mem::size_of::<u32>())) as u64;
        if mem.meter.is_some() {
            mem.reserve(scratch_bytes)?;
        }
        let mut counts = vec![0u32; span];
        if is_desc {
            for &(k, _) in entries.iter() {
                counts[(max_k.wrapping_sub(k)) as usize] += 1;
            }
        } else {
            for &(k, _) in entries.iter() {
                counts[(k.wrapping_sub(min_k)) as usize] += 1;
            }
        }
        let mut acc = 0u32;
        for c in &mut counts {
            let v = *c;
            *c = acc;
            acc += v;
        }
        let mut out = vec![(0i64, 0i64); n];
        if is_desc {
            for &(k, rid) in entries.iter() {
                let b = (max_k.wrapping_sub(k)) as usize;
                let pos = counts[b] as usize;
                counts[b] = (pos + 1) as u32;
                out[pos] = (k, rid);
            }
        } else {
            for &(k, rid) in entries.iter() {
                let b = (k.wrapping_sub(min_k)) as usize;
                let pos = counts[b] as usize;
                counts[b] = (pos + 1) as u32;
                out[pos] = (k, rid);
            }
        }
        *entries = out;
        return Ok(());
    }

    // 2-pass 11-bit stable LSD radix sort for ranges up to 2^22 - 1 (~4.19M).
    if span_u128 <= 0x3f_ffff {
        let scratch_bytes = (n * std::mem::size_of::<(i64, i64)>()) as u64;
        if mem.meter.is_some() {
            mem.reserve(scratch_bytes)?;
        }
        let mut tmp = vec![(0i64, 0i64); n];
        // Pass 1: low 11 bits
        let mut c0 = [0u32; 2048];
        let mut c1 = [0u32; 2048];
        for &(k, _) in entries.iter() {
            let u = if is_desc {
                max_k.wrapping_sub(k) as u32
            } else {
                k.wrapping_sub(min_k) as u32
            };
            c0[(u & 0x7ff) as usize] += 1;
            c1[((u >> 11) & 0x7ff) as usize] += 1;
        }
        let (mut a0, mut a1) = (0u32, 0u32);
        for i in 0..2048 {
            let v0 = c0[i];
            c0[i] = a0;
            a0 += v0;
            let v1 = c1[i];
            c1[i] = a1;
            a1 += v1;
        }
        for &(k, rid) in entries.iter() {
            let u = if is_desc {
                max_k.wrapping_sub(k) as u32
            } else {
                k.wrapping_sub(min_k) as u32
            };
            let b = (u & 0x7ff) as usize;
            let pos = c0[b] as usize;
            c0[b] = (pos + 1) as u32;
            tmp[pos] = (k, rid);
        }
        // Pass 2: high 11 bits
        for &(k, rid) in &tmp {
            let u = if is_desc {
                max_k.wrapping_sub(k) as u32
            } else {
                k.wrapping_sub(min_k) as u32
            };
            let b = ((u >> 11) & 0x7ff) as usize;
            let pos = c1[b] as usize;
            c1[b] = (pos + 1) as u32;
            entries[pos] = (k, rid);
        }
        return Ok(());
    }

    if is_desc {
        entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    } else {
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    }
    Ok(())
}

fn encode_table_record_into(
    table: &TableInfo,
    row: &[SqlValue],
    enc: TextEncoding,
    schema_format: u32,
    out: &mut Vec<u8>,
) {
    let n_cols = table.columns.len();
    if n_cols <= 32 {
        let mut refs: [&SqlValue; 32] = [&NULL_SQL_VAL; 32];
        let mut count = 0usize;
        for (cid, col) in table.columns.iter().enumerate() {
            if col.storage_index.is_none() {
                continue;
            }
            if Some(cid) == table.ipk_column && !table.without_rowid {
                refs[count] = &NULL_SQL_VAL;
            } else {
                refs[count] = &row[cid];
            }
            count += 1;
        }
        encode_record_refs_into(&refs[..count], enc, schema_format, out);
        return;
    }
    let mut refs: Vec<&SqlValue> = Vec::with_capacity(n_cols);
    for (cid, col) in table.columns.iter().enumerate() {
        if col.storage_index.is_none() {
            continue;
        }
        if Some(cid) == table.ipk_column && !table.without_rowid {
            refs.push(&NULL_SQL_VAL);
        } else {
            refs.push(&row[cid]);
        }
    }
    encode_record_refs_into(&refs, enc, schema_format, out);
}

#[derive(Debug)]
struct DmlTableState {
    table: Arc<TableInfo>,
    simple_insert_elidable: bool,
    simple_delete_elidable: bool,
    any_unique_secondary: bool,
    any_default_expr: bool,
    indexed_col_mask: u64,
    simple_delete_want_fields: Vec<usize>,
    simple_delete_want_cids: Vec<usize>,
    gen_order: Vec<usize>,
    gen_scope_cols: Vec<ScopeColumn>,
    check_scope_cols: Vec<ScopeColumn>,
    wr_pk_ki: Option<KeyInfo>,
    index_key_infos: Vec<Option<KeyInfo>>,
    index_expr_scope_cols: Vec<ScopeColumn>,
    index_where_scope_cols: Vec<ScopeColumn>,
}

impl DmlTableState {
    fn build(table: Arc<TableInfo>, enc: TextEncoding) -> SqliteResult<Self> {
        let gen_order = generated_eval_order_from_defs(&table.name, &table.definition.columns)?;
        let gen_scope_cols = if gen_order.is_empty() {
            Vec::new()
        } else {
            build_plain_scope_columns(&table)
        };
        let check_scope_cols = if table.definition.checks.is_empty() {
            Vec::new()
        } else {
            build_table_scope_columns(&table, &table.name)
        };
        let colls = CollationRegistry::new(enc);
        let wr_pk_ki = if table.without_rowid {
            if let Some(pk_idx) = table.indexes.iter().find(|ix| ix.without_rowid_primary_key) {
                Some(key_info_for_without_rowid_pk(pk_idx, enc, &colls)?)
            } else {
                None
            }
        } else {
            None
        };
        let mut index_key_infos = Vec::with_capacity(table.indexes.len());
        let mut any_idx_expr = false;
        let mut any_idx_where = false;
        let mut any_idx_replace = false;
        let mut any_unique_secondary = false;
        let mut indexed_col_mask = 0u64;
        for ix in &table.indexes {
            if ix.without_rowid_primary_key {
                index_key_infos.push(None);
            } else {
                if ix.unique {
                    any_unique_secondary = true;
                }
                if index_needs_expr_scope(ix) {
                    any_idx_expr = true;
                    indexed_col_mask = u64::MAX;
                }
                if ix.definition.where_clause.is_some() {
                    any_idx_where = true;
                    indexed_col_mask = u64::MAX;
                }
                if ix.definition.on_error == OnError::Replace {
                    any_idx_replace = true;
                }
                for ic in &ix.columns[..ix.key_count] {
                    if ic.cid >= 0 {
                        let cid = ic.cid as usize;
                        if cid < 64 {
                            indexed_col_mask |= 1u64 << cid;
                        } else {
                            indexed_col_mask = u64::MAX;
                        }
                    }
                }
                index_key_infos.push(Some(key_info_for_index(ix, enc, Some(&colls))?));
            }
        }
        let any_default_expr = table
            .definition
            .columns
            .iter()
            .any(|c| c.default_expr.is_some());
        let simple_delete_elidable =
            !table.without_rowid && !table.has_virtual_columns && !any_idx_expr && !any_idx_where;
        let simple_insert_elidable = simple_delete_elidable
            && table.definition.ipk_on_error != OnError::Replace
            && !any_idx_replace;
        let mut simple_delete_want_fields = Vec::new();
        let mut simple_delete_want_cids = Vec::new();
        if simple_delete_elidable {
            for ix in &table.indexes {
                for ic in &ix.columns[..ix.key_count] {
                    if ic.cid >= 0 {
                        let cid = ic.cid as usize;
                        if Some(cid) != table.ipk_column {
                            if let Some(sidx) = table.columns[cid].storage_index {
                                if !simple_delete_want_fields.contains(&sidx) {
                                    simple_delete_want_fields.push(sidx);
                                    simple_delete_want_cids.push(cid);
                                }
                            }
                        }
                    }
                }
            }
        }
        let index_expr_scope_cols = if any_idx_expr {
            build_plain_scope_columns(&table)
        } else {
            Vec::new()
        };
        let index_where_scope_cols = if any_idx_where {
            build_table_scope_columns(&table, &table.name)
        } else {
            Vec::new()
        };
        Ok(Self {
            table,
            simple_insert_elidable,
            simple_delete_elidable,
            any_unique_secondary,
            any_default_expr,
            indexed_col_mask,
            simple_delete_want_fields,
            simple_delete_want_cids,
            gen_order,
            gen_scope_cols,
            check_scope_cols,
            wr_pk_ki,
            index_key_infos,
            index_expr_scope_cols,
            index_where_scope_cols,
        })
    }
}

fn build_plain_scope_columns(table: &TableInfo) -> Vec<ScopeColumn> {
    table
        .columns
        .iter()
        .map(|c| ScopeColumn {
            table: Some(table.name.clone()),
            name: c.name.clone(),
            affinity: c.affinity,
            collation: c.collation.clone(),
            is_rowid_alias: false,
        })
        .collect()
}

fn index_needs_expr_scope(index: &IndexInfo) -> bool {
    index.columns.iter().any(|ic| ic.cid < -1)
}

fn eval_virtual_columns(
    table: &TableInfo,
    full: &mut [SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    let gen_order = generated_eval_order_from_defs(&table.name, &table.definition.columns)?;
    let scope_cols = build_plain_scope_columns(table);
    eval_virtual_columns_with_plan(table, &gen_order, &scope_cols, full, qopts)
}

fn eval_virtual_columns_with_plan(
    table: &TableInfo,
    gen_order: &[usize],
    scope_cols: &[ScopeColumn],
    full: &mut [SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    for &cid in gen_order {
        if table.columns[cid].generated == Some(GeneratedKind::Virtual) {
            if let Some(ref expr) = table.definition.columns[cid].generated {
                full[cid] =
                    eval_generated_column_value(table, cid, expr, scope_cols, full, qopts)?;
            }
        }
    }
    Ok(())
}

fn build_without_rowid_record_values(table: &TableInfo, row: &[SqlValue]) -> Vec<SqlValue> {
    let n_stored = table
        .columns
        .iter()
        .filter(|c| c.storage_index.is_some())
        .count();
    let mut out = vec![SqlValue::Null; n_stored];
    for (cid, col) in table.columns.iter().enumerate() {
        if let Some(si) = col.storage_index {
            if si < out.len() {
                out[si] = row[cid].clone();
            }
        }
    }
    out
}

fn decode_without_rowid_full_row(
    table: &TableInfo,
    record_vals: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<Vec<SqlValue>> {
    let mut full = vec![SqlValue::Null; table.columns.len()];
    for (cid, col) in table.columns.iter().enumerate() {
        if let Some(si) = col.storage_index {
            let mut v = record_vals
                .get(si)
                .cloned()
                .unwrap_or_else(|| col.default_value.clone());
            // SQLite compactly stores integer-valued REALs as Int on disk; convert back on read.
            if col.affinity == Affinity::Real {
                if let SqlValue::Int(i) = v {
                    v = SqlValue::Real(i as f64);
                }
            }
            full[cid] = v;
        }
    }
    if table.has_virtual_columns {
        eval_virtual_columns(table, &mut full, qopts)?;
    }
    Ok(full)
}

fn build_index_entry_values_fast(
    table: &TableInfo,
    index: &IndexInfo,
    rowid: Option<i64>,
    row: &[SqlValue],
    qopts: &QueryOptions,
    expr_scope_cols: &[ScopeColumn],
    where_scope_cols: &[ScopeColumn],
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let has_where = index.definition.where_clause.is_some();
    let has_exprs = index_needs_expr_scope(index);

    // Fast path: non-partial, column-only index -> zero EvalEnv or ScopeColumn allocations!
    if !has_where && !has_exprs {
        let mut out = Vec::with_capacity(index.columns.len());
        for ic in &index.columns {
            if ic.cid >= 0 {
                out.push(row[ic.cid as usize].clone());
            } else if ic.cid == -1 {
                out.push(rowid.map_or(SqlValue::Null, SqlValue::Int));
            } else {
                out.push(SqlValue::Null);
            }
        }
        return Ok(Some(out));
    }

    let params = QueryParams::empty();
    let no_sub = NoSubqueries;

    // Partial index WHERE predicate check (rowid aliases are permitted in partial index WHERE)
    if let Some(ref where_expr) = index.definition.where_clause {
        let mut where_vals = row.to_vec();
        if !table.without_rowid {
            let rid_v = rowid.map_or(SqlValue::Null, SqlValue::Int);
            while where_vals.len() < where_scope_cols.len() {
                where_vals.push(rid_v.clone());
            }
        }
        let mut where_env = EvalEnv::new(qopts, &params, &no_sub);
        where_env.dqs = true;
        where_env.fn_ctx.pure_context = Some("partial index WHERE clauses");
        let where_scope = [EvalScope::simple(where_scope_cols, &where_vals)];
        let cond = eval_expr(where_expr, &where_scope, &where_env)?;
        if !boolean_value(&cond, false) {
            return Ok(None);
        }
    }

    if !has_exprs {
        let mut out = Vec::with_capacity(index.columns.len());
        for ic in &index.columns {
            if ic.cid >= 0 {
                out.push(row[ic.cid as usize].clone());
            } else if ic.cid == -1 {
                out.push(rowid.map_or(SqlValue::Null, SqlValue::Int));
            } else {
                out.push(SqlValue::Null);
            }
        }
        return Ok(Some(out));
    }

    let mut env = EvalEnv::new(qopts, &params, &no_sub);
    env.dqs = true;
    env.fn_ctx.pure_context = Some("index expressions");
    let mut expr_scope = EvalScope::simple(expr_scope_cols, row);
    expr_scope.no_rowid_aliases = true;
    let scope = [expr_scope];

    let mut out = Vec::with_capacity(index.columns.len());
    for (pos, ic) in index.columns.iter().enumerate() {
        let val = if ic.cid >= 0 {
            row[ic.cid as usize].clone()
        } else if ic.cid == -1 {
            rowid.map_or(SqlValue::Null, SqlValue::Int)
        } else if let Some(ref expr) = index.definition.columns[pos].expr {
            eval_expr(expr, &scope, &env)?
        } else if let Some(ref expr_sql) = ic.expr {
            let parsed = parse_expression(expr_sql, &ParseOptions::default())
                .map_err(|e| SqliteError::Sql(e.to_string()))?;
            eval_expr(&parsed, &scope, &env)?
        } else {
            SqlValue::Null
        };
        out.push(val);
    }
    Ok(Some(out))
}

#[inline]
fn sql_value_exact_eq(a: &SqlValue, b: &SqlValue) -> bool {
    match (a, b) {
        (SqlValue::Null, SqlValue::Null) => true,
        (SqlValue::Int(x), SqlValue::Int(y)) => x == y,
        (SqlValue::Real(x), SqlValue::Real(y)) => x.to_bits() == y.to_bits(),
        (SqlValue::Text(x), SqlValue::Text(y)) => x == y,
        (SqlValue::Blob(x), SqlValue::Blob(y)) => x == y,
        _ => false,
    }
}

fn index_may_have_changed(
    table: &TableInfo,
    ix: &IndexInfo,
    rowid: Option<i64>,
    old_row: &[SqlValue],
    new_row: &[SqlValue],
    qopts: &QueryOptions,
    dml: &DmlTableState,
) -> SqliteResult<bool> {
    if ix.definition.where_clause.is_none() && !index_needs_expr_scope(ix) {
        for ic in &ix.columns[..ix.key_count] {
            if ic.cid >= 0 {
                let c = ic.cid as usize;
                if !sql_value_exact_eq(&old_row[c], &new_row[c]) {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }
    let old_entry = build_index_entry_values_fast(
        table,
        ix,
        rowid,
        old_row,
        qopts,
        &dml.index_expr_scope_cols,
        &dml.index_where_scope_cols,
    )?;
    let new_entry = build_index_entry_values_fast(
        table,
        ix,
        rowid,
        new_row,
        qopts,
        &dml.index_expr_scope_cols,
        &dml.index_where_scope_cols,
    )?;
    match (old_entry, new_entry) {
        (None, None) => Ok(false),
        (Some(a), Some(b)) if a.len() == b.len() => {
            Ok(!a.iter().zip(&b).all(|(x, y)| sql_value_exact_eq(x, y)))
        }
        _ => Ok(true),
    }
}

fn table_has_ignore_or_fail_conflict(table: &TableInfo) -> bool {
    if matches!(table.definition.ipk_on_error, OnError::Ignore | OnError::Fail) {
        return true;
    }
    if table
        .definition
        .columns
        .iter()
        .any(|c| matches!(c.not_null_on_error, OnError::Ignore | OnError::Fail))
    {
        return true;
    }
    table
        .indexes
        .iter()
        .any(|ix| matches!(ix.definition.on_error, OnError::Ignore | OnError::Fail))
}

// ───────────────────────────── DML pushdown & UPDATE FROM join helpers ─────────────────────────────

fn collect_dml_and_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
        ..
    } = expr
    {
        collect_dml_and_conjuncts(left, out);
        collect_dml_and_conjuncts(right, out);
    } else {
        out.push(expr);
    }
}

fn is_dml_rowid_ref(expr: &Expr, table: &TableInfo, alias: &str) -> bool {
    let Expr::Column(c) = expr else {
        return false;
    };
    if c.schema.is_some() {
        return false;
    }
    if let Some(ref tbl) = c.table {
        if !tbl.name.eq_ignore_ascii_case(alias) {
            return false;
        }
    }
    if let Some(ipk) = table.ipk_column {
        if c.column.name.eq_ignore_ascii_case(&table.columns[ipk].name) {
            return true;
        }
    }
    let cname = &c.column.name;
    (cname.eq_ignore_ascii_case("rowid")
        || cname.eq_ignore_ascii_case("_rowid_")
        || cname.eq_ignore_ascii_case("oid"))
        && !table
            .columns
            .iter()
            .any(|col| col.name.eq_ignore_ascii_case(cname))
}

fn is_dml_table_col_ref(expr: &Expr, table: &TableInfo, alias: &str, cid: usize) -> bool {
    let Expr::Column(c) = expr else {
        return false;
    };
    if c.schema.is_some() {
        return false;
    }
    if let Some(ref tbl) = c.table {
        if !tbl.name.eq_ignore_ascii_case(alias) {
            return false;
        }
    }
    c.column.name.eq_ignore_ascii_case(&table.columns[cid].name)
}

fn expr_has_subquery_or_intable(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. } => true,
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .any(expr_has_subquery_or_intable),
    }
}

fn direct_values_fast_path<'a>(
    stmt: &'a InsertStmt,
    sel: &'a SelectStmt,
) -> Option<&'a xdb_parser::ValuesClause> {
    if stmt.with.is_some()
        || sel.with.is_some()
        || sel.order_by.is_some()
        || sel.order_by_all.is_some()
        || sel.limit.is_some()
    {
        return None;
    }
    let SelectBody::Values(ref vals) = sel.body else {
        return None;
    };
    if vals
        .rows
        .iter()
        .any(|r| r.items.iter().any(expr_has_subquery_or_intable))
    {
        return None;
    }
    Some(vals)
}

fn has_direct_rowid_eq_conjunct(where_expr: &Expr, table: &TableInfo, alias: &str) -> bool {
    match where_expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } => {
            has_direct_rowid_eq_conjunct(left, table, alias)
                || has_direct_rowid_eq_conjunct(right, table, alias)
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            ..
        } => {
            (is_dml_rowid_ref(left, table, alias) && is_dml_const_expr(right))
                || (is_dml_rowid_ref(right, table, alias) && is_dml_const_expr(left))
        }
        _ => false,
    }
}

fn try_eval_simple_const_expr(expr: &Expr, params: &QueryParams) -> SqliteResult<Option<SqlValue>> {
    match expr {
        Expr::Variable(v) => Ok(Some(params.resolve(v.index, &v.text))),
        Expr::Null { .. } => Ok(Some(SqlValue::Null)),
        Expr::Boolean { value, .. } => Ok(Some(SqlValue::Int(i64::from(*value)))),
        Expr::String(s) => Ok(Some(SqlValue::text(s.value.as_str()))),
        Expr::Number(n) if !n.is_float => integer_literal_value(n.text.as_bytes(), false)
            .map(Some)
            .ok_or_else(|| SqliteError::Sql("hex literal too big".to_string())),
        Expr::Number(n) => Ok(Some(real_literal_value(n.text.as_bytes(), false))),
        _ => Ok(None),
    }
}

/// Fast path for `UPDATE t SET col = <simple_const>, ... WHERE id = <const>`:
/// avoids `build_table_scope_columns`, `EvalEnv::new` (`SystemTime::now()`), and `eval_expr`
/// when all SET targets are single non-IPK, non-generated stored columns with simple RHS values.
fn try_apply_simple_set_clauses(
    table: &TableInfo,
    old_row: &[SqlValue],
    clauses: &[SetClause],
    params: &QueryParams,
    qopts: &QueryOptions,
) -> SqliteResult<Option<Vec<SqlValue>>> {
    let mut edits: Vec<(usize, SqlValue)> = Vec::with_capacity(clauses.len());
    for sc in clauses {
        if sc.columns.len() != 1 {
            return Ok(None);
        }
        let col_name = &sc.columns[0].name;
        let Some(cid) = table
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col_name))
        else {
            return Ok(None);
        };
        if table.columns[cid].generated.is_some()
            || (!table.without_rowid && Some(cid) == table.ipk_column)
        {
            return Ok(None);
        }
        let Some(val) = try_eval_simple_const_expr(&sc.expr, params)? else {
            return Ok(None);
        };
        edits.push((cid, val));
    }
    if let Some(ref m) = qopts.meter {
        m.charge_steps(clauses.len() as u64)?;
    }
    let mut new_row = old_row.to_vec();
    for (cid, val) in edits {
        new_row[cid] = val;
    }
    Ok(Some(new_row))
}

/// Zero-allocation exact `WHERE <ipk|rowid> = <int_lit|param>` extractor requiring no `QueryOptions`.
fn extract_fast_exact_rowid_eq_no_qopts(
    where_expr: &Expr,
    table: &TableInfo,
    alias: &str,
    params: &QueryParams,
) -> Option<i64> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
        ..
    } = where_expr
    else {
        return None;
    };
    let val_expr = if is_dml_rowid_ref(left, table, alias) {
        right.as_ref()
    } else if is_dml_rowid_ref(right, table, alias) {
        left.as_ref()
    } else {
        return None;
    };
    let raw_val = try_eval_simple_const_expr(val_expr, params).ok().flatten()?;
    match raw_val {
        SqlValue::Int(i) => Some(i),
        SqlValue::Real(r) => real_to_exact_i64(r),
        SqlValue::Text(b) => match apply_numeric_affinity(&b, false) {
            Some(SqlValue::Int(i)) => Some(i),
            Some(SqlValue::Real(r)) => real_to_exact_i64(r),
            _ => None,
        },
        _ => None,
    }
}

fn extract_direct_rowid_eq(
    where_expr: &Expr,
    table: &TableInfo,
    alias: &str,
    params: &QueryParams,
    qopts: &QueryOptions,
) -> Option<(i64, bool)> {
    let mut conjuncts = Vec::new();
    collect_dml_and_conjuncts(where_expr, &mut conjuncts);
    let exact_only = conjuncts.len() == 1;
    let mut env_opt: Option<EvalEnv<'_>> = None;
    for conj in conjuncts {
        if let Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            ..
        } = conj
        {
            let val_expr = if is_dml_rowid_ref(left, table, alias) && is_dml_const_expr(right) {
                Some(right.as_ref())
            } else if is_dml_rowid_ref(right, table, alias) && is_dml_const_expr(left) {
                Some(left.as_ref())
            } else {
                None
            };
            if let Some(ve) = val_expr {
                let v = match try_eval_simple_const_expr(ve, params) {
                    Ok(Some(sv)) => sv,
                    Ok(None) => {
                        let env =
                            env_opt.get_or_insert_with(|| EvalEnv::new(qopts, params, &NoSubqueries));
                        match eval_expr(ve, &[], env) {
                            Ok(v) => v,
                            Err(_) => continue,
                        }
                    }
                    Err(_) => continue,
                };
                if let RowidEqLookup::Exact(rid) = coerce_rowid_eq_lookup(&v) {
                    return Some((rid, exact_only));
                }
            }
        }
    }
    None
}

fn is_dml_const_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Blob(_)
        | Expr::Null { .. }
        | Expr::Boolean { .. }
        | Expr::Variable(_) => true,
        Expr::Column(_)
        | Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. }
        | Expr::CurrentTime { .. }
        | Expr::Raise { .. } => false,
        Expr::Function(f) => {
            let lower = f.name.name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "random" | "randomblob" | "changes" | "total_changes" | "last_insert_rowid"
            ) || f.over.is_some()
                || f.filter.is_some()
            {
                return false;
            }
            f.args.iter().all(is_dml_const_expr)
        }
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .all(is_dml_const_expr),
    }
}

fn flip_dml_cmp(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Le => BinaryOp::Ge,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Ge => BinaryOp::Le,
        other => other,
    }
}

enum RowidEqLookup {
    Exact(i64),
    Impossible,
}

fn coerce_rowid_eq_lookup(v: &SqlValue) -> RowidEqLookup {
    match v {
        SqlValue::Int(i) => RowidEqLookup::Exact(*i),
        SqlValue::Real(r) => match real_to_exact_i64(*r) {
            Some(i) => RowidEqLookup::Exact(i),
            None => RowidEqLookup::Impossible,
        },
        SqlValue::Text(b) => match apply_numeric_affinity(b, false) {
            Some(SqlValue::Int(i)) => RowidEqLookup::Exact(i),
            Some(SqlValue::Real(r)) => match real_to_exact_i64(r) {
                Some(i) => RowidEqLookup::Exact(i),
                None => RowidEqLookup::Impossible,
            },
            _ => RowidEqLookup::Impossible,
        },
        SqlValue::Null | SqlValue::Blob(_) => RowidEqLookup::Impossible,
    }
}

/// Returns `Some(Some(i64_lo))` for a valid integer lower bound, `Some(None)` if empty
/// (e.g., `rowid > i64::MAX` or `rowid >= NULL`), or `None` to fall back to a full scan.
fn coerce_rowid_lower_bound(v: &SqlValue, inclusive: bool) -> Option<Option<i64>> {
    match v {
        SqlValue::Null => Some(None),
        SqlValue::Int(i) => {
            if inclusive {
                Some(Some(*i))
            } else {
                Some(i.checked_add(1))
            }
        }
        SqlValue::Real(r) => {
            let i = real_to_exact_i64(*r)?;
            if inclusive {
                Some(Some(i))
            } else {
                Some(i.checked_add(1))
            }
        }
        SqlValue::Text(b) => match apply_numeric_affinity(b, false)? {
            SqlValue::Int(i) => {
                if inclusive {
                    Some(Some(i))
                } else {
                    Some(i.checked_add(1))
                }
            }
            SqlValue::Real(r) => {
                let i = real_to_exact_i64(r)?;
                if inclusive {
                    Some(Some(i))
                } else {
                    Some(i.checked_add(1))
                }
            }
            _ => None,
        },
        SqlValue::Blob(_) => None,
    }
}

/// Returns `Some(Some(i64_hi))` for a valid integer upper bound, `Some(None)` if empty,
/// or `None` to fall back to a full scan.
fn coerce_rowid_upper_bound(v: &SqlValue, inclusive: bool) -> Option<Option<i64>> {
    match v {
        SqlValue::Null => Some(None),
        SqlValue::Int(i) => {
            if inclusive {
                Some(Some(*i))
            } else {
                Some(i.checked_sub(1))
            }
        }
        SqlValue::Real(r) => {
            let i = real_to_exact_i64(*r)?;
            if inclusive {
                Some(Some(i))
            } else {
                Some(i.checked_sub(1))
            }
        }
        SqlValue::Text(b) => match apply_numeric_affinity(b, false)? {
            SqlValue::Int(i) => {
                if inclusive {
                    Some(Some(i))
                } else {
                    Some(i.checked_sub(1))
                }
            }
            SqlValue::Real(r) => {
                let i = real_to_exact_i64(r)?;
                if inclusive {
                    Some(Some(i))
                } else {
                    Some(i.checked_sub(1))
                }
            }
            _ => None,
        },
        SqlValue::Blob(_) => None,
    }
}

fn resolve_unique_col_in_side(
    c: &xdb_parser::ColumnRef,
    side_cols: &[ScopeColumn],
    other_cols: &[ScopeColumn],
) -> Option<usize> {
    if c.schema.is_some() {
        return None;
    }
    let matches_col = |sc: &ScopeColumn| -> bool {
        if !sc.name.eq_ignore_ascii_case(&c.column.name) {
            return false;
        }
        match &c.table {
            Some(t) => sc
                .table
                .as_deref()
                .is_some_and(|st| st.eq_ignore_ascii_case(&t.name)),
            None => true,
        }
    };
    let mut found: Option<usize> = None;
    for (i, sc) in side_cols.iter().enumerate() {
        if matches_col(sc) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    let idx = found?;
    if other_cols.iter().any(matches_col) {
        return None;
    }
    Some(idx)
}

fn detect_update_from_equijoin(
    target_cols: &[ScopeColumn],
    extra_cols: &[ScopeColumn],
    where_expr: &Expr,
) -> Option<(usize, usize, Affinity)> {
    let mut conjuncts = Vec::new();
    collect_dml_and_conjuncts(where_expr, &mut conjuncts);
    for conj in conjuncts {
        let Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            ..
        } = conj
        else {
            continue;
        };
        let (Expr::Column(lc), Expr::Column(rc)) = (left.as_ref(), right.as_ref()) else {
            continue;
        };
        let pair = if let (Some(ti), Some(ei)) = (
            resolve_unique_col_in_side(lc, target_cols, extra_cols),
            resolve_unique_col_in_side(rc, extra_cols, target_cols),
        ) {
            Some((ti, ei, true))
        } else if let (Some(ti), Some(ei)) = (
            resolve_unique_col_in_side(rc, target_cols, extra_cols),
            resolve_unique_col_in_side(lc, extra_cols, target_cols),
        ) {
            Some((ti, ei, false))
        } else {
            None
        };
        let Some((ti, ei, target_on_left)) = pair else {
            continue;
        };
        let tcol = &target_cols[ti];
        let ecol = &extra_cols[ei];
        let t_oc = Some(OperandCollation {
            name: Some(&tcol.collation),
            explicit: false,
        });
        let e_oc = Some(OperandCollation {
            name: Some(&ecol.collation),
            explicit: false,
        });
        let eff_coll = if target_on_left {
            comparison_collation_name(t_oc, e_oc)
        } else {
            comparison_collation_name(e_oc, t_oc)
        }
        .unwrap_or("BINARY");
        if !eff_coll.eq_ignore_ascii_case("BINARY") {
            continue;
        }
        let aff = comparison_affinity(Some(tcol.affinity), Some(ecol.affinity));
        return Some((ti, ei, aff));
    }
    None
}

fn update_join_hash_key(v: &SqlValue, aff: Affinity) -> Option<Vec<u8>> {
    let owned;
    let val = if aff >= Affinity::Numeric {
        if let SqlValue::Text(b) = v {
            if let Some(num) = apply_numeric_affinity(b, false) {
                owned = num;
                &owned
            } else {
                v
            }
        } else {
            v
        }
    } else {
        v
    };
    match val {
        SqlValue::Null => None,
        SqlValue::Int(i) => {
            let mut k = Vec::with_capacity(9);
            k.push(1);
            k.extend_from_slice(&i.to_le_bytes());
            Some(k)
        }
        SqlValue::Real(r) => {
            if r.is_nan() {
                return None;
            }
            if let Some(i) = real_to_exact_i64(*r) {
                let mut k = Vec::with_capacity(9);
                k.push(1);
                k.extend_from_slice(&i.to_le_bytes());
                return Some(k);
            }
            let bits = if *r == 0.0 {
                0.0f64.to_bits()
            } else {
                r.to_bits()
            };
            let mut k = Vec::with_capacity(9);
            k.push(2);
            k.extend_from_slice(&bits.to_le_bytes());
            Some(k)
        }
        SqlValue::Text(b) => {
            let mut k = Vec::with_capacity(1 + b.len());
            k.push(3);
            k.extend_from_slice(b);
            Some(k)
        }
        SqlValue::Blob(b) => {
            let mut k = Vec::with_capacity(1 + b.len());
            k.push(4);
            k.extend_from_slice(b);
            Some(k)
        }
    }
}

/// Compute the total positional parameter count consumed by a single statement,
/// matching `xdb_parser`'s exact SQLite `n_var` numbering (`?`, `?N`, `:name`/`@name`/`$name`).
fn count_stmt_positional_params(sql: &str, dialect: Dialect) -> usize {
    let mut n_var = 0usize;
    let mut seen_named: HashSet<&str> = HashSet::new();
    for tok in xdb_parser::raw_tokens(sql, dialect) {
        if tok.kind != xdb_parser::tk::VARIABLE {
            continue;
        }
        let z = &sql[tok.start..tok.end];
        let zb = z.as_bytes();
        if zb.len() > 1 && zb[0] == b'#' && zb[1].is_ascii_digit() {
            continue;
        }
        if zb.len() == 1 {
            n_var = n_var.saturating_add(1);
        } else if zb[0] == b'?' {
            if let Ok(v) = z[1..].parse::<usize>() {
                if v > n_var {
                    n_var = v;
                }
            }
        } else if seen_named.insert(z) {
            n_var = n_var.saturating_add(1);
        }
    }
    n_var
}

fn expr_has_variables(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) => true,
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            if let Expr::InSelect { operand, .. } = expr {
                if expr_has_variables(operand) {
                    return true;
                }
            }
            select_has_variables(select)
        }
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .any(expr_has_variables),
    }
}

fn from_has_variables(from: &xdb_parser::FromSource) -> bool {
    match from {
        xdb_parser::FromSource::Table(_) => false,
        xdb_parser::FromSource::Function(f) => f.args.iter().any(expr_has_variables),
        xdb_parser::FromSource::Subquery(s) => select_has_variables(&s.select),
        xdb_parser::FromSource::Group(g) => from_has_variables(&g.source),
        xdb_parser::FromSource::Join {
            left, right, on, ..
        } => {
            from_has_variables(left)
                || from_has_variables(right)
                || on.as_deref().is_some_and(expr_has_variables)
        }
    }
}

fn select_body_has_variables(body: &SelectBody) -> bool {
    match body {
        SelectBody::Core(core) => {
            if core.from.as_ref().is_some_and(from_has_variables) {
                return true;
            }
            for rc in &core.columns {
                match rc {
                    ResultColumn::Expr(re) => {
                        if expr_has_variables(&re.expr) {
                            return true;
                        }
                    }
                    ResultColumn::Star(st) => {
                        if st
                            .replace
                            .as_ref()
                            .is_some_and(|reps| reps.iter().any(|r| expr_has_variables(&r.expr)))
                        {
                            return true;
                        }
                    }
                }
            }
            if core.where_clause.as_ref().is_some_and(expr_has_variables)
                || core
                    .group_by
                    .as_ref()
                    .is_some_and(|gb| gb.iter().any(expr_has_variables))
                || core.having.as_ref().is_some_and(expr_has_variables)
            {
                return true;
            }
            if let Some(ref wins) = core.windows {
                for w in wins {
                    if w.window
                        .partition_by
                        .as_ref()
                        .is_some_and(|pb| pb.iter().any(expr_has_variables))
                        || w.window
                            .order_by
                            .as_ref()
                            .is_some_and(|ob| ob.iter().any(|o| expr_has_variables(&o.expr)))
                    {
                        return true;
                    }
                }
            }
            false
        }
        SelectBody::Values(v) => v
            .rows
            .iter()
            .any(|r| r.items.iter().any(expr_has_variables)),
        SelectBody::Compound { left, right, .. } => {
            select_body_has_variables(left) || select_body_has_variables(right)
        }
    }
}

fn select_has_variables(sel: &SelectStmt) -> bool {
    if let Some(ref with) = sel.with {
        if with.ctes.iter().any(|c| select_has_variables(&c.select)) {
            return true;
        }
    }
    if select_body_has_variables(&sel.body) {
        return true;
    }
    if sel
        .order_by
        .as_ref()
        .is_some_and(|ob| ob.iter().any(|o| expr_has_variables(&o.expr)))
    {
        return true;
    }
    if let Some(ref lim) = sel.limit {
        if expr_has_variables(&lim.count)
            || lim.offset.as_deref().is_some_and(expr_has_variables)
        {
            return true;
        }
    }
    false
}

fn prefix_has_null(vals: &[SqlValue]) -> bool {
    vals.iter().any(SqlValue::is_null)
}

fn coerce_ipk_value(v: &SqlValue) -> SqliteResult<i64> {
    must_be_int(v).ok_or_else(|| SqliteError::Sql("datatype mismatch".to_string()))
}

fn validate_strict_value(
    v: SqlValue,
    decl_type: &str,
    table_name: &str,
    col_name: &str,
) -> SqliteResult<SqlValue> {
    if v.is_null() {
        return Ok(v);
    }
    let dt = decl_type.trim().to_ascii_uppercase();
    match dt.as_str() {
        "ANY" => Ok(v),
        "INT" | "INTEGER" => {
            let coerced = apply_affinity(v, Affinity::Integer);
            if coerced.storage_class() == StorageClass::Integer {
                Ok(coerced)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in {dt} column {table_name}.{col_name}",
                    storage_label(&coerced)
                )))
            }
        }
        "REAL" => {
            let coerced = apply_affinity(v, Affinity::Real);
            if coerced.storage_class() == StorageClass::Real {
                Ok(coerced)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in REAL column {table_name}.{col_name}",
                    storage_label(&coerced)
                )))
            }
        }
        "TEXT" => {
            if v.storage_class() == StorageClass::Text {
                Ok(v)
            } else {
                let coerced = apply_affinity(v, Affinity::Text);
                if coerced.storage_class() == StorageClass::Text {
                    Ok(coerced)
                } else {
                    Err(SqliteError::Sql(format!(
                        "cannot store {} value in TEXT column {table_name}.{col_name}",
                        storage_label(&coerced)
                    )))
                }
            }
        }
        "BLOB" => {
            if v.storage_class() == StorageClass::Blob {
                Ok(v)
            } else {
                Err(SqliteError::Sql(format!(
                    "cannot store {} value in BLOB column {table_name}.{col_name}",
                    storage_label(&v)
                )))
            }
        }
        _ => Ok(v),
    }
}

fn storage_label(v: &SqlValue) -> &'static str {
    match v {
        SqlValue::Null => "NULL",
        SqlValue::Int(_) => "INT",
        SqlValue::Real(_) => "REAL",
        SqlValue::Text(_) => "TEXT",
        SqlValue::Blob(_) => "BLOB",
    }
}

fn eval_standalone_expr(
    expr: &Expr,
    params: &QueryParams,
    qopts: &QueryOptions,
) -> SqliteResult<SqlValue> {
    let no_sub = NoSubqueries;
    let mut env = EvalEnv::new(qopts, params, &no_sub);
    env.dqs = true;
    eval_expr(expr, &[], &env).map_err(SqliteError::from)
}

fn count_synth_rowid_aliases(table: &TableInfo) -> usize {
    if table.without_rowid {
        return 0;
    }
    let mut n = 0;
    for rname in ["rowid", "_rowid_", "oid"] {
        if !table
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(rname))
        {
            n += 1;
        }
    }
    n
}

fn build_table_scope_columns(table: &TableInfo, alias: &str) -> Vec<ScopeColumn> {
    let mut cols: Vec<ScopeColumn> = table
        .columns
        .iter()
        .map(|c| ScopeColumn {
            table: Some(alias.to_string()),
            name: c.name.clone(),
            affinity: c.affinity,
            collation: c.collation.clone(),
            is_rowid_alias: false,
        })
        .collect();
    if !table.without_rowid {
        for rname in ["rowid", "_rowid_", "oid"] {
            if !table
                .columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(rname))
            {
                cols.push(ScopeColumn {
                    table: Some(alias.to_string()),
                    name: rname.to_string(),
                    affinity: Affinity::Integer,
                    collation: "BINARY".to_string(),
                    is_rowid_alias: true,
                });
            }
        }
    }
    cols
}

fn build_upsert_scope_columns(table: &TableInfo, alias: &str) -> Vec<ScopeColumn> {
    let mut cols = build_table_scope_columns(table, alias);
    for c in &table.columns {
        cols.push(ScopeColumn {
            table: Some("excluded".to_string()),
            name: c.name.clone(),
            affinity: c.affinity,
            collation: c.collation.clone(),
            is_rowid_alias: false,
        });
    }
    cols
}

fn validate_set_targets(table: &TableInfo, sets: &[SetClause]) -> SqliteResult<()> {
    for sc in sets {
        for c in &sc.columns {
            let cname = &c.name;
            if let Some(cid) = table
                .columns
                .iter()
                .position(|tc| tc.name.eq_ignore_ascii_case(cname))
            {
                if table.columns[cid].generated.is_some() {
                    return Err(SqliteError::Sql(format!(
                        "cannot UPDATE generated column \"{cname}\""
                    )));
                }
            } else if !table.without_rowid
                && matches!(
                    cname.to_ascii_lowercase().as_str(),
                    "rowid" | "_rowid_" | "oid"
                )
            {
                // Valid rowid alias target.
            } else {
                return Err(SqliteError::Sql(format!("no such column: {cname}")));
            }
        }
    }
    Ok(())
}

fn apply_set_clauses(
    table: &TableInfo,
    old_rowid: Option<i64>,
    old_row: &[SqlValue],
    sets: &[SetClause],
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> SqliteResult<(Option<i64>, Vec<SqlValue>)> {
    let mut new_row = old_row.to_vec();
    let mut new_rowid = old_rowid;

    for sc in sets {
        if sc.columns.len() == 1 {
            let cname = &sc.columns[0].name;
            let val = eval_expr(&sc.expr, scopes, env)?;
            assign_column(table, cname, val, &mut new_rowid, &mut new_row)?;
        } else if let Expr::Vector { items, .. } = &sc.expr {
            if items.len() != sc.columns.len() {
                return Err(SqliteError::Sql(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    items.len()
                )));
            }
            for (c, e) in sc.columns.iter().zip(items.iter()) {
                let val = eval_expr(e, scopes, env)?;
                assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)?;
            }
        } else if let Expr::Subquery { select, .. } = &sc.expr {
            let (cols, rows) = env.subqueries.run_select(select, scopes)?;
            if cols.len() != sc.columns.len() {
                return Err(SqliteError::Sql(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    cols.len()
                )));
            }
            if let Some(first_row) = rows.into_iter().next() {
                for (c, val) in sc.columns.iter().zip(first_row.into_iter()) {
                    assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)?;
                }
            } else {
                for c in &sc.columns {
                    assign_column(table, &c.name, SqlValue::Null, &mut new_rowid, &mut new_row)?;
                }
            }
        } else {
            return Err(SqliteError::Sql(
                "multi-column SET requires row value".to_string(),
            ));
        }
    }
    Ok((new_rowid, new_row))
}

fn assign_column(
    table: &TableInfo,
    cname: &str,
    val: SqlValue,
    new_rowid: &mut Option<i64>,
    new_row: &mut [SqlValue],
) -> SqliteResult<()> {
    if let Some(cid) = table
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(cname))
    {
        if table.columns[cid].generated.is_some() {
            return Err(SqliteError::Sql(format!(
                "cannot UPDATE generated column \"{cname}\""
            )));
        }
        if Some(cid) == table.ipk_column && !table.without_rowid {
            let rid = coerce_ipk_value(&val)?;
            *new_rowid = Some(rid);
            new_row[cid] = SqlValue::Int(rid);
        } else {
            new_row[cid] = val;
        }
        return Ok(());
    }
    if !table.without_rowid
        && matches!(
            cname.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
        )
    {
        let rid = coerce_ipk_value(&val)?;
        *new_rowid = Some(rid);
        if let Some(ipk) = table.ipk_column {
            new_row[ipk] = SqlValue::Int(rid);
        }
        return Ok(());
    }
    Err(SqliteError::Sql(format!("no such column: {cname}")))
}

fn unwrap_collate(mut e: &Expr) -> &Expr {
    while let Expr::Collate { operand, .. } = e {
        e = operand;
    }
    e
}

fn outermost_collate(e: &Expr) -> Option<&str> {
    match e {
        Expr::Collate { collation, .. } => Some(&collation.name),
        _ => None,
    }
}

fn expr_structural_eq_ci(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (
            Expr::Collate {
                operand: oa,
                collation: ca,
                ..
            },
            Expr::Collate {
                operand: ob,
                collation: cb,
                ..
            },
        ) => ca.name.eq_ignore_ascii_case(&cb.name) && expr_structural_eq_ci(oa, ob),
        (Expr::Column(ca), Expr::Column(cb)) => {
            ca.column.name.eq_ignore_ascii_case(&cb.column.name)
        }
        (Expr::Number(na), Expr::Number(nb)) => na.text == nb.text,
        (Expr::String(sa), Expr::String(sb)) => sa.value == sb.value,
        (Expr::Blob(ba), Expr::Blob(bb)) => ba.value == bb.value,
        (Expr::Null { .. }, Expr::Null { .. }) => true,
        (Expr::Boolean { value: va, .. }, Expr::Boolean { value: vb, .. }) => va == vb,
        (
            Expr::Unary {
                op: oa,
                operand: xa,
                ..
            },
            Expr::Unary {
                op: ob,
                operand: xb,
                ..
            },
        ) => oa == ob && expr_structural_eq_ci(xa, xb),
        (
            Expr::IsNull {
                not: na,
                operand: xa,
                ..
            },
            Expr::IsNull {
                not: nb,
                operand: xb,
                ..
            },
        ) => na == nb && expr_structural_eq_ci(xa, xb),
        (
            Expr::Binary {
                op: oa,
                left: la,
                right: ra,
                ..
            },
            Expr::Binary {
                op: ob,
                left: lb,
                right: rb,
                ..
            },
        ) => oa == ob && expr_structural_eq_ci(la, lb) && expr_structural_eq_ci(ra, rb),
        (
            Expr::Between {
                not: na,
                operand: oa,
                low: la,
                high: ha,
                ..
            },
            Expr::Between {
                not: nb,
                operand: ob,
                low: lb,
                high: hb,
                ..
            },
        ) => {
            na == nb
                && expr_structural_eq_ci(oa, ob)
                && expr_structural_eq_ci(la, lb)
                && expr_structural_eq_ci(ha, hb)
        }
        (
            Expr::InList {
                not: na,
                operand: oa,
                items: ia,
                ..
            },
            Expr::InList {
                not: nb,
                operand: ob,
                items: ib,
                ..
            },
        ) => {
            na == nb
                && ia.len() == ib.len()
                && expr_structural_eq_ci(oa, ob)
                && ia
                    .iter()
                    .zip(ib)
                    .all(|(x, y)| expr_structural_eq_ci(x, y))
        }
        (
            Expr::Like {
                op: oa,
                not: na,
                left: la,
                right: ra,
                escape: ea,
                ..
            },
            Expr::Like {
                op: ob,
                not: nb,
                left: lb,
                right: rb,
                escape: eb,
                ..
            },
        ) => {
            oa == ob
                && na == nb
                && expr_structural_eq_ci(la, lb)
                && expr_structural_eq_ci(ra, rb)
                && match (ea, eb) {
                    (None, None) => true,
                    (Some(x), Some(y)) => expr_structural_eq_ci(x, y),
                    _ => false,
                }
        }
        (Expr::Function(fa), Expr::Function(fb)) => {
            fa.name.name.eq_ignore_ascii_case(&fb.name.name)
                && fa.star == fb.star
                && fa.quantifier == fb.quantifier
                && fa.args.len() == fb.args.len()
                && fa
                    .args
                    .iter()
                    .zip(&fb.args)
                    .all(|(x, y)| expr_structural_eq_ci(x, y))
        }
        (
            Expr::Cast {
                operand: oa,
                type_name: ta,
                ..
            },
            Expr::Cast {
                operand: ob,
                type_name: tb,
                ..
            },
        ) => {
            expr_structural_eq_ci(oa, ob)
                && ta.as_ref().map(|t| t.text.to_ascii_uppercase())
                    == tb.as_ref().map(|t| t.text.to_ascii_uppercase())
        }
        _ => false,
    }
}

fn validate_expr_columns_exist(expr: &Expr, table: &TableInfo) -> SqliteResult<()> {
    if let Expr::Column(c) = expr {
        let name = &c.column.name;
        let exists = table
            .columns
            .iter()
            .any(|col| col.name.eq_ignore_ascii_case(name))
            || (!table.without_rowid
                && (name.eq_ignore_ascii_case("rowid")
                    || name.eq_ignore_ascii_case("_rowid_")
                    || name.eq_ignore_ascii_case("oid")));
        if !exists {
            return Err(SqliteError::Sql(format!("no such column: {name}")));
        }
    }
    for ch in xdb_parser::expr_children(expr) {
        validate_expr_columns_exist(ch, table)?;
    }
    Ok(())
}

fn upsert_target_matches_ipk(
    table: &TableInfo,
    target: &[xdb_parser::OrderingTerm],
) -> bool {
    if target.len() != 1 {
        return false;
    }
    let Some(ipk) = table.ipk_column else {
        return false;
    };
    let term = &target[0];
    if let Some(explicit_coll) = outermost_collate(&term.expr) {
        if !explicit_coll.eq_ignore_ascii_case("BINARY") {
            return false;
        }
    }
    matches!(
        unwrap_collate(&term.expr),
        Expr::Column(c) if c.column.name.eq_ignore_ascii_case(&table.columns[ipk].name)
    )
}

fn upsert_target_matches_index(
    target: &[xdb_parser::OrderingTerm],
    target_where: Option<&Expr>,
    ix: &IndexInfo,
) -> bool {
    if !ix.unique || ix.key_count != target.len() {
        return false;
    }
    // If the index is partial, the UPSERT target MUST supply an equivalent WHERE predicate.
    // A non-partial UNIQUE index matches both conditional and unconditional targets.
    if let Some(ref ix_where) = ix.definition.where_clause {
        match target_where {
            Some(tw) if expr_structural_eq_ci(tw, ix_where) => {}
            _ => return false,
        }
    }
    // Match each target term against a distinct key slot of `ix` (also verifying explicit COLLATE).
    let mut used = vec![false; ix.key_count];
    for term in target {
        let explicit_coll = outermost_collate(&term.expr);
        let mut found = false;
        for (k, ic) in ix.columns[..ix.key_count].iter().enumerate() {
            if used[k] {
                continue;
            }
            if let Some(ec) = explicit_coll {
                if !ec.eq_ignore_ascii_case(&ic.collation) {
                    continue;
                }
            }
            let matched = if ic.cid >= 0 {
                matches!(
                    unwrap_collate(&term.expr),
                    Expr::Column(c) if ic.name.as_ref().is_some_and(|n| n.eq_ignore_ascii_case(&c.column.name))
                )
            } else if let Some(ref ix_expr) = ix.definition.columns[k].expr {
                expr_structural_eq_ci(unwrap_collate(&term.expr), unwrap_collate(ix_expr))
            } else {
                false
            };
            if matched {
                used[k] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

fn validate_upsert_targets(
    table: &TableInfo,
    upserts: &[xdb_parser::UpsertClause],
) -> SqliteResult<()> {
    for up in upserts {
        let Some(ref target) = up.target else {
            continue;
        };
        if target.is_empty() {
            return Err(SqliteError::Sql(
                "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint"
                    .to_string(),
            ));
        }
        for term in target {
            validate_expr_columns_exist(&term.expr, table)?;
        }
        if let Some(ref tw) = up.target_where {
            validate_expr_columns_exist(tw, table)?;
        }
        let matches_any = upsert_target_matches_ipk(table, target)
            || table
                .indexes
                .iter()
                .any(|ix| upsert_target_matches_index(target, up.target_where.as_ref(), ix));
        if !matches_any {
            return Err(SqliteError::Sql(
                "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn upsert_target_matches(
    table: &TableInfo,
    up: &xdb_parser::UpsertClause,
    conflicts: &[(Option<i64>, Vec<SqlValue>, String, OnError)],
) -> Option<usize> {
    if conflicts.is_empty() {
        return None;
    }
    let Some(ref target) = up.target else {
        return Some(0); // `ON CONFLICT DO NOTHING` / catch-all matches any conflict
    };
    if target.is_empty() {
        return None;
    }
    for (idx, (_, _, label, _)) in conflicts.iter().enumerate() {
        if upsert_target_matches_ipk(table, target) {
            if let Some(ipk) = table.ipk_column {
                let ipk_name = &table.columns[ipk].name;
                if label.eq_ignore_ascii_case(&format!("{}.{ipk_name}", table.name)) {
                    return Some(idx);
                }
            }
        }
        for ix in &table.indexes {
            if !upsert_target_matches_index(target, up.target_where.as_ref(), ix) {
                continue;
            }
            let label_matches = label.eq_ignore_ascii_case(&format!("{}.{}", table.name, ix.name))
                || (ix.without_rowid_primary_key && label.ends_with("PRIMARY KEY"));
            if label_matches {
                return Some(idx);
            }
        }
    }
    None
}

fn returning_column_names(table: &TableInfo, ret: &[ResultColumn]) -> SqliteResult<Vec<String>> {
    let mut names = Vec::new();
    for rc in ret {
        match rc {
            ResultColumn::Star(s) => {
                if s.table.is_some() {
                    return Err(SqliteError::Sql(
                        "RETURNING may not use \"TABLE.*\" wildcards".to_string(),
                    ));
                }
                for c in &table.columns {
                    names.push(c.name.clone());
                }
            }
            ResultColumn::Expr(re) => {
                let name = match &re.alias {
                    Some(a) => a.name.clone(),
                    None => match &re.expr {
                        Expr::Column(c) => c.column.name.clone(),
                        _ => re.text.trim().to_string(),
                    },
                };
                names.push(name);
            }
        }
    }
    Ok(names)
}

fn expr_contains_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery { .. }
        | Expr::Exists { .. }
        | Expr::InSelect { .. }
        | Expr::InTable { .. } => true,
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .any(expr_contains_subquery),
    }
}

fn returning_has_subqueries(ret: &[ResultColumn]) -> bool {
    ret.iter().any(|rc| match rc {
        ResultColumn::Expr(re) => expr_contains_subquery(&re.expr),
        ResultColumn::Star(_) => false,
    })
}

// ───────────────────────────── DDL SQL rewrite helpers ─────────────────────────────

fn normalize_stored_ddl(raw_sql: &str, has_if_not_exists: bool, has_schema: bool) -> String {
    // Match SQLite's `sqlite_schema.sql` normalization:
    // 1. Strip leading whitespace/comments and trailing whitespace/comments/`;`.
    // 2. Remove the header `IF NOT EXISTS` clause when present in the AST.
    // 3. Strip leading `<schema>.` qualifier before the created object name.
    let raw_tokens = xdb_parser::raw_tokens(raw_sql, Dialect::Sqlite);
    let mut first_idx = 0;
    while first_idx < raw_tokens.len()
        && matches!(
            raw_tokens[first_idx].kind,
            xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
        )
    {
        first_idx += 1;
    }
    let mut last_idx = raw_tokens.len();
    while last_idx > first_idx
        && matches!(
            raw_tokens[last_idx - 1].kind,
            xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT | xdb_parser::tk::SEMI
        )
    {
        last_idx -= 1;
    }
    let mut sql = if first_idx < last_idx {
        raw_sql[raw_tokens[first_idx].start..raw_tokens[last_idx - 1].end].to_string()
    } else {
        raw_sql.trim().to_string()
    };

    if has_if_not_exists {
        let tokens = xdb_parser::raw_tokens(&sql, Dialect::Sqlite);
        let mut i = 0;
        while i + 2 < tokens.len() {
            if tokens[i].kind == xdb_parser::tk::LP || tokens[i].kind == xdb_parser::tk::AS {
                break;
            }
            if sql[tokens[i].start..tokens[i].end].eq_ignore_ascii_case("IF") {
                let mut j = i + 1;
                while j < tokens.len()
                    && matches!(
                        tokens[j].kind,
                        xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                    )
                {
                    j += 1;
                }
                if j < tokens.len()
                    && sql[tokens[j].start..tokens[j].end].eq_ignore_ascii_case("NOT")
                {
                    let mut k = j + 1;
                    while k < tokens.len()
                        && matches!(
                            tokens[k].kind,
                            xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                        )
                    {
                        k += 1;
                    }
                    if k < tokens.len()
                        && sql[tokens[k].start..tokens[k].end].eq_ignore_ascii_case("EXISTS")
                    {
                        let mut after = tokens[k].end;
                        while after < sql.len() && sql.as_bytes()[after].is_ascii_whitespace() {
                            after += 1;
                        }
                        let mut out = String::with_capacity(sql.len());
                        out.push_str(&sql[..tokens[i].start]);
                        out.push_str(&sql[after..]);
                        sql = out;
                        break;
                    }
                }
            }
            i += 1;
        }
    }

    if has_schema {
        let tokens = xdb_parser::raw_tokens(&sql, Dialect::Sqlite);
        let mut i = 0;
        while i < tokens.len() {
            if matches!(
                tokens[i].kind,
                xdb_parser::tk::LP | xdb_parser::tk::AS | xdb_parser::tk::ON
            ) {
                break;
            }
            if tokens[i].kind == xdb_parser::tk::DOT {
                // Find the schema name token immediately before `.` and the object name token after `.`.
                let mut prev = i;
                let mut schema_start = None;
                while prev > 0 {
                    prev -= 1;
                    if !matches!(
                        tokens[prev].kind,
                        xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                    ) {
                        schema_start = Some(tokens[prev].start);
                        break;
                    }
                }
                let mut next = i + 1;
                let mut obj_start = None;
                while next < tokens.len() {
                    if !matches!(
                        tokens[next].kind,
                        xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT
                    ) {
                        obj_start = Some(tokens[next].start);
                        break;
                    }
                    next += 1;
                }
                if let (Some(s_start), Some(o_start)) = (schema_start, obj_start) {
                    let mut out = String::with_capacity(sql.len());
                    out.push_str(&sql[..s_start]);
                    out.push_str(&sql[o_start..]);
                    sql = out;
                }
                break;
            }
            i += 1;
        }
    }

    sql
}

fn quote_ident_if_needed(name: &str) -> String {
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
        && xdb_parser::keyword_code(name.as_bytes()) == xdb_parser::tk::ID
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn rewrite_create_table_name(ddl: &str, new_name: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.name.span;
    let mut out = String::with_capacity(ddl.len() + new_name.len());
    out.push_str(&ddl[..sp.start]);
    out.push_str(&quote_ident_if_needed(new_name));
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

fn collect_expr_table_ref_spans(expr: &Expr, old_name: &str, out: &mut Vec<xdb_parser::Span>) {
    if let Expr::Column(c) = expr {
        if let Some(ref t) = c.table {
            if t.name.eq_ignore_ascii_case(old_name) {
                out.push(t.span);
            }
        }
    }
    for child in xdb_parser::expr_children(expr) {
        collect_expr_table_ref_spans(child, old_name, out);
    }
}

fn rewrite_on_table_name(ddl: &str, old_name: &str, new_name: &str) -> String {
    match xdb_parser::parse_statement(ddl, &ParseOptions::default()) {
        Ok(Statement::CreateIndex(ci)) => {
            let mut spans = vec![ci.table.span];
            for ot in &ci.columns {
                collect_expr_table_ref_spans(&ot.expr, old_name, &mut spans);
            }
            if let Some(ref we) = ci.where_clause {
                collect_expr_table_ref_spans(we, old_name, &mut spans);
            }
            let quoted = quote_ident_if_needed(new_name);
            apply_span_replacements(ddl, spans, &quoted)
        }
        Ok(Statement::CreateTrigger(ct)) => {
            if ct.table.name.eq_ignore_ascii_case(old_name) {
                let quoted = quote_ident_if_needed(new_name);
                apply_span_replacements(ddl, vec![ct.table.span], &quoted)
            } else {
                ddl.to_string()
            }
        }
        _ => ddl.to_string(),
    }
}

fn append_column_to_create_table(ddl: &str, col_sql: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    // Insert `, <col_sql>` right after the last column definition (BEFORE any trailing
    // table constraints like `UNIQUE (...)` / `PRIMARY KEY (...)` / `FOREIGN KEY (...)`,
    // which must follow the column list in SQLite's CREATE TABLE grammar).
    let last_end = ct
        .columns
        .last()
        .map(|c| c.span.end)
        .ok_or_else(|| SqliteError::corrupt("empty CREATE TABLE"))?;
    let mut out = String::with_capacity(ddl.len() + col_sql.len() + 2);
    out.push_str(&ddl[..last_end]);
    out.push_str(", ");
    out.push_str(col_sql);
    out.push_str(&ddl[last_end..]);
    Ok(out)
}

fn collect_expr_col_ref_spans(
    expr: &Expr,
    table_name: &str,
    old_col: &str,
    out: &mut Vec<xdb_parser::Span>,
) {
    match expr {
        Expr::Column(c) => {
            let tbl_ok = c.table.is_none()
                || c.table
                    .as_ref()
                    .is_some_and(|t| t.name.eq_ignore_ascii_case(table_name));
            if tbl_ok && c.column.name.eq_ignore_ascii_case(old_col) {
                out.push(c.column.span);
            }
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_col_ref_spans(left, table_name, old_col, out);
            collect_expr_col_ref_spans(right, table_name, old_col, out);
        }
        Expr::Like {
            left,
            right,
            escape,
            ..
        } => {
            collect_expr_col_ref_spans(left, table_name, old_col, out);
            collect_expr_col_ref_spans(right, table_name, old_col, out);
            if let Some(e) = escape {
                collect_expr_col_ref_spans(e, table_name, old_col, out);
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
            collect_expr_col_ref_spans(low, table_name, old_col, out);
            collect_expr_col_ref_spans(high, table_name, old_col, out);
        }
        Expr::InList { operand, items, .. } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
            for item in items {
                collect_expr_col_ref_spans(item, table_name, old_col, out);
            }
        }
        Expr::InTable { operand, args, .. } => {
            collect_expr_col_ref_spans(operand, table_name, old_col, out);
            if let Some(arg_list) = args {
                for a in arg_list {
                    collect_expr_col_ref_spans(a, table_name, old_col, out);
                }
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                collect_expr_col_ref_spans(op, table_name, old_col, out);
            }
            for w in whens {
                collect_expr_col_ref_spans(&w.when, table_name, old_col, out);
                collect_expr_col_ref_spans(&w.then, table_name, old_col, out);
            }
            if let Some(el) = else_expr {
                collect_expr_col_ref_spans(el, table_name, old_col, out);
            }
        }
        Expr::Function(f) => {
            for a in &f.args {
                collect_expr_col_ref_spans(a, table_name, old_col, out);
            }
            if let Some(ref obs) = f.order_by {
                for ob in obs {
                    collect_expr_col_ref_spans(&ob.expr, table_name, old_col, out);
                }
            }
            if let Some(ref flt) = f.filter {
                collect_expr_col_ref_spans(flt, table_name, old_col, out);
            }
        }
        Expr::Vector { items, .. } => {
            for item in items {
                collect_expr_col_ref_spans(item, table_name, old_col, out);
            }
        }
        Expr::Raise {
            message: Some(m), ..
        } => {
            collect_expr_col_ref_spans(m, table_name, old_col, out);
        }
        _ => {}
    }
}

fn collect_create_table_col_refs(
    ct: &CreateTableStmt,
    table_name: &str,
    skip_cid: Option<usize>,
    old_col: &str,
    out: &mut Vec<xdb_parser::Span>,
) {
    for (i, col) in ct.columns.iter().enumerate() {
        if skip_cid == Some(i) {
            continue;
        }
        for con in &col.constraints {
            match con {
                ColumnConstraint::Check(chk) => {
                    collect_expr_col_ref_spans(&chk.expr, table_name, old_col, out);
                }
                ColumnConstraint::Generated { expr, .. } => {
                    collect_expr_col_ref_spans(expr, table_name, old_col, out);
                }
                ColumnConstraint::References { clause: fk, .. } => {
                    if fk.table.name.eq_ignore_ascii_case(table_name) {
                        if let Some(ref cols) = fk.columns {
                            for item in cols {
                                if item.name.name.eq_ignore_ascii_case(old_col) {
                                    out.push(item.name.span);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for con in &ct.constraints {
        match con {
            TableConstraint::PrimaryKey { columns, .. }
            | TableConstraint::Unique { columns, .. } => {
                for ot in columns {
                    collect_expr_col_ref_spans(&ot.expr, table_name, old_col, out);
                }
            }
            TableConstraint::Check(chk) => {
                collect_expr_col_ref_spans(&chk.expr, table_name, old_col, out);
            }
            TableConstraint::ForeignKey {
                columns,
                clause: foreign_key,
                ..
            } => {
                for item in columns {
                    if item.name.name.eq_ignore_ascii_case(old_col) {
                        out.push(item.name.span);
                    }
                }
                if foreign_key.table.name.eq_ignore_ascii_case(table_name) {
                    if let Some(ref fk_cols) = foreign_key.columns {
                        for item in fk_cols {
                            if item.name.name.eq_ignore_ascii_case(old_col) {
                                out.push(item.name.span);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn apply_span_replacements(ddl: &str, mut spans: Vec<xdb_parser::Span>, replacement: &str) -> String {
    if spans.is_empty() {
        return ddl.to_string();
    }
    spans.sort_by(|a, b| b.start.cmp(&a.start).then_with(|| b.end.cmp(&a.end)));
    spans.dedup();
    let mut out = ddl.to_string();
    let mut last_start = usize::MAX;
    for sp in spans {
        if sp.end <= out.len() && sp.start <= sp.end && sp.end <= last_start {
            out.replace_range(sp.start..sp.end, replacement);
            last_start = sp.start;
        }
    }
    out
}

fn rewrite_column_name_in_ddl(
    ddl: &str,
    table_name: &str,
    cid: usize,
    old_col: &str,
    new_name: &str,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let mut spans = vec![ct.columns[cid].name.span];
    collect_create_table_col_refs(&ct, table_name, None, old_col, &mut spans);
    let quoted = quote_ident_if_needed(new_name);
    Ok(apply_span_replacements(ddl, spans, &quoted))
}

fn rewrite_column_name_in_index_ddl(
    ddl: &str,
    table_name: &str,
    old_col: &str,
    new_name: &str,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateIndex(ci) = stmt else {
        return Ok(ddl.to_string());
    };
    let mut spans = Vec::new();
    for ot in &ci.columns {
        collect_expr_col_ref_spans(&ot.expr, table_name, old_col, &mut spans);
    }
    if let Some(ref we) = ci.where_clause {
        collect_expr_col_ref_spans(we, table_name, old_col, &mut spans);
    }
    let quoted = quote_ident_if_needed(new_name);
    Ok(apply_span_replacements(ddl, spans, &quoted))
}

fn collect_view_table_rename_spans_from(
    from: &xdb_parser::FromSource,
    old_name: &str,
    ctes: &[String],
    spans: &mut Vec<xdb_parser::Span>,
    has_unaliased_ref: &mut bool,
    subqueries: &mut Vec<SelectStmt>,
) {
    match from {
        xdb_parser::FromSource::Table(t) => {
            let schema_ok = t.schema.is_none()
                || t.schema
                    .as_ref()
                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
            let shadowed = t.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&t.name.name));
            if schema_ok && !shadowed && t.name.name.eq_ignore_ascii_case(old_name) {
                spans.push(t.name.span);
                if t.alias.is_none() {
                    *has_unaliased_ref = true;
                }
            }
        }
        xdb_parser::FromSource::Subquery(s) => {
            subqueries.push((*s.select).clone());
        }
        xdb_parser::FromSource::Group(g) => {
            collect_view_table_rename_spans_from(
                &g.source,
                old_name,
                ctes,
                spans,
                has_unaliased_ref,
                subqueries,
            );
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            collect_view_table_rename_spans_from(
                left,
                old_name,
                ctes,
                spans,
                has_unaliased_ref,
                subqueries,
            );
            collect_view_table_rename_spans_from(
                right,
                old_name,
                ctes,
                spans,
                has_unaliased_ref,
                subqueries,
            );
        }
        xdb_parser::FromSource::Function(_) => {}
    }
}

fn collect_view_table_rename_spans_expr(
    expr: &Expr,
    old_name: &str,
    qualify_ref: bool,
    ctes: &[String],
    spans: &mut Vec<xdb_parser::Span>,
) {
    match expr {
        Expr::Column(c) => {
            if qualify_ref {
                if let Some(ref tbl) = c.table {
                    if tbl.name.eq_ignore_ascii_case(old_name) {
                        spans.push(tbl.span);
                    }
                }
            }
        }
        Expr::InTable {
            operand,
            schema,
            table,
            args,
            ..
        } => {
            collect_view_table_rename_spans_expr(operand, old_name, qualify_ref, ctes, spans);
            let schema_ok = schema.is_none()
                || schema
                    .as_ref()
                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
            let shadowed = schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&table.name));
            if schema_ok && !shadowed && table.name.eq_ignore_ascii_case(old_name) {
                spans.push(table.span);
            }
            if let Some(arg_list) = args {
                for a in arg_list {
                    collect_view_table_rename_spans_expr(a, old_name, qualify_ref, ctes, spans);
                }
            }
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            if let Expr::InSelect { operand, .. } = expr {
                collect_view_table_rename_spans_expr(operand, old_name, qualify_ref, ctes, spans);
            }
            collect_view_table_rename_spans_select(select, old_name, qualify_ref, ctes, spans);
        }
        _ => {
            for ch in xdb_parser::expr_children(expr) {
                collect_view_table_rename_spans_expr(ch, old_name, qualify_ref, ctes, spans);
            }
        }
    }
}

fn collect_view_table_rename_spans_join_on(
    from: &xdb_parser::FromSource,
    old_name: &str,
    qualify_ref: bool,
    ctes: &[String],
    spans: &mut Vec<xdb_parser::Span>,
) {
    match from {
        xdb_parser::FromSource::Join {
            left, right, on, ..
        } => {
            collect_view_table_rename_spans_join_on(left, old_name, qualify_ref, ctes, spans);
            collect_view_table_rename_spans_join_on(right, old_name, qualify_ref, ctes, spans);
            if let Some(on_e) = on {
                collect_view_table_rename_spans_expr(on_e, old_name, qualify_ref, ctes, spans);
            }
        }
        xdb_parser::FromSource::Group(g) => {
            collect_view_table_rename_spans_join_on(&g.source, old_name, qualify_ref, ctes, spans);
        }
        xdb_parser::FromSource::Function(f) => {
            for a in &f.args {
                collect_view_table_rename_spans_expr(a, old_name, qualify_ref, ctes, spans);
            }
        }
        _ => {}
    }
}

fn from_shadows_alias(
    from: &xdb_parser::FromSource,
    target_alias: &str,
    exempt_table: Option<&str>,
    ctes: &[String],
) -> bool {
    match from {
        xdb_parser::FromSource::Table(t) => {
            let exposed = t.alias.as_ref().unwrap_or(&t.name).name.as_str();
            if !exposed.eq_ignore_ascii_case(target_alias) {
                return false;
            }
            if let Some(exempt) = exempt_table {
                let schema_ok = t.schema.is_none()
                    || t.schema
                        .as_ref()
                        .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
                let cte_shadowed = t.schema.is_none()
                    && ctes.iter().any(|c| c.eq_ignore_ascii_case(&t.name.name));
                if schema_ok && !cte_shadowed && t.name.name.eq_ignore_ascii_case(exempt) {
                    return false;
                }
            }
            true
        }
        xdb_parser::FromSource::Subquery(s) => s
            .alias
            .as_ref()
            .is_some_and(|a| a.name.eq_ignore_ascii_case(target_alias)),
        xdb_parser::FromSource::Function(f) => f
            .alias
            .as_ref()
            .unwrap_or(&f.name)
            .name
            .eq_ignore_ascii_case(target_alias),
        xdb_parser::FromSource::Group(g) => {
            if let Some(ref a) = g.alias {
                a.name.eq_ignore_ascii_case(target_alias)
            } else {
                from_shadows_alias(&g.source, target_alias, exempt_table, ctes)
            }
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            from_shadows_alias(left, target_alias, exempt_table, ctes)
                || from_shadows_alias(right, target_alias, exempt_table, ctes)
        }
    }
}

fn from_contains_target_table(
    from: &xdb_parser::FromSource,
    table_name: &str,
    ctes: &[String],
) -> bool {
    match from {
        xdb_parser::FromSource::Table(t) => {
            let schema_ok = t.schema.is_none()
                || t.schema
                    .as_ref()
                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
            let shadowed = t.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&t.name.name));
            schema_ok && !shadowed && t.name.name.eq_ignore_ascii_case(table_name)
        }
        xdb_parser::FromSource::Group(g) => from_contains_target_table(&g.source, table_name, ctes),
        xdb_parser::FromSource::Join { left, right, .. } => {
            from_contains_target_table(left, table_name, ctes)
                || from_contains_target_table(right, table_name, ctes)
        }
        xdb_parser::FromSource::Subquery(_) | xdb_parser::FromSource::Function(_) => false,
    }
}

/// Returns `true` iff every source in `from` is a known catalog table in `schema` and NONE of
/// those tables defines a column named `col_name` (meaning an unqualified `col_name` inside this
/// subquery unambiguously resolves to an outer table rather than a local table).
fn from_tables_known_without_col(
    from: &xdb_parser::FromSource,
    col_name: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match from {
        xdb_parser::FromSource::Table(t) => {
            let schema_ok = t.schema.is_none()
                || t.schema
                    .as_ref()
                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
            let cte_shadowed = t.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&t.name.name));
            if !schema_ok || cte_shadowed {
                return false;
            }
            let Some(tbl) = schema.table(&t.name.name) else {
                return false;
            };
            !tbl.columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(col_name))
        }
        xdb_parser::FromSource::Group(g) => {
            from_tables_known_without_col(&g.source, col_name, ctes, schema)
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            from_tables_known_without_col(left, col_name, ctes, schema)
                && from_tables_known_without_col(right, col_name, ctes, schema)
        }
        xdb_parser::FromSource::Subquery(_) | xdb_parser::FromSource::Function(_) => false,
    }
}

fn collect_view_table_rename_spans_body(
    body: &SelectBody,
    old_name: &str,
    outer_qualify: bool,
    ctes: &[String],
    spans: &mut Vec<xdb_parser::Span>,
) -> bool {
    match body {
        SelectBody::Core(core) => {
            let mut local_unaliased = false;
            let mut subqueries = Vec::new();
            if let Some(ref from) = core.from {
                collect_view_table_rename_spans_from(
                    from,
                    old_name,
                    ctes,
                    spans,
                    &mut local_unaliased,
                    &mut subqueries,
                );
            }
            for sq in &subqueries {
                collect_view_table_rename_spans_select(sq, old_name, outer_qualify, ctes, spans);
            }
            let local_shadows = core
                .from
                .as_ref()
                .is_some_and(|f| from_shadows_alias(f, old_name, Some(old_name), ctes));
            let qualify = local_unaliased || (outer_qualify && !local_shadows);
            if let Some(ref from) = core.from {
                collect_view_table_rename_spans_join_on(from, old_name, qualify, ctes, spans);
            }
            for rc in &core.columns {
                match rc {
                    ResultColumn::Star(s) => {
                        if qualify {
                            if let Some(ref tbl) = s.table {
                                if tbl.name.eq_ignore_ascii_case(old_name) {
                                    spans.push(tbl.span);
                                }
                            }
                        }
                    }
                    ResultColumn::Expr(re) => {
                        collect_view_table_rename_spans_expr(
                            &re.expr, old_name, qualify, ctes, spans,
                        );
                    }
                }
            }
            if let Some(ref we) = core.where_clause {
                collect_view_table_rename_spans_expr(we, old_name, qualify, ctes, spans);
            }
            if let Some(ref gb) = core.group_by {
                for e in gb {
                    collect_view_table_rename_spans_expr(e, old_name, qualify, ctes, spans);
                }
            }
            if let Some(ref hv) = core.having {
                collect_view_table_rename_spans_expr(hv, old_name, qualify, ctes, spans);
            }
            if let Some(ref wins) = core.windows {
                for w in wins {
                    if let Some(ref pb) = w.window.partition_by {
                        for e in pb {
                            collect_view_table_rename_spans_expr(e, old_name, qualify, ctes, spans);
                        }
                    }
                    if let Some(ref ob) = w.window.order_by {
                        for o in ob {
                            collect_view_table_rename_spans_expr(
                                &o.expr, old_name, qualify, ctes, spans,
                            );
                        }
                    }
                }
            }
            qualify
        }
        SelectBody::Values(v) => {
            for r in &v.rows {
                for e in &r.items {
                    collect_view_table_rename_spans_expr(e, old_name, outer_qualify, ctes, spans);
                }
            }
            outer_qualify
        }
        SelectBody::Compound { left, right, .. } => {
            let lq =
                collect_view_table_rename_spans_body(left, old_name, outer_qualify, ctes, spans);
            let rq =
                collect_view_table_rename_spans_body(right, old_name, outer_qualify, ctes, spans);
            lq || rq
        }
    }
}

fn collect_view_table_rename_spans_select(
    sel: &SelectStmt,
    old_name: &str,
    outer_qualify: bool,
    outer_ctes: &[String],
    spans: &mut Vec<xdb_parser::Span>,
) {
    let mut ctes = outer_ctes.to_vec();
    if let Some(ref with) = sel.with {
        for cte in &with.ctes {
            let mut cte_scope = ctes.clone();
            if with.recursive {
                cte_scope.push(cte.name.name.clone());
            }
            collect_view_table_rename_spans_select(&cte.select, old_name, false, &cte_scope, spans);
            ctes.push(cte.name.name.clone());
        }
    }
    let body_qualify =
        collect_view_table_rename_spans_body(&sel.body, old_name, outer_qualify, &ctes, spans);
    if let Some(ref ob) = sel.order_by {
        for o in ob {
            collect_view_table_rename_spans_expr(&o.expr, old_name, body_qualify, &ctes, spans);
        }
    }
    if let Some(ref lim) = sel.limit {
        collect_view_table_rename_spans_expr(&lim.count, old_name, body_qualify, &ctes, spans);
        if let Some(ref off) = lim.offset {
            collect_view_table_rename_spans_expr(off, old_name, body_qualify, &ctes, spans);
        }
    }
}

fn rewrite_table_name_in_view_ddl(ddl: &str, old_name: &str, new_name: &str) -> String {
    let Ok(Statement::CreateView(cv)) = xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return ddl.to_string();
    };
    let mut spans = Vec::new();
    collect_view_table_rename_spans_select(&cv.select, old_name, false, &[], &mut spans);
    let quoted = quote_ident_if_needed(new_name);
    apply_span_replacements(ddl, spans, &quoted)
}

#[allow(clippy::too_many_arguments)]
fn collect_view_col_rename_spans_expr(
    expr: &Expr,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
    spans: &mut Vec<xdb_parser::Span>,
) {
    match expr {
        Expr::Column(c) => {
            if c.column.name.eq_ignore_ascii_case(old_col) {
                let matches = match &c.table {
                    None => allow_unqualified,
                    Some(t) => aliases.iter().any(|a| t.name.eq_ignore_ascii_case(a)),
                };
                if matches {
                    spans.push(c.column.span);
                }
            }
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            if let Expr::InSelect { operand, .. } = expr {
                collect_view_col_rename_spans_expr(
                    operand,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                    spans,
                );
            }
            collect_view_col_rename_spans_select(
                select,
                table_name,
                old_col,
                aliases,
                allow_unqualified,
                ctes,
                schema,
                spans,
            );
        }
        _ => {
            for ch in xdb_parser::expr_children(expr) {
                collect_view_col_rename_spans_expr(
                    ch,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                    spans,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_view_col_rename_spans_join(
    from: &xdb_parser::FromSource,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
    spans: &mut Vec<xdb_parser::Span>,
) {
    match from {
        xdb_parser::FromSource::Join {
            left,
            right,
            on,
            using,
            ..
        } => {
            collect_view_col_rename_spans_join(
                left,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
                spans,
            );
            collect_view_col_rename_spans_join(
                right,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
                spans,
            );
            if let Some(on_e) = on {
                collect_view_col_rename_spans_expr(
                    on_e,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                    spans,
                );
            }
            if let Some(us) = using {
                if from_contains_target_table(left, table_name, ctes)
                    || from_contains_target_table(right, table_name, ctes)
                {
                    for u in us {
                        if u.name.eq_ignore_ascii_case(old_col) {
                            spans.push(u.span);
                        }
                    }
                }
            }
        }
        xdb_parser::FromSource::Group(g) => {
            collect_view_col_rename_spans_join(
                &g.source,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
                spans,
            );
        }
        xdb_parser::FromSource::Function(f) => {
            for a in &f.args {
                collect_view_col_rename_spans_expr(
                    a,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                    spans,
                );
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_view_col_rename_spans_body(
    body: &SelectBody,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
    spans: &mut Vec<xdb_parser::Span>,
) {
    match body {
        SelectBody::Core(core) => {
            let mut local_aliases = Vec::new();
            let mut subqueries = Vec::new();
            if let Some(ref from) = core.from {
                collect_from_aliases_for_table_scoped(
                    from,
                    table_name,
                    ctes,
                    &mut local_aliases,
                    &mut subqueries,
                );
            }
            for sq in &subqueries {
                collect_view_col_rename_spans_select(
                    sq,
                    table_name,
                    old_col,
                    outer_aliases,
                    outer_unqualified,
                    ctes,
                    schema,
                    spans,
                );
            }
            let mut aliases: Vec<String> = outer_aliases
                .iter()
                .filter(|a| {
                    core.from
                        .as_ref()
                        .is_none_or(|f| !from_shadows_alias(f, a, Some(table_name), ctes))
                })
                .cloned()
                .collect();
            aliases.extend(local_aliases.iter().cloned());
            let allow_unq = if !local_aliases.is_empty() {
                true
            } else if let Some(ref from) = core.from {
                outer_unqualified && from_tables_known_without_col(from, old_col, ctes, schema)
            } else {
                outer_unqualified
            };
            if let Some(ref from) = core.from {
                collect_view_col_rename_spans_join(
                    from, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                );
            }
            for rc in &core.columns {
                if let ResultColumn::Expr(re) = rc {
                    collect_view_col_rename_spans_expr(
                        &re.expr, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                    );
                }
            }
            if let Some(ref we) = core.where_clause {
                collect_view_col_rename_spans_expr(
                    we, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                );
            }
            if let Some(ref gb) = core.group_by {
                for e in gb {
                    collect_view_col_rename_spans_expr(
                        e, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                    );
                }
            }
            if let Some(ref hv) = core.having {
                collect_view_col_rename_spans_expr(
                    hv, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                );
            }
            if let Some(ref wins) = core.windows {
                for w in wins {
                    if let Some(ref pb) = w.window.partition_by {
                        for e in pb {
                            collect_view_col_rename_spans_expr(
                                e, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                            );
                        }
                    }
                    if let Some(ref ob) = w.window.order_by {
                        for o in ob {
                            collect_view_col_rename_spans_expr(
                                &o.expr, &aliases, allow_unq, table_name, old_col, ctes, schema,
                                spans,
                            );
                        }
                    }
                }
            }
        }
        SelectBody::Values(v) => {
            for r in &v.rows {
                for e in &r.items {
                    collect_view_col_rename_spans_expr(
                        e,
                        outer_aliases,
                        outer_unqualified,
                        table_name,
                        old_col,
                        ctes,
                        schema,
                        spans,
                    );
                }
            }
        }
        SelectBody::Compound { left, right, .. } => {
            collect_view_col_rename_spans_body(
                left,
                table_name,
                old_col,
                outer_aliases,
                outer_unqualified,
                ctes,
                schema,
                spans,
            );
            collect_view_col_rename_spans_body(
                right,
                table_name,
                old_col,
                outer_aliases,
                outer_unqualified,
                ctes,
                schema,
                spans,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_view_col_rename_spans_select(
    sel: &SelectStmt,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
    outer_ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
    spans: &mut Vec<xdb_parser::Span>,
) {
    let mut ctes = outer_ctes.to_vec();
    if let Some(ref with) = sel.with {
        for cte in &with.ctes {
            let mut cte_scope = ctes.clone();
            if with.recursive {
                cte_scope.push(cte.name.name.clone());
            }
            collect_view_col_rename_spans_select(
                &cte.select,
                table_name,
                old_col,
                &[],
                false,
                &cte_scope,
                schema,
                spans,
            );
            ctes.push(cte.name.name.clone());
        }
    }
    collect_view_col_rename_spans_body(
        &sel.body,
        table_name,
        old_col,
        outer_aliases,
        outer_unqualified,
        &ctes,
        schema,
        spans,
    );
    if let Some(ref ob) = sel.order_by {
        let mut local_aliases = Vec::new();
        let mut subq = Vec::new();
        let top_from = match &sel.body {
            SelectBody::Core(core) => core.from.as_ref(),
            _ => None,
        };
        if let Some(from) = top_from {
            collect_from_aliases_for_table_scoped(
                from,
                table_name,
                &ctes,
                &mut local_aliases,
                &mut subq,
            );
        }
        let mut top_aliases: Vec<String> = outer_aliases
            .iter()
            .filter(|a| top_from.is_none_or(|f| !from_shadows_alias(f, a, Some(table_name), &ctes)))
            .cloned()
            .collect();
        top_aliases.extend(local_aliases.iter().cloned());
        let allow_unq = if !local_aliases.is_empty() {
            true
        } else if let Some(from) = top_from {
            outer_unqualified && from_tables_known_without_col(from, old_col, &ctes, schema)
        } else {
            outer_unqualified
        };
        for o in ob {
            collect_view_col_rename_spans_expr(
                &o.expr,
                &top_aliases,
                allow_unq,
                table_name,
                old_col,
                &ctes,
                schema,
                spans,
            );
        }
    }
}

fn rewrite_column_name_in_view_ddl(
    ddl: &str,
    table_name: &str,
    old_col: &str,
    new_name: &str,
    schema: &crate::schema::SchemaCatalog,
) -> String {
    let Ok(Statement::CreateView(cv)) = xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return ddl.to_string();
    };
    let mut spans = Vec::new();
    collect_view_col_rename_spans_select(
        &cv.select,
        table_name,
        old_col,
        &[],
        false,
        &[],
        schema,
        &mut spans,
    );
    let quoted = quote_ident_if_needed(new_name);
    apply_span_replacements(ddl, spans, &quoted)
}

fn check_column_unreferenced_in_ddl(
    ddl: &str,
    table_name: &str,
    cid: usize,
    old_col: &str,
) -> SqliteResult<()> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Ok(());
    };
    let mut spans = Vec::new();
    collect_create_table_col_refs(&ct, table_name, Some(cid), old_col, &mut spans);
    if !spans.is_empty() {
        return Err(SqliteError::Sql(format!(
            "error in table {table_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn check_column_unreferenced_in_index_ddl(
    ddl: &str,
    idx_name: &str,
    table_name: &str,
    old_col: &str,
) -> SqliteResult<()> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateIndex(ci) = stmt else {
        return Ok(());
    };
    let mut spans = Vec::new();
    for ot in &ci.columns {
        collect_expr_col_ref_spans(&ot.expr, table_name, old_col, &mut spans);
    }
    if let Some(ref we) = ci.where_clause {
        collect_expr_col_ref_spans(we, table_name, old_col, &mut spans);
    }
    if !spans.is_empty() {
        return Err(SqliteError::Sql(format!(
            "error in index {idx_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn collect_from_aliases_for_table_scoped(
    from: &xdb_parser::FromSource,
    table_name: &str,
    ctes: &[String],
    aliases: &mut Vec<String>,
    subqueries: &mut Vec<SelectStmt>,
) {
    match from {
        xdb_parser::FromSource::Table(t) => {
            let schema_ok = t.schema.is_none()
                || t.schema
                    .as_ref()
                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main"));
            let shadowed = t.schema.is_none()
                && ctes.iter().any(|c| c.eq_ignore_ascii_case(&t.name.name));
            if schema_ok && !shadowed && t.name.name.eq_ignore_ascii_case(table_name) {
                let alias = t.alias.as_ref().unwrap_or(&t.name).name.clone();
                aliases.push(alias);
            }
        }
        xdb_parser::FromSource::Subquery(s) => {
            subqueries.push((*s.select).clone());
        }
        xdb_parser::FromSource::Group(g) => {
            collect_from_aliases_for_table_scoped(&g.source, table_name, ctes, aliases, subqueries);
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            collect_from_aliases_for_table_scoped(left, table_name, ctes, aliases, subqueries);
            collect_from_aliases_for_table_scoped(right, table_name, ctes, aliases, subqueries);
        }
        xdb_parser::FromSource::Function(_) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn expr_refs_any_alias_col(
    expr: &Expr,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match expr {
        Expr::Column(c) => {
            if c.column.name.eq_ignore_ascii_case(old_col) {
                match &c.table {
                    None => {
                        if allow_unqualified {
                            return true;
                        }
                    }
                    Some(t) => {
                        if aliases.iter().any(|a| t.name.eq_ignore_ascii_case(a)) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            if let Expr::InSelect { operand, .. } = expr {
                if expr_refs_any_alias_col(
                    operand,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                ) {
                    return true;
                }
            }
            select_refs_table_col(
                select,
                table_name,
                old_col,
                aliases,
                allow_unqualified,
                ctes,
                schema,
            )
        }
        _ => xdb_parser::expr_children(expr).into_iter().any(|ch| {
            expr_refs_any_alias_col(
                ch,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
            )
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn from_join_clauses_ref_col(
    from: &xdb_parser::FromSource,
    aliases: &[String],
    allow_unqualified: bool,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match from {
        xdb_parser::FromSource::Join {
            left,
            right,
            on,
            using,
            ..
        } => {
            if from_join_clauses_ref_col(
                left,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
            ) || from_join_clauses_ref_col(
                right,
                aliases,
                allow_unqualified,
                table_name,
                old_col,
                ctes,
                schema,
            ) {
                return true;
            }
            if let Some(on_e) = on {
                if expr_refs_any_alias_col(
                    on_e,
                    aliases,
                    allow_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                ) {
                    return true;
                }
            }
            if let Some(us) = using {
                if (from_contains_target_table(left, table_name, ctes)
                    || from_contains_target_table(right, table_name, ctes))
                    && us.iter().any(|u| u.name.eq_ignore_ascii_case(old_col))
                {
                    return true;
                }
            }
            false
        }
        xdb_parser::FromSource::Group(g) => from_join_clauses_ref_col(
            &g.source,
            aliases,
            allow_unqualified,
            table_name,
            old_col,
            ctes,
            schema,
        ),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn select_core_refs_table_col(
    core: &SelectCore,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    let mut local_aliases = Vec::new();
    let mut subqueries = Vec::new();
    if let Some(ref from) = core.from {
        collect_from_aliases_for_table_scoped(
            from,
            table_name,
            ctes,
            &mut local_aliases,
            &mut subqueries,
        );
    }
    let mut aliases: Vec<String> = outer_aliases
        .iter()
        .filter(|a| {
            core.from
                .as_ref()
                .is_none_or(|f| !from_shadows_alias(f, a, Some(table_name), ctes))
        })
        .cloned()
        .collect();
    aliases.extend(local_aliases.iter().cloned());
    let allow_unq = if !local_aliases.is_empty() {
        true
    } else if let Some(ref from) = core.from {
        outer_unqualified && from_tables_known_without_col(from, old_col, ctes, schema)
    } else {
        outer_unqualified
    };

    for sq in &subqueries {
        if select_refs_table_col(
            sq,
            table_name,
            old_col,
            outer_aliases,
            outer_unqualified,
            ctes,
            schema,
        ) {
            return true;
        }
    }
    if let Some(ref from) = core.from {
        if from_join_clauses_ref_col(from, &aliases, allow_unq, table_name, old_col, ctes, schema) {
            return true;
        }
    }
    for rc in &core.columns {
        if let ResultColumn::Expr(re) = rc {
            if expr_refs_any_alias_col(
                &re.expr, &aliases, allow_unq, table_name, old_col, ctes, schema,
            ) {
                return true;
            }
        }
    }
    if let Some(ref we) = core.where_clause {
        if expr_refs_any_alias_col(we, &aliases, allow_unq, table_name, old_col, ctes, schema) {
            return true;
        }
    }
    if let Some(ref gb) = core.group_by {
        if gb.iter().any(|e| {
            expr_refs_any_alias_col(e, &aliases, allow_unq, table_name, old_col, ctes, schema)
        }) {
            return true;
        }
    }
    if let Some(ref hv) = core.having {
        if expr_refs_any_alias_col(hv, &aliases, allow_unq, table_name, old_col, ctes, schema) {
            return true;
        }
    }
    if let Some(ref wins) = core.windows {
        for w in wins {
            if let Some(ref pb) = w.window.partition_by {
                if pb.iter().any(|e| {
                    expr_refs_any_alias_col(
                        e, &aliases, allow_unq, table_name, old_col, ctes, schema,
                    )
                }) {
                    return true;
                }
            }
            if let Some(ref ob) = w.window.order_by {
                if ob.iter().any(|o| {
                    expr_refs_any_alias_col(
                        &o.expr, &aliases, allow_unq, table_name, old_col, ctes, schema,
                    )
                }) {
                    return true;
                }
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn select_body_refs_table_col(
    body: &SelectBody,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match body {
        SelectBody::Core(core) => select_core_refs_table_col(
            core,
            table_name,
            old_col,
            outer_aliases,
            outer_unqualified,
            ctes,
            schema,
        ),
        SelectBody::Compound { left, right, .. } => {
            select_body_refs_table_col(
                left,
                table_name,
                old_col,
                outer_aliases,
                outer_unqualified,
                ctes,
                schema,
            ) || select_body_refs_table_col(
                right,
                table_name,
                old_col,
                outer_aliases,
                outer_unqualified,
                ctes,
                schema,
            )
        }
        SelectBody::Values(v) => v.rows.iter().any(|r| {
            r.items.iter().any(|e| {
                expr_refs_any_alias_col(
                    e,
                    outer_aliases,
                    outer_unqualified,
                    table_name,
                    old_col,
                    ctes,
                    schema,
                )
            })
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn select_refs_table_col(
    stmt: &SelectStmt,
    table_name: &str,
    old_col: &str,
    outer_aliases: &[String],
    outer_unqualified: bool,
    outer_ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    let mut ctes = outer_ctes.to_vec();
    if let Some(ref with) = stmt.with {
        for cte in &with.ctes {
            let mut cte_scope = ctes.clone();
            if with.recursive {
                cte_scope.push(cte.name.name.clone());
            }
            if select_refs_table_col(
                &cte.select,
                table_name,
                old_col,
                &[],
                false,
                &cte_scope,
                schema,
            ) {
                return true;
            }
            ctes.push(cte.name.name.clone());
        }
    }
    if select_body_refs_table_col(
        &stmt.body,
        table_name,
        old_col,
        outer_aliases,
        outer_unqualified,
        &ctes,
        schema,
    ) {
        return true;
    }
    if let Some(ref ob) = stmt.order_by {
        let mut local_aliases = Vec::new();
        let mut subq = Vec::new();
        let top_from = match &stmt.body {
            SelectBody::Core(core) => core.from.as_ref(),
            _ => None,
        };
        if let Some(from) = top_from {
            collect_from_aliases_for_table_scoped(
                from,
                table_name,
                &ctes,
                &mut local_aliases,
                &mut subq,
            );
        }
        let mut top_aliases: Vec<String> = outer_aliases
            .iter()
            .filter(|a| top_from.is_none_or(|f| !from_shadows_alias(f, a, Some(table_name), &ctes)))
            .cloned()
            .collect();
        top_aliases.extend(local_aliases.iter().cloned());
        let allow_unq = if !local_aliases.is_empty() {
            true
        } else if let Some(from) = top_from {
            outer_unqualified && from_tables_known_without_col(from, old_col, &ctes, schema)
        } else {
            outer_unqualified
        };
        if ob.iter().any(|o| {
            expr_refs_any_alias_col(
                &o.expr,
                &top_aliases,
                allow_unq,
                table_name,
                old_col,
                &ctes,
                schema,
            )
        }) {
            return true;
        }
    }
    false
}

fn check_column_unreferenced_in_view_ddl(
    ddl: &str,
    view_name: &str,
    table_name: &str,
    old_col: &str,
    schema: &crate::schema::SchemaCatalog,
) -> SqliteResult<()> {
    let Ok(Statement::CreateView(cv)) = xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return Ok(());
    };
    if select_refs_table_col(&cv.select, table_name, old_col, &[], false, &[], schema) {
        return Err(SqliteError::Sql(format!(
            "error in view {view_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn check_column_unreferenced_in_trigger_ddl(
    ddl: &str,
    trigger_name: &str,
    table_name: &str,
    old_col: &str,
    schema: &crate::schema::SchemaCatalog,
) -> SqliteResult<()> {
    let Ok(Statement::CreateTrigger(tr)) =
        xdb_parser::parse_statement(ddl, &ParseOptions::default())
    else {
        return Ok(());
    };
    let on_this_table = tr.table.name.eq_ignore_ascii_case(table_name);
    let mut trigger_aliases: Vec<String> = Vec::new();
    if on_this_table {
        trigger_aliases.push("NEW".to_string());
        trigger_aliases.push("OLD".to_string());
        if let Some(ref cols) = tr.update_of {
            if cols.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)) {
                return Err(SqliteError::Sql(format!(
                    "error in trigger {trigger_name} after drop column: no such column: {old_col}"
                )));
            }
        }
        if let Some(ref when) = tr.when {
            if expr_refs_any_alias_col(
                when,
                &trigger_aliases,
                false,
                table_name,
                old_col,
                &[],
                schema,
            ) {
                return Err(SqliteError::Sql(format!(
                    "error in trigger {trigger_name} after drop column: no such column: {old_col}"
                )));
            }
        }
    }
    for step in &tr.body {
        let hit = match step {
            xdb_parser::TriggerStep::Select(s) => {
                select_refs_table_col(s, table_name, old_col, &trigger_aliases, false, &[], schema)
            }
            xdb_parser::TriggerStep::Insert(ins) => {
                let target_hit = ins.table.name.name.eq_ignore_ascii_case(table_name)
                    && ins
                        .columns
                        .as_ref()
                        .is_some_and(|cols| cols.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)));
                let sel_hit = ins.select.as_ref().is_some_and(|s| {
                    select_refs_table_col(
                        s,
                        table_name,
                        old_col,
                        &trigger_aliases,
                        false,
                        &[],
                        schema,
                    )
                });
                target_hit || sel_hit
            }
            xdb_parser::TriggerStep::Update(upd) => {
                let same_tbl = upd.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(upd.table.alias.as_ref().unwrap_or(&upd.table.name).name.clone());
                }
                let set_hit = upd.set.iter().any(|sc| {
                    (same_tbl && sc.columns.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)))
                        || expr_refs_any_alias_col(
                            &sc.expr, &aliases, same_tbl, table_name, old_col, &[], schema,
                        )
                });
                let where_hit = upd.where_clause.as_ref().is_some_and(|w| {
                    expr_refs_any_alias_col(w, &aliases, same_tbl, table_name, old_col, &[], schema)
                });
                set_hit || where_hit
            }
            xdb_parser::TriggerStep::Delete(del) => {
                let same_tbl = del.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(del.table.alias.as_ref().unwrap_or(&del.table.name).name.clone());
                }
                del.where_clause.as_ref().is_some_and(|w| {
                    expr_refs_any_alias_col(w, &aliases, same_tbl, table_name, old_col, &[], schema)
                })
            }
        };
        if hit {
            return Err(SqliteError::Sql(format!(
                "error in trigger {trigger_name} after drop column: no such column: {old_col}"
            )));
        }
    }
    Ok(())
}

fn remove_column_from_ddl(ddl: &str, cid: usize) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let sp = ct.columns[cid].span;
    // Use SQL lexer tokens so SQL comments between column definition and `,` are skipped cleanly.
    let tokens = xdb_parser::raw_tokens(ddl, Dialect::Sqlite);
    if let Some(next_tok) = tokens
        .iter()
        .find(|t| t.start >= sp.end && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT))
    {
        if next_tok.kind == xdb_parser::tk::COMMA {
            let mut end = next_tok.end;
            let bytes = ddl.as_bytes();
            while end < bytes.len() && matches!(bytes[end], b' ' | b'\t') {
                end += 1;
            }
            let mut out = String::with_capacity(ddl.len());
            out.push_str(&ddl[..sp.start]);
            out.push_str(&ddl[end..]);
            return Ok(out);
        }
    }
    if let Some(prev_tok) = tokens
        .iter()
        .rev()
        .find(|t| t.end <= sp.start && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT))
    {
        if prev_tok.kind == xdb_parser::tk::COMMA {
            let mut out = String::with_capacity(ddl.len());
            out.push_str(&ddl[..prev_tok.start]);
            out.push_str(&ddl[sp.end..]);
            return Ok(out);
        }
    }
    let mut out = String::with_capacity(ddl.len());
    out.push_str(&ddl[..sp.start]);
    out.push_str(&ddl[sp.end..]);
    Ok(out)
}

/// Build a [`KeyInfo`] covering ONLY the primary-key columns of a `WITHOUT ROWID` table's
/// primary-key B-tree (non-PK columns are payload, never B-tree sort/equality keys).
fn key_info_for_without_rowid_pk(
    pk_idx: &IndexInfo,
    encoding: TextEncoding,
    colls: &CollationRegistry,
) -> SqliteResult<KeyInfo> {
    let specs: Vec<KeyFieldSpec<'_>> = pk_idx
        .columns
        .iter()
        .take(pk_idx.key_count)
        .map(|c| KeyFieldSpec {
            collation: &c.collation,
            desc: c.desc,
        })
        .collect();
    make_key_info(encoding, &specs, pk_idx.key_count, Some(colls))
}
