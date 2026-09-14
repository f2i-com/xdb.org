# Dependency maintenance

How the two Rust advisories that Dependabot raised on this repository were
closed, and what to do the next time one appears.

## The lockfile is the whole story

`Cargo.lock` at the root covers the `xdb` crate and the demo desktop app.
Dependabot reads that file, so an advisory can name a crate that only the
demo app pulls in through Tauri. Re-resolving is usually enough:

```sh
cargo update            # or `cargo update --offline` with a warm registry
cargo test -p xdb
cargo check             # the demo app too
```

The `rand` 0.7 advisory (RUSTSEC, "unsound with a custom logger") came in
through `tauri-utils → kuchikiki → selectors → phf_codegen 0.8`; a fresh
resolution picks a `tauri-utils` feature set that no longer needs
`kuchikiki`, and `rand` 0.7 leaves the graph with it.

## glib stays on 0.18, patched

GTK 3, which Tauri uses on Linux, requires glib 0.18, and upstream shipped
the fix for RUSTSEC-2024-0429 (unsound `VariantStrIter` iterators) only in
0.20. `vendor/glib` is 0.18.5 with that fix backported, applied through
`[patch.crates-io]` in the root `Cargo.toml`. It is the same copy Softn's
desktop apps use (`softn.com/vendor/glib`); keep the two identical. Drop
the patch when Tauri moves to a GTK that accepts glib 0.20.

## Softn consumes this crate by path and by pin

`softn.com/apps/softn-loader` depends on `crates/xdb` by relative path and
carries its own lockfile, so a change here reaches the loader on its next
`cargo update`; CI checks out the commit named in
`softn.com/.github/scripts/checkout-xdb.sh`, so bump that pin after
committing here.
