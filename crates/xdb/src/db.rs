//! XDB Database Module
//! Handles SQLite persistence and Yrs (CRDT) synchronization logic

use rusqlite::{
    backup::{Backup, StepResult},
    params, Connection, DatabaseName, OpenFlags, OptionalExtension,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, Observable, ReadTxn, Transact, Update, WriteTxn};

#[derive(Error, Debug)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("CRDT error: {0}")]
    Crdt(String),
    #[error("Record not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid database operation: {0}")]
    InvalidOperation(String),
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub collection: String,
    pub data: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    pub deleted: bool,
}

/// One collection's contribution to a bulk import (audit SN-01).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionImport {
    pub collection: String,
    /// Replace: every existing record NOT in `records` is tombstoned (a
    /// replicated deletion), then the incoming records are upserted. Merge
    /// (false): incoming records are upserted over what exists.
    #[serde(default)]
    pub replace: bool,
    pub records: Vec<Record>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionImportResult {
    pub collection: String,
    pub replaced: bool,
    pub imported: u64,
    pub tombstoned: u64,
    pub epoch: u64,
}

/// What a committed bulk import did. Only returned after the transaction
/// committed, so a caller holding it holds a durable-completion signal.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportSummary {
    pub collections: Vec<CollectionImportResult>,
    pub imported: u64,
    pub tombstoned: u64,
}

/// Deltas produced by a committed import: (collection, epoch, update bytes).
/// Publish them only after the commit that produced them.
pub type CommittedDeltas = Vec<(String, u64, Vec<u8>)>;

/// Outcome of applying a peer's update against the local reset epoch (audit XD-03).
#[derive(Debug)]
pub enum RemoteApplyOutcome {
    /// Applied; the records the update touched.
    Applied(Vec<Record>),
    /// The sender is behind an acknowledged reset. Nothing was applied.
    StaleEpoch { local: u64, remote: u64 },
    /// The sender knows a newer reset than this node. Nothing was applied;
    /// adopt it with `apply_remote_reset` and retry.
    MissingReset { local: u64, remote: u64 },
}

/// The XDB Database - wraps SQLite with CRDT sync capabilities
pub struct XdbDatabase {
    conn: Connection,
    docs: HashMap<String, Doc>,
    db_path: PathBuf,
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let raw_data: String = row.get(2)?;
    let data = serde_json::from_str(&raw_data).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Record {
        id: row.get(0)?,
        collection: row.get(1)?,
        data,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        deleted: row.get::<_, i32>(5)? != 0,
    })
}

impl XdbDatabase {
    /// Create or open an XDB database at the given path
    pub fn open(path: PathBuf) -> DbResult<Self> {
        let conn = Connection::open(&path)?;

        // Enable WAL mode for better concurrent read performance.
        // WAL allows readers to proceed without blocking on writers,
        // when other connections read the same database.
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Initialize schema
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS records (
                id TEXT PRIMARY KEY,
                collection TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted INTEGER DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_collection ON records(collection);
            CREATE INDEX IF NOT EXISTS idx_deleted ON records(deleted);

            CREATE TABLE IF NOT EXISTS crdt_state (
                collection TEXT PRIMARY KEY,
                state_vector BLOB NOT NULL,
                doc_state BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                peer_id TEXT NOT NULL,
                collection TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                update_data BLOB NOT NULL
            );

            -- Reset epochs (audit XD-03): an administrative reset of a shared
            -- collection bumps its epoch; updates from peers on an older epoch
            -- are rejected instead of resurrecting pre-reset records.
            CREATE TABLE IF NOT EXISTS collection_epochs (
                collection TEXT PRIMARY KEY,
                epoch INTEGER NOT NULL,
                origin TEXT,
                reset_at TEXT NOT NULL
            );
            "#,
        )?;

        Ok(Self {
            conn,
            docs: HashMap::new(),
            db_path: path,
        })
    }

    /// Get the database file path
    pub fn path(&self) -> &PathBuf {
        &self.db_path
    }

    /// Reload the database from disk (e.g. after an import replaced the file).
    /// Reopens the SQLite connection and clears all cached CRDT docs.
    pub fn reload(&mut self) -> DbResult<()> {
        self.require_autocommit("reload")?;
        *self = Self::open(self.db_path.clone())?;
        Ok(())
    }

