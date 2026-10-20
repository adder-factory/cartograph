# Dependency update — 2026-10-20

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [Architecture](ARCHITECTURE.md)

This audit records the dependency selection for Cartograph v3.0.3. Rust's
official stable manifest and release announcement select Rust 1.99.1, released
on 2026-10-15. The workspace, native compatibility facade, CI cargo-deny step,
Linux image policy, and release-workflow contract use that exact compiler.
The workspace retains edition 2024.

Rust 1.99.1 corrects x86-64 FFI `bool` return-value zero extension, the macOS
`rust-lld` library search path, and missing `wcslen` symbols on affected targets.
The compiler change requires fresh builds and the full native release gates;
it does not establish a measured performance improvement.

All 90 direct external dependencies match their highest non-yanked stable
crates.io releases. The lockfile's 288 distinct registry crate names were also
checked. A full Cargo update advances `rustls-webpki` from 0.103.15 to 0.103.16,
which rejects certificate paths containing newly distrusted intermediate public
keys. License, advisory, source, and duplicate-package gates remain enforced.

The RocksDB binding remains `rust-rocksdb` 0.53.0 at immutable commit
`50017c3de5bc24cf5d81fdc0fe6b0f23df700765`, retaining its upstream bindgen 0.73
fix and bundled RocksDB 11.8.1. This is the latest published Rust binding.
Tree-sitter 0.27.1 remains the single native parser runtime. Frozen
extraction fixtures are unchanged.

All six GitHub Actions already match their latest stable release commit pins.
The Docker registry manifests and image configurations verify Rust 1.99.1 for
both native Linux architectures:

| Image | Architecture | Selected manifest digest |
| --- | --- | --- |
| rust:1.99.1-trixie | amd64 | `sha256:d7c9d46725b95c89b733b30883c02fc39d6cbf829be6cdfdc2ace077c7c830e8` |
| rust:1.99.1-trixie | arm64 | `sha256:3a24abf17df2ba07041a07dc9af812921cbc28aa9cbafb5ce6bafa48f197ea33` |

Debian 13-slim runtime digests remain current: amd64
`sha256:918311b7b6c4c6f68b232ba516584925f6c78ad82b6fd534b98979df6438e483`
and arm64
`sha256:a2ca65377c251208de42e1a2722cb949211236c161b634f6f3663754e2d06850`.

Ten transitive crate families remain below newer stable major versions because
their upstream consumers require the existing compatible line:

| Transitive crate | Locked | Latest stable | Upstream constraint |
| --- | --- | --- | --- |
| constant_time_eq | 0.4.2 | 0.6.1 | blake3: `^0.4.2` |
| fsevent-sys | 4.1.0 | 5.2.0 | notify: `^4.0.0` |
| libloading | 0.8.9 | 0.9.0 | clang-sys and jni: `^0.8` |
| nom | 7.1.3 | 8.0.0 | cexpr: `^7` |
| r-efi | 6.0.0 | 7.1.0 | getrandom: `^6` |
| redox_syscall | 0.5.18 | 0.9.5 | parking_lot_core: `^0.5` |
| tower-http | 0.6.11 | 0.7.1 | reqwest: `^0.6.8` |
| wasi | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | getrandom and mio: `^0.11` |
| wasm-streams | 0.5.0 | 0.7.0 | reqwest: `^0.5` |
| windows-link | 0.2.1 | 0.100.0 | jiff, jni, libloading, parking_lot_core and Windows support: `^0.2` |

**Compatibility:** native RocksDB format v1 and generation contract V23.
Existing native generations remain readable. The release requires the full
local, native integration, Sonar, independent review, and remote artifact gates.

Sources: [Rust 1.99.1](https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/),
[crates.io](https://crates.io/),
[rustls-webpki 0.103.16](https://github.com/rustls/webpki/releases/tag/v%2F0.103.16),
[rust-rocksdb releases](https://github.com/zaidoon1/rust-rocksdb/releases),
[RocksDB releases](https://github.com/facebook/rocksdb/releases), and
[Docker Official Images](https://github.com/docker-library/official-images).
