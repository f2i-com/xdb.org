//! # XDB - Local-First P2P Database
//!
//! A local-first, peer-to-peer database library with CRDT-based conflict resolution
//! and automatic P2P synchronization via libp2p.
//!
//! ## Features
//!
//! - **Local-First**: All data stored locally in SQLite
//! - **P2P Sync**: Automatic discovery and sync via mDNS and GossipSub
//! - **CRDT Conflict Resolution**: Concurrent edits automatically merged using Yrs
//! - **Tauri Integration**: Optional Tauri command handlers (enabled by default)
//!
//! ## Usage
//!
//! ### As a Rust Library
//!
//! ```rust,ignore
//! use xdb::{XdbDatabase, create_shared_db};
//! use std::path::PathBuf;
//!
//! // Create/open a database
//! let db = create_shared_db(PathBuf::from("./my-data.sqlite")).unwrap();
//!
//! // Create a record
//! let mut db_lock = db.lock().unwrap();
//! let (record, update) = db_lock.create_record(
//!     "notes",
//!     serde_json::json!({"title": "Hello", "content": "World"})
//! ).unwrap();
//! ```
//!
//! ### With Tauri
//!
//! Add XDB to your Tauri app's `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! xdb = { path = "../crates/xdb" }  # or from crates.io
//! ```
//!
//! Then in your `lib.rs`:
//!
//! ```rust,ignore
//! use xdb::tauri::{setup_xdb, get_xdb_commands};
//!
//! pub fn run() {
//!     tauri::Builder::default()
//!         .setup(|app| {
//!             setup_xdb(app)?;
//!             Ok(())
//!         })
//!         .invoke_handler(tauri::generate_handler![
//!             // Include XDB commands
//!             xdb::tauri::create_record,
//!             xdb::tauri::update_record,
//!             // ... other commands
//!         ])
//!         .run(tauri::generate_context!())
//!         .expect("error running app");
//! }
//! ```

mod db;
mod network;

#[cfg(feature = "tauri-commands")]
pub mod tauri;

// Re-export main types
pub use db::{
    create_shared_db, DbError, DbResult, DbStats, Record, SharedDb, XdbDatabase,
};

#[cfg(feature = "tauri-commands")]
pub use tauri::{DbManager, SharedDbManager};

pub use network::{
    create_shared_network, NetworkCommand, NetworkEvent, NetworkMessage, NetworkNode,
    PeerInfo, SharedNetwork,
};

/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
