//! Tauri Integration Module
//!
//! Provides Tauri command handlers and setup utilities for easy integration
//! with Tauri applications.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use xdb::tauri::{setup_xdb, XdbState};
//!
//! tauri::Builder::default()
//!     .setup(|app| {
//!         xdb::tauri::setup_xdb(app)?;
//!         Ok(())
//!     })
//!     .invoke_handler(tauri::generate_handler![
//!         xdb::tauri::create_record,
//!         xdb::tauri::update_record,
//!         xdb::tauri::delete_record,
//!         xdb::tauri::get_record,
//!         xdb::tauri::get_collection,
//!         xdb::tauri::get_collections,
//!         xdb::tauri::get_db_stats,
//!         xdb::tauri::get_network_status,
//!         xdb::tauri::request_sync,
//!         xdb::tauri::export_database,
//!         xdb::tauri::import_database,
//!         xdb::tauri::get_db_path,
//!     ])
//!     .run(tauri::generate_context!())
//!     .expect("error running app");
//! ```

use crate::db::{create_shared_db, DbStats, Record, SharedDb};
use crate::network::{
    create_shared_network, NetworkEvent, NetworkMessage, NetworkNode, SharedNetwork,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::broadcast;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRecordPayload {
    pub collection: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRecordPayload {
    pub id: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub peer_id: String,
    pub connected_peers: Vec<String>,
    pub is_running: bool,
}

// ============================================================================
// Per-App Database Manager
// ============================================================================

/// Manages per-app SQLite databases.
///
/// Each app gets its own SQLite file at `{base_dir}/apps/{app_id}/data.sqlite`.
/// Databases are created lazily on first access.
pub struct DbManager {
    base_dir: PathBuf,
    default_db_path: PathBuf,
    databases: StdMutex<HashMap<String, SharedDb>>,
    app_handle: Option<AppHandle>,
}

impl DbManager {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            default_db_path: base_dir.join("apps").join("_default").join("data.sqlite"),
            base_dir,
            databases: StdMutex::new(HashMap::new()),
            app_handle: None,
        }
    }

    /// Use an already opened database for the default app. Named apps continue
    /// to use independent files under `base_dir/apps`.
    pub fn with_default_database(base_dir: PathBuf, db: SharedDb) -> Result<Self, String> {
        let default_db_path = db.lock().map_err(|e| e.to_string())?.path().clone();
        Ok(Self {
            base_dir,
            default_db_path,
            databases: StdMutex::new(HashMap::from([("_default".to_string(), db)])),
            app_handle: None,
        })
    }

    fn with_app_handle(mut self, app_handle: AppHandle) -> Self {
        self.app_handle = Some(app_handle);
        self
    }

    fn emit_change(&self, app_id: &str, change_type: &str, collection: Option<&str>) {
        if let Some(app) = &self.app_handle {
            let mut payload = serde_json::json!({
                "type": change_type,
                "app_id": sanitize_app_id(app_id),
            });
            if let Some(collection) = collection {
                payload["collection"] = collection.into();
            }
            let _ = app.emit("xdb-data-event", payload);
        }
    }

    /// Get or create a database for the given app ID.
    /// Empty or missing app_id defaults to "_default".
    pub fn get_db(&self, app_id: &str) -> Result<SharedDb, String> {
        let app_id = sanitize_app_id(app_id);
        let mut dbs = self.databases.lock().map_err(|e| e.to_string())?;
        if let Some(db) = dbs.get(&app_id) {
            return Ok(db.clone());
        }
        let db_path = self.get_app_path(&app_id);
        if let Some(db_dir) = db_path.parent() {
            std::fs::create_dir_all(db_dir).map_err(|e| e.to_string())?;
        }
        info!("Opening per-app database: {:?}", db_path);
        let db = create_shared_db(db_path).map_err(|e| e.to_string())?;
        dbs.insert(app_id, db.clone());
        Ok(db)
    }

    /// Get the database file path for a given app ID.
    pub fn get_app_path(&self, app_id: &str) -> PathBuf {
        let app_id = sanitize_app_id(app_id);
        if app_id == "_default" {
            self.default_db_path.clone()
        } else {
            self.base_dir.join("apps").join(&app_id).join("data.sqlite")
        }
    }

    /// Get the base directory where all app databases are stored.
    pub fn base_dir(&self) -> &PathBuf {
        &self.base_dir
    }
}

/// Thread-safe shared database manager
pub type SharedDbManager = Arc<DbManager>;

