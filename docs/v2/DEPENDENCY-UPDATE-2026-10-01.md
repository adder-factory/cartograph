# Dependency update — 2026-10-01

This audit records the dependency selection for Cartograph v2.1.33. Publication
requires the separate local, live, Sonar, reviewer, and remote artifact gates.

## Native dependencies

All 86 direct registry dependencies were compared with the newest non-yanked
stable versions from crates.io. Two direct packages changed:

| Dependency | Previous | Selected |
| --- | --- | --- |
| thiserror | 2.0.20 | 2.0.21 |
| tree-sitter-abap-sqry | 31.0.0 | 32.0.1 |

Dependabot PR #170 (thiserror 2.0.21) is covered by this update. Cargo
additionally updates `cc` 1.5.1, `find-msvc-tools` 0.1.14, `hyper-util` 0.1.21,
`rustls-platform-verifier` 0.7.1, `rustls-platform-verifier-android` 0.2.0,
`smallvec` 1.16.2, `sqry-tree-sitter-support` 32.0.1, `tokio-rustls` 0.26.6,
`yoke-derive` 0.8.4, and the wasm-only `js-sys`, `web-sys`, and `wasm-bindgen`
family, which no release target builds.

The ABAP grammar release changes packaging only: the published 32.0.1 grammar
sources, generated parser, and binding are byte-identical to 31.0.0, and its
support crate changes only its version. No grammar, node type, or extraction
behavior changes, so the generation contract stays at V18. Both crates still
request Tree-sitter 0.26, so the [ABAP facade](../../vendor/README.md) and its
cargo-deny entry remain necessary. The lockfile change still refreshes the
extractor fingerprint and parse-cache identity, as every dependency update does.

`cc` 1.5 compiles every grammar's C and C++ sources. Its changes are MSVC
argument handling and static C++ standard-library linking; the repository sets
no target features for it to forward. The Windows portability job covers the
MSVC path.

## Database and toolchain

ParadeDB advances from **0.25.10 to 0.25.11**, the latest stable release. It
fixes a crash when a search scan is cancelled or terminated, which statement
timeouts and request cancellation can trigger. 0.26.0-rc.4 is a prerelease and
is not selected.

| Image | Digest |
| --- | --- |
| paradedb/paradedb:0.25.11 (index) | `sha256:a9cbdcfd8a1c349ab21590fd6d6dcbe7da489878df6502922d032dd64c1a7ae7` |
| linux/amd64 | `sha256:ab4a2a49cf8a3b935c6f374859fa8c8a9a612c241f2ac7e7c8b39b435be45b5b` |
| linux/arm64 | `sha256:91d8f96296a23d0487ca9d936076ba410169cbf7a6485466dcd58ccdcf532e2c` |

Both platforms ship PostgreSQL 18.6, `pg_search` 0.25.11, and pgvector 0.8.4,
and include the `pg_search--0.25.10--0.25.11` upgrade script. pgvector 0.8.6
remains the newest stable tag and the recommended external version.

Rust 1.99.0 was released on the day of this audit, but the official
`rust:1.99.0-trixie` build image was not yet published, so the validation and
release workflows cannot pin it. The toolchain stays at **1.98.1** for this
release; the weekly stable canary reports the difference until the next update.

## Build and workflow dependencies

The `rust:1.98.1-trixie` and `debian:13-slim` amd64/arm64 platform digests are
unchanged. All six GitHub Actions already match their latest stable release and
resolved commit SHA: checkout 7.0.1, cache 6.1.0, download-artifact 8.0.1,
upload-artifact 7.0.1, attest-build-provenance 4.2.2, and cargo-deny-action
2.1.1 (cargo-deny 0.20.2).

Sources: [crates.io](https://crates.io/),
[ParadeDB releases](https://github.com/paradedb/paradedb/releases),
[Rust releases](https://blog.rust-lang.org/releases/), and
[pgvector tags](https://github.com/pgvector/pgvector/tags).
