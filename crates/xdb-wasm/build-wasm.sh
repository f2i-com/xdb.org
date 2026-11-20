#!/usr/bin/env bash
# Build hardened XDB WebAssembly release artifacts (web + nodejs),
# optimize with wasm-opt -Oz, Brotli-compress, verify under Node.js,
# and emit SHA256SUMS into crates/xdb-wasm/dist/.
set -euo pipefail

caller_pwd="$(pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="${1:-$here/dist}"
case "$OUT_DIR" in
  /*|[A-Za-z]:/*) ;;
  *) OUT_DIR="$caller_pwd/$OUT_DIR" ;;
esac
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
cd "$here"

TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.92.0}"
VERSION="$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"

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

if command -v wasm-opt >/dev/null 2>&1; then
  echo "=== Running wasm-opt -Oz"
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
    "$WEB_DIR/xdb_wasm_bg.wasm" -o "$WEB_DIR/xdb_wasm_bg.opt.wasm"
  mv "$WEB_DIR/xdb_wasm_bg.opt.wasm" "$WEB_DIR/xdb_wasm_bg.wasm"
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
    "$NODE_DIR/xdb_wasm_bg.wasm" -o "$NODE_DIR/xdb_wasm_bg.opt.wasm"
  mv "$NODE_DIR/xdb_wasm_bg.opt.wasm" "$NODE_DIR/xdb_wasm_bg.wasm"
fi

echo "=== Verifying xdb-wasm under Node.js"
node - "$NODE_DIR/xdb_wasm.js" <<'EOF'
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
conn.close();
console.log('ok: xdb-wasm Node smoke verification passed (' + exported.length + ' exported bytes)');
EOF

echo "=== Packaging versioned release assets into $OUT_DIR"
cp "$WEB_DIR/xdb_wasm_bg.wasm" "$OUT_DIR/xdb-${VERSION}-wasm.wasm"
cp "$WEB_DIR/xdb_wasm.js" "$OUT_DIR/xdb-${VERSION}-wasm.js"
cp "$WEB_DIR/xdb_wasm.d.ts" "$OUT_DIR/xdb-${VERSION}-wasm.d.ts"
cp "$WEB_DIR/xdb_wasm_bg.wasm.d.ts" "$OUT_DIR/xdb-${VERSION}-wasm.wasm.d.ts"

node - "$OUT_DIR/xdb-${VERSION}-wasm.wasm" "$OUT_DIR/xdb-${VERSION}-wasm.wasm.br" <<'EOF'
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
