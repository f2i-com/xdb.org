# XDB Rust Migration & Architecture Plan

Porting XDB from pure TypeScript to pure safe Rust (`xdb-rs`), targeting native Rust, `wasm32-unknown-unknown`, and first-class sandboxed embedding in **ZIPP** (`f2i-com/zipp.org`).

## Design Principles

1. **`#![forbid(unsafe_code)]` everywhere in the core engine** (`xdb-core`, `xdb-sqlite`, `xdb-vfs-fs`, `xdb-wasm`). This preserves `zipp-sandbox` and `zipp-wasm`'s compile-time memory-safety invariant (which C SQLite cannot satisfy).
2. **Zero external runtime dependencies in `xdb-core` and `xdb-sqlite`.** Keeps the audited TCB tiny and the `.wasm` artifact small.
3. **Metering built into the core (`xdb_core::meter::Meter`).** Page reads, B-tree traversal steps, record decodes, and later query operators charge deterministic step and memory budgets so `zipp-vm` can wire its own `SandboxCaps` directly into XDB.
4. **Reuse the existing TypeScript differential oracle & fixtures.** SQLite 3.54.1 fixture databases + goldens (25 DBs), `tools/oracle`, `tools/slt` (18,124 TCL SELECT cases + 8,152 `select1–4`), and `fuzzdata1–8.db` stay the ground truth.
5. **Isolated `xdb-wasm` workspace** (matching `zipp-wasm`), pinned to Rust `1.92` and `wasm-bindgen = "=0.2.126"`, built with `overflow-checks = true`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`.

## Phases

- **Phase 0 — Foundations (`xdb-core`, `xdb-vfs-fs`)** *(complete)*
  - Sandbox metering (`Meter`, `NoopMeter`, `BudgetMeter`, `ResourceLimits`).
  - Full SQLite 3.54.1 value semantics: `SqlValue` (native `i64`, `f64`, raw-byte `Text`, `Blob`), `TextEncoding` + UTF-8/UTF-16 transcoding, `fp_core` / `fp_format` (`%!.17g`), `atof` / `atoi64` / `dec_or_hex_to_i64` / `get_int32`, numeric conversions, affinity, `BINARY`/`NOCASE`/`RTRIM` collations, exact `int_float_compare` & `mem_compare`, `CAST`, arithmetic & bitwise operators.
  - Synchronous VFS traits (`Vfs`, `VfsFile`, `ByteSource`), SQLite `IoErrorCode`, 5-level `LockManager`, quota-capped `MemoryVfs`, and `StdFsVfs`.
- **Phase 1 — SQL Parser, SQLite File Format, Snapshots, Schema & Logical Reader (`xdb-parser`, `xdb-sqlite`, `xdb-wasm`)** *(complete)*
  - Full SQLite 3.54.1 + XDB dialect LALR(1) parser (`xdb-parser`): static Lemon `.rodata` tables (412 SQLite + 18 XDB rules), metered driver, complete AST, `build.c` `build_table` / `build_index`.
  - Format decoders (`varint`, `header`, `db_header`, `page`, `ptrmap`, `record`).
  - Stage S1 read-only snapshots: in-heap WAL recovery (`wal.rs`), in-memory hot-journal playback (`journal.rs`, `image.rs`), `open_snapshot`, `open_snapshot_from_bytes`, `open_snapshot_from_image`.
  - Metered LRU `Pager`, lazy `RecordView` with overflow chains & corruption guards, `TableCursor` & `IndexCursor`, `Btree`.
  - `sqlite3InitOne` schema catalog (`SchemaCatalog`), `ALTER TABLE ADD COLUMN` defaults (`value_from_expr` / `real_read_back`), `SearchKey` (`sqlite3VdbeRecordCompareWithSkip`), and `SqliteDatabase` / `TableReader` / `RowCursor` / `IndexReader` / `IndexEntryCursor`.
  - Verified 100% exact parity on all 25 SQLite 3.54.1 fixtures + 3 WAL main-only goldens (**964,103 cells + 49,957 catalog checks** in 1.13s release) and **0 panics / 0 hangs** across all **11,210 malformed `fuzzdata1–8.db` images** (0.28s release).
  - `xdb-wasm` verified compiling cleanly to `wasm32-unknown-unknown` (**416 KB raw / 142.9 KB Brotli** with `wasm-opt -Oz`, `overflow-checks = true`, `#![forbid(unsafe_code)]`).