    /// Restore a validated XDB snapshot, including committed WAL contents.
    /// SQLite's backup API keeps the current connection usable if restore fails.
    pub fn replace_from_file(&mut self, source_path: &PathBuf) -> DbResult<()> {
        self.require_autocommit("import")?;
        self.require_distinct_path(source_path)?;
        let source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Hold one read snapshot through validation and restore.
        source.execute_batch("BEGIN")?;
        let integrity: String = source.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DbError::InvalidOperation(format!(
                "Invalid SQLite snapshot: {integrity}"
            )));
        }
        source.prepare(
            "SELECT id, collection, data, created_at, updated_at, deleted FROM records LIMIT 0",
        )?;
        source.prepare("SELECT collection, state_vector, doc_state FROM crdt_state LIMIT 0")?;
        source.prepare(
            "SELECT id, peer_id, collection, timestamp, update_data FROM sync_log LIMIT 0",
        )?;
        let mut records = source
            .prepare("SELECT id, collection, data, created_at, updated_at, deleted FROM records")?;
        for record in records.query_map([], record_from_row)? {
            record?;
        }
        let mut states =
            source.prepare("SELECT collection, state_vector, doc_state FROM crdt_state")?;
        let snapshots = states.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for snapshot in snapshots {
            let (collection, state_vector, doc_state) = snapshot?;
            yrs::StateVector::decode_v1(&state_vector).map_err(|e| DbError::Crdt(e.to_string()))?;
            let update = Update::decode_v1(&doc_state).map_err(|e| DbError::Crdt(e.to_string()))?;
            let doc = Doc::new();
            doc.transact_mut()
                .apply_update(update)
                .map_err(|e| DbError::Crdt(e.to_string()))?;
            Self::records_from_doc(&collection, &doc)?;
        }
        {
            let backup = Backup::new(&source, &mut self.conn)?;
            if backup.step(-1)? != StepResult::Done {
                return Err(DbError::InvalidOperation(
                    "Database is busy; try importing again".into(),
                ));
            }
        }
        self.docs.clear();
        Ok(())
    }

    /// Execute a closure atomically, rolling back both SQLite and cached CRDT state.
    /// Nested calls use savepoints: a caught inner error rolls back only that call,
    /// while successful batched operations still share one outer disk commit.
    pub fn with_transaction<F, T>(&mut self, f: F) -> DbResult<T>
    where
        F: FnOnce(&mut Self) -> DbResult<T>,
    {
        self.conn.execute_batch("SAVEPOINT xdb_transaction")?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(Ok(result)) => {
                if let Err(error) = self.conn.execute_batch("RELEASE SAVEPOINT xdb_transaction") {
                    self.rollback_savepoint();
                    return Err(error.into());
                }
                Ok(result)
            }
            Ok(Err(error)) => {
                self.rollback_savepoint();
                Err(error)
            }
            Err(panic) => {
                self.rollback_savepoint();
                std::panic::resume_unwind(panic)
            }
        }
    }

    fn rollback_savepoint(&mut self) {
        let _ = self.conn.execute_batch(
            "ROLLBACK TO SAVEPOINT xdb_transaction; RELEASE SAVEPOINT xdb_transaction",
        );
        // Yrs transactions commit eagerly. Reload from the rolled-back SQLite
        // state on next access instead of retaining writes that never committed.
        self.docs.clear();
    }

    fn require_autocommit(&self, operation: &str) -> DbResult<()> {
        if !self.conn.is_autocommit() {
            return Err(DbError::InvalidOperation(format!(
                "Cannot {operation} inside a transaction"
            )));
        }
        Ok(())
    }

    fn require_distinct_path(&self, path: &PathBuf) -> DbResult<()> {
        if path.exists() && std::fs::canonicalize(path)? == std::fs::canonicalize(&self.db_path)? {
            return Err(DbError::InvalidOperation(
                "Source and destination must be different database files".into(),
            ));
        }
        Ok(())
    }

    /// Get or create a Yrs Doc for a collection
    fn get_or_create_doc(&mut self, collection: &str) -> DbResult<&mut Doc> {
        if !self.docs.contains_key(collection) {
            let doc = Doc::new();

            // Try to load existing CRDT state
            let state: Option<Vec<u8>> = self
                .conn
                .query_row(
                    "SELECT doc_state FROM crdt_state WHERE collection = ?1",
                    params![collection],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(state_bytes) = state {
                let update = Update::decode_v1(&state_bytes).map_err(|e| {
                    DbError::Crdt(format!("Invalid saved CRDT state for {collection}: {e}"))
                })?;
                let mut txn = doc.transact_mut();
                txn.apply_update(update)
                    .map_err(|e| DbError::Crdt(e.to_string()))?;
            }

            self.docs.insert(collection.to_string(), doc);
        }

        self.docs
            .get_mut(collection)
            .ok_or_else(|| DbError::NotFound(format!("Doc missing for collection: {}", collection)))
    }

    /// Save CRDT state for a collection (static helper to avoid borrow issues)
    fn save_crdt_state_to_db(conn: &Connection, collection: &str, doc: &Doc) -> DbResult<()> {
        let txn = doc.transact();
        let state_vector = txn.state_vector().encode_v1();
        let doc_state = txn.encode_state_as_update_v1(&yrs::StateVector::default());

        conn.execute(
            "INSERT OR REPLACE INTO crdt_state (collection, state_vector, doc_state) VALUES (?1, ?2, ?3)",
            params![collection, state_vector, doc_state],
        )?;

        Ok(())
    }

    /// Create a new record
    pub fn create_record(
        &mut self,
        collection: &str,
        data: serde_json::Value,
    ) -> DbResult<(Record, Vec<u8>)> {
        let collection = collection.to_string();
        let data = data.clone();
        self.with_transaction(|this| this.create_record_inner(&collection, data))
    }

    fn create_record_inner(
        &mut self,
        collection: &str,
        data: serde_json::Value,
    ) -> DbResult<(Record, Vec<u8>)> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let record = Record {
            id: id.clone(),
            collection: collection.to_string(),
            data: data.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            deleted: false,
        };

        // Insert into SQLite
        self.conn.execute(
            "INSERT INTO records (id, collection, data, created_at, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &record.id,
                &record.collection,
                serde_json::to_string(&record.data)?,
                &record.created_at,
                &record.updated_at,
                record.deleted as i32
            ],
        )?;

        // Update CRDT doc and get update bytes
        let update = {
            let doc = self.get_or_create_doc(collection)?;
            let mut txn = doc.transact_mut();
            let map = txn.get_or_insert_map("records");
            map.insert(&mut txn, id.clone(), serde_json::to_string(&record)?);
            txn.encode_update_v1()
        };

        // Save CRDT state (separate borrow scope)
        if let Some(doc) = self.docs.get(collection) {
            Self::save_crdt_state_to_db(&self.conn, collection, doc)?;
        }

        Ok((record, update))
    }

    /// Update an existing record
    pub fn update_record(
        &mut self,
        id: &str,
        data: serde_json::Value,
    ) -> DbResult<(Record, Vec<u8>)> {
        let id = id.to_string();
        let data = data.clone();
        self.with_transaction(|this| this.update_record_inner(&id, data))
    }

    fn update_record_inner(
        &mut self,
        id: &str,
        data: serde_json::Value,
    ) -> DbResult<(Record, Vec<u8>)> {
        let now = chrono::Utc::now().to_rfc3339();

        // Get existing record (need current data for merge)
        let existing = self.get_record(id)?;
        if existing.deleted {
            return Err(DbError::NotFound(id.to_string()));
        }
        let collection = existing.collection.clone();

        // Merge incoming data with existing data (shallow merge, incoming wins)
        let merged_data = match (existing.data, data) {
            (
                serde_json::Value::Object(mut existing_map),
                serde_json::Value::Object(incoming_map),
            ) => {
                for (k, v) in incoming_map {
                    existing_map.insert(k, v);
                }
                serde_json::Value::Object(existing_map)
            }
            // If incoming is not an object, treat as full replacement
            (_, incoming) => incoming,
        };

        // Update SQLite with merged data
        self.conn.execute(
            "UPDATE records SET data = ?1, updated_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&merged_data)?, &now, id],
        )?;

        let record = self.get_record(id)?;
        let record_json = serde_json::to_string(&record)?;

        // Update CRDT and get update bytes
        let update = {
            let doc = self.get_or_create_doc(&collection)?;
            let mut txn = doc.transact_mut();
            let map = txn.get_or_insert_map("records");
            map.insert(&mut txn, id.to_string(), record_json);
            txn.encode_update_v1()
        };

        // Save CRDT state (separate borrow scope)
        if let Some(doc) = self.docs.get(&collection) {
            Self::save_crdt_state_to_db(&self.conn, &collection, doc)?;
        }

        Ok((record, update))
    }

    /// Soft delete a record
    pub fn delete_record(&mut self, id: &str) -> DbResult<Vec<u8>> {
        let id = id.to_string();
        self.with_transaction(|this| this.delete_record_inner(&id))
    }

    fn delete_record_inner(&mut self, id: &str) -> DbResult<Vec<u8>> {
        self.delete_record_batched(id, true)
    }

    /// `persist_doc = false` defers the CRDT snapshot write to the caller (one
    /// save per collection per batch instead of one per record, audit XD-04).
    fn delete_record_batched(&mut self, id: &str, persist_doc: bool) -> DbResult<Vec<u8>> {
        let now = chrono::Utc::now().to_rfc3339();

        let mut record = self.get_record(id)?;
        let collection = record.collection.clone();

        self.conn.execute(
            "UPDATE records SET deleted = 1, updated_at = ?1 WHERE id = ?2",
            params![&now, id],
        )?;

        record.updated_at = now;
        record.deleted = true;
        let record_json = serde_json::to_string(&record)?;

        // Update CRDT and get update bytes
        let update = {
            let doc = self.get_or_create_doc(&collection)?;
            let mut txn = doc.transact_mut();
            let map = txn.get_or_insert_map("records");
            map.insert(&mut txn, id.to_string(), record_json);
            txn.encode_update_v1()
        };

        // Save CRDT state (separate borrow scope)
        if persist_doc {
            if let Some(doc) = self.docs.get(&collection) {
                Self::save_crdt_state_to_db(&self.conn, &collection, doc)?;
            }
        }

        Ok(update)
    }

    /// Upsert a record with a specific ID (used for external sync sources)
    pub fn upsert_record(&mut self, record: Record) -> DbResult<Vec<u8>> {
        self.with_transaction(|this| this.upsert_record_inner(record))
    }

    fn upsert_record_inner(&mut self, record: Record) -> DbResult<Vec<u8>> {
        self.upsert_record_batched(record, true)
    }

    /// `persist_doc = false` defers the CRDT snapshot write to the caller (audit XD-04).
    fn upsert_record_batched(&mut self, record: Record, persist_doc: bool) -> DbResult<Vec<u8>> {
        let record_json = serde_json::to_string(&record)?;

        // Upsert into SQLite
        self.conn.execute(
            "INSERT OR REPLACE INTO records (id, collection, data, created_at, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &record.id,
                &record.collection,
                serde_json::to_string(&record.data)?,
                &record.created_at,
                &record.updated_at,
                record.deleted as i32
            ],
        )?;

        // Update CRDT doc and get update bytes
        let update = {
            let doc = self.get_or_create_doc(&record.collection)?;
            let mut txn = doc.transact_mut();
            let map = txn.get_or_insert_map("records");
            map.insert(&mut txn, record.id.clone(), record_json);
            txn.encode_update_v1()
        };

        // Save CRDT state (separate borrow scope)
        if persist_doc {
            if let Some(doc) = self.docs.get(&record.collection) {
                Self::save_crdt_state_to_db(&self.conn, &record.collection, doc)?;
            }
        }

        Ok(update)
    }

    /// Get a single record by ID
    pub fn get_record(&self, id: &str) -> DbResult<Record> {
        self.conn
            .query_row(
                "SELECT id, collection, data, created_at, updated_at, deleted FROM records WHERE id = ?1",
                params![id],
                record_from_row,
            )
            .optional()?
            .ok_or_else(|| DbError::NotFound(id.to_string()))
    }

    /// Get all records in a collection
    pub fn get_collection(&self, collection: &str) -> DbResult<Vec<Record>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, collection, data, created_at, updated_at, deleted FROM records WHERE collection = ?1 AND deleted = 0 ORDER BY created_at DESC",
        )?;

        let records = stmt
            .query_map(params![collection], record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get all collections
    pub fn get_collections(&self) -> DbResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT collection FROM records")?;
        let collections = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(collections)
    }

    // ── Reset epochs (audit XD-03) ────────────────────────────────────────────

    /// The collection's reset epoch (0 until it has ever been reset).
    pub fn get_epoch(&self, collection: &str) -> DbResult<u64> {
        let epoch: Option<i64> = self
            .conn
            .query_row(
                "SELECT epoch FROM collection_epochs WHERE collection = ?1",
                params![collection],
                |row| row.get(0),
            )
            .optional()?;
        Ok(epoch.unwrap_or(0).max(0) as u64)
    }

    fn set_epoch(&self, collection: &str, epoch: u64, origin: &str) -> DbResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO collection_epochs (collection, epoch, origin, reset_at) VALUES (?1, ?2, ?3, ?4)",
            params![collection, epoch as i64, origin, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Administrative reset of a SHARED collection: clears local records and
    /// CRDT state and advances the epoch. The caller broadcasts the returned
    /// epoch; peers on the old epoch adopt it and can no longer reintroduce
    /// pre-reset records. Compare `clear_collection`, which is local only.
    pub fn reset_collection(&mut self, collection: &str, origin: &str) -> DbResult<u64> {
        let collection = collection.to_string();
        let origin = origin.to_string();
        self.with_transaction(|this| {
            let epoch = this.get_epoch(&collection)? + 1;
            this.clear_collection_inner(&collection)?;
            this.set_epoch(&collection, epoch, &origin)?;
            Ok(epoch)
        })
    }

    /// Advance the epoch WITHOUT clearing: the current local contents become
    /// the authoritative post-reset state (a "replace synchronized state"
    /// restore). Peers adopt the reset and then fetch this node's state.
    pub fn bump_epoch(&mut self, collection: &str, origin: &str) -> DbResult<u64> {
        let collection = collection.to_string();
        let origin = origin.to_string();
        self.with_transaction(|this| {
            let epoch = this.get_epoch(&collection)? + 1;
            this.set_epoch(&collection, epoch, &origin)?;
            Ok(epoch)
        })
    }

    /// Adopt a reset announced by a peer. Returns true when the epoch was
    /// newer than ours and the collection was cleared; false (nothing done)
    /// for an equal or OLDER epoch — a stale peer cannot undo a newer reset.
    pub fn apply_remote_reset(
        &mut self,
        collection: &str,
        epoch: u64,
        origin: &str,
    ) -> DbResult<bool> {
        let collection = collection.to_string();
        let origin = origin.to_string();
        self.with_transaction(|this| {
            if epoch <= this.get_epoch(&collection)? {
                return Ok(false);
            }
            this.clear_collection_inner(&collection)?;
            this.set_epoch(&collection, epoch, &origin)?;
            Ok(true)
        })
    }

    /// Apply a peer's update only when its epoch matches ours.
    pub fn apply_remote_update_at_epoch(
        &mut self,
        collection: &str,
        epoch: u64,
        update_bytes: &[u8],
    ) -> DbResult<RemoteApplyOutcome> {
        let local = self.get_epoch(collection)?;
        if epoch < local {
            return Ok(RemoteApplyOutcome::StaleEpoch {
                local,
                remote: epoch,
            });
        }
        if epoch > local {
            return Ok(RemoteApplyOutcome::MissingReset {
                local,
                remote: epoch,
            });
        }
        Ok(RemoteApplyOutcome::Applied(
            self.apply_remote_update(collection, update_bytes)?,
        ))
    }

    // ── Bulk import (audit SN-01) ─────────────────────────────────────────────

    fn validate_import_record(collection: &str, record: &Record) -> DbResult<()> {
        if record.id.trim().is_empty() {
            return Err(DbError::InvalidOperation(format!(
                "Import into '{collection}' contains a record without an id"
            )));
        }
        if record.collection != collection {
            return Err(DbError::InvalidOperation(format!(
                "Record '{}' belongs to '{}' but was imported into '{}'",
                record.id, record.collection, collection
            )));
        }
        Ok(())
    }

    /// Import several collections in ONE transaction. Either every batch
    /// commits or none does; the summary is returned only after the commit.
    ///
    /// - The whole batch is validated (non-empty collection names, non-empty
    ///   ids, records that belong to their batch) before any write.
    /// - `replace` tombstones records absent from the batch instead of hard
    ///   deleting them, so the removal REPLICATES to peers; a local hard
    ///   reset stays the separate, explicit `clear_collection`.
    /// - Returned deltas must be published only after this returns.
    pub fn import_records(
        &mut self,
        batches: Vec<CollectionImport>,
    ) -> DbResult<(ImportSummary, CommittedDeltas)> {
        for batch in &batches {
            if batch.collection.trim().is_empty() {
                return Err(DbError::InvalidOperation(
                    "Import contains a batch without a collection name".into(),
                ));
            }
            for record in &batch.records {
                Self::validate_import_record(&batch.collection, record)?;
            }
        }
        self.with_transaction(|this| {
            let mut summary = ImportSummary::default();
            let mut deltas: CommittedDeltas = Vec::new();
            for batch in batches {
                let epoch = this.get_epoch(&batch.collection)?;
                let mut tombstoned = 0u64;
                if batch.replace {
                    let incoming: HashMap<&str, ()> =
                        batch.records.iter().map(|r| (r.id.as_str(), ())).collect();
                    let existing = this.get_collection(&batch.collection)?;
                    for record in existing {
                        if !incoming.contains_key(record.id.as_str()) {
                            let update = this.delete_record_batched(&record.id, false)?;
                            deltas.push((batch.collection.clone(), epoch, update));
                            tombstoned += 1;
                        }
                    }
                }
                let mut imported = 0u64;
                for record in batch.records {
                    let update = this.upsert_record_batched(record, false)?;
                    deltas.push((batch.collection.clone(), epoch, update));
                    imported += 1;
                }
                // One CRDT snapshot per collection per batch (audit XD-04): saving
                // the whole document after EVERY record made a bulk import O(N^2)
                // in encoded bytes. The transaction still commits all or nothing.
                if let Some(doc) = this.docs.get(&batch.collection) {
                    Self::save_crdt_state_to_db(&this.conn, &batch.collection, doc)?;
                }
                summary.imported += imported;
                summary.tombstoned += tombstoned;
                summary.collections.push(CollectionImportResult {
                    collection: batch.collection,
                    replaced: batch.replace,
                    imported,
                    tombstoned,
                    epoch,
                });
            }
            Ok((summary, deltas))
        })
    }

    // ── Reconciliation helpers (audit XD-02) ──────────────────────────────────

    /// (collection, epoch, state vector) for every local collection: what a
    /// join/repair pass sends so peers can return exactly what we lack.
    pub fn reconcile_plan(&mut self) -> DbResult<Vec<(String, u64, Vec<u8>)>> {
        let mut plan = Vec::new();
        for collection in self.get_collections()? {
            let epoch = self.get_epoch(&collection)?;
            let sv = self.get_state_vector(&collection)?;
            plan.push((collection, epoch, sv));
        }
        Ok(plan)
    }

    /// Collections a peer announced that this node has never stored.
    pub fn unknown_collections(&self, announced: &[String]) -> DbResult<Vec<String>> {
        let known: HashMap<String, ()> = self
            .get_collections()?
            .into_iter()
            .map(|c| (c, ()))
            .collect();
        Ok(announced
            .iter()
            .filter(|c| !known.contains_key(*c))
            .cloned()
            .collect())
    }

    /// Total rows changed on this connection since it was opened (SQLite's
    /// own counter). Benchmarks use it to measure write amplification.
    pub fn total_changes(&self) -> u64 {
        // SAFETY: the handle belongs to this live connection and is only read.
        unsafe { rusqlite::ffi::sqlite3_total_changes(self.conn.handle()) as u64 }
    }

    /// Apply a remote CRDT update
    pub fn apply_remote_update(
        &mut self,
        collection: &str,
        update_bytes: &[u8],
    ) -> DbResult<Vec<Record>> {
        let collection = collection.to_string();
        let update_bytes = update_bytes.to_vec();
        self.with_transaction(|this| this.apply_remote_update_inner(&collection, &update_bytes))
    }

    fn apply_remote_update_inner(
        &mut self,
        collection: &str,
        update_bytes: &[u8],
    ) -> DbResult<Vec<Record>> {
        // Parse the update first
        let update = Update::decode_v1(update_bytes).map_err(|e| DbError::Crdt(e.to_string()))?;

        // Apply the update and extract ONLY the records it changed (audit
        // XD-04): a map observer collects the touched keys while the
        // transaction commits, so a one-record delta writes one row instead
        // of re-upserting the whole collection. A duplicate delivery changes
        // no key and therefore writes nothing.
        let updated_records: Vec<Record> = {
            let doc = self.get_or_create_doc(collection)?;
            let changed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            let map = doc.get_or_insert_map("records");
            let sink = changed.clone();
            let subscription = map.observe(move |txn, event| {
                if let Ok(mut keys) = sink.lock() {
                    for key in event.keys(txn).keys() {
                        keys.insert(key.to_string());
                    }
                }
            });

            // Apply the update (observers fire when the transaction commits)
            {
                let mut txn = doc.transact_mut();
                txn.apply_update(update)
                    .map_err(|e| DbError::Crdt(e.to_string()))?;
            }
            drop(subscription);

            let keys = changed
                .lock()
                .map(|k| k.clone())
                .unwrap_or_default();
            Self::records_from_doc_keys(collection, doc, &keys)?
        };

        // Now update SQLite with the extracted records
        for record in &updated_records {
            self.conn.execute(
                "INSERT OR REPLACE INTO records (id, collection, data, created_at, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &record.id,
                    &record.collection,
                    serde_json::to_string(&record.data)?,
                    &record.created_at,
                    &record.updated_at,
                    record.deleted as i32
                ],
            )?;
        }

        // Save CRDT state
        let doc = self
            .docs
            .get(collection)
            .ok_or_else(|| DbError::NotFound(collection.to_string()))?;
        Self::save_crdt_state_to_db(&self.conn, collection, doc)?;

        Ok(updated_records)
    }

    /// Validated records for the given map keys only (keys no longer present
    /// are skipped: records are never removed from the map, only tombstoned).
    fn records_from_doc_keys(
        collection: &str,
        doc: &Doc,
        keys: &HashSet<String>,
    ) -> DbResult<Vec<Record>> {
        let txn = doc.transact();
        let mut records = Vec::with_capacity(keys.len());
        if let Some(map) = txn.get_map("records") {
            for key in keys {
                let Some(value) = map.get(&txn, key.as_str()) else {
                    continue;
                };
                let yrs::Out::Any(yrs::Any::String(json_str)) = value else {
                    return Err(DbError::Crdt(
                        "Invalid record payload type in CRDT map".to_string(),
                    ));
                };
                let record = serde_json::from_str::<Record>(json_str.as_ref()).map_err(|e| {
                    DbError::Crdt(format!("Invalid record JSON in CRDT map: {}", e))
                })?;
                if record.collection != collection {
                    return Err(DbError::Crdt(format!(
                        "CRDT record collection mismatch: expected '{}', got '{}'",
                        collection, record.collection
                    )));
                }
                if &record.id != key {
                    return Err(DbError::Crdt(format!(
                        "CRDT record id mismatch: key '{}' vs record.id '{}'",
                        key, record.id
                    )));
                }
                records.push(record);
            }
        }
        Ok(records)
    }

    fn records_from_doc(collection: &str, doc: &Doc) -> DbResult<Vec<Record>> {
        let txn = doc.transact();
        let mut records = Vec::new();
        if let Some(map) = txn.get_map("records") {
            for (key, value) in map.iter(&txn) {
                let yrs::Out::Any(yrs::Any::String(json_str)) = value else {
                    return Err(DbError::Crdt(
                        "Invalid record payload type in CRDT map".to_string(),
                    ));
                };

                let record = serde_json::from_str::<Record>(json_str.as_ref()).map_err(|e| {
                    DbError::Crdt(format!("Invalid record JSON in CRDT map: {}", e))
                })?;

                if record.collection != collection {
                    return Err(DbError::Crdt(format!(
                        "CRDT record collection mismatch: expected '{}', got '{}'",
                        collection, record.collection
                    )));
                }

                let map_key = key.to_string();
                if record.id != map_key {
                    return Err(DbError::Crdt(format!(
                        "CRDT record id mismatch: key '{}' vs record.id '{}'",
                        map_key, record.id
                    )));
                }

                records.push(record);
            }
        }
        Ok(records)
    }

    /// Get current state vector for syncing
    pub fn get_state_vector(&mut self, collection: &str) -> DbResult<Vec<u8>> {
        let doc = self.get_or_create_doc(collection)?;
        let txn = doc.transact();
        Ok(txn.state_vector().encode_v1())
    }

    /// Get updates since a given state vector
    pub fn get_updates_since(
        &mut self,
        collection: &str,
        state_vector: &[u8],
    ) -> DbResult<Vec<u8>> {
        let doc = self.get_or_create_doc(collection)?;
        let txn = doc.transact();
        let sv =
            yrs::StateVector::decode_v1(state_vector).map_err(|e| DbError::Crdt(e.to_string()))?;
        Ok(txn.encode_state_as_update_v1(&sv))
    }

    /// Get full state for initial sync
    #[allow(dead_code)]
    pub fn get_full_state(&mut self, collection: &str) -> DbResult<Vec<u8>> {
        let doc = self.get_or_create_doc(collection)?;
        let txn = doc.transact();
        Ok(txn.encode_state_as_update_v1(&yrs::StateVector::default()))
    }

    /// LOCAL cache reset: hard-delete the collection's records and CRDT state on
    /// THIS node only. Nothing is replicated — peers keep their copy and will
    /// repopulate this node on the next reconciliation. For a replicated
    /// deletion use `delete_record` (a tombstone); for an authoritative reset
    /// of a shared collection use `reset_collection` (an epoch), audit XD-03.
    pub fn clear_collection(&mut self, collection: &str) -> DbResult<()> {
        self.with_transaction(|this| this.clear_collection_inner(collection))
    }

    fn clear_collection_inner(&mut self, collection: &str) -> DbResult<()> {
        self.conn.execute(
            "DELETE FROM records WHERE collection = ?1",
            params![collection],
        )?;
        self.conn.execute(
            "DELETE FROM crdt_state WHERE collection = ?1",
            params![collection],
        )?;
        // Reset the in-memory CRDT doc
        self.docs.remove(collection);
        Ok(())
    }

    /// Export a consistent SQLite snapshot, including recent writes in the WAL.
    pub fn export_to_file(&self, path: &PathBuf) -> DbResult<()> {
        self.require_autocommit("export")?;
        self.require_distinct_path(path)?;
        self.conn.backup(DatabaseName::Main, path, None)?;
        Ok(())
    }

    /// Get database statistics
    pub fn get_stats(&self) -> DbResult<DbStats> {
        let record_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM records WHERE deleted = 0",
            [],
            |row| row.get(0),
        )?;

        let collection_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT collection) FROM records",
            [],
            |row| row.get(0),
        )?;

        // Logical database size includes pages whose latest version is in WAL.
        let page_count: u64 = self
            .conn
            .pragma_query_value(None, "page_count", |row| row.get(0))?;
        let page_size: u64 = self
            .conn
            .pragma_query_value(None, "page_size", |row| row.get(0))?;

        Ok(DbStats {
            record_count: record_count as u64,
            collection_count: collection_count as u64,
            db_size_bytes: page_count * page_size,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    pub record_count: u64,
    pub collection_count: u64,
    pub db_size_bytes: u64,
}

/// Thread-safe wrapper for the database
pub type SharedDb = Arc<Mutex<XdbDatabase>>;

pub fn create_shared_db(path: PathBuf) -> DbResult<SharedDb> {
    Ok(Arc::new(Mutex::new(XdbDatabase::open(path)?)))
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bench_tests.rs"]
mod bench_tests;
