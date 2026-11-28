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
use xdb_core::vfs::{FileKind, OpenFlags, OpenMode};
use xdb_core::{Affinity, Meter, SqlValue, StorageClass, TextEncoding, Vfs};
use xdb_parser::{
    build_index, build_table, parse_expression, parse_statement, parse_statement_metered,
    AlterTableAction, AlterTableStmt, BinaryOp, BuildOptions, ColumnConstraint, ConflictAction,
    CreateIndexStmt, CreateTableStmt, CreateTriggerStmt, CreateViewStmt, DefaultForm, DeleteStmt,
    Dialect, DropObjectType, DropStmt, Expr, ForeignKeyAction, GeneratedStorage, InsertStmt,
    OnError, ParseOptions, Parser, PragmaStmt, RaiseAction, ResultColumn, SelectBody, SelectCore,
    SelectStmt, SetClause, Statement, TableConstraint, TriggerEventKind, TriggerStep, TriggerTime,
    UpdateStmt, UpsertAction,
};
use xdb_sql::eval::{expr_affinity, expr_collation};
use xdb_sql::{
    eval_expr, execute_select_stmt_detailed, execute_select_stmt_with_scopes,
    CatalogSubqueryRunner, EvalEnv, EvalScope, FunctionLimits, FunctionRegistry, NoSubqueries,
    QueryOptions, QueryParams, QueryResult, ScopeColumn, SqlError,
};

use crate::access::{key_info_for_index, make_key_info, KeyFieldSpec, KeyInfo, SearchKey};
use crate::btree::{SeekOp, SeekResult};
use crate::database::{open_from_page_source_cached, ColumnSpec, ScanOptions, SqliteDatabase};
use crate::errors::{SqliteError, SqliteResult};
use crate::format::BtreePageType;
use crate::schema::catalog::{GeneratedKind, SchemaCatalog};
use crate::schema::{ColumnHiddenKind, IndexInfo, SchemaObject, TableInfo, ViewInfo};
use crate::write::btree::{
    append_index_cell_body, check_integrity_metered_scoped, compare_index_values, create_btree,
    drop_btree,
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
use crate::write::fast_scans::{
    table_count_in_subquery_direct, table_cumul_sum_window_ipk_direct,
    table_group_by_single_col_direct, table_hash_equijoin_direct, table_rowid_range_scan_direct,
    DirectAggInput, DirectCumulOut, DirectGroupOut, DirectJoinProj,
};
use crate::write::fk::{
    count_child_refs_matching_parent_key, fk_keys_match, incoming_fks_for, parent_has_key,
    parent_key_unchanged, resolve_parent_key_cids, rewrite_fk_parent_column_in_ddl,
    rewrite_fk_table_name_in_ddl, MAX_FK_CASCADE_DEPTH,
};
use crate::write::pager::WritePager;
use crate::write::record::{
    encode_record_refs_into, encode_record_with_format, encode_record_with_format_into,
    encode_two_int_record_stack,
};
use crate::write::trigger::{
    compile_triggers_for_target, rewrite_trigger_column_name, rewrite_trigger_table_name,
    CompiledTrigger, TriggerOutcome, TriggerRowCtx, MAX_TRIGGER_DEPTH,
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
    RowidRange {
        schema_gen: u64,
        root_page: u32,
        lo: Option<i64>,
        hi: Option<i64>,
        empty: bool,
        limit: Option<usize>,
        offset: usize,
        out_cols: Vec<String>,
        slots: Vec<DirectTableSlot>,
    },
    GroupBySingleCol {
        schema_gen: u64,
        root_page: u32,
        grp_storage_idx: usize,
        grp_default: SqlValue,
        grp_real_back: bool,
        agg_inputs: Vec<DirectAggInput>,
        outputs: Vec<DirectGroupOut>,
        out_cols: Vec<String>,
    },
    CumulativeWindowIpk {
        schema_gen: u64,
        root_page: u32,
        hi_rowid: Option<i64>,
        empty: bool,
        val_storage_idx: usize,
        val_default: SqlValue,
        val_real_back: bool,
        outputs: Vec<DirectCumulOut>,
        out_cols: Vec<String>,
    },
    HashEquiJoin2 {
        schema_gen: u64,
        left_root: u32,
        left_key: DirectTableSlot,
        right_root: u32,
        right_key: DirectTableSlot,
        right_proj: Vec<DirectTableSlot>,
        out_proj: Vec<DirectJoinProj>,
        cmp_aff: Affinity,
        is_left: bool,
        out_cols: Vec<String>,
    },
    CountInSubquery {
        schema_gen: u64,
        outer_root: u32,
        outer_lo: Option<i64>,
        outer_hi: Option<i64>,
        outer_empty: bool,
        outer_col: DirectTableSlot,
        sub_root: u32,
        sub_col: DirectTableSlot,
        cmp_aff: Affinity,
        out_col: String,
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
            | Self::TopK { schema_gen, .. }
            | Self::RowidRange { schema_gen, .. }
            | Self::GroupBySingleCol { schema_gen, .. }
            | Self::CumulativeWindowIpk { schema_gen, .. }
            | Self::HashEquiJoin2 { schema_gen, .. }
            | Self::CountInSubquery { schema_gen, .. } => *schema_gen,
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
    defer_foreign_keys: bool,
    /// Immediate FK violation counter for the current outermost statement (SQLite `nFkConstraint`).
    stmt_imm_fk: i64,
    /// Deferred FK violation counter for `DEFERRABLE INITIALLY DEFERRED` FKs (SQLite `nDeferredCons`).
    txn_def_fk: i64,
    /// Deferred immediate-FK violation counter for `PRAGMA defer_foreign_keys = ON` (SQLite `nDeferredImmCons`).
    txn_def_imm_fk: i64,
    /// Savepoint checkpoint stack for `(txn_def_fk, txn_def_imm_fk)`.
    txn_def_fk_sp_stack: Vec<(Option<String>, i64, i64)>,
    fk_cascade_depth: usize,
    recursive_triggers: bool,
    case_sensitive_like: bool,
    ignore_check_constraints: bool,
    query_only: bool,
    busy_timeout: i64,
    synchronous: i64,
    closed: bool,
    pending_error_conflict: Option<ConflictAction>,
    /// Active outer statement `ON CONFLICT` policy propagated into triggered statements.
    active_conflict_override: Option<ConflictAction>,
    /// Active trigger call stack for recursion prevention (`PRAGMA recursive_triggers`) and depth cap.
    active_triggers: Vec<String>,
    /// Current trigger `OLD.*` / `NEW.*` outer row scope (when executing inside a trigger body).
    trigger_ctx: Option<Arc<TriggerRowCtx>>,
    /// Set when a trigger body step directly evaluates `RAISE(IGNORE)`.
    trigger_ignore_signaled: bool,
    /// True while `VACUUM` replays existing `sqlite_schema` table DDL into a fresh database.
    in_vacuum_replay: bool,
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
            defer_foreign_keys: false,
            stmt_imm_fk: 0,
            txn_def_fk: 0,
            txn_def_imm_fk: 0,
            txn_def_fk_sp_stack: Vec::new(),
            fk_cascade_depth: 0,
            recursive_triggers: false,
            case_sensitive_like: false,
            ignore_check_constraints: false,
            query_only: false,
            busy_timeout: 0,
            synchronous: 2,
            closed: false,
            pending_error_conflict: None,
            active_conflict_override: None,
            active_triggers: Vec::new(),
            trigger_ctx: None,
            trigger_ignore_signaled: false,
            in_vacuum_replay: false,
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
        let enc = self.pager.encoding();
        let st = Arc::new(DmlTableState::build(
            table,
            &schema,
            enc,
            &self.cached_collations.1,
        )?);
        drop(schema);
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
            case_sensitive_like: self.case_sensitive_like,
            max_cte_iterations: 100_000,
            changes: self.changes,
            total_changes: self.total_changes,
            last_insert_rowid: self.last_insert_rowid,
            in_trigger: self.trigger_ctx.is_some(),
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
            CachedFastSelect::RowidRange {
                root_page,
                lo,
                hi,
                empty,
                limit,
                offset,
                out_cols,
                slots,
                ..
            } => {
                if *empty {
                    return Ok(QueryResult {
                        columns: out_cols.clone(),
                        rows: Vec::new(),
                    });
                }
                let enc = self.pager.encoding();
                let rows = table_rowid_range_scan_direct(
                    &self.pager,
                    *root_page,
                    *lo,
                    *hi,
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
            CachedFastSelect::GroupBySingleCol {
                root_page,
                grp_storage_idx,
                grp_default,
                grp_real_back,
                agg_inputs,
                outputs,
                out_cols,
                ..
            } => {
                let enc = self.pager.encoding();
                let rows = table_group_by_single_col_direct(
                    &self.pager,
                    *root_page,
                    *grp_storage_idx,
                    grp_default,
                    *grp_real_back,
                    agg_inputs,
                    outputs,
                    enc,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
            CachedFastSelect::CumulativeWindowIpk {
                root_page,
                hi_rowid,
                empty,
                val_storage_idx,
                val_default,
                val_real_back,
                outputs,
                out_cols,
                ..
            } => {
                if *empty {
                    return Ok(QueryResult {
                        columns: out_cols.clone(),
                        rows: Vec::new(),
                    });
                }
                let enc = self.pager.encoding();
                let rows = table_cumul_sum_window_ipk_direct(
                    &self.pager,
                    *root_page,
                    *hi_rowid,
                    *val_storage_idx,
                    val_default,
                    *val_real_back,
                    outputs,
                    enc,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
            CachedFastSelect::HashEquiJoin2 {
                left_root,
                left_key,
                right_root,
                right_key,
                right_proj,
                out_proj,
                cmp_aff,
                is_left,
                out_cols,
                ..
            } => {
                let enc = self.pager.encoding();
                let rows = table_hash_equijoin_direct(
                    &self.pager,
                    *left_root,
                    left_key,
                    *right_root,
                    right_key,
                    right_proj,
                    out_proj,
                    *cmp_aff,
                    *is_left,
                    enc,
                )?;
                Ok(QueryResult {
                    columns: out_cols.clone(),
                    rows,
                })
            }
            CachedFastSelect::CountInSubquery {
                outer_root,
                outer_lo,
                outer_hi,
                outer_empty,
                outer_col,
                sub_root,
                sub_col,
                cmp_aff,
                out_col,
                ..
            } => {
                let cnt = if *outer_empty {
                    0
                } else {
                    table_count_in_subquery_direct(
                        &self.pager,
                        *outer_root,
                        *outer_lo,
                        *outer_hi,
                        outer_col,
                        *sub_root,
                        sub_col,
                        *cmp_aff,
                        self.pager.encoding(),
                    )?
                };
                Ok(QueryResult {
                    columns: vec![out_col.clone()],
                    rows: vec![vec![SqlValue::Int(cnt)]],
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
            || core.group_by_all
            || core.having.is_some()
            || core.windows.is_some()
            || core.from_first
        {
            return None;
        }
        let sgen = self.pager.schema_gen();
        let schema = self.schema_arc().ok()?;

        // A. Direct 2-table hash equi-join (`FROM a [INNER|LEFT] JOIN b ON a.k = b.k`)
        if let Some(xdb_parser::FromSource::Join {
            left,
            operator: op,
            right,
            on: Some(on_expr),
            using: None,
            ..
        }) = &core.from
        {
            if core.where_clause.is_none()
                && core.group_by.is_none()
                && sel.order_by.is_none()
                && sel.limit.is_none()
            {
                if let Some(plan) =
                    self.try_build_fast_hash_join(sgen, &schema, core, left, *op, right, on_expr)
                {
                    return Some(plan);
                }
            }
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
        let table = schema.table(&ft.name.name)?;
        if table.without_rowid || table.has_virtual_columns || table.system {
            return None;
        }
        let alias = ft.alias.as_ref().unwrap_or(&ft.name).name.as_str();

        // B. Direct single-column GROUP BY (`SELECT grp, count(*), sum(val), avg(val) FROM tbl GROUP BY grp`)
        if let Some(ref gb) = core.group_by {
            if core.where_clause.is_none()
                && sel.order_by.is_none()
                && sel.limit.is_none()
                && gb.len() == 1
            {
                return self.try_build_fast_group_by(sgen, table, alias, core, &gb[0]);
            }
            return None;
        }

        // C. Direct cumulative IPK window (`SELECT id, sum(val) OVER (ORDER BY id) FROM tbl [WHERE id <= N]`)
        if core.columns.iter().any(|rc| {
            matches!(rc, ResultColumn::Expr(re) if matches!(&re.expr, Expr::Function(f) if f.over.is_some()))
        }) {
            return self.try_build_fast_cumul_window(sgen, sel, table, alias, core);
        }

        // 0. `SELECT <cols> FROM tbl ORDER BY <col> [ASC|DESC] [NULLS FIRST|LAST] LIMIT <k> [OFFSET <o>]`
        //    (no WHERE): covering-index fast path OR direct-on-WritePager Top-K scan!
        if core.where_clause.is_none() && (sel.order_by.is_some() || sel.limit.is_some()) {
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
            if sel.order_by.is_some() || sel.limit.is_some() {
                return None;
            }
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

        // 2. `SELECT <cols> FROM tbl WHERE ...`:
        //    - `SELECT count(*) FROM tbl WHERE [rowid range AND] col IN (SELECT c FROM sub)`
        //    - rowid/IPK range (`BETWEEN` / `>=` / `>` / `<=` / `<`)
        //    - rowid/IPK equality (`=`)
        //    - secondary-index equality (`=`)
        let we = core.where_clause.as_ref()?;
        if core.columns.len() == 1 && sel.order_by.is_none() && sel.limit.is_none() {
            if let Some(plan) =
                self.try_build_fast_count_in_subquery(sgen, &schema, table, alias, core, we)
            {
                return Some(plan);
            }
        }
        if !matches!(we, Expr::Binary { op: BinaryOp::Eq, .. }) {
            return self.try_build_fast_rowid_range(sgen, sel, table, alias, core, we);
        }
        if sel.order_by.is_some() || sel.limit.is_some() {
            return None;
        }
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

    fn resolve_direct_table_proj(
        table: &TableInfo,
        alias: &str,
        columns: &[ResultColumn],
    ) -> Option<(Vec<String>, Vec<DirectTableSlot>)> {
        let mut out_cols = Vec::with_capacity(columns.len());
        let mut slots = Vec::with_capacity(columns.len());
        for rc in columns {
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
                        if Some(cid) == table.ipk_column {
                            slots.push(DirectTableSlot::Rowid);
                        } else {
                            slots.push(DirectTableSlot::Field {
                                storage_idx: col.storage_index?,
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
                        if Some(cid) == table.ipk_column {
                            slots.push(DirectTableSlot::Rowid);
                        } else {
                            slots.push(DirectTableSlot::Field {
                                storage_idx: col.storage_index?,
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
                        slots.push(DirectTableSlot::Rowid);
                    } else {
                        return None;
                    }
                }
            }
        }
        Some((out_cols, slots))
    }

    fn try_build_fast_rowid_range(
        &self,
        sgen: u64,
        sel: &SelectStmt,
        table: &TableInfo,
        alias: &str,
        core: &SelectCore,
        we: &Expr,
    ) -> Option<CachedFastSelect> {
        if let Some(ref ob) = sel.order_by {
            if ob.len() != 1
                || ob[0].order == Some(xdb_parser::SortOrder::Desc)
                || !is_dml_rowid_ref(&ob[0].expr, table, alias)
            {
                return None;
            }
            if let Expr::Column(oc) = &ob[0].expr {
                if oc.table.is_none()
                    && core.columns.iter().any(|rc| {
                        matches!(
                            rc,
                            ResultColumn::Expr(re)
                                if re.alias.as_ref().is_some_and(|a| {
                                    a.name.eq_ignore_ascii_case(&oc.column.name)
                                })
                        )
                    })
                {
                    return None;
                }
            }
        }
        let (limit, offset) = if let Some(ref lim) = sel.limit {
            let lv = try_eval_literal_const_expr(&lim.count).ok()??;
            let li = lv.as_i64()?;
            if li < 0 {
                return None;
            }
            let off = if let Some(ref oe) = lim.offset {
                let ov = try_eval_literal_const_expr(oe).ok()??;
                let oi = ov.as_i64()?;
                if oi < 0 {
                    return None;
                }
                usize::try_from(oi).ok()?
            } else {
                0
            };
            (Some(usize::try_from(li).ok()?), off)
        } else {
            (None, 0)
        };

        let mut conjs = Vec::new();
        collect_dml_and_conjuncts(we, &mut conjs);
        if conjs.is_empty() {
            return None;
        }
        let mut rid_lo: Option<i64> = None;
        let mut rid_hi: Option<i64> = None;
        let mut rid_empty = false;

        for conj in conjs {
            match conj {
                Expr::Between {
                    not: false,
                    operand,
                    low,
                    high,
                    ..
                } if is_dml_rowid_ref(operand, table, alias) => {
                    let lv = try_eval_literal_const_expr(low).ok()??;
                    let hv = try_eval_literal_const_expr(high).ok()??;
                    match coerce_rowid_lower_bound(&lv, true)? {
                        Some(l) => rid_lo = Some(rid_lo.map_or(l, |p| p.max(l))),
                        None => rid_empty = true,
                    }
                    match coerce_rowid_upper_bound(&hv, true)? {
                        Some(h) => rid_hi = Some(rid_hi.map_or(h, |p| p.min(h))),
                        None => rid_empty = true,
                    }
                }
                Expr::Binary {
                    op, left, right, ..
                } if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                ) =>
                {
                    let (val_expr, eff_op) = if is_dml_rowid_ref(left, table, alias) {
                        (right.as_ref(), *op)
                    } else if is_dml_rowid_ref(right, table, alias) {
                        (left.as_ref(), flip_dml_cmp(*op))
                    } else {
                        return None;
                    };
                    let val = try_eval_literal_const_expr(val_expr).ok()??;
                    match eff_op {
                        BinaryOp::Ge | BinaryOp::Gt => {
                            match coerce_rowid_lower_bound(&val, eff_op == BinaryOp::Ge)? {
                                Some(l) => rid_lo = Some(rid_lo.map_or(l, |p| p.max(l))),
                                None => rid_empty = true,
                            }
                        }
                        BinaryOp::Le | BinaryOp::Lt => {
                            match coerce_rowid_upper_bound(&val, eff_op == BinaryOp::Le)? {
                                Some(h) => rid_hi = Some(rid_hi.map_or(h, |p| p.min(h))),
                                None => rid_empty = true,
                            }
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        if !rid_empty && rid_lo.is_none() && rid_hi.is_none() {
            return None;
        }
        let (out_cols, slots) = Self::resolve_direct_table_proj(table, alias, &core.columns)?;
        Some(CachedFastSelect::RowidRange {
            schema_gen: sgen,
            root_page: table.root_page,
            lo: rid_lo,
            hi: rid_hi,
            empty: rid_empty,
            limit,
            offset,
            out_cols,
            slots,
        })
    }

    fn try_build_fast_group_by(
        &self,
        sgen: u64,
        table: &TableInfo,
        alias: &str,
        core: &SelectCore,
        gb_expr: &Expr,
    ) -> Option<CachedFastSelect> {
        let Expr::Column(gc) = gb_expr else {
            return None;
        };
        if let Some(ref sc) = gc.schema {
            if !sc.name.eq_ignore_ascii_case("main") {
                return None;
            }
        }
        if let Some(ref tq) = gc.table {
            if !tq.name.eq_ignore_ascii_case(alias) {
                return None;
            }
        }
        let grp_cid = table
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&gc.column.name))?;
        if Some(grp_cid) == table.ipk_column {
            return None;
        }
        let grp_col = &table.columns[grp_cid];
        if grp_col.hidden == ColumnHiddenKind::Virtual
            || !grp_col.collation.eq_ignore_ascii_case("BINARY")
        {
            return None;
        }
        let grp_storage_idx = grp_col.storage_index?;

        let mut agg_inputs: Vec<DirectAggInput> = Vec::new();
        let mut outputs: Vec<DirectGroupOut> = Vec::with_capacity(core.columns.len());
        let mut out_cols: Vec<String> = Vec::with_capacity(core.columns.len());

        for rc in &core.columns {
            let ResultColumn::Expr(re) = rc else {
                return None;
            };
            let out_name = match &re.alias {
                Some(a) => a.name.clone(),
                None => re.text.trim().to_string(),
            };
            match &re.expr {
                Expr::Column(c) => {
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
                    if cid != grp_cid {
                        return None;
                    }
                    out_cols.push(
                        re.alias
                            .as_ref()
                            .map(|a| a.name.clone())
                            .unwrap_or_else(|| grp_col.name.clone()),
                    );
                    outputs.push(DirectGroupOut::GroupKey);
                }
                Expr::Function(f) => {
                    if f.filter.is_some()
                        || f.over.is_some()
                        || f.order_by.is_some()
                        || f.quantifier.is_some()
                    {
                        return None;
                    }
                    let fname = f.name.name.to_ascii_lowercase();
                    if fname == "count" && (f.star || f.args.is_empty()) {
                        out_cols.push(out_name);
                        outputs.push(DirectGroupOut::CountStar);
                        continue;
                    }
                    if f.star || f.args.len() != 1 {
                        return None;
                    }
                    let Expr::Column(ac) = &f.args[0] else {
                        return None;
                    };
                    if let Some(ref sc) = ac.schema {
                        if !sc.name.eq_ignore_ascii_case("main") {
                            return None;
                        }
                    }
                    if let Some(ref tq) = ac.table {
                        if !tq.name.eq_ignore_ascii_case(alias) {
                            return None;
                        }
                    }
                    let acid = table
                        .columns
                        .iter()
                        .position(|col| col.name.eq_ignore_ascii_case(&ac.column.name))?;
                    if Some(acid) == table.ipk_column {
                        return None;
                    }
                    let acol = &table.columns[acid];
                    if acol.hidden == ColumnHiddenKind::Virtual {
                        return None;
                    }
                    let asidx = acol.storage_index?;
                    let real_back = acol.affinity == Affinity::Real;
                    let sum_slot = match agg_inputs.iter().position(|inp| inp.storage_idx == asidx)
                    {
                        Some(pos) => pos,
                        None => {
                            let pos = agg_inputs.len();
                            agg_inputs.push(DirectAggInput {
                                storage_idx: asidx,
                                default_val: acol.default_value.clone(),
                                real_back,
                            });
                            pos
                        }
                    };
                    let out_kind = match fname.as_str() {
                        "count" => DirectGroupOut::CountCol { sum_slot },
                        "sum" => DirectGroupOut::Sum { sum_slot },
                        "total" => DirectGroupOut::Total { sum_slot },
                        "avg" => DirectGroupOut::Avg { sum_slot },
                        _ => return None,
                    };
                    out_cols.push(out_name);
                    outputs.push(out_kind);
                }
                _ => return None,
            }
        }

        Some(CachedFastSelect::GroupBySingleCol {
            schema_gen: sgen,
            root_page: table.root_page,
            grp_storage_idx,
            grp_default: grp_col.default_value.clone(),
            grp_real_back: grp_col.affinity == Affinity::Real,
            agg_inputs,
            outputs,
            out_cols,
        })
    }

    fn try_build_fast_cumul_window(
        &self,
        sgen: u64,
        sel: &SelectStmt,
        table: &TableInfo,
        alias: &str,
        core: &SelectCore,
    ) -> Option<CachedFastSelect> {
        if sel.limit.is_some() {
            return None;
        }
        if let Some(ref ob) = sel.order_by {
            if ob.len() != 1
                || ob[0].order == Some(xdb_parser::SortOrder::Desc)
                || !is_dml_rowid_ref(&ob[0].expr, table, alias)
            {
                return None;
            }
            if let Expr::Column(oc) = &ob[0].expr {
                if oc.table.is_none()
                    && core.columns.iter().any(|rc| {
                        matches!(
                            rc,
                            ResultColumn::Expr(re)
                                if re.alias.as_ref().is_some_and(|a| {
                                    a.name.eq_ignore_ascii_case(&oc.column.name)
                                })
                        )
                    })
                {
                    return None;
                }
            }
        }
        // Optional `WHERE id <= <const>` (or `< <const>`).
        let mut hi_rowid: Option<i64> = None;
        let mut empty = false;
        if let Some(ref we) = core.where_clause {
            let Expr::Binary {
                op, left, right, ..
            } = we
            else {
                return None;
            };
            let (val_expr, eff_op) = if is_dml_rowid_ref(left, table, alias) {
                (right.as_ref(), *op)
            } else if is_dml_rowid_ref(right, table, alias) {
                (left.as_ref(), flip_dml_cmp(*op))
            } else {
                return None;
            };
            if !matches!(eff_op, BinaryOp::Le | BinaryOp::Lt) {
                return None;
            }
            let val = try_eval_literal_const_expr(val_expr).ok()??;
            match coerce_rowid_upper_bound(&val, eff_op == BinaryOp::Le)? {
                Some(h) => hi_rowid = Some(h),
                None => empty = true,
            }
        }

        let mut val_cid_opt: Option<usize> = None;
        let mut outputs: Vec<DirectCumulOut> = Vec::with_capacity(core.columns.len());
        let mut out_cols: Vec<String> = Vec::with_capacity(core.columns.len());

        for rc in &core.columns {
            let ResultColumn::Expr(re) = rc else {
                return None;
            };
            match &re.expr {
                Expr::Column(c) => {
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
                        if Some(cid) == table.ipk_column {
                            outputs.push(DirectCumulOut::TableSlot(DirectTableSlot::Rowid));
                        } else {
                            outputs.push(DirectCumulOut::TableSlot(DirectTableSlot::Field {
                                storage_idx: col.storage_index?,
                                default_val: col.default_value.clone(),
                                real_back: col.affinity == Affinity::Real,
                            }));
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
                        outputs.push(DirectCumulOut::TableSlot(DirectTableSlot::Rowid));
                    } else {
                        return None;
                    }
                }
                Expr::Function(f) => {
                    if f.filter.is_some()
                        || f.order_by.is_some()
                        || f.quantifier.is_some()
                        || f.star
                        || f.args.len() != 1
                    {
                        return None;
                    }
                    let fname = f.name.name.to_ascii_lowercase();
                    let out_kind = match fname.as_str() {
                        "sum" => DirectCumulOut::RunningSum,
                        "total" => DirectCumulOut::RunningTotal,
                        _ => return None,
                    };
                    let Some(xdb_parser::WindowRef::Spec(ws)) = &f.over else {
                        return None;
                    };
                    if ws.base.is_some() || ws.partition_by.is_some() {
                        return None;
                    }
                    let ob = ws.order_by.as_ref()?;
                    if ob.len() != 1
                        || ob[0].order == Some(xdb_parser::SortOrder::Desc)
                        || !is_dml_rowid_ref(&ob[0].expr, table, alias)
                    {
                        return None;
                    }
                    if let Some(ref fr) = ws.frame {
                        if !matches!(
                            fr.units,
                            xdb_parser::FrameUnits::Range | xdb_parser::FrameUnits::Rows
                        ) || fr.from.kind != xdb_parser::FrameBoundType::UnboundedPreceding
                            || fr.to.as_ref().is_some_and(|e| {
                                e.kind != xdb_parser::FrameBoundType::CurrentRow
                            })
                            || fr.exclude.is_some_and(|ex| {
                                ex != xdb_parser::FrameExclude::NoOthers
                            })
                        {
                            return None;
                        }
                    }
                    let Expr::Column(vc) = &f.args[0] else {
                        return None;
                    };
                    if let Some(ref sc) = vc.schema {
                        if !sc.name.eq_ignore_ascii_case("main") {
                            return None;
                        }
                    }
                    if let Some(ref tq) = vc.table {
                        if !tq.name.eq_ignore_ascii_case(alias) {
                            return None;
                        }
                    }
                    let vcid = table
                        .columns
                        .iter()
                        .position(|col| col.name.eq_ignore_ascii_case(&vc.column.name))?;
                    if Some(vcid) == table.ipk_column
                        || table.columns[vcid].hidden == ColumnHiddenKind::Virtual
                    {
                        return None;
                    }
                    if let Some(prev) = val_cid_opt {
                        if prev != vcid {
                            return None;
                        }
                    } else {
                        val_cid_opt = Some(vcid);
                    }
                    out_cols.push(match &re.alias {
                        Some(a) => a.name.clone(),
                        None => re.text.trim().to_string(),
                    });
                    outputs.push(out_kind);
                }
                _ => return None,
            }
        }

        let vcid = val_cid_opt?;
        let vcol = &table.columns[vcid];
        Some(CachedFastSelect::CumulativeWindowIpk {
            schema_gen: sgen,
            root_page: table.root_page,
            hi_rowid,
            empty,
            val_storage_idx: vcol.storage_index?,
            val_default: vcol.default_value.clone(),
            val_real_back: vcol.affinity == Affinity::Real,
            outputs,
            out_cols,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn try_build_fast_hash_join(
        &self,
        sgen: u64,
        schema: &SchemaCatalog,
        core: &SelectCore,
        left_src: &xdb_parser::FromSource,
        op: xdb_parser::JoinOperator,
        right_src: &xdb_parser::FromSource,
        on_expr: &Expr,
    ) -> Option<CachedFastSelect> {
        use xdb_parser::join_flag;
        if (op.flags & (join_flag::RIGHT | join_flag::NATURAL)) != 0 {
            return None;
        }
        let is_left = (op.flags & join_flag::LEFT) != 0;
        let (xdb_parser::FromSource::Table(lt), xdb_parser::FromSource::Table(rt)) =
            (left_src, right_src)
        else {
            return None;
        };
        if lt.indexed_by.is_some() || rt.indexed_by.is_some() {
            return None;
        }
        if lt
            .schema
            .as_ref()
            .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
            || rt
                .schema
                .as_ref()
                .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
        {
            return None;
        }
        let ltbl = schema.table(&lt.name.name)?;
        let rtbl = schema.table(&rt.name.name)?;
        if ltbl.without_rowid
            || ltbl.has_virtual_columns
            || ltbl.system
            || rtbl.without_rowid
            || rtbl.has_virtual_columns
            || rtbl.system
        {
            return None;
        }
        let lalias = lt.alias.as_ref().unwrap_or(&lt.name).name.as_str();
        let ralias = rt.alias.as_ref().unwrap_or(&rt.name).name.as_str();
        if lalias.eq_ignore_ascii_case(ralias) {
            return None;
        }

        let Expr::Binary {
            op: BinaryOp::Eq,
            left: e1,
            right: e2,
            ..
        } = on_expr
        else {
            return None;
        };

        let resolve_side = |e: &Expr| -> Option<(bool /* is_right */, DirectTableSlot, Affinity, &str)> {
            let Expr::Column(c) = e else {
                return None;
            };
            if c.schema.is_some() {
                return None;
            }
            let (is_right, tbl) = if let Some(ref tq) = c.table {
                if tq.name.eq_ignore_ascii_case(lalias) {
                    (false, ltbl)
                } else if tq.name.eq_ignore_ascii_case(ralias) {
                    (true, rtbl)
                } else {
                    return None;
                }
            } else {
                let in_l = ltbl
                    .columns
                    .iter()
                    .any(|col| col.name.eq_ignore_ascii_case(&c.column.name));
                let in_r = rtbl
                    .columns
                    .iter()
                    .any(|col| col.name.eq_ignore_ascii_case(&c.column.name));
                match (in_l, in_r) {
                    (true, false) => (false, ltbl),
                    (false, true) => (true, rtbl),
                    _ => return None,
                }
            };
            let cid = tbl
                .columns
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(&c.column.name))?;
            let col = &tbl.columns[cid];
            if col.hidden == ColumnHiddenKind::Virtual {
                return None;
            }
            let slot = if Some(cid) == tbl.ipk_column {
                DirectTableSlot::Rowid
            } else {
                DirectTableSlot::Field {
                    storage_idx: col.storage_index?,
                    default_val: col.default_value.clone(),
                    real_back: col.affinity == Affinity::Real,
                }
            };
            Some((is_right, slot, col.affinity, col.collation.as_str()))
        };

        let (r1, slot1, aff1, coll1) = resolve_side(e1)?;
        let (r2, slot2, aff2, coll2) = resolve_side(e2)?;
        if r1 == r2
            || !coll1.eq_ignore_ascii_case("BINARY")
            || !coll2.eq_ignore_ascii_case("BINARY")
        {
            return None;
        }
        let (left_key, right_key) = if !r1 { (slot1, slot2) } else { (slot2, slot1) };
        let cmp_aff = comparison_affinity(Some(aff1), Some(aff2));
        if cmp_aff < Affinity::Numeric {
            return None;
        }

        let mut out_cols = Vec::with_capacity(core.columns.len());
        let mut right_proj: Vec<DirectTableSlot> = Vec::new();
        let mut out_proj: Vec<DirectJoinProj> = Vec::with_capacity(core.columns.len());
        for rc in &core.columns {
            let ResultColumn::Expr(re) = rc else {
                return None;
            };
            let (is_right, slot, _, _) = resolve_side(&re.expr)?;
            let col_label = match &re.alias {
                Some(a) => a.name.clone(),
                None => match &re.expr {
                    Expr::Column(c) => {
                        let tbl = if is_right { rtbl } else { ltbl };
                        tbl.columns
                            .iter()
                            .find(|col| col.name.eq_ignore_ascii_case(&c.column.name))
                            .map(|col| col.name.clone())
                            .unwrap_or_else(|| c.column.name.clone())
                    }
                    _ => re.text.trim().to_string(),
                },
            };
            out_cols.push(col_label);
            if is_right {
                let ridx = right_proj.len();
                right_proj.push(slot);
                out_proj.push(DirectJoinProj::Right(ridx));
            } else {
                out_proj.push(DirectJoinProj::Left(slot));
            }
        }

        Some(CachedFastSelect::HashEquiJoin2 {
            schema_gen: sgen,
            left_root: ltbl.root_page,
            left_key,
            right_root: rtbl.root_page,
            right_key,
            right_proj,
            out_proj,
            cmp_aff,
            is_left,
            out_cols,
        })
    }

    fn try_build_fast_count_in_subquery(
        &self,
        sgen: u64,
        schema: &SchemaCatalog,
        table: &TableInfo,
        alias: &str,
        core: &SelectCore,
        we: &Expr,
    ) -> Option<CachedFastSelect> {
        let ResultColumn::Expr(re) = &core.columns[0] else {
            return None;
        };
        let Expr::Function(f) = &re.expr else {
            return None;
        };
        if !f.name.name.eq_ignore_ascii_case("count")
            || !(f.star || f.args.is_empty())
            || f.filter.is_some()
            || f.over.is_some()
            || f.order_by.is_some()
            || f.quantifier.is_some()
        {
            return None;
        }
        let out_col = match &re.alias {
            Some(a) => a.name.clone(),
            None => re.text.trim().to_string(),
        };

        let mut conjs = Vec::new();
        collect_dml_and_conjuncts(we, &mut conjs);
        let mut outer_lo: Option<i64> = None;
        let mut outer_hi: Option<i64> = None;
        let mut outer_empty = false;
        let mut in_clause: Option<(&Expr, &SelectStmt)> = None;

        for conj in conjs {
            match conj {
                Expr::InSelect {
                    not: false,
                    operand,
                    select,
                    ..
                } if in_clause.is_none() => {
                    in_clause = Some((operand.as_ref(), select.as_ref()));
                }
                Expr::Between {
                    not: false,
                    operand,
                    low,
                    high,
                    ..
                } if is_dml_rowid_ref(operand, table, alias) => {
                    let lv = try_eval_literal_const_expr(low).ok()??;
                    let hv = try_eval_literal_const_expr(high).ok()??;
                    match coerce_rowid_lower_bound(&lv, true)? {
                        Some(l) => outer_lo = Some(outer_lo.map_or(l, |p| p.max(l))),
                        None => outer_empty = true,
                    }
                    match coerce_rowid_upper_bound(&hv, true)? {
                        Some(h) => outer_hi = Some(outer_hi.map_or(h, |p| p.min(h))),
                        None => outer_empty = true,
                    }
                }
                Expr::Binary {
                    op, left, right, ..
                } if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                ) =>
                {
                    let (val_expr, eff_op) = if is_dml_rowid_ref(left, table, alias) {
                        (right.as_ref(), *op)
                    } else if is_dml_rowid_ref(right, table, alias) {
                        (left.as_ref(), flip_dml_cmp(*op))
                    } else {
                        return None;
                    };
                    let val = try_eval_literal_const_expr(val_expr).ok()??;
                    match eff_op {
                        BinaryOp::Ge | BinaryOp::Gt => {
                            match coerce_rowid_lower_bound(&val, eff_op == BinaryOp::Ge)? {
                                Some(l) => outer_lo = Some(outer_lo.map_or(l, |p| p.max(l))),
                                None => outer_empty = true,
                            }
                        }
                        BinaryOp::Le | BinaryOp::Lt => {
                            match coerce_rowid_upper_bound(&val, eff_op == BinaryOp::Le)? {
                                Some(h) => outer_hi = Some(outer_hi.map_or(h, |p| p.min(h))),
                                None => outer_empty = true,
                            }
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }

        let (operand, sub_sel) = in_clause?;
        let Expr::Column(oc) = operand else {
            return None;
        };
        if oc
            .schema
            .as_ref()
            .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
            || oc
                .table
                .as_ref()
                .is_some_and(|t| !t.name.eq_ignore_ascii_case(alias))
        {
            return None;
        }
        let ocid = table
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&oc.column.name))?;
        let ocol = &table.columns[ocid];
        if ocol.hidden == ColumnHiddenKind::Virtual || !ocol.collation.eq_ignore_ascii_case("BINARY")
        {
            return None;
        }
        let outer_col = if Some(ocid) == table.ipk_column {
            DirectTableSlot::Rowid
        } else {
            DirectTableSlot::Field {
                storage_idx: ocol.storage_index?,
                default_val: ocol.default_value.clone(),
                real_back: ocol.affinity == Affinity::Real,
            }
        };

        // Match simple uncorrelated `SELECT sub_col FROM sub_tbl` (no WHERE/GROUP BY/LIMIT).
        if sub_sel.with.is_some()
            || sub_sel.order_by.is_some()
            || sub_sel.order_by_all.is_some()
            || sub_sel.limit.is_some()
        {
            return None;
        }
        let SelectBody::Core(sub_core) = &sub_sel.body else {
            return None;
        };
        if sub_core.where_clause.is_some()
            || sub_core.group_by.is_some()
            || sub_core.group_by_all
            || sub_core.having.is_some()
            || sub_core.windows.is_some()
            || sub_core.columns.len() != 1
        {
            return None;
        }
        let Some(xdb_parser::FromSource::Table(sft)) = &sub_core.from else {
            return None;
        };
        if sft.indexed_by.is_some()
            || sft
                .schema
                .as_ref()
                .is_some_and(|s| !s.name.eq_ignore_ascii_case("main"))
        {
            return None;
        }
        let sub_tbl = schema.table(&sft.name.name)?;
        if sub_tbl.without_rowid || sub_tbl.has_virtual_columns || sub_tbl.system {
            return None;
        }
        let sub_alias = sft.alias.as_ref().unwrap_or(&sft.name).name.as_str();
        let ResultColumn::Expr(sub_re) = &sub_core.columns[0] else {
            return None;
        };
        let Expr::Column(sc) = &sub_re.expr else {
            return None;
        };
        if sc.schema.is_some()
            || sc
                .table
                .as_ref()
                .is_some_and(|t| !t.name.eq_ignore_ascii_case(sub_alias))
        {
            return None;
        }
        let scid = sub_tbl
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&sc.column.name))?;
        let scol = &sub_tbl.columns[scid];
        if scol.hidden == ColumnHiddenKind::Virtual || !scol.collation.eq_ignore_ascii_case("BINARY")
        {
            return None;
        }
        if ocol.affinity < Affinity::Numeric || scol.affinity < Affinity::Numeric {
            return None;
        }
        let sub_col = if Some(scid) == sub_tbl.ipk_column {
            DirectTableSlot::Rowid
        } else {
            DirectTableSlot::Field {
                storage_idx: scol.storage_index?,
                default_val: scol.default_value.clone(),
                real_back: scol.affinity == Affinity::Real,
            }
        };
        let cmp_aff = comparison_affinity(Some(ocol.affinity), Some(scol.affinity));

        Some(CachedFastSelect::CountInSubquery {
            schema_gen: sgen,
            outer_root: table.root_page,
            outer_lo,
            outer_hi,
            outer_empty,
            outer_col,
            sub_root: sub_tbl.root_page,
            sub_col,
            cmp_aff,
            out_col,
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
                            let hit_last = cache
                                .last()
                                .is_some_and(|e| e.dialect == self.dialect && e.sql == sql);
                            let entry_opt = if hit_last {
                                cache.last_mut()
                            } else {
                                cache
                                    .iter_mut()
                                    .rev()
                                    .find(|e| e.dialect == self.dialect && e.sql == sql)
                            };
                            if let Some(entry) = entry_opt {
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
                if self.stmt_cache[last_idx].stmts.len() == 1
                    && !self.query_only
                    && self.trigger_ctx.is_none()
                {
                    let sgen = self.pager.schema_gen();
                    let taken = self.stmt_cache[last_idx].fast_dml.take();
                    let fast = match taken {
                        Some(f) if f.schema_gen() == sgen => f,
                        _ => {
                            let stmt0 = Arc::clone(&self.stmt_cache[last_idx].stmts[0].stmt);
                            Arc::new(self.try_build_fast_dml(&stmt0))
                        }
                    };
                    let fk_blocks_fast = self.foreign_keys
                        && match fast.as_ref() {
                            CachedFastDml::SimpleInsert { dml, .. }
                            | CachedFastDml::PointUpdateSingleCol { dml, .. }
                            | CachedFastDml::PointDeleteRowid { dml, .. } => {
                                dml.has_outgoing_fks || dml.has_incoming_fks
                            }
                            CachedFastDml::NotEligible { .. } => true,
                        };
                    let exec_res = if !fk_blocks_fast
                        && !matches!(fast.as_ref(), CachedFastDml::NotEligible { .. })
                    {
                        self.try_exec_fast_dml(&fast, params)
                    } else {
                        Ok(None)
                    };
                    if let Some(slot) = self.stmt_cache.get_mut(last_idx) {
                        slot.fast_dml = Some(fast);
                    }
                    if let Some(res) = exec_res? {
                        return Ok(res);
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
                    || dml.has_insert_triggers
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
                    || upd.table.indexed_by.is_some()
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
                    || dml.has_update_triggers
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
                    || del.table.indexed_by.is_some()
                    || del.table.name.name.eq_ignore_ascii_case("sqlite_sequence")
                {
                    return not_elig();
                }
                let Ok(dml) = self.get_dml_state_by_name(&del.table.name.name) else {
                    return not_elig();
                };
                let table = &dml.table;
                if table.system
                    || table.without_rowid
                    || !dml.simple_delete_elidable
                    || dml.has_delete_triggers
                {
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
                            conn.update_sqlite_sequence_at(&table.name, seq_idx, rid)
                        } else {
                            conn.update_sqlite_sequence(&table.name, rid)
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
                self.check_deferred_fks_for_commit()?;
                self.invalidate_warm_view();
                self.pager.commit()?;
                self.fk_clear_txn();
                Ok(self.empty_exec_result())
            }
            Statement::Rollback { savepoint, .. } => {
                if let Some(sp) = savepoint {
                    // Validate savepoint BEFORE discarding deferred `sqlite_sequence` cache so a
                    // failed `ROLLBACK TO <nonexistent>` is a pure no-op!
                    self.pager.rollback_to_savepoint(&sp.name)?;
                    self.invalidate_warm_view();
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    self.invalidate_max_rowid_cache();
                    if let Some(pos) = self.txn_def_fk_sp_stack.iter().rposition(|(n, _, _)| {
                        n.as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case(&sp.name))
                    }) {
                        self.txn_def_fk = self.txn_def_fk_sp_stack[pos].1;
                        self.txn_def_imm_fk = self.txn_def_fk_sp_stack[pos].2;
                        self.txn_def_fk_sp_stack.truncate(pos + 1);
                    }
                } else {
                    self.pager.rollback()?;
                    self.invalidate_warm_view();
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    self.invalidate_max_rowid_cache();
                    self.fk_clear_txn();
                }
                Ok(self.empty_exec_result())
            }
            Statement::Savepoint { name, .. } => {
                self.flush_seq_cache()?;
                self.pager.push_savepoint(Some(name.name.clone()));
                self.txn_def_fk_sp_stack.push((
                    Some(name.name.clone()),
                    self.txn_def_fk,
                    self.txn_def_imm_fk,
                ));
                Ok(self.empty_exec_result())
            }
            Statement::Release { name, .. } => {
                self.flush_seq_cache()?;
                let will_commit = self.pager.will_release_commit(&name.name);
                if will_commit {
                    self.check_deferred_fks_for_commit()?;
                }
                self.invalidate_warm_view();
                self.pager.release_savepoint(&name.name)?;
                if will_commit {
                    self.fk_clear_txn();
                } else if let Some(pos) = self.txn_def_fk_sp_stack.iter().rposition(|(n, _, _)| {
                    n.as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&name.name))
                }) {
                    self.txn_def_fk_sp_stack.truncate(pos);
                }
                Ok(self.empty_exec_result())
            }
            Statement::Pragma(p) => {
                self.flush_seq_cache()?;
                self.invalidate_warm_view();
                self.exec_pragma(p)
            }
            Statement::CreateTable(ct) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.exec_create_table(ct, full_sql, raw_sql, params, dialect)
            }),
            Statement::CreateIndex(ci) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.exec_create_index(ci, raw_sql)
            }),
            Statement::CreateView(cv) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.exec_create_view(cv, raw_sql)
            }),
            Statement::CreateTrigger(ct) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.exec_create_trigger(ct, raw_sql)
            }),
            Statement::Drop(dr) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.cached_seq = None;
                conn.seq_dirty = false;
                conn.invalidate_max_rowid_cache();
                conn.exec_drop(dr)
            }),
            Statement::AlterTable(al) => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.flush_seq_cache()?;
                conn.cached_seq = None;
                conn.seq_dirty = false;
                conn.invalidate_max_rowid_cache();
                conn.exec_alter_table(al, full_sql)
            }),
            Statement::Insert(ins) => {
                self.check_writable()?;
                if self.can_elide_insert_savepoint(ins) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_insert(ins, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_insert(ins, params, dialect))
                }
            }
            Statement::Update(upd) => {
                self.check_writable()?;
                if self.can_elide_update_savepoint(upd) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_update(upd, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_update(upd, params, dialect))
                }
            }
            Statement::Delete(del) => {
                self.check_writable()?;
                if self.can_elide_delete_savepoint(del) {
                    self.run_mutating_stmt_no_savepoint(|conn| {
                        conn.exec_delete(del, params, dialect)
                    })
                } else {
                    self.run_mutating_stmt(|conn| conn.exec_delete(del, params, dialect))
                }
            }
            Statement::Vacuum { schema, into, .. } => {
                validate_main_schema(schema.as_ref())?;
                if self.pager.in_transaction() {
                    return Err(SqliteError::Sql(
                        "cannot VACUUM from within a transaction".to_string(),
                    ));
                }
                if let Some(into_expr) = into {
                    self.exec_vacuum_into(into_expr, params, dialect)
                } else {
                    self.check_writable()?;
                    self.exec_vacuum()
                }
            }
            Statement::Analyze { schema, name, .. } => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.exec_analyze(schema.as_ref(), name.as_ref())
            }),
            Statement::Reindex { schema, name, .. } => self.run_mutating_stmt(|conn| {
                conn.check_writable()?;
                conn.exec_reindex(schema.as_ref(), name.as_ref())
            }),
            Statement::Explain {
                query_plan,
                statement,
                ..
            } => {
                if !*query_plan {
                    return Err(SqliteError::Sql(
                        "EXPLAIN bytecode listing is not supported; use EXPLAIN QUERY PLAN"
                            .to_string(),
                    ));
                }
                let qopts = self.query_options(dialect);
                let view = self.live_view()?;
                let catalog = crate::sql::live_catalog_for(&view, &qopts);
                let qres = xdb_sql::explain_query_plan(&catalog, statement, params, &qopts)?;
                Ok(ExecResult {
                    columns: qres.columns,
                    rows: qres.rows,
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            other => Err(SqliteError::Sql(format!(
                "unsupported statement in Phase 3 engine: {other:?}"
            ))),
        }
    }

    pub(super) fn check_writable(&self) -> SqliteResult<()> {
        if self.query_only {
            Err(SqliteError::Sql(
                "attempt to write a readonly database".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn can_elide_insert_savepoint(&self, ins: &InsertStmt) -> bool {
        if !self.pager.in_transaction()
            || !self.pager.is_noop_meter()
            || self.trigger_ctx.is_some()
            || self.foreign_keys
        {
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
                && !dml.table.definition.autoincrement
                && !dml.has_insert_triggers;
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
            || schema
                .triggers
                .iter()
                .any(|tr| tr.table_name.eq_ignore_ascii_case(&tbl.name))
        {
            return false;
        }
        true
    }

    fn can_elide_delete_savepoint(&self, del: &DeleteStmt) -> bool {
        if !self.pager.in_transaction()
            || !self.pager.is_noop_meter()
            || self.trigger_ctx.is_some()
            || self.foreign_keys
        {
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
                && !dml.has_delete_triggers
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
        if tbl.without_rowid
            || tbl.has_virtual_columns
            || !tbl.indexes.is_empty()
            || schema
                .triggers
                .iter()
                .any(|tr| tr.table_name.eq_ignore_ascii_case(&tbl.name))
        {
            return false;
        }
        del.where_clause
            .as_ref()
            .is_some_and(|we| has_direct_rowid_eq_conjunct(we, tbl, alias))
    }

    fn can_elide_update_savepoint(&self, upd: &UpdateStmt) -> bool {
        if !self.pager.in_transaction()
            || !self.pager.is_noop_meter()
            || self.trigger_ctx.is_some()
            || self.foreign_keys
        {
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
            if dml.has_update_triggers {
                return false;
            }
            &dml.table
        } else {
            let Ok(schema) = self.schema_arc() else {
                return false;
            };
            schema_opt = schema;
            let Some(t) = schema_opt.table(&upd.table.name.name) else {
                return false;
            };
            if schema_opt
                .triggers
                .iter()
                .any(|tr| tr.table_name.eq_ignore_ascii_case(&t.name))
            {
                return false;
            }
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
        self.stmt_imm_fk = 0;
        match f(self) {
            Ok(res) => Ok(res),
            Err(e) => {
                self.stmt_imm_fk = 0;
                self.invalidate_max_rowid_cache();
                let conflict = self.pending_error_conflict.take();
                if conflict != Some(ConflictAction::Fail) {
                    self.changes = 0;
                }
                if conflict == Some(ConflictAction::Rollback) && self.pager.in_transaction() {
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    let _ = self.pager.rollback();
                    self.fk_clear_txn();
                } else {
                    let _ = self.flush_seq_cache();
                    self.cached_seq = None;
                }
                Err(e)
            }
        }
    }

    #[inline]
    fn fk_snapshot(&self) -> (i64, i64, i64) {
        (self.stmt_imm_fk, self.txn_def_fk, self.txn_def_imm_fk)
    }

    #[inline]
    fn fk_restore(&mut self, snap: (i64, i64, i64)) {
        self.stmt_imm_fk = snap.0;
        self.txn_def_fk = snap.1;
        self.txn_def_imm_fk = snap.2;
    }

    #[inline]
    fn fk_clear_txn(&mut self) {
        self.defer_foreign_keys = false;
        self.txn_def_fk = 0;
        self.txn_def_imm_fk = 0;
        self.txn_def_fk_sp_stack.clear();
    }

    #[inline]
    fn has_pending_fk(&self) -> bool {
        self.stmt_imm_fk > 0 || self.txn_def_fk > 0 || self.txn_def_imm_fk > 0
    }

    #[inline]
    fn has_pending_deferred_fk(&self) -> bool {
        self.txn_def_fk > 0 || self.txn_def_imm_fk > 0
    }

    fn check_deferred_fks_for_commit(&mut self) -> SqliteResult<()> {
        if self.foreign_keys && self.has_pending_deferred_fk() {
            return Err(SqliteError::Sql(
                "FOREIGN KEY constraint failed".to_string(),
            ));
        }
        Ok(())
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
    /// `ON CONFLICT ABORT`/`FAIL`/`ROLLBACK`, immediate FK verification, and auto-commit handling.
    fn run_mutating_stmt<F>(&mut self, f: F) -> SqliteResult<ExecResult>
    where
        F: FnOnce(&mut Self) -> SqliteResult<ExecResult>,
    {
        self.flush_seq_cache()?;
        self.invalidate_warm_view();
        self.pending_error_conflict = None;
        self.stmt_imm_fk = 0;
        let prev_changes = self.changes;
        let prev_total_changes = self.total_changes;
        let prev_last_insert_rowid = self.last_insert_rowid;
        let prev_txn_def_fk = self.txn_def_fk;
        let prev_txn_def_imm_fk = self.txn_def_imm_fk;
        self.pager.push_savepoint(None);
        match f(self) {
            Ok(res) => {
                let had_imm_fk = self.foreign_keys && self.stmt_imm_fk > 0;
                self.stmt_imm_fk = 0;
                if had_imm_fk {
                    self.pager.rollback_statement_savepoint();
                    self.invalidate_warm_view();
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    self.invalidate_max_rowid_cache();
                    self.txn_def_fk = prev_txn_def_fk;
                    self.txn_def_imm_fk = prev_txn_def_imm_fk;
                    self.changes = 0;
                    self.total_changes = prev_total_changes;
                    self.last_insert_rowid = prev_last_insert_rowid;
                    if !self.pager.in_transaction() {
                        self.defer_foreign_keys = false;
                    }
                    return Err(SqliteError::Sql(
                        "FOREIGN KEY constraint failed".to_string(),
                    ));
                }
                if let Err(e) = self.pager.commit_statement_and_autocommit() {
                    self.invalidate_warm_view();
                    self.cached_seq = None;
                    self.seq_dirty = false;
                    self.invalidate_max_rowid_cache();
                    self.changes = prev_changes;
                    self.total_changes = prev_total_changes;
                    self.last_insert_rowid = prev_last_insert_rowid;
                    self.txn_def_fk = prev_txn_def_fk;
                    self.txn_def_imm_fk = prev_txn_def_imm_fk;
                    if !self.pager.in_transaction() {
                        self.fk_clear_txn();
                    }
                    return Err(e);
                }
                if !self.pager.in_transaction() {
                    self.fk_clear_txn();
                }
                Ok(res)
            }
            Err(e) => {
                let had_imm_fk = self.foreign_keys && self.stmt_imm_fk > 0;
                self.stmt_imm_fk = 0;
                self.invalidate_warm_view();
                self.cached_seq = None;
                self.seq_dirty = false;
                self.invalidate_max_rowid_cache();
                let eff = self.pending_error_conflict.take();
                match eff {
                    Some(ConflictAction::Rollback) if self.pager.in_transaction() => {
                        let _ = self.pager.rollback();
                        self.fk_clear_txn();
                        self.changes = 0;
                        self.total_changes = prev_total_changes;
                        self.last_insert_rowid = prev_last_insert_rowid;
                    }
                    Some(ConflictAction::Fail) if !had_imm_fk => {
                        if self.pager.commit_statement_and_autocommit().is_err() {
                            self.changes = prev_changes;
                            self.total_changes = prev_total_changes;
                            self.last_insert_rowid = prev_last_insert_rowid;
                            self.txn_def_fk = prev_txn_def_fk;
                            self.txn_def_imm_fk = prev_txn_def_imm_fk;
                        }
                        if !self.pager.in_transaction() {
                            self.fk_clear_txn();
                        }
                    }
                    Some(ConflictAction::Fail) => {
                        // Immediate FK violation on earlier rows overrides `OR FAIL` / `RAISE(FAIL)`!
                        self.pager.rollback_statement_savepoint();
                        self.txn_def_fk = prev_txn_def_fk;
                        self.txn_def_imm_fk = prev_txn_def_imm_fk;
                        self.changes = 0;
                        self.total_changes = prev_total_changes;
                        self.last_insert_rowid = prev_last_insert_rowid;
                        if !self.pager.in_transaction() {
                            self.defer_foreign_keys = false;
                        }
                        return Err(SqliteError::Sql(
                            "FOREIGN KEY constraint failed".to_string(),
                        ));
                    }
                    _ => {
                        self.pager.rollback_statement_savepoint();
                        self.txn_def_fk = prev_txn_def_fk;
                        self.txn_def_imm_fk = prev_txn_def_imm_fk;
                        self.changes = 0;
                        self.total_changes = prev_total_changes;
                        self.last_insert_rowid = prev_last_insert_rowid;
                        if !self.pager.in_transaction() {
                            self.defer_foreign_keys = false;
                        }
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
        let mut invalidated = false;
        for (_, name_val, rid, clamped, loc_init, dirty) in list {
            if !*dirty {
                continue;
            }
            let mut loc = if invalidated { None } else { *loc_init };
            match table_update_int_field_cached(
                &mut ov,
                seq_root,
                *rid,
                1,
                *clamped,
                &mut loc,
                &mut scratch,
            ) {
                Ok((FastFieldUpdateOutcome::Updated, repacked)) => {
                    if repacked {
                        invalidated = true;
                    }
                }
                _ => {
                    let seq_val = SqlValue::Int(*clamped);
                    let refs = [name_val, &seq_val];
                    encode_record_refs_into(&refs, enc, fmt, &mut scratch);
                    let _ = table_insert(&mut ov, seq_root, *rid, &scratch, true);
                    invalidated = true;
                }
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
        let Some((seq_root, rid, old_seq, mut loc)) =
            self.cached_seq.as_ref().and_then(|(root, list)| {
                list.get(idx)
                    .map(|(_, _, rid, seq, loc, _)| (*root, *rid, *seq, *loc))
            })
        else {
            return self.update_sqlite_sequence(table_name, new_seq);
        };
        if new_seq <= old_seq {
            return Ok(());
        }
        // Inside an explicit transaction with no nested savepoints and noop meter, defer
        // physical `sqlite_sequence` page writes until COMMIT / SAVEPOINT / DDL / read overlay.
        if self.pager.in_transaction() && !self.pager.has_savepoints() && self.pager.is_noop_meter()
        {
            if let Some((_, ref mut list)) = self.cached_seq {
                list[idx].3 = new_seq;
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
            new_seq,
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
                list[idx].3 = new_seq;
                list[idx].4 = loc;
                list[idx].5 = false;
            }
            return Ok(());
        }
        self.invalidate_max_rowid_for(seq_root);
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let seq_val = SqlValue::Int(new_seq);
        if let Some((_, ref list)) = self.cached_seq {
            let refs = [&list[idx].1, &seq_val];
            encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
        }
        table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, true)?;
        if let Some((_, ref mut list)) = self.cached_seq {
            for entry in list.iter_mut() {
                entry.4 = None;
            }
            list[idx].3 = new_seq;
            list[idx].5 = false;
        }
        Ok(())
    }

    fn update_sqlite_sequence(&mut self, table_name: &str, new_seq: i64) -> SqliteResult<()> {
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
            if new_seq > old_seq {
                if self.pager.in_transaction()
                    && !self.pager.has_savepoints()
                    && self.pager.is_noop_meter()
                {
                    if let Some((_, ref mut list)) = self.cached_seq {
                        list[idx].3 = new_seq;
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
                    new_seq,
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
                        list[idx].3 = new_seq;
                        list[idx].4 = loc;
                        list[idx].5 = false;
                    }
                    return Ok(());
                }
                self.invalidate_max_rowid_for(seq_root);
                let seq_val = SqlValue::Int(new_seq);
                if let Some((_, ref list)) = self.cached_seq {
                    let refs = [&list[idx].1, &seq_val];
                    encode_record_refs_into(&refs, enc, fmt, &mut self.record_scratch);
                }
                table_insert(&mut self.pager, seq_root, rid, &self.record_scratch, true)?;
                if let Some((_, ref mut list)) = self.cached_seq {
                    for entry in list.iter_mut() {
                        entry.4 = None;
                    }
                    list[idx].3 = new_seq;
                    list[idx].5 = false;
                }
            }
        } else {
            // Only clamp negative explicit rowids to 0 when inserting a NEW row into `sqlite_sequence`.
            let clamped = new_seq.max(0);
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

    // ───────────────────────────── VACUUM / ANALYZE / REINDEX ─────────────────────────────

    fn ensure_sqlite_stat1(&mut self) -> SqliteResult<u32> {
        {
            let schema = self.schema_arc()?;
            if let Some(tbl) = schema.table("sqlite_stat1") {
                return Ok(tbl.root_page);
            }
        }
        self.pager.ensure_initialized()?;
        let root = create_btree(&mut self.pager, BtreePageType::TableLeaf)?;
        self.insert_schema_row(
            "table",
            "sqlite_stat1",
            "sqlite_stat1",
            root,
            Some("CREATE TABLE sqlite_stat1(tbl,idx,stat)"),
        )?;
        self.pager.bump_schema_cookie()?;
        let _ = self.live_view()?;
        Ok(root)
    }

    fn build_vacuumed_connection(&mut self) -> SqliteResult<SqliteConnection> {
        self.flush_seq_cache()?;
        let reserved = self.pager.reserved_bytes();
        let target_ps = self
            .pager
            .pending_page_size()
            .filter(|&ps| u32::from(reserved) + 480 <= ps)
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
        tmp.pager.set_reserved_bytes(reserved);
        tmp.cached_collations = self.cached_collations.clone();
        tmp.cached_functions = self.cached_functions.clone();
        tmp.foreign_keys = false;
        tmp.pager.ensure_initialized()?;
        tmp.pager.set_user_version(self.pager.user_version())?;
        tmp.pager.set_application_id(self.pager.application_id())?;

        if self.pager.page_count() == 0 {
            return Ok(tmp);
        }

        let view = self.live_view()?;
        let mut table_ddls: Vec<String> = Vec::new();
        let mut extra_stat_tables: Vec<(String, String)> = Vec::new();
        let mut vtab_rows: Vec<(String, String)> = Vec::new();
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
                    let lname = name.to_ascii_lowercase();
                    if view.schema().virtual_table(name).is_some() {
                        vtab_rows.push((name.to_string(), sql.clone()));
                    } else if matches!(
                        lname.as_str(),
                        "sqlite_stat2" | "sqlite_stat3" | "sqlite_stat4"
                    ) {
                        extra_stat_tables.push((name.to_string(), sql.clone()));
                    } else if !lname.starts_with("sqlite_") {
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

        // 1. Recreate all user tables cleanly (creating fresh root pages & autoindexes),
        //    preserving any virtual table entries verbatim in `sqlite_schema`.
        tmp.in_vacuum_replay = true;
        for ddl in &table_ddls {
            tmp.execute(ddl)?;
        }
        tmp.in_vacuum_replay = false;
        for (vt_name, vt_sql) in &vtab_rows {
            tmp.insert_schema_row("table", vt_name, vt_name, 0, Some(vt_sql))?;
            tmp.pager.bump_schema_cookie()?;
        }

        // 2. Copy all user table rows compactly into `tmp`, preserving exact rowids & WITHOUT ROWID PKs.
        let qopts = tmp.query_options(self.dialect);
        for src_tbl in &view.schema().tables {
            if src_tbl.system
                || src_tbl.name.to_ascii_lowercase().starts_with("sqlite_")
            {
                continue;
            }
            let dst_tbl = {
                let tv = tmp.live_view()?;
                tv.schema()
                    .table(&src_tbl.name)
                    .cloned()
                    .ok_or_else(|| {
                        SqliteError::Sql(format!("VACUUM missing recreated table {}", src_tbl.name))
                    })?
            };
            let need_virt = dst_tbl.has_virtual_columns
                && dst_tbl
                    .indexes
                    .iter()
                    .any(|ix| !ix.without_rowid_primary_key);
            let mut rows_mem = ConnMemGuard::new(&self.pager);
            let rows = self.read_all_table_rows_opts(&view, src_tbl, &mut rows_mem, need_virt)?;
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
                let mut wr_mem = ConnMemGuard::new(&self.pager);
                let mut wr_entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                for (_, full_row) in &rows {
                    let mut rec = Vec::with_capacity(pk_idx.columns.len());
                    for ic in &pk_idx.columns {
                        rec.push(full_row[ic.cid as usize].clone());
                    }
                    wr_mem.reserve(estimate_sql_row_bytes(&rec))?;
                    wr_entries.push(rec);
                }
                wr_entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(wr_entries.len());
                for e in &wr_entries {
                    let p = encode_record_with_format(e, enc, sf);
                    wr_mem.reserve(24u64.saturating_add(p.len() as u64))?;
                    payloads.push(p);
                }
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
                let mut idx_mem = ConnMemGuard::new(&self.pager);
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
                        idx_mem.reserve(estimate_sql_row_bytes(&entry_vals))?;
                        idx_entries.push(entry_vals);
                    }
                }
                idx_entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(idx_entries.len());
                for e in &idx_entries {
                    let p = encode_record_with_format(e, enc, sf);
                    idx_mem.reserve(24u64.saturating_add(p.len() as u64))?;
                    payloads.push(p);
                }
                index_bulk_load_sorted(&mut tmp.pager, ix.root_page, &payloads)?;
            }
        }

        // 3. Preserve `sqlite_sequence` and `sqlite_stat1` rows if present.
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
        if let Some(src_stat) = view.schema().table("sqlite_stat1") {
            let stat_root = tmp.ensure_sqlite_stat1()?;
            let mut stat_mem = ConnMemGuard::new(&self.pager);
            let stat_rows = self.read_all_table_rows(&view, src_stat, &mut stat_mem)?;
            let mut hint_leaf: Option<u32> = None;
            for (rid_opt, full_row) in &stat_rows {
                let rid = rid_opt.unwrap_or(1);
                encode_record_with_format_into(full_row, enc, sf, &mut tmp.record_scratch);
                table_append_monotonic(
                    &mut tmp.pager,
                    stat_root,
                    &mut hint_leaf,
                    rid,
                    &tmp.record_scratch,
                )?;
            }
        }

        for (stat_name, stat_sql) in &extra_stat_tables {
            if let Some(src_stat) = view.schema().table(stat_name) {
                let root = create_btree(&mut tmp.pager, BtreePageType::TableLeaf)?;
                tmp.insert_schema_row("table", stat_name, stat_name, root, Some(stat_sql))?;
                tmp.pager.bump_schema_cookie()?;
                let mut stat_mem = ConnMemGuard::new(&self.pager);
                let stat_rows = self.read_all_table_rows(&view, src_stat, &mut stat_mem)?;
                let mut hint_leaf: Option<u32> = None;
                for (rid_opt, full_row) in &stat_rows {
                    let rid = rid_opt.unwrap_or(1);
                    encode_record_with_format_into(full_row, enc, sf, &mut tmp.record_scratch);
                    table_append_monotonic(
                        &mut tmp.pager,
                        root,
                        &mut hint_leaf,
                        rid,
                        &tmp.record_scratch,
                    )?;
                }
            }
        }

        // 4. Rebuild explicit indexes, views, and preserve trigger definitions.
        for ddl in &index_ddls {
            tmp.execute(ddl)?;
        }
        for ddl in &view_ddls {
            match tmp.execute(ddl) {
                Ok(_) => {}
                Err(SqliteError::ResourceLimit(rl)) => return Err(SqliteError::ResourceLimit(rl)),
                Err(_) if self.dialect != Dialect::Xdb => {
                    let prev = tmp.dialect;
                    tmp.dialect = Dialect::Xdb;
                    let res = tmp.execute(ddl);
                    tmp.dialect = prev;
                    res?;
                }
                Err(e) => return Err(e),
            }
        }
        for (tname, tbl_name, tsql) in &trigger_rows {
            tmp.insert_schema_row("trigger", tname, tbl_name, 0, Some(tsql))?;
        }

        drop(view);
        Ok(tmp)
    }

    fn exec_vacuum(&mut self) -> SqliteResult<ExecResult> {
        self.flush_seq_cache()?;
        if self.pager.page_count() == 0 {
            if let Some(ps) = self.pager.pending_page_size() {
                self.pager.set_page_size(ps)?;
            }
            return Ok(self.empty_exec_result());
        }
        let tmp = self.build_vacuumed_connection()?;
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

    fn exec_vacuum_into(
        &mut self,
        into_expr: &Expr,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        let qopts = self.query_options(dialect);
        let path_val = eval_standalone_expr(into_expr, params, &qopts)?;
        let SqlValue::Text(path_bytes) = path_val else {
            return Err(SqliteError::Sql("non-text filename".to_string()));
        };
        let path = String::from_utf8_lossy(&path_bytes).into_owned();
        if path.is_empty() {
            return Err(SqliteError::Sql("unable to open database file".to_string()));
        }
        let Some(vfs) = self.pager.vfs_handle() else {
            return Err(SqliteError::Sql(
                "VACUUM INTO requires a VFS-backed connection".to_string(),
            ));
        };
        if vfs.exists(&path)? {
            return Err(SqliteError::Sql("output file already exists".to_string()));
        }
        let tmp = self.build_vacuumed_connection()?;
        let mut out_mem = ConnMemGuard::new(&self.pager);
        let est_bytes = u64::from(tmp.pager.page_count()).saturating_mul(u64::from(tmp.pager.page_size()));
        out_mem.reserve(est_bytes)?;
        let bytes = tmp.to_bytes();
        let mut out_file = vfs.open(
            &path,
            OpenFlags {
                mode: OpenMode::CreateExclusive,
                kind: FileKind::MainDb,
                delete_on_close: false,
            },
        )?;
        let write_res: SqliteResult<()> = (|| {
            out_file.write(&bytes, 0)?;
            out_file.sync(false)?;
            out_file.close()?;
            Ok(())
        })();
        if let Err(e) = write_res {
            let _ = out_file.close();
            drop(out_file);
            let _ = vfs.delete(&path, false);
            return Err(e);
        }
        Ok(self.empty_exec_result())
    }

    fn exec_reindex(
        &mut self,
        schema_name: Option<&xdb_parser::Name>,
        target_name: Option<&xdb_parser::Name>,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(schema_name)?;
        self.flush_seq_cache()?;
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let qopts = self.query_options(self.dialect);

        // Determine `(table_name, rebuild_wr_pk, Vec<secondary_index_name>)` triples to rebuild.
        let targets: Vec<(String, bool, Vec<String>)> = {
            let view = self.live_view()?;
            let cat = view.schema();
            if let Some(t) = target_name {
                let tn = t.name.as_str();
                // Per SQLite: collation names are only resolved when no schema qualifier is given.
                let is_collation =
                    schema_name.is_none() && self.cached_collations.1.find(tn).is_some();
                if is_collation {
                    let mut out = Vec::new();
                    for tbl in &cat.tables {
                        if tbl.system {
                            continue;
                        }
                        let rebuild_wr = tbl.without_rowid
                            && tbl.indexes.iter().any(|ix| {
                                ix.without_rowid_primary_key
                                    && ix
                                        .columns
                                        .iter()
                                        .take(ix.key_count)
                                        .any(|ic| ic.collation.eq_ignore_ascii_case(tn))
                            });
                        let ixs: Vec<String> = tbl
                            .indexes
                            .iter()
                            .filter(|ix| {
                                !ix.without_rowid_primary_key
                                    && ix
                                        .columns
                                        .iter()
                                        .any(|ic| ic.collation.eq_ignore_ascii_case(tn))
                            })
                            .map(|ix| ix.name.clone())
                            .collect();
                        if rebuild_wr || !ixs.is_empty() {
                            out.push((tbl.name.clone(), rebuild_wr, ixs));
                        }
                    }
                    out
                } else if let Some(tbl) = cat.table(tn) {
                    let rebuild_wr = tbl.without_rowid
                        && tbl.indexes.iter().any(|ix| ix.without_rowid_primary_key);
                    let ixs: Vec<String> = tbl
                        .indexes
                        .iter()
                        .filter(|ix| !ix.without_rowid_primary_key)
                        .map(|ix| ix.name.clone())
                        .collect();
                    if !rebuild_wr && ixs.is_empty() {
                        Vec::new()
                    } else {
                        vec![(tbl.name.clone(), rebuild_wr, ixs)]
                    }
                } else if let Some(ix) = cat.index(tn) {
                    if ix.without_rowid_primary_key {
                        vec![(ix.definition.table.clone(), true, Vec::new())]
                    } else {
                        vec![(ix.definition.table.clone(), false, vec![ix.name.clone()])]
                    }
                } else {
                    return Err(SqliteError::Sql(
                        "unable to identify the object to be reindexed".to_string(),
                    ));
                }
            } else {
                let mut out = Vec::new();
                for tbl in &cat.tables {
                    if tbl.system {
                        continue;
                    }
                    let rebuild_wr = tbl.without_rowid
                        && tbl.indexes.iter().any(|ix| ix.without_rowid_primary_key);
                    let ixs: Vec<String> = tbl
                        .indexes
                        .iter()
                        .filter(|ix| !ix.without_rowid_primary_key)
                        .map(|ix| ix.name.clone())
                        .collect();
                    if rebuild_wr || !ixs.is_empty() {
                        out.push((tbl.name.clone(), rebuild_wr, ixs));
                    }
                }
                out
            }
        };

        if targets.is_empty() {
            return Ok(self.empty_exec_result());
        }

        for (tname, rebuild_wr, ix_names) in targets {
            let (table, rows, table_entry, schema_entries) = {
                let view = self.live_view()?;
                let Some(tbl) = view.schema().table(&tname).cloned() else {
                    continue;
                };
                let mut rows_mem = ConnMemGuard::new(&self.pager);
                let rows = self.read_all_table_rows(&view, &tbl, &mut rows_mem)?;
                let tent = view
                    .schema()
                    .entries
                    .iter()
                    .find(|e| {
                        e.obj_type.as_deref() == Some("table")
                            && e.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned();
                let entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.name.as_deref().is_some_and(|n| {
                                ix_names.iter().any(|want| want.eq_ignore_ascii_case(n))
                            })
                    })
                    .cloned()
                    .collect();
                (tbl, rows, tent, entries)
            };

            if rebuild_wr {
                if let (Some(pk_idx), Some(te)) = (
                    table.indexes.iter().find(|ix| ix.without_rowid_primary_key),
                    table_entry.as_ref(),
                ) {
                    let ki = key_info_for_index(pk_idx, enc, Some(&self.cached_collations.1))?;
                    let mut wr_mem = ConnMemGuard::new(&self.pager);
                    let mut entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                    for (_, full_row) in &rows {
                        let mut rec = Vec::with_capacity(pk_idx.columns.len());
                        for ic in &pk_idx.columns {
                            rec.push(full_row[ic.cid as usize].clone());
                        }
                        wr_mem.reserve(estimate_sql_row_bytes(&rec))?;
                        entries.push(rec);
                    }
                    entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                    for pair in entries.windows(2) {
                        if key_tuples_equal(
                            &pair[0][..pk_idx.key_count],
                            &pair[1][..pk_idx.key_count],
                            &ki,
                        ) {
                            return Err(SqliteError::Sql(format!(
                                "UNIQUE constraint failed: {}",
                                table.name
                            )));
                        }
                    }
                    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
                    for e in &entries {
                        let p = encode_record_with_format(e, enc, fmt);
                        wr_mem.reserve(24u64.saturating_add(p.len() as u64))?;
                        payloads.push(p);
                    }
                    let new_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
                    index_bulk_load_sorted(&mut self.pager, new_root, &payloads)?;
                    self.update_schema_row(
                        te.rowid,
                        "table",
                        te.name.as_deref().unwrap_or(&table.name),
                        te.table_name.as_deref().unwrap_or(&table.name),
                        new_root,
                        te.sql.as_deref(),
                    )?;
                    if te.root_page > 1 {
                        drop_btree(&mut self.pager, te.root_page)?;
                    }
                }
            }

            let expr_scope_cols = build_plain_scope_columns(&table);
            let where_scope_cols = build_table_scope_columns(&table, &table.name);
            for ix_name in &ix_names {
                let Some(ix) = table
                    .indexes
                    .iter()
                    .find(|i| i.name.eq_ignore_ascii_case(ix_name))
                else {
                    continue;
                };
                let Some(se) = schema_entries
                    .iter()
                    .find(|e| e.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&ix.name)))
                else {
                    continue;
                };
                let ki = key_info_for_index(ix, enc, Some(&self.cached_collations.1))?;
                let mut ix_mem = ConnMemGuard::new(&self.pager);
                let mut entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                for (rid_opt, full_row) in &rows {
                    if let Some(entry_vals) = build_index_entry_values_fast(
                        &table,
                        ix,
                        *rid_opt,
                        full_row,
                        &qopts,
                        &expr_scope_cols,
                        &where_scope_cols,
                    )? {
                        ix_mem.reserve(estimate_sql_row_bytes(&entry_vals))?;
                        entries.push(entry_vals);
                    }
                }
                entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                if ix.unique {
                    for pair in entries.windows(2) {
                        let a_prefix = &pair[0][..ix.key_count];
                        let b_prefix = &pair[1][..ix.key_count];
                        if !prefix_has_null(a_prefix)
                            && !prefix_has_null(b_prefix)
                            && key_tuples_equal(a_prefix, b_prefix, &ki)
                        {
                            return Err(SqliteError::Sql(format!(
                                "UNIQUE constraint failed: index {}",
                                ix.name
                            )));
                        }
                    }
                }
                let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
                for e in &entries {
                    let p = encode_record_with_format(e, enc, fmt);
                    ix_mem.reserve(24u64.saturating_add(p.len() as u64))?;
                    payloads.push(p);
                }
                let new_root = create_btree(&mut self.pager, BtreePageType::IndexLeaf)?;
                index_bulk_load_sorted(&mut self.pager, new_root, &payloads)?;
                self.update_schema_row(
                    se.rowid,
                    "index",
                    se.name.as_deref().unwrap_or(&ix.name),
                    se.table_name.as_deref().unwrap_or(&table.name),
                    new_root,
                    se.sql.as_deref(),
                )?;
                if se.root_page > 1 {
                    drop_btree(&mut self.pager, se.root_page)?;
                }
            }
        }
        self.pager.bump_schema_cookie()?;
        let _ = self.live_view()?;
        Ok(self.empty_exec_result())
    }

    fn exec_analyze(
        &mut self,
        schema_name: Option<&xdb_parser::Name>,
        target_name: Option<&xdb_parser::Name>,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(schema_name)?;
        self.flush_seq_cache()?;
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();
        let qopts = self.query_options(self.dialect);

        // Determine `(table_names, single_index_filter, clear_all_stat1)`.
        let (table_names, single_index_filter, clear_all_stat1): (Vec<String>, Option<String>, bool) =
        {
            let view = self.live_view()?;
            let cat = view.schema();
            if let Some(t) = target_name {
                let tn = t.name.as_str();
                if schema_name.is_none() && tn.eq_ignore_ascii_case("main") {
                    let all = cat
                        .tables
                        .iter()
                        .filter(|tbl| {
                            !tbl.system && !tbl.name.to_ascii_lowercase().starts_with("sqlite_")
                        })
                        .map(|tbl| tbl.name.clone())
                        .collect();
                    (all, None, true)
                } else if let Some(tbl) = cat.table(tn) {
                    if tbl.system || tbl.name.to_ascii_lowercase().starts_with("sqlite_") {
                        (Vec::new(), None, false)
                    } else {
                        (vec![tbl.name.clone()], None, false)
                    }
                } else if let Some(ix) = cat.index(tn) {
                    (vec![ix.definition.table.clone()], Some(ix.name.clone()), false)
                } else if cat.view(tn).is_some() {
                    (Vec::new(), None, false)
                } else {
                    return Err(SqliteError::Sql(format!("no such table: {tn}")));
                }
            } else {
                let all = cat
                    .tables
                    .iter()
                    .filter(|tbl| {
                        !tbl.system && !tbl.name.to_ascii_lowercase().starts_with("sqlite_")
                    })
                    .map(|tbl| tbl.name.clone())
                    .collect();
                (all, None, true)
            }
        };

        let stat_root = self.ensure_sqlite_stat1()?;
        let mut new_stat_rows: Vec<(String, Option<String>, String)> = Vec::new();
        {
            let view = self.live_view()?;
            for tname in &table_names {
                let Some(tbl) = view.schema().table(tname) else {
                    continue;
                };
                let mut rows_mem = ConnMemGuard::new(&self.pager);
                let rows = self.read_all_table_rows(&view, tbl, &mut rows_mem)?;
                let nrows = rows.len() as u64;
                if nrows == 0 {
                    continue;
                }
                if tbl.indexes.is_empty() && single_index_filter.is_none() {
                    new_stat_rows.push((tbl.name.clone(), None, nrows.to_string()));
                    continue;
                }
                let expr_scope_cols = build_plain_scope_columns(tbl);
                let where_scope_cols = build_table_scope_columns(tbl, &tbl.name);
                let mut emitted_any_full_index = false;
                let metered = !self.pager.is_noop_meter();
                for ix in &tbl.indexes {
                    if let Some(ref only_ix) = single_index_filter {
                        if !ix.name.eq_ignore_ascii_case(only_ix) {
                            continue;
                        }
                    }
                    let ki = key_info_for_index(ix, enc, Some(&self.cached_collations.1))?;
                    let mut ix_mem = ConnMemGuard::new(&self.pager);
                    let mut entries: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                    if ix.without_rowid_primary_key {
                        for (_, full_row) in &rows {
                            if metered {
                                self.pager.meter().charge_steps(1)?;
                            }
                            let mut rec = Vec::with_capacity(ix.key_count);
                            for ic in &ix.columns[..ix.key_count] {
                                rec.push(full_row[ic.cid as usize].clone());
                            }
                            ix_mem.reserve(estimate_sql_row_bytes(&rec))?;
                            entries.push(rec);
                        }
                    } else {
                        for (rid_opt, full_row) in &rows {
                            if metered {
                                self.pager.meter().charge_steps(1)?;
                            }
                            if let Some(entry_vals) = build_index_entry_values_fast(
                                tbl,
                                ix,
                                *rid_opt,
                                full_row,
                                &qopts,
                                &expr_scope_cols,
                                &where_scope_cols,
                            )? {
                                ix_mem.reserve(estimate_sql_row_bytes(&entry_vals))?;
                                entries.push(entry_vals);
                            }
                        }
                    }
                    let idx_nrows = entries.len() as u64;
                    if idx_nrows == 0 {
                        continue;
                    }
                    if metered {
                        let log_n = (64u64 - idx_nrows.leading_zeros() as u64).max(1);
                        self.pager
                            .meter()
                            .charge_steps(idx_nrows.saturating_mul(log_n))?;
                    }
                    entries.sort_by(|a, b| compare_index_values(a, b, &ki));
                    let n_keys = ix.key_count;
                    let mut distinct = vec![1u64; n_keys];
                    for pair in entries.windows(2) {
                        if metered {
                            self.pager.meter().charge_steps(1)?;
                        }
                        let mut first_diff = n_keys;
                        for (k, (a, b)) in pair[0].iter().zip(&pair[1]).take(n_keys).enumerate() {
                            let coll = ki.collations.get(k).and_then(|c| c.as_ref());
                            if mem_compare(a, b, coll) != std::cmp::Ordering::Equal {
                                first_diff = k;
                                break;
                            }
                        }
                        for d in &mut distinct[first_diff..] {
                            *d += 1;
                        }
                    }
                    let mut stat = idx_nrows.to_string();
                    for d in distinct {
                        let avg = ((idx_nrows + d / 2) / d).max(1);
                        stat.push(' ');
                        stat.push_str(&avg.to_string());
                    }
                    let idx_label = if ix.without_rowid_primary_key {
                        tbl.name.clone()
                    } else {
                        ix.name.clone()
                    };
                    new_stat_rows.push((tbl.name.clone(), Some(idx_label), stat));
                    if !ix.partial {
                        emitted_any_full_index = true;
                    }
                }
                if !emitted_any_full_index && single_index_filter.is_none() {
                    new_stat_rows.push((tbl.name.clone(), None, nrows.to_string()));
                }
            }
        }

        // Remove stale `sqlite_stat1` entries (all on global/main ANALYZE, single index on
        // `ANALYZE idx`, or matching table on `ANALYZE tbl`).
        let stale_rids: Vec<i64> = {
            let view = self.live_view()?;
            let mut rids = Vec::new();
            if let Some(stat_tbl) = view.schema().table("sqlite_stat1") {
                let mut smem = ConnMemGuard::new(&self.pager);
                let existing = self.read_all_table_rows(&view, stat_tbl, &mut smem)?;
                for (rid_opt, row) in existing {
                    if let Some(rid) = rid_opt {
                        let is_stale = if clear_all_stat1 {
                            true
                        } else if let Some(ref only_ix) = single_index_filter {
                            row.first()
                                .and_then(SqlValue::as_str)
                                .is_some_and(|t| {
                                    table_names.iter().any(|tn| tn.eq_ignore_ascii_case(t))
                                })
                                && row
                                    .get(1)
                                    .and_then(SqlValue::as_str)
                                    .is_some_and(|ixn| ixn.eq_ignore_ascii_case(only_ix))
                        } else {
                            row.first().and_then(SqlValue::as_str).is_some_and(|t| {
                                table_names.iter().any(|tn| tn.eq_ignore_ascii_case(t))
                            })
                        };
                        if is_stale {
                            rids.push(rid);
                        }
                    }
                }
            }
            rids
        };
        for rid in stale_rids {
            table_delete(&mut self.pager, stat_root, rid)?;
        }
        let mut next_rid = table_max_rowid(&self.pager, stat_root)?.unwrap_or(0);
        for (tbl_name, idx_opt, stat) in new_stat_rows {
            next_rid = next_rid
                .checked_add(1)
                .ok_or_else(|| SqliteError::Sql("database or disk is full".into()))?;
            let vals = [
                SqlValue::text(tbl_name),
                idx_opt.map_or(SqlValue::Null, SqlValue::text),
                SqlValue::text(stat),
            ];
            encode_record_with_format_into(&vals, enc, fmt, &mut self.record_scratch);
            table_insert(&mut self.pager, stat_root, next_rid, &self.record_scratch, false)?;
        }
        self.invalidate_max_rowid_for(stat_root);
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
        if !self.in_vacuum_replay {
            for col in &def.columns {
                if let Some(ref cname) = col.collation {
                    if self.cached_collations.1.find(cname).is_none() {
                        return Err(SqliteError::UnknownCollation(cname.clone()));
                    }
                }
            }
        }
        for idx_def in &def.indexes {
            let check_cols = if idx_def.without_rowid_primary_key {
                &idx_def.columns[..idx_def.key_count.min(idx_def.columns.len())]
            } else {
                &idx_def.columns[..]
            };
            for ic in check_cols {
                if self.cached_collations.1.find(&ic.collation).is_none() {
                    return Err(SqliteError::UnknownCollation(ic.collation.clone()));
                }
            }
        }
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
        // Verify stored view DDL parses cleanly under the connection's active dialect + schema mode.
        let v_opts = ParseOptions {
            dialect: self.dialect,
            schema: true,
            ..ParseOptions::default()
        };
        xdb_parser::parse_statement(&stored_sql, &v_opts)
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        self.insert_schema_row("view", vname, vname, 0, Some(&stored_sql))?;
        self.pager.bump_schema_cookie()?;
        let _ = self.live_view()?;
        Ok(self.empty_exec_result())
    }

    fn exec_create_trigger(
        &mut self,
        stmt: &CreateTriggerStmt,
        raw_sql: &str,
    ) -> SqliteResult<ExecResult> {
        validate_main_schema(stmt.schema.as_ref())?;
        validate_main_schema(stmt.table_schema.as_ref())?;
        if stmt.temp {
            return Err(SqliteError::Sql(
                "TEMP triggers are not supported in single-database mode".to_string(),
            ));
        }
        self.pager.ensure_initialized()?;
        let tr_name = &stmt.name.name;
        if tr_name.len() >= 7 && tr_name.as_bytes()[..7].eq_ignore_ascii_case(b"sqlite_") {
            return Err(SqliteError::Sql(format!(
                "object name reserved for internal use: {tr_name}"
            )));
        }
        let target_canonical: String;
        let is_view: bool;
        {
            let view = self.live_view()?;
            if view.schema().trigger(tr_name).is_some() {
                if stmt.if_not_exists {
                    return Ok(self.empty_exec_result());
                }
                return Err(SqliteError::Sql(format!(
                    "trigger {tr_name} already exists"
                )));
            }
            match view.schema().object(&stmt.table.name) {
                Some(SchemaObject::Table(t)) => {
                    if t.system
                        || (t.name.len() >= 7
                            && t.name.as_bytes()[..7].eq_ignore_ascii_case(b"sqlite_"))
                    {
                        return Err(SqliteError::Sql(
                            "cannot create trigger on system table".to_string(),
                        ));
                    }
                    target_canonical = t.name.clone();
                    is_view = false;
                }
                Some(SchemaObject::View(v)) => {
                    target_canonical = v.name.clone();
                    is_view = true;
                }
                Some(SchemaObject::VirtualTable(_)) => {
                    return Err(SqliteError::Sql(
                        "cannot create triggers on virtual tables".to_string(),
                    ));
                }
                _ => {
                    return Err(SqliteError::Sql(format!(
                        "no such table: main.{}",
                        stmt.table.name
                    )));
                }
            }
        }
        let time = stmt.time.unwrap_or(TriggerTime::Before);
        if is_view && time != TriggerTime::InsteadOf {
            let word = match time {
                TriggerTime::Before => "BEFORE",
                TriggerTime::After => "AFTER",
                TriggerTime::InsteadOf => "INSTEAD OF",
            };
            return Err(SqliteError::Sql(format!(
                "cannot create {word} trigger on view: {}",
                stmt.table.name
            )));
        }
        if !is_view && time == TriggerTime::InsteadOf {
            return Err(SqliteError::Sql(format!(
                "cannot create INSTEAD OF trigger on table: {}",
                stmt.table.name
            )));
        }
        // SQLite forbids bind parameters (`?`, `:x`, etc.) and `RETURNING` inside triggers.
        if let Some(ref when_expr) = stmt.when {
            if trigger_expr_has_variable(when_expr) {
                return Err(SqliteError::Sql("trigger cannot use variables".to_string()));
            }
        }
        for step in &stmt.body {
            match step {
                TriggerStep::Select(sel) => {
                    if trigger_select_has_variable(sel) {
                        return Err(SqliteError::Sql("trigger cannot use variables".to_string()));
                    }
                }
                TriggerStep::Insert(ins) => {
                    if ins.returning.is_some() {
                        return Err(SqliteError::Sql(
                            "cannot use RETURNING in a trigger".to_string(),
                        ));
                    }
                    if trigger_insert_has_variable(ins) {
                        return Err(SqliteError::Sql("trigger cannot use variables".to_string()));
                    }
                }
                TriggerStep::Update(upd) => {
                    if upd.returning.is_some() {
                        return Err(SqliteError::Sql(
                            "cannot use RETURNING in a trigger".to_string(),
                        ));
                    }
                    if trigger_update_has_variable(upd) {
                        return Err(SqliteError::Sql("trigger cannot use variables".to_string()));
                    }
                }
                TriggerStep::Delete(del) => {
                    if del.returning.is_some() {
                        return Err(SqliteError::Sql(
                            "cannot use RETURNING in a trigger".to_string(),
                        ));
                    }
                    if trigger_delete_has_variable(del) {
                        return Err(SqliteError::Sql("trigger cannot use variables".to_string()));
                    }
                }
            }
        }
        let stored_sql =
            normalize_stored_ddl(raw_sql, stmt.if_not_exists, stmt.schema.is_some());
        let tr_opts = ParseOptions {
            dialect: self.dialect,
            schema: true,
            ..ParseOptions::default()
        };
        xdb_parser::parse_statement(&stored_sql, &tr_opts)
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        self.insert_schema_row("trigger", tr_name, &target_canonical, 0, Some(&stored_sql))?;
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
                let tbl_arc = Arc::clone(tbl);
                if self.foreign_keys {
                    let has_other_incoming = incoming_fks_for(view.schema(), &tbl_arc.name)
                        .iter()
                        .any(|(child_name, _)| !child_name.eq_ignore_ascii_case(&tbl_arc.name));
                    let has_outgoing_deferred = self.has_pending_deferred_fk()
                        && tbl_arc.definition.foreign_keys.iter().any(|fk| {
                            (fk.deferred && self.txn_def_fk > 0)
                                || (!fk.deferred && self.txn_def_imm_fk > 0)
                        });
                    if has_other_incoming || has_outgoing_deferred {
                        let mut _pmem = ConnMemGuard::new(&self.pager);
                        let parent_rows =
                            self.read_all_table_rows(&view, &tbl_arc, &mut _pmem)?;
                        if !parent_rows.is_empty() {
                            drop(view);
                            let dml = self.get_dml_state_by_name(&tbl_arc.name)?;
                            let mut qopts = self.query_options(self.dialect);
                            qopts.in_trigger = false;
                            // Decrement pending deferred FK counter(s) for any unmatched outgoing
                            // rows in the table being dropped (matching SQLite `sqlite3FkDropTable`,
                            // including when the referenced parent table was already dropped).
                            if has_outgoing_deferred {
                                for (_, row) in &parent_rows {
                                    self.fk_drop_child_row(&tbl_arc, row, &qopts)?;
                                }
                            }
                            if has_other_incoming {
                                self.clear_append_leaf_for(tbl_arc.root_page);
                                // Delete all rows from `parent` first (without firing `parent`'s own
                                // DELETE triggers, matching SQLite's `sqlite3FkDropTable`) so that
                                // parent-side FK checks and cascades observe `parent` as empty.
                                for (rid, prow) in &parent_rows {
                                    self.delete_row_and_indexes(
                                        &dml.table,
                                        &dml,
                                        *rid,
                                        prow,
                                        &qopts,
                                    )?;
                                }
                                for (_, prow) in &parent_rows {
                                    self.apply_incoming_fk_delete_actions_filtered(
                                        &tbl_arc,
                                        prow,
                                        self.dialect,
                                        true,
                                    )?;
                                }
                            }
                            return self.finish_drop_table(&tbl_arc.name);
                        }
                    }
                }
                drop(view);
                self.finish_drop_table(&tbl_arc.name)
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
                let ix_tbl = ix.table_name.clone();
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
                let stat_cleanup = view.schema().table("sqlite_stat1").map(|st| {
                    let st_root = st.root_page;
                    let mut rids = Vec::new();
                    if let Ok(reader) = view.table(&st.name) {
                        let opts = ScanOptions {
                            columns: None,
                            rowid: false,
                        };
                        if let Ok(mut cur) = reader.cursor(Some(&opts)) {
                            while let Ok(true) = cur.next() {
                                if let (Ok(rid), Ok(r)) = (cur.rowid(), cur.row()) {
                                    let tbl_match = r
                                        .first()
                                        .and_then(SqlValue::as_str)
                                        .is_some_and(|s| s.eq_ignore_ascii_case(&ix_tbl));
                                    let idx_match = r
                                        .get(1)
                                        .and_then(SqlValue::as_str)
                                        .is_some_and(|s| s.eq_ignore_ascii_case(&ix_name));
                                    if tbl_match && idx_match {
                                        rids.push(rid);
                                    }
                                }
                            }
                        }
                    }
                    (st_root, rids)
                });
                drop(view);

                drop_btree(&mut self.pager, ix_root)?;
                if let Some(rid) = srid {
                    table_delete(&mut self.pager, 1, rid)?;
                }
                if let Some((st_root, rids)) = stat_cleanup {
                    for rid in rids {
                        table_delete(&mut self.pager, st_root, rid)?;
                    }
                    self.invalidate_max_rowid_for(st_root);
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
                let srids: Vec<i64> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        (e.obj_type.as_deref() == Some("view")
                            && e.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&vname)))
                            || (e.obj_type.as_deref() == Some("trigger")
                                && e.table_name
                                    .as_deref()
                                    .is_some_and(|tn| tn.eq_ignore_ascii_case(&vname)))
                    })
                    .map(|e| e.rowid)
                    .collect();
                drop(view);
                for rid in srids {
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

    fn finish_drop_table(&mut self, tbl_name: &str) -> SqliteResult<ExecResult> {
        let view = self.live_view()?;
        let Some(tbl) = view.schema().table(tbl_name) else {
            return Ok(self.empty_exec_result());
        };
        let canon = tbl.name.clone();
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
                    .is_some_and(|tn| tn.eq_ignore_ascii_case(&canon))
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
                        .is_some_and(|n| n.eq_ignore_ascii_case(&canon))
                    {
                        seq_del_rids.push(rid);
                    }
                }
            }
        }
        let stat_root = view.schema().table("sqlite_stat1").map(|s| s.root_page);
        let mut stat_del_rids: Vec<i64> = Vec::new();
        if let Some(stat_tbl) = view.schema().table("sqlite_stat1") {
            if let Ok(reader) = view.table(&stat_tbl.name) {
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
                        .is_some_and(|n| n.eq_ignore_ascii_case(&canon))
                    {
                        stat_del_rids.push(rid);
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
        if let Some(sr) = stat_root {
            for rid in stat_del_rids {
                table_delete(&mut self.pager, sr, rid)?;
            }
        }
        self.cached_seq = None;
        self.pager.bump_schema_cookie()?;
        Ok(self.empty_exec_result())
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
                let updated_sql =
                    rewrite_fk_table_name_in_ddl(&updated_sql, &tbl.name, &new_name.name);
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
                let other_table_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("table")
                            && e.sql.is_some()
                            && !e
                                .name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
                let other_trigger_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("trigger")
                            && e.sql.is_some()
                            && !e
                                .table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
                    .collect();
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
                    let dep_sql = dep.sql.as_deref().map(|s| {
                        if dep.obj_type.as_deref() == Some("trigger") {
                            rewrite_trigger_table_name(s, &tbl.name, &new_name.name)
                        } else {
                            rewrite_on_table_name(s, &tbl.name, &new_name.name)
                        }
                    });
                    self.update_schema_row(
                        dep.rowid,
                        dep.obj_type.as_deref().unwrap_or("index"),
                        &dep_name_new,
                        &new_name.name,
                        dep.root_page,
                        dep_sql.as_deref(),
                    )?;
                }
                for te in other_table_entries {
                    if let Some(ref tsql) = te.sql {
                        let new_tsql =
                            rewrite_fk_table_name_in_ddl(tsql, &tbl.name, &new_name.name);
                        if new_tsql != *tsql {
                            let tname = te.name.as_deref().unwrap_or("");
                            self.update_schema_row(
                                te.rowid,
                                "table",
                                tname,
                                tname,
                                te.root_page,
                                Some(&new_tsql),
                            )?;
                        }
                    }
                }
                for te in other_trigger_entries {
                    if let Some(ref tsql) = te.sql {
                        let new_tsql = rewrite_trigger_table_name(tsql, &tbl.name, &new_name.name);
                        if new_tsql != *tsql {
                            let tname = te.name.as_deref().unwrap_or("");
                            let tbl_ref = te.table_name.as_deref().unwrap_or(tname);
                            self.update_schema_row(
                                te.rowid,
                                "trigger",
                                tname,
                                tbl_ref,
                                te.root_page,
                                Some(&new_tsql),
                            )?;
                        }
                    }
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
                let new_view = self.live_view()?;
                self.validate_all_views_after_alter(&new_view, "rename")?;
                self.validate_all_triggers_after_alter(&new_view, "rename")?;
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
                let mut has_references = false;
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
                        ColumnConstraint::References { .. } => {
                            has_references = true;
                        }
                        ColumnConstraint::Default { form, expr, .. } => {
                            let is_current_time = matches!(expr, Expr::CurrentTime { .. });
                            let paren_const_val = if *form == DefaultForm::Paren {
                                if expr_has_variables(expr) {
                                    None
                                } else {
                                    crate::schema::value_from_expr(
                                        expr,
                                        Affinity::Blob,
                                        self.pager.encoding(),
                                    )
                                }
                            } else {
                                None
                            };
                            let is_non_const =
                                is_current_time || (*form == DefaultForm::Paren && paren_const_val.is_none());
                            if is_non_const {
                                let has_rows = view.table(&tbl.name)?.cursor(None)?.next()?;
                                if has_rows {
                                    return Err(SqliteError::Sql(
                                        "Cannot add a column with non-constant default".to_string(),
                                    ));
                                }
                            }
                            let is_null_dflt = if is_current_time {
                                false
                            } else if *form == DefaultForm::Paren {
                                if let Some(v) = paren_const_val {
                                    v.is_null()
                                } else {
                                    let qopts = self.query_options(self.dialect);
                                    eval_standalone_expr(expr, &QueryParams::empty(), &qopts)
                                        .is_ok_and(|v| v.is_null())
                                }
                            } else {
                                match expr {
                                    Expr::Null { .. } => true,
                                    Expr::Unary {
                                        op: xdb_parser::UnaryOp::Pos | xdb_parser::UnaryOp::Neg,
                                        operand,
                                        ..
                                    } => matches!(operand.as_ref(), Expr::Null { .. }),
                                    _ => false,
                                }
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
                if self.foreign_keys && has_references && has_non_null_default {
                    let has_rows = view.table(&tbl.name)?.cursor(None)?.next()?;
                    if has_rows {
                        return Err(SqliteError::Sql(
                            "Cannot add a REFERENCES column with non-NULL default value"
                                .to_string(),
                        ));
                    }
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
                    if let Some(new_col) = def.columns.last() {
                        if let Some(ref cname) = new_col.collation {
                            if self.cached_collations.1.find(cname).is_none() {
                                return Err(SqliteError::UnknownCollation(cname.clone()));
                            }
                        }
                    }
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
                        let new_schema = Arc::clone(new_view.schema_arc());
                        drop(new_view);
                        let qopts = self.query_options(self.dialect);
                        let empty_params = QueryParams::empty();
                        let dml = DmlTableState::build(
                            Arc::clone(&new_tbl),
                            &new_schema,
                            self.pager.encoding(),
                            &self.cached_collations.1,
                        )?;
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
                    .enumerate()
                    .any(|(i, c)| i != cid && c.name.eq_ignore_ascii_case(&new_name.name))
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
                let mut view_rewrites: Vec<(crate::schema::SchemaEntry, String)> = Vec::new();
                for ve in view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| e.obj_type.as_deref() == Some("view") && e.sql.is_some())
                {
                    let Some(vsql) = ve.sql.as_deref() else {
                        continue;
                    };
                    let vname = ve.name.as_deref().unwrap_or("");
                    if !old_col_name.eq_ignore_ascii_case(&new_name.name) {
                        check_view_using_rename_safe(vsql, vname, &tbl.name, &old_col_name)?;
                    }
                    let new_vsql = rewrite_column_name_in_view_ddl(
                        vsql,
                        &tbl.name,
                        &old_col_name,
                        &new_name.name,
                        view.schema(),
                    );
                    if new_vsql != vsql {
                        view_rewrites.push((ve.clone(), new_vsql));
                    }
                }
                let fk_table_rewrites: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| e.obj_type.as_deref() == Some("table") && e.sql.is_some())
                    .filter_map(|te| {
                        if te
                            .name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&tbl.name))
                        {
                            return None;
                        }
                        let tsql = te.sql.as_deref()?;
                        let new_tsql = rewrite_fk_parent_column_in_ddl(
                            tsql,
                            &tbl.name,
                            &old_col_name,
                            &new_name.name,
                        );
                        if new_tsql != tsql {
                            Some((te.clone(), new_tsql))
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut trigger_rewrites = Vec::new();
                for te in view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| e.obj_type.as_deref() == Some("trigger") && e.sql.is_some())
                {
                    let Some(tsql) = te.sql.as_deref() else {
                        continue;
                    };
                    let tname = te.name.as_deref().unwrap_or("");
                    if !old_col_name.eq_ignore_ascii_case(&new_name.name)
                        && super::trigger::trigger_has_using_join_conflict(
                            tsql,
                            &tbl.name,
                            &old_col_name,
                        )
                    {
                        return Err(SqliteError::Sql(format!(
                            "error in trigger {tname} after rename: cannot join using column {old_col_name} - column not present in both tables"
                        )));
                    }
                    let new_tsql = rewrite_trigger_column_name(
                        tsql,
                        &tbl.name,
                        &old_col_name,
                        &new_name.name,
                        Some(view.schema()),
                    );
                    if new_tsql != tsql {
                        trigger_rewrites.push((te.clone(), new_tsql));
                    }
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
                for (te, new_tsql) in fk_table_rewrites {
                    let tname = te.name.as_deref().unwrap_or("");
                    self.update_schema_row(
                        te.rowid,
                        "table",
                        tname,
                        tname,
                        te.root_page,
                        Some(&new_tsql),
                    )?;
                }
                for (te, new_tsql) in trigger_rewrites {
                    let tname = te.name.as_deref().unwrap_or("");
                    let tbl_ref = te.table_name.as_deref().unwrap_or(tname);
                    self.update_schema_row(
                        te.rowid,
                        "trigger",
                        tname,
                        tbl_ref,
                        te.root_page,
                        Some(&new_tsql),
                    )?;
                }
                self.pager.bump_schema_cookie()?;
                let new_view = self.live_view()?;
                // Re-validate dependent views after column rename (catches indirect view-over-view,
                // CTE, or derived-table column references broken by the rename).
                if !old_col_name.eq_ignore_ascii_case(&new_name.name) {
                    self.validate_all_views_after_alter(&new_view, "rename")?;
                    self.validate_all_triggers_after_alter(&new_view, "rename")?;
                }
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
                for other_tbl in &view.schema().tables {
                    if other_tbl.system {
                        continue;
                    }
                    for fk in &other_tbl.definition.foreign_keys {
                        if fk.table.eq_ignore_ascii_case(&tbl.name) {
                            let refs_col = match &fk.to {
                                Some(cols) => {
                                    cols.iter().any(|c| c.eq_ignore_ascii_case(&old_col_name))
                                }
                                None => tbl.primary_key.contains(&(cid as i32))
                                    || tbl.ipk_column == Some(cid),
                            };
                            if refs_col {
                                return Err(SqliteError::Sql(format!(
                                    "error in table {} after drop column: foreign key references {}.{}",
                                    other_tbl.name, tbl.name, old_col_name
                                )));
                            }
                        }
                    }
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
                self.validate_all_views_after_alter(&new_view, "drop column")?;
                self.validate_all_triggers_after_alter(&new_view, "drop column")?;
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
                        let ki =
                            key_info_for_without_rowid_pk(pk_idx, enc, &self.cached_collations.1)?;
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
            AlterTableAction::DropNotNull { column, .. } => {
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
                if tbl.columns[cid].pk > 0 {
                    return Err(SqliteError::Sql(format!(
                        "cannot drop NOT NULL from PRIMARY KEY column \"{}\"",
                        column.name
                    )));
                }
                let updated_sql = drop_not_null_from_ddl(&old_sql, cid)?;
                drop(view);
                if updated_sql != old_sql {
                    self.update_schema_row(
                        table_entry.rowid,
                        "table",
                        &tbl.name,
                        &tbl.name,
                        table_entry.root_page,
                        Some(&updated_sql),
                    )?;
                    self.pager.bump_schema_cookie()?;
                    let _ = self.live_view()?;
                }
                Ok(self.empty_exec_result())
            }
            AlterTableAction::SetNotNull {
                column,
                on_conflict,
                ..
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
                let mut _rows_mem = ConnMemGuard::new(&self.pager);
                let existing = self.read_all_table_rows(&view, &tbl, &mut _rows_mem)?;
                for (_, row) in &existing {
                    if row.get(cid).is_none_or(SqlValue::is_null) {
                        return Err(SqliteError::Sql(format!(
                            "NOT NULL constraint failed: {}.{}",
                            tbl.name, tbl.columns[cid].name
                        )));
                    }
                }
                let updated_sql = set_not_null_in_ddl(&old_sql, cid, *on_conflict)?;
                drop(view);
                if updated_sql != old_sql {
                    self.update_schema_row(
                        table_entry.rowid,
                        "table",
                        &tbl.name,
                        &tbl.name,
                        table_entry.root_page,
                        Some(&updated_sql),
                    )?;
                    self.pager.bump_schema_cookie()?;
                    let _ = self.live_view()?;
                }
                Ok(self.empty_exec_result())
            }
            AlterTableAction::AddCheck {
                name,
                expr,
                text,
                on_conflict,
                ..
            } => {
                if xdb_sql::expr_contains_raise(expr) {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
                }
                let mut _rows_mem = ConnMemGuard::new(&self.pager);
                let existing = self.read_all_table_rows(&view, &tbl, &mut _rows_mem)?;
                let mut qopts = self.query_options(self.dialect);
                qopts.in_trigger = false;
                let scope_cols = build_table_scope_columns(&tbl, &tbl.name);
                let no_sub = NoSubqueries;
                let empty_params = QueryParams::empty();
                let mut env = EvalEnv::new(&qopts, &empty_params, &no_sub);
                env.dqs = true;
                env.in_trigger = false;
                for (rid, row) in &existing {
                    let mut vals = row.clone();
                    if !tbl.without_rowid {
                        let rv = rid.map_or(SqlValue::Null, SqlValue::Int);
                        while vals.len() < scope_cols.len() {
                            vals.push(rv.clone());
                        }
                    }
                    let scope = [EvalScope::simple(&scope_cols, &vals)];
                    let res = eval_expr(expr, &scope, &env)?;
                    if !res.is_null() && !boolean_value(&res, true) {
                        let cname = name
                            .as_ref()
                            .map(|n| n.name.as_str())
                            .unwrap_or(tbl.name.as_str());
                        return Err(SqliteError::Sql(format!(
                            "CHECK constraint failed: {cname}"
                        )));
                    }
                }
                let oc_suffix = match on_conflict {
                    None => "",
                    Some(ConflictAction::Rollback) => " ON CONFLICT ROLLBACK",
                    Some(ConflictAction::Abort) => " ON CONFLICT ABORT",
                    Some(ConflictAction::Fail) => " ON CONFLICT FAIL",
                    Some(ConflictAction::Ignore) => " ON CONFLICT IGNORE",
                    Some(ConflictAction::Replace) => " ON CONFLICT REPLACE",
                };
                let clause = match name {
                    Some(n) => format!(
                        "CONSTRAINT {} CHECK({text}){oc_suffix}",
                        quote_ident_if_needed(&n.name)
                    ),
                    None => format!("CHECK({text}){oc_suffix}"),
                };
                let updated_sql = append_table_constraint_to_ddl(&old_sql, &clause)?;
                let parsed = parse_statement(&updated_sql, &ParseOptions::default())
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::CreateTable(ct) = parsed {
                    let b_opts = BuildOptions {
                        schema: false,
                        file_format: self.pager.schema_format().clamp(1, 4),
                        ..BuildOptions::default()
                    };
                    let _ = build_table(&ct, &updated_sql, &b_opts)
                        .map_err(|e| SqliteError::Sql(e.to_string()))?;
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
                let _ = self.live_view()?;
                Ok(self.empty_exec_result())
            }
            AlterTableAction::DropConstraint { name, .. } => {
                let updated_sql = drop_named_constraint_from_ddl(&old_sql, &name.name, &tbl)?;
                let parsed = parse_statement(&updated_sql, &ParseOptions::default())
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                let new_auto_names: Vec<String> = if let Statement::CreateTable(ct) = parsed {
                    let b_opts = BuildOptions {
                        schema: false,
                        file_format: self.pager.schema_format().clamp(1, 4),
                        ..BuildOptions::default()
                    };
                    let def = build_table(&ct, &updated_sql, &b_opts)
                        .map_err(|e| SqliteError::Sql(e.to_string()))?;
                    def.indexes
                        .iter()
                        .filter(|ix| ix.auto && !ix.without_rowid_primary_key)
                        .map(|ix| ix.name.clone())
                        .collect()
                } else {
                    Vec::new()
                };
                let old_auto_entries: Vec<_> = view
                    .schema()
                    .entries
                    .iter()
                    .filter(|e| {
                        e.obj_type.as_deref() == Some("index")
                            && e.sql.is_none()
                            && e.table_name
                                .as_deref()
                                .is_some_and(|tn| tn.eq_ignore_ascii_case(&tbl.name))
                    })
                    .cloned()
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
                // Drop any `sqlite_autoindex_*` row whose name is no longer produced by the
                // updated DDL (order-independent, since `build_table` prepends autoindexes),
                // then reindex surviving autoindexes so their keys match the updated constraints.
                let mut dropped_any_auto = false;
                for old_entry in &old_auto_entries {
                    let keep = old_entry.name.as_deref().is_some_and(|en| {
                        new_auto_names.iter().any(|n| en.eq_ignore_ascii_case(n))
                    });
                    if !keep {
                        if old_entry.root_page > 1 {
                            drop_btree(&mut self.pager, old_entry.root_page)?;
                        }
                        table_delete(&mut self.pager, 1, old_entry.rowid)?;
                        dropped_any_auto = true;
                    }
                }
                self.pager.bump_schema_cookie()?;
                let _ = self.live_view()?;
                if dropped_any_auto {
                    // Remove stale `sqlite_stat1` rows for autoindexes on this table.
                    let stale_stat: Option<(u32, Vec<i64>)> = {
                        let lv = self.live_view()?;
                        if let Some(st) = lv.schema().table("sqlite_stat1") {
                            let mut smem = ConnMemGuard::new(&self.pager);
                            let rows = self.read_all_table_rows(&lv, st, &mut smem)?;
                            let rids: Vec<i64> = rows
                                .into_iter()
                                .filter_map(|(rid, r)| {
                                    let rid = rid?;
                                    let same_tbl = r
                                        .first()
                                        .and_then(SqlValue::as_str)
                                        .is_some_and(|t| t.eq_ignore_ascii_case(&tbl.name));
                                    let is_auto_ix = r
                                        .get(1)
                                        .and_then(SqlValue::as_str)
                                        .is_some_and(|ix| {
                                            ix.to_ascii_lowercase().starts_with("sqlite_autoindex_")
                                        });
                                    if same_tbl && is_auto_ix {
                                        Some(rid)
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            Some((st.root_page, rids))
                        } else {
                            None
                        }
                    };
                    if let Some((stat_root, rids)) = stale_stat {
                        for rid in rids {
                            table_delete(&mut self.pager, stat_root, rid)?;
                        }
                    }
                }
                if dropped_any_auto && !new_auto_names.is_empty() {
                    let main_schema = xdb_parser::Name {
                        name: "main".to_string(),
                        quote: xdb_parser::QuoteStyle::None,
                        span: stmt.table.span,
                    };
                    let _ = self.exec_reindex(Some(&main_schema), Some(&stmt.table))?;
                }
                Ok(self.empty_exec_result())
            }
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
        let dml = match self.get_dml_state_by_name(&stmt.table.name.name) {
            Ok(d) => d,
            Err(orig_err) => {
                if let Ok(schema) = self.schema_arc() {
                    if let Some(vinfo) = schema.view(&stmt.table.name.name).cloned() {
                        drop(schema);
                        return self.exec_insert_view_instead_of(stmt, &vinfo, params, dialect);
                    }
                }
                return Err(orig_err);
            }
        };
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

        let own_conflict = if stmt.replace_into {
            Some(ConflictAction::Replace)
        } else {
            stmt.or_conflict
        };
        let stmt_conflict = self.active_conflict_override.or(own_conflict);
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
            && !dml.has_insert_triggers
            && self.trigger_ctx.is_none()
            && !self.foreign_keys
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
                                        self.update_sqlite_sequence(&table.name, rid)?;
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

        // Statically reject `RAISE()` outside triggers (even in single-row fast paths or 0-row paths).
        if self.active_triggers.is_empty() {
            if let Some(ref with) = stmt.with {
                if with
                    .ctes
                    .iter()
                    .any(|c| xdb_sql::select_contains_raise(&c.select))
                {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
                }
            }
            if let Some(ref sel) = stmt.select {
                if xdb_sql::select_contains_raise(sel) {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
                }
            }
            for up in &stmt.upsert {
                if up
                    .target_where
                    .as_ref()
                    .is_some_and(xdb_sql::expr_contains_raise)
                    || up
                        .where_clause
                        .as_ref()
                        .is_some_and(xdb_sql::expr_contains_raise)
                    || up.set.as_ref().is_some_and(|sets| {
                        sets.iter().any(|sc| xdb_sql::expr_contains_raise(&sc.expr))
                    })
                {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
                }
            }
            if let Some(ref ret) = stmt.returning {
                if ret.iter().any(|rc| {
                    matches!(rc, ResultColumn::Expr(re) if xdb_sql::expr_contains_raise(&re.expr))
                }) {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
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
            && !dml.has_insert_triggers
            && self.trigger_ctx.is_none()
            && !(self.foreign_keys && (dml.has_outgoing_fks || dml.has_incoming_fks))
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

                        // Per SQLite: `AUTOINCREMENT` advances `sqlite_sequence` to at least the
                        // candidate rowid even if the row is subsequently skipped via `OR IGNORE`
                        // (NOT NULL / CHECK / STRICT / UNIQUE conflict).
                        let mut pending_autoinc_max: Option<i64> = if table.definition.autoincrement
                        {
                            rowid
                        } else {
                            None
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
                                if let Some(max_rid) = pending_autoinc_max {
                                    self.update_sqlite_sequence(&table.name, max_rid)?;
                                }
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
                            true,
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
                                        pending_autoinc_max = Some(
                                            pending_autoinc_max
                                                .map_or(rid, |prev| prev.max(rid)),
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
                            Ok(RowWriteOutcome::Ignored) => {
                                known_max_rowid = None;
                            }
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

        // Validate UPSERT targets, DO UPDATE expressions, and RETURNING clauses upfront
        // (even when 0 rows or 0 conflicts occur).
        let returning_cols = if !stmt.upsert.is_empty() || stmt.returning.is_some() {
            let val_view = self.live_view()?;
            let val_cat = crate::sql::live_catalog_for(&val_view, &qopts);
            if !stmt.upsert.is_empty() {
                validate_upsert_targets(
                    table,
                    target_alias,
                    &stmt.upsert,
                    self.trigger_ctx.as_deref(),
                    &val_cat,
                    stmt.with.as_ref(),
                    &qopts,
                )?;
            }
            match stmt.returning.as_deref() {
                Some(rc) => {
                    let names = returning_column_names(table, rc)?;
                    validate_returning_clauses(table, rc, &val_cat, stmt.with.as_ref(), &qopts)?;
                    names
                }
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);

        // Evaluate source rows (fast direct VALUES path avoids `live_view()` & `QueryExec`!).
        // Inside triggers, a bare `VALUES (...)` list must be evaluated row-by-row so `RAISE(IGNORE)`
        // or `RAISE(FAIL)` on row `k` preserves already-inserted rows `0..k-1`, matching SQLite.
        let trigger_lazy_values: Option<&xdb_parser::ValuesClause> =
            if self.trigger_ctx.is_some() {
                stmt.select.as_deref().and_then(|sel| {
                    if stmt.with.is_none()
                        && sel.with.is_none()
                        && sel.order_by.is_none()
                        && sel.order_by_all.is_none()
                        && sel.limit.is_none()
                    {
                        if let SelectBody::Values(ref vals) = sel.body {
                            return Some(vals);
                        }
                    }
                    None
                })
            } else {
                None
            };
        if let Some(vals) = trigger_lazy_values {
            for vrow in &vals.rows {
                if vrow.items.len() != target_cols.len() {
                    return Err(SqliteError::Sql(format!(
                        "table {} has {} columns but {} values were supplied",
                        table.name,
                        target_cols.len(),
                        vrow.items.len()
                    )));
                }
            }
        }
        let mut _source_mem = ConnMemGuard::new(&self.pager);
        let source_rows: Vec<Vec<SqlValue>> = if trigger_lazy_values.is_some() {
            Vec::new()
        } else if let Some(ref sel) = stmt.select {
            if self.trigger_ctx.is_none() && direct_values_fast_path(stmt, sel).is_some() {
                let vals = direct_values_fast_path(stmt, sel).unwrap();
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
                    if _source_mem.meter.is_some() {
                        _source_mem.reserve(estimate_sql_row_bytes(&r))?;
                    }
                    rows.push(r);
                }
                rows
            } else {
                let view = self.live_view()?;
                let qres = match self.eval_select_stmt_in_dml(
                    &view,
                    sel,
                    stmt.with.as_ref(),
                    params,
                    &qopts,
                )? {
                    Some(r) => r,
                    None => return Ok(self.empty_exec_result()),
                };
                if qres.columns.len() != target_cols.len() {
                    return Err(SqliteError::Sql(format!(
                        "table {} has {} columns but {} values were supplied",
                        table.name,
                        target_cols.len(),
                        qres.columns.len()
                    )));
                }
                if _source_mem.meter.is_some() {
                    for r in &qres.rows {
                        _source_mem.reserve(estimate_sql_row_bytes(r))?;
                    }
                }
                qres.rows
            }
        } else {
            // `INSERT INTO t DEFAULT VALUES`
            vec![Vec::new()]
        };

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
        let mut returning_mem = ConnMemGuard::new(&self.pager);
        let mut returning_rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut dflt_qopts = qopts.clone();
        dflt_qopts.in_trigger = false;
        let mut targeted_cols = vec![false; table.columns.len()];
        if stmt.select.is_some() {
            for tcol in target_cols {
                if let TargetCol::Col(cid) = *tcol {
                    targeted_cols[cid] = true;
                }
            }
        }
        let mut assigned_cols = vec![false; table.columns.len()];

        let total_rows = trigger_lazy_values
            .map_or(source_rows.len(), |v| v.rows.len());
        let mut source_iter = source_rows.into_iter();
        for row_idx in 0..total_rows {
            let src = if let Some(vals) = trigger_lazy_values {
                let vrow = &vals.rows[row_idx];
                if let Some(ref m) = qopts.meter {
                    m.charge_steps(vrow.items.len() as u64)?;
                }
                let row_needs_subq = vrow.items.iter().any(expr_has_subquery_or_intable);
                if row_needs_subq {
                    if let Some(max_rid) = pending_autoinc_max.take() {
                        self.update_sqlite_sequence(&table.name, max_rid)?;
                    }
                }
                let eval_row_res: Result<Vec<SqlValue>, SqlError> = if row_needs_subq {
                    let view = self.live_view()?;
                    let catalog = crate::sql::live_catalog_for(&view, &qopts);
                    let sub_runner = CatalogSubqueryRunner::with_ctes(
                        &catalog,
                        &qopts,
                        params,
                        stmt.with.as_ref(),
                    )?;
                    let env = EvalEnv::new(&qopts, params, &sub_runner);
                    let t_scope = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scopes: &[EvalScope<'_>] = match t_scope {
                        Some(ref s) => std::slice::from_ref(s),
                        None => &[],
                    };
                    (|| -> Result<Vec<SqlValue>, SqlError> {
                        let mut r = Vec::with_capacity(vrow.items.len());
                        for item in &vrow.items {
                            r.push(eval_expr(item, scopes, &env)?);
                        }
                        Ok(r)
                    })()
                } else {
                    let env = EvalEnv::new(&qopts, params, &NoSubqueries);
                    let t_scope = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scopes: &[EvalScope<'_>] = match t_scope {
                        Some(ref s) => std::slice::from_ref(s),
                        None => &[],
                    };
                    (|| -> Result<Vec<SqlValue>, SqlError> {
                        let mut r = Vec::with_capacity(vrow.items.len());
                        for item in &vrow.items {
                            r.push(eval_expr(item, scopes, &env)?);
                        }
                        Ok(r)
                    })()
                };
                match eval_row_res {
                    Ok(r) => r,
                    Err(e) => {
                        let is_fail = matches!(
                            e,
                            SqlError::Raise {
                                action: RaiseAction::Fail,
                                ..
                            }
                        );
                        match self.handle_trigger_sql_err(e) {
                            Ok(TriggerOutcome::Ignore) => {
                                self.trigger_ignore_signaled = true;
                                break;
                            }
                            Ok(TriggerOutcome::Continue) => break,
                            Err(err) => {
                                if is_fail {
                                    self.changes = inserted_count;
                                    self.total_changes += inserted_count;
                                }
                                return Err(err);
                            }
                        }
                    }
                }
            } else {
                source_iter.next().expect("source row present")
            };
            // Build candidate row with defaults (skip DEFAULT expressions for targeted columns).
            let mut row = Vec::with_capacity(table.columns.len());
            for (cid, col) in table.columns.iter().enumerate() {
                if targeted_cols[cid]
                    || col.generated.is_some()
                    || (Some(cid) == table.ipk_column && !table.without_rowid)
                {
                    row.push(SqlValue::Null);
                } else if let Some(ref dflt_expr) = table.definition.columns[cid].default_expr {
                    if xdb_sql::expr_contains_raise(dflt_expr) {
                        return Err(SqliteError::Sql(
                            "RAISE() may only be used within a trigger-program".to_string(),
                        ));
                    }
                    let v = eval_standalone_expr(dflt_expr, params, &dflt_qopts)?;
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

            // Fire `BEFORE INSERT` triggers (matching SQLite: auto-assigned rowid appears as `-1`
            // in `NEW.rowid` / `NEW.<ipk>`, while explicit rowid appears as its coerced value;
            // final rowid is allocated AFTER `BEFORE INSERT` so trigger inserts never collide!).
            if dml.has_insert_triggers {
                if let Some(max_rid) = pending_autoinc_max.take() {
                    self.update_sqlite_sequence(&table.name, max_rid)?;
                }
                let mut pre_row = row.clone();
                let before_rid: Option<i64> = if table.without_rowid {
                    None
                } else if let Some(ipk) = table.ipk_column {
                    let cand = ipk_candidate.as_ref().unwrap_or(&pre_row[ipk]);
                    if cand.is_null() {
                        pre_row[ipk] = SqlValue::Int(-1);
                        Some(-1)
                    } else {
                        let r = coerce_ipk_value(cand)?;
                        pre_row[ipk] = SqlValue::Int(r);
                        Some(r)
                    }
                } else if let Some(ref er) = ipk_candidate {
                    if er.is_null() {
                        Some(-1)
                    } else {
                        Some(coerce_ipk_value(er)?)
                    }
                } else {
                    Some(-1)
                };
                self.apply_affinities_and_generated(table, &dml, &mut pre_row, &qopts)?;
                let tctx = Arc::new(TriggerRowCtx::for_table(
                    table,
                    None,
                    Some((before_rid, &pre_row)),
                ));
                let outcome = match self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::Before,
                    TriggerEventKind::Insert,
                    None,
                    &tctx,
                    stmt_conflict,
                    dialect,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        if self.pending_error_conflict == Some(ConflictAction::Fail) {
                            self.changes = inserted_count;
                            self.total_changes += inserted_count;
                        }
                        return Err(e);
                    }
                };
                // A `BEFORE INSERT` trigger may have inserted/deleted rows in the same table
                // even when it subsequently fired `RAISE(IGNORE)`!
                known_max_rowid = None;
                if table.definition.autoincrement {
                    autoinc_seq_cache = Some(
                        self.get_sqlite_sequence(&table.name)?
                            .map_or(0, |(_, s)| s),
                    );
                }
                if outcome == TriggerOutcome::Ignore {
                    continue;
                }
            }

            // Determine rowid for rowid tables (and whether it is guaranteed strictly fresh).
            let mut rowid_guaranteed_new = false;
            let mut auto_allocated_rowid: Option<i64> = None;
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
                    auto_allocated_rowid = Some(next);
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
                    auto_allocated_rowid = Some(next);
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
                auto_allocated_rowid = Some(next);
                Some(next)
            };

            let candidate_rowid = rowid;

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
                Ok(false) => {
                    if table.definition.autoincrement {
                        if let Some(rid) = candidate_rowid {
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if rid > *cached {
                                    *cached = rid;
                                }
                            }
                            pending_autoinc_max =
                                Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
                        }
                    }
                    continue;
                }
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
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
                stmt.returning.is_some() || dml.has_insert_triggers || dml.has_update_triggers,
                true,
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    was_insert,
                    deleted_conflict,
                    upsert_after,
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
                    }
                    if table.definition.autoincrement {
                        // Per SQLite: `AUTOINCREMENT` records the inserted rowid (or the candidate
                        // rowid determined prior to an UPSERT conflict, whether explicit or
                        // auto-allocated), never a new id assigned by `UPSERT DO UPDATE SET id = ...`.
                        let seq_rid = if was_insert {
                            final_rowid
                        } else {
                            candidate_rowid.or(auto_allocated_rowid)
                        };
                        if let Some(rid) = seq_rid {
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if rid > *cached {
                                    *cached = rid;
                                }
                            }
                            pending_autoinc_max =
                                Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
                        }
                    }
                    // Per SQLite: `RETURNING` runs immediately after the row write and BEFORE `AFTER` triggers.
                    if let Some(ref ret) = stmt.returning {
                        if returning_needs_subq {
                            if let Some(max_rid) = pending_autoinc_max.take() {
                                self.update_sqlite_sequence(&table.name, max_rid)?;
                            }
                        }
                        let ret_row = self.eval_returning(
                            table,
                            final_rowid,
                            &final_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?;
                        if returning_mem.meter.is_some() {
                            returning_mem.reserve(estimate_sql_row_bytes(&ret_row))?;
                        }
                        returning_rows.push(ret_row);
                    }
                    if was_insert && dml.has_insert_triggers {
                        if let Some(max_rid) = pending_autoinc_max.take() {
                            self.update_sqlite_sequence(&table.name, max_rid)?;
                        }
                        let tctx = Arc::new(TriggerRowCtx::for_table(
                            table,
                            None,
                            Some((final_rowid, &final_row)),
                        ));
                        if let Err(e) = self.fire_triggers(
                            &dml.triggers,
                            TriggerTime::After,
                            TriggerEventKind::Insert,
                            None,
                            &tctx,
                            stmt_conflict,
                            dialect,
                        ) {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = inserted_count;
                                self.total_changes += inserted_count;
                            }
                            return Err(e);
                        }
                        known_max_rowid = None;
                        if table.definition.autoincrement {
                            autoinc_seq_cache = Some(
                                self.get_sqlite_sequence(&table.name)?
                                    .map_or(0, |(_, s)| s),
                            );
                        }
                    } else if let Some((old_rid, orig_old_row, upsert_upd_cols)) = upsert_after {
                        if let Some(max_rid) = pending_autoinc_max.take() {
                            self.update_sqlite_sequence(&table.name, max_rid)?;
                        }
                        let tctx = Arc::new(TriggerRowCtx::for_table(
                            table,
                            Some((old_rid, &orig_old_row)),
                            Some((final_rowid, &final_row)),
                        ));
                        // Per SQLite: UPSERT `DO UPDATE` (including its `AFTER UPDATE` triggers)
                        // always runs under mandatory `ON CONFLICT ABORT` semantics.
                        let prev_conflict_ov = self.active_conflict_override;
                        self.active_conflict_override = Some(ConflictAction::Abort);
                        let after_res = self.fire_triggers(
                            &dml.triggers,
                            TriggerTime::After,
                            TriggerEventKind::Update,
                            Some(&upsert_upd_cols),
                            &tctx,
                            Some(ConflictAction::Abort),
                            dialect,
                        );
                        self.active_conflict_override = prev_conflict_ov;
                        if let Err(e) = after_res {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = inserted_count;
                                self.total_changes += inserted_count;
                            }
                            return Err(e);
                        }
                        known_max_rowid = None;
                        if table.definition.autoincrement {
                            autoinc_seq_cache = Some(
                                self.get_sqlite_sequence(&table.name)?
                                    .map_or(0, |(_, s)| s),
                            );
                        }
                    }
                    // If an OR REPLACE (statement- or schema-level) or UPSERT DO UPDATE modified/deleted rows, reset max cache.
                    if !was_insert || deleted_conflict || stmt_conflict == Some(ConflictAction::Replace) {
                        known_max_rowid = None;
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {
                    if table.definition.autoincrement {
                        if let Some(rid) = candidate_rowid.or(auto_allocated_rowid) {
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if rid > *cached {
                                    *cached = rid;
                                }
                            }
                            pending_autoinc_max =
                                Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
                        }
                    }
                    // A `BEFORE UPDATE` trigger inside UPSERT `DO UPDATE` (or delete trigger) may
                    // have mutated the table before returning `RAISE(IGNORE)` or deleting the row.
                    if dml.has_update_triggers || dml.has_delete_triggers || !stmt.upsert.is_empty() {
                        known_max_rowid = None;
                        if table.definition.autoincrement {
                            if let Some(max_rid) = pending_autoinc_max.take() {
                                self.update_sqlite_sequence(&table.name, max_rid)?;
                            }
                            autoinc_seq_cache = Some(
                                self.get_sqlite_sequence(&table.name)?
                                    .map_or(0, |(_, s)| s),
                            );
                        }
                    }
                    if self.trigger_ignore_signaled {
                        break;
                    }
                }
                Err(e) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
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
            || dml.has_insert_triggers
            || (dml.has_delete_triggers && self.recursive_triggers)
            || self.foreign_keys
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
            let mut autoinc_seq_cache: Option<i64> = if table.definition.autoincrement {
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
                            let m = (!conn.pager.is_noop_meter())
                                .then(|| Arc::clone(conn.pager.meter()));
                            for (tcol, (_, jv)) in target_cols.iter().zip(fields.into_iter()) {
                                let val = crate::write::seed::seed_json_to_sql_value_owned_metered(
                                    jv,
                                    m.as_deref(),
                                )?;
                                match *tcol {
                                    SeedTarget::Col(cid) => {
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
                                    SeedTarget::Rowid => {
                                        ipk_candidate = Some(val);
                                    }
                                }
                            }
                        }
                        JsonVal::Array(cells) => {
                            let m = (!conn.pager.is_noop_meter())
                                .then(|| Arc::clone(conn.pager.meter()));
                            for (tcol, jv) in target_cols.iter().zip(cells.into_iter()) {
                                let val = crate::write::seed::seed_json_to_sql_value_owned_metered(
                                    jv,
                                    m.as_deref(),
                                )?;
                                match *tcol {
                                    SeedTarget::Col(cid) => {
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
                                    SeedTarget::Rowid => {
                                        ipk_candidate = Some(val);
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
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if rid > *cached {
                                    *cached = rid;
                                }
                            }
                            pending_autoinc_max =
                                Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
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
                        if table.definition.autoincrement {
                            if let Some(ref mut cached) = autoinc_seq_cache {
                                if rid > *cached {
                                    *cached = rid;
                                }
                            }
                            pending_autoinc_max =
                                Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
                        }
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
                        true,
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
                                    if let Some(ref mut cached) = autoinc_seq_cache {
                                        if r > *cached {
                                            *cached = r;
                                        }
                                    }
                                    pending_autoinc_max = Some(
                                        pending_autoinc_max.map_or(r, |prev| prev.max(r)),
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
                        RowWriteOutcome::Ignored => {
                            if table.definition.autoincrement {
                                if let Some(ref mut cached) = autoinc_seq_cache {
                                    if rid > *cached {
                                        *cached = rid;
                                    }
                                }
                                pending_autoinc_max =
                                    Some(pending_autoinc_max.map_or(rid, |prev| prev.max(rid)));
                            }
                            known_max_rowid = None;
                        }
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
        let dml = match self.get_dml_state_by_name(&stmt.table.name.name) {
            Ok(d) => d,
            Err(orig_err) => {
                if let Ok(schema) = self.schema_arc() {
                    if let Some(vinfo) = schema.view(&stmt.table.name.name).cloned() {
                        drop(schema);
                        return self.exec_update_view_instead_of(stmt, &vinfo, params, dialect);
                    }
                }
                return Err(orig_err);
            }
        };
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
        let target_alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();
        validate_dml_indexed_by(
            table,
            stmt.table.indexed_by.as_ref(),
            stmt.where_clause.as_ref(),
            target_alias,
        )?;
        validate_set_targets(table, &stmt.set)?;
        let stmt_conflict = self.active_conflict_override.or(stmt.or_conflict);

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
            && !dml.has_update_triggers
            && self.trigger_ctx.is_none()
            && !self.foreign_keys
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

        if self.active_triggers.is_empty()
            && (stmt
                .from
                .as_ref()
                .is_some_and(xdb_sql::from_contains_raise)
                || stmt
                    .where_clause
                    .as_ref()
                    .is_some_and(xdb_sql::expr_contains_raise)
                || stmt
                    .set
                    .iter()
                    .any(|sc| xdb_sql::expr_contains_raise(&sc.expr))
                || stmt.with.as_ref().is_some_and(|w| {
                    w.ctes
                        .iter()
                        .any(|c| xdb_sql::select_contains_raise(&c.select))
                })
                || stmt.returning.as_ref().is_some_and(|ret| {
                    ret.iter().any(|rc| match rc {
                        ResultColumn::Expr(re) => xdb_sql::expr_contains_raise(&re.expr),
                        ResultColumn::Star(rs) => rs.replace.as_deref().is_some_and(|reps| {
                            reps.iter().any(|r| xdb_sql::expr_contains_raise(&r.expr))
                        }),
                    })
                }))
        {
            return Err(SqliteError::Sql(
                "RAISE() may only be used within a trigger-program".to_string(),
            ));
        }
        let qopts = self.query_options(dialect);
        let returning_cols = match stmt.returning.as_deref() {
            Some(rc) => {
                let names = returning_column_names(table, rc)?;
                let val_view = self.live_view()?;
                let val_cat = crate::sql::live_catalog_for(&val_view, &qopts);
                validate_returning_clauses(table, rc, &val_cat, stmt.with.as_ref(), &qopts)?;
                names
            }
            None => Vec::new(),
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);

        type PendingUpdate = (Option<i64>, Vec<SqlValue>, Option<i64>, Vec<SqlValue>);
        let mut updates_mem = ConnMemGuard::new(&self.pager);
        let mut updates: Vec<PendingUpdate> = Vec::new();
        let mut pending_loop_sql_err: Option<SqlError> = None;

        // Direct point-rowid fast path (`UPDATE t SET ... WHERE id = ?` with no FROM/WITH/subqueries):
        // bypasses `live_view()`, `SqliteDatabase`, `Pager`, and `CatalogSubqueryRunner` completely!
        let can_direct_point = stmt.from.is_none()
            && stmt.with.is_none()
            && !table.without_rowid
            && self.trigger_ctx.is_none()
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
                    } else {
                        for sc in &stmt.set {
                            validate_single_table_expr_columns(
                                &sc.expr,
                                table,
                                target_alias,
                                stmt.table.alias.is_some(),
                                self.trigger_ctx.as_deref(),
                            )?;
                        }
                    }
                }
            } else {
                // Validate column references upfront even when the direct point-rowid lookup misses.
                if !exact_where_only {
                    if let Some(ref we) = stmt.where_clause {
                        validate_single_table_expr_columns(
                            we,
                            table,
                            target_alias,
                            stmt.table.alias.is_some(),
                            self.trigger_ctx.as_deref(),
                        )?;
                    }
                }
                for sc in &stmt.set {
                    validate_single_table_expr_columns(
                        &sc.expr,
                        table,
                        target_alias,
                        stmt.table.alias.is_some(),
                        self.trigger_ctx.as_deref(),
                    )?;
                }
            }
        } else {
            let view = self.live_view()?;
            // Read candidate rows matching WHERE (using fast point/range/index seek when no FROM!).
            let mut _existing_mem = ConnMemGuard::new(&self.pager);
            let existing_rows = if stmt.from.is_none() && self.trigger_ctx.is_none() {
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
                // Expose rowid/_rowid_/oid for single rowid-table FROM sources too (unless shadowed by a CTE).
                let single_from_rowid_alias: Option<(String, Vec<&'static str>)> =
                    if let xdb_parser::FromSource::Table(ft) = from_src {
                        let shadowed_by_cte = ft.schema.is_none()
                            && stmt.with.as_ref().is_some_and(|w| {
                                w.ctes
                                    .iter()
                                    .any(|c| c.name.name.eq_ignore_ascii_case(&ft.name.name))
                            });
                        if !shadowed_by_cte
                            && (ft.schema.is_none()
                                || ft
                                    .schema
                                    .as_ref()
                                    .is_some_and(|s| s.name.eq_ignore_ascii_case("main")))
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
                let Some((mut ecols, mut erows)) =
                    self.eval_select_stmt_detailed_in_dml(&view, &from_select, params, &qopts)?
                else {
                    return Ok(self.empty_exec_result());
                };
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

                let mut loop_sql_err: Option<SqlError> = None;
                'outer_from: for (old_rowid, old_row) in existing_rows {
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
                                    let t_scope_opt =
                                        self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                                    let scope_storage: [EvalScope<'_>; 2];
                                    let scope_single: [EvalScope<'_>; 1];
                                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                                        scope_storage =
                                            [ts, EvalScope::simple(&combined_cols, &combined_vals)];
                                        &scope_storage
                                    } else {
                                        scope_single =
                                            [EvalScope::simple(&combined_cols, &combined_vals)];
                                        &scope_single
                                    };
                                    if let Some(ref we) = stmt.where_clause {
                                        let cond = match eval_expr(we, scope, &eval_env) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                loop_sql_err = Some(e);
                                                break 'outer_from;
                                            }
                                        };
                                        if !boolean_value(&cond, false) {
                                            continue;
                                        }
                                    }
                                    let (new_rid, new_row) = match apply_set_clauses_sql_err(
                                        table,
                                        old_rowid,
                                        &old_row,
                                        &stmt.set,
                                        scope,
                                        &eval_env,
                                    ) {
                                        Ok(pair) => pair,
                                        Err(e) => {
                                            loop_sql_err = Some(e);
                                            break 'outer_from;
                                        }
                                    };
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
                            let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                            let scope_storage: [EvalScope<'_>; 2];
                            let scope_single: [EvalScope<'_>; 1];
                            let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                                scope_storage = [ts, EvalScope::simple(&combined_cols, &combined_vals)];
                                &scope_storage
                            } else {
                                scope_single = [EvalScope::simple(&combined_cols, &combined_vals)];
                                &scope_single
                            };
                            if let Some(ref we) = stmt.where_clause {
                                let cond = match eval_expr(we, scope, &eval_env) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        loop_sql_err = Some(e);
                                        break 'outer_from;
                                    }
                                };
                                if !boolean_value(&cond, false) {
                                    continue;
                                }
                            }
                            let (new_rid, new_row) = match apply_set_clauses_sql_err(
                                table,
                                old_rowid,
                                &old_row,
                                &stmt.set,
                                scope,
                                &eval_env,
                            ) {
                                Ok(pair) => pair,
                                Err(e) => {
                                    loop_sql_err = Some(e);
                                    break 'outer_from;
                                }
                            };
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
                if updates.is_empty() && loop_sql_err.is_none() {
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&combined_cols, &[])];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&combined_cols, &[])];
                        &scope_single
                    };
                    if let Some(ref we) = stmt.where_clause {
                        validate_dml_expr_with_catalog(
                            we,
                            scope,
                            &catalog,
                            stmt.with.as_ref(),
                            &qopts,
                        )?;
                    }
                    for sc in &stmt.set {
                        validate_dml_expr_with_catalog(
                            &sc.expr,
                            scope,
                            &catalog,
                            stmt.with.as_ref(),
                            &qopts,
                        )?;
                    }
                }
                drop(eval_env);
                drop(sub_runner);
                drop(view);
                if let Some(e) = loop_sql_err {
                    if matches!(
                        e,
                        SqlError::Raise {
                            action: RaiseAction::Ignore | RaiseAction::Fail,
                            ..
                        }
                    ) {
                        pending_loop_sql_err = Some(e);
                    } else {
                        let _ = self.handle_trigger_sql_err(e)?;
                    }
                }
            } else {
                let mut loop_sql_err: Option<SqlError> = None;
                let had_existing = !existing_rows.is_empty();
                for (old_rowid, old_row) in existing_rows {
                    let mut full_vals = old_row.clone();
                    if !table.without_rowid {
                        let rid_v = old_rowid.map_or(SqlValue::Null, SqlValue::Int);
                        while full_vals.len() < base_scope_cols.len() {
                            full_vals.push(rid_v.clone());
                        }
                    }
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&base_scope_cols, &full_vals)];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&base_scope_cols, &full_vals)];
                        &scope_single
                    };
                    if let Some(ref we) = stmt.where_clause {
                        let cond = match eval_expr(we, scope, &eval_env) {
                            Ok(v) => v,
                            Err(e) => {
                                loop_sql_err = Some(e);
                                break;
                            }
                        };
                        if !boolean_value(&cond, false) {
                            continue;
                        }
                    }
                    let (new_rid, new_row) = match apply_set_clauses_sql_err(
                        table,
                        old_rowid,
                        &old_row,
                        &stmt.set,
                        scope,
                        &eval_env,
                    ) {
                        Ok(pair) => pair,
                        Err(e) => {
                            loop_sql_err = Some(e);
                            break;
                        }
                    };
                    updates_mem.reserve(
                        estimate_sql_row_bytes(&old_row)
                            .saturating_add(estimate_sql_row_bytes(&new_row))
                            .saturating_add(32),
                    )?;
                    updates.push((old_rowid, old_row, new_rid, new_row));
                }
                if updates.is_empty() && loop_sql_err.is_none() {
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&base_scope_cols, &[])];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&base_scope_cols, &[])];
                        &scope_single
                    };
                    if !had_existing {
                        if let Some(ref we) = stmt.where_clause {
                            validate_dml_expr_with_catalog(
                                we,
                                scope,
                                &catalog,
                                stmt.with.as_ref(),
                                &qopts,
                            )?;
                        }
                    }
                    for sc in &stmt.set {
                        validate_dml_expr_with_catalog(
                            &sc.expr,
                            scope,
                            &catalog,
                            stmt.with.as_ref(),
                            &qopts,
                        )?;
                    }
                }
                drop(eval_env);
                drop(sub_runner);
                drop(view);
                if let Some(e) = loop_sql_err {
                    if matches!(
                        e,
                        SqlError::Raise {
                            action: RaiseAction::Ignore | RaiseAction::Fail,
                            ..
                        }
                    ) {
                        pending_loop_sql_err = Some(e);
                    } else {
                        let _ = self.handle_trigger_sql_err(e)?;
                    }
                }
            }
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
        let fk_outgoing = self.foreign_keys && dml.has_outgoing_fks;
        let fk_incoming = self.foreign_keys && dml.has_incoming_fks;
        let has_replace_conflict =
            stmt_conflict == Some(ConflictAction::Replace) || table_has_replace_conflict(table);
        let needs_row_savepoint = matches!(
            stmt_conflict,
            Some(ConflictAction::Ignore | ConflictAction::Fail | ConflictAction::Replace)
        ) || table_has_ignore_or_fail_conflict(table)
            || table_has_replace_conflict(table)
            || dml.has_delete_triggers
            || dml.has_update_triggers
            || fk_outgoing
            || fk_incoming;

        let mut updated_count = 0i64;
        let mut returning_mem = ConnMemGuard::new(&self.pager);
        let mut returning_rows = Vec::new();
        let mut replaced = ReplacedTracker::default();
        let need_live_refresh = dml.has_update_triggers
            || (self.recursive_triggers && dml.has_delete_triggers)
            || fk_incoming
            || fk_outgoing
            || (has_replace_conflict
                && updates
                    .iter()
                    .any(|(old_rid, _, new_rid, _)| table.without_rowid || old_rid != new_rid));
        let set_has_subquery = need_live_refresh
            && stmt.from.is_none()
            && stmt
                .set
                .iter()
                .any(|sc| expr_has_subquery_or_intable(&sc.expr));
        let enc = self.pager.encoding();
        let fmt = self.pager.schema_format();

        let mut set_col_mask = vec![false; table.columns.len()];
        let updated_col_names: Vec<String> = if dml.has_update_triggers || need_live_refresh {
            let mut names = Vec::new();
            for sc in &stmt.set {
                for c in &sc.columns {
                    if let Some(pos) = table
                        .columns
                        .iter()
                        .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                    {
                        set_col_mask[pos] = true;
                        names.push(table.columns[pos].name.clone());
                    } else if !table.without_rowid
                        && matches!(
                            c.name.to_ascii_lowercase().as_str(),
                            "rowid" | "_rowid_" | "oid"
                        )
                    {
                        if let Some(ipk) = table.ipk_column {
                            set_col_mask[ipk] = true;
                        }
                        // Per SQLite: `UPDATE t SET rowid = ...` fires `UPDATE OF rowid`
                        // triggers, not `UPDATE OF <ipk_col>` triggers.
                        names.push(c.name.clone());
                    } else {
                        names.push(c.name.clone());
                    }
                }
            }
            names
        } else {
            Vec::new()
        };

        let live_reval_scope_cols = if need_live_refresh && stmt.from.is_none() {
            build_table_scope_columns(table, target_alias)
        } else {
            Vec::new()
        };

        for (old_rowid, mut old_row, mut new_rowid, mut new_row) in updates {
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

            // Re-fetch live row when triggers, FK cascades, or `OR REPLACE` rowid moves may have
            // mutated/deleted it during earlier iterations (fixes self-referencing cascades,
            // `OR REPLACE` delete triggers, and multi-row `SET` re-evaluation).
            if need_live_refresh {
                match self.reload_live_row(table, &dml, old_rowid, &old_row, &qopts)? {
                    None => continue,
                    Some(live_row) => {
                        let row_changed = live_row.len() != old_row.len()
                            || live_row
                                .iter()
                                .zip(&old_row)
                                .any(|(a, b)| !a.exact_eq(b));
                        if stmt.from.is_none() && (row_changed || set_has_subquery) {
                            let view = self.live_view()?;
                            let catalog = crate::sql::live_catalog_for(&view, &qopts);
                            let sub_runner = CatalogSubqueryRunner::with_ctes(
                                &catalog,
                                &qopts,
                                params,
                                stmt.with.as_ref(),
                            )?;
                            let eval_env = EvalEnv::new(&qopts, params, &sub_runner);
                            let mut full_vals = live_row.clone();
                            if !table.without_rowid {
                                let rid_v = old_rowid.map_or(SqlValue::Null, SqlValue::Int);
                                while full_vals.len() < live_reval_scope_cols.len() {
                                    full_vals.push(rid_v.clone());
                                }
                            }
                            let t_scope_opt =
                                self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                            let scope_storage: [EvalScope<'_>; 2];
                            let scope_single: [EvalScope<'_>; 1];
                            let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                                scope_storage =
                                    [ts, EvalScope::simple(&live_reval_scope_cols, &full_vals)];
                                &scope_storage
                            } else {
                                scope_single =
                                    [EvalScope::simple(&live_reval_scope_cols, &full_vals)];
                                &scope_single
                            };
                            let reval = apply_set_clauses_sql_err(
                                table,
                                old_rowid,
                                &live_row,
                                &stmt.set,
                                scope,
                                &eval_env,
                            );
                            drop(eval_env);
                            drop(sub_runner);
                            drop(view);
                            match reval {
                                Ok((nr, nrow)) => {
                                    if !table.without_rowid && nr != old_rowid {
                                        self.invalidate_max_rowid_for(table.root_page);
                                    }
                                    new_rowid = nr;
                                    new_row = nrow;
                                }
                                Err(e) => match self.handle_trigger_sql_err(e) {
                                    Ok(TriggerOutcome::Ignore) => {
                                        self.trigger_ignore_signaled = true;
                                        break;
                                    }
                                    Ok(TriggerOutcome::Continue) => continue,
                                    Err(err) => {
                                        if self.pending_error_conflict == Some(ConflictAction::Fail)
                                        {
                                            self.changes = updated_count;
                                            self.total_changes += updated_count;
                                        }
                                        return Err(err);
                                    }
                                },
                            }
                        } else {
                            for cid in 0..table.columns.len() {
                                if !set_col_mask[cid] && table.columns[cid].generated.is_none() {
                                    new_row[cid] = live_row[cid].clone();
                                }
                            }
                        }
                        old_row = live_row;
                    }
                }
            }

            // Preserve original pre-`BEFORE`-trigger row for `AFTER UPDATE` `OLD.*`!
            let orig_old_row = old_row.clone();

            if dml.has_update_triggers {
                let mut pre_new = new_row.clone();
                if !table.without_rowid {
                    if let (Some(ipk), Some(rid)) = (table.ipk_column, new_rowid) {
                        pre_new[ipk] = SqlValue::Int(rid);
                    }
                }
                self.apply_affinities_and_generated(table, &dml, &mut pre_new, &qopts)?;
                let tctx = Arc::new(TriggerRowCtx::for_table(
                    table,
                    Some((old_rowid, &orig_old_row)),
                    Some((new_rowid, &pre_new)),
                ));
                let outcome = match self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::Before,
                    TriggerEventKind::Update,
                    Some(&updated_col_names),
                    &tctx,
                    stmt_conflict,
                    dialect,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        if self.pending_error_conflict == Some(ConflictAction::Fail) {
                            self.changes = updated_count;
                            self.total_changes += updated_count;
                        }
                        return Err(e);
                    }
                };
                if outcome == TriggerOutcome::Ignore {
                    continue;
                }
                // Re-fetch live row after `BEFORE UPDATE` so deletions skip and modifications
                // to untouched columns (or indexed columns) stay consistent with B-tree indexes!
                match self.reload_live_row(table, &dml, old_rowid, &old_row, &qopts)? {
                    None => continue,
                    Some(live_row) => {
                        for cid in 0..table.columns.len() {
                            if !set_col_mask[cid] && table.columns[cid].generated.is_none() {
                                new_row[cid] = live_row[cid].clone();
                            }
                        }
                        old_row = live_row;
                    }
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
                    if fk_outgoing {
                        self.fk_check_child_row(table, &old_row, Some(&set_col_mask), -1, &qopts)?;
                    }
                    encode_table_record_into(table, &new_row, enc, fmt, &mut self.record_scratch);
                    table_insert(&mut self.pager, table.root_page, rid, &self.record_scratch, true)?;
                    if fk_outgoing {
                        self.fk_check_child_row(table, &new_row, Some(&set_col_mask), 1, &qopts)?;
                    }
                    updated_count += 1;
                    if fk_incoming {
                        if let Err(e) = self.apply_incoming_fk_update_actions(
                            table,
                            &old_row,
                            &new_row,
                            new_rowid,
                            dialect,
                        ) {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = updated_count;
                                self.total_changes += updated_count;
                            }
                            return Err(e);
                        }
                    }
                    // Per SQLite: `RETURNING` runs before `AFTER` triggers.
                    if let Some(ref ret) = stmt.returning {
                        let ret_row = self.eval_returning(
                            table,
                            new_rowid,
                            &new_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?;
                        if returning_mem.meter.is_some() {
                            returning_mem.reserve(estimate_sql_row_bytes(&ret_row))?;
                        }
                        returning_rows.push(ret_row);
                    }
                    if dml.has_update_triggers {
                        let tctx = Arc::new(TriggerRowCtx::for_table(
                            table,
                            Some((old_rowid, &orig_old_row)),
                            Some((new_rowid, &new_row)),
                        ));
                        if let Err(e) = self.fire_triggers(
                            &dml.triggers,
                            TriggerTime::After,
                            TriggerEventKind::Update,
                            Some(&updated_col_names),
                            &tctx,
                            stmt_conflict,
                            dialect,
                        ) {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = updated_count;
                                self.total_changes += updated_count;
                            }
                            return Err(e);
                        }
                    }
                    continue;
                }
            }

            // Per SQLite: when `UPDATE` with `REPLACE` conflict resolution has active recursive
            // `DELETE` triggers, resolve and delete non-self conflicting rows BEFORE deleting the
            // updated row's old entry and BEFORE pushing the mini row-savepoint. That way a
            // `RAISE(FAIL)` inside `BEFORE DELETE` or `AFTER DELETE` preserves both the target
            // row's original entry and any trigger writes / already-deleted conflict rows.
            let mut row_replaced = ReplacedTracker::default();
            let mut pre_deleted_conflict = false;
            let mut old_row_still_alive = true;
            if has_replace_conflict && self.recursive_triggers && dml.has_delete_triggers {
                match self.pre_delete_update_replace_conflicts(
                    table,
                    &dml,
                    wr_pk_idx,
                    old_rowid,
                    &old_row,
                    new_rowid,
                    &new_row,
                    stmt_conflict,
                    &qopts,
                    &mut row_replaced,
                    &mut pre_deleted_conflict,
                ) {
                    Ok(true) => {
                        match self.reload_live_row(table, &dml, old_rowid, &old_row, &qopts)? {
                            Some(live_old) => old_row = live_old,
                            None => old_row_still_alive = false,
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        if self.pending_error_conflict == Some(ConflictAction::Fail) {
                            self.changes = updated_count;
                            self.total_changes += updated_count;
                        }
                        return Err(e);
                    }
                }
            }

            let saved_fk = self.fk_snapshot();
            if needs_row_savepoint {
                self.pager.push_savepoint(None);
            }

            if old_row_still_alive {
                if fk_outgoing {
                    if let Err(e) =
                        self.fk_check_child_row(table, &old_row, Some(&set_col_mask), -1, &qopts)
                    {
                        if needs_row_savepoint {
                            self.pager.rollback_statement_savepoint();
                        }
                        self.fk_restore(saved_fk);
                        return Err(e);
                    }
                }

                if let Err(e) =
                    self.delete_row_and_indexes(table, &dml, old_rowid, &old_row, &qopts)
                {
                    if needs_row_savepoint {
                        self.pager.rollback_statement_savepoint();
                    }
                    self.fk_restore(saved_fk);
                    return Err(e);
                }
            }

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
                stmt.returning.is_some()
                    || dml.has_update_triggers
                    || fk_outgoing
                    || fk_incoming
                    || (table.without_rowid && has_replace_conflict),
                false,
            ) {
                Ok(RowWriteOutcome::Written {
                    final_rowid,
                    final_row,
                    deleted_conflict,
                    ..
                }) => {
                    if fk_outgoing {
                        if let Err(e) = self.fk_check_child_row(
                            table,
                            &final_row,
                            Some(&set_col_mask),
                            1,
                            &qopts,
                        ) {
                            if needs_row_savepoint {
                                self.pager.rollback_statement_savepoint();
                            }
                            self.fk_restore(saved_fk);
                            return Err(e);
                        }
                    }
                    if needs_row_savepoint {
                        self.pager.commit_statement_savepoint();
                    }
                    if (deleted_conflict || pre_deleted_conflict) && !table.without_rowid {
                        self.invalidate_max_rowid_for(table.root_page);
                    }
                    // If the updated row moved directly onto a rowid/PK it replaced (e.g.
                    // `UPDATE OR REPLACE t SET id = id + 1`), that slot now holds the moved
                    // row rather than being deleted, so SQLite's scan visits it next!
                    if !table.without_rowid {
                        if let Some(frid) = final_rowid {
                            row_replaced.rowids.remove(&frid);
                            replaced.rowids.remove(&frid);
                        }
                    } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                        if !final_row.is_empty() {
                            let final_pk: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                                .iter()
                                .map(|ic| final_row[ic.cid as usize].clone())
                                .collect();
                            row_replaced
                                .wr_pks
                                .retain(|p| !key_tuples_equal(p, &final_pk, pk_ki));
                            replaced
                                .wr_pks
                                .retain(|p| !key_tuples_equal(p, &final_pk, pk_ki));
                        }
                    }
                    replaced.extend(row_replaced);
                    updated_count += 1;
                    if fk_incoming {
                        if let Err(e) = self.apply_incoming_fk_update_actions(
                            table,
                            &old_row,
                            &final_row,
                            final_rowid,
                            dialect,
                        ) {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = updated_count;
                                self.total_changes += updated_count;
                            }
                            return Err(e);
                        }
                    }
                    if let Some(ref ret) = stmt.returning {
                        let ret_row = self.eval_returning(
                            table,
                            final_rowid,
                            &final_row,
                            ret,
                            stmt.with.as_ref(),
                            params,
                            dialect,
                            returning_needs_subq,
                        )?;
                        if returning_mem.meter.is_some() {
                            returning_mem.reserve(estimate_sql_row_bytes(&ret_row))?;
                        }
                        returning_rows.push(ret_row);
                    }
                    if dml.has_update_triggers {
                        let tctx = Arc::new(TriggerRowCtx::for_table(
                            table,
                            Some((old_rowid, &orig_old_row)),
                            Some((final_rowid, &final_row)),
                        ));
                        if let Err(e) = self.fire_triggers(
                            &dml.triggers,
                            TriggerTime::After,
                            TriggerEventKind::Update,
                            Some(&updated_col_names),
                            &tctx,
                            stmt_conflict,
                            dialect,
                        ) {
                            if self.pending_error_conflict == Some(ConflictAction::Fail) {
                                self.changes = updated_count;
                                self.total_changes += updated_count;
                            }
                            return Err(e);
                        }
                    }
                }
                Ok(RowWriteOutcome::Ignored) => {
                    if needs_row_savepoint {
                        self.pager.rollback_statement_savepoint();
                    }
                    self.fk_restore(saved_fk);
                }
                Err(e) => {
                    if needs_row_savepoint {
                        self.pager.rollback_statement_savepoint();
                    }
                    self.fk_restore(saved_fk);
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(e);
                }
            }
        }

        if let Some(e) = pending_loop_sql_err {
            match self.handle_trigger_sql_err(e) {
                Ok(TriggerOutcome::Ignore) => {
                    self.trigger_ignore_signaled = true;
                }
                Ok(TriggerOutcome::Continue) => {}
                Err(err) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = updated_count;
                        self.total_changes += updated_count;
                    }
                    return Err(err);
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
        let dml = match self.get_dml_state_by_name(&stmt.table.name.name) {
            Ok(d) => d,
            Err(orig_err) => {
                if let Ok(schema) = self.schema_arc() {
                    if let Some(vinfo) = schema.view(&stmt.table.name.name).cloned() {
                        drop(schema);
                        return self.exec_delete_view_instead_of(stmt, &vinfo, params, dialect);
                    }
                }
                return Err(orig_err);
            }
        };
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
        validate_dml_indexed_by(
            table,
            stmt.table.indexed_by.as_ref(),
            stmt.where_clause.as_ref(),
            alias,
        )?;

        // Ultra-fast single-descent point DELETE (`DELETE FROM t WHERE id = ?` with no
        // RETURNING/WITH/complex indexes): reads ONLY indexed columns and removes the row in ONE
        // B-tree descent without constructing `QueryOptions`!
        if stmt.with.is_none()
            && stmt.returning.is_none()
            && dml.simple_delete_elidable
            && !dml.has_delete_triggers
            && self.trigger_ctx.is_none()
            && !self.foreign_keys
        {
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

        if self.active_triggers.is_empty()
            && (stmt
                .where_clause
                .as_ref()
                .is_some_and(xdb_sql::expr_contains_raise)
                || stmt.with.as_ref().is_some_and(|w| {
                    w.ctes
                        .iter()
                        .any(|c| xdb_sql::select_contains_raise(&c.select))
                })
                || stmt.returning.as_ref().is_some_and(|ret| {
                    ret.iter().any(|rc| match rc {
                        ResultColumn::Expr(re) => xdb_sql::expr_contains_raise(&re.expr),
                        ResultColumn::Star(rs) => rs.replace.as_deref().is_some_and(|reps| {
                            reps.iter().any(|r| xdb_sql::expr_contains_raise(&r.expr))
                        }),
                    })
                }))
        {
            return Err(SqliteError::Sql(
                "RAISE() may only be used within a trigger-program".to_string(),
            ));
        }
        let qopts = self.query_options(dialect);
        let returning_cols = match stmt.returning.as_deref() {
            Some(rc) => {
                let names = returning_column_names(table, rc)?;
                let val_view = self.live_view()?;
                let val_cat = crate::sql::live_catalog_for(&val_view, &qopts);
                validate_returning_clauses(table, rc, &val_cat, stmt.with.as_ref(), &qopts)?;
                names
            }
            None => Vec::new(),
        };
        let returning_needs_subq = stmt
            .returning
            .as_deref()
            .is_some_and(returning_has_subqueries);

        let mut _existing_mem = ConnMemGuard::new(&self.pager);
        let mut to_delete = Vec::new();
        let mut pending_loop_sql_err: Option<SqlError> = None;
        let can_direct_point = stmt.with.is_none()
            && !table.without_rowid
            && self.trigger_ctx.is_none()
            && !dml.has_delete_triggers
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
                && !(self.foreign_keys && (dml.has_incoming_fks || dml.has_outgoing_fks))
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
            } else if !exact_where_only {
                if let Some(ref we) = stmt.where_clause {
                    validate_single_table_expr_columns(
                        we,
                        table,
                        alias,
                        stmt.table.alias.is_some(),
                        self.trigger_ctx.as_deref(),
                    )?;
                }
            }
        } else {
            let view = self.live_view()?;
            let existing_rows = if self.trigger_ctx.is_none() {
                self.read_candidate_table_rows(
                    &view,
                    table,
                    alias,
                    stmt.where_clause.as_ref(),
                    params,
                    &qopts,
                    &mut _existing_mem,
                )?
            } else {
                self.read_all_table_rows(&view, table, &mut _existing_mem)?
            };
            let scope_cols = build_table_scope_columns(table, alias);
            let catalog = crate::sql::live_catalog_for(&view, &qopts);
            let sub_runner =
                CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
            let eval_env = EvalEnv::new(&qopts, params, &sub_runner);

            let had_existing = !existing_rows.is_empty();
            let mut loop_sql_err: Option<SqlError> = None;
            for (rid, row) in existing_rows {
                if let Some(ref we) = stmt.where_clause {
                    let mut vals = row.clone();
                    if !table.without_rowid {
                        let rid_v = rid.map_or(SqlValue::Null, SqlValue::Int);
                        while vals.len() < scope_cols.len() {
                            vals.push(rid_v.clone());
                        }
                    }
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&scope_cols, &vals)];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&scope_cols, &vals)];
                        &scope_single
                    };
                    let cond = match eval_expr(we, scope, &eval_env) {
                        Ok(v) => v,
                        Err(e) => {
                            loop_sql_err = Some(e);
                            break;
                        }
                    };
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }
                to_delete.push((rid, row));
            }
            if !had_existing && loop_sql_err.is_none() {
                if let Some(ref we) = stmt.where_clause {
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&scope_cols, &[])];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&scope_cols, &[])];
                        &scope_single
                    };
                    validate_dml_expr_with_catalog(
                        we,
                        scope,
                        &catalog,
                        stmt.with.as_ref(),
                        &qopts,
                    )?;
                }
            }
            drop(eval_env);
            drop(sub_runner);
            drop(view);
            if let Some(e) = loop_sql_err {
                if matches!(
                    e,
                    SqlError::Raise {
                        action: RaiseAction::Ignore | RaiseAction::Fail,
                        ..
                    }
                ) {
                    pending_loop_sql_err = Some(e);
                } else {
                    let _ = self.handle_trigger_sql_err(e)?;
                }
            }
        }

        if !table.without_rowid && !to_delete.is_empty() {
            self.invalidate_max_rowid_for(table.root_page);
        }
        let mut deleted_count = 0i64;
        let mut returning_mem = ConnMemGuard::new(&self.pager);
        let mut returning_rows = Vec::new();
        let fk_outgoing = self.foreign_keys && dml.has_outgoing_fks;
        let fk_incoming = self.foreign_keys && dml.has_incoming_fks;
        let need_live_check = dml.has_delete_triggers || fk_incoming;
        let del_conflict_override = self.active_conflict_override;

        for (rid, mut row) in to_delete {
            if need_live_check {
                match self.reload_live_row(table, &dml, rid, &row, &qopts)? {
                    None => continue,
                    Some(live_row) => row = live_row,
                }
            }
            // Preserve pre-`BEFORE`-trigger row for `DELETE ... RETURNING` and `AFTER DELETE` `OLD.*`!
            let orig_row = row.clone();
            if dml.has_delete_triggers {
                let tctx = Arc::new(TriggerRowCtx::for_table(table, Some((rid, &orig_row)), None));
                let outcome = match self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::Before,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    del_conflict_override,
                    dialect,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        if self.pending_error_conflict == Some(ConflictAction::Fail) {
                            self.changes = deleted_count;
                            self.total_changes += deleted_count;
                        }
                        return Err(e);
                    }
                };
                if outcome == TriggerOutcome::Ignore {
                    continue;
                }
                match self.reload_live_row(table, &dml, rid, &row, &qopts)? {
                    None => continue,
                    Some(live_row) => row = live_row,
                }
            }
            if fk_outgoing {
                self.fk_check_child_row(table, &row, None, -1, &qopts)?;
            }
            self.delete_row_and_indexes(table, &dml, rid, &row, &qopts)?;
            deleted_count += 1;
            if fk_incoming {
                if let Err(e) = self.apply_incoming_fk_delete_actions(table, &row, dialect) {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = deleted_count;
                        self.total_changes += deleted_count;
                    }
                    return Err(e);
                }
            }
            // Per SQLite: `RETURNING` evaluates before `AFTER` triggers and uses original `OLD` row.
            if let Some(ref ret) = stmt.returning {
                let ret_row = self.eval_returning(
                    table,
                    rid,
                    &orig_row,
                    ret,
                    stmt.with.as_ref(),
                    params,
                    dialect,
                    returning_needs_subq,
                )?;
                if returning_mem.meter.is_some() {
                    returning_mem.reserve(estimate_sql_row_bytes(&ret_row))?;
                }
                returning_rows.push(ret_row);
            }
            if dml.has_delete_triggers {
                let tctx = Arc::new(TriggerRowCtx::for_table(table, Some((rid, &orig_row)), None));
                if let Err(e) = self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::After,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    del_conflict_override,
                    dialect,
                ) {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = deleted_count;
                        self.total_changes += deleted_count;
                    }
                    return Err(e);
                }
            }
        }

        if let Some(e) = pending_loop_sql_err {
            match self.handle_trigger_sql_err(e) {
                Ok(TriggerOutcome::Ignore) => {
                    self.trigger_ignore_signaled = true;
                }
                Ok(TriggerOutcome::Continue) => {}
                Err(err) => {
                    if self.pending_error_conflict == Some(ConflictAction::Fail) {
                        self.changes = deleted_count;
                        self.total_changes += deleted_count;
                    }
                    return Err(err);
                }
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
        // 0. Keep `row[ipk]` synchronized with `rowid` (e.g. after `UPDATE ... SET rowid = ...`).
        if !table.without_rowid {
            if let (Some(ipk), Some(rid)) = (table.ipk_column, rowid) {
                row[ipk] = SqlValue::Int(rid);
            }
        }
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
        if !self.ignore_check_constraints && !table.definition.checks.is_empty() {
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
            env.in_trigger = qopts.in_trigger;
            let scope = [EvalScope::simple(&dml.check_scope_cols, &check_vals)];
            let eff = stmt_conflict.unwrap_or(ConflictAction::Abort);
            for chk in &table.definition.checks {
                if !qopts.in_trigger && xdb_sql::expr_contains_raise(&chk.expr) {
                    return Err(SqliteError::Sql(
                        "RAISE() may only be used within a trigger-program".to_string(),
                    ));
                }
                let res = match eval_expr(&chk.expr, &scope, &env) {
                    Ok(v) => v,
                    Err(e @ SqlError::Raise { .. }) if qopts.in_trigger => {
                        if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                            self.trigger_ignore_signaled = true;
                        }
                        return Ok(false);
                    }
                    Err(e) => return Err(SqliteError::from(e)),
                };
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

    /// Pre-delete non-self conflicting rows during `UPDATE` with `REPLACE` conflict resolution
    /// and active recursive `DELETE` triggers, before removing the updated row's old entry.
    /// Returns `Ok(true)` if any non-self conflict delete (or trigger side effect) ran.
    #[allow(clippy::too_many_arguments)]
    fn pre_delete_update_replace_conflicts(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        wr_pk_idx: Option<&Arc<IndexInfo>>,
        old_rowid: Option<i64>,
        old_row: &[SqlValue],
        new_rowid: Option<i64>,
        new_row: &[SqlValue],
        stmt_conflict: Option<ConflictAction>,
        qopts: &QueryOptions,
        row_replaced: &mut ReplacedTracker,
        deleted_conflict: &mut bool,
    ) -> SqliteResult<bool> {
        let old_wr_pk: Option<Vec<SqlValue>> = wr_pk_idx.map(|pk_idx| {
            pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| old_row[ic.cid as usize].clone())
                .collect()
        });
        let is_self_wr = |pk: &[SqlValue]| -> bool {
            if let (Some(ref opk), Some(ref ki)) = (&old_wr_pk, &dml.wr_pk_ki) {
                key_tuples_equal(pk, opk, ki)
            } else {
                false
            }
        };

        // First probe all initial non-self conflicts without mutating anything.
        // Only proceed when EVERY non-self conflict resolves to `ConflictAction::Replace`.
        let mut any_non_self = false;
        if !table.without_rowid {
            if let Some(nrid) = new_rowid {
                if Some(nrid) != old_rowid && table_contains_rowid(&self.pager, table.root_page, nrid)?
                {
                    if resolve_conflict(stmt_conflict, table.definition.ipk_on_error)
                        != ConflictAction::Replace
                    {
                        return Ok(false);
                    }
                    any_non_self = true;
                }
            }
        } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
            let new_pk: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| new_row[ic.cid as usize].clone())
                .collect();
            if !is_self_wr(&new_pk)
                && index_contains_prefix(&self.pager, table.root_page, &new_pk, ki)?.is_some()
            {
                if resolve_conflict(stmt_conflict, pk_idx.definition.on_error)
                    != ConflictAction::Replace
                {
                    return Ok(false);
                }
                any_non_self = true;
            }
        }
        for (ix_idx, ix) in table.indexes.iter().enumerate() {
            if !ix.unique || ix.without_rowid_primary_key {
                continue;
            }
            let Some(entry_vals) = build_index_entry_values_fast(
                table,
                ix,
                new_rowid,
                new_row,
                qopts,
                &dml.index_expr_scope_cols,
                &dml.index_where_scope_cols,
            )?
            else {
                continue;
            };
            let key_prefix = &entry_vals[..ix.key_count];
            if prefix_has_null(key_prefix) {
                continue;
            }
            let ki = dml.index_key_infos[ix_idx]
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
            let Some(hit_entry) =
                index_contains_prefix(&self.pager, ix.root_page, key_prefix, ki)?
            else {
                continue;
            };
            let is_self = if !table.without_rowid {
                hit_entry.last().and_then(SqlValue::as_i64) == old_rowid
            } else if let Some(pk_idx) = wr_pk_idx {
                let mut hit_pk = Vec::with_capacity(pk_idx.key_count);
                let mut complete = true;
                for pk_col in &pk_idx.columns[..pk_idx.key_count] {
                    if let Some(pos) = ix.columns.iter().position(|ic| ic.cid == pk_col.cid) {
                        if let Some(v) = hit_entry.get(pos) {
                            hit_pk.push(v.clone());
                            continue;
                        }
                    }
                    complete = false;
                    break;
                }
                complete && is_self_wr(&hit_pk)
            } else {
                false
            };
            if is_self {
                continue;
            }
            if resolve_conflict(stmt_conflict, ix.definition.on_error) != ConflictAction::Replace {
                return Ok(false);
            }
            any_non_self = true;
        }
        if !any_non_self {
            return Ok(false);
        }

        // Sequentially re-probe and delete each live non-self conflict (PK first, then UNIQUE
        // secondary indexes in schema order), firing `BEFORE`/`AFTER DELETE` triggers live.
        let mut ran_side_effects = false;
        let mut rep_opt = Some(row_replaced);
        if !table.without_rowid {
            if let Some(nrid) = new_rowid {
                if Some(nrid) != old_rowid && table_contains_rowid(&self.pager, table.root_page, nrid)?
                {
                    let col_label = table
                        .ipk_column
                        .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                        .unwrap_or_else(|| format!("{}.rowid", table.name));
                    let live_row = self.fetch_rowid_row(table, nrid, qopts)?;
                    self.delete_conflict_row_for_replace(
                        table,
                        dml,
                        wr_pk_idx,
                        Some(nrid),
                        live_row,
                        &col_label,
                        stmt_conflict,
                        qopts,
                        true,
                        &mut rep_opt,
                        deleted_conflict,
                        &mut ran_side_effects,
                    )?;
                }
            }
        } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
            let new_pk: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| new_row[ic.cid as usize].clone())
                .collect();
            if !is_self_wr(&new_pk) {
                if let Some(hit_entry) =
                    index_contains_prefix(&self.pager, table.root_page, &new_pk, ki)?
                {
                    let live_row = decode_without_rowid_full_row(table, &hit_entry, qopts)?;
                    let label = format!("{}.PRIMARY KEY", table.name);
                    self.delete_conflict_row_for_replace(
                        table,
                        dml,
                        wr_pk_idx,
                        None,
                        live_row,
                        &label,
                        stmt_conflict,
                        qopts,
                        true,
                        &mut rep_opt,
                        deleted_conflict,
                        &mut ran_side_effects,
                    )?;
                }
            }
        }

        for (ix_idx, ix) in table.indexes.iter().enumerate() {
            if !ix.unique || ix.without_rowid_primary_key {
                continue;
            }
            let Some(entry_vals) = build_index_entry_values_fast(
                table,
                ix,
                new_rowid,
                new_row,
                qopts,
                &dml.index_expr_scope_cols,
                &dml.index_where_scope_cols,
            )?
            else {
                continue;
            };
            let key_prefix = &entry_vals[..ix.key_count];
            if prefix_has_null(key_prefix) {
                continue;
            }
            let ki = dml.index_key_infos[ix_idx]
                .as_ref()
                .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
            let Some(hit_entry) =
                index_contains_prefix(&self.pager, ix.root_page, key_prefix, ki)?
            else {
                continue;
            };
            let label = format!("{}.{}", table.name, ix.name);
            if !table.without_rowid {
                if let Some(hrid) = hit_entry.last().and_then(SqlValue::as_i64) {
                    if Some(hrid) == old_rowid {
                        continue;
                    }
                    let live_row = self.fetch_rowid_row(table, hrid, qopts)?;
                    self.delete_conflict_row_for_replace(
                        table,
                        dml,
                        wr_pk_idx,
                        Some(hrid),
                        live_row,
                        &label,
                        stmt_conflict,
                        qopts,
                        true,
                        &mut rep_opt,
                        deleted_conflict,
                        &mut ran_side_effects,
                    )?;
                }
            } else if let (Some(pk_idx), Some(ref pk_ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                let mut hit_pk = Vec::with_capacity(pk_idx.key_count);
                let mut complete = true;
                for pk_col in &pk_idx.columns[..pk_idx.key_count] {
                    if let Some(pos) = ix.columns.iter().position(|ic| ic.cid == pk_col.cid) {
                        if let Some(v) = hit_entry.get(pos) {
                            hit_pk.push(v.clone());
                            continue;
                        }
                    }
                    complete = false;
                    break;
                }
                if complete && !is_self_wr(&hit_pk) {
                    if let Some(full_entry) = index_contains_prefix(
                        &self.pager,
                        table.root_page,
                        &hit_pk,
                        pk_ki,
                    )? {
                        let live_row =
                            decode_without_rowid_full_row(table, &full_entry, qopts)?;
                        self.delete_conflict_row_for_replace(
                            table,
                            dml,
                            wr_pk_idx,
                            None,
                            live_row,
                            &label,
                            stmt_conflict,
                            qopts,
                            true,
                            &mut rep_opt,
                            deleted_conflict,
                            &mut ran_side_effects,
                        )?;
                    }
                }
            }
        }
        if ran_side_effects {
            *deleted_conflict = true;
        }
        Ok(ran_side_effects || *deleted_conflict)
    }

    #[allow(clippy::too_many_arguments)]
    fn delete_conflict_row_for_replace(
        &mut self,
        table: &TableInfo,
        dml: &DmlTableState,
        wr_pk_idx: Option<&Arc<IndexInfo>>,
        crid: Option<i64>,
        mut crow: Vec<SqlValue>,
        label: &str,
        stmt_conflict: Option<ConflictAction>,
        qopts: &QueryOptions,
        fire_replace_del_triggers: bool,
        replaced: &mut Option<&mut ReplacedTracker>,
        deleted_conflict: &mut bool,
        ran_replace_side_effects: &mut bool,
    ) -> SqliteResult<()> {
        if crow.is_empty() {
            return Ok(());
        }
        if !table.without_rowid {
            let Some(rid) = crid else {
                return Ok(());
            };
            let orig_crow = crow.clone();
            if fire_replace_del_triggers {
                *ran_replace_side_effects = true;
                let tctx = Arc::new(TriggerRowCtx::for_table(
                    table,
                    Some((Some(rid), &orig_crow)),
                    None,
                ));
                if self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::Before,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    stmt_conflict,
                    qopts.dialect,
                )? == TriggerOutcome::Ignore
                {
                    return Err(self.constraint_err(
                        ConflictAction::Abort,
                        format!("UNIQUE constraint failed: {label}"),
                    ));
                }
                match self.reload_live_row(table, dml, Some(rid), &crow, qopts)? {
                    None => {
                        *deleted_conflict = true;
                        if let Some(tr) = replaced.as_deref_mut() {
                            tr.rowids.insert(rid);
                        }
                        return Ok(());
                    }
                    Some(live_row) => crow = live_row,
                }
            }
            if self.foreign_keys && dml.has_outgoing_fks {
                self.fk_check_child_row(table, &crow, None, -1, qopts)?;
            }
            self.delete_row_and_indexes(table, dml, Some(rid), &crow, qopts)?;
            *deleted_conflict = true;
            if let Some(tr) = replaced.as_deref_mut() {
                tr.rowids.insert(rid);
            }
            if self.foreign_keys && dml.has_incoming_fks {
                *ran_replace_side_effects = true;
                self.apply_incoming_fk_delete_actions(table, &crow, qopts.dialect)?;
            }
            if fire_replace_del_triggers {
                *ran_replace_side_effects = true;
                let tctx = Arc::new(TriggerRowCtx::for_table(
                    table,
                    Some((Some(rid), &orig_crow)),
                    None,
                ));
                let _ = self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::After,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    stmt_conflict,
                    qopts.dialect,
                )?;
            }
        } else if let Some(pk_idx) = wr_pk_idx {
            let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                .iter()
                .map(|ic| crow[ic.cid as usize].clone())
                .collect();
            let orig_crow = crow.clone();
            if fire_replace_del_triggers {
                *ran_replace_side_effects = true;
                let tctx =
                    Arc::new(TriggerRowCtx::for_table(table, Some((None, &orig_crow)), None));
                if self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::Before,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    stmt_conflict,
                    qopts.dialect,
                )? == TriggerOutcome::Ignore
                {
                    return Err(self.constraint_err(
                        ConflictAction::Abort,
                        format!("UNIQUE constraint failed: {label}"),
                    ));
                }
                match self.reload_live_row(table, dml, None, &crow, qopts)? {
                    None => {
                        *deleted_conflict = true;
                        if let Some(tr) = replaced.as_deref_mut() {
                            tr.wr_pks.push(pk_vals);
                        }
                        return Ok(());
                    }
                    Some(live_row) => crow = live_row,
                }
            }
            if self.foreign_keys && dml.has_outgoing_fks {
                self.fk_check_child_row(table, &crow, None, -1, qopts)?;
            }
            self.delete_row_and_indexes(table, dml, None, &crow, qopts)?;
            *deleted_conflict = true;
            if let Some(tr) = replaced.as_deref_mut() {
                tr.wr_pks.push(pk_vals);
            }
            if self.foreign_keys && dml.has_incoming_fks {
                *ran_replace_side_effects = true;
                self.apply_incoming_fk_delete_actions(table, &crow, qopts.dialect)?;
            }
            if fire_replace_del_triggers {
                *ran_replace_side_effects = true;
                let tctx =
                    Arc::new(TriggerRowCtx::for_table(table, Some((None, &orig_crow)), None));
                let _ = self.fire_triggers(
                    &dml.triggers,
                    TriggerTime::After,
                    TriggerEventKind::Delete,
                    None,
                    &tctx,
                    stmt_conflict,
                    qopts.dialect,
                )?;
            }
        }
        Ok(())
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
        is_insert: bool,
    ) -> SqliteResult<RowWriteOutcome> {
        let wr_pk_idx = if table.without_rowid {
            table.indexes.iter().find(|ix| ix.without_rowid_primary_key)
        } else {
            None
        };

        let mut deleted_conflict = false;

        // Phase A: Probe initial conflicts (rowid PK, WITHOUT ROWID PK, and UNIQUE secondary indexes)
        // without mutating any state yet so UPSERT and non-REPLACE conflict actions run cleanly.
        let mut conflicting_rows: Vec<(Option<i64>, Vec<SqlValue>, String, OnError)> = Vec::new();
        if !table.without_rowid {
            let rid = rowid.expect("rowid table has rowid");
            if !rowid_guaranteed_new && table_contains_rowid(&self.pager, table.root_page, rid)? {
                let col_label = table
                    .ipk_column
                    .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                    .unwrap_or_else(|| format!("{}.rowid", table.name));
                let old_row = self.fetch_rowid_row(table, rid, qopts)?;
                conflicting_rows.push((
                    Some(rid),
                    old_row,
                    col_label,
                    table.definition.ipk_on_error,
                ));
            }
        } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
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
                        if let Some(full_entry) = index_contains_prefix(
                            &self.pager,
                            table.root_page,
                            &pk_vals,
                            pk_ki,
                        )? {
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
                                if !table.without_rowid {
                                    let excl_rv = rowid.map_or(SqlValue::Null, SqlValue::Int);
                                    for _ in 0..count_synth_rowid_aliases(table) {
                                        scope_vals.push(excl_rv.clone());
                                    }
                                }
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
                                    let t_scope_opt =
                                        self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                                    let scope_storage: [EvalScope<'_>; 2];
                                    let scope_single: [EvalScope<'_>; 1];
                                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                                        scope_storage = [ts, sc];
                                        &scope_storage
                                    } else {
                                        scope_single = [sc];
                                        &scope_single
                                    };

                                    let res: Result<bool, SqlError> = (|| {
                                        if let Some(ref we) = up.where_clause {
                                            let cond = eval_expr(we, scope, &eval_env)?;
                                            if !boolean_value(&cond, false) {
                                                return Ok(false);
                                            }
                                        }
                                        if let Some(ref sets) = up.set {
                                            let (nr, nrow) = apply_set_clauses_sql_err(
                                                table,
                                                old_rid,
                                                &old_row,
                                                sets,
                                                scope,
                                                &eval_env,
                                            )?;
                                            upd_rid = nr;
                                            upd_row = nrow;
                                        }
                                        Ok(true)
                                    })();
                                    res
                                };
                                let should_update = match should_update {
                                    Ok(b) => b,
                                    Err(e) => {
                                        if self.handle_trigger_sql_err(e)?
                                            == TriggerOutcome::Ignore
                                        {
                                            self.trigger_ignore_signaled = true;
                                            return Ok(RowWriteOutcome::Ignored);
                                        }
                                        return Ok(RowWriteOutcome::Ignored);
                                    }
                                };
                                if !should_update {
                                    return Ok(RowWriteOutcome::Ignored);
                                }
                                let mut upsert_set_mask = vec![false; table.columns.len()];
                                let mut upsert_upd_cols: Vec<String> = Vec::new();
                                for sc in up.set.as_deref().unwrap_or(&[]) {
                                    for c in &sc.columns {
                                        if let Some(pos) = table
                                            .columns
                                            .iter()
                                            .position(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                                        {
                                            upsert_set_mask[pos] = true;
                                            upsert_upd_cols.push(table.columns[pos].name.clone());
                                        } else if !table.without_rowid
                                            && matches!(
                                                c.name.to_ascii_lowercase().as_str(),
                                                "rowid" | "_rowid_" | "oid"
                                            )
                                        {
                                            if let Some(ipk) = table.ipk_column {
                                                upsert_set_mask[ipk] = true;
                                            }
                                            upsert_upd_cols.push(c.name.clone());
                                        } else {
                                            upsert_upd_cols.push(c.name.clone());
                                        }
                                    }
                                }
                                let mut old_row = old_row;
                                let orig_old_row = old_row.clone();
                                // Per SQLite: UPSERT `DO UPDATE` always enforces mandatory
                                // `ON CONFLICT ABORT` semantics, overriding statement-level
                                // `OR REPLACE`/`OR IGNORE`/etc., schema `ON CONFLICT`, and any
                                // outer trigger conflict override!
                                let prev_conflict_ov = self.active_conflict_override;
                                self.active_conflict_override = Some(ConflictAction::Abort);
                                let do_upd_res: SqliteResult<RowWriteOutcome> = (|| {
                                    if dml.has_update_triggers {
                                        let mut pre_upd = upd_row.clone();
                                        if !table.without_rowid {
                                            if let (Some(ipk), Some(rid)) =
                                                (table.ipk_column, upd_rid)
                                            {
                                                pre_upd[ipk] = SqlValue::Int(rid);
                                            }
                                        }
                                        self.apply_affinities_and_generated(
                                            table,
                                            dml,
                                            &mut pre_upd,
                                            qopts,
                                        )?;
                                        let tctx = Arc::new(TriggerRowCtx::for_table(
                                            table,
                                            Some((old_rid, &orig_old_row)),
                                            Some((upd_rid, &pre_upd)),
                                        ));
                                        if self.fire_triggers(
                                            &dml.triggers,
                                            TriggerTime::Before,
                                            TriggerEventKind::Update,
                                            Some(&upsert_upd_cols),
                                            &tctx,
                                            Some(ConflictAction::Abort),
                                            qopts.dialect,
                                        )? == TriggerOutcome::Ignore
                                        {
                                            return Ok(RowWriteOutcome::Ignored);
                                        }
                                        match self.reload_live_row(
                                            table, dml, old_rid, &old_row, qopts,
                                        )? {
                                            None => return Ok(RowWriteOutcome::Ignored),
                                            Some(live_row) => {
                                                for cid in 0..table.columns.len() {
                                                    if !upsert_set_mask[cid]
                                                        && table.columns[cid].generated.is_none()
                                                    {
                                                        upd_row[cid] = live_row[cid].clone();
                                                    }
                                                }
                                                old_row = live_row;
                                            }
                                        }
                                    }
                                    if !self.finalize_and_validate_row(
                                        table,
                                        dml,
                                        upd_rid,
                                        &mut upd_row,
                                        Some(ConflictAction::Abort),
                                        params,
                                        qopts,
                                    )? {
                                        return Ok(RowWriteOutcome::Ignored);
                                    }
                                    let fk_outgoing = self.foreign_keys && dml.has_outgoing_fks;
                                    let fk_incoming = self.foreign_keys && dml.has_incoming_fks;
                                    let saved_fk = self.fk_snapshot();
                                    self.pager.push_savepoint(None);
                                    if fk_outgoing {
                                        if let Err(e) = self.fk_check_child_row(
                                            table,
                                            &old_row,
                                            Some(&upsert_set_mask),
                                            -1,
                                            qopts,
                                        ) {
                                            self.pager.rollback_statement_savepoint();
                                            self.fk_restore(saved_fk);
                                            return Err(e);
                                        }
                                    }
                                    if let Err(e) = self.delete_row_and_indexes(
                                        table, dml, old_rid, &old_row, qopts,
                                    ) {
                                        self.pager.rollback_statement_savepoint();
                                        self.fk_restore(saved_fk);
                                        return Err(e);
                                    }
                                    // Re-check all remaining PK/UNIQUE constraints on the updated row!
                                    match self.insert_row_with_constraints(
                                        table,
                                        dml,
                                        target_alias,
                                        &mut upd_rid,
                                        &mut upd_row,
                                        Some(ConflictAction::Abort),
                                        &[],
                                        with_clause,
                                        params,
                                        qopts,
                                        None,
                                        None,
                                        false,
                                        need_returning
                                            || dml.has_update_triggers
                                            || fk_outgoing
                                            || fk_incoming,
                                        false,
                                    ) {
                                        Ok(RowWriteOutcome::Written {
                                            final_rowid,
                                            final_row,
                                            deleted_conflict,
                                            ..
                                        }) => {
                                            if fk_outgoing {
                                                if let Err(e) = self.fk_check_child_row(
                                                    table,
                                                    &final_row,
                                                    Some(&upsert_set_mask),
                                                    1,
                                                    qopts,
                                                ) {
                                                    self.pager.rollback_statement_savepoint();
                                                    self.fk_restore(saved_fk);
                                                    return Err(e);
                                                }
                                            }
                                            self.pager.commit_statement_savepoint();
                                            if fk_incoming {
                                                self.apply_incoming_fk_update_actions(
                                                    table,
                                                    &old_row,
                                                    &final_row,
                                                    final_rowid,
                                                    qopts.dialect,
                                                )?;
                                            }
                                            let upsert_after = if dml.has_update_triggers {
                                                Some((old_rid, orig_old_row, upsert_upd_cols))
                                            } else {
                                                None
                                            };
                                            Ok(RowWriteOutcome::Written {
                                                final_rowid,
                                                final_row,
                                                was_insert: false,
                                                deleted_conflict,
                                                upsert_after,
                                            })
                                        }
                                        Ok(RowWriteOutcome::Ignored) => {
                                            self.pager.rollback_statement_savepoint();
                                            self.fk_restore(saved_fk);
                                            Ok(RowWriteOutcome::Ignored)
                                        }
                                        Err(e) => {
                                            self.pager.rollback_statement_savepoint();
                                            self.fk_restore(saved_fk);
                                            Err(e)
                                        }
                                    }
                                })();
                                self.active_conflict_override = prev_conflict_ov;
                                return do_upd_res;
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
            drop(conflicting_rows);

            // Phase B: All conflicts resolve to REPLACE. Execute SQLite's exact sequential LIVE
            // per-constraint check & delete: (1) PK/rowid, then (2) each UNIQUE secondary index
            // in order (always re-probing live B-tree state after any earlier DELETE triggers or
            // incoming FK cascades so we never delete using stale row snapshots!), then (3) if any
            // triggers or FK actions ran, one final LIVE verification pass rejecting any surviving
            // or reborn PK / UNIQUE conflict with `UNIQUE constraint failed`.
            let fire_replace_del_triggers = self.recursive_triggers && dml.has_delete_triggers;
            if fire_replace_del_triggers {
                if let Some(max_rid) = pending_autoinc_max.and_then(Option::take) {
                    self.update_sqlite_sequence(&table.name, max_rid)?;
                }
            }
            let mut ran_replace_side_effects = false;

            // Step B1: Live PK / rowid conflict check & delete.
            if !table.without_rowid {
                let rid = rowid.expect("rowid table has rowid");
                if !rowid_guaranteed_new && table_contains_rowid(&self.pager, table.root_page, rid)?
                {
                    let col_label = table
                        .ipk_column
                        .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                        .unwrap_or_else(|| format!("{}.rowid", table.name));
                    let live_row = self.fetch_rowid_row(table, rid, qopts)?;
                    self.delete_conflict_row_for_replace(
                        table,
                        dml,
                        wr_pk_idx,
                        Some(rid),
                        live_row,
                        &col_label,
                        stmt_conflict,
                        qopts,
                        fire_replace_del_triggers,
                        &mut replaced,
                        &mut deleted_conflict,
                        &mut ran_replace_side_effects,
                    )?;
                }
            } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                    .iter()
                    .map(|ic| row[ic.cid as usize].clone())
                    .collect();
                if let Some(hit_entry) =
                    index_contains_prefix(&self.pager, table.root_page, &pk_vals, ki)?
                {
                    let live_row = decode_without_rowid_full_row(table, &hit_entry, qopts)?;
                    let label = format!("{}.PRIMARY KEY", table.name);
                    self.delete_conflict_row_for_replace(
                        table,
                        dml,
                        wr_pk_idx,
                        None,
                        live_row,
                        &label,
                        stmt_conflict,
                        qopts,
                        fire_replace_del_triggers,
                        &mut replaced,
                        &mut deleted_conflict,
                        &mut ran_replace_side_effects,
                    )?;
                }
            }

            // Step B2: Live UNIQUE secondary index checks & deletes in schema order.
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
                    continue;
                }
                let ki = dml.index_key_infos[ix_idx]
                    .as_ref()
                    .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
                let Some(hit_entry) =
                    index_contains_prefix(&self.pager, ix.root_page, key_prefix, ki)?
                else {
                    continue;
                };
                let label = format!("{}.{}", table.name, ix.name);
                let eff = resolve_conflict(stmt_conflict, ix.definition.on_error);
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
                if !table.without_rowid {
                    if let Some(hrid) = hit_entry.last().and_then(SqlValue::as_i64) {
                        let live_row = self.fetch_rowid_row(table, hrid, qopts)?;
                        self.delete_conflict_row_for_replace(
                            table,
                            dml,
                            wr_pk_idx,
                            Some(hrid),
                            live_row,
                            &label,
                            stmt_conflict,
                            qopts,
                            fire_replace_del_triggers,
                            &mut replaced,
                            &mut deleted_conflict,
                            &mut ran_replace_side_effects,
                        )?;
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
                    if complete {
                        if let Some(full_entry) = index_contains_prefix(
                            &self.pager,
                            table.root_page,
                            &pk_vals,
                            pk_ki,
                        )? {
                            let live_row =
                                decode_without_rowid_full_row(table, &full_entry, qopts)?;
                            self.delete_conflict_row_for_replace(
                                table,
                                dml,
                                wr_pk_idx,
                                None,
                                live_row,
                                &label,
                                stmt_conflict,
                                qopts,
                                fire_replace_del_triggers,
                                &mut replaced,
                                &mut deleted_conflict,
                                &mut ran_replace_side_effects,
                            )?;
                        }
                    }
                }
            }

            // Step B3: If DELETE triggers or incoming FK actions ran, verify that no PK/rowid or
            // UNIQUE secondary index conflict was re-introduced before writing the new row.
            if ran_replace_side_effects {
                deleted_conflict = true;
                if !table.without_rowid {
                    let rid = rowid.expect("rowid table has rowid");
                    if table_contains_rowid(&self.pager, table.root_page, rid)? {
                        let col_label = table
                            .ipk_column
                            .map(|c| format!("{}.{}", table.name, table.columns[c].name))
                            .unwrap_or_else(|| format!("{}.rowid", table.name));
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            format!("UNIQUE constraint failed: {col_label}"),
                        ));
                    }
                } else if let (Some(pk_idx), Some(ref ki)) = (wr_pk_idx, &dml.wr_pk_ki) {
                    let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
                        .iter()
                        .map(|ic| row[ic.cid as usize].clone())
                        .collect();
                    if index_contains_prefix(&self.pager, table.root_page, &pk_vals, ki)?.is_some()
                    {
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            format!("UNIQUE constraint failed: {}.PRIMARY KEY", table.name),
                        ));
                    }
                }
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
                        continue;
                    }
                    let ki = dml.index_key_infos[ix_idx]
                        .as_ref()
                        .ok_or_else(|| SqliteError::corrupt("missing KeyInfo for secondary index"))?;
                    if index_contains_prefix(&self.pager, ix.root_page, key_prefix, ki)?.is_some() {
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            format!("UNIQUE constraint failed: {}.{}", table.name, ix.name),
                        ));
                    }
                }
            }
        }

        if is_insert && self.foreign_keys && dml.has_incoming_fks {
            self.fk_check_parent_insert(table, row, qopts)?;
        }
        self.write_row_and_indexes(table, dml, *rowid, row, qopts)?;
        if is_insert && self.foreign_keys && dml.has_outgoing_fks {
            self.fk_check_child_row(table, row, None, 1, qopts)?;
        }
        Ok(RowWriteOutcome::Written {
            final_rowid: *rowid,
            final_row: if need_returning {
                row.to_owned()
            } else {
                Vec::new()
            },
            was_insert: true,
            deleted_conflict,
            upsert_after: None,
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
        self.read_all_table_rows_opts(view, table, mem, true)
    }

    fn read_all_table_rows_opts(
        &self,
        view: &SqliteDatabase,
        table: &TableInfo,
        mem: &mut ConnMemGuard,
        eval_virtual: bool,
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
        let virt_plan = if eval_virtual && table.has_virtual_columns {
            Some((
                generated_eval_order_from_defs(&table.name, &table.definition.columns)?,
                build_plain_scope_columns(table),
            ))
        } else {
            None
        };
        let metered = !self.pager.is_noop_meter();
        let mut out = Vec::new();
        while cur.next()? {
            if metered {
                self.pager.meter().charge_steps(1)?;
            }
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
        let (cols, rows) = self.eval_select_stmt_detailed(view, stmt, params, options)?;
        Ok(QueryResult {
            columns: cols.into_iter().map(|c| c.name).collect(),
            rows,
        })
    }

    fn eval_select_stmt_detailed(
        &self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)> {
        let catalog = crate::sql::live_catalog_for(view, options);
        if let Some(ref tctx) = self.trigger_ctx {
            let scope = [tctx.as_eval_scope()];
            execute_select_stmt_with_scopes(&catalog, stmt, params, options, &scope)
                .map_err(SqliteError::from)
        } else {
            execute_select_stmt_detailed(&catalog, stmt, params, options).map_err(SqliteError::from)
        }
    }

    fn handle_trigger_sql_err(&mut self, err: SqlError) -> SqliteResult<TriggerOutcome> {
        match err {
            SqlError::Raise {
                action: RaiseAction::Ignore,
                ..
            } => Ok(TriggerOutcome::Ignore),
            SqlError::Raise {
                action: RaiseAction::Abort,
                message,
            } => Err(self.constraint_err(
                ConflictAction::Abort,
                message.unwrap_or_else(|| "constraint failed".to_string()),
            )),
            SqlError::Raise {
                action: RaiseAction::Rollback,
                message,
            } => Err(self.constraint_err(
                ConflictAction::Rollback,
                message.unwrap_or_else(|| "constraint failed".to_string()),
            )),
            SqlError::Raise {
                action: RaiseAction::Fail,
                message,
            } => Err(self.constraint_err(
                ConflictAction::Fail,
                message.unwrap_or_else(|| "constraint failed".to_string()),
            )),
            other => Err(SqliteError::from(other)),
        }
    }

    #[allow(clippy::type_complexity)]
    fn eval_select_stmt_detailed_in_dml(
        &mut self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<Option<(Vec<ScopeColumn>, Vec<Vec<SqlValue>>)>> {
        let catalog = crate::sql::live_catalog_for(view, options);
        let res = if let Some(ref tctx) = self.trigger_ctx {
            let scope = [tctx.as_eval_scope()];
            execute_select_stmt_with_scopes(&catalog, stmt, params, options, &scope)
        } else {
            execute_select_stmt_detailed(&catalog, stmt, params, options)
        };
        match res {
            Ok(pair) => Ok(Some(pair)),
            Err(e) => {
                if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                    self.trigger_ignore_signaled = true;
                    return Ok(None);
                }
                Ok(None)
            }
        }
    }

    fn eval_select_stmt_in_dml(
        &mut self,
        view: &SqliteDatabase,
        stmt: &SelectStmt,
        outer_with: Option<&xdb_parser::WithClause>,
        params: &QueryParams,
        options: &QueryOptions,
    ) -> SqliteResult<Option<QueryResult>> {
        let catalog = crate::sql::live_catalog_for(view, options);
        let res = if let Some(ref tctx) = self.trigger_ctx {
            let scope = [tctx.as_eval_scope()];
            xdb_sql::execute_select_stmt_with_outer_with(
                &catalog, stmt, params, options, &scope, outer_with,
            )
        } else {
            xdb_sql::execute_select_stmt_with_outer_with(
                &catalog, stmt, params, options, &[], outer_with,
            )
        };
        match res {
            Ok((cols, rows)) => Ok(Some(QueryResult {
                columns: cols.into_iter().map(|c| c.name).collect(),
                rows,
            })),
            Err(e) => {
                if self.trigger_ctx.is_some() {
                    if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                        self.trigger_ignore_signaled = true;
                    }
                    Ok(None)
                } else {
                    Err(SqliteError::from(e))
                }
            }
        }
    }

    /// Re-read the live on-disk row for `(old_rowid, old_row)` (rowid or `WITHOUT ROWID` table).
    /// Returns `Ok(None)` if an earlier cascade or `BEFORE` trigger deleted the row.
    fn reload_live_row(
        &self,
        table: &TableInfo,
        dml: &DmlTableState,
        old_rowid: Option<i64>,
        old_row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<Option<Vec<SqlValue>>> {
        let enc = self.pager.encoding();
        let mut gen_qopts = qopts.clone();
        gen_qopts.in_trigger = false;
        if !table.without_rowid {
            let Some(rid) = old_rowid else {
                return Ok(None);
            };
            let Some(fields) = table_get_record_fields(&self.pager, table.root_page, rid, enc)?
            else {
                return Ok(None);
            };
            let mut row = Vec::with_capacity(table.columns.len());
            for (cid, col) in table.columns.iter().enumerate() {
                if Some(cid) == table.ipk_column {
                    row.push(SqlValue::Int(rid));
                } else if col.hidden == ColumnHiddenKind::Virtual {
                    row.push(SqlValue::Null);
                } else if let Some(sidx) = col.storage_index {
                    let raw = fields
                        .get(sidx)
                        .cloned()
                        .unwrap_or_else(|| col.default_value.clone());
                    let v = if col.affinity == Affinity::Real {
                        crate::schema::real_read_back(raw)
                    } else {
                        raw
                    };
                    row.push(v);
                } else {
                    row.push(col.default_value.clone());
                }
            }
            if !dml.gen_order.is_empty() {
                for &cid in &dml.gen_order {
                    if table.columns[cid].hidden == ColumnHiddenKind::Virtual {
                        if let Some(ref expr) = table.definition.columns[cid].generated {
                            row[cid] = eval_generated_column_value(
                                table,
                                cid,
                                expr,
                                &dml.gen_scope_cols,
                                &row,
                                &gen_qopts,
                            )?;
                        }
                    }
                }
            }
            return Ok(Some(row));
        }

        // WITHOUT ROWID table: look up live row by PK values in the table's index B-tree.
        let Some(pk_idx) = table.indexes.iter().find(|ix| ix.without_rowid_primary_key) else {
            return Ok(None);
        };
        let Some(ref pk_ki) = dml.wr_pk_ki else {
            return Ok(None);
        };
        let pk_vals: Vec<SqlValue> = pk_idx.columns[..pk_idx.key_count]
            .iter()
            .map(|ic| old_row[ic.cid as usize].clone())
            .collect();
        let Some(entry) = index_contains_prefix(&self.pager, table.root_page, &pk_vals, pk_ki)?
        else {
            return Ok(None);
        };
        let mut row = vec![SqlValue::Null; table.columns.len()];
        for (i, ic) in pk_idx.columns.iter().enumerate() {
            if ic.cid >= 0 {
                let cid = ic.cid as usize;
                let raw = entry
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| table.columns[cid].default_value.clone());
                row[cid] = if table.columns[cid].affinity == Affinity::Real {
                    crate::schema::real_read_back(raw)
                } else {
                    raw
                };
            }
        }
        if !dml.gen_order.is_empty() {
            for &cid in &dml.gen_order {
                if table.columns[cid].hidden == ColumnHiddenKind::Virtual {
                    if let Some(ref expr) = table.definition.columns[cid].generated {
                        row[cid] = eval_generated_column_value(
                            table,
                            cid,
                            expr,
                            &dml.gen_scope_cols,
                            &row,
                            &gen_qopts,
                        )?;
                    }
                }
            }
        }
        Ok(Some(row))
    }

    fn apply_affinities_and_generated(
        &self,
        table: &TableInfo,
        dml: &DmlTableState,
        row: &mut [SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        let mut gen_qopts = qopts.clone();
        gen_qopts.in_trigger = false;
        for (cid, col) in table.columns.iter().enumerate() {
            if col.generated.is_some() || (Some(cid) == table.ipk_column && !table.without_rowid) {
                continue;
            }
            let raw = std::mem::replace(&mut row[cid], SqlValue::Null);
            if table.strict {
                // Per SQLite: STRICT datatype validation runs BEFORE `BEFORE` triggers.
                row[cid] = validate_strict_value(raw, &col.decl_type, &table.name, &col.name)?;
            } else {
                row[cid] = apply_affinity(raw, col.affinity);
            }
        }
        if !dml.gen_order.is_empty() {
            for &cid in &dml.gen_order {
                if let Some(ref expr) = table.definition.columns[cid].generated {
                    row[cid] = eval_generated_column_value(
                        table,
                        cid,
                        expr,
                        &dml.gen_scope_cols,
                        row,
                        &gen_qopts,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn resolve_view_columns(
        &self,
        view_db: &SqliteDatabase,
        vinfo: &ViewInfo,
        qopts: &QueryOptions,
    ) -> SqliteResult<Vec<ScopeColumn>> {
        let sql = format!(
            "SELECT * FROM \"{}\" LIMIT 0",
            vinfo.name.replace('"', "\"\"")
        );
        let stmt = parse_statement(&sql, &ParseOptions::default())
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        let Statement::Select(sel) = stmt else {
            return Err(SqliteError::Sql("expected SELECT".into()));
        };
        let (cols, _) = self.eval_select_stmt_detailed(view_db, &sel, &QueryParams::empty(), qopts)?;
        Ok(cols)
    }

    fn exec_insert_view_instead_of(
        &mut self,
        stmt: &InsertStmt,
        vinfo: &ViewInfo,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        let triggers = {
            let schema = self.schema_arc()?;
            compile_triggers_for_target(&schema, &vinfo.name)
        };
        let has_io = triggers
            .iter()
            .any(|t| t.time == TriggerTime::InsteadOf && t.event == TriggerEventKind::Insert);
        if !has_io {
            return Err(SqliteError::Sql(format!(
                "cannot modify {} because it is a view",
                vinfo.name
            )));
        }
        if stmt.returning.is_some() {
            return Err(SqliteError::Sql(
                "cannot use RETURNING on views".to_string(),
            ));
        }
        if !stmt.upsert.is_empty() {
            return Err(SqliteError::Sql("cannot UPSERT a view".to_string()));
        }
        if self.active_triggers.is_empty()
            && (stmt
                .select
                .as_deref()
                .is_some_and(xdb_sql::select_contains_raise)
                || stmt.with.as_ref().is_some_and(|w| {
                    w.ctes
                        .iter()
                        .any(|c| xdb_sql::select_contains_raise(&c.select))
                }))
        {
            return Err(SqliteError::Sql(
                "RAISE() may only be used within a trigger-program".to_string(),
            ));
        }
        let qopts = self.query_options(dialect);
        let view_db = self.live_view()?;
        let vcols = self.resolve_view_columns(&view_db, vinfo, &qopts)?;
        let target_cids: Vec<usize> = if let Some(ref cols) = stmt.columns {
            let mut out = Vec::with_capacity(cols.len());
            for c in cols {
                let Some(pos) = vcols
                    .iter()
                    .position(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                else {
                    return Err(SqliteError::Sql(format!(
                        "table {} has no column named {}",
                        vinfo.name, c.name
                    )));
                };
                out.push(pos);
            }
            out
        } else {
            (0..vcols.len()).collect()
        };
        let own_conflict = if stmt.replace_into {
            Some(ConflictAction::Replace)
        } else {
            stmt.or_conflict
        };
        let stmt_conflict = self.active_conflict_override.or(own_conflict);
        let mut rows_mem = ConnMemGuard::new(&self.pager);
        let source_rows: Vec<Vec<SqlValue>> = if let Some(ref sel) = stmt.select {
            let qres = match self.eval_select_stmt_in_dml(
                &view_db,
                sel,
                stmt.with.as_ref(),
                params,
                &qopts,
            )? {
                Some(r) => r,
                None => return Ok(self.empty_exec_result()),
            };
            if qres.columns.len() != target_cids.len() {
                return Err(SqliteError::Sql(format!(
                    "table {} has {} columns but {} values were supplied",
                    vinfo.name,
                    target_cids.len(),
                    qres.columns.len()
                )));
            }
            for r in &qres.rows {
                rows_mem.reserve(estimate_sql_row_bytes(r))?;
            }
            qres.rows
        } else {
            vec![Vec::new()]
        };
        drop(view_db);

        for src in source_rows {
            let mut new_row = vec![SqlValue::Null; vcols.len()];
            let mut assigned = vec![false; vcols.len()];
            for (&cid, val) in target_cids.iter().zip(src.into_iter()) {
                if !assigned[cid] {
                    new_row[cid] = val;
                    assigned[cid] = true;
                }
            }
            let tctx = Arc::new(TriggerRowCtx::for_view(&vcols, None, Some(&new_row)));
            if let Err(e) = self.fire_triggers(
                &triggers,
                TriggerTime::InsteadOf,
                TriggerEventKind::Insert,
                None,
                &tctx,
                stmt_conflict,
                dialect,
            ) {
                self.changes = 0;
                return Err(e);
            }
        }
        self.changes = 0;
        Ok(ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    fn exec_update_view_instead_of(
        &mut self,
        stmt: &UpdateStmt,
        vinfo: &ViewInfo,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        let triggers = {
            let schema = self.schema_arc()?;
            compile_triggers_for_target(&schema, &vinfo.name)
        };
        let has_io = triggers
            .iter()
            .any(|t| t.time == TriggerTime::InsteadOf && t.event == TriggerEventKind::Update);
        if !has_io {
            return Err(SqliteError::Sql(format!(
                "cannot modify {} because it is a view",
                vinfo.name
            )));
        }
        if let Some(ref idx_name) = stmt.table.indexed_by {
            return Err(SqliteError::Sql(format!(
                "no such index: {}",
                idx_name.name
            )));
        }
        if stmt.returning.is_some() {
            return Err(SqliteError::Sql(
                "cannot use RETURNING on views".to_string(),
            ));
        }
        if self.active_triggers.is_empty()
            && (stmt
                .from
                .as_ref()
                .is_some_and(xdb_sql::from_contains_raise)
                || stmt
                    .where_clause
                    .as_ref()
                    .is_some_and(xdb_sql::expr_contains_raise)
                || stmt
                    .set
                    .iter()
                    .any(|sc| xdb_sql::expr_contains_raise(&sc.expr))
                || stmt.with.as_ref().is_some_and(|w| {
                    w.ctes
                        .iter()
                        .any(|c| xdb_sql::select_contains_raise(&c.select))
                }))
        {
            return Err(SqliteError::Sql(
                "RAISE() may only be used within a trigger-program".to_string(),
            ));
        }
        let stmt_conflict = self.active_conflict_override.or(stmt.or_conflict);
        let qopts = self.query_options(dialect);
        let view_db = self.live_view()?;
        let alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();
        let sql = format!("SELECT * FROM \"{}\"", vinfo.name.replace('"', "\"\""));
        let sel_stmt = parse_statement(&sql, &ParseOptions::default())
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        let Statement::Select(sel) = sel_stmt else {
            return Err(SqliteError::Sql("expected SELECT".into()));
        };
        let (raw_cols, all_rows) =
            self.eval_select_stmt_detailed(&view_db, &sel, &QueryParams::empty(), &qopts)?;
        let scope_cols: Vec<ScopeColumn> = raw_cols
            .iter()
            .map(|c| ScopeColumn {
                table: Some(alias.to_string()),
                name: c.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_rowid_alias: false,
            })
            .collect();
        let mut updated_col_names = Vec::new();
        for sc in &stmt.set {
            for c in &sc.columns {
                if !scope_cols
                    .iter()
                    .any(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                {
                    return Err(SqliteError::Sql(format!("no such column: {}", c.name)));
                }
                updated_col_names.push(c.name.clone());
            }
        }
        let has_matching_io = triggers.iter().any(|t| {
            t.time == TriggerTime::InsteadOf
                && t.event == TriggerEventKind::Update
                && t.matches_update(&updated_col_names)
        });
        if !has_matching_io {
            return Err(SqliteError::Sql(format!(
                "cannot modify {} because it is a view",
                vinfo.name
            )));
        }
        let from_data = if let Some(ref from_src) = stmt.from {
            // Expose rowid/_rowid_/oid for single rowid-table FROM sources (unless shadowed by a CTE).
            let single_from_rowid_alias: Option<(String, Vec<&'static str>)> =
                if let xdb_parser::FromSource::Table(ft) = from_src {
                    let shadowed_by_cte = ft.schema.is_none()
                        && stmt.with.as_ref().is_some_and(|w| {
                            w.ctes
                                .iter()
                                .any(|c| c.name.name.eq_ignore_ascii_case(&ft.name.name))
                        });
                    if !shadowed_by_cte
                        && (ft.schema.is_none()
                            || ft
                                .schema
                                .as_ref()
                                .is_some_and(|s| s.name.eq_ignore_ascii_case("main")))
                    {
                        if let Some(ftbl) = view_db.schema().table(&ft.name.name) {
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
                table: None,
                exclude: None,
                replace: None,
                span: from_src.span(),
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
            let from_sel = SelectStmt {
                with: stmt.with.clone(),
                body: SelectBody::Core(Box::new(SelectCore {
                    distinct: None,
                    columns: cols_select,
                    from: Some(from_src.clone()),
                    where_clause: None,
                    group_by: None,
                    group_by_all: false,
                    having: None,
                    windows: None,
                    from_first: false,
                    span: from_src.span(),
                })),
                order_by: None,
                order_by_all: None,
                limit: None,
                span: from_src.span(),
            };
            let Some((mut ecols, mut erows)) =
                self.eval_select_stmt_detailed_in_dml(&view_db, &from_sel, params, &qopts)?
            else {
                return Ok(self.empty_exec_result());
            };
            if let Some((falias, synth)) = single_from_rowid_alias {
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
        let catalog = crate::sql::live_catalog_for(&view_db, &qopts);
        let sub_runner =
            CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
        let eval_env = EvalEnv::new(&qopts, params, &sub_runner);
        let mut pairs_mem = ConnMemGuard::new(&self.pager);
        let mut pairs: Vec<(Vec<SqlValue>, Vec<SqlValue>)> = Vec::new();
        let mut loop_sql_err: Option<SqlError> = None;

        let eval_view_sets = |old_row: &[SqlValue],
                              scope: &[EvalScope<'_>]|
         -> Result<Vec<SqlValue>, SqlError> {
            let mut new_row = old_row.to_vec();
            for sc in &stmt.set {
                if sc.columns.len() == 1 {
                    let v = eval_expr(&sc.expr, scope, &eval_env)?;
                    if let Some(pos) = scope_cols
                        .iter()
                        .position(|vc| vc.name.eq_ignore_ascii_case(&sc.columns[0].name))
                    {
                        new_row[pos] = apply_affinity(v, scope_cols[pos].affinity);
                    }
                } else if let Expr::Vector { items, .. } = &sc.expr {
                    if items.len() != sc.columns.len() {
                        return Err(SqlError::runtime(format!(
                            "{} columns assigned {} values",
                            sc.columns.len(),
                            items.len()
                        )));
                    }
                    for (c, e) in sc.columns.iter().zip(items.iter()) {
                        let v = eval_expr(e, scope, &eval_env)?;
                        if let Some(pos) = scope_cols
                            .iter()
                            .position(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                        {
                            new_row[pos] = apply_affinity(v, scope_cols[pos].affinity);
                        }
                    }
                } else if let Expr::Subquery { select, .. } = &sc.expr {
                    let (cols, rows) = eval_env.subqueries.run_select(select, scope)?;
                    if cols.len() != sc.columns.len() {
                        return Err(SqlError::runtime(format!(
                            "{} columns assigned {} values",
                            sc.columns.len(),
                            cols.len()
                        )));
                    }
                    let vals = rows
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| vec![SqlValue::Null; sc.columns.len()]);
                    for (c, v) in sc.columns.iter().zip(vals.into_iter()) {
                        if let Some(pos) = scope_cols
                            .iter()
                            .position(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                        {
                            new_row[pos] = apply_affinity(v, scope_cols[pos].affinity);
                        }
                    }
                } else {
                    return Err(SqlError::runtime(
                        "multi-column SET requires row value".to_string(),
                    ));
                }
            }
            Ok(new_row)
        };

        if let Some((ref extra_cols, ref extra_rows)) = from_data {
            let mut combined_cols = scope_cols.clone();
            combined_cols.extend(extra_cols.iter().cloned());
            'outer_v: for old_row in all_rows {
                for erow in extra_rows {
                    let mut combined_vals = old_row.clone();
                    combined_vals.extend_from_slice(erow);
                    let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                    let scope_storage: [EvalScope<'_>; 2];
                    let scope_single: [EvalScope<'_>; 1];
                    let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                        scope_storage = [ts, EvalScope::simple(&combined_cols, &combined_vals)];
                        &scope_storage
                    } else {
                        scope_single = [EvalScope::simple(&combined_cols, &combined_vals)];
                        &scope_single
                    };
                    if let Some(ref we) = stmt.where_clause {
                        let cond = match eval_expr(we, scope, &eval_env) {
                            Ok(v) => v,
                            Err(e) => {
                                loop_sql_err = Some(e);
                                break 'outer_v;
                            }
                        };
                        if !boolean_value(&cond, false) {
                            continue;
                        }
                    }
                    let new_row = match eval_view_sets(&old_row, scope) {
                        Ok(r) => r,
                        Err(e) => {
                            loop_sql_err = Some(e);
                            break 'outer_v;
                        }
                    };
                    pairs_mem.reserve(
                        estimate_sql_row_bytes(&old_row)
                            .saturating_add(estimate_sql_row_bytes(&new_row)),
                    )?;
                    pairs.push((old_row, new_row));
                    break;
                }
            }
        } else {
            for old_row in all_rows {
                let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                let scope_storage: [EvalScope<'_>; 2];
                let scope_single: [EvalScope<'_>; 1];
                let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                    scope_storage = [ts, EvalScope::simple(&scope_cols, &old_row)];
                    &scope_storage
                } else {
                    scope_single = [EvalScope::simple(&scope_cols, &old_row)];
                    &scope_single
                };
                if let Some(ref we) = stmt.where_clause {
                    let cond = match eval_expr(we, scope, &eval_env) {
                        Ok(v) => v,
                        Err(e) => {
                            loop_sql_err = Some(e);
                            break;
                        }
                    };
                    if !boolean_value(&cond, false) {
                        continue;
                    }
                }
                let new_row = match eval_view_sets(&old_row, scope) {
                    Ok(r) => r,
                    Err(e) => {
                        loop_sql_err = Some(e);
                        break;
                    }
                };
                pairs_mem.reserve(
                    estimate_sql_row_bytes(&old_row)
                        .saturating_add(estimate_sql_row_bytes(&new_row)),
                )?;
                pairs.push((old_row, new_row));
            }
        }
        if pairs.is_empty() && loop_sql_err.is_none() {
            let mut val_cols = scope_cols.clone();
            if let Some((ref extra_cols, _)) = from_data {
                val_cols.extend(extra_cols.iter().cloned());
            }
            let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
            let scope_storage: [EvalScope<'_>; 2];
            let scope_single: [EvalScope<'_>; 1];
            let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                scope_storage = [ts, EvalScope::simple(&val_cols, &[])];
                &scope_storage
            } else {
                scope_single = [EvalScope::simple(&val_cols, &[])];
                &scope_single
            };
            if let Some(ref we) = stmt.where_clause {
                validate_dml_expr_with_catalog(we, scope, &catalog, stmt.with.as_ref(), &qopts)?;
            }
            for sc in &stmt.set {
                validate_dml_expr_with_catalog(
                    &sc.expr,
                    scope,
                    &catalog,
                    stmt.with.as_ref(),
                    &qopts,
                )?;
            }
        }
        drop(eval_env);
        drop(sub_runner);
        drop(view_db);
        let mut pending_loop_sql_err: Option<SqlError> = None;
        if let Some(e) = loop_sql_err {
            if matches!(
                e,
                SqlError::Raise {
                    action: RaiseAction::Ignore | RaiseAction::Fail,
                    ..
                }
            ) {
                pending_loop_sql_err = Some(e);
            } else {
                let _ = self.handle_trigger_sql_err(e)?;
            }
        }

        for (old_row, new_row) in pairs {
            let tctx = Arc::new(TriggerRowCtx::for_view(
                &raw_cols,
                Some(&old_row),
                Some(&new_row),
            ));
            if let Err(e) = self.fire_triggers(
                &triggers,
                TriggerTime::InsteadOf,
                TriggerEventKind::Update,
                Some(&updated_col_names),
                &tctx,
                stmt_conflict,
                dialect,
            ) {
                self.changes = 0;
                return Err(e);
            }
        }
        if let Some(e) = pending_loop_sql_err {
            if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                self.trigger_ignore_signaled = true;
            }
        }
        self.changes = 0;
        Ok(ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    fn exec_delete_view_instead_of(
        &mut self,
        stmt: &DeleteStmt,
        vinfo: &ViewInfo,
        params: &QueryParams,
        dialect: Dialect,
    ) -> SqliteResult<ExecResult> {
        let triggers = {
            let schema = self.schema_arc()?;
            compile_triggers_for_target(&schema, &vinfo.name)
        };
        let has_io = triggers
            .iter()
            .any(|t| t.time == TriggerTime::InsteadOf && t.event == TriggerEventKind::Delete);
        if !has_io {
            return Err(SqliteError::Sql(format!(
                "cannot modify {} because it is a view",
                vinfo.name
            )));
        }
        if let Some(ref idx_name) = stmt.table.indexed_by {
            return Err(SqliteError::Sql(format!(
                "no such index: {}",
                idx_name.name
            )));
        }
        if stmt.returning.is_some() {
            return Err(SqliteError::Sql(
                "cannot use RETURNING on views".to_string(),
            ));
        }
        if self.active_triggers.is_empty()
            && (stmt
                .where_clause
                .as_ref()
                .is_some_and(xdb_sql::expr_contains_raise)
                || stmt.with.as_ref().is_some_and(|w| {
                    w.ctes
                        .iter()
                        .any(|c| xdb_sql::select_contains_raise(&c.select))
                }))
        {
            return Err(SqliteError::Sql(
                "RAISE() may only be used within a trigger-program".to_string(),
            ));
        }
        let stmt_conflict = self.active_conflict_override;
        let qopts = self.query_options(dialect);
        let view_db = self.live_view()?;
        let alias = stmt
            .table
            .alias
            .as_ref()
            .unwrap_or(&stmt.table.name)
            .name
            .as_str();
        let sql = format!("SELECT * FROM \"{}\"", vinfo.name.replace('"', "\"\""));
        let sel_stmt = parse_statement(&sql, &ParseOptions::default())
            .map_err(|e| SqliteError::Sql(e.to_string()))?;
        let Statement::Select(sel) = sel_stmt else {
            return Err(SqliteError::Sql("expected SELECT".into()));
        };
        let (raw_cols, all_rows) =
            self.eval_select_stmt_detailed(&view_db, &sel, &QueryParams::empty(), &qopts)?;
        let scope_cols: Vec<ScopeColumn> = raw_cols
            .iter()
            .map(|c| ScopeColumn {
                table: Some(alias.to_string()),
                name: c.name.clone(),
                affinity: c.affinity,
                collation: c.collation.clone(),
                is_rowid_alias: false,
            })
            .collect();
        let catalog = crate::sql::live_catalog_for(&view_db, &qopts);
        let sub_runner =
            CatalogSubqueryRunner::with_ctes(&catalog, &qopts, params, stmt.with.as_ref())?;
        let eval_env = EvalEnv::new(&qopts, params, &sub_runner);
        let mut del_mem = ConnMemGuard::new(&self.pager);
        let mut to_del: Vec<Vec<SqlValue>> = Vec::new();
        let had_all_rows = !all_rows.is_empty();
        let mut loop_sql_err: Option<SqlError> = None;
        for old_row in all_rows {
            if let Some(ref we) = stmt.where_clause {
                let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                let scope_storage: [EvalScope<'_>; 2];
                let scope_single: [EvalScope<'_>; 1];
                let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                    scope_storage = [ts, EvalScope::simple(&scope_cols, &old_row)];
                    &scope_storage
                } else {
                    scope_single = [EvalScope::simple(&scope_cols, &old_row)];
                    &scope_single
                };
                let cond = match eval_expr(we, scope, &eval_env) {
                    Ok(v) => v,
                    Err(e) => {
                        loop_sql_err = Some(e);
                        break;
                    }
                };
                if !boolean_value(&cond, false) {
                    continue;
                }
            }
            del_mem.reserve(estimate_sql_row_bytes(&old_row))?;
            to_del.push(old_row);
        }
        if !had_all_rows && loop_sql_err.is_none() {
            if let Some(ref we) = stmt.where_clause {
                let t_scope_opt = self.trigger_ctx.as_ref().map(|t| t.as_eval_scope());
                let scope_storage: [EvalScope<'_>; 2];
                let scope_single: [EvalScope<'_>; 1];
                let scope: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                    scope_storage = [ts, EvalScope::simple(&scope_cols, &[])];
                    &scope_storage
                } else {
                    scope_single = [EvalScope::simple(&scope_cols, &[])];
                    &scope_single
                };
                validate_dml_expr_with_catalog(we, scope, &catalog, stmt.with.as_ref(), &qopts)?;
            }
        }
        drop(eval_env);
        drop(sub_runner);
        drop(view_db);
        let mut pending_loop_sql_err: Option<SqlError> = None;
        if let Some(e) = loop_sql_err {
            if matches!(
                e,
                SqlError::Raise {
                    action: RaiseAction::Ignore | RaiseAction::Fail,
                    ..
                }
            ) {
                pending_loop_sql_err = Some(e);
            } else {
                let _ = self.handle_trigger_sql_err(e)?;
            }
        }

        for old_row in to_del {
            let tctx = Arc::new(TriggerRowCtx::for_view(&raw_cols, Some(&old_row), None));
            if let Err(e) = self.fire_triggers(
                &triggers,
                TriggerTime::InsteadOf,
                TriggerEventKind::Delete,
                None,
                &tctx,
                stmt_conflict,
                dialect,
            ) {
                self.changes = 0;
                return Err(e);
            }
        }
        if let Some(e) = pending_loop_sql_err {
            if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                self.trigger_ignore_signaled = true;
            }
        }
        self.changes = 0;
        Ok(ExecResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: self.last_insert_rowid,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fire_triggers(
        &mut self,
        triggers: &[Arc<CompiledTrigger>],
        time: TriggerTime,
        event: TriggerEventKind,
        updated_cols: Option<&[String]>,
        row_ctx: &Arc<TriggerRowCtx>,
        outer_conflict: Option<ConflictAction>,
        dialect: Dialect,
    ) -> SqliteResult<TriggerOutcome> {
        if triggers.is_empty() {
            return Ok(TriggerOutcome::Continue);
        }
        let empty_params = QueryParams::empty();
        for tr in triggers {
            if tr.time != time || tr.event != event {
                continue;
            }
            if event == TriggerEventKind::Update {
                if let Some(ucols) = updated_cols {
                    if !tr.matches_update(ucols) {
                        continue;
                    }
                }
            }
            if !self.recursive_triggers
                && self
                    .active_triggers
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&tr.name))
            {
                continue;
            }
            if self.active_triggers.len() >= MAX_TRIGGER_DEPTH {
                return Err(SqliteError::Sql(
                    "too many levels of trigger recursion".to_string(),
                ));
            }
            if !self.pager.is_noop_meter() {
                self.pager.meter().charge_steps(1)?;
            }

            self.active_triggers.push(tr.name.clone());
            let prev_ctx = self.trigger_ctx.replace(Arc::clone(row_ctx));
            let prev_changes = self.changes;
            let prev_last_insert_rowid = self.last_insert_rowid;
            let prev_ignore = self.trigger_ignore_signaled;
            let prev_conflict = self.active_conflict_override;
            self.trigger_ignore_signaled = false;
            self.active_conflict_override = prev_conflict.or(outer_conflict);

            let step_outcome: SqliteResult<TriggerOutcome> = (|| {
                // Evaluate optional `WHEN` expression against `OLD.*`/`NEW.*`.
                if let Some(ref when_expr) = tr.when {
                    let qopts = self.query_options(dialect);
                    let view = self.live_view()?;
                    let catalog = crate::sql::live_catalog_for(&view, &qopts);
                    let sub_runner = CatalogSubqueryRunner::new(&catalog, &qopts, &empty_params);
                    let env = EvalEnv::new(&qopts, &empty_params, &sub_runner);
                    let scope = [row_ctx.as_eval_scope()];
                    let res = eval_expr(when_expr, &scope, &env);
                    drop(env);
                    drop(sub_runner);
                    drop(view);
                    match res {
                        Ok(v) => {
                            if !boolean_value(&v, false) {
                                return Ok(TriggerOutcome::Continue);
                            }
                        }
                        Err(e) => {
                            return self.handle_trigger_sql_err(e);
                        }
                    }
                }

                // Execute trigger body steps in order.
                for step in &tr.body {
                    self.trigger_ignore_signaled = false;
                    match step {
                        TriggerStep::Select(sel) => {
                            let qopts = self.query_options(dialect);
                            let view = self.live_view()?;
                            let catalog = crate::sql::live_catalog_for(&view, &qopts);
                            let scope = [row_ctx.as_eval_scope()];
                            let res = execute_select_stmt_with_scopes(
                                &catalog,
                                sel,
                                &empty_params,
                                &qopts,
                                &scope,
                            );
                            drop(view);
                            if let Err(e) = res {
                                if self.handle_trigger_sql_err(e)? == TriggerOutcome::Ignore {
                                    return Ok(TriggerOutcome::Ignore);
                                }
                            }
                        }
                        TriggerStep::Insert(ins) => {
                            self.exec_insert(ins, &empty_params, dialect)?;
                            if self.trigger_ignore_signaled {
                                return Ok(TriggerOutcome::Ignore);
                            }
                        }
                        TriggerStep::Update(upd) => {
                            self.exec_update(upd, &empty_params, dialect)?;
                            if self.trigger_ignore_signaled {
                                return Ok(TriggerOutcome::Ignore);
                            }
                        }
                        TriggerStep::Delete(del) => {
                            self.exec_delete(del, &empty_params, dialect)?;
                            if self.trigger_ignore_signaled {
                                return Ok(TriggerOutcome::Ignore);
                            }
                        }
                    }
                }
                Ok(TriggerOutcome::Continue)
            })();

            self.active_triggers.pop();
            self.trigger_ctx = prev_ctx;
            self.changes = prev_changes;
            self.last_insert_rowid = prev_last_insert_rowid;
            self.trigger_ignore_signaled = prev_ignore;
            self.active_conflict_override = prev_conflict;

            if step_outcome? == TriggerOutcome::Ignore {
                return Ok(TriggerOutcome::Ignore);
            }
        }
        Ok(TriggerOutcome::Continue)
    }

    // ───────────────────────────── Foreign Keys ─────────────────────────────

    /// Adjust SQLite's three foreign-key violation counters:
    /// - `stmt_imm_fk` (`p->nFkConstraint`): immediate FKs when `defer_foreign_keys` is OFF
    /// - `txn_def_imm_fk` (`db->nDeferredImmCons`): immediate FKs deferred by `PRAGMA defer_foreign_keys = ON`
    /// - `txn_def_fk` (`db->nDeferredCons`): `DEFERRABLE INITIALLY DEFERRED` FKs
    ///
    /// Counters are strictly segregated (`DEFERRABLE` never spills into immediate, and vice versa)
    /// and clamped at `>= 0` so resolving a pre-existing orphan never masks a fresh violation.
    fn adjust_fk_counter(&mut self, fk_deferred: bool, delta: i64) {
        if delta == 0 {
            return;
        }
        let in_tx = self.pager.in_transaction();
        if fk_deferred && in_tx {
            if delta > 0 {
                self.txn_def_fk = self.txn_def_fk.saturating_add(delta);
            } else {
                self.txn_def_fk = self.txn_def_fk.saturating_sub(delta.saturating_neg());
            }
        } else if !fk_deferred && self.defer_foreign_keys && in_tx {
            if delta > 0 {
                self.txn_def_imm_fk = self.txn_def_imm_fk.saturating_add(delta);
            } else {
                self.txn_def_imm_fk = self.txn_def_imm_fk.saturating_sub(delta.saturating_neg());
            }
        } else if delta > 0 {
            self.stmt_imm_fk = self.stmt_imm_fk.saturating_add(delta);
        } else {
            let mut rem = delta.saturating_neg();
            let take = rem.min(self.stmt_imm_fk);
            self.stmt_imm_fk -= take;
            rem -= take;
            // If `PRAGMA defer_foreign_keys` was toggled ON -> OFF mid-transaction, resolving an
            // immediate FK violation can still drain `txn_def_imm_fk`, never `txn_def_fk`.
            if rem > 0 && !fk_deferred && in_tx && self.txn_def_imm_fk > 0 {
                let take2 = rem.min(self.txn_def_imm_fk);
                self.txn_def_imm_fk -= take2;
            }
        }
    }

    /// Decrement pending deferred FK counters when dropping `table` (`sqlite3FkDropTable`).
    /// Treats already-dropped parent tables as unmatched instead of failing with `no such table`.
    fn fk_drop_child_row(
        &mut self,
        table: &TableInfo,
        row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        if !self.foreign_keys || !self.has_pending_deferred_fk() {
            return Ok(());
        }
        let view = self.live_view()?;
        let schema = view.schema();
        let mut dec_def = 0i64;
        let mut dec_def_imm = 0i64;
        for fk in &table.definition.foreign_keys {
            if fk.deferred && self.txn_def_fk == 0 {
                continue;
            }
            if !fk.deferred && self.txn_def_imm_fk == 0 {
                continue;
            }
            let mut cvals = Vec::with_capacity(fk.from.len());
            let mut any_null = false;
            for &fcid in &fk.from {
                let v = row.get(fcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                cvals.push(v);
            }
            if any_null {
                continue;
            }
            let unmatched = match schema.table(&fk.table) {
                None => true,
                Some(parent) => match resolve_parent_key_cids(&table.name, parent, fk) {
                    Ok(pcids) => !parent_has_key(&self.pager, &view, parent, &pcids, &cvals, qopts)?,
                    Err(_) => true,
                },
            };
            if unmatched {
                if fk.deferred {
                    dec_def += 1;
                } else {
                    dec_def_imm += 1;
                }
            }
        }
        drop(view);
        self.txn_def_fk = self.txn_def_fk.saturating_sub(dec_def);
        self.txn_def_imm_fk = self.txn_def_imm_fk.saturating_sub(dec_def_imm);
        Ok(())
    }

    /// Child-side outgoing FK check for a single row (`delta = +1` after insert/update,
    /// `delta = -1` before delete/update). When `mask` is `Some`, checks FKs touching any
    /// modified child column (or generated column), PLUS self-referencing FKs whose parent
    /// key columns were modified!
    fn fk_check_child_row(
        &mut self,
        table: &TableInfo,
        row: &[SqlValue],
        mask: Option<&[bool]>,
        delta: i64,
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        if !self.foreign_keys || table.definition.foreign_keys.is_empty() {
            return Ok(());
        }
        let view = self.live_view()?;
        let schema = view.schema();
        let mut deltas: Vec<(bool, i64)> = Vec::new();
        for fk in &table.definition.foreign_keys {
            if let Some(m) = mask {
                let is_self_ref = fk.table.eq_ignore_ascii_case(&table.name);
                let child_touched = fk.from.iter().any(|&fcid| {
                    m.get(fcid).copied().unwrap_or(false)
                        || table
                            .columns
                            .get(fcid)
                            .is_some_and(|c| c.generated.is_some())
                });
                let self_parent_touched = is_self_ref && {
                    if let Ok(pcids) = resolve_parent_key_cids(&table.name, table, fk) {
                        pcids.iter().any(|&pcid| {
                            m.get(pcid).copied().unwrap_or(false)
                                || table
                                    .columns
                                    .get(pcid)
                                    .is_some_and(|c| c.generated.is_some())
                        })
                    } else {
                        true
                    }
                };
                if !child_touched && !self_parent_touched {
                    continue;
                }
            }
            let mut cvals = Vec::with_capacity(fk.from.len());
            let mut any_null = false;
            for &fcid in &fk.from {
                let v = row.get(fcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                cvals.push(v);
            }
            // Per SQLite: on INSERT/UPDATE (`delta > 0`), validate parent table existence and
            // unique-key compatibility EVEN when the child FK column value is NULL!
            if any_null && delta < 0 {
                continue;
            }
            let Some(parent) = schema.table(&fk.table) else {
                let msg = if schema.view(&fk.table).is_some()
                    || schema.virtual_table(&fk.table).is_some()
                {
                    format!(
                        "foreign key mismatch - \"{}\" referencing \"{}\"",
                        table.name, fk.table
                    )
                } else {
                    format!("no such table: main.{}", fk.table)
                };
                drop(view);
                return Err(SqliteError::Sql(msg));
            };
            let pcids = resolve_parent_key_cids(&table.name, parent, fk)?;
            if any_null {
                continue;
            }
            if !parent_has_key(&self.pager, &view, parent, &pcids, &cvals, qopts)? {
                deltas.push((fk.deferred, delta));
            }
        }
        drop(view);
        for (is_def, d) in deltas {
            self.adjust_fk_counter(is_def, d);
        }
        Ok(())
    }

    /// Parent-side check run BEFORE inserting `new_row` into `parent`: any existing child rows
    /// already referencing `new_row`'s parent key are now satisfied, so decrement FK counters.
    fn fk_check_parent_insert(
        &mut self,
        parent: &TableInfo,
        new_row: &[SqlValue],
        qopts: &QueryOptions,
    ) -> SqliteResult<()> {
        if !self.foreign_keys || !self.has_pending_fk() {
            return Ok(());
        }
        let view = self.live_view()?;
        let schema = view.schema();
        let incoming = incoming_fks_for(schema, &parent.name);
        if incoming.is_empty() {
            return Ok(());
        }
        let mut deltas: Vec<(bool, i64)> = Vec::new();
        for (child_name, fk) in incoming {
            if fk.deferred {
                if self.txn_def_fk == 0 {
                    continue;
                }
            } else if self.stmt_imm_fk == 0 && self.txn_def_imm_fk == 0 {
                continue;
            }
            let Some(child_tbl) = schema.table(&child_name) else {
                continue;
            };
            let pcids = resolve_parent_key_cids(&child_name, parent, fk)?;
            let mut new_pvals = Vec::with_capacity(pcids.len());
            let mut any_null = false;
            for &pcid in &pcids {
                let v = new_row.get(pcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                new_pvals.push(v);
            }
            if any_null {
                continue;
            }
            let cnt = count_child_refs_matching_parent_key(
                &view,
                child_tbl,
                fk,
                parent,
                &pcids,
                &new_pvals,
                None,
                None,
                qopts,
            )?;
            if cnt > 0 {
                deltas.push((fk.deferred, -cnt));
            }
        }
        drop(view);
        for (is_def, d) in deltas {
            self.adjust_fk_counter(is_def, d);
        }
        Ok(())
    }

    fn apply_incoming_fk_delete_actions(
        &mut self,
        parent: &TableInfo,
        old_row: &[SqlValue],
        dialect: Dialect,
    ) -> SqliteResult<()> {
        self.apply_incoming_fk_delete_actions_filtered(parent, old_row, dialect, false)
    }

    fn apply_incoming_fk_delete_actions_filtered(
        &mut self,
        parent: &TableInfo,
        old_row: &[SqlValue],
        dialect: Dialect,
        skip_self: bool,
    ) -> SqliteResult<()> {
        if !self.foreign_keys {
            return Ok(());
        }
        if self.fk_cascade_depth >= MAX_FK_CASCADE_DEPTH {
            return Err(SqliteError::Sql(
                "too many levels of foreign key recursion".to_string(),
            ));
        }
        let incoming: Vec<(String, xdb_parser::ForeignKeyDefinition)> = {
            let schema = self.schema_arc()?;
            incoming_fks_for(&schema, &parent.name)
                .into_iter()
                .filter(|(n, _)| !skip_self || !n.eq_ignore_ascii_case(&parent.name))
                .map(|(n, fk)| (n, fk.clone()))
                .collect()
        };
        if incoming.is_empty() {
            return Ok(());
        }
        let colls = self.cached_collations.1.clone();
        for (child_name, fk) in incoming {
            let pcids = resolve_parent_key_cids(&child_name, parent, &fk)?;
            let mut old_pvals = Vec::with_capacity(pcids.len());
            let mut any_null = false;
            for &pcid in &pcids {
                let v = old_row.get(pcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                old_pvals.push(v);
            }
            if any_null {
                continue;
            }
            let (child_tbl, matches) = self.find_matching_fk_child_rows(
                &child_name,
                &fk,
                parent,
                &pcids,
                &old_pvals,
                &colls,
            )?;
            if matches.is_empty() {
                continue;
            }
            let cnt = matches.len() as i64;
            match fk.on_delete {
                ForeignKeyAction::Restrict => {
                    // Per SQLite: DEFERRABLE INITIALLY DEFERRED does NOT defer RESTRICT on its
                    // own, whereas `PRAGMA defer_foreign_keys = ON` inside a transaction DOES;
                    // record into the same bucket (`fk.deferred`) that `fk_check_parent_insert`
                    // decrements if the parent key is later restored before COMMIT.
                    if self.defer_foreign_keys && self.pager.in_transaction() {
                        self.adjust_fk_counter(fk.deferred, cnt);
                    } else {
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            "FOREIGN KEY constraint failed".to_string(),
                        ));
                    }
                }
                ForeignKeyAction::NoAction => {
                    self.adjust_fk_counter(fk.deferred, cnt);
                }
                ForeignKeyAction::Cascade => {
                    self.adjust_fk_counter(fk.deferred, cnt);
                    self.exec_fk_child_delete(&child_tbl, &matches, dialect)?;
                }
                ForeignKeyAction::SetNull => {
                    self.adjust_fk_counter(fk.deferred, cnt);
                    let nulls = vec![SqlValue::Null; fk.from.len()];
                    self.exec_fk_child_update(&child_tbl, &fk.from, &nulls, &matches, dialect)?;
                }
                ForeignKeyAction::SetDefault => {
                    self.adjust_fk_counter(fk.deferred, cnt);
                    let mut qopts = self.query_options(dialect);
                    qopts.in_trigger = false;
                    let empty_params = QueryParams::empty();
                    let mut dflts = Vec::with_capacity(fk.from.len());
                    for &fcid in &fk.from {
                        let v = if let Some(ref expr) =
                            child_tbl.definition.columns[fcid].default_expr
                        {
                            eval_standalone_expr(expr, &empty_params, &qopts)?
                        } else {
                            child_tbl.columns[fcid].default_value.clone()
                        };
                        dflts.push(v);
                    }
                    self.exec_fk_child_update(&child_tbl, &fk.from, &dflts, &matches, dialect)?;
                }
            }
        }
        Ok(())
    }

    fn apply_incoming_fk_update_actions(
        &mut self,
        parent: &TableInfo,
        old_row: &[SqlValue],
        new_row: &[SqlValue],
        new_rid: Option<i64>,
        dialect: Dialect,
    ) -> SqliteResult<()> {
        if !self.foreign_keys {
            return Ok(());
        }
        if self.fk_cascade_depth >= MAX_FK_CASCADE_DEPTH {
            return Err(SqliteError::Sql(
                "too many levels of foreign key recursion".to_string(),
            ));
        }
        let incoming: Vec<(String, xdb_parser::ForeignKeyDefinition)> = {
            let schema = self.schema_arc()?;
            incoming_fks_for(&schema, &parent.name)
                .into_iter()
                .map(|(n, fk)| (n, fk.clone()))
                .collect()
        };
        if incoming.is_empty() {
            return Ok(());
        }
        let qopts = self.query_options(dialect);
        let colls = self.cached_collations.1.clone();
        let new_wr_pk: Option<Vec<SqlValue>> = if parent.without_rowid {
            parent
                .indexes
                .iter()
                .find(|ix| ix.without_rowid_primary_key)
                .map(|ix| {
                    ix.columns[..ix.key_count]
                        .iter()
                        .map(|ic| new_row[ic.cid as usize].clone())
                        .collect()
                })
        } else {
            None
        };
        let wr_pk_cids: Vec<(usize, String)> = if parent.without_rowid {
            parent
                .indexes
                .iter()
                .find(|ix| ix.without_rowid_primary_key)
                .map(|ix| {
                    ix.columns[..ix.key_count]
                        .iter()
                        .filter(|ic| ic.cid >= 0)
                        .map(|ic| (ic.cid as usize, ic.collation.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        for (child_name, fk) in incoming {
            // Check whether the referenced parent columns actually changed BEFORE requiring
            // unique-index validation so updating an unrelated parent column does not trip on
            // a mismatched child FK referencing a different column.
            let raw_pcids: Option<Vec<usize>> = match fk.to.as_deref() {
                None | Some([]) => {
                    if !parent.primary_key.is_empty()
                        && parent.primary_key.iter().all(|&cid| cid >= 0)
                    {
                        Some(parent.primary_key.iter().map(|&cid| cid as usize).collect())
                    } else {
                        None
                    }
                }
                Some(to_cols) => to_cols
                    .iter()
                    .map(|col_name| {
                        parent
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    })
                    .collect(),
            };
            if let Some(ref rpcs) = raw_pcids {
                if parent_key_unchanged(parent, rpcs, old_row, new_row, &colls)? {
                    continue;
                }
            }
            let pcids = resolve_parent_key_cids(&child_name, parent, &fk)?;
            if parent_key_unchanged(parent, &pcids, old_row, new_row, &colls)? {
                continue;
            }
            let is_self_ref = child_name.eq_ignore_ascii_case(&parent.name);
            let new_pvals: Vec<SqlValue> = pcids
                .iter()
                .map(|&pcid| new_row.get(pcid).cloned().unwrap_or(SqlValue::Null))
                .collect();
            // 1. If `new_pvals` has no NULLs, any existing children waiting for `new_pvals` are satisfied.
            if !new_pvals.iter().any(SqlValue::is_null) && self.has_pending_fk() {
                let view = self.live_view()?;
                if let Some(child_tbl) = view.schema().table(&child_name) {
                    let resolved_cnt = count_child_refs_matching_parent_key(
                        &view,
                        child_tbl,
                        &fk,
                        parent,
                        &pcids,
                        &new_pvals,
                        if is_self_ref { new_rid } else { None },
                        if is_self_ref {
                            new_wr_pk.as_deref()
                        } else {
                            None
                        },
                        &qopts,
                    )?;
                    drop(view);
                    if resolved_cnt > 0 {
                        self.adjust_fk_counter(fk.deferred, -resolved_cnt);
                    }
                }
            }
            // 2. Check children referencing `old_pvals`.
            let mut old_pvals = Vec::with_capacity(pcids.len());
            let mut any_null = false;
            for &pcid in &pcids {
                let v = old_row.get(pcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                old_pvals.push(v);
            }
            if any_null {
                continue;
            }
            let (child_tbl, matches) = self.find_matching_fk_child_rows(
                &child_name,
                &fk,
                parent,
                &pcids,
                &old_pvals,
                &colls,
            )?;
            if matches.is_empty() {
                continue;
            }
            // For self-referencing FKs, the updated row itself was already accounted for by
            // `fk_check_child_row` (+1 on `new_row`), so don't double-count it in `cnt`,
            // while still passing it in `matches` so CASCADE/SET NULL/SET DEFAULT updates it!
            let non_self_cnt = if is_self_ref {
                let mut cnt = 0i64;
                for (mrid, mrow) in &matches {
                    let is_self = if !parent.without_rowid {
                        *mrid == new_rid
                    } else if let Some(ref npk) = new_wr_pk {
                        if wr_pk_cids.len() == npk.len() {
                            let mut all_eq = true;
                            for (i, (cid, cname)) in wr_pk_cids.iter().enumerate() {
                                let coll = colls
                                    .find(cname)
                                    .ok_or_else(|| SqliteError::UnknownCollation(cname.clone()))?;
                                if mem_compare(&mrow[*cid], &npk[i], Some(coll))
                                    != std::cmp::Ordering::Equal
                                {
                                    all_eq = false;
                                    break;
                                }
                            }
                            all_eq
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !is_self {
                        cnt += 1;
                    }
                }
                cnt
            } else {
                matches.len() as i64
            };
            match fk.on_update {
                ForeignKeyAction::Restrict => {
                    if self.defer_foreign_keys && self.pager.in_transaction() {
                        if non_self_cnt > 0 {
                            self.adjust_fk_counter(fk.deferred, non_self_cnt);
                        }
                    } else {
                        return Err(self.constraint_err(
                            ConflictAction::Abort,
                            "FOREIGN KEY constraint failed".to_string(),
                        ));
                    }
                }
                ForeignKeyAction::NoAction => {
                    if non_self_cnt > 0 {
                        self.adjust_fk_counter(fk.deferred, non_self_cnt);
                    }
                }
                ForeignKeyAction::Cascade => {
                    if non_self_cnt > 0 {
                        self.adjust_fk_counter(fk.deferred, non_self_cnt);
                    }
                    self.exec_fk_child_update(&child_tbl, &fk.from, &new_pvals, &matches, dialect)?;
                }
                ForeignKeyAction::SetNull => {
                    if non_self_cnt > 0 {
                        self.adjust_fk_counter(fk.deferred, non_self_cnt);
                    }
                    let nulls = vec![SqlValue::Null; fk.from.len()];
                    self.exec_fk_child_update(&child_tbl, &fk.from, &nulls, &matches, dialect)?;
                }
                ForeignKeyAction::SetDefault => {
                    if non_self_cnt > 0 {
                        self.adjust_fk_counter(fk.deferred, non_self_cnt);
                    }
                    let mut dflt_qopts = self.query_options(dialect);
                    dflt_qopts.in_trigger = false;
                    let empty_params = QueryParams::empty();
                    let mut dflts = Vec::with_capacity(fk.from.len());
                    for &fcid in &fk.from {
                        let v = if let Some(ref expr) =
                            child_tbl.definition.columns[fcid].default_expr
                        {
                            eval_standalone_expr(expr, &empty_params, &dflt_qopts)?
                        } else {
                            child_tbl.columns[fcid].default_value.clone()
                        };
                        dflts.push(v);
                    }
                    self.exec_fk_child_update(&child_tbl, &fk.from, &dflts, &matches, dialect)?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn find_matching_fk_child_rows(
        &self,
        child_name: &str,
        fk: &xdb_parser::ForeignKeyDefinition,
        parent: &TableInfo,
        pcids: &[usize],
        parent_vals: &[SqlValue],
        colls: &CollationRegistry,
    ) -> SqliteResult<(Arc<TableInfo>, Vec<(Option<i64>, Vec<SqlValue>)>)> {
        let view = self.live_view()?;
        let Some(child) = view.schema().table(child_name).cloned() else {
            return Err(SqliteError::Sql(format!("no such table: {child_name}")));
        };
        let child_vplan = if fk.from.iter().any(|&c| {
            child
                .columns
                .get(c)
                .is_some_and(|col| col.hidden == ColumnHiddenKind::Virtual)
        }) {
            Some(
                crate::sql::VirtualColumnPlan::build_for(&child, Some(&fk.from))
                    .map_err(SqliteError::from)?,
            )
        } else {
            None
        };
        let mut virt_qopts = self.query_options(self.dialect);
        virt_qopts.in_trigger = false;
        let reader = view.table(&child.name)?;
        let stored_cids: Vec<usize> = child
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let mut cur = reader.cursor(Some(&scan_opts))?;
        let mut matched_mem = ConnMemGuard::new(&self.pager);
        let mut matched = Vec::new();
        while cur.next()? {
            if !self.pager.is_noop_meter() {
                self.pager.meter().charge_steps(1)?;
            }
            let rid = if child.without_rowid {
                None
            } else {
                Some(cur.rowid()?)
            };
            let raw = cur.row()?;
            let mut full = vec![SqlValue::Null; child.columns.len()];
            for (&cid, val) in stored_cids.iter().zip(raw) {
                full[cid] = val;
            }
            if let Some(ref vp) = child_vplan {
                vp.evaluate(&mut full, &virt_qopts)
                    .map_err(SqliteError::from)?;
            }
            let mut cvals = Vec::with_capacity(fk.from.len());
            let mut any_null = false;
            for &fcid in &fk.from {
                let v = full.get(fcid).cloned().unwrap_or(SqlValue::Null);
                if v.is_null() {
                    any_null = true;
                    break;
                }
                cvals.push(v);
            }
            if any_null {
                continue;
            }
            if fk_keys_match(parent, pcids, &cvals, parent_vals, colls)? {
                matched_mem.reserve(estimate_sql_row_bytes(&full).saturating_add(24))?;
                matched.push((rid, full));
            }
        }
        Ok((child, matched))
    }

    fn build_fk_row_where_clause(
        child: &TableInfo,
        rid: Option<i64>,
        row: &[SqlValue],
        param_start: usize,
        out_params: &mut Vec<SqlValue>,
    ) -> String {
        if !child.without_rowid {
            if let Some(r) = rid {
                if let Some(ipk) = child.ipk_column {
                    out_params.push(SqlValue::Int(r));
                    let cname = child.columns[ipk].name.replace('"', "\"\"");
                    return format!("\"{cname}\" = ?{param_start}");
                }
                for alias in ["_rowid_", "rowid", "oid"] {
                    if !child
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(alias))
                    {
                        out_params.push(SqlValue::Int(r));
                        return format!("{alias} = ?{param_start}");
                    }
                }
            }
        }
        // WITHOUT ROWID: use PK index columns with their explicit PK index collations!
        if child.without_rowid {
            if let Some(pk_idx) = child.indexes.iter().find(|ix| ix.without_rowid_primary_key) {
                let mut parts = Vec::with_capacity(pk_idx.key_count);
                let mut p = param_start;
                for ic in &pk_idx.columns[..pk_idx.key_count] {
                    if ic.cid < 0 {
                        continue;
                    }
                    let cid = ic.cid as usize;
                    let cname = child.columns[cid].name.replace('"', "\"\"");
                    let coll = ic.collation.replace('"', "\"\"");
                    let val = row.get(cid).cloned().unwrap_or(SqlValue::Null);
                    if val.is_null() {
                        parts.push(format!("\"{cname}\" IS NULL"));
                    } else {
                        out_params.push(val);
                        parts.push(format!("\"{cname}\" COLLATE \"{coll}\" = ?{p}"));
                        p += 1;
                    }
                }
                if !parts.is_empty() {
                    return parts.join(" AND ");
                }
            }
        }
        // Rowid table with all 3 rowid aliases shadowed: match ALL non-virtual columns with
        // BINARY collation (never rely solely on a nullable non-IPK PRIMARY KEY!).
        let match_cids: Vec<usize> = child
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let mut parts = Vec::with_capacity(match_cids.len());
        let mut p = param_start;
        for cid in match_cids {
            let cname = child.columns[cid].name.replace('"', "\"\"");
            let val = row.get(cid).cloned().unwrap_or(SqlValue::Null);
            if val.is_null() {
                parts.push(format!("\"{cname}\" IS NULL"));
            } else {
                out_params.push(val);
                parts.push(format!("\"{cname}\" COLLATE \"BINARY\" = ?{p}"));
                p += 1;
            }
        }
        parts.join(" AND ")
    }

    fn exec_fk_child_delete(
        &mut self,
        child: &TableInfo,
        matches: &[(Option<i64>, Vec<SqlValue>)],
        dialect: Dialect,
    ) -> SqliteResult<()> {
        let tname = child.name.replace('"', "\"\"");
        self.fk_cascade_depth += 1;
        let prev_changes = self.changes;
        let prev_last_rid = self.last_insert_rowid;
        let prev_tctx = self.trigger_ctx.take();
        let prev_conflict = self.active_conflict_override.take();
        let res: SqliteResult<()> = (|| {
            for (rid, row) in matches {
                let mut pvals = Vec::new();
                let where_sql =
                    Self::build_fk_row_where_clause(child, *rid, row, 1, &mut pvals);
                let sql = format!("DELETE FROM \"{tname}\" WHERE {where_sql}");
                let parsed = parse_statement(&sql, &ParseOptions::default())
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::Delete(del) = parsed {
                    let qp = QueryParams::positional(pvals);
                    let _ = self.exec_delete(&del, &qp, dialect)?;
                }
            }
            Ok(())
        })();
        self.fk_cascade_depth -= 1;
        self.changes = prev_changes;
        self.last_insert_rowid = prev_last_rid;
        self.trigger_ctx = prev_tctx;
        self.active_conflict_override = prev_conflict;
        res
    }

    fn exec_fk_child_update(
        &mut self,
        child: &TableInfo,
        fk_from_cids: &[usize],
        new_vals: &[SqlValue],
        matches: &[(Option<i64>, Vec<SqlValue>)],
        dialect: Dialect,
    ) -> SqliteResult<()> {
        let tname = child.name.replace('"', "\"\"");
        let mut set_parts = Vec::with_capacity(fk_from_cids.len());
        for (i, &cid) in fk_from_cids.iter().enumerate() {
            let cname = child.columns[cid].name.replace('"', "\"\"");
            set_parts.push(format!("\"{cname}\" = ?{}", i + 1));
        }
        let set_sql = set_parts.join(", ");
        let where_param_start = fk_from_cids.len() + 1;
        self.fk_cascade_depth += 1;
        let prev_changes = self.changes;
        let prev_last_rid = self.last_insert_rowid;
        let prev_tctx = self.trigger_ctx.take();
        let prev_conflict = self.active_conflict_override.take();
        let res: SqliteResult<()> = (|| {
            for (rid, row) in matches {
                let mut pvals = new_vals.to_vec();
                let where_sql = Self::build_fk_row_where_clause(
                    child,
                    *rid,
                    row,
                    where_param_start,
                    &mut pvals,
                );
                let sql = format!("UPDATE \"{tname}\" SET {set_sql} WHERE {where_sql}");
                let parsed = parse_statement(&sql, &ParseOptions::default())
                    .map_err(|e| SqliteError::Sql(e.to_string()))?;
                if let Statement::Update(upd) = parsed {
                    let qp = QueryParams::positional(pvals);
                    let _ = self.exec_update(&upd, &qp, dialect)?;
                }
            }
            Ok(())
        })();
        self.fk_cascade_depth -= 1;
        self.changes = prev_changes;
        self.last_insert_rowid = prev_last_rid;
        self.trigger_ctx = prev_tctx;
        self.active_conflict_override = prev_conflict;
        res
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
            "auto_vacuum" => {
                if val_text.is_some() {
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row("auto_vacuum", SqlValue::Int(0)))
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
                    self.check_writable()?;
                    let uv = parse_pragma_i32(&v).unwrap_or(0);
                    return self.run_mutating_stmt(|conn| {
                        conn.pager.set_user_version(uv)?;
                        Ok(conn.empty_exec_result())
                    });
                }
                Ok(single_row(
                    "user_version",
                    SqlValue::Int(i64::from(self.pager.user_version())),
                ))
            }
            "application_id" => {
                if let Some(v) = val_text {
                    self.check_writable()?;
                    let aid = parse_pragma_i32(&v).unwrap_or(0);
                    return self.run_mutating_stmt(|conn| {
                        conn.pager.set_application_id(aid)?;
                        Ok(conn.empty_exec_result())
                    });
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
                        if actual != self.cached_collations.0 {
                            let preserved = self.cached_collations.1.with_encoding(actual);
                            self.cached_collations = (actual, preserved);
                            self.cached_dml.1.clear();
                        }
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
            "journal_mode" => {
                let mode = if self.pager.is_vfs_backed() {
                    "delete"
                } else {
                    "memory"
                };
                Ok(single_row("journal_mode", SqlValue::text(mode)))
            }
            "recursive_triggers" => {
                if let Some(v) = val_text {
                    self.recursive_triggers = parse_pragma_bool(&v);
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "recursive_triggers",
                    SqlValue::Int(i64::from(self.recursive_triggers)),
                ))
            }
            "case_sensitive_like" => {
                if let Some(v) = val_text {
                    self.case_sensitive_like = parse_pragma_bool(&v);
                    if let Ok(mut guard) = self.select_cache.lock() {
                        guard.clear();
                    }
                    return Ok(self.empty_exec_result());
                }
                Ok(self.empty_exec_result())
            }
            "ignore_check_constraints" => {
                if let Some(v) = val_text {
                    self.ignore_check_constraints = parse_pragma_bool(&v);
                    return Ok(self.empty_exec_result());
                }
                Ok(self.empty_exec_result())
            }
            "query_only" => {
                if let Some(v) = val_text {
                    self.query_only = parse_pragma_bool(&v);
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "query_only",
                    SqlValue::Int(i64::from(self.query_only)),
                ))
            }
            "defer_foreign_keys" => {
                if let Some(v) = val_text {
                    self.defer_foreign_keys = parse_pragma_bool(&v);
                    return Ok(self.empty_exec_result());
                }
                Ok(single_row(
                    "defer_foreign_keys",
                    SqlValue::Int(i64::from(self.defer_foreign_keys)),
                ))
            }
            "busy_timeout" => {
                if let Some(v) = val_text {
                    let ms = v
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .or_else(|| parse_pragma_i32(&v).map(i64::from))
                        .unwrap_or(0);
                    self.busy_timeout = ms.max(0);
                    return Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)));
                }
                Ok(single_row("timeout", SqlValue::Int(self.busy_timeout)))
            }
            "foreign_keys" => {
                if let Some(v) = val_text {
                    // SQLite treats `PRAGMA foreign_keys = ON/OFF` as a no-op inside an active transaction.
                    if !self.pager.in_transaction() {
                        self.foreign_keys = parse_pragma_bool(&v);
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
                let view = self.live_view()?;
                let check_indexes = name == "integrity_check";
                let (max_errors, roots, check_whole_db, target_tables): (
                    usize,
                    Vec<u32>,
                    bool,
                    Vec<Arc<TableInfo>>,
                ) = if let Some(ref raw_arg) = val_text {
                    let trimmed = raw_arg.trim();
                    if let Ok(n) = trimmed.parse::<i64>() {
                        let mut roots = vec![1u32];
                        let mut tbls = Vec::new();
                        for t in view.tables() {
                            if t.root_page > 1 {
                                roots.push(t.root_page);
                            }
                            if !t.system {
                                tbls.push(Arc::clone(t));
                            }
                        }
                        for ix in view.indexes() {
                            if !ix.without_rowid_primary_key && ix.root_page > 1 {
                                roots.push(ix.root_page);
                            }
                        }
                        let cap = if n > 0 {
                            usize::try_from(n).unwrap_or(usize::MAX)
                        } else {
                            100
                        };
                        (cap, roots, true, tbls)
                    } else if let Some(tbl) = view.schema().table(trimmed) {
                        let mut roots = Vec::new();
                        if tbl.root_page >= 1 {
                            roots.push(tbl.root_page);
                        }
                        for ix in &tbl.indexes {
                            if !ix.without_rowid_primary_key && ix.root_page > 1 {
                                roots.push(ix.root_page);
                            }
                        }
                        let tbls = if tbl.system {
                            Vec::new()
                        } else {
                            vec![Arc::clone(tbl)]
                        };
                        (100, roots, false, tbls)
                    } else if view.schema().view(trimmed).is_some() {
                        return Ok(single_row(&name, SqlValue::text("ok")));
                    } else {
                        return Err(SqliteError::Sql(format!("no such table: {trimmed}")));
                    }
                } else {
                    let mut roots = vec![1u32];
                    let mut tbls = Vec::new();
                    for t in view.tables() {
                        if t.root_page > 1 {
                            roots.push(t.root_page);
                        }
                        if !t.system {
                            tbls.push(Arc::clone(t));
                        }
                    }
                    for ix in view.indexes() {
                        if !ix.without_rowid_primary_key && ix.root_page > 1 {
                            roots.push(ix.root_page);
                        }
                    }
                    (100, roots, true, tbls)
                };
                let struct_msgs = check_integrity_metered_scoped(
                    &self.pager,
                    &roots,
                    max_errors,
                    check_whole_db,
                )?;
                let mut errors: Vec<String> = struct_msgs
                    .into_iter()
                    .filter(|m| m != "ok")
                    .collect();
                if errors.len() < max_errors {
                    let enc = self.pager.encoding();
                    let metered = !self.pager.is_noop_meter();
                    let mut qopts = self.query_options(self.dialect);
                    qopts.in_trigger = false;
                    let empty_params = QueryParams::empty();
                    let no_sub = NoSubqueries;
                    let mut eval_env = EvalEnv::new(&qopts, &empty_params, &no_sub);
                    eval_env.dqs = true;
                    'tables: for tbl in &target_tables {
                        let mut rmem = ConnMemGuard::new(&self.pager);
                        let rows = match self.read_all_table_rows(&view, tbl, &mut rmem) {
                            Ok(r) => r,
                            Err(SqliteError::ResourceLimit(rl)) => {
                                return Err(SqliteError::ResourceLimit(rl));
                            }
                            Err(e) => {
                                errors.push(format!("cannot scan table {}: {e}", tbl.name));
                                if errors.len() >= max_errors {
                                    break 'tables;
                                }
                                continue;
                            }
                        };
                        let scope_cols = build_table_scope_columns(tbl, &tbl.name);
                        let expr_scope_cols = build_plain_scope_columns(tbl);
                        let mut expected_idx_counts: Vec<usize> = if check_indexes {
                            vec![0usize; tbl.indexes.len()]
                        } else {
                            Vec::new()
                        };
                        let idx_key_infos: Vec<Option<KeyInfo>> = if check_indexes {
                            let mut infos = Vec::with_capacity(tbl.indexes.len());
                            for ix in &tbl.indexes {
                                match key_info_for_index(ix, enc, Some(&self.cached_collations.1)) {
                                    Ok(ki) => infos.push(Some(ki)),
                                    Err(SqliteError::UnknownCollation(c)) => {
                                        return Err(SqliteError::UnknownCollation(c));
                                    }
                                    Err(SqliteError::ResourceLimit(rl)) => {
                                        return Err(SqliteError::ResourceLimit(rl));
                                    }
                                    Err(e) => {
                                        errors.push(format!("index {} error: {e}", ix.name));
                                        if errors.len() >= max_errors {
                                            break 'tables;
                                        }
                                        infos.push(None);
                                    }
                                }
                            }
                            infos
                        } else {
                            Vec::new()
                        };
                        for (rid_opt, row) in &rows {
                            if metered {
                                self.pager.meter().charge_steps(1)?;
                            }
                            for (cid, col) in tbl.columns.iter().enumerate() {
                                if Some(cid) == tbl.ipk_column {
                                    continue;
                                }
                                let val = row.get(cid).unwrap_or(&SqlValue::Null);
                                if col.not_null && val.is_null() {
                                    errors.push(format!(
                                        "NULL value in {}.{}",
                                        tbl.name, col.name
                                    ));
                                    if errors.len() >= max_errors {
                                        break 'tables;
                                    }
                                }
                                if tbl.strict
                                    && !val.is_null()
                                    && !strict_storage_class_matches(val, &col.decl_type)
                                {
                                    errors.push(format!(
                                        "non-{} value in {}.{}",
                                        col.decl_type, tbl.name, col.name
                                    ));
                                    if errors.len() >= max_errors {
                                        break 'tables;
                                    }
                                }
                            }
                            if !self.ignore_check_constraints && !tbl.definition.checks.is_empty() {
                                let mut full_vals = row.clone();
                                if !tbl.without_rowid {
                                    let rid_v = rid_opt.map_or(SqlValue::Null, SqlValue::Int);
                                    while full_vals.len() < scope_cols.len() {
                                        full_vals.push(rid_v.clone());
                                    }
                                }
                                let scope = [EvalScope::simple(&scope_cols, &full_vals)];
                                for chk in &tbl.definition.checks {
                                    match eval_expr(&chk.expr, &scope, &eval_env) {
                                        Ok(v) if !v.is_null() && !boolean_value(&v, false) => {
                                            errors.push(format!(
                                                "CHECK constraint failed in {}",
                                                tbl.name
                                            ));
                                            if errors.len() >= max_errors {
                                                break 'tables;
                                            }
                                        }
                                        Err(xdb_sql::SqlError::ResourceLimit(rl)) => {
                                            return Err(SqliteError::ResourceLimit(rl));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if check_indexes {
                                for (ix_i, ix) in tbl.indexes.iter().enumerate() {
                                    if ix.without_rowid_primary_key {
                                        expected_idx_counts[ix_i] += 1;
                                    } else {
                                        match build_index_entry_values_fast(
                                            tbl,
                                            ix,
                                            *rid_opt,
                                            row,
                                            &qopts,
                                            &expr_scope_cols,
                                            &scope_cols,
                                        ) {
                                            Ok(Some(entry_vals)) => {
                                                expected_idx_counts[ix_i] += 1;
                                                if let Some(ref ki) = idx_key_infos[ix_i] {
                                                    match index_contains_prefix(
                                                        &self.pager,
                                                        ix.root_page,
                                                        &entry_vals,
                                                        ki,
                                                    ) {
                                                        Ok(Some(_)) => {}
                                                        Ok(None) => {
                                                            if let Some(rid) = rid_opt {
                                                                errors.push(format!(
                                                                    "row {rid} missing from index {}",
                                                                    ix.name
                                                                ));
                                                            } else {
                                                                errors.push(format!(
                                                                    "row missing from index {}",
                                                                    ix.name
                                                                ));
                                                            }
                                                            if errors.len() >= max_errors {
                                                                break 'tables;
                                                            }
                                                        }
                                                        Err(SqliteError::ResourceLimit(rl)) => {
                                                            return Err(SqliteError::ResourceLimit(
                                                                rl,
                                                            ));
                                                        }
                                                        Err(SqliteError::UnknownCollation(c)) => {
                                                            return Err(
                                                                SqliteError::UnknownCollation(c),
                                                            );
                                                        }
                                                        Err(e) => {
                                                            errors.push(format!(
                                                                "index {} lookup error: {e}",
                                                                ix.name
                                                            ));
                                                            if errors.len() >= max_errors {
                                                                break 'tables;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Ok(None) => {}
                                            Err(SqliteError::ResourceLimit(rl)) => {
                                                return Err(SqliteError::ResourceLimit(rl));
                                            }
                                            Err(SqliteError::UnknownCollation(c)) => {
                                                return Err(SqliteError::UnknownCollation(c));
                                            }
                                            Err(e) => {
                                                errors.push(format!(
                                                    "index {} key evaluation error: {e}",
                                                    ix.name
                                                ));
                                                if errors.len() >= max_errors {
                                                    break 'tables;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if check_indexes {
                            for (ix_i, ix) in tbl.indexes.iter().enumerate() {
                                let ki = match key_info_for_index(
                                    ix,
                                    enc,
                                    Some(&self.cached_collations.1),
                                ) {
                                    Ok(ki) => ki,
                                    Err(SqliteError::UnknownCollation(c)) => {
                                        return Err(SqliteError::UnknownCollation(c));
                                    }
                                    Err(SqliteError::ResourceLimit(rl)) => {
                                        return Err(SqliteError::ResourceLimit(rl));
                                    }
                                    Err(e) => {
                                        errors.push(format!("index {} error: {e}", ix.name));
                                        if errors.len() >= max_errors {
                                            break 'tables;
                                        }
                                        continue;
                                    }
                                };
                                let Ok(ix_reader) = view.index(&ix.name) else {
                                    continue;
                                };
                                let Ok(mut ix_cur) = ix_reader.cursor() else {
                                    errors.push(format!(
                                        "cannot open cursor on index {}",
                                        ix.name
                                    ));
                                    if errors.len() >= max_errors {
                                        break 'tables;
                                    }
                                    continue;
                                };
                                let mut prev_entry: Option<Vec<SqlValue>> = None;
                                let mut actual_cnt = 0usize;
                                loop {
                                    match ix_cur.next() {
                                        Ok(true) => {}
                                        Ok(false) => break,
                                        Err(SqliteError::ResourceLimit(rl)) => {
                                            return Err(SqliteError::ResourceLimit(rl));
                                        }
                                        Err(e) => {
                                            errors.push(format!(
                                                "corrupt index {}: {e}",
                                                ix.name
                                            ));
                                            if errors.len() >= max_errors {
                                                break 'tables;
                                            }
                                            break;
                                        }
                                    }
                                    if metered {
                                        self.pager.meter().charge_steps(1)?;
                                    }
                                    let entry = match ix_cur.row() {
                                        Ok(r) => r,
                                        Err(SqliteError::ResourceLimit(rl)) => {
                                            return Err(SqliteError::ResourceLimit(rl));
                                        }
                                        Err(e) => {
                                            errors.push(format!(
                                                "corrupt entry in index {}: {e}",
                                                ix.name
                                            ));
                                            if errors.len() >= max_errors {
                                                break 'tables;
                                            }
                                            break;
                                        }
                                    };
                                    actual_cnt += 1;
                                    if let Some(ref prev) = prev_entry {
                                        if compare_index_values(prev, &entry, &ki)
                                            == std::cmp::Ordering::Greater
                                        {
                                            errors.push(format!(
                                                "row {} out of order in index {}",
                                                actual_cnt, ix.name
                                            ));
                                            if errors.len() >= max_errors {
                                                break 'tables;
                                            }
                                        }
                                        if ix.unique {
                                            let key_len = ix.key_count.min(prev.len()).min(entry.len());
                                            let any_null = entry[..key_len]
                                                .iter()
                                                .any(SqlValue::is_null);
                                            if !any_null
                                                && key_tuples_equal(
                                                    &prev[..key_len],
                                                    &entry[..key_len],
                                                    &ki,
                                                )
                                            {
                                                errors.push(format!(
                                                    "non-unique entry in index {}",
                                                    ix.name
                                                ));
                                                if errors.len() >= max_errors {
                                                    break 'tables;
                                                }
                                            }
                                        }
                                    }
                                    prev_entry = Some(entry);
                                }
                                if actual_cnt != expected_idx_counts[ix_i] {
                                    errors.push(format!(
                                        "wrong # of entries in index {}",
                                        ix.name
                                    ));
                                    if errors.len() >= max_errors {
                                        break 'tables;
                                    }
                                }
                            }
                        }
                    }
                }
                let msgs = if errors.is_empty() {
                    vec!["ok".to_string()]
                } else {
                    errors
                };
                Ok(ExecResult {
                    columns: vec![name],
                    rows: msgs.into_iter().map(|m| vec![SqlValue::text(m)]).collect(),
                    changes: 0,
                    last_insert_rowid: self.last_insert_rowid,
                })
            }
            "table_info" | "table_xinfo" | "index_list" | "index_info" | "index_xinfo"
            | "database_list" | "foreign_key_list" | "foreign_key_check" | "table_list"
            | "collation_list" | "compile_options" | "pragma_list" | "function_list" => {
                let qopts = self.query_options(self.dialect);
                let view = self.live_view()?;
                let sql = match val_text {
                    Some(arg) => {
                        let esc = arg.replace('\'', "''");
                        format!("SELECT * FROM pragma_{name}('{esc}')")
                    }
                    None => format!("SELECT * FROM pragma_{name}"),
                };
                let qres = view.query_with_options(&sql, &QueryParams::empty(), &qopts)?;
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

    /// Re-validate all views after `ALTER TABLE RENAME` / `RENAME COLUMN` / `DROP COLUMN` using
    /// zero-row schema-only validation (`xdb_sql::validate_select_stmt_schema`), so runtime
    /// expressions in unrelated views (e.g., `VALUES(json('invalid'))`, `json_each('invalid')`,
    /// ungrouped aggregates, CTEs, subqueries, and views-over-views) are never executed while all
    /// column, table, `JOIN ... USING`, `ORDER BY`, `LIMIT`/`OFFSET`, and column-count bindings
    /// are strictly verified.
    fn validate_all_views_after_alter(
        &self,
        new_view: &SqliteDatabase,
        verb: &str,
    ) -> SqliteResult<()> {
        let mut qopts = self.query_options(self.dialect);
        qopts.in_trigger = true;
        let live_cat = crate::sql::live_catalog_for(new_view, &qopts);
        let views_to_check: Vec<Arc<ViewInfo>> = new_view.schema().views.clone();
        let parse_opts = ParseOptions {
            dialect: self.dialect,
            schema: true,
            ..ParseOptions::default()
        };
        let xdb_parse_opts = ParseOptions {
            dialect: Dialect::Xdb,
            schema: true,
            ..ParseOptions::default()
        };
        for vinfo in &views_to_check {
            let parsed = parse_statement(&vinfo.sql, &parse_opts).or_else(|_| {
                if self.dialect != Dialect::Xdb {
                    parse_statement(&vinfo.sql, &xdb_parse_opts)
                } else {
                    parse_statement(&vinfo.sql, &parse_opts)
                }
            });
            let Ok(Statement::CreateView(cv)) = parsed else {
                continue;
            };
            match xdb_sql::validate_select_stmt_schema(&live_cat, &cv.select, &qopts, &[], None) {
                Ok(cols) => {
                    if let Some(ref decl_cols) = cv.columns {
                        if decl_cols.len() != cols.len() {
                            return Err(SqliteError::Sql(format!(
                                "error in view {} after {verb}: expected {} columns for {} but got {}",
                                vinfo.name,
                                decl_cols.len(),
                                vinfo.name,
                                cols.len()
                            )));
                        }
                    }
                }
                Err(SqlError::Raise { .. }) => {}
                Err(SqlError::Runtime(ref msg))
                    if msg.contains("RAISE() may only be used within a trigger-program") => {}
                Err(SqlError::ResourceLimit(rl)) => return Err(SqliteError::ResourceLimit(rl)),
                Err(e) => {
                    return Err(SqliteError::Sql(format!(
                        "error in view {} after {verb}: {e}",
                        vinfo.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Re-validate all triggers after `ALTER TABLE RENAME` / `RENAME COLUMN` / `DROP COLUMN`
    /// against the post-alter schema (catches broken `INSTEAD OF` view `OLD`/`NEW` references,
    /// broken `UPDATE OF` lists, and broken `JOIN ... USING` or column references inside trigger
    /// bodies, CTEs, derived tables, and subqueries).
    fn validate_all_triggers_after_alter(
        &self,
        new_view: &SqliteDatabase,
        verb: &str,
    ) -> SqliteResult<()> {
        let triggers = &new_view.schema().triggers;
        if triggers.is_empty() {
            return Ok(());
        }
        let mut qopts = self.query_options(self.dialect);
        qopts.in_trigger = true;
        let live_cat = crate::sql::live_catalog_for(new_view, &qopts);
        let xdb_opts = ParseOptions {
            dialect: Dialect::Xdb,
            schema: true,
            ..ParseOptions::default()
        };

        let resolve_view_scope_cols = |vinfo: &ViewInfo| -> SqliteResult<Vec<ScopeColumn>> {
            let Ok(Statement::CreateView(cv)) = parse_statement(&vinfo.sql, &xdb_opts) else {
                return Ok(Vec::new());
            };
            match xdb_sql::validate_select_stmt_schema(&live_cat, &cv.select, &qopts, &[], None) {
                Ok(mut cols) => {
                    if let Some(ref decl) = cv.columns {
                        if decl.len() == cols.len() {
                            for (c, d) in cols.iter_mut().zip(decl.iter()) {
                                c.name = d.name.name.clone();
                            }
                        }
                    }
                    Ok(cols)
                }
                Err(SqlError::ResourceLimit(rl)) => Err(SqliteError::ResourceLimit(rl)),
                Err(e) => Err(SqliteError::Sql(e.to_string())),
            }
        };

        for tr in triggers {
            let Ok(Statement::CreateTrigger(ct)) = parse_statement(&tr.sql, &xdb_opts) else {
                continue;
            };
            let wrap_err = |e: SqliteError| -> SqliteError {
                match e {
                    SqliteError::ResourceLimit(rl) => SqliteError::ResourceLimit(rl),
                    other => {
                        SqliteError::Sql(format!("error in trigger {} after {verb}: {other}", tr.name))
                    }
                }
            };

            let tctx = if let Some(tbl) = new_view.schema().table(&tr.table_name) {
                if let Some(ref ucols) = ct.update_of {
                    for uc in ucols {
                        if !tbl
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&uc.name))
                        {
                            return Err(wrap_err(SqliteError::Sql(format!(
                                "no such column: {}",
                                uc.name
                            ))));
                        }
                    }
                }
                let dummy = vec![SqlValue::Null; tbl.columns.len()];
                let has_old = matches!(
                    ct.event,
                    TriggerEventKind::Update | TriggerEventKind::Delete
                );
                let has_new = matches!(
                    ct.event,
                    TriggerEventKind::Insert | TriggerEventKind::Update
                );
                TriggerRowCtx::for_table(
                    tbl,
                    has_old.then_some((None, dummy.as_slice())),
                    has_new.then_some((None, dummy.as_slice())),
                )
            } else if let Some(vinfo) = new_view.schema().view(&tr.table_name) {
                let vcols = resolve_view_scope_cols(vinfo).map_err(wrap_err)?;
                if let Some(ref ucols) = ct.update_of {
                    for uc in ucols {
                        if !vcols
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&uc.name))
                        {
                            return Err(wrap_err(SqliteError::Sql(format!(
                                "no such column: {}",
                                uc.name
                            ))));
                        }
                    }
                }
                let dummy = vec![SqlValue::Null; vcols.len()];
                let has_old = matches!(
                    ct.event,
                    TriggerEventKind::Update | TriggerEventKind::Delete
                );
                let has_new = matches!(
                    ct.event,
                    TriggerEventKind::Insert | TriggerEventKind::Update
                );
                TriggerRowCtx::for_view(
                    &vcols,
                    has_old.then_some(dummy.as_slice()),
                    has_new.then_some(dummy.as_slice()),
                )
            } else {
                continue;
            };

            let t_scope = [tctx.as_eval_scope()];
            if let Some(ref w) = ct.when {
                validate_dml_expr_with_catalog(w, &t_scope, &live_cat, None, &qopts)
                    .map_err(wrap_err)?;
            }
            for step in &ct.body {
                match step {
                    TriggerStep::Select(sel) => {
                        validate_select_schema(sel, &t_scope, &live_cat, None, &qopts)
                            .map_err(wrap_err)?;
                    }
                    TriggerStep::Insert(ins) => {
                        let tname = &ins.table.name.name;
                        if new_view.schema().virtual_table(tname).is_some()
                            || (tname.eq_ignore_ascii_case("sqlite_sequence")
                                && new_view.schema().table(tname).is_none())
                        {
                            continue;
                        }
                        let (expected_cols, tbl_opt) =
                            if let Some(tbl) = new_view.schema().table(tname) {
                                let exp = if let Some(ref cols) = ins.columns {
                                    for c in cols {
                                        let ok = tbl
                                            .columns
                                            .iter()
                                            .any(|tc| tc.name.eq_ignore_ascii_case(&c.name))
                                            || (!tbl.without_rowid
                                                && matches!(
                                                    c.name.to_ascii_lowercase().as_str(),
                                                    "rowid" | "_rowid_" | "oid"
                                                ));
                                        if !ok {
                                            return Err(wrap_err(SqliteError::Sql(format!(
                                                "table {} has no column named {}",
                                                tbl.name, c.name
                                            ))));
                                        }
                                    }
                                    cols.len()
                                } else {
                                    tbl.columns
                                        .iter()
                                        .filter(|c| c.generated.is_none())
                                        .count()
                                };
                                (exp, Some(tbl))
                            } else if let Some(vinfo) = new_view.schema().view(tname) {
                                let vcols = resolve_view_scope_cols(vinfo).map_err(wrap_err)?;
                                let exp = if let Some(ref cols) = ins.columns {
                                    for c in cols {
                                        if !vcols
                                            .iter()
                                            .any(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                                        {
                                            return Err(wrap_err(SqliteError::Sql(format!(
                                                "table {} has no column named {}",
                                                vinfo.name, c.name
                                            ))));
                                        }
                                    }
                                    cols.len()
                                } else {
                                    vcols.len()
                                };
                                (exp, None)
                            } else {
                                return Err(wrap_err(SqliteError::Sql(format!(
                                    "no such table: {tname}"
                                ))));
                            };
                        if let Some(ref sel) = ins.select {
                            let is_plain_values = ins.with.is_none()
                                && sel.with.is_none()
                                && sel.order_by.is_none()
                                && sel.order_by_all.is_none()
                                && sel.limit.is_none()
                                && matches!(sel.body, SelectBody::Values(_));
                            if is_plain_values {
                                if let SelectBody::Values(ref vals) = sel.body {
                                    for vrow in &vals.rows {
                                        if vrow.items.len() != expected_cols {
                                            return Err(wrap_err(SqliteError::Sql(format!(
                                                "table {tname} has {expected_cols} columns but {} values were supplied",
                                                vrow.items.len()
                                            ))));
                                        }
                                        for item in &vrow.items {
                                            validate_dml_expr_with_catalog(
                                                item, &t_scope, &live_cat, None, &qopts,
                                            )
                                            .map_err(wrap_err)?;
                                        }
                                    }
                                }
                            } else {
                                let cols = validate_select_schema(
                                    sel,
                                    &t_scope,
                                    &live_cat,
                                    ins.with.as_ref(),
                                    &qopts,
                                )
                                .map_err(wrap_err)?;
                                if cols.len() != expected_cols {
                                    return Err(wrap_err(SqliteError::Sql(format!(
                                        "table {tname} has {expected_cols} columns but {} values were supplied",
                                        cols.len()
                                    ))));
                                }
                            }
                        }
                        if let Some(tbl) = tbl_opt {
                            if !ins.upsert.is_empty() {
                                let ins_alias = ins
                                    .table
                                    .alias
                                    .as_ref()
                                    .unwrap_or(&ins.table.name)
                                    .name
                                    .as_str();
                                validate_upsert_targets(
                                    tbl,
                                    ins_alias,
                                    &ins.upsert,
                                    Some(&tctx),
                                    &live_cat,
                                    ins.with.as_ref(),
                                    &qopts,
                                )
                                .map_err(wrap_err)?;
                            }
                        }
                    }
                    TriggerStep::Update(upd) => {
                        let tname = &upd.table.name.name;
                        if new_view.schema().virtual_table(tname).is_some()
                            || (tname.eq_ignore_ascii_case("sqlite_sequence")
                                && new_view.schema().table(tname).is_none())
                        {
                            continue;
                        }
                        let alias = upd
                            .table
                            .alias
                            .as_ref()
                            .unwrap_or(&upd.table.name)
                            .name
                            .as_str();
                        let mut target_cols = if let Some(tbl) = new_view.schema().table(tname) {
                            validate_set_targets(tbl, &upd.set).map_err(wrap_err)?;
                            build_table_scope_columns(tbl, alias)
                        } else if let Some(vinfo) = new_view.schema().view(tname) {
                            let vcols = resolve_view_scope_cols(vinfo).map_err(wrap_err)?;
                            for sc in &upd.set {
                                for c in &sc.columns {
                                    if !vcols
                                        .iter()
                                        .any(|vc| vc.name.eq_ignore_ascii_case(&c.name))
                                    {
                                        return Err(wrap_err(SqliteError::Sql(format!(
                                            "no such column: {}",
                                            c.name
                                        ))));
                                    }
                                }
                            }
                            vcols
                                .into_iter()
                                .map(|mut c| {
                                    c.table = Some(alias.to_string());
                                    c
                                })
                                .collect()
                        } else {
                            return Err(wrap_err(SqliteError::Sql(format!(
                                "no such table: {tname}"
                            ))));
                        };
                        if let Some(ref from_src) = upd.from {
                            let from_sel = SelectStmt {
                                span: from_src.span(),
                                with: upd.with.clone(),
                                body: SelectBody::Core(Box::new(SelectCore {
                                    span: from_src.span(),
                                    distinct: None,
                                    columns: vec![ResultColumn::Star(xdb_parser::ResultStar {
                                        span: from_src.span(),
                                        table: None,
                                        exclude: None,
                                        replace: None,
                                    })],
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
                            let extra_cols =
                                validate_select_schema(&from_sel, &t_scope, &live_cat, None, &qopts)
                                    .map_err(wrap_err)?;
                            target_cols.extend(extra_cols);
                            collect_from_rowid_aliases(from_src, new_view.schema(), &mut target_cols);
                        }
                        let scopes = [tctx.as_eval_scope(), EvalScope::simple(&target_cols, &[])];
                        if let Some(ref we) = upd.where_clause {
                            validate_dml_expr_with_catalog(
                                we,
                                &scopes,
                                &live_cat,
                                upd.with.as_ref(),
                                &qopts,
                            )
                            .map_err(wrap_err)?;
                        }
                        for sc in &upd.set {
                            validate_dml_expr_with_catalog(
                                &sc.expr,
                                &scopes,
                                &live_cat,
                                upd.with.as_ref(),
                                &qopts,
                            )
                            .map_err(wrap_err)?;
                        }
                    }
                    TriggerStep::Delete(del) => {
                        let tname = &del.table.name.name;
                        if new_view.schema().virtual_table(tname).is_some()
                            || (tname.eq_ignore_ascii_case("sqlite_sequence")
                                && new_view.schema().table(tname).is_none())
                        {
                            continue;
                        }
                        let alias = del
                            .table
                            .alias
                            .as_ref()
                            .unwrap_or(&del.table.name)
                            .name
                            .as_str();
                        let target_cols = if let Some(tbl) = new_view.schema().table(tname) {
                            build_table_scope_columns(tbl, alias)
                        } else if let Some(vinfo) = new_view.schema().view(tname) {
                            resolve_view_scope_cols(vinfo)
                                .map_err(wrap_err)?
                                .into_iter()
                                .map(|mut c| {
                                    c.table = Some(alias.to_string());
                                    c
                                })
                                .collect()
                        } else {
                            return Err(wrap_err(SqliteError::Sql(format!(
                                "no such table: {tname}"
                            ))));
                        };
                        let scopes = [tctx.as_eval_scope(), EvalScope::simple(&target_cols, &[])];
                        if let Some(ref we) = del.where_clause {
                            validate_dml_expr_with_catalog(
                                we,
                                &scopes,
                                &live_cat,
                                del.with.as_ref(),
                                &qopts,
                            )
                            .map_err(wrap_err)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn collect_from_rowid_aliases(
    src: &xdb_parser::FromSource,
    schema: &SchemaCatalog,
    out: &mut Vec<ScopeColumn>,
) {
    match src {
        xdb_parser::FromSource::Table(ft) => {
            if let Some(ftbl) = schema.table(&ft.name.name) {
                if !ftbl.without_rowid {
                    let falias = ft.alias.as_ref().unwrap_or(&ft.name).name.clone();
                    for rname in ["rowid", "_rowid_", "oid"] {
                        if !ftbl
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(rname))
                        {
                            out.push(ScopeColumn {
                                table: Some(falias.clone()),
                                name: rname.to_string(),
                                affinity: Affinity::Integer,
                                collation: "BINARY".to_string(),
                                is_rowid_alias: true,
                            });
                        }
                    }
                }
            }
        }
        xdb_parser::FromSource::Group(g) => {
            if g.alias.is_none() {
                collect_from_rowid_aliases(&g.source, schema, out);
            }
        }
        xdb_parser::FromSource::Join { left, right, .. } => {
            collect_from_rowid_aliases(left, schema, out);
            collect_from_rowid_aliases(right, schema, out);
        }
        xdb_parser::FromSource::Function(_) | xdb_parser::FromSource::Subquery(_) => {}
    }
}

fn validate_select_schema(
    sel: &SelectStmt,
    outer_scopes: &[EvalScope<'_>],
    catalog: &dyn xdb_sql::SqlCatalog,
    outer_with: Option<&xdb_parser::WithClause>,
    qopts: &QueryOptions,
) -> SqliteResult<Vec<ScopeColumn>> {
    match xdb_sql::validate_select_stmt_schema(catalog, sel, qopts, outer_scopes, outer_with) {
        Ok(cols) => Ok(cols),
        Err(SqlError::ResourceLimit(rl)) => Err(SqliteError::ResourceLimit(rl)),
        Err(e) => Err(SqliteError::Sql(e.to_string())),
    }
}

fn validate_dml_expr_with_catalog(
    expr: &Expr,
    scopes: &[EvalScope<'_>],
    catalog: &dyn xdb_sql::SqlCatalog,
    with_clause: Option<&xdb_parser::WithClause>,
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    if xdb_sql::eval::expr_contains_aggregate(expr, qopts) {
        return Err(SqliteError::Sql("misuse of aggregate function".to_string()));
    }
    if xdb_sql::eval::expr_contains_window(expr) {
        return Err(SqliteError::Sql("misuse of window function".to_string()));
    }
    match xdb_sql::validate_expr_schema(catalog, expr, qopts, scopes, with_clause) {
        Ok(()) => Ok(()),
        Err(SqlError::ResourceLimit(rl)) => Err(SqliteError::ResourceLimit(rl)),
        Err(e) => Err(SqliteError::Sql(e.to_string())),
    }
}

fn validate_returning_clauses(
    table: &TableInfo,
    ret: &[ResultColumn],
    catalog: &dyn xdb_sql::SqlCatalog,
    with_clause: Option<&xdb_parser::WithClause>,
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    let scope_cols = build_table_scope_columns(table, &table.name);
    let scope = [EvalScope::simple(&scope_cols, &[])];
    for rc in ret {
        match rc {
            ResultColumn::Expr(re) => {
                validate_dml_expr_with_catalog(&re.expr, &scope, catalog, with_clause, qopts)?;
            }
            ResultColumn::Star(rs) => {
                if let Some(ref reps) = rs.replace {
                    for rep in reps {
                        validate_dml_expr_with_catalog(
                            &rep.expr,
                            &scope,
                            catalog,
                            with_clause,
                            qopts,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ───────────────────────────── Helper functions ─────────────────────────────

fn parse_pragma_bool(s: &str) -> bool {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return n > 0;
    }
    matches!(t.to_ascii_lowercase().as_str(), "on" | "true" | "yes")
}

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
    env.in_trigger = false;
    env.fn_ctx.pure_context = Some("a generated column");
    if xdb_sql::expr_contains_raise(expr) {
        return Err(SqliteError::Sql(
            "RAISE() may only be used within a trigger-program".to_string(),
        ));
    }

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
        upsert_after: Option<(Option<i64>, Vec<SqlValue>, Vec<String>)>,
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
    triggers: Vec<Arc<CompiledTrigger>>,
    has_insert_triggers: bool,
    has_update_triggers: bool,
    has_delete_triggers: bool,
    has_outgoing_fks: bool,
    has_incoming_fks: bool,
}

impl DmlTableState {
    fn build(
        table: Arc<TableInfo>,
        schema: &SchemaCatalog,
        enc: TextEncoding,
        colls: &CollationRegistry,
    ) -> SqliteResult<Self> {
        let triggers = compile_triggers_for_target(schema, &table.name);
        let has_insert_triggers = triggers.iter().any(|t| t.event == TriggerEventKind::Insert);
        let has_update_triggers = triggers.iter().any(|t| t.event == TriggerEventKind::Update);
        let has_delete_triggers = triggers.iter().any(|t| t.event == TriggerEventKind::Delete);
        let has_outgoing_fks = !table.definition.foreign_keys.is_empty();
        let has_incoming_fks = schema.tables.iter().any(|t| {
            t.definition
                .foreign_keys
                .iter()
                .any(|fk| fk.table.eq_ignore_ascii_case(&table.name))
        });
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
        let wr_pk_ki = if table.without_rowid {
            if let Some(pk_idx) = table.indexes.iter().find(|ix| ix.without_rowid_primary_key) {
                Some(key_info_for_without_rowid_pk(pk_idx, enc, colls)?)
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
                index_key_infos.push(Some(key_info_for_index(ix, enc, Some(colls))?));
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
            triggers,
            has_insert_triggers,
            has_update_triggers,
            has_delete_triggers,
            has_outgoing_fks,
            has_incoming_fks,
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

fn table_has_replace_conflict(table: &TableInfo) -> bool {
    if table.definition.ipk_on_error == OnError::Replace {
        return true;
    }
    if table
        .definition
        .columns
        .iter()
        .any(|c| c.not_null_on_error == OnError::Replace)
    {
        return true;
    }
    table
        .indexes
        .iter()
        .any(|ix| ix.definition.on_error == OnError::Replace)
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

fn try_eval_literal_const_expr(expr: &Expr) -> SqliteResult<Option<SqlValue>> {
    match expr {
        Expr::Variable(_) => Ok(None),
        Expr::Unary {
            op: xdb_parser::UnaryOp::Neg,
            operand,
            ..
        } => match operand.as_ref() {
            Expr::Number(n) if !n.is_float => integer_literal_value(n.text.as_bytes(), true)
                .map(Some)
                .ok_or_else(|| SqliteError::Sql("hex literal too big".to_string())),
            Expr::Number(n) => Ok(Some(real_literal_value(n.text.as_bytes(), true))),
            _ => Ok(None),
        },
        Expr::Unary {
            op: xdb_parser::UnaryOp::Pos,
            operand,
            ..
        } => try_eval_literal_const_expr(operand),
        _ => try_eval_simple_const_expr(expr, &QueryParams::empty()),
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

fn strict_storage_class_matches(v: &SqlValue, decl_type: &str) -> bool {
    if v.is_null() {
        return true;
    }
    match decl_type.trim().to_ascii_uppercase().as_str() {
        "ANY" => true,
        "INT" | "INTEGER" => matches!(v, SqlValue::Int(_)),
        "REAL" => matches!(v, SqlValue::Real(_) | SqlValue::Int(_)),
        "TEXT" => matches!(v, SqlValue::Text(_)),
        "BLOB" => matches!(v, SqlValue::Blob(_)),
        _ => true,
    }
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
    if !table.without_rowid {
        for rname in ["rowid", "_rowid_", "oid"] {
            if !table
                .columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(rname))
            {
                cols.push(ScopeColumn {
                    table: Some("excluded".to_string()),
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

fn validate_dml_indexed_by(
    table: &TableInfo,
    indexed_by: Option<&xdb_parser::Name>,
    where_clause: Option<&Expr>,
    alias: &str,
) -> SqliteResult<()> {
    if let Some(idx_name) = indexed_by {
        let Some(ix) = table
            .indexes
            .iter()
            .find(|ix| ix.name.eq_ignore_ascii_case(&idx_name.name))
        else {
            return Err(SqliteError::Sql(format!(
                "no such index: {}",
                idx_name.name
            )));
        };
        if let Some(ref iw) = ix.definition.where_clause {
            let mut preds = Vec::new();
            if let Some(we) = where_clause {
                let mut stack = vec![we];
                while let Some(cur) = stack.pop() {
                    if let Expr::Binary {
                        op: xdb_parser::BinaryOp::And,
                        left,
                        right,
                        ..
                    } = cur
                    {
                        stack.push(right);
                        stack.push(left);
                    } else {
                        preds.push(cur);
                    }
                }
            }
            if !xdb_sql::explain::where_implies_partial_index(&preds, iw, &table.name, alias) {
                return Err(SqliteError::Sql(format!("cannot use index: {}", ix.name)));
            }
        }
    }
    Ok(())
}

fn validate_single_table_expr_columns(
    expr: &Expr,
    table: &TableInfo,
    alias: &str,
    has_explicit_alias: bool,
    trigger_ctx: Option<&TriggerRowCtx>,
) -> SqliteResult<()> {
    match expr {
        Expr::Subquery { .. } | Expr::Exists { .. } => return Ok(()),
        Expr::InSelect { operand, .. } => {
            return validate_single_table_expr_columns(
                operand,
                table,
                alias,
                has_explicit_alias,
                trigger_ctx,
            );
        }
        Expr::Column(c) => {
            if let Some(ref sc) = c.schema {
                if has_explicit_alias || !sc.name.eq_ignore_ascii_case("main") {
                    return Err(SqliteError::Sql(format!(
                        "no such column: {}.{}.{}",
                        sc.name,
                        c.table.as_ref().map_or("", |t| t.name.as_str()),
                        c.column.name
                    )));
                }
            }
            if let Some(ref tbl) = c.table {
                if let Some(tctx) = trigger_ctx {
                    if c.schema.is_none()
                        && tctx.cols.iter().any(|tc| {
                            tc.table
                                .as_deref()
                                .is_some_and(|tt| tt.eq_ignore_ascii_case(&tbl.name))
                                && tc.name.eq_ignore_ascii_case(&c.column.name)
                        })
                    {
                        return Ok(());
                    }
                }
                if !tbl.name.eq_ignore_ascii_case(alias) {
                    return Err(SqliteError::Sql(format!(
                        "no such column: {}.{}",
                        tbl.name, c.column.name
                    )));
                }
            }
            let name = &c.column.name;
            let exists = table
                .columns
                .iter()
                .any(|col| col.name.eq_ignore_ascii_case(name))
                || (!table.without_rowid
                    && matches!(
                        name.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ));
            if !exists {
                return Err(SqliteError::Sql(format!("no such column: {name}")));
            }
        }
        _ => {}
    }
    for ch in xdb_parser::expr_children(expr) {
        validate_single_table_expr_columns(ch, table, alias, has_explicit_alias, trigger_ctx)?;
    }
    Ok(())
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

fn apply_set_clauses_sql_err(
    table: &TableInfo,
    old_rowid: Option<i64>,
    old_row: &[SqlValue],
    sets: &[SetClause],
    scopes: &[EvalScope<'_>],
    env: &EvalEnv<'_>,
) -> Result<(Option<i64>, Vec<SqlValue>), SqlError> {
    let mut new_row = old_row.to_vec();
    let mut new_rowid = old_rowid;

    let map_assign_err = |e: SqliteError| match e {
        SqliteError::ResourceLimit(r) => SqlError::ResourceLimit(r),
        other => SqlError::runtime(other.to_string()),
    };

    for sc in sets {
        if sc.columns.len() == 1 {
            let cname = &sc.columns[0].name;
            let val = eval_expr(&sc.expr, scopes, env)?;
            assign_column(table, cname, val, &mut new_rowid, &mut new_row)
                .map_err(map_assign_err)?;
        } else if let Expr::Vector { items, .. } = &sc.expr {
            if items.len() != sc.columns.len() {
                return Err(SqlError::runtime(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    items.len()
                )));
            }
            for (c, e) in sc.columns.iter().zip(items.iter()) {
                let val = eval_expr(e, scopes, env)?;
                assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)
                    .map_err(map_assign_err)?;
            }
        } else if let Expr::Subquery { select, .. } = &sc.expr {
            let (cols, rows) = env.subqueries.run_select(select, scopes)?;
            if cols.len() != sc.columns.len() {
                return Err(SqlError::runtime(format!(
                    "{} columns assigned {} values",
                    sc.columns.len(),
                    cols.len()
                )));
            }
            if let Some(first_row) = rows.into_iter().next() {
                for (c, val) in sc.columns.iter().zip(first_row.into_iter()) {
                    assign_column(table, &c.name, val, &mut new_rowid, &mut new_row)
                        .map_err(map_assign_err)?;
                }
            } else {
                for c in &sc.columns {
                    assign_column(table, &c.name, SqlValue::Null, &mut new_rowid, &mut new_row)
                        .map_err(map_assign_err)?;
                }
            }
        } else {
            return Err(SqlError::runtime(
                "multi-column SET requires row value".to_string(),
            ));
        }
    }
    Ok((new_rowid, new_row))
}

fn trigger_window_spec_has_variable(ws: &xdb_parser::WindowSpec) -> bool {
    ws.partition_by
        .as_deref()
        .is_some_and(|pbs| pbs.iter().any(trigger_expr_has_variable))
        || ws
            .order_by
            .as_deref()
            .is_some_and(|obs| obs.iter().any(|o| trigger_expr_has_variable(&o.expr)))
        || ws.frame.as_ref().is_some_and(|fr| {
            let bound_has = |b: &xdb_parser::FrameBound| {
                b.expr.as_deref().is_some_and(trigger_expr_has_variable)
            };
            bound_has(&fr.from) || fr.to.as_ref().is_some_and(bound_has)
        })
}

fn trigger_expr_has_variable(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) => true,
        Expr::Subquery { select, .. }
        | Expr::Exists { select, .. }
        | Expr::InSelect { select, .. } => {
            trigger_select_has_variable(select)
                || xdb_parser::expr_children(expr)
                    .into_iter()
                    .any(trigger_expr_has_variable)
        }
        Expr::Function(f) => {
            matches!(f.over, Some(xdb_parser::WindowRef::Spec(ref ws)) if trigger_window_spec_has_variable(ws))
                || xdb_parser::expr_children(expr)
                    .into_iter()
                    .any(trigger_expr_has_variable)
        }
        _ => xdb_parser::expr_children(expr)
            .into_iter()
            .any(trigger_expr_has_variable),
    }
}

fn trigger_from_has_variable(src: &xdb_parser::FromSource) -> bool {
    match src {
        xdb_parser::FromSource::Table(_) => false,
        xdb_parser::FromSource::Function(ff) => ff.args.iter().any(trigger_expr_has_variable),
        xdb_parser::FromSource::Subquery(sub) => trigger_select_has_variable(&sub.select),
        xdb_parser::FromSource::Group(g) => trigger_from_has_variable(&g.source),
        xdb_parser::FromSource::Join { left, right, on, .. } => {
            trigger_from_has_variable(left)
                || trigger_from_has_variable(right)
                || on.as_deref().is_some_and(trigger_expr_has_variable)
        }
    }
}

fn trigger_body_has_variable(body: &SelectBody) -> bool {
    match body {
        SelectBody::Core(core) => {
            core.columns.iter().any(|rc| match rc {
                ResultColumn::Expr(re) => trigger_expr_has_variable(&re.expr),
                ResultColumn::Star(rs) => rs
                    .replace
                    .as_deref()
                    .is_some_and(|r| r.iter().any(|x| trigger_expr_has_variable(&x.expr))),
            }) || core.from.as_ref().is_some_and(trigger_from_has_variable)
                || core
                    .where_clause
                    .as_ref()
                    .is_some_and(trigger_expr_has_variable)
                || core
                    .group_by
                    .as_deref()
                    .is_some_and(|gs| gs.iter().any(trigger_expr_has_variable))
                || core.having.as_ref().is_some_and(trigger_expr_has_variable)
                || core.windows.as_deref().is_some_and(|wins| {
                    wins.iter()
                        .any(|wd| trigger_window_spec_has_variable(&wd.window))
                })
        }
        SelectBody::Values(v) => v
            .rows
            .iter()
            .any(|r| r.items.iter().any(trigger_expr_has_variable)),
        SelectBody::Compound { left, right, .. } => {
            trigger_body_has_variable(left) || trigger_body_has_variable(right)
        }
    }
}

fn trigger_select_has_variable(sel: &SelectStmt) -> bool {
    sel.with
        .as_ref()
        .is_some_and(|w| w.ctes.iter().any(|c| trigger_select_has_variable(&c.select)))
        || trigger_body_has_variable(&sel.body)
        || sel
            .order_by
            .as_deref()
            .is_some_and(|obs| obs.iter().any(|o| trigger_expr_has_variable(&o.expr)))
        || sel.limit.as_ref().is_some_and(|l| {
            trigger_expr_has_variable(&l.count)
                || l.offset.as_deref().is_some_and(trigger_expr_has_variable)
        })
}

fn trigger_insert_has_variable(ins: &InsertStmt) -> bool {
    ins.with
        .as_ref()
        .is_some_and(|w| w.ctes.iter().any(|c| trigger_select_has_variable(&c.select)))
        || ins
            .select
            .as_deref()
            .is_some_and(trigger_select_has_variable)
        || ins.upsert.iter().any(|up| {
            up.target_where
                .as_ref()
                .is_some_and(trigger_expr_has_variable)
                || up.set.as_deref().is_some_and(|sets| {
                    sets.iter().any(|sc| trigger_expr_has_variable(&sc.expr))
                })
                || up
                    .where_clause
                    .as_ref()
                    .is_some_and(trigger_expr_has_variable)
        })
}

fn trigger_update_has_variable(upd: &UpdateStmt) -> bool {
    upd.with
        .as_ref()
        .is_some_and(|w| w.ctes.iter().any(|c| trigger_select_has_variable(&c.select)))
        || upd.from.as_ref().is_some_and(trigger_from_has_variable)
        || upd.set.iter().any(|sc| trigger_expr_has_variable(&sc.expr))
        || upd
            .where_clause
            .as_ref()
            .is_some_and(trigger_expr_has_variable)
}

fn trigger_delete_has_variable(del: &DeleteStmt) -> bool {
    del.with
        .as_ref()
        .is_some_and(|w| w.ctes.iter().any(|c| trigger_select_has_variable(&c.select)))
        || del
            .where_clause
            .as_ref()
            .is_some_and(trigger_expr_has_variable)
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
    if target.len() != 1 || table.without_rowid {
        return false;
    }
    let term = &target[0];
    if let Some(explicit_coll) = outermost_collate(&term.expr) {
        if !explicit_coll.eq_ignore_ascii_case("BINARY") {
            return false;
        }
    }
    let Expr::Column(c) = unwrap_collate(&term.expr) else {
        return false;
    };
    if let Some(ipk) = table.ipk_column {
        if c.column.name.eq_ignore_ascii_case(&table.columns[ipk].name) {
            return true;
        }
    }
    !table
        .columns
        .iter()
        .any(|tc| tc.name.eq_ignore_ascii_case(&c.column.name))
        && matches!(
            c.column.name.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
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
    target_alias: &str,
    upserts: &[xdb_parser::UpsertClause],
    trigger_ctx: Option<&TriggerRowCtx>,
    catalog: &dyn xdb_sql::SqlCatalog,
    with_clause: Option<&xdb_parser::WithClause>,
    qopts: &QueryOptions,
) -> SqliteResult<()> {
    let base_len = table.columns.len() + count_synth_rowid_aliases(table);
    let scope_cols = if upserts.is_empty() {
        Vec::new()
    } else {
        build_upsert_scope_columns(table, target_alias)
    };
    let mut hidden_unq = Vec::with_capacity(scope_cols.len());
    if !scope_cols.is_empty() {
        hidden_unq.resize(base_len, false);
        hidden_unq.resize(scope_cols.len(), true);
    }
    for up in upserts {
        if let Some(ref sets) = up.set {
            validate_set_targets(table, sets)?;
            let mut sc = EvalScope::simple(&scope_cols, &[]);
            sc.hidden_unqualified = Some(&hidden_unq);
            let t_scope_opt = trigger_ctx.map(|t| t.as_eval_scope());
            let scope_storage: [EvalScope<'_>; 2];
            let scope_single: [EvalScope<'_>; 1];
            let scopes: &[EvalScope<'_>] = if let Some(ts) = t_scope_opt {
                scope_storage = [ts, sc];
                &scope_storage
            } else {
                scope_single = [sc];
                &scope_single
            };
            for s in sets {
                validate_dml_expr_with_catalog(&s.expr, scopes, catalog, with_clause, qopts)?;
            }
            if let Some(ref we) = up.where_clause {
                validate_dml_expr_with_catalog(we, scopes, catalog, with_clause, qopts)?;
            }
        }
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
            let pk_label = table
                .ipk_column
                .map(|ipk| format!("{}.{}", table.name, table.columns[ipk].name))
                .unwrap_or_else(|| format!("{}.rowid", table.name));
            if label.eq_ignore_ascii_case(&pk_label) {
                return Some(idx);
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

fn validate_returning_expr(expr: &Expr, table: &TableInfo) -> SqliteResult<()> {
    match expr {
        Expr::Subquery { .. } | Expr::Exists { .. } => return Ok(()),
        Expr::InSelect { operand, .. } => return validate_returning_expr(operand, table),
        Expr::Function(fc) => {
            if fc.over.is_some() {
                return Err(SqliteError::Sql(format!(
                    "misuse of window function {}()",
                    fc.name.name
                )));
            }
            let lname = fc.name.name.to_ascii_lowercase();
            if xdb_sql::functions::aggregates::is_builtin_aggregate(
                &lname,
                fc.args.len(),
                fc.star,
            ) {
                return Err(SqliteError::Sql(format!(
                    "misuse of aggregate function {}()",
                    fc.name.name
                )));
            }
            let is_agg_only_name = matches!(
                lname.as_str(),
                "count"
                    | "sum"
                    | "total"
                    | "avg"
                    | "group_concat"
                    | "string_agg"
                    | "json_group_array"
                    | "json_group_object"
            ) || (matches!(lname.as_str(), "min" | "max")
                && (fc.args.len() < 2
                    || fc.star
                    || fc.filter.is_some()
                    || fc.quantifier.is_some()
                    || fc.order_by.is_some()));
            if is_agg_only_name {
                return Err(SqliteError::Sql(format!(
                    "wrong number of arguments to function {}()",
                    fc.name.name
                )));
            }
            if fc.filter.is_some() {
                return Err(SqliteError::Sql(format!(
                    "FILTER may not be used with non-aggregate {}()",
                    fc.name.name
                )));
            }
            if fc.star {
                return Err(SqliteError::Sql(format!(
                    "wrong number of arguments to function {}()",
                    fc.name.name
                )));
            }
        }
        Expr::Column(c) => {
            if let Some(ref sc) = c.schema {
                return Err(SqliteError::Sql(format!(
                    "no such column: {}.{}.{}",
                    sc.name,
                    c.table.as_ref().map_or("", |t| t.name.as_str()),
                    c.column.name
                )));
            }
            if let Some(ref tbl) = c.table {
                if !tbl.name.eq_ignore_ascii_case(&table.name) {
                    return Err(SqliteError::Sql(format!(
                        "no such column: {}.{}",
                        tbl.name, c.column.name
                    )));
                }
            }
            let name = &c.column.name;
            let exists = table
                .columns
                .iter()
                .any(|col| col.name.eq_ignore_ascii_case(name))
                || (!table.without_rowid
                    && matches!(
                        name.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ));
            if !exists {
                return Err(SqliteError::Sql(format!("no such column: {name}")));
            }
        }
        _ => {}
    }
    for ch in xdb_parser::expr_children(expr) {
        validate_returning_expr(ch, table)?;
    }
    Ok(())
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
                validate_returning_expr(&re.expr, table)?;
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
    let old_name = ct.name.name.as_str();
    let mut spans = vec![ct.name.span];
    // Rewrite table-qualified column references inside the table's own CHECK(...) constraints.
    for col in &ct.columns {
        for con in &col.constraints {
            if let ColumnConstraint::Check(chk) = con {
                collect_expr_table_ref_spans(&chk.expr, old_name, &mut spans);
            }
        }
    }
    for con in &ct.constraints {
        if let TableConstraint::Check(chk) = con {
            collect_expr_table_ref_spans(&chk.expr, old_name, &mut spans);
        }
    }
    let quoted = quote_ident_if_needed(new_name);
    Ok(apply_span_replacements(ddl, spans, &quoted))
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

fn append_table_constraint_to_ddl(ddl: &str, constraint_sql: &str) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let last_end = ct
        .constraints
        .last()
        .map(|tc| match tc {
            TableConstraint::PrimaryKey { span, .. }
            | TableConstraint::Unique { span, .. }
            | TableConstraint::ForeignKey { span, .. } => span.end,
            TableConstraint::Check(chk) => chk.span.end,
        })
        .or_else(|| ct.columns.last().map(|c| c.span.end))
        .ok_or_else(|| SqliteError::corrupt("empty CREATE TABLE"))?;
    let mut out = String::with_capacity(ddl.len() + constraint_sql.len() + 2);
    out.push_str(&ddl[..last_end]);
    out.push_str(", ");
    out.push_str(constraint_sql);
    out.push_str(&ddl[last_end..]);
    Ok(out)
}

/// Expand a constraint's byte span backward to include an immediately preceding
/// `CONSTRAINT <name>` prefix (since `xdb-parser` parses `CONSTRAINT <name>` separately).
fn expand_span_for_constraint_prefix(ddl: &str, span: xdb_parser::Span) -> xdb_parser::Span {
    let tokens = xdb_parser::raw_tokens(ddl, Dialect::Sqlite);
    let mut prev2: Option<(usize, u16)> = None;
    let mut prev1: Option<(usize, u16)> = None;
    for t in &tokens {
        if t.start >= span.start {
            break;
        }
        if matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT) {
            continue;
        }
        prev2 = prev1;
        prev1 = Some((t.start, t.kind));
    }
    if let (Some((c_start, c_kind)), Some(_)) = (prev2, prev1) {
        if c_kind == xdb_parser::tk::CONSTRAINT {
            return xdb_parser::Span {
                start: c_start,
                end: span.end,
            };
        }
    }
    span
}

fn drop_not_null_from_ddl(ddl: &str, cid: usize) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let col = ct
        .columns
        .get(cid)
        .ok_or_else(|| SqliteError::corrupt("column index out of bounds"))?;
    let mut spans: Vec<xdb_parser::Span> = Vec::new();
    for c in &col.constraints {
        if let ColumnConstraint::NotNull { span, .. } = c {
            spans.push(expand_span_for_constraint_prefix(ddl, *span));
        }
    }
    if spans.is_empty() {
        return Ok(ddl.to_string());
    }
    Ok(apply_span_replacements(ddl, spans, ""))
}

fn set_not_null_in_ddl(
    ddl: &str,
    cid: usize,
    on_conflict: Option<ConflictAction>,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };
    let col = ct
        .columns
        .get(cid)
        .ok_or_else(|| SqliteError::corrupt("column index out of bounds"))?;
    let suffix = match on_conflict {
        None => "NOT NULL".to_string(),
        Some(ConflictAction::Rollback) => "NOT NULL ON CONFLICT ROLLBACK".to_string(),
        Some(ConflictAction::Abort) => "NOT NULL ON CONFLICT ABORT".to_string(),
        Some(ConflictAction::Fail) => "NOT NULL ON CONFLICT FAIL".to_string(),
        Some(ConflictAction::Ignore) => "NOT NULL ON CONFLICT IGNORE".to_string(),
        Some(ConflictAction::Replace) => "NOT NULL ON CONFLICT REPLACE".to_string(),
    };
    if let Some(existing_span) = col.constraints.iter().find_map(|c| match c {
        ColumnConstraint::NotNull { span, .. } => Some(*span),
        _ => None,
    }) {
        if on_conflict.is_none() {
            return Ok(ddl.to_string());
        }
        let mut out = String::with_capacity(ddl.len() + suffix.len());
        out.push_str(&ddl[..existing_span.start]);
        out.push_str(&suffix);
        out.push_str(&ddl[existing_span.end..]);
        return Ok(out);
    }
    let pos = col.span.end;
    let mut out = String::with_capacity(ddl.len() + suffix.len() + 1);
    out.push_str(&ddl[..pos]);
    out.push(' ');
    out.push_str(&suffix);
    out.push_str(&ddl[pos..]);
    Ok(out)
}

fn drop_named_constraint_from_ddl(
    ddl: &str,
    target_name: &str,
    tbl: &TableInfo,
) -> SqliteResult<String> {
    let stmt = xdb_parser::parse_statement(ddl, &ParseOptions::default())
        .map_err(|e| SqliteError::Sql(e.to_string()))?;
    let Statement::CreateTable(ct) = stmt else {
        return Err(SqliteError::corrupt("expected CREATE TABLE"));
    };

    // 1. Check table-level constraints first (use lexer tokens so SQL comments around `,` work).
    for (idx, tc) in ct.constraints.iter().enumerate() {
        let (cname_opt, span, is_pk) = match tc {
            TableConstraint::PrimaryKey { name, span, .. } => (name.as_ref(), *span, true),
            TableConstraint::Unique { name, span, .. } => (name.as_ref(), *span, false),
            TableConstraint::Check(chk) => (chk.name.as_ref(), chk.span, false),
            TableConstraint::ForeignKey { name, span, .. } => (name.as_ref(), *span, false),
        };
        if cname_opt.is_some_and(|n| n.name.eq_ignore_ascii_case(target_name)) {
            if is_pk && (tbl.without_rowid || tbl.ipk_column.is_some()) {
                return Err(SqliteError::Sql(format!(
                    "cannot drop PRIMARY KEY constraint \"{target_name}\""
                )));
            }
            let full_span = expand_span_for_constraint_prefix(ddl, span);
            let tokens = xdb_parser::raw_tokens(ddl, Dialect::Sqlite);
            let prev_comma = tokens
                .iter()
                .rev()
                .find(|t| {
                    t.end <= full_span.start
                        && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT)
                })
                .filter(|t| t.kind == xdb_parser::tk::COMMA);
            if let Some(comma) = prev_comma {
                let mut out = String::with_capacity(ddl.len());
                out.push_str(&ddl[..comma.start]);
                out.push_str(&ddl[full_span.end..]);
                return Ok(out);
            }
            if idx + 1 < ct.constraints.len() {
                let next_comma = tokens
                    .iter()
                    .find(|t| {
                        t.start >= full_span.end
                            && !matches!(t.kind, xdb_parser::tk::SPACE | xdb_parser::tk::COMMENT)
                    })
                    .filter(|t| t.kind == xdb_parser::tk::COMMA);
                if let Some(comma) = next_comma {
                    let mut out = String::with_capacity(ddl.len());
                    out.push_str(&ddl[..full_span.start]);
                    out.push_str(&ddl[comma.end..]);
                    return Ok(out);
                }
            }
            let mut out = String::with_capacity(ddl.len());
            out.push_str(&ddl[..full_span.start]);
            out.push_str(&ddl[full_span.end..]);
            return Ok(out);
        }
    }

    // 2. Check column-level named constraints (excluding DEFAULT, COLLATE, GENERATED).
    for col in &ct.columns {
        for (cidx, c) in col.constraints.iter().enumerate() {
            let (cname_opt, span, is_pk, is_ref) = match c {
                ColumnConstraint::PrimaryKey { name, span, .. } => {
                    (name.as_ref(), *span, true, false)
                }
                ColumnConstraint::NotNull { name, span, .. } => {
                    (name.as_ref(), *span, false, false)
                }
                ColumnConstraint::Null { name, span, .. } => (name.as_ref(), *span, false, false),
                ColumnConstraint::Unique { name, span, .. } => {
                    (name.as_ref(), *span, false, false)
                }
                ColumnConstraint::Check(chk) => (chk.name.as_ref(), chk.span, false, false),
                ColumnConstraint::References { name, span, .. } => {
                    (name.as_ref(), *span, false, true)
                }
                _ => continue,
            };
            if cname_opt.is_some_and(|n| n.name.eq_ignore_ascii_case(target_name)) {
                if is_pk && (tbl.without_rowid || tbl.ipk_column.is_some()) {
                    return Err(SqliteError::Sql(format!(
                        "cannot drop PRIMARY KEY constraint \"{target_name}\""
                    )));
                }
                let mut full_span = expand_span_for_constraint_prefix(ddl, span);
                // Column-level `DEFERRABLE ...` is parsed as an immediately following
                // `ColumnConstraint::Deferrable` — strip it together with `REFERENCES`!
                if is_ref {
                    if let Some(ColumnConstraint::Deferrable {
                        span: def_span, ..
                    }) = col.constraints.get(cidx + 1)
                    {
                        full_span.end = full_span.end.max(def_span.end);
                    }
                }
                let mut out = String::with_capacity(ddl.len());
                out.push_str(&ddl[..full_span.start]);
                out.push_str(&ddl[full_span.end..]);
                return Ok(out);
            }
        }
    }

    Err(SqliteError::Sql(format!(
        "no such constraint: {target_name}"
    )))
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

#[inline]
fn is_ident_boundary_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn apply_span_replacements(ddl: &str, mut spans: Vec<xdb_parser::Span>, replacement: &str) -> String {
    if spans.is_empty() {
        return ddl.to_string();
    }
    spans.sort_by(|a, b| b.start.cmp(&a.start).then_with(|| b.end.cmp(&a.end)));
    spans.dedup();
    let already_quoted = replacement.starts_with('"');
    let forced_quoted = if already_quoted {
        None
    } else {
        Some(format!("\"{}\"", replacement.replace('"', "\"\"")))
    };
    let orig_bytes = ddl.as_bytes();
    let mut out = ddl.to_string();
    let mut last_start = usize::MAX;
    for sp in spans {
        if sp.end <= out.len() && sp.start <= sp.end && sp.end <= last_start {
            let need_quote = !already_quoted
                && (matches!(orig_bytes.get(sp.start), Some(b'"' | b'`' | b'[' | b'\''))
                    || (sp.start > 0 && is_ident_boundary_byte(orig_bytes[sp.start - 1]))
                    || (sp.end < orig_bytes.len() && is_ident_boundary_byte(orig_bytes[sp.end])));
            let rep = if need_quote {
                forced_quoted.as_deref().unwrap_or(replacement)
            } else {
                replacement
            };
            out.replace_range(sp.start..sp.end, rep);
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

fn view_select_body_aliases_col(body: &SelectBody, old_col: &str) -> bool {
    match body {
        SelectBody::Core(core) => core.columns.iter().any(|rc| {
            matches!(
                rc,
                ResultColumn::Expr(re)
                    if re.alias.as_ref().is_some_and(|a| a.name.eq_ignore_ascii_case(old_col))
            )
        }),
        SelectBody::Compound { left, .. } => view_select_body_aliases_col(left, old_col),
        SelectBody::Values(_) => false,
    }
}

fn from_non_target_has_col_before_target_in_natural(
    from: &xdb_parser::FromSource,
    table_name: &str,
    old_col: &str,
    ctes: &[String],
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match from {
        xdb_parser::FromSource::Join {
            left,
            right,
            operator,
            ..
        } => {
            if (operator.flags & xdb_parser::join_flag::NATURAL) != 0
                && from_contains_target_table(right, table_name, ctes)
                && !from_contains_target_table(left, table_name, ctes)
                && !from_tables_known_without_col(left, old_col, ctes, schema)
            {
                return true;
            }
            from_non_target_has_col_before_target_in_natural(
                left, table_name, old_col, ctes, schema,
            ) || from_non_target_has_col_before_target_in_natural(
                right, table_name, old_col, ctes, schema,
            )
        }
        xdb_parser::FromSource::Group(g) => from_non_target_has_col_before_target_in_natural(
            &g.source, table_name, old_col, ctes, schema,
        ),
        _ => false,
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
                        if let Some(ref reps) = s.replace {
                            for rep in reps {
                                collect_view_table_rename_spans_expr(
                                    &rep.expr, old_name, qualify, ctes, spans,
                                );
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

fn parse_view_ddl_stmt(ddl: &str) -> Option<CreateViewStmt> {
    let opts = ParseOptions {
        dialect: Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    match xdb_parser::parse_statement(ddl, &opts) {
        Ok(Statement::CreateView(cv)) => Some(*cv),
        _ => None,
    }
}

fn rewrite_table_name_in_view_ddl(ddl: &str, old_name: &str, new_name: &str) -> String {
    let Some(cv) = parse_view_ddl_stmt(ddl) else {
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
                core.from.as_ref().is_none_or(|f| {
                    !from_non_target_has_col_before_target_in_natural(
                        f, table_name, old_col, ctes, schema,
                    )
                })
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
                match rc {
                    ResultColumn::Expr(re) => {
                        collect_view_col_rename_spans_expr(
                            &re.expr, &aliases, allow_unq, table_name, old_col, ctes, schema, spans,
                        );
                    }
                    ResultColumn::Star(rs) => {
                        let star_targets_table = match &rs.table {
                            Some(t) => aliases.iter().any(|a| t.name.eq_ignore_ascii_case(a)),
                            None => allow_unq,
                        };
                        if let Some(ref excl) = rs.exclude {
                            if star_targets_table {
                                for ex in excl {
                                    if ex.name.eq_ignore_ascii_case(old_col) {
                                        spans.push(ex.span);
                                    }
                                }
                            }
                        }
                        if let Some(ref reps) = rs.replace {
                            for rep in reps {
                                collect_view_col_rename_spans_expr(
                                    &rep.expr, &aliases, allow_unq, table_name, old_col, ctes,
                                    schema, spans,
                                );
                                if star_targets_table && rep.name.name.eq_ignore_ascii_case(old_col)
                                {
                                    spans.push(rep.name.span);
                                }
                            }
                        }
                    }
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
        fn leftmost_core_from(body: &SelectBody) -> Option<&xdb_parser::FromSource> {
            match body {
                SelectBody::Core(core) => core.from.as_ref(),
                SelectBody::Compound { left, .. } => leftmost_core_from(left),
                SelectBody::Values(_) => None,
            }
        }
        let top_from = leftmost_core_from(&sel.body);
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
            top_from.is_none_or(|f| {
                !from_non_target_has_col_before_target_in_natural(
                    f, table_name, old_col, &ctes, schema,
                )
            })
        } else if let Some(from) = top_from {
            outer_unqualified && from_tables_known_without_col(from, old_col, &ctes, schema)
        } else {
            outer_unqualified
        };
        let eff_ob_unq = allow_unq && !view_select_body_aliases_col(&sel.body, old_col);
        for o in ob {
            collect_view_col_rename_spans_expr(
                &o.expr,
                &top_aliases,
                eff_ob_unq,
                table_name,
                old_col,
                &ctes,
                schema,
                spans,
            );
        }
    }
    if let Some(ref lim) = sel.limit {
        collect_view_col_rename_spans_expr(
            &lim.count,
            outer_aliases,
            outer_unqualified,
            table_name,
            old_col,
            &ctes,
            schema,
            spans,
        );
        if let Some(ref off) = lim.offset {
            collect_view_col_rename_spans_expr(
                off,
                outer_aliases,
                outer_unqualified,
                table_name,
                old_col,
                &ctes,
                schema,
                spans,
            );
        }
    }
}

fn check_view_using_rename_safe(
    ddl: &str,
    view_name: &str,
    table_name: &str,
    old_col: &str,
) -> SqliteResult<()> {
    fn from_only_target_table(
        src: &xdb_parser::FromSource,
        table_name: &str,
        ctes: &[String],
    ) -> bool {
        match src {
            xdb_parser::FromSource::Table(ft) => {
                ft.name.name.eq_ignore_ascii_case(table_name)
                    && !ctes.iter().any(|c| c.eq_ignore_ascii_case(&ft.name.name))
            }
            xdb_parser::FromSource::Group(g) => from_only_target_table(&g.source, table_name, ctes),
            _ => false,
        }
    }
    fn check_from(
        src: &xdb_parser::FromSource,
        view_name: &str,
        table_name: &str,
        old_col: &str,
        ctes: &[String],
    ) -> SqliteResult<()> {
        match src {
            xdb_parser::FromSource::Join {
                left, right, using, ..
            } => {
                check_from(left, view_name, table_name, old_col, ctes)?;
                check_from(right, view_name, table_name, old_col, ctes)?;
                if let Some(us) = using {
                    if us.iter().any(|u| u.name.eq_ignore_ascii_case(old_col)) {
                        let l_has = from_contains_target_table(left, table_name, ctes);
                        let r_has = from_contains_target_table(right, table_name, ctes);
                        if (l_has || r_has)
                            && !(from_only_target_table(left, table_name, ctes)
                                && from_only_target_table(right, table_name, ctes))
                        {
                            return Err(SqliteError::Sql(format!(
                                "error in view {view_name} after rename: cannot join using column {old_col} - column not present in both tables"
                            )));
                        }
                    }
                }
                Ok(())
            }
            xdb_parser::FromSource::Subquery(sub) => {
                check_select(&sub.select, view_name, table_name, old_col, ctes)
            }
            xdb_parser::FromSource::Group(g) => {
                check_from(&g.source, view_name, table_name, old_col, ctes)
            }
            _ => Ok(()),
        }
    }
    fn check_body(
        body: &SelectBody,
        view_name: &str,
        table_name: &str,
        old_col: &str,
        ctes: &[String],
    ) -> SqliteResult<()> {
        match body {
            SelectBody::Core(core) => {
                if let Some(ref f) = core.from {
                    check_from(f, view_name, table_name, old_col, ctes)?;
                }
                Ok(())
            }
            SelectBody::Compound { left, right, .. } => {
                check_body(left, view_name, table_name, old_col, ctes)?;
                check_body(right, view_name, table_name, old_col, ctes)
            }
            SelectBody::Values(_) => Ok(()),
        }
    }
    fn check_select(
        sel: &SelectStmt,
        view_name: &str,
        table_name: &str,
        old_col: &str,
        outer_ctes: &[String],
    ) -> SqliteResult<()> {
        let mut ctes = outer_ctes.to_vec();
        if let Some(ref w) = sel.with {
            for cte in &w.ctes {
                if w.recursive {
                    ctes.push(cte.name.name.clone());
                }
                check_select(&cte.select, view_name, table_name, old_col, &ctes)?;
                if !w.recursive {
                    ctes.push(cte.name.name.clone());
                }
            }
        }
        check_body(&sel.body, view_name, table_name, old_col, &ctes)
    }
    let Some(cv) = parse_view_ddl_stmt(ddl) else {
        return Ok(());
    };
    check_select(&cv.select, view_name, table_name, old_col, &[])
}

fn rewrite_column_name_in_view_ddl(
    ddl: &str,
    table_name: &str,
    old_col: &str,
    new_name: &str,
    schema: &crate::schema::SchemaCatalog,
) -> String {
    let Some(cv) = parse_view_ddl_stmt(ddl) else {
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
        core.from.as_ref().is_none_or(|f| {
            !from_non_target_has_col_before_target_in_natural(
                f, table_name, old_col, ctes, schema,
            )
        })
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
        match rc {
            ResultColumn::Expr(re) => {
                if expr_refs_any_alias_col(
                    &re.expr, &aliases, allow_unq, table_name, old_col, ctes, schema,
                ) {
                    return true;
                }
            }
            ResultColumn::Star(rs) => {
                let star_targets_table = match &rs.table {
                    Some(t) => aliases.iter().any(|a| t.name.eq_ignore_ascii_case(a)),
                    None => allow_unq,
                };
                if star_targets_table
                    && rs
                        .exclude
                        .as_ref()
                        .is_some_and(|ex| ex.iter().any(|n| n.name.eq_ignore_ascii_case(old_col)))
                {
                    return true;
                }
                if let Some(ref reps) = rs.replace {
                    for rep in reps {
                        if (star_targets_table && rep.name.name.eq_ignore_ascii_case(old_col))
                            || expr_refs_any_alias_col(
                                &rep.expr, &aliases, allow_unq, table_name, old_col, ctes, schema,
                            )
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    if let Some(ref we) = core.where_clause {
        if expr_refs_any_alias_col(we, &aliases, allow_unq, table_name, old_col, ctes, schema) {
            return true;
        }
    }
    if let Some(ref gb) = core.group_by {
        let gb_allow_unq = allow_unq
            && !core.columns.iter().any(|rc| {
                matches!(
                    rc,
                    ResultColumn::Expr(re)
                        if re.alias.as_ref().is_some_and(|a| a.name.eq_ignore_ascii_case(old_col))
                )
            });
        if gb.iter().any(|e| {
            expr_refs_any_alias_col(
                e,
                &aliases,
                gb_allow_unq,
                table_name,
                old_col,
                ctes,
                schema,
            )
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
                outer_aliases,
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
        fn leftmost_core_from(body: &SelectBody) -> Option<&xdb_parser::FromSource> {
            match body {
                SelectBody::Core(core) => core.from.as_ref(),
                SelectBody::Compound { left, .. } => leftmost_core_from(left),
                SelectBody::Values(_) => None,
            }
        }
        let top_from = leftmost_core_from(&stmt.body);
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
            top_from.is_none_or(|f| {
                !from_non_target_has_col_before_target_in_natural(
                    f, table_name, old_col, &ctes, schema,
                )
            })
        } else if let Some(from) = top_from {
            outer_unqualified && from_tables_known_without_col(from, old_col, &ctes, schema)
        } else {
            outer_unqualified
        };
        let eff_ob_unq = allow_unq && !view_select_body_aliases_col(&stmt.body, old_col);
        if ob.iter().any(|o| {
            expr_refs_any_alias_col(
                &o.expr,
                &top_aliases,
                eff_ob_unq,
                table_name,
                old_col,
                &ctes,
                schema,
            )
        }) {
            return true;
        }
    }
    if let Some(ref lim) = stmt.limit {
        if expr_refs_any_alias_col(
            &lim.count,
            outer_aliases,
            outer_unqualified,
            table_name,
            old_col,
            &ctes,
            schema,
        ) || lim.offset.as_ref().is_some_and(|off| {
            expr_refs_any_alias_col(
                off,
                outer_aliases,
                outer_unqualified,
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
    let Some(cv) = parse_view_ddl_stmt(ddl) else {
        return Ok(());
    };
    if select_refs_table_col(&cv.select, table_name, old_col, &[], false, &[], schema) {
        return Err(SqliteError::Sql(format!(
            "error in view {view_name} after drop column: no such column: {old_col}"
        )));
    }
    Ok(())
}

fn trigger_from_refs_table_col(
    src: &xdb_parser::FromSource,
    table_name: &str,
    old_col: &str,
    aliases: &mut Vec<String>,
    from_has_target: &mut bool,
    schema: &crate::schema::SchemaCatalog,
) -> bool {
    match src {
        xdb_parser::FromSource::Table(ft) => {
            if ft.name.name.eq_ignore_ascii_case(table_name) {
                *from_has_target = true;
                aliases.push(ft.alias.as_ref().unwrap_or(&ft.name).name.clone());
            }
            false
        }
        xdb_parser::FromSource::Function(ff) => ff.args.iter().any(|a| {
            expr_refs_any_alias_col(a, aliases, false, table_name, old_col, &[], schema)
        }),
        xdb_parser::FromSource::Subquery(sub) => {
            select_refs_table_col(&sub.select, table_name, old_col, aliases, false, &[], schema)
        }
        xdb_parser::FromSource::Group(g) => {
            trigger_from_refs_table_col(&g.source, table_name, old_col, aliases, from_has_target, schema)
        }
        xdb_parser::FromSource::Join {
            left,
            right,
            on,
            using,
            ..
        } => {
            let l = trigger_from_refs_table_col(left, table_name, old_col, aliases, from_has_target, schema);
            let r = trigger_from_refs_table_col(right, table_name, old_col, aliases, from_has_target, schema);
            let u = using.as_ref().is_some_and(|us| {
                (from_contains_target_table(left, table_name, &[])
                    || from_contains_target_table(right, table_name, &[]))
                    && us.iter().any(|col| col.name.eq_ignore_ascii_case(old_col))
            });
            let o = on.as_deref().is_some_and(|e| {
                expr_refs_any_alias_col(e, aliases, *from_has_target, table_name, old_col, &[], schema)
            });
            l || r || u || o
        }
    }
}

fn check_column_unreferenced_in_trigger_ddl(
    ddl: &str,
    trigger_name: &str,
    table_name: &str,
    old_col: &str,
    schema: &crate::schema::SchemaCatalog,
) -> SqliteResult<()> {
    let parse_opts = ParseOptions {
        dialect: Dialect::Xdb,
        schema: true,
        ..ParseOptions::default()
    };
    let Ok(Statement::CreateTrigger(tr)) = xdb_parser::parse_statement(ddl, &parse_opts) else {
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
    for step in &tr.body {
        let hit = match step {
            xdb_parser::TriggerStep::Select(s) => {
                select_refs_table_col(s, table_name, old_col, &trigger_aliases, false, &[], schema)
            }
            xdb_parser::TriggerStep::Insert(ins) => {
                let with_hit = ins.with.as_ref().is_some_and(|w| {
                    w.ctes.iter().any(|c| {
                        select_refs_table_col(
                            &c.select,
                            table_name,
                            old_col,
                            &trigger_aliases,
                            false,
                            &[],
                            schema,
                        )
                    })
                });
                let same_tbl = ins.table.name.name.eq_ignore_ascii_case(table_name);
                let target_hit = same_tbl
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
                let mut up_aliases = trigger_aliases.clone();
                if same_tbl {
                    up_aliases.push(ins.table.alias.as_ref().unwrap_or(&ins.table.name).name.clone());
                    up_aliases.push("excluded".to_string());
                }
                let upsert_hit = ins.upsert.iter().any(|up| {
                    let tcol_hit = same_tbl
                        && up.target.as_ref().is_some_and(|tcs| {
                            tcs.iter().any(|ic| {
                                expr_refs_any_alias_col(
                                    &ic.expr,
                                    &up_aliases,
                                    true,
                                    table_name,
                                    old_col,
                                    &[],
                                    schema,
                                )
                            })
                        });
                    let tw_hit = up.target_where.as_ref().is_some_and(|w| {
                        expr_refs_any_alias_col(w, &up_aliases, same_tbl, table_name, old_col, &[], schema)
                    });
                    let set_hit = up.set.as_ref().is_some_and(|sets| {
                        sets.iter().any(|sc| {
                            (same_tbl && sc.columns.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)))
                                || expr_refs_any_alias_col(
                                    &sc.expr, &up_aliases, same_tbl, table_name, old_col, &[], schema,
                                )
                        })
                    });
                    let w_hit = up.where_clause.as_ref().is_some_and(|w| {
                        expr_refs_any_alias_col(w, &up_aliases, same_tbl, table_name, old_col, &[], schema)
                    });
                    tcol_hit || tw_hit || set_hit || w_hit
                });
                with_hit || target_hit || sel_hit || upsert_hit
            }
            xdb_parser::TriggerStep::Update(upd) => {
                let with_hit = upd.with.as_ref().is_some_and(|w| {
                    w.ctes.iter().any(|c| {
                        select_refs_table_col(
                            &c.select,
                            table_name,
                            old_col,
                            &trigger_aliases,
                            false,
                            &[],
                            schema,
                        )
                    })
                });
                let same_tbl = upd.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(upd.table.alias.as_ref().unwrap_or(&upd.table.name).name.clone());
                }
                let mut from_has_target = false;
                let from_hit = upd.from.as_ref().is_some_and(|f| {
                    trigger_from_refs_table_col(
                        f,
                        table_name,
                        old_col,
                        &mut aliases,
                        &mut from_has_target,
                        schema,
                    )
                });
                let other_upd_has_col = !same_tbl
                    && schema.table(&upd.table.name.name).is_some_and(|ot| {
                        ot.columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(old_col))
                    });
                let allow_unq = same_tbl || (from_has_target && !other_upd_has_col);
                let set_hit = upd.set.iter().any(|sc| {
                    (same_tbl && sc.columns.iter().any(|c| c.name.eq_ignore_ascii_case(old_col)))
                        || expr_refs_any_alias_col(
                            &sc.expr, &aliases, allow_unq, table_name, old_col, &[], schema,
                        )
                });
                let where_hit = upd.where_clause.as_ref().is_some_and(|w| {
                    expr_refs_any_alias_col(w, &aliases, allow_unq, table_name, old_col, &[], schema)
                });
                with_hit || from_hit || set_hit || where_hit
            }
            xdb_parser::TriggerStep::Delete(del) => {
                let with_hit = del.with.as_ref().is_some_and(|w| {
                    w.ctes.iter().any(|c| {
                        select_refs_table_col(
                            &c.select,
                            table_name,
                            old_col,
                            &trigger_aliases,
                            false,
                            &[],
                            schema,
                        )
                    })
                });
                let same_tbl = del.table.name.name.eq_ignore_ascii_case(table_name);
                let mut aliases = trigger_aliases.clone();
                if same_tbl {
                    aliases.push(del.table.alias.as_ref().unwrap_or(&del.table.name).name.clone());
                }
                with_hit
                    || del.where_clause.as_ref().is_some_and(|w| {
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
