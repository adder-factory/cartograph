# Dependency update — 2026-09-22

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [V2 architecture](ARCHITECTURE.md)

> [!NOTE]
> Dated historical record of the 2026-09-22 dependency audit; these selections shipped in [v2.1.31](../releases/v2.1.31.md).

This audit records the dependency selection for Cartograph v2.1.31. Publication
requires the separate local, live, Sonar, reviewer, and remote artifact gates.

## Native dependencies

All 86 direct registry dependencies were compared with the newest non-yanked
stable versions from crates.io. Two direct packages changed:

| Dependency | Previous | Selected |
| --- | --- | --- |
| tree-sitter-cuda | 0.21.1 | 0.21.2 |
| unicode-ident | 1.0.24 | 1.0.26 |

Dependabot PR #162's commit ancestry is retained. Cargo additionally updates
`cc` 1.4.7, `cfg-if` 1.0.5, `find-msvc-tools` 0.1.13,
`hyper-rustls` 0.27.10, `rand` 0.10.3, `rustix` 1.1.5, `syn` 3.0.6,
`synstructure` 0.14.0, `yoke-derive` 0.8.3, and `zerofrom-derive` 0.1.8.

The CUDA crate's generated parser, grammar, and node types changed; the grammar
adds `__tile__` and `__tile_global__` qualifiers. A focused extraction regression
checks their declarations and call ownership. Generation contract V18 and
append-only schema migration 44 force existing V17 projects to refresh even
when source bytes are unchanged. Parse-cache identity also includes the exact
lockfile. Frozen fixture digest changes reflect the V18 domain; independent
fact counts, source identities, ranking expectations, and worker matrices remain
required. Existing migration checksums 1–43 remain frozen.

`cargo outdated --workspace` still cannot inspect the excluded local Tree-sitter
facade in its temporary workspace. Direct registry comparisons and Cargo's full
resolver provide the dependency inventory without removing that compatibility
boundary. The [ABAP facade](../../vendor/README.md) remains necessary for the
published 31.0.0 bindings and owns no native parser.

## Database and toolchain

Rust **1.98.1**, ParadeDB **0.25.9**, and the recommended external pgvector
**0.8.6** remain the latest stable releases. ParadeDB 0.26.0-rc.1 is a prerelease
and is not selected. The existing immutable ParadeDB multi-architecture pin
and its amd64/arm64 manifests are unchanged. The dedicated live service proves
PostgreSQL 18.6, pg_search 0.25.9, and the managed image's pgvector 0.8.4.

## Build and workflow dependencies

The official build/runtime tags have refreshed platform manifests:

| Image | Platform | Selected immutable digest |
| --- | --- | --- |
| rust:1.98.1-trixie | amd64 | `sha256:f31fa9eaac4e417505e10009786ec0c636558e02dff3fa662cc92c10894ba065` |
| rust:1.98.1-trixie | arm64 | `sha256:218765d808d35e2e3fa1b12b2e1314e2bfdd427a3530de071398553a95743cba` |
| debian:13-slim | amd64 | `sha256:7792b1f7702a86946cd518db72b6a407302c3e9bc1635634368b878189e8221c` |
| debian:13-slim | arm64 | `sha256:da496358bd6934d2bd6a563a33176a2e50eff5490c54b4ac6fb051b69fef4071` |

Both validation and release workflows use these platform pins. All six GitHub
Actions already match their latest stable release and resolved commit SHA:
checkout 7.0.1, cache 6.1.0, download-artifact 8.0.1, upload-artifact 7.0.1,
attest-build-provenance 4.2.2, and cargo-deny-action 2.1.1.

Sources: [crates.io](https://crates.io/),
[ParadeDB releases](https://github.com/paradedb/paradedb/releases),
[Rust releases](https://blog.rust-lang.org/releases/), and
[pgvector tags](https://github.com/pgvector/pgvector/tags).
