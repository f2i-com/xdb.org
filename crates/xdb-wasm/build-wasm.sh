#!/usr/bin/env bash
# Build hardened XDB WebAssembly release artifacts (web + nodejs),
# optimize with wasm-opt -Oz, Brotli-compress, verify under Node.js,
# and emit SHA256SUMS into crates/xdb-wasm/dist/.
set -euo pipefail

caller_pwd="$(pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="${1:-$here/dist}"
case "$OUT_DIR" in
  /*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
  *) OUT_DIR="$caller_pwd/$OUT_DIR" ;;
esac
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
cd "$here"

TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.92.0}"
VERSION="$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1 | tr -d '\r')"

echo "=== Building xdb-wasm v$VERSION (toolchain $TOOLCHAIN)"
RUSTFLAGS="-Dwarnings" cargo +"$TOOLCHAIN" build --locked --release --target wasm32-unknown-unknown

RAW_WASM="target/wasm32-unknown-unknown/release/xdb_wasm.wasm"
WEB_DIR="$OUT_DIR/web"
NODE_DIR="$OUT_DIR/nodejs"
rm -rf "$WEB_DIR" "$NODE_DIR"

wasm-bindgen --target web --out-dir "$WEB_DIR" \
  --remove-name-section --remove-producers-section "$RAW_WASM"
wasm-bindgen --target nodejs --out-dir "$NODE_DIR" \
  --remove-name-section --remove-producers-section "$RAW_WASM"

to_node_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

if command -v wasm-opt >/dev/null 2>&1; then
  echo "=== Running wasm-opt -Oz"
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
    "$WEB_DIR/xdb_wasm_bg.wasm" -o "$WEB_DIR/xdb_wasm_bg.opt.wasm"
  mv "$WEB_DIR/xdb_wasm_bg.opt.wasm" "$WEB_DIR/xdb_wasm_bg.wasm"
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
    "$NODE_DIR/xdb_wasm_bg.wasm" -o "$NODE_DIR/xdb_wasm_bg.opt.wasm"
  mv "$NODE_DIR/xdb_wasm_bg.opt.wasm" "$NODE_DIR/xdb_wasm_bg.wasm"
elif [[ "${REQUIRE_WASM_OPT:-0}" = "1" ]]; then
  echo "ERROR: wasm-opt not found on PATH (REQUIRE_WASM_OPT=1)" >&2
  exit 1
else
  echo "WARNING: wasm-opt not found on PATH; skipping Binaryen -Oz optimization" >&2
fi

echo "=== Verifying xdb-wasm (nodejs target) under Node.js"
node - "$(to_node_path "$NODE_DIR/xdb_wasm.js")" <<'EOF'
const assert = require('node:assert/strict');
const { WasmConnection, WasmDatabase } = require(process.argv[2]);

const conn = new WasmConnection(undefined, undefined, undefined, 4096, 10_000_000n, 33_554_432n, 50_000n);
conn.execute_json(`
  CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, price INTEGER NOT NULL);
  INSERT INTO items (name, price) VALUES ('Espresso', 450), ('Flat White', 500);
`);
const q = JSON.parse(conn.execute_json('SELECT id, name, price FROM items WHERE price >= ? ORDER BY id', JSON.stringify([450])));
assert.deepEqual(q.columns, ['id', 'name', 'price']);
assert.deepEqual(q.rows, [[1, 'Espresso', 450], [2, 'Flat White', 500]]);

const seed = JSON.parse(conn.seed_json(JSON.stringify({ tags: [{ id: 1, label: 'hot' }] })));
assert.equal(seed.rowsInserted, 1);

const exported = conn.export_bytes();
assert.ok(exported.length >= 4096);
assert.equal(Buffer.from(exported.slice(0, 16)).toString('ascii'), 'SQLite format 3\0');

const ro = new WasmDatabase(exported, undefined, undefined, 10_000_000n, 33_554_432n, 50_000n);
const xdbQ = JSON.parse(ro.query_xdb_json('FROM items SELECT name ORDER BY id'));
assert.deepEqual(xdbQ.rows, [['Espresso'], ['Flat White']]);
ro.close();
ro.free();
conn.close();
conn.free();
console.log('ok: xdb-wasm Node smoke verification passed (' + exported.length + ' exported bytes)');
EOF

echo "=== Packaging versioned release assets into $OUT_DIR"
cp "$WEB_DIR/xdb_wasm_bg.wasm" "$OUT_DIR/xdb-${VERSION}-wasm.wasm"
cp "$WEB_DIR/xdb_wasm.js" "$OUT_DIR/xdb-${VERSION}-wasm.js"
cp "$WEB_DIR/xdb_wasm.d.ts" "$OUT_DIR/xdb-${VERSION}-wasm.d.ts"
cp "$WEB_DIR/xdb_wasm_bg.wasm.d.ts" "$OUT_DIR/xdb-${VERSION}-wasm.wasm.d.ts"

# Rewrite default .wasm URL in packaged web JS so zero-arg `await initXdb()` resolves the versioned filename.
node - "$(to_node_path "$OUT_DIR/xdb-${VERSION}-wasm.js")" "xdb-${VERSION}-wasm.wasm" <<'EOF'
const fs = require('node:fs');
const file = process.argv[2];
const wasmName = process.argv[3];
const src = fs.readFileSync(file, 'utf8');
const updated = src.replaceAll('xdb_wasm_bg.wasm', wasmName);
if (updated === src) {
  throw new Error('Failed to rewrite default wasm filename in ' + file);
}
fs.writeFileSync(file, updated, 'utf8');
EOF

echo "=== Verifying packaged web ESM asset under Node.js"
node --input-type=module - "$(to_node_path "$OUT_DIR/xdb-${VERSION}-wasm.js")" "$(to_node_path "$OUT_DIR/xdb-${VERSION}-wasm.wasm")" <<'EOF'
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';

const mod = await import(pathToFileURL(process.argv[2]).href);
await mod.default({ module_or_path: fs.readFileSync(process.argv[3]) });
const conn = new mod.WasmConnection(undefined, undefined, undefined, 4096, 10_000_000n, 33_554_432n, 50_000n);
const res = JSON.parse(conn.execute_json('SELECT 40 + 2 AS ans'));
assert.deepEqual(res.rows, [[42]]);
conn.close();
conn.free();
console.log('ok: xdb-wasm packaged web ESM verification passed');
EOF

node - "$(to_node_path "$OUT_DIR/xdb-${VERSION}-wasm.wasm")" "$(to_node_path "$OUT_DIR/xdb-${VERSION}-wasm.wasm.br")" <<'EOF'
const fs = require('node:fs');
const zlib = require('node:zlib');
const raw = fs.readFileSync(process.argv[2]);
const br = zlib.brotliCompressSync(raw, {
  params: {
    [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
    [zlib.constants.BROTLI_PARAM_LGWIN]: 24,
    [zlib.constants.BROTLI_PARAM_SIZE_HINT]: raw.length,
  },
});
if (!zlib.brotliDecompressSync(br).equals(raw)) {
  throw new Error('Brotli round-trip mismatch');
}
fs.writeFileSync(process.argv[3], br);
console.log(`xdb-wasm: raw=${raw.length} bytes, brotli-11=${br.length} bytes`);
EOF

(
  cd "$OUT_DIR"
  node - "$VERSION" <<'EOF'
const crypto = require('node:crypto');
const fs = require('node:fs');
const v = process.argv[2];
const files = [
  `xdb-${v}-wasm.wasm`,
  `xdb-${v}-wasm.wasm.br`,
  `xdb-${v}-wasm.js`,
  `xdb-${v}-wasm.d.ts`,
  `xdb-${v}-wasm.wasm.d.ts`,
];
const lines = files.map((f) => {
  const hash = crypto.createHash('sha256').update(fs.readFileSync(f)).digest('hex');
  return `${hash}  ${f}`;
});
fs.writeFileSync('SHA256SUMS', lines.join('\n') + '\n');
console.log('SHA256SUMS:\n' + lines.join('\n'));
EOF
)
