//! XDB Demo Application
//!
//! This demo showcases the XDB library for building local-first,
//! peer-to-peer applications with Tauri.

use tauri::{Manager, RunEvent};
use tracing::info;
use tracing_subscriber::EnvFilter;

// Re-export XDB types for convenience
use xdb::SharedNetwork;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize tracing/logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting XDB Demo Application");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // Setup XDB with default configuration
            // This initializes the database and P2P network
            xdb::tauri::setup_xdb(app)?;

            info!("XDB initialized successfully");
            Ok(())
        })
        // Register all XDB commands
        .invoke_handler(tauri::generate_handler![
            xdb::tauri::create_record,
            xdb::tauri::update_record,
            xdb::tauri::delete_record,
            xdb::tauri::get_record,
            xdb::tauri::get_collection,
            xdb::tauri::get_collections,
            xdb::tauri::get_db_stats,
            xdb::tauri::get_network_status,
            xdb::tauri::request_sync,
            xdb::tauri::export_database,
            xdb::tauri::import_database,
            xdb::tauri::get_db_path,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                // Gracefully shutdown XDB network
                let network = app_handle.state::<SharedNetwork>();
                tauri::async_runtime::block_on(async {
                    xdb::tauri::shutdown_xdb(&network).await;
                });
            }
        });
}
