# Dependency update — 2026-10-04

This audit records the dependency selection for Cartograph v2.1.39. Publication
requires the local, live, Sonar, reviewer, and remote artifact gates.

## Native dependencies

All 87 direct registry dependencies were compared with the latest stable
crates.io versions. Three direct packages advance:

| Dependency | Previous | Selected |
| --- | --- | --- |
| libc | 0.2.189 | 0.2.190 |
| signal-hook | 0.4.4 | 0.4.5 |
| tokio | 1.53.1 | 1.53.2 |

Cargo also updates `cc` from 1.5.1 to 1.6.0 and `mio` from 1.2.3 to 1.2.4.
The lockfile resolves the newest versions admitted by the current upstream
dependency constraints. Older transitive major versions remain where upstream
packages require them; the update does not override those requirements.
`toml_edit` 0.25.15 resolves to 0.25.15+spec-1.1.0; its build metadata does not
require a manifest change. The runtime-neutral ABAP Tree-sitter compatibility
facade remains necessary and unchanged.

## Database

ParadeDB advances from **0.25.11 to 0.26.0**, the latest stable release. The
published registry index, both platform manifests, and both configuration blobs
were checked by their SHA-256 digests. Both configurations install PostgreSQL
18.6, `pg_search` 0.26.0, and pgvector 0.8.6.

| Image | Digest |
| --- | --- |
| paradedb/paradedb:0.26.0 (index) | `sha256:52fc9c95fdfd462201168d1d82334ed61f85cbd921957cab083ec39800a217ac` |
| linux/amd64 | `sha256:ccc799051736fbe753b17fcbf0491f2be9f27680b8c76648b98c3fb114a93db6` |
| linux/arm64 | `sha256:fc9a047b00f40143c831d308466257d237d874618861eae189af2d1e4ded8934` |

A fresh arm64 container confirms these extension versions, both `paradedb` and
legacy `bm25` access methods, and an extension upgrade path from 0.25.3,
0.25.6, 0.25.10, and 0.25.11 to 0.26.0. The managed upgrade transaction pins
pgvector 0.8.6 before updating `pg_search` to 0.26.0. External PostgreSQL retains
the compatible pgvector 0.8.4 minimum and recommends 0.8.7.

pgvector 0.8.7 is the latest upstream stable release. The current ParadeDB
0.26.0 image bundles 0.8.6; the managed version follows that verified upstream
image rather than installing an unpinned extension over it.

## Build and workflow dependencies

Rust **1.99.0** is the latest stable toolchain. The published
`rust:1.99.0-trixie` and `debian:13-slim` amd64/arm64 platform digests still match
the release workflow pins.

All six GitHub Actions already match their latest stable release: checkout
7.0.1, cache 6.1.0, download-artifact 8.0.1, upload-artifact 7.0.1,
attest-build-provenance 4.2.2, and cargo-deny-action 2.1.1. Each remains pinned
to its reviewed commit SHA.

Sources: [crates.io](https://crates.io/),
[ParadeDB 0.26.0](https://github.com/paradedb/paradedb/releases/tag/v0.26.0),
[Rust stable channel](https://static.rust-lang.org/dist/channel-rust-stable.toml),
[pgvector 0.8.7 changelog](https://github.com/pgvector/pgvector/blob/v0.8.7/CHANGELOG.md),
and [Docker Official Images](https://github.com/docker-library/official-images).
