# Dependency update — 2026-10-19

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [Architecture](ARCHITECTURE.md)

This audit records the dependency selection for Cartograph v3.0.2. All 90
direct external dependencies were compared with the highest non-yanked stable
crates.io release, including build metadata in version comparisons.

| Direct dependency | Previous | Selected |
| --- | --- | --- |
| jiff | 0.2.35 | 0.2.38 |
| libc | 0.2.190 | 0.2.191 |
| tokio | 1.53.2 | 1.54.0 |

The lockfile also updates seven transitive packages: `cc` 1.9.0, `combine`
4.6.9, `equivalent` 1.0.3, `find-msvc-tools` 0.1.15, `jiff-static` 0.2.38,
`schannel` 0.1.30, and `webpki-root-certs` 1.0.10. The Jiff and Tokio selections
include Dependabot PR #191. Cargo's full update preserves the existing compatible
Syn and Windows dependency selections, avoiding that PR's duplicate-package
regressions. License, advisory, source, and duplicate rules remain enforced.

Verification exposed a startup-test race: a real filesystem event could publish
before the startup timer, leaving the reconciliation counter correctly at zero.
The startup test now drives the real native synchronization loop with an open,
silent event channel, proving that the timer builds an unindexed project.
Existing real filesystem-watcher tests remain in place; production behavior,
deadlines, assertions, and concurrency limits are unchanged.

The bounded Git-dirtiness test now initializes its real Git repository with an
explicit empty template. The previous Git-init deadline failure reproduced with
both the 3.0.1 and updated dependency binaries; phase diagnostics showed Git
still running at the deadline. Removing ambient template copying reduces fixture
variability without claiming a proven cause. Real Tokio subprocess execution,
all three dirtiness observations, and their existing bounds remain unchanged.

The remaining direct dependencies already match their latest stable releases.
The RocksDB binding remains `rust-rocksdb` 0.53.0 at immutable commit
`50017c3de5bc24cf5d81fdc0fe6b0f23df700765`, retaining its upstream bindgen 0.73
fix and RocksDB 11.8.1. The published binding release still needs that fix.
Tree-sitter 0.27.1 remains the single native parser runtime; the runtime-neutral
ABAP compatibility facade and frozen parity fixtures remain necessary.

All six GitHub Actions were checked against their latest stable releases;
their existing commit pins are current. Both architectures of the Rust
1.99.0-trixie build image and Debian 13-slim runtime image also match their
existing pinned digests. Rust 1.99.0 and edition 2024 remain the reviewed
toolchain. Fixture manifests are frozen extraction inputs and are excluded
from dependency updates.

The 288 distinct registry crate names in the resolved lockfile were also
checked. Ten transitive crate families remain below a newer stable major
because their upstream consumers require the existing line:

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

Sources: [crates.io](https://crates.io/),
[Tokio releases](https://github.com/tokio-rs/tokio/releases),
[Jiff releases](https://github.com/BurntSushi/jiff/releases),
[libc releases](https://github.com/rust-lang/libc/releases),
[rust-rocksdb releases](https://github.com/zaidoon1/rust-rocksdb/releases),
[RocksDB releases](https://github.com/facebook/rocksdb/releases), and
[Docker Official Images](https://github.com/docker-library/official-images).
The fixture uses the documented [Git template selection](https://git-scm.com/docs/git-init#_template_directory).
