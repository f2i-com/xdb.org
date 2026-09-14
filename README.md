# XDB

**Local SQLite storage for Softn's native runtime and Rust server, with a React + Tauri integration and optional peer synchronization.**

XDB stores JSON records in named collections. Applications can work offline, group writes into transactions, and export or restore a SQLite database. The Rust library also maintains Yrs documents for its native peer protocol.

[Where XDB is used](#where-xdb-is-used) · [Run the demo](#run-the-demo) · [Rust](#use-the-rust-library) · [React + Tauri](#use-react--tauri) · [Manual checks](#manual-checks)

## Where XDB is used

XDB is still a live dependency. Its Rust crate and the browser API with the same name have different responsibilities:

| Consumer | What it uses |
| --- | --- |
| [Softn desktop runtime](https://github.com/f2i-com/softn.com/tree/main/apps/softn-loader) | This repository's `xdb` crate. Tauri commands persist each app's records in SQLite. |
| [Softn Rust server](https://github.com/f2i-com/softn.com/tree/main/apps/softn-rust) | This repository's `xdb` crate without Tauri commands. It backs the server's collection API, `.logic` database bridge and synchronized records. |
| [Softn browser runtime, Studio and Builder](https://github.com/f2i-com/softn.com/blob/main/packages/%40softn/core/src/runtime/xdb.ts) | Softn's own TypeScript XDB service. Browser apps use local storage; editors keep preview data separate from the running app. That service invokes this crate's commands when hosted in the native runtime. |
| [FormLogic](https://github.com/f2i-com/formlogic.com) and [Aokie's front desk](https://github.com/f2i-com/softn.com/tree/main/examples/aokie-workspace) | Softn provides the editable app interface. Connected business records go through FormLogic's backend API and its own SQLite storage. Aokie's native call, transcript and message storage uses its own `rusqlite` layer. |
| [OAIY](https://github.com/f2i-com/oaiy.com) and [ZIPP](https://github.com/f2i-com/zipp.org) | No direct dependency on this crate or `@xdb/react`. ZIPP executes app logic; Softn supplies the database bridge. |
| [XDB demo](apps/demo) | Both the Rust crate and the `@xdb/react` hooks from this repository. |

The Softn browser service is maintained in the Softn repository, so improvements to it do not necessarily produce commits here. `@xdb/react` is a separate convenience package for React + Tauri applications; Softn uses its own adapter.

See the [September 2026 review](docs/audit-2026-09-12.md) for the fixes, verification results and remaining design work.

### Three synchronization paths

- **Native XDB peer sync (experimental, opt-in):** libp2p, mDNS discovery, GossipSub messages and Yrs documents. OFF by default: opening a database never starts it; a host enables it explicitly (`set_network_enabled`) and the choice is persisted. The current wire protocol supports the default database only and is a trusted-LAN development feature. See [docs/networking-and-restore-policy.md](docs/networking-and-restore-policy.md).
- **Softn browser peer sync:** Softn's separate Yjs + WebRTC implementation, joined explicitly by app and room.
- **Softn server sync:** Softn's WebSocket client and Rust server. FormLogic's hosted actions use a separate authenticated backend bridge.

These are separate transports. Running a Softn browser app does not start this repository's libp2p network or make it a native XDB peer.

Native writes remain saved locally while offline. There is no delivery queue; convergence comes from retained CRDT state plus anti-entropy: on every new peer connection the node announces its collections and requests reconciliation for each (unknown collections included), and a bounded repair pass repeats that every 30 s while peers are connected. `get_network_status` reports what actually happened (publishes that reached a peer versus none, updates applied versus rejected, last reconcile time) rather than a single "synced" flag.

## Data model

A record contains `id`, `collection`, `data`, `created_at`, `updated_at` and `deleted`. IDs are UUIDs for newly created records. Collections are created as records are written.

- `update_record` shallow-merges object fields; a non-object payload replaces `data`.
- `delete_record` writes a tombstone. `get_collection` hides deleted records; the lower-level `get_record` can return a tombstone.
- `clear_collection` is a LOCAL hard reset, not a replicated deletion: peers repopulate this node on the next reconciliation. A replicated deletion is a tombstone (`delete_record`, or `import_records` with `replace`). An authoritative reset of a shared collection is `reset_collection` with scope `replicated`: it advances the collection's reset epoch, which every sync message carries; peers adopt the reset and updates from peers still on the old epoch are rejected.
- `import_database` takes a `scope`: `local` (this node only; synchronization pauses until `resume_sync`), `fork` (a new isolated namespace whose directory is reserved atomically) or `replace` (this data becomes authoritative for peers through reset epochs, activated together with the data in one step). The pending-restore record is a recovery journal (`phase`, backup, source, plan) written before anything is replaced.
- `resume_sync` refuses an unreadable record or a restore interrupted before it was applied; `recover_restore` (`action`: `rollback` | `complete` | `discard`) resolves those explicitly. While paused, no inbound update, reset or request is applied or answered and no reconciliation runs; see the policy document.
- `import_records` imports several collections in one transaction and returns its summary only after the commit.
- Tombstones and epochs are retained indefinitely; there is no compaction that could make a rejoining peer diverge silently.
- `with_transaction` groups SQLite and CRDT changes. Publish returned deltas only after the containing transaction succeeds.
- Nested transactions use savepoints. A failed operation rolls back its SQLite writes and cached CRDT state, including when the caller catches an inner error.
- Each CRDT map entry contains one serialized record. Concurrent changes to **different records** can merge; concurrent edits to the **same record** resolve to one whole record. This is not field-by-field collaborative editing.
- A `.xdb` file inside a Softn bundle is a JSON schema/seed document. It is not a SQLite database file, and it is not an XDB database backup.

Exports use SQLite snapshots and include recent committed WAL writes. Imports validate database structure, record JSON and saved CRDT documents before replacing existing data. Database size reports the logical SQLite snapshot size, including pages still in the WAL.

## Run the demo

The demo includes notes, tasks and contacts, with CRUD controls, peer status, statistics and database export/import.

### Requirements

- Node.js **22.12 or later** and npm. The checked-in Vite dependency also accepts Node 20.19, but Node 18 cannot run it.
- Rust through `rustup`. This checkout selects the toolchain in [rust-toolchain.toml](rust-toolchain.toml).
- Your platform's Tauri build prerequisites. Windows native builds use the MSVC C++ toolchain and WebView2; Linux and macOS need their corresponding native development dependencies.

```sh
git clone https://github.com/f2i-com/xdb.org.git
cd xdb.org
npm ci
npm run build:lib
npm run tauri:dev
```

The demo uses port **1420**. Stop another development server using that port before starting it. `npm run dev` opens only the frontend; database commands require the Tauri host.

```sh
# Compile the React package and demo frontend
npm run build

# Build the desktop application
npm run tauri:build
```

The checked-in demo bundle configuration targets Windows MSI/NSIS installers. Native bundles are written under `target/release/bundle/`. For another platform, configure the demo's Tauri bundle targets for that platform. The `tauri:build:windows` script selects the Windows GNU target; it is not a complete cross-compilation environment.

## Use the Rust library

For a server or another host that does not need Tauri commands:

```toml
[dependencies]
xdb = { path = "../xdb.org/crates/xdb", default-features = false }
serde_json = "1"
```

```rust
use std::path::PathBuf;
use xdb::{create_shared_db, DbResult};

fn save_notes() -> DbResult<()> {
    let shared = create_shared_db(PathBuf::from("notes.sqlite"))?;
    let mut db = shared.lock().expect("database lock poisoned");

    db.with_transaction(|db| {
        db.create_record("notes", serde_json::json!({ "title": "First note" }))?;
        db.create_record("notes", serde_json::json!({ "title": "Second note" }))?;
        Ok(())
    })?;

    let notes = db.get_collection("notes")?;
    println!("{} saved notes", notes.len());
    Ok(())
}
```

Opening `XdbDatabase` or calling `create_shared_db` does not start networking. `default-features = false` disables the Tauri integration; networking types remain available for hosts that explicitly create a network node.

Core methods include:

| Method | Purpose |
| --- | --- |
| `create_record`, `update_record`, `delete_record`, `upsert_record` | Write a record and return its CRDT update. |
| `get_record`, `get_collection`, `get_collections` | Read persisted records and collection names. |
| `with_transaction` | Run a group of operations in one transaction. |
| `get_full_state`, `get_state_vector`, `get_updates_since`, `apply_remote_update` | Integrate a host's synchronization transport. |
| `export_to_file`, `replace_from_file` | Export and restore SQLite storage. |
| `plan_authoritative_restore`, `apply_authoritative_restore`, `replace_from_file_authoritative` | Compute the reset plan without touching the live database, then activate snapshot and epochs in one step (R3-XD-02). |
| `clear_collection`, `get_stats` | Local reset of a collection or inspect database statistics. |
| `import_records` | Import several collections in ONE transaction; summary returned after the commit (SN-01). |
| `update_records` | Update several records in ONE transaction with one CRDT snapshot per touched collection; all-or-nothing (XD-04). |
| `reset_collection`, `bump_epoch`, `get_epoch`, `apply_remote_reset`, `apply_remote_update_at_epoch` | Reset epochs for replicated resets and stale-peer rejection (XD-03). |
| `reconcile_plan`, `unknown_collections` | Inputs for join/repair reconciliation (XD-02). |

## Use React + Tauri

The [demo's native entry point](apps/demo/src-tauri/src/lib.rs) shows initialization, command registration and graceful network shutdown. Use the crate's default features for a Tauri host:

```toml
[dependencies]
xdb = { path = "../xdb.org/crates/xdb" }
```

Call `xdb::tauri::setup_xdb(app)` during Tauri setup and register the commands your UI uses with `tauri::generate_handler!`. Build this repository's React package before adding it to a sibling application:

```sh
# In the XDB checkout
npm run build:lib

# In your React + Tauri application
npm install ../xdb.org/packages/xdb-react
```

```tsx
import { useCollection } from "@xdb/react";

interface Note {
  title: string;
}

export function Notes() {
  const { records, loading, error, mutating, create, remove } =
    useCollection<Note>("notes", { appId: "my-notes-app" });

  if (loading) return <p>Opening notes…</p>;

  return (
    <section>
      <h1>Notes</h1>
      {error && <p role="alert">{error}</p>}
      <button disabled={mutating} onClick={() => create({ title: "New note" })}>
        Add note
      </button>
      {records.map(note => (
        <div key={note.id}>
          <span>{note.data.title}</span>
          <button disabled={mutating} onClick={() => remove(note.id)}>Delete</button>
        </div>
      ))}
    </section>
  );
}
```

### App scope and storage

Pass the same stable `appId` to every database hook for an app. Tauri `invoke` calls use `appId` in their arguments, while Rust commands call the parameter `app_id`.

With `setup_xdb`, the default database is `<app data>/apps/_default/data.sqlite`. Named apps use `<app data>/apps/<sanitized app ID>/data.sqlite`. Empty or omitted IDs select `_default`. The host should choose stable identifiers; sanitized names are a storage namespace, not user authentication.

`setup_xdb_with_path` uses the supplied path for the default database. Named app databases are stored beneath its parent directory in `apps/<sanitized app ID>/data.sqlite`.

Only the default database participates in native peer sync. Named app writes stay local, and `request_sync` reports that native sync is unavailable for that scope. App-scoped replication needs a transport with explicit app identity; Softn's browser and server implementations live in the Softn repository.

`setup_xdb` starts the native default peer network automatically. Use the Rust database API directly when a host needs storage without that network. The existing `xdb-sync` protocol discovers peers on the local network; it does not provide account login or per-user permissions.

### Hooks

| Hook | Purpose |
| --- | --- |
| `useCollection<T>(name, options?)` | Records, loading/error state, CRUD, refresh and sync request. |
| `useFind<T>(name, options?)` | Filter, sort and paginate a collection locally. |
| `useDbStats(pollInterval?, appId?)` | Record/collection counts and database size. |
| `useDbPath(appId?)` | Resolved database path. |
| `useDbExport(appId?)`, `useDbImport(appId?)` | Backup and restore controls. Import replaces the selected database. |
| `useNetworkStatus(pollInterval?)` | Default network status and connected peers. |
| `useSyncEvents(callback)`, `usePeerEvents(callback)` | Native network notifications. |

`useCollection` accepts `appId`, `autoRefresh`, `pollInterval`, `optimisticUpdates`, `initialData`, `sortBy` and `sortOrder`. `useFind` accepts `appId`, filters, sort and pagination options. See the [TypeScript definitions](packages/xdb-react/src/types/index.ts) for exact types.

The hooks invoke native commands; they do not include a standalone browser database fallback.

Nonpositive or nonfinite polling intervals disable background polling. `useFind` reports the count after filtering and before pagination. Statistics, network status and import/export hooks expose errors so the interface can explain a failed operation.

### Native commands

Database commands accept optional `appId`:

- CRUD: `create_record`, `update_record`, `delete_record`, `upsert_record`; `update_records` batches updates in one transaction.
- Reads: `get_record`, `get_collection`, `get_collections`, `get_db_stats`, `get_db_path`.
- Maintenance: `clear_collection` (local), `reset_collection` (`scope`: `local` | `replicated`), `import_records`, `export_database`, `import_database` (`scope`: `local` | `fork` | `replace`).
- Synchronization (default scope only): `request_sync`, `reconcile_network`, `resume_sync`, `recover_restore` (`action`: `rollback` | `complete` | `discard`).
- Networking policy: `get_network_settings`, `set_network_enabled` (persisted opt-in; local-only by default).

`get_network_status` reports the shared default network honestly: `mode`, `enabled`, `discovery`, `listening`, `is_running`, `sync_paused` and `stats` (see the policy document). `get_db_base_dir` reports the directory containing app databases. Register additional commands explicitly if your application uses more than the demo does.

### Events

| Event | Payload and scope |
| --- | --- |
| `xdb-data-event` | Local change: `{ type, app_id, collection? }`. Types are `create`, `update`, `delete`, `upsert`, `clear` and `import`; import omits the collection to refresh the whole selected database. |
| `xdb-sync-event` | Peer update: `{ type, collection }`. The current native protocol uses the default database only. |
| `xdb-peer-event` | Peer connection status: `{ type, peer_id, addresses? }`. |
| `db-imported` | Restore completion: `{ app_id }`. |

Event `app_id` values are canonical sanitized IDs, with `_default` for the default database. `useCollection` filters local events by that scope and refreshes on relevant changes when `autoRefresh` is enabled.

## Manual checks

Run from this repository's root:

```sh
cargo test --locked -p xdb --no-default-features
cargo test --locked -p xdb
cargo clippy -p xdb --all-targets
npm run typecheck -w @xdb/react
npm test -w @xdb/react
npm run build
# persistence / write-amplification baseline (docs/benchmarks.md)
cargo test -p xdb --release -- --ignored --nocapture bench_persistence
```

Rust checks cover database behavior and, with default features enabled, Tauri integration helpers. The second command needs platform-native Tauri build dependencies. Build checks alone do not verify discovery or live synchronization between two machines.

One further test drives two real network nodes over loopback sockets in one process (mDNS discovery, GossipSub delivery, a partition with writes on both sides, a restart under a new peer identity, automatic reconciliation, live delivery afterwards). It needs multicast on the host and takes about 35 seconds, so it is ignored by default:

```sh
cargo test -p xdb -- --ignored two_nodes --nocapture
```

On Windows the multicast route decides which interface the discovery packets leave by; a virtual adapter with a better metric than the real card (WSL's Hyper-V adapter, for one) makes the two nodes invisible to each other and the test fails at discovery. A Linux container has no such problem, and `--network host` or the default bridge both work:

```sh
docker run --rm -v "$PWD:/work" -w /work rustlang/rust:nightly-bookworm   bash -c 'apt-get update -qq && apt-get install -y -qq cmake pkg-config >/dev/null; cargo test -p xdb --no-default-features -- --ignored two_nodes --nocapture'
```

Two nodes in two containers, each with its own network namespace, come closer to the two-host check. `scripts/lan-two-containers.sh` builds the `lan-probe` binary (`crates/xdb/src/bin/lan-probe.rs`: one node as a process that writes one record and waits until it holds the peer's too), starts two of them on a Docker bridge network and exits 0 only when both converged:

```sh
scripts/lan-two-containers.sh
```

Discovery re-queries the LAN every 30 s (`MDNS_QUERY_INTERVAL`) instead of libp2p's five-minute default, so two nodes that start at the same instant, or a peer that restarts, find each other within a minute even when both initial queries were sent before the other side was listening.

For a manual demo check, create a note, update it, restart the app and verify it persists. Export to a new backup file, change the note, then import the backup and verify the restored state. Use a separate test database for this restore check. For peer testing across two machines, enable networking in BOTH demo instances first (it is off by default), use a trusted local network, and verify create/update/delete propagation in the default database; then disconnect one instance, edit on both sides, reconnect and confirm both converge without pressing sync. The loopback test and the two-container run are the single-machine versions of that sequence; a run across two real hosts still has to be recorded separately.

Softn native builds use this crate as a sibling path dependency. Their [dependency checkout script](https://github.com/f2i-com/softn.com/blob/main/.github/scripts/checkout-xdb.sh) pins a specific XDB revision; adopting changes in release builds requires updating that pin as well as the checkout.

## Repository layout

```text
crates/xdb/src/
  db.rs          SQLite storage, transactions and Yrs documents
  network.rs     Native libp2p discovery and synchronization
  tauri.rs       App database manager, commands and UI events
packages/xdb-react/src/
  hooks/         React hooks for the native commands
  types/         Shared TypeScript payloads and options
apps/demo/       React + Tauri demonstration application
```

## License

[MIT](LICENSE).