/// Sanitize an app ID to a safe directory name.
fn sanitize_app_id(app_id: &str) -> String {
    let trimmed = app_id.trim();
    if trimmed.is_empty() {
        return "_default".to_string();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn supports_legacy_sync(app_id: &str) -> bool {
    sanitize_app_id(app_id) == "_default"
}

async fn broadcast_scoped_update(
    network: &SharedNetwork,
    app_id: &str,
    collection: &str,
    update: Vec<u8>,
) {
    // The v1 wire format has no app identity. Sending named-app data through
    // this node would mix it with the default database on other peers.
    if !supports_legacy_sync(app_id) {
        return;
    }
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        if let Err(e) = net.broadcast_update(collection, update).await {
            error!("Failed to broadcast update: {}", e);
        }
    }
}

fn backup_database_for_import(
    db: &crate::db::XdbDatabase,
    source: &PathBuf,
) -> Result<PathBuf, String> {
    let mut backup_path = db.path().with_extension("db.backup");
    if backup_path.canonicalize().ok().as_ref() == Some(source) {
        // Restoring our last backup must not overwrite that source first.
        backup_path = db
            .path()
            .with_extension(format!("db.backup-{}", uuid::Uuid::new_v4()));
    }
    db.export_to_file(&backup_path).map_err(|e| e.to_string())?;
    Ok(backup_path)
}

/// Setup XDB in a Tauri application
///
/// This function initializes the database and network, and stores the state
/// in the Tauri app. Call this in your `setup` hook.
///
/// ## Example
///
/// ```rust,ignore
/// tauri::Builder::default()
///     .setup(|app| {
///         xdb::tauri::setup_xdb(app)?;
///         Ok(())
///     })
/// ```
pub fn setup_xdb(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // Get app data directory for database
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    // Create the directory if it doesn't exist
    std::fs::create_dir_all(&app_data_dir)?;

    // Create per-app database manager
    let db_manager =
        Arc::new(DbManager::new(app_data_dir.clone()).with_app_handle(app.handle().clone()));
    // Commands and the legacy network must share the same default database.
    // Older orphaned xdb.sqlite files are left untouched for manual recovery.
    let db = db_manager
        .get_db("")
        .map_err(|e| format!("Failed to initialize XDB database: {}", e))?;
    info!(
        "XDB per-app database directory: {:?}",
        app_data_dir.join("apps")
    );

    // Create shared network state
    let network = create_shared_network();

    // Store state in app
    app.manage(db.clone());
    app.manage(db_manager);
    app.manage(network.clone());

    // Initialize network in background
    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = init_network(app_handle, db, network).await {
            error!("Failed to initialize XDB network: {}", e);
        }
    });

    Ok(())
}

/// Setup XDB with a custom database path
pub fn setup_xdb_with_path(
    app: &tauri::App,
    db_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create parent directory if it doesn't exist
    let base_dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&base_dir)?;

    info!("XDB database path: {:?}", db_path);

    // Initialize the database
    let db = create_shared_db(db_path)
        .map_err(|e| format!("Failed to initialize XDB database: {}", e))?;

    // Create per-app database manager
    let db_manager = Arc::new(
        DbManager::with_default_database(base_dir, db.clone())?
            .with_app_handle(app.handle().clone()),
    );

    // Create shared network state
    let network = create_shared_network();

    // Store state in app
    app.manage(db.clone());
    app.manage(db_manager);
    app.manage(network.clone());

    // Initialize network in background
    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = init_network(app_handle, db, network).await {
            error!("Failed to initialize XDB network: {}", e);
        }
    });

    Ok(())
}

/// Initialize the P2P network
async fn init_network(
    app_handle: AppHandle,
    db: SharedDb,
    network: SharedNetwork,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Create broadcast channel for network events
    let (event_tx, event_rx) = broadcast::channel::<NetworkEvent>(100);

    // Start the P2P network node
    let node = NetworkNode::new(db.clone(), event_tx).await?;
    info!("XDB Network started with peer ID: {}", node.local_peer_id());

    // Store the network node
    *network.lock().await = Some(node);

    // Setup event forwarding to frontend
    setup_network_events(app_handle, event_rx);

    Ok(())
}

/// Shutdown XDB gracefully
///
/// Call this when your app is closing to ensure clean shutdown.
pub async fn shutdown_xdb(network: &SharedNetwork) {
    let net = { network.lock().await.take() };
    if let Some(net) = net {
        if let Err(e) = net.shutdown().await {
            error!("Failed to shutdown XDB network: {}", e);
        } else {
            info!("XDB network shutdown complete");
        }
    }
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Create a new record in a collection
#[tauri::command]
pub async fn create_record(
    db_manager: State<'_, SharedDbManager>,
    network: State<'_, SharedNetwork>,
    app_id: Option<String>,
    payload: CreateRecordPayload,
) -> Result<Record, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let (record, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .create_record(&payload.collection, payload.data)
            .map_err(|e| e.to_string())?
    };

    db_manager.emit_change(&app_id, "create", Some(&payload.collection));
    broadcast_scoped_update(&network, &app_id, &payload.collection, update).await;

    info!(
        "Created record {} in collection {}",
        record.id, record.collection
    );
    Ok(record)
}

