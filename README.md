# XDB

**A pure safe Rust (`#![forbid(unsafe_code)]`) SQLite 3-compatible database engine for native Rust and WebAssembly.**

XDB implements the SQLite 3 binary file format, B-tree storage engine, rollback-journal ACID transactions, full SQLite 3.54.1 SQL engine, and XDB ergonomic SQL extensions in 100% safe Rust — with **zero C toolchain**, **zero `unsafe` code**, **zero external runtime dependencies** in its core crates, and **built-in deterministic sandbox metering**.

Designed to run standalone (native Rust library, browser/Node/Bun/Deno WebAssembly) and as the sandboxed SQLite storage engine for [ZIPP](https://github.com/f2i-com/zipp.org) and [SoftN](https://github.com/f2i-com/softn.com).

---

## Highlights

- **Zero `unsafe`, zero C dependencies:** Every crate enforces `#![forbid(unsafe_code)]` with `overflow-checks = true` and `panic = "abort"` in release builds. No `libsqlite3`, no FFI, no Emscripten.
- **Real SQLite 3 binary format compatibility:** Reads and writes standard SQLite 3 `.db` files (format 4 records, table & index B-trees, overflow chains, freelist trunk/leaf pages, WAL recovery, hot-journal playback, ACID rollback journals, nested savepoints). Exported `.db` binaries pass native C SQLite `PRAGMA integrity_check == "ok"`.
- **SQLite 3.54.1-exact semantics:** Native `i64` / `f64`, raw-byte `TEXT` (preserving non-UTF-8 byte sequences losslessly), exact `sqlite3AtoF` / `sqlite3Atoi64` / `sqlite3FpDecode` (`%!.17g`), exact `BINARY` / `NOCASE` / `RTRIM` collations across UTF-8 / UTF-16LE / UTF-16BE, and exact `sqlite3IntFloatCompare`.
- **Complete SQL engine + XDB dialect:**
  - Full DDL (`CREATE TABLE` incl. `INTEGER PRIMARY KEY AUTOINCREMENT`, `WITHOUT ROWID`, `STRICT`, `CHECK`, `UNIQUE`, `DEFAULT`, `GENERATED ALWAYS AS ... VIRTUAL|STORED`; `CREATE [UNIQUE] INDEX` incl. partial & expression indexes; `CREATE VIEW`; `ALTER TABLE`; `DROP`).
  - Full DML (`INSERT`, `ON CONFLICT` / `UPSERT` with `excluded.*`, `UPDATE ... FROM`, `DELETE`, `RETURNING`), transactions & nested `SAVEPOINT`, `PRAGMA`.
  - Full `SELECT`: `WITH` / `WITH RECURSIVE` CTEs, views, `INNER`/`LEFT`/`RIGHT`/`FULL`/`CROSS`/`NATURAL`/`USING` joins, correlated subqueries, compound `UNION`/`INTERSECT`/`EXCEPT`, window functions (`ROWS`/`RANGE`/`GROUPS`/`EXCLUDE`), JSON1 (`->`, `->>`, `json_*`, `json_each`, `json_tree`), `generate_series`, math, date/time, `printf`, aggregates with `FILTER`/`DISTINCT`/`ORDER BY`, and $O(\log N)$ rowid & secondary-index B-tree seek pushdown.
  - **XDB dialect extensions:** `FROM`-first queries, `GROUP BY ALL`, `ORDER BY ALL`, `::` cast operator, `* EXCLUDE(...)` / `* REPLACE(...)`, and `ILIKE`.
- **Deterministic sandbox metering (`xdb_core::meter`):** Explicit ceilings on work units (`max_steps`), heap allocations (`max_memory_bytes`), and page reads (`max_page_reads`) so untrusted SQL or corrupt database files cannot hang or OOM the host.
- **Compact hardened WebAssembly (~312 KB Brotli):** Isolated `crates/xdb-wasm` workspace (`wasm-bindgen = "=0.2.126"`, `opt-level = "z"`, `lto = "fat"`, `overflow-checks = true`) exposing both read-only `WasmDatabase` and full read-write `WasmConnection` with `seed_json()` and `export_bytes()`.

---

## Workspace Crates

| Crate | External Deps | Description |
| --- | --- | --- |
| [`crates/xdb-core`](crates/xdb-core) | 0 | Sandbox metering (`Meter`, `BudgetMeter`, `ResourceLimits`), SQLite 3.54.1 `SqlValue` & collation semantics, VFS traits, 5-level `LockManager`, quota-enforced `MemoryVfs`. |
| [`crates/xdb-parser`](crates/xdb-parser) | 0 | SQLite 3.54.1 + XDB dialect tokenizer, static Lemon LALR(1) tables (412 SQLite + 18 XDB rules), metered parser driver, full AST, and schema DDL builder. |
| [`crates/xdb-sql`](crates/xdb-sql) | 0 | Storage-agnostic metered SQLite 3.54.1 + XDB `SELECT` engine (scalars, JSON1, date/time, math, aggregates, window framing, TVFs, joins, CTEs, subqueries, compounds, virtual generated columns). |
| [`crates/xdb-vfs-fs`](crates/xdb-vfs-fs) | 0 | Safe `std::fs` VFS implementation for native hosts (read-only and exclusive write). |
| [`crates/xdb-sqlite`](crates/xdb-sqlite) | 0 | SQLite 3 binary file format, Stage S1 snapshots (WAL + hot-journal recovery), Stage S2 read-write engine (`SqliteConnection`, `WritePager`, rollback journal, Table & Index B-tree writer, `seed_json`, `PRAGMA integrity_check`), metered LRU pager, and $O(\log N)$ seek pushdown. |
| [`crates/xdb-wasm`](crates/xdb-wasm) | `wasm-bindgen` | Isolated WebAssembly workspace exposing `WasmDatabase` (read-only) and `WasmConnection` (read-write DDL/DML/transactions, `seed_json`, `export_bytes`) for browsers, Node, Bun, Deno, and ZIPP. |

---

## Quick Start

### Native Rust

```rust
use xdb_sqlite::{ConnectionOptions, QueryParams, SqliteConnection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = SqliteConnection::open_in_memory(ConnectionOptions::default())?;

    conn.execute(
        "CREATE TABLE tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            done INTEGER NOT NULL DEFAULT 0
        );",
        &QueryParams::empty(),
    )?;

    conn.execute(
        "INSERT INTO tasks (title, done) VALUES ('Ship XDB 0.1.0', 1), ('Run in browser WASM', 0);",
        &QueryParams::empty(),
    )?;

    let res = conn.execute(
        "SELECT id, title, done FROM tasks ORDER BY id;",
        &QueryParams::empty(),
    )?;
    println!("columns: {:?}, rows: {}", res.columns, res.rows.len());

    // Export a genuine SQLite 3 .db binary image
    let sqlite_bytes: Vec<u8> = conn.to_bytes();
    assert!(sqlite_bytes.starts_with(b"SQLite format 3\0"));
    Ok(())
}
```

### Browser / Node / WebAssembly

Download `xdb-<version>-wasm.wasm` (or `.wasm.br`) and `xdb-<version>-wasm.js` from the [GitHub Releases](https://github.com/f2i-com/xdb.org/releases) page, or build them locally from `crates/xdb-wasm`.

```js
import initXdb, { WasmConnection, WasmDatabase } from './xdb_wasm.js';

await initXdb('./xdb_wasm_bg.wasm');

// Open an in-memory read-write SQLite database with explicit sandbox ceilings:
// (initialBytes, walBytes, journalBytes, pageSize, maxSteps, maxMemoryBytes, maxPageReads)
const conn = new WasmConnection(
  undefined,
  undefined,
  undefined,
  4096,
  50_000_000n,       // max work units
  64n * 1024n * 1024n, // max 64 MiB
  200_000n           // max page reads
);

conn.execute_json(`
  CREATE TABLE items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    price_cents INTEGER NOT NULL
  );
  INSERT INTO items (name, price_cents) VALUES ('Espresso', 450), ('Matcha', 550);
`);

// Parameterized queries (positional array or named object)
const out = JSON.parse(
  conn.execute_json('SELECT id, name, price_cents FROM items WHERE price_cents >= ?', JSON.stringify([450]))
);
console.log(out.columns, out.rows);

// Declarative JSON seeding (atomic transaction)
conn.seed_json(JSON.stringify({
  customers: [
    { id: 1, name: 'Ada Lovelace', tier: 'pro' },
    { id: 2, name: 'Grace Hopper', tier: 'pro' },
  ],
}));

// Export a standard SQLite 3 .db Uint8Array (e.g. for IndexedDB persistence or download)
const dbBytes = conn.export_bytes();
console.log('Exported SQLite bytes:', dbBytes.byteLength);

// Re-open snapshot read-only with WasmDatabase
const ro = new WasmDatabase(dbBytes, undefined, undefined, 10_000_000n, 32n * 1024n * 1024n, 50_000n);
console.log(JSON.parse(ro.query_xdb_json('FROM items SELECT name, price_cents / 100.0 AS price')));
ro.close();
conn.close();
```

---

## Verification

- **SQLite 3.54.1 fixture parity:** 100% cell-for-cell parity across 25 SQLite 3.54.1 fixture databases + WAL goldens (**957,246 cells**, including all 960 `VIRTUAL` generated column cells).
- **Fuzz hardening:** 0 panics and 0 hangs across 11,210 malformed SQLite images (`fuzzdata1–8.db`).
- **Native C SQLite cross-checks:** Exported databases from DDL, DML, `UPSERT`, `ALTER TABLE`, multi-level B-tree splits, overflow chains, mass deletes/freelist reuse, nested savepoints, and mid-commit crash-recovery tests pass native C SQLite `PRAGMA integrity_check == "ok"`.
- **Metered sandbox safety:** Statement rollback and clean error returns when step, memory, or page-read ceilings are exceeded.

---

## Building from Source

Requires Rust **1.92.0** (for WASM: `wasm32-unknown-unknown` target, `wasm-bindgen-cli 0.2.126`, and Binaryen `wasm-opt`).

```sh
# Run the full native test suite with warnings denied
RUSTFLAGS="-Dwarnings" cargo +1.92.0 test --locked --workspace

# Build hardened WebAssembly release artifacts (web + nodejs + .wasm.br + SHA256SUMS)
bash crates/xdb-wasm/build-wasm.sh
```

See [RUST_PLAN.md](RUST_PLAN.md) for the phased architecture roadmap and ZIPP/SoftN integration design.

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE).
