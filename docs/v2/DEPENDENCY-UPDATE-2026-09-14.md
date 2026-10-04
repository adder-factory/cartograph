# Dependency update — 2026-09-14

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [V2 architecture](ARCHITECTURE.md)

> [!NOTE]
> Dated historical record of the 2026-09-14 dependency audit; these selections shipped in [v2.1.30](../releases/v2.1.30.md).

This audit records the dependency selection for Cartograph v2.1.30. Release
publication requires the separate local, live, Sonar, reviewer, and remote
artifact gates; registry metadata alone is not compatibility evidence.

## Native dependencies

All 86 direct registry dependencies were compared with the newest non-yanked
stable versions in the crates.io sparse index. The resulting updates are:

| Dependency | Previous | Selected |
| --- | --- | --- |
| toml_edit | 0.25.13+spec-1.1.0 | 0.25.15+spec-1.1.0 |
| clap | 4.6.6 | 4.6.7 |
| rustls | 0.23.44 | 0.23.45 |
| tree-sitter-arkts | 0.2.0 | 0.3.0 |
| tree-sitter-ocaml | 0.25.0 | 0.26.0 |

The TOML update preserves Dependabot PR #160's commit ancestry. Cargo resolves
the complete lockfile under the exact Rust 1.98.1 toolchain, including compatible
updates to bitflags, cc, clap_builder, clap_derive, clap_lex, smallvec, and tinyvec.
The direct pins have no newer stable registry versions at this audit boundary.

`cargo outdated --workspace` cannot inspect this workspace because its temporary
copy omits the excluded local Tree-sitter facade. The audit therefore uses
direct registry comparisons and Cargo's full resolver dry-run, without removing
the patch or modifying the shipped dependency graph to accommodate the tool.
The [ABAP facade](../../vendor/README.md) remains necessary for the published
31.0.0 bindings; it owns no native parser and reexports the shared 0.27 runtime.
The extractor build contract includes Cargo.lock, so changed grammar versions
invalidate reusable parse-cache entries. Generation contract V17 additionally
fences status and the unchanged-index shortcut: a V16 project must publish a new
generation even when its source bytes have not changed.

## Database image

ParadeDB and the exact pg_search capability advance from 0.25.7 to 0.25.9.
Both published platform manifests and their image configuration histories were
checked, including the installed pg_search package version and pgvector pin.

| Image | Immutable digest |
| --- | --- |
| ParadeDB 0.25.9 multi-architecture | `sha256:8b96369912d4d5611756383df8a7d87d4561750ceb4a12df606ec55e21194b18` |
| ParadeDB Linux amd64 | `sha256:151281831e03371eb52c4eaa36f276548dc996b10f9bcf1d9dba32646de464f4` |
| ParadeDB Linux arm64 | `sha256:c17153b8b7307734c3aede0393dfe8fa447f4a528ec028b1c3937186ab3ee242` |

Both images contain PostgreSQL 18.6, pg_search 0.25.9, and pgvector 0.8.4.
pgvector 0.8.6 is the latest tagged upstream version and remains the external
PostgreSQL recommendation. Cartograph uses the upstream managed image without
injecting a custom extension build. External service requirements remain
PostgreSQL 18.4+ within major 18 and pgvector 0.8.4+, with exact pg_search 0.25.9.

Recovery tests cover upgrades from 0.25.3, 0.25.6, and 0.25.7. Append-only schema migration 43 admits
generation contract V17; migrations 1–42 and their checksums are unchanged. Runtime proof requires the live capability,
query, publication, backup, rollback, crash-recovery, and deterministic worker
suites against isolated services.

## Build and workflow dependencies

Rust 1.98.1 remains the latest stable compiler. Its official Docker images are
now published, replacing the previous 1.98.0 images used only for build utilities.
The packaging helper continues to install and verify the exact reviewed compiler
in its writable container-local Rustup directory.

| Build image | Immutable platform digest |
| --- | --- |
| rust:1.98.1-trixie Linux amd64 | `sha256:af753e6e729c839de28010e323abc550eceaa9572bdaa765429d4f585e2e43dc` |
| rust:1.98.1-trixie Linux arm64 | `sha256:f4afe6d7edbfd62fbb0d090ba65ca5202c5bfa97ed650f5dfba1500dc1ec035c` |

The current Debian 13 slim platform digests match the existing release pins.
All configured GitHub Actions match their latest stable releases and immutable
commit SHAs: checkout 7.0.1, cache 6.1.0, download-artifact 8.0.1,
upload-artifact 7.0.1, attest-build-provenance 4.2.2, and cargo-deny-action 2.1.1.

Sources: [ParadeDB 0.25.9](https://github.com/paradedb/paradedb/releases/tag/v0.25.9),
[Rust 1.98.1](https://github.com/rust-lang/rust/releases/tag/1.98.1),
[pgvector tags](https://github.com/pgvector/pgvector/tags), and the
[crates.io index](https://index.crates.io/config.json).
