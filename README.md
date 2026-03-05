# XDB - Local-First P2P Database

A modular, local-first, peer-to-peer database library for Tauri applications with automatic synchronization via CRDTs.

## Features

- **Local-First:** All data stored locally in SQLite - works offline
- **P2P Sync:** Automatic peer discovery via mDNS and real-time sync via GossipSub
- **CRDT Conflict Resolution:** Concurrent edits automatically merged using Yrs (Y-CRDT)
- **Modular Design:** Use as a Rust crate (`xdb`) and/or React npm package (`@xdb/react`)
- **Tauri Integration:** Ready-to-use commands and event handlers
- **Cross-Platform:** Build for Linux, Windows, and macOS

## Project Structure

```
xdb-org/
├── crates/
│   └── xdb/                      # Rust library crate
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs            # Public API & exports
│           ├── db.rs             # SQLite + CRDT logic
│           ├── network.rs        # libp2p P2P networking
│           └── tauri.rs          # Tauri command handlers
│
├── packages/
│   └── xdb-react/                # npm package (@xdb/react)
│       ├── package.json
│       ├── tsup.config.ts        # Build configuration
│       └── src/
│           ├── index.ts          # Main exports
│           ├── hooks/index.ts    # React hooks
│           └── types/index.ts    # TypeScript definitions
│
├── apps/
│   └── demo/                     # Demo Tauri application
│       ├── package.json
│       ├── src/                  # React frontend
│       │   ├── App.tsx           # Demo UI components
│       │   └── App.css           # Styles
│       └── src-tauri/            # Tauri backend (uses xdb crate)
│           ├── Cargo.toml
│           └── src/
│               └── lib.rs        # App entry point
│
├── Cargo.toml                    # Cargo workspace root
├── package.json                  # npm workspace root
└── rust-toolchain.toml           # Rust nightly configuration
```

## Demo Application

The demo app showcases XDB functionality with three collections:

- **Notes** - Colored sticky notes with title, content, and color picker
- **Tasks** - Priority-based todo list with completion tracking (low/medium/high)
- **Contacts** - Contact management with name, email, and phone

Features demonstrated:
- Real-time CRUD operations
- P2P sync status display
- Connected peers list
- Database statistics (record count, size)
- Database export/import (backup & restore)

## Prerequisites

- **Node.js** 18+ (with npm)
- **Rust** nightly (automatically configured via `rust-toolchain.toml`)

### Platform-specific dependencies

**Linux (Debian/Ubuntu):**
```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev
```

**Windows (for native builds):**
- Visual Studio Build Tools with C++ workload, or
- MinGW-w64

**Windows cross-compilation from Linux:**
```bash
# Install MinGW-w64
sudo apt install mingw-w64

# Add Rust target
rustup target add x86_64-pc-windows-gnu
```

## Installation

```bash
# Clone the repository
git clone https://github.com/f2i-com/xdb.org.git
cd xdb-org

# Install npm dependencies
npm install

# Build the React package
npm run build:lib
```

## Running the Demo

### Development mode
```bash
npm run tauri:dev
```

### Production builds

**Build for current platform:**
```bash
npm run tauri:build
```

**Build for Windows (cross-compile from Linux):**
```bash
npm run tauri:build:windows
```

Build outputs:
- **Linux:** `target/release/bundle/`
- **Windows:** `target/x86_64-pc-windows-gnu/release/bundle/nsis/`

## Using XDB in Your Project

### 1. Add the Rust Crate

In your Tauri app's `Cargo.toml`:

```toml
[dependencies]
xdb = { path = "../path/to/crates/xdb" }
# or when published: xdb = "1.0"
```

In your `lib.rs`:

```rust
use tauri::{Manager, RunEvent};
use xdb::SharedNetwork;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // Initialize XDB (database + P2P network)
            xdb::tauri::setup_xdb(app)?;
            Ok(())
        })
        // Register XDB commands
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
        .expect("error building app")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                // Graceful shutdown
                let network = app_handle.state::<SharedNetwork>();
                tauri::async_runtime::block_on(async {
                    xdb::tauri::shutdown_xdb(&network).await;
                });
            }
        });
}
```

### 2. Add the React Package

```bash
npm install @xdb/react
```

In your React components:

```tsx
import {
  useCollection,
  useNetworkStatus,
  useDbStats,
  useDbPath,
  useSyncEvents,
  usePeerEvents,
} from "@xdb/react";

interface Note {
  title: string;
  content: string;
}

function NotesApp() {
  const { records, loading, create, update, remove, requestSync } = useCollection<Note>("notes");
  const { status } = useNetworkStatus();
  const { stats } = useDbStats();

  // Listen for sync events
  useSyncEvents((event) => {
    console.log(`Collection ${event.collection} synced`);
  });

  // Listen for peer events
  usePeerEvents((event) => {
    console.log(`Peer ${event.peer_id} ${event.type}`);
  });

  const handleCreate = async () => {
    await create({ title: "New Note", content: "Hello World!" });
  };

  if (loading) return <div>Loading...</div>;

  return (
    <div>
      <p>Status: {status?.is_running ? "Online" : "Offline"}</p>
      <p>Connected Peers: {status?.connected_peers.length ?? 0}</p>
      <p>Total Records: {stats?.record_count ?? 0}</p>

      {records.map((record) => (
        <div key={record.id}>
          <h3>{record.data.title}</h3>
          <p>{record.data.content}</p>
          <button onClick={() => update(record.id, { ...record.data, title: "Updated" })}>
            Update
          </button>
          <button onClick={() => remove(record.id)}>Delete</button>
        </div>
      ))}

      <button onClick={handleCreate}>Add Note</button>
      <button onClick={requestSync}>Sync Now</button>
    </div>
  );
}
```

