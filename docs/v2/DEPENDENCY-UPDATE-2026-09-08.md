# Dependency and retention update — 2026-09-08

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits) · [V2 architecture](ARCHITECTURE.md)

> [!NOTE]
> Dated historical record of the 2026-09-08 development change; it later shipped in [v2.1.28](../releases/v2.1.28.md).

This development change extends the [architecture improvements](ARCHITECTURE-IMPROVEMENTS.md)
with resumable storage cleanup and the latest published stable direct dependencies.
It does not represent a published release or an upgrade of an already attached host.

## Dependency decisions

| Surface | Reviewed selection | Compatibility work |
| --- | --- | --- |
| Rust compiler and workspace MSRV | 1.98.1 | Includes the upstream correction for the 1.98.0 trait-object miscompilation; all required gates use this exact stable compiler. |
| Tree-sitter native runtime | 0.27.0 | Updated query-capture and cursor APIs; grammar bindings use the same native runtime. |
| Arborium grammars | 2.18.2 across all 41 workspace pins | Complete language corpus and publication/determinism coverage remains required. |
| ABAP grammar and its support crate | Published 31.0.0 registry packages | A local compatibility facade reexports only the 0.27 `Language` and ABI constants to bindings still requesting 0.26. See [vendor contract](../../vendor/README.md). |
| Other direct crates | Latest stable versions verified against crates.io | Includes BLAKE3 1.8.7, reqwest 0.13.5, rustls 0.23.44 and tree-sitter-language 0.1.8; regenerated lockfile includes compatible transitive updates. |
| TOML editor | 0.25.13+spec-1.1.0 | Already current; the suffix describes build metadata, not a prerelease. |
| ParadeDB / pg_search | 0.25.6 | Exact multi-platform manifest and per-platform image configurations checked; managed upgrade coverage starts from 0.25.3. |
| PostgreSQL | 18.6 in the managed image | Both published amd64 and arm64 configurations advertise the same PostgreSQL version. |
| pgvector | Managed 0.8.4; external recommendation 0.8.6 | The upstream managed image still bundles 0.8.4. No custom extension binary is injected into it. |
| Coverage and dependency tools | cargo-llvm-cov 0.9.1; cargo-deny 0.20.2 | Fresh coverage, advisory, license and dependency checks remain mandatory. |
| GitHub Actions | Latest stable releases at reviewed immutable SHAs | Existing SHAs were already current; corrected the download-artifact version annotation. |

The Tree-sitter facade has no native library, parser, build script, unsafe code,
or representation conversion. Registry grammar sources and checksums remain
unchanged. The dependency gate verifies that exactly one package owns the native
`tree-sitter` link. Remove the facade when the published ABAP bindings accept 0.27.
Its complete source participates in cache invalidation. Cargo-deny exceptions
are limited to that local facade and base64 0.22.1, still required by SQLx and
hyper-util while reqwest has advanced to 0.23.1.

The subsequent [maintenance audit](DEPENDENCY-MAINTENANCE-2026-09-08.md)
checks the full resolved dependency graph, upstream notices and repository state.
It found no confirmed abandoned dependency requiring removal and records the
packages that need continued attention.

The official Rust 1.98.1 Docker image was not yet published when reviewed.
Linux packaging therefore uses a refreshed, digest-pinned `rust:1.98.0-trixie`
image for Debian/build utilities, installs exact toolchain 1.98.1 into a writable
container-local Rustup directory, and verifies `rustc --version` before compiling.
The Debian 13 slim runtime digests are also refreshed. The release compiler never
floats with an image tag.

The ParadeDB manifest is
`sha256:c5b04eba22497fa25de12265692e9578e309c2e2001d023ce6d08a17226c200a`.
Its Linux amd64 image is
`sha256:dc02f24819fb99a30542569ddbf023e4e2f5b675e4fe16aa206fd710053cad33`;
its arm64 image is
`sha256:b2442350f666741859b877a0387a38b00fb6dafae3ab3177eaac64244fc176cb`.
Runtime capability checks remain stricter than image metadata: startup must prove
PostgreSQL, extensions, preload, BM25, and source-code tokenization.