/// Update an existing record
#[tauri::command]
pub async fn update_record(
    db_manager: State<'_, SharedDbManager>,
    network: State<'_, SharedNetwork>,
    app_id: Option<String>,
    payload: UpdateRecordPayload,
) -> Result<Record, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let (record, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .update_record(&payload.id, payload.data)
            .map_err(|e| e.to_string())?
    };

    db_manager.emit_change(&app_id, "update", Some(&record.collection));
    broadcast_scoped_update(&network, &app_id, &record.collection, update).await;

    info!("Updated record {}", record.id);
    Ok(record)
}

/// Delete a record (soft delete)
#[tauri::command]
pub async fn delete_record(
    db_manager: State<'_, SharedDbManager>,
    network: State<'_, SharedNetwork>,
    app_id: Option<String>,
    id: String,
) -> Result<bool, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let (collection, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        let record = db_lock.get_record(&id).map_err(|e| e.to_string())?;
        let update = db_lock.delete_record(&id).map_err(|e| e.to_string())?;
        (record.collection, update)
    };

    db_manager.emit_change(&app_id, "delete", Some(&collection));
    broadcast_scoped_update(&network, &app_id, &collection, update).await;

    info!("Deleted record {}", id);
    Ok(true)
}

/// Upsert a record with a specific ID (for external sync sources)
#[tauri::command]
pub async fn upsert_record(
    db_manager: State<'_, SharedDbManager>,
    network: State<'_, SharedNetwork>,
    app_id: Option<String>,
    record: Record,
) -> Result<Record, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let update = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .upsert_record(record.clone())
            .map_err(|e| e.to_string())?
    };

    db_manager.emit_change(&app_id, "upsert", Some(&record.collection));
    broadcast_scoped_update(&network, &app_id, &record.collection, update).await;

    Ok(record)
}

/// Get a single record by ID
#[tauri::command]
pub fn get_record(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    id: String,
) -> Result<Record, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_record(&id).map_err(|e| e.to_string())
}

/// Get all records in a collection
#[tauri::command]
pub fn get_collection(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    collection: String,
) -> Result<Vec<Record>, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock
        .get_collection(&collection)
        .map_err(|e| e.to_string())
}

/// Get all collection names
#[tauri::command]
pub fn get_collections(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
) -> Result<Vec<String>, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_collections().map_err(|e| e.to_string())
}

/// Clear all records in a collection
#[tauri::command]
pub fn clear_collection(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    collection: String,
) -> Result<bool, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let mut db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock
        .clear_collection(&collection)
        .map_err(|e| e.to_string())?;
    drop(db_lock);
    db_manager.emit_change(&app_id, "clear", Some(&collection));
    info!("Cleared collection {}", collection);
    Ok(true)
}

/// Get database statistics
#[tauri::command]
pub fn get_db_stats(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
) -> Result<DbStats, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_stats().map_err(|e| e.to_string())
}

/// Get network status
#[tauri::command]
pub async fn get_network_status(
    network: State<'_, SharedNetwork>,
) -> Result<NetworkStatus, String> {
    let net = { network.lock().await.clone() };
    if let Some(net) = net.filter(NetworkNode::is_running) {
        Ok(NetworkStatus {
            peer_id: net.local_peer_id(),
            connected_peers: net.get_connected_peers().await,
            is_running: true,
        })
    } else {
        Ok(NetworkStatus {
            peer_id: String::new(),
            connected_peers: vec![],
            is_running: false,
        })
    }
}

/// Request sync from peers for a collection
#[tauri::command]
pub async fn request_sync(
    db_manager: State<'_, SharedDbManager>,
    network: State<'_, SharedNetwork>,
    app_id: Option<String>,
    collection: String,
) -> Result<bool, String> {
    let app_id = app_id.unwrap_or_default();
    if !supports_legacy_sync(&app_id) {
        return Err("Native P2P sync is only available for the default database; named apps require an app-scoped sync backend".to_string());
    }
    let db = db_manager.get_db(&app_id)?;
    let state_vector = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .get_state_vector(&collection)
            .map_err(|e| e.to_string())?
    };

    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        net.request_sync(&collection, state_vector)
            .await
            .map_err(|e| e.to_string())?;
        info!("Requested sync for collection: {}", collection);
        Ok(true)
    } else {
        Err("Network not initialized".to_string())
    }
}

