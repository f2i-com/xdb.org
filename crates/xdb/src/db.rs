//! XDB Database Module
//! Handles SQLite persistence and Yrs (CRDT) synchronization logic

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, ReadTxn, Transact, Update, WriteTxn};

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

/// The XDB Database - wraps SQLite with CRDT sync capabilities
pub struct XdbDatabase {
    conn: Connection,
    docs: HashMap<String, Doc>,
    db_path: PathBuf,
}

impl XdbDatabase {
    /// Create or open an XDB database at the given path
    pub fn open(path: PathBuf) -> DbResult<Self> {
        let conn = Connection::open(&path)?;

        // Enable WAL mode for better concurrent read performance.
        // WAL allows readers to proceed without blocking on writers,
        // which is important when multiple worker threads share a single connection.
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
        self.conn = Connection::open(&self.db_path)?;
        self.docs.clear();
        Ok(())
    }

    /// Replace current database file contents from another SQLite file.
    /// Drops the current connection first so file replacement is safe on Windows.
    pub fn replace_from_file(&mut self, source_path: &PathBuf) -> DbResult<()> {
        let old_conn = std::mem::replace(&mut self.conn, Connection::open_in_memory()?);
        drop(old_conn);

        std::fs::copy(source_path, &self.db_path)?;
        self.conn = Connection::open(&self.db_path)?;
        self.docs.clear();
        Ok(())
    }

    /// Execute a closure within a SQLite transaction (BEGIN/COMMIT/ROLLBACK).
    /// Uses manual SQL statements to avoid borrow conflicts with rusqlite's Transaction type.
    fn with_transaction<F, T>(&mut self, f: F) -> DbResult<T>
    where
        F: FnOnce(&mut Self) -> DbResult<T>,
    {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(result) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(result)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
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
                .ok();

            if let Some(state_bytes) = state {
                if let Ok(update) = Update::decode_v1(&state_bytes) {
                    let mut txn = doc.transact_mut();
                    txn.apply_update(update)
                        .map_err(|e| DbError::Crdt(e.to_string()))?;
                }
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
        let now = chrono::Utc::now().to_rfc3339();

        let collection: String = self
            .conn
            .query_row(
                "SELECT collection FROM records WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|_| DbError::NotFound(id.to_string()))?;

        self.conn.execute(
            "UPDATE records SET deleted = 1, updated_at = ?1 WHERE id = ?2",
            params![&now, id],
        )?;

        let record = Record {
            id: id.to_string(),
            collection: collection.clone(),
            data: serde_json::Value::Null,
            created_at: String::new(),
            updated_at: now,
            deleted: true,
        };
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

        Ok(update)
    }

    /// Upsert a record with a specific ID (used for external sync sources)
    pub fn upsert_record(&mut self, record: Record) -> DbResult<Vec<u8>> {
        self.with_transaction(|this| this.upsert_record_inner(record))
    }

    fn upsert_record_inner(&mut self, record: Record) -> DbResult<Vec<u8>> {
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
        if let Some(doc) = self.docs.get(&record.collection) {
            Self::save_crdt_state_to_db(&self.conn, &record.collection, doc)?;
        }

        Ok(update)
    }

    /// Get a single record by ID
    pub fn get_record(&self, id: &str) -> DbResult<Record> {
        self.conn
            .query_row(
                "SELECT id, collection, data, created_at, updated_at, deleted FROM records WHERE id = ?1",
                params![id],
                |row| {
                    let raw_data = row.get::<_, String>(2)?;
                    Ok(Record {
                        id: row.get(0)?,
                        collection: row.get(1)?,
                        data: serde_json::from_str(&raw_data).unwrap_or_else(|e| {
                            tracing::warn!("Invalid JSON in record data (get_record): {}", e);
                            serde_json::Value::Null
                        }),
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        deleted: row.get::<_, i32>(5)? != 0,
                    })
                },
            )
            .map_err(|_| DbError::NotFound(id.to_string()))
    }

    /// Get all records in a collection
    pub fn get_collection(&self, collection: &str) -> DbResult<Vec<Record>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, collection, data, created_at, updated_at, deleted FROM records WHERE collection = ?1 AND deleted = 0 ORDER BY created_at DESC",
        )?;

        let records = stmt
            .query_map(params![collection], |row| {
                let raw_data = row.get::<_, String>(2)?;
                Ok(Record {
                    id: row.get(0)?,
                    collection: row.get(1)?,
                    data: serde_json::from_str(&raw_data).unwrap_or_else(|e| {
                        tracing::warn!("Invalid JSON in record data (get_collection): {}", e);
                        serde_json::Value::Null
                    }),
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    deleted: row.get::<_, i32>(5)? != 0,
                })
            })?
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

        // Apply update and extract records in one scope
        let updated_records: Vec<Record> = {
            let doc = self.get_or_create_doc(collection)?;

            // Apply the update
            {
                let mut txn = doc.transact_mut();
                txn.apply_update(update)
                    .map_err(|e| DbError::Crdt(e.to_string()))?;
            }

            // Extract records from CRDT state
            let txn = doc.transact();
            let mut records = Vec::new();
            if let Some(map) = txn.get_map("records") {
                for (key, value) in map.iter(&txn) {
                    let yrs::Out::Any(yrs::Any::String(json_str)) = value else {
                        return Err(DbError::Crdt(
                            "Invalid record payload type in CRDT map".to_string(),
                        ));
                    };

                    let record =
                        serde_json::from_str::<Record>(json_str.as_ref()).map_err(|e| {
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
            records
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

    /// Clear all records in a collection (hard delete from SQLite and reset CRDT state)
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

    /// Export database to a file path
    pub fn export_to_file(&self, path: &PathBuf) -> DbResult<()> {
        std::fs::copy(&self.db_path, path)?;
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

        let db_size = std::fs::metadata(&self.db_path)?.len();

        Ok(DbStats {
            record_count: record_count as u64,
            collection_count: collection_count as u64,
            db_size_bytes: db_size,
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