## API Reference

### React Hooks

| Hook | Description |
|------|-------------|
| `useCollection<T>(name, options?)` | Manage a collection with CRUD operations |
| `useDbStats(pollInterval?)` | Get database statistics (records, collections, size) |
| `useNetworkStatus(pollInterval?)` | Get P2P network status and connected peers |
| `useDbPath()` | Get the database file path |
| `useDbExport()` | Export database to a file |
| `useDbImport()` | Import database from a file |
| `useSyncEvents(callback)` | Listen for sync events |
| `usePeerEvents(callback)` | Listen for peer connect/disconnect events |

#### useCollection Options

```typescript
interface UseCollectionOptions {
  autoRefresh?: boolean;  // Auto-refresh on sync events (default: true)
  pollInterval?: number;  // Background polling interval in ms (default: none)
}
```

### Tauri Commands

| Command | Description |
|---------|-------------|
| `create_record` | Create a new record in a collection |
| `update_record` | Update an existing record |
| `delete_record` | Soft delete a record |
| `get_record` | Get a single record by ID |
| `get_collection` | Get all records in a collection |
| `get_collections` | List all collection names |
| `get_db_stats` | Get database statistics |
| `get_network_status` | Get P2P network status |
| `request_sync` | Request sync from peers |
| `export_database` | Export database to file |
| `import_database` | Import database from file |
| `get_db_path` | Get database file path |

### Events

| Event | Payload | Description |
|-------|---------|-------------|
| `xdb-sync-event` | `{ type, collection }` | Fired when sync data is received from peers |
| `xdb-peer-event` | `{ type, peer_id, addresses? }` | Fired when peers connect or disconnect |
| `db-imported` | `()` | Fired after database import completes |

## Tech Stack

| Component | Library | Version | Description |
|-----------|---------|---------|-------------|
| App Framework | `tauri` | 2.x | Cross-platform desktop apps |
| P2P Networking | `libp2p` | 0.55 | TCP, mDNS discovery, GossipSub messaging |
| Async Runtime | `tokio` | 1.x | Async runtime for Rust |
| Storage Engine | `rusqlite` | 0.31 | SQLite with bundled support |
| Conflict Resolution | `yrs` | 0.21 | Y-CRDT implementation for Rust |
| Serialization | `serde` | 1.x | Data serialization |
| Frontend | `react` | 19.x | UI framework |

## Architecture

### Data Flow

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   React UI  │────▶│  Tauri IPC  │────▶│  XDB Core   │
└─────────────┘     └─────────────┘     └──────┬──────┘
                                               │
                    ┌──────────────────────────┼──────────────────────────┐
                    │                          ▼                          │
                    │  ┌─────────────┐   ┌─────────────┐   ┌───────────┐  │
                    │  │   SQLite    │◀─▶│   Y-CRDT    │◀─▶│  libp2p   │  │
                    │  │  (Storage)  │   │  (Merging)  │   │  (Sync)   │  │
                    │  └─────────────┘   └─────────────┘   └───────────┘  │
                    │                                            │        │
                    │                      XDB Core              │        │
                    └────────────────────────────────────────────┼────────┘
                                                                 │
                                                                 ▼
                                                        ┌─────────────────┐
                                                        │   Other Peers   │
                                                        │  (via GossipSub)│
                                                        └─────────────────┘
```

### Write Path
1. User action triggers React hook (`create`, `update`, `remove`)
2. Tauri IPC invokes Rust command
3. XDB saves record to SQLite
4. XDB updates CRDT document (Yrs)
5. CRDT delta published via GossipSub to all connected peers
6. Peers receive delta, merge into their local CRDT, update SQLite

### Sync Path
1. libp2p receives message on `xdb-sync` topic
2. CRDT delta decoded and applied to local Yrs document
3. Merged state written to SQLite
4. `xdb-sync-event` emitted to frontend via Tauri
5. React hooks automatically refresh affected collections

### Peer Discovery
- **mDNS:** Automatic discovery of peers on local network
- **GossipSub:** Pub/sub messaging for sync updates
- **Identify:** Protocol for peer identification

## Scripts Reference

| Script | Description |
|--------|-------------|
| `npm install` | Install all workspace dependencies |
| `npm run build` | Build all packages |
| `npm run build:lib` | Build @xdb/react package only |
| `npm run build:demo` | Build demo frontend only |
| `npm run dev` | Run demo in development mode |
| `npm run tauri:dev` | Run Tauri demo with hot reload |
| `npm run tauri:build` | Build for current platform |
| `npm run tauri:build:windows` | Cross-compile for Windows (x86_64-pc-windows-gnu) |

## Notes

- **Rust Nightly Required:** The project uses Rust nightly due to dependency requirements. This is configured automatically via `rust-toolchain.toml`.
- **Network Discovery:** Peers are discovered via mDNS on the local network. Ensure your firewall allows mDNS traffic (UDP port 5353).
- **Port:** The P2P network listens on a random available TCP port.
- **Database Location:** The SQLite database is stored in the app's data directory (platform-specific).

## License

MIT
