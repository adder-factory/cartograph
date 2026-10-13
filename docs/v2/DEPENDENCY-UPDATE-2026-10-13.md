# Dependency update — 2026-10-13

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [V2 architecture](ARCHITECTURE.md)

This audit records the dependency selection for Cartograph v2.1.43. Publication
requires the local, live, Sonar, reviewer, and remote artifact gates.

## Native dependencies

All 87 direct registry dependencies were compared with the highest non-yanked
stable crates.io release. Six direct packages advance:

| Dependency | Previous | Selected |
| --- | --- | --- |
| serde_json | 1.0.151 | 1.0.154 |
| sha2 | 0.11.0 | 0.11.1 |
| toml_edit | 0.25.15 | 0.25.17+spec-1.1.0 |
| tree-sitter | 0.27.0 | 0.27.1 |
| tree-sitter-abap-sqry | 32.0.1 | 33.0.1 |
| unicode-ident | 1.0.26 | 1.0.27 |

The Cargo lockfile also refreshes 15 other transitive packages, including `cc`,
`hyper`, `rustls-native-certs`, `smallvec`, `sqry-tree-sitter-support`, `syn`,
`tokio-util`, the TOML support crates, and `zeroize`. Upstream constraints still
own older transitive major versions; no dependency override widens them.
The runtime-neutral ABAP compatibility facade remains necessary because both
33.0.1 bindings still require Tree-sitter 0.26. Its direct reexports now use the
single native 0.27.1 runtime. The frozen v1 parity corpus and captures stay fixed.

## Database

ParadeDB advances from **0.26.0 to 0.26.1**, the latest stable release. The
registry index, both platform manifests, and both image configurations were
checked by SHA-256. Both configurations install PostgreSQL 18.6, `pg_search`
0.26.1, and pgvector 0.8.6.

The image index is `sha256:29bd8cc90a89ff15f8350f89b4849c2ef807aa9381c79febd1afb1a8a25fb706`.
Live capability and managed recovery gates check the extension update and
retained data from 0.26.0. Existing 0.25.3, 0.25.6, 0.25.10, and 0.25.11
recovery fixtures remain. The extension transaction updates pgvector before
`pg_search`; the compatible external pgvector minimum stays 0.8.4 and the
recommended external version stays 0.8.7, the latest upstream stable release.
The managed image retains its verified bundled 0.8.6 extension.

## Build and workflow dependencies

Rust **1.99.0** remains the latest stable compiler and the workspace uses edition
**2024**. Rust and Debian 13 images have refreshed platform digests:

| Image | Platform | Digest |
| --- | --- | --- |
| paradedb/paradedb:0.26.1 | amd64 | `sha256:a87d7afcf45fc8670bf94120499a60aceb8a8ad7e2760b5be32b9f551c1c5731` |
| paradedb/paradedb:0.26.1 | arm64 | `sha256:fd1206c1e99213e972cb9d234176d601f6630461a4d7ddd5643ff1f063fc9125` |
| library/rust:1.99.0-trixie | amd64 | `sha256:d3ba1ad5d5b5a180ca8428b4fab8543a1b3933b32a8a7ad90a7ebbaf5497203a` |
| library/rust:1.99.0-trixie | arm64 | `sha256:585dced3da590f4f7dc2719172ead5b39ea781bd8b698c092112da8ef3be0659` |
| library/debian:13-slim | amd64 | `sha256:918311b7b6c4c6f68b232ba516584925f6c78ad82b6fd534b98979df6438e483` |
| library/debian:13-slim | arm64 | `sha256:a2ca65377c251208de42e1a2722cb949211236c161b634f6f3663754e2d06850` |

All six GitHub Actions were checked against their latest stable releases.
Download-artifact advances to 8.0.2 and upload-artifact to 7.0.2; checkout
7.0.1, cache 6.1.0, attest-build-provenance 4.2.2, and cargo-deny-action 2.1.1
remain current. Every action is pinned to its exact upstream commit.
These selections include the updates proposed by Dependabot PRs #185–#187.

Sources: [crates.io](https://crates.io/),
[ParadeDB 0.26.1](https://github.com/paradedb/paradedb/releases/tag/v0.26.1),
[Rust stable channel](https://static.rust-lang.org/dist/channel-rust-stable.toml),
[pgvector 0.8.7](https://github.com/pgvector/pgvector/blob/v0.8.7/CHANGELOG.md),
[download-artifact 8.0.2](https://github.com/actions/download-artifact/releases/tag/v8.0.2),
[upload-artifact 7.0.2](https://github.com/actions/upload-artifact/releases/tag/v7.0.2),
and [Docker Official Images](https://github.com/docker-library/official-images).