- **Phase 2 — SQL Engine (`xdb-sql`, `xdb-sqlite::sql`, `xdb-wasm`)** *(complete)*
  - Storage-agnostic SQLite 3.54.1 + XDB dialect `SELECT` engine in `crates/xdb-sql` (`#![forbid(unsafe_code)]`, 0 external deps, metered end-to-end).
  - Full built-in function library: core scalars (`func.c`), `printf`/`format`, non-recursive `LIKE`/`GLOB`/`ILIKE`/`ESCAPE`, 26 math functions, exact `date.c` date/time & modifiers, JSON1 (`->`, `->>`, `json_extract`, `json_set`/`insert`/`replace`/`remove`, `json_patch`, `json_pretty`, `json_group_*`, `json_each`, `json_tree`), `generate_series`, aggregates (`count`/`sum`/`total`/`avg`/`min`/`max`/`group_concat`/`string_agg`/`percentile`/`median` with `xInverse`, `FILTER`, `DISTINCT`, aggregate `ORDER BY`, bare-column `min`/`max`), and full window functions + `ROWS`/`RANGE`/`GROUPS`/`EXCLUDE` framing.
  - Planner & executor: `WITH`/`WITH RECURSIVE` CTEs, views, `VALUES`, `INNER`/`CROSS`/`LEFT`/`RIGHT`/`FULL`/`NATURAL`/`USING` joins (hash equi-join + nested-loop), correlated subqueries, row-value vectors, compound `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`, `ORDER BY` (`NULLS FIRST`/`LAST`, ordinals, aliases), `LIMIT`/`OFFSET`, and XDB dialect (`FROM`-first, `GROUP BY ALL`, `ORDER BY ALL`, `::`, `* EXCLUDE`/`REPLACE`, `ILIKE`).
  - Wired into `crates/xdb-sqlite` (`SqliteDatabase::query` / `query_with_params` / `query_with_options`) with `sqlite_schema`, `pragma_table_info`/`table_xinfo`/`index_list`/`index_info`/`index_xinfo`/`database_list` TVFs, topological `VIRTUAL` generated column evaluation, and $O(\log N)$ rowid & secondary-index B-tree seek pushdown.
  - Verified **100% parity on ALL 957,246 cells across all 25 SQLite 3.54.1 fixtures — including all 960 `VIRTUAL` generated column cells (0 excluded!)** — plus real SQL queries, $O(\log N)$ page-read verification, UTF-16/WAL SQL queries, and deterministic `BudgetMeter` halts.
  - Exposed `query_json` / `query_xdb_json` in `crates/xdb-wasm`: **764.8 KB raw / 308.2 KB gzip-9 / 251.8 KB Brotli** (`wasm-opt -Oz`, `overflow-checks = true`, `#![forbid(unsafe_code)]`), verified end-to-end in Node.js WASM.
- **Phase 3 — Writes & Native Storage (Stage S2: `xdb-sqlite::write`, `xdb-wasm::WasmConnection`)** *(complete)*
  - Exact SQLite 3 binary record encoder (`encode_record`, Huffman `put_varint`, schema format 4 serial types 0..9 and 12+, UTF-8/UTF-16).
  - Copy-on-write `WritePager`: freelist trunk/leaf page allocation & reclamation, 100-byte SQLite 3 header management, nested transaction & savepoint undo stack, and genuine SQLite 3 rollback journal (`<path>-journal` with `JOURNAL_MAGIC`, 4096-byte sector headers, `journal_checksum`, sync-journal → write-main → sync-main → delete-journal), metered end-to-end.
  - Full SQLite 3 binary B-tree writer (`write/btree.rs`) for both Table B-trees (`0x0d`/`0x05`) and Index B-trees (`0x0a`/`0x02`): overflow page chains, balanced multi-level splits (`balance_deeper` keeping root page fixed), interior index deletion (predecessor swap + root collapse), `drop_btree`, and full structural `PRAGMA integrity_check`.
  - Full read-write `SqliteConnection` (`write/connection.rs`): multi-statement `execute` / `execute_with_params` / `execute_with_dialect`, DDL (`CREATE TABLE` incl. `INTEGER PRIMARY KEY AUTOINCREMENT` + `sqlite_sequence`, `WITHOUT ROWID`, `STRICT`, `NOT NULL`, `UNIQUE`, `CHECK`, `DEFAULT`, `GENERATED ALWAYS AS ... VIRTUAL|STORED`, `CREATE TABLE ... AS SELECT`; `CREATE [UNIQUE] INDEX` incl. partial & expression indexes; `CREATE VIEW`; `DROP TABLE/INDEX/VIEW/TRIGGER`; `ALTER TABLE` `RENAME TO` / `ADD COLUMN` / `RENAME COLUMN` / `DROP COLUMN`), DML (`INSERT`, `ON CONFLICT`, `UPSERT` with `excluded.*`, `UPDATE ... FROM`, `DELETE`, `RETURNING`), live `changes()` / `total_changes()` / `last_insert_rowid()`, transactions & nested savepoints, and `PRAGMA` statements.
  - Verified with **real native C SQLite (`node:sqlite` 3.53.3) `PRAGMA integrity_check == "ok"`** and exact query parity across DDL/DML/ALTER/UPSERT, 400-row 512-byte-page multi-level splits + 2,600-byte overflow chains + 75% mass deletes + freelist reuse, nested savepoints, **fault-injecting VFS mid-commit crash recovery** (recovered cleanly by both `xdb-sqlite` `HotJournalOverlay` AND real C SQLite), and deterministic `BudgetMeter` statement-rollback halts (31/31 workspace release tests pass in ~1.5s, 0 clippy warnings).
  - Exposed read-write `WasmConnection` (`execute_json`, `execute_xdb_json`, `export_bytes`, `meter_usage_json`) in `crates/xdb-wasm`: **976.0 KB raw / 384.9 KB gzip-9 / 312.0 KB Brotli** (`wasm-opt -Oz`, `overflow-checks = true`, `#![forbid(unsafe_code)]`), verified end-to-end in Node.js WASM including exporting `.db` bytes and passing real C SQLite `PRAGMA integrity_check == "ok"`.
- **Phase 4 — ZIPP Sandboxed Integration** *(next)*
  - Host capability bridge for `zipp-vm` / `zipp-sandbox` / `zipp-wasm`: parent app grants named databases (`readOnly`, `writable`, `quota`, in-memory or virtual/OPFS-backed `Vfs`), sharing ZIPP's instruction and heap meters.
  - Optional `zipp-xdb-package.wasm` loadable module following the `zipp-torch-package.wasm` ABI pattern.
  - Language bindings for ZIPP JS/TS, Python (`sqlite3`), and PHP (`PDO`/`SQLite3`).
- **Phase 5 — Multi-Format Federation (DuckDB-style)**
  - Feature-gated / plugin format drivers: CSV (DFA + sniffer), JSON/NDJSON, Parquet, Arrow IPC, `ATTACH`.
