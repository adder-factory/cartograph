# Native grammar provenance

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Coverage report](LANGUAGE-COVERAGE-REPORT.md) ·
[Extension guide](EXTENDING-EXTRACTORS-RESOLVERS.md)

Cartograph v2 links pinned Rust tree-sitter grammar crates. It does not ship or
load the v1 WebAssembly grammar directory.

## Release inventory

The authoritative release inventory is:

| What | Where |
| --- | --- |
| Exact crate versions and checksums | `Cargo.lock` (also the complete set of linked grammar crates) |
| Direct grammar pins | Workspace dependencies in `Cargo.toml`, enabled for the extractor in `crates/cartograph-extract/Cargo.toml` |
| Language-to-grammar/custom-scanner registration | `crates/cartograph-extract/src/language.rs`, `grammars.rs`, and related native modules |
| Third-party licensing and upstream attribution | `ACKNOWLEDGEMENTS.md` and `deny.toml` |

`ACKNOWLEDGEMENTS.md` credits only the principal grammar families. The full
grammar-license inventory is every grammar crate in `Cargo.lock`, whose licenses
`deny.toml` enforces through `cargo deny`.

> [!NOTE]
> **ABAP compatibility shim.** The workspace `Cargo.toml` carries
> `[patch.crates-io] tree-sitter = { path = "vendor/tree-sitter-026-compat" }`.
> The published ABAP grammar bindings still request tree-sitter 0.26, so this
> local facade re-exports the exact 0.27 runtime types without any native code;
> `Cargo.lock` therefore lists two `tree-sitter` package versions, and a narrow
> `deny.toml` skip entry records why. See [`vendor/README.md`](../vendor/README.md)
> for the full rationale and the removal condition.

## Adding or upgrading a grammar

Adding or upgrading a grammar requires more than a successful parser load:

1. pin an exact compatible crate version;
2. verify its license and update acknowledgements/deny policy when needed;
3. inspect its real node vocabulary and update native extraction;
4. test declarations, references, malformed syntax, cancellation, bounds, and
   literal safety;
5. prove deterministic output under every supported worker count;
6. publish the language corpus to live PostgreSQL/ParadeDB and verify BM25;
7. run `cargo deny --all-features check` and the complete release gates.

See [the native extension guide](EXTENDING-EXTRACTORS-RESOLVERS.md) for the
full admission contract.
