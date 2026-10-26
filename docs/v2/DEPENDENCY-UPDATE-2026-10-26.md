# Dependency update — 2026-10-26

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits)

This audit records Cartograph v3.0.6. All 90 direct external dependencies were
checked against their latest non-yanked stable releases. Notify advances from
8.2.0 to 8.2.1 and tree-sitter-ada from 0.1.0 to 0.9.1. The complete lockfile
update upgrades 16 packages, adds two Apple framework bindings, and removes
fsevent-sys 4.1 and the Windows 0.60/0.53 dependency family. The obsolete
cargo-deny exception for windows-sys 0.60.2 is removed.

The 289 distinct registry crate names in the resulting lockfile were checked.
Updates include cc 1.10.0, ICU 2.4, smallvec 1.16.4, tinystr 0.8.5,
webpki-root-certs 1.0.11, yoke 0.8.4, zerovec 0.11.9, and zerovec-derive 0.11.7.
The crates.io API confirms toml_edit 0.25.17+spec-1.1.0 despite an older
highest-version response from its sparse-index endpoint.

Rust remains exactly 1.99.1 with edition 2024. The RocksDB Rust binding remains
0.53.0 at immutable commit `50017c3de5bc24cf5d81fdc0fe6b0f23df700765`, retaining
its bindgen 0.73 fix and bundled RocksDB 11.8.1. Both already match their latest
stable releases. All six GitHub Actions also retain their current latest stable
commit pins.

The Docker registry publishes newer Linux build/runtime manifests. Their
configuration documents confirm the selected architecture and Rust 1.99.1:

| Image | Architecture | Selected manifest digest |
| --- | --- | --- |
| rust:1.99.1-trixie | amd64 | `sha256:c3d8fc865b8bee83c7894fb833c2c78c8cac5e865d45afc9d05ed026a6301c14` |
| rust:1.99.1-trixie | arm64 | `sha256:85dfd20bf1b23fa5001e30c181e148a218a2af53a1e5aa7ac4c8824d1be55e85` |
| debian:13-slim | amd64 | `sha256:4876fdea29cac1a33a3195f1eb1952feb1fca7dd6dd00be3e2f1879cf20d34d2` |
| debian:13-slim | arm64 | `sha256:a66b19aa552f595b4ab276c6b4f9b24fa4546632c5ecd88cccdced4a6866323a` |

Newer incompatible transitive releases remain outside their upstream consumers'
supported version ranges. Cargo resolves the newest compatible versions without
patching those upstream contracts:

| Transitive family | Locked | Latest stable | Upstream constraint |
| --- | --- | --- | --- |
| constant_time_eq | 0.4.2 | 0.6.1 | blake3: `^0.4.2` |
| libloading | 0.8.9 | 0.9.0 | clang-sys and jni: `^0.8` |
| parking_lot / lock_api / parking_lot_core | 0.12.5 / 0.4.14 / 0.9.12 | 0.13.0 / 0.5.0 / 0.10.0 | rust-rocksdb: parking_lot `^0.12` |
| nom | 7.1.3 | 8.0.0 | cexpr: `^7` |
| r-efi | 6.0.0 | 7.1.0 | getrandom: `^6` |
| redox_syscall | 0.5.18 | 0.9.5 | parking_lot_core: `^0.5` |
| tower-http | 0.6.11 | 0.7.1 | reqwest: `^0.6.8` |
| wasi | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | getrandom and mio: `^0.11` |
| wasm-streams | 0.5.0 | 0.7.0 | reqwest: `^0.5` |
| windows-link | 0.2.1 | 0.100.0 | Windows support consumers: `^0.2` |
| windows-targets / architecture crates | 0.52.6 | 0.53.5 / 0.53.1 | ring via windows-sys 0.52 |

The Ada grammar and callable-initializer correction require generation contract
V26 and one ordinary source reindex. Frozen extraction source/captures and
benchmark inputs remain unchanged. The benchmark's independent wire encoding
reproduces V25's digest over 6,408,142 canonical bytes and derives V26 by changing
only the versioned hash domain. The same independent encoding reproduces all
frozen parser-only, admitted, generic, custom and historical projection pins
under V25 before deriving their V26 values.

Sources: [crates.io](https://crates.io/),
[Rust stable manifest](https://static.rust-lang.org/dist/channel-rust-stable.toml),
[rust-rocksdb releases](https://github.com/zaidoon1/rust-rocksdb/releases), and
[Docker Official Images](https://github.com/docker-library/official-images).

The frozen real-source corpus retains all 34 files, 6,362 declarations/documents,
21,320 references, resolved targets and exact BM25 results. The initializer fix
moves 2,381 executable references to a containing owner and coalesces 28 edges
(11,278 to 11,250). An independent reconstruction starts from the previous
extractor's facts, verifies every owner change and rebuilds the edge counts.
Independent wire encoding reproduces V25 `4801ce5e...c37da` and derives V26
`c75fca59...0b56a8`. Callback execution scopes and initializer self-targets have
focused regressions. The corpus, source fingerprint and benchmark inputs stay
frozen.
