# Architecture improvement scope and evidence

This development package follows the 2026-09-08 review of `v2.1.27`, based on
commit `12dc7c3fe215c2bebd24e840ce4aabed54043476`. It preserves native Rust,
PostgreSQL-only storage, immutable generations, explicit uncertainty, bounded
execution, and the existing CLI/MCP contracts.

## Implemented boundaries

| Area | Change | Acceptance evidence |
| --- | --- | --- |
| Rust graph correctness | Nominal `self` ownership resolves private parent methods across split impl files; unknown and ambiguous receivers abstain | Positive/negative resolver fixtures; published caller multiplicity and affected tests in both storage modes at 1/2/4/8/16 workers |
| Source windows | One bounded manifest/capture scan serves a batch bound to an expected generation | Scan counter, edited-file hashes, generation-change rejection, and cancellation regressions |
| Application services | Structural symbol summary sweep moved to a typed agent service; transport serializes the existing report | Direct service cancellation/cache checks and the existing CLI/MCP job integration test |
| Configuration | Source/storage policy and atomic JSON I/O moved from LLM ownership to config | Concurrent writer, unrelated-key preservation, bounded lock, and stale-write rejection tests |
| Model transport | Bounded reusable embedding/rerank clients and shared origin admission | Real connection reuse, credential/config rotation, foreground capacity under background load, queue timeout and cancellation tests |
| SCIP spill | Shared source-verified replacement plan filters native batches and appends bounded compiler facts before reduction | More than 1,200 symbols across batch boundaries; memory/spill digests and centrality at five worker counts; injected overlay COPY failure preserves current generation |
| Storage diagnostics | Nullable unobserved estimates, statistics provenance, vacuum/analyze history, independent inventory pagination | Live table-counter reset and complete catalog pagination checks |
| Parse cache | Retain full-lock invalidation and add workspace manifests, toolchain, target/feature flags, and repository Cargo config to the shared source contract | Review rejected forward-only dependency filtering because excluded packages can change unified dependency features |
| Architecture gates | Reviewed production/build dependency directions and cycle detection | Negative fixtures for forbidden, renamed, target-specific, test-only, cyclic, and unknown-member edges |
| Workload evidence | Versioned repeated-edit fixture with phase timings and allocation observations | Cached results equal clean rebuilds after body edits, rename, deleted exports, and admission-policy changes |

Migration 41 admits digest V16 for the Rust receiver contract. Earlier migration
SQL and checksums remain unchanged. Frozen V16 corpus digests intentionally use
the new digest domain; structural fixture assertions remain independent of those
digest constants. Deploying this schema requires a matching binary; an attached
older host must not be assumed to hot-reload development code.

## Repeated-edit measurement contract

`rust-repeated-edit-v1` has 65 Rust files, 64 modules, ten callers per leaf,
four workers, no model tiers, and disabled Git/issue-history enrichment. Both
storage modes run cold, no-op, body-edit, rename, deleted-export, and source-policy
cases. Each edit is compared with a forced clean parse/full resolution. Automatic
retention keeps the current generation and two superseded generations; the
comparison rebuilds also participate in retention and allocation.

Run against a dedicated capability-checked PostgreSQL test database:

```sh
cargo test --locked -p cartograph-agent --test live_project \
  repeated_edit_workload -- --ignored --nocapture --test-threads=1
```

The `ARCHITECTURE_WORKLOAD_V1` JSON line records the fixture/configuration,
per-phase timing, cache use, catalog counts, and allocated storage. Timing
observations never gate ordinary correctness. Preserve the exact source tree,
compiler/target, command, database image/configuration, concurrency conditions,
and raw output alongside any comparison. Debug, coverage, and optimized runs
must remain separate series.

One initial local **debug-build observation**, with one sample per case on an
Apple Silicon development host and a dedicated PostgreSQL container, gave:

| Mode and case | Total ms | Parse ms | Resolve ms | Reduce ms | Copy ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Memory, cold | 1,020 | 292 | 105 | 216 | 262 |
| Memory, body edit | 760 | 59 | 99 | 205 | 275 |
| PostgreSQL, cold | 9,438 | 474 | 721 | 8,015 | 98 |
| PostgreSQL, body edit | 2,264 | 124 | 711 | 1,153 | 139 |

No-op observations were 54 ms and 53 ms. These are preliminary stage-cost
observations, not speedup claims or a release benchmark. Cold database effects,
debug compilation, and host activity are not isolated by a single sample.
The small fixture deliberately forces spill to test parity; automatic storage
selection normally uses memory at this size.

After the complete edit/rebuild sequence, observed schema allocation was about
75 MiB for memory construction and 98 MiB for spill construction, with one current
and two superseded generations. Allocation includes reusable space and shared
heaps/indexes. It is not a live-payload estimate or a reclaimable-byte promise.
This fixture does not measure index density, peak temporary space, WAL, or
reclamation latency.

## Evidence required before the next changes

- **Cross-request scan sharing:** establish a source-observation epoch and its
  cancellation, watcher-overflow, configuration, overlay, and missed-event
  semantics. Current batching removes repeated window scans within a request;
  separate requests still validate independently.
- **Further service extraction:** move file/module rollups, model summaries,
  classification, and enrichment as separate typed workflow slices, then split
  adapter tool families. Keep generated CLI/MCP schema and error parity.
- **Narrower cache keys:** prove parser/source independence and effective Cargo
  feature unification before replacing the conservative shared extraction and
  complete-lockfile contract. A forward dependency closure alone is insufficient.
- **Storage and incremental redesign:** use repeated optimized measurements on
  larger real corpora, fixed retention, index density, WAL/temporary-space peaks,
  read latency, and cleanup cost. Reduction/search construction as well as
  resolution need measurement. Reuse only facts with complete dependency identity,
  and compare every incremental output with a full immutable generation.
- **Broader product evaluation:** extend labelled multi-language architecture,
  retrieval, impact, and known-negative tasks; measure concurrent end-to-end MCP
  bytes and task accuracy separately from the existing five patch tasks.

These follow-ups require their own evidence and acceptance criteria. This package
does not change fact-table representation, patch a published generation in place,
relax admission limits, or claim that the existing evaluation proves broad task
accuracy.

The subsequent [dependency and retention update](DEPENDENCY-UPDATE-2026-09-08.md)
adds resumable generation draining, independent cache retention, durable
maintenance diagnostics, and Tree-sitter 0.27 compatibility.