## Storage defects and resulting behavior

The previous retention pass counted every candidate's rows across 27 relations
before enforcing its budget, then deleted a generation through one cascading
transaction. A large retained generation could repeatedly time out without
committing any cleanup. Automatic parse-cache retention ran only after successful
generation cleanup, so the same failure also accumulated old cache contracts.

Migration 42 adds an explicitly non-publishable `retiring` state and bounded
project maintenance telemetry. Cleanup claims an eligible generation, drops its
admitted derived search relation, and drains child tables before their parents.
Each transaction admits at most 10,000 actual deleted rows and 32 generations,
with a ten-second deadline and no more than 512 transactions per invocation.
The original row, byte, relation, generation, and overall time limits still apply.
The absolute deadline covers pool acquisition, transaction setup, commit/rollback,
and post-retention maintenance. Exhausting the DDL allowance cannot hide later
relation-free generations behind a page of relation-bearing work.
Catalog checks prove foreign-key ordering under DDL locks; every commit requires
the exact live migration fence. Earlier commits survive later failures.

Oversized derived relations are filtered before candidate pagination, so later
smaller work remains eligible. The CLI and MCP expose an explicit byte-budget
override up to 64 GiB. Current generations, live leases, recent staging/ready work,
incomplete imports, and retained superseded history remain protected.

Cache cleanup now runs independently under the same fence, and protects the
exact parsing-policy cache digest, including the AST-depth limit. The previous
maintenance input used the raw extractor digest even though stored cache keys
included that policy. Tests use an independently constructed expected digest and
newer historical cache timestamps to prove that the current cache survives.
The latest generation/cache outcomes and consecutive failures are retained in a
bounded project record and exposed through `db usage`.

Diagnostics distinguish allocated catalog space, the remaining database
allocation gap, and empty spill heaps retaining large B-tree files. Neither the
gap nor an upstream extension upgrade proves that a file is safe to delete.
Online B-tree compaction and heap rewrites remain separate, explicitly confirmed
operations with headroom checks. Historical uncatalogued files require a verified
backup and supported fresh-storage restoration, never filename-based deletion.

## Regression evidence and operational boundary

The live retirement regression injects a document-delete failure after two
transactions have committed 20,000 embedding deletions. It verifies rollback of
the next transaction, rejection of a new writer for the retiring generation, and
convergence through subsequent 3,000-row invocations. The complete fixture removes
43,036 canonical rows without losing committed progress. Another live fixture
places an over-budget search relation before a cheaper generation, proves that
the cheap generation is removed, and then resumes the older relation with an
explicit larger byte budget.

Additional live regressions exhaust the entire connection pool under a short
cleanup deadline, interrupt an in-flight transaction and verify a clean retry,
and exercise two older relation-bearing generations before a later relation-free
generation under a one-relation DDL allowance.

A real no-op indexing test injects an unexpected foreign-key cascade, verifies
that cache eviction still commits while generation cleanup fails, checks that the
current-policy cache remains present, and verifies persisted failure/reset
telemetry. Existing v1 import, rollback, lease expiry, compaction, language,
publication and deterministic worker-count suites remain part of the gate.

Migrations 1–41 remain unchanged by this retention update. Schema 42 and pg_search
0.25.6 require a matching binary; a process already attached to the old runtime
cannot hot-load either change. Managed rollout requires verified backups,
headroom, the confirmed upgrade path, and new-process capability/query evidence.
Source validation and isolated database tests do not establish live deployment.

Upstream references: [Rust 1.98.1](https://blog.rust-lang.org/2026/09/03/Rust-1.98.1/),
[Tree-sitter 0.27.0](https://github.com/tree-sitter/tree-sitter/releases/tag/v0.27.0),
[ParadeDB 0.25.6](https://github.com/paradedb/paradedb/releases/tag/v0.25.6), and
[ParadeDB merge-crash fix](https://github.com/paradedb/paradedb/pull/5328).
The crash fix explicitly leaves orphaned-file cleanup for separate work; it does
not establish the origin or reclamation of a particular local allocation gap.