/// Export database to a file
#[tauri::command]
pub async fn export_database(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    path: String,
) -> Result<String, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let export_path = PathBuf::from(&path);

    // Canonicalize parent to resolve path traversal (e.g. ../../etc/passwd)
    let parent = export_path
        .parent()
        .ok_or("Invalid export path: no parent directory")?;
    if !parent.exists() {
        return Err(format!(
            "Export directory does not exist: {}",
            parent.display()
        ));
    }
    let _canonical = parent
        .canonicalize()
        .map_err(|e| format!("Invalid export path: {}", e))?;

    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock
        .export_to_file(&export_path)
        .map_err(|e| e.to_string())?;
    info!("Exported database to: {}", path);
    Ok(path)
}

/// Get the database file path for an app
#[tauri::command]
pub fn get_db_path(
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
) -> Result<String, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    Ok(db_lock.path().to_string_lossy().to_string())
}

/// Get the base directory where all app databases are stored
#[tauri::command]
pub fn get_db_base_dir(db_manager: State<'_, SharedDbManager>) -> Result<String, String> {
    Ok(db_manager
        .base_dir()
        .join("apps")
        .to_string_lossy()
        .to_string())
}

/// Import/restore database from a file
#[tauri::command]
pub async fn import_database(
    app: AppHandle,
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    source_path: String,
) -> Result<bool, String> {
    let app_id = app_id.unwrap_or_default();
    let db = db_manager.get_db(&app_id)?;
    let source = PathBuf::from(&source_path);
    if !source.exists() {
        return Err("Source file does not exist".to_string());
    }

    // Canonicalize to resolve path traversal
    let source = source
        .canonicalize()
        .map_err(|e| format!("Invalid import path: {}", e))?;

    // Validate source database integrity before replacing
    let source_conn =
        rusqlite::Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Invalid database file: {}", e))?;
    let integrity: String = source_conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("Integrity check failed: {}", e))?;
    if integrity != "ok" {
        return Err(format!("Database integrity check failed: {}", integrity));
    }
    drop(source_conn);

    // Serialize import under the DB mutex to prevent concurrent writes.
    let mut db_lock = db.lock().map_err(|e| e.to_string())?;
    // Create backup of current database
    let backup_path = backup_database_for_import(&db_lock, &source)?;
    info!(
        "Saved pre-import database backup: {}",
        backup_path.display()
    );

    // Replace the database file and reload in-memory state atomically under lock.
    db_lock
        .replace_from_file(&source)
        .map_err(|e| format!("Failed to replace database after import: {}", e))?;

    drop(db_lock);
    db_manager.emit_change(&app_id, "import", None);
    info!("Imported database from: {}", source_path);

    // Emit event to notify frontend to reload
    let _ = app.emit(
        "db-imported",
        serde_json::json!({ "app_id": sanitize_app_id(&app_id) }),
    );

    Ok(true)
}

/// Setup network event listener that emits to frontend
fn setup_network_events(app: AppHandle, mut event_rx: broadcast::Receiver<NetworkEvent>) {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => match &event {
                    NetworkEvent::MessageReceived(msg) => match msg {
                        NetworkMessage::SyncUpdate { collection, .. } => {
                            let _ = app.emit(
                                "xdb-sync-event",
                                serde_json::json!({
                                    "type": "sync_update",
                                    "collection": collection
                                }),
                            );
                        }
                        NetworkMessage::SyncResponse { collection, .. } => {
                            let _ = app.emit(
                                "xdb-sync-event",
                                serde_json::json!({
                                    "type": "sync_response",
                                    "collection": collection
                                }),
                            );
                        }
                        _ => {}
                    },
                    NetworkEvent::PeerConnected(peer) => {
                        let _ = app.emit(
                            "xdb-peer-event",
                            serde_json::json!({
                                "type": "connected",
                                "peer_id": peer.peer_id,
                                "addresses": peer.addresses
                            }),
                        );
                    }
                    NetworkEvent::PeerDisconnected(peer_id) => {
                        let _ = app.emit(
                            "xdb-peer-event",
                            serde_json::json!({
                                "type": "disconnected",
                                "peer_id": peer_id
                            }),
                        );
                    }
                },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    error!("Event receiver lagged by {} messages", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Event channel closed");
                    break;
                }
            }
        }
    });
}

