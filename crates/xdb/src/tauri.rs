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
    databases: StdMutex<HashMap<String, SharedDb>>,
}

impl DbManager {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            databases: StdMutex::new(HashMap::new()),
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
        let db_dir = self.base_dir.join("apps").join(&app_id);
        std::fs::create_dir_all(&db_dir).map_err(|e| e.to_string())?;
        let db_path = db_dir.join("data.sqlite");
        info!("Opening per-app database: {:?}", db_path);
        let db = create_shared_db(db_path).map_err(|e| e.to_string())?;
        dbs.insert(app_id, db.clone());
        Ok(db)
    }

    /// Get the database file path for a given app ID.
    pub fn get_app_path(&self, app_id: &str) -> PathBuf {
        let app_id = sanitize_app_id(app_id);
        self.base_dir.join("apps").join(&app_id).join("data.sqlite")
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
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
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

    // Legacy single-database path (kept for network init)
    let db_path = app_data_dir.join("xdb.sqlite");
    info!("XDB legacy database path: {:?}", db_path);

    // Initialize the legacy database (for backward compat / network)
    let db = create_shared_db(db_path)
        .map_err(|e| format!("Failed to initialize XDB database: {}", e))?;

    // Create per-app database manager
    let db_manager = Arc::new(DbManager::new(app_data_dir.clone()));
    info!("XDB per-app database directory: {:?}", app_data_dir.join("apps"));

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
    let base_dir = db_path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&base_dir)?;

    info!("XDB database path: {:?}", db_path);

    // Initialize the database
    let db = create_shared_db(db_path)
        .map_err(|e| format!("Failed to initialize XDB database: {}", e))?;

    // Create per-app database manager
    let db_manager = Arc::new(DbManager::new(base_dir));

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
    let net = { network.lock().await.clone() };
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
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let (record, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .create_record(&payload.collection, payload.data)
            .map_err(|e| e.to_string())?
    };

    // Broadcast update to network
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        if let Err(e) = net.broadcast_update(&payload.collection, update).await {
            error!("Failed to broadcast update: {}", e);
        }
    }

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
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let (record, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .update_record(&payload.id, payload.data)
            .map_err(|e| e.to_string())?
    };

    // Broadcast update to network
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        if let Err(e) = net.broadcast_update(&record.collection, update).await {
            error!("Failed to broadcast update: {}", e);
        }
    }

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
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let (collection, update) = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        let record = db_lock.get_record(&id).map_err(|e| e.to_string())?;
        let update = db_lock.delete_record(&id).map_err(|e| e.to_string())?;
        (record.collection, update)
    };

    // Broadcast update to network
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        if let Err(e) = net.broadcast_update(&collection, update).await {
            error!("Failed to broadcast delete: {}", e);
        }
    }

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
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let update = {
        let mut db_lock = db.lock().map_err(|e| e.to_string())?;
        db_lock
            .upsert_record(record.clone())
            .map_err(|e| e.to_string())?
    };

    // Broadcast update to network
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
        if let Err(e) = net.broadcast_update(&record.collection, update).await {
            error!("Failed to broadcast upsert: {}", e);
        }
    }

    Ok(record)
}

/// Get a single record by ID
#[tauri::command]
pub fn get_record(db_manager: State<'_, SharedDbManager>, app_id: Option<String>, id: String) -> Result<Record, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_record(&id).map_err(|e| e.to_string())
}

/// Get all records in a collection
#[tauri::command]
pub fn get_collection(db_manager: State<'_, SharedDbManager>, app_id: Option<String>, collection: String) -> Result<Vec<Record>, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_collection(&collection).map_err(|e| e.to_string())
}

/// Get all collection names
#[tauri::command]
pub fn get_collections(db_manager: State<'_, SharedDbManager>, app_id: Option<String>) -> Result<Vec<String>, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_collections().map_err(|e| e.to_string())
}

/// Clear all records in a collection
#[tauri::command]
pub fn clear_collection(db_manager: State<'_, SharedDbManager>, app_id: Option<String>, collection: String) -> Result<bool, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let mut db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock
        .clear_collection(&collection)
        .map_err(|e| e.to_string())?;
    info!("Cleared collection {}", collection);
    Ok(true)
}

/// Get database statistics
#[tauri::command]
pub fn get_db_stats(db_manager: State<'_, SharedDbManager>, app_id: Option<String>) -> Result<DbStats, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    db_lock.get_stats().map_err(|e| e.to_string())
}

/// Get network status
#[tauri::command]
pub async fn get_network_status(network: State<'_, SharedNetwork>) -> Result<NetworkStatus, String> {
    let net = { network.lock().await.clone() };
    if let Some(net) = net {
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
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
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
pub async fn export_database(db_manager: State<'_, SharedDbManager>, app_id: Option<String>, path: String) -> Result<String, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let export_path = PathBuf::from(&path);

    // Canonicalize parent to resolve path traversal (e.g. ../../etc/passwd)
    let parent = export_path
        .parent()
        .ok_or("Invalid export path: no parent directory")?;
    if !parent.exists() {
        return Err(format!("Export directory does not exist: {}", parent.display()));
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
pub fn get_db_path(db_manager: State<'_, SharedDbManager>, app_id: Option<String>) -> Result<String, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let db_lock = db.lock().map_err(|e| e.to_string())?;
    Ok(db_lock.path().to_string_lossy().to_string())
}

/// Get the base directory where all app databases are stored
#[tauri::command]
pub fn get_db_base_dir(db_manager: State<'_, SharedDbManager>) -> Result<String, String> {
    Ok(db_manager.base_dir().join("apps").to_string_lossy().to_string())
}

/// Import/restore database from a file
#[tauri::command]
pub async fn import_database(
    app: AppHandle,
    db_manager: State<'_, SharedDbManager>,
    app_id: Option<String>,
    source_path: String,
) -> Result<bool, String> {
    let db = db_manager.get_db(&app_id.unwrap_or_default())?;
    let source = PathBuf::from(&source_path);
    if !source.exists() {
        return Err("Source file does not exist".to_string());
    }

    // Canonicalize to resolve path traversal
    let source = source
        .canonicalize()
        .map_err(|e| format!("Invalid import path: {}", e))?;

    // Validate source database integrity before replacing
    let source_conn = rusqlite::Connection::open(&source)
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
    let current_path = db_lock.path().clone();

    // Create backup of current database
    let backup_path = current_path.with_extension("db.backup");
    std::fs::copy(&current_path, &backup_path).map_err(|e| e.to_string())?;

    // Replace the database file and reload in-memory state atomically under lock.
    db_lock
        .replace_from_file(&source)
        .map_err(|e| format!("Failed to replace database after import: {}", e))?;

    info!("Imported database from: {}", source_path);

    // Emit event to notify frontend to reload
    let _ = app.emit("db-imported", ());

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