/// Returns all XDB command handlers for use with `tauri::generate_handler!`
///
/// ## Example
///
/// ```rust,ignore
/// // In your lib.rs:
/// .invoke_handler(tauri::generate_handler![
///     xdb::tauri::create_record,
///     xdb::tauri::update_record,
///     xdb::tauri::delete_record,
///     xdb::tauri::get_record,
///     xdb::tauri::get_collection,
///     xdb::tauri::get_collections,
///     xdb::tauri::get_db_stats,
///     xdb::tauri::get_network_status,
///     xdb::tauri::request_sync,
///     xdb::tauri::export_database,
///     xdb::tauri::import_database,
///     xdb::tauri::get_db_path,
/// ])
/// ```
#[macro_export]
macro_rules! xdb_commands {
    () => {
        tauri::generate_handler![
            $crate::tauri::create_record,
            $crate::tauri::update_record,
            $crate::tauri::delete_record,
            $crate::tauri::upsert_record,
            $crate::tauri::get_record,
            $crate::tauri::get_collection,
            $crate::tauri::get_collections,
            $crate::tauri::clear_collection,
            $crate::tauri::get_db_stats,
            $crate::tauri::get_network_status,
            $crate::tauri::request_sync,
            $crate::tauri::export_database,
            $crate::tauri::import_database,
            $crate::tauri::get_db_path,
            $crate::tauri::get_db_base_dir,
        ]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_app_aliases_share_one_database_instance() {
        let dir = tempfile::tempdir().unwrap();
        let manager = DbManager::new(dir.path().to_path_buf());
        let default = manager.get_db("").unwrap();
        assert!(Arc::ptr_eq(&default, &manager.get_db("_default").unwrap()));
        assert!(Arc::ptr_eq(&default, &manager.get_db("  ").unwrap()));
        assert_eq!(
            default.lock().unwrap().path(),
            &dir.path().join("apps/_default/data.sqlite")
        );
    }

    #[test]
    fn custom_default_path_is_used_by_commands_and_network() {
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom.sqlite");
        let network_db = create_shared_db(custom_path.clone()).unwrap();
        let manager =
            DbManager::with_default_database(dir.path().to_path_buf(), network_db.clone()).unwrap();
        assert!(Arc::ptr_eq(&network_db, &manager.get_db("").unwrap()));
        assert_eq!(manager.get_app_path("_default"), custom_path);
        network_db
            .lock()
            .unwrap()
            .create_record("notes", serde_json::json!({"title": "Shared default"}))
            .unwrap();
        assert_eq!(
            manager
                .get_db("")
                .unwrap()
                .lock()
                .unwrap()
                .get_collection("notes")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn named_apps_keep_independent_local_records() {
        let dir = tempfile::tempdir().unwrap();
        let manager = DbManager::new(dir.path().to_path_buf());
        let first = manager.get_db("first").unwrap();
        let second = manager.get_db("second").unwrap();
        first
            .lock()
            .unwrap()
            .create_record("notes", serde_json::json!({"title": "Only first"}))
            .unwrap();
        assert!(second
            .lock()
            .unwrap()
            .get_collection("notes")
            .unwrap()
            .is_empty());
        assert!(manager
            .get_db("")
            .unwrap()
            .lock()
            .unwrap()
            .get_collection("notes")
            .unwrap()
            .is_empty());
        assert!(!supports_legacy_sync("first"));
        assert!(!supports_legacy_sync("second"));
    }

    #[test]
    fn only_default_app_aliases_use_the_legacy_sync_protocol() {
        assert!(supports_legacy_sync(""));
        assert!(supports_legacy_sync("   "));
        assert!(supports_legacy_sync("_default"));
        assert!(!supports_legacy_sync("fieldnotes"));
        assert!(!supports_legacy_sync("bundle-123"));
    }

    #[test]
    fn restoring_the_last_backup_preserves_its_original_records() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.sqlite");
        let mut db = crate::db::XdbDatabase::open(db_path.clone()).unwrap();
        db.create_record("notes", serde_json::json!({"title": "Original"}))
            .unwrap();
        let source = db_path.with_extension("db.backup");
        db.export_to_file(&source).unwrap();
        let source = source.canonicalize().unwrap();
        db.create_record("notes", serde_json::json!({"title": "Added later"}))
            .unwrap();

        let backup = backup_database_for_import(&db, &source).unwrap();
        assert_ne!(backup.canonicalize().unwrap(), source);
        db.replace_from_file(&source).unwrap();
        assert_eq!(db.get_collection("notes").unwrap().len(), 1);
        let saved_current = crate::db::XdbDatabase::open(backup).unwrap();
        assert_eq!(saved_current.get_collection("notes").unwrap().len(), 2);
    }
}
