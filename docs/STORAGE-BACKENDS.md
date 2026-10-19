# Native RocksDB storage

Cartograph `v3.0.1` uses RocksDB exclusively. Earlier v2 releases used
PostgreSQL; install v3 and follow the source-rebuild migration below.

Cartograph stores each project's graph and agent state in its own
`.cartograph/data` RocksDB database. RocksDB **11.8.1** is compiled into the
Rust executable through the pinned `rust-rocksdb` binding. There is no database
server, Docker requirement, SQL runtime, or selectable PostgreSQL backend.

## Ownership and concurrency

Use one database per project, shared by every command serving that project.
RocksDB permits one primary process. On macOS/Linux, an MCP owner exposes a
same-user private Unix socket so other CLI/MCP clients reuse that primary.
Clients never bypass its lock by deleting files. Cross-process owner routing
is currently Unix-only; on Windows, close the existing primary before opening
another command for the same project.

On Unix, native admission requires state and data directories owned by the
current effective user. It makes owned directories private (`0700`), including
existing directories, and refuses foreign ownership and symlinks. Owner routing
checks the state directory, socket, and peer against the actual effective user.

Four dedicated foreground storage workers isolate native blocking calls from
Tokio. Admission bounds jobs and bytes, and reserves capacity for reads during
writes. The CLI reserves 512 MiB of pending request buffers; the lower-level
worker default is 256 MiB. These are admission limits, not total RSS ceilings.
RocksDB shares a 64 MiB block cache and a 32 MiB write-buffer budget across
column families. The cache uses four shards so existing large SST index blocks
can stay cached without increasing the total budget. Native metadata, request
buffers, extraction working sets, and the filesystem cache need additional memory.

Each graph view reuses at most four point-read iterators tied to its exact
immutable snapshot. Every lookup re-seeks and checks the logical key and
published revision. Read budgets include adjacent records inspected while
seeking a missing key.

Async calls carry deadlines and cooperative cancellation through queue wait
and execution. A WAL sync already in progress cannot be interrupted safely;
the owner retains that work and reports its actual commit outcome. These Rust
async APIs do not enable Linux io_uring or RocksDB native AsyncIO.

## Graph publication and freshness

Canonical extraction, identities, and generation digests remain deterministic.
Revisions store only changed records; packed adjacency blocks support bounded
graph traversal. Generation-scoped name postings seek directly to simple or
qualified declaration names, with the existing Unicode case/filter semantics
applied before limiting results. Old manifests and graphs too large for the
admitted posting workspace use bounded symbol scans; `name_index_version`
reports this derived capability without changing the canonical digest. Publication atomically changes the current revision pointer
with synchronous WAL durability. Snapshot readers see a complete revision and
remain protected while obsolete versions are collected.

The watcher uses a 150 ms quiet window for supported-source changes, coalesces
bursts, and retries bounded failures. After one complete build in the same owner,
proven local edits can reuse the validated graph and publish sparse file
replacements. Declaration, resolution, clone and policy
changes retain full reconciliation; see [freshness tuning](PERF-TUNING.md#freshness). Status still compares the live supported source with the
published generation. A running watcher does not prove freshness: require
`cartograph status .` and a real query to report current evidence. A source edit
during indexing is reported explicitly and schedules another reconciliation.
Use `--no-auto-sync` on a host only when intentionally keeping it quiescent.

For autosync across MCP sessions, run `cartograph watch --project-path .` on
Unix or use `cartograph service start --project-path .` on macOS. The login
service runs the same native owner and same-user IPC path. It owns one private
LaunchAgent registration per canonical project, pins the invoking executable,
restarts unsuccessful exits, and drains work on termination. `service status`
reports registration, loaded state, and IPC availability independently.
`service stop` removes only the verified owned login registration.

## Migration and first use

```sh
cartograph db migrate --to rocksdb --project-path . --format json
cartograph doctor . --format json
cartograph status . --format json
cartograph find ProjectRuntime --project-path . --format json
```

Migration rebuilds graph facts from the current checkout, verifies destination
freshness and retrieval, and only then records `storageBackend: "rocksdb"`.
It preserves unrelated project configuration and all old PostgreSQL files,
containers, and volumes. It does not copy PostgreSQL-only notes, sessions,
embeddings, or generated artifacts. Keep legacy state if those records matter;
there is no SQL importer in this checkout. `storageBackend` is a completed
migration marker, not an engine-selection switch.

New projects can run `cartograph index .` directly. Doctor reports native
capabilities separately from index freshness, model readiness, registration,
and live MCP transport. Re-running an unchanged index should skip publication.

## Usage and bounded maintenance

```sh
cartograph db usage --project-path . --format json
cartograph db prune --project-path . --keep-superseded 2 \
  --maximum-deletions 4 --maximum-cascade-rows 10000 \
  --confirm prune-old-generations --format json
```

Usage reports allocated/native file bytes, WAL, SSTs, memtables, and pending
compaction. Logical record bytes are a different measure from disk allocation.
RocksDB automatically flushes and compacts its LSM tree. Deleting obsolete
logical versions does not promise immediate filesystem reclamation.

Successful index and no-op reconciliations attempt bounded graph, parse-cache,
and auxiliary cleanup. Reports preserve deferred work and phase errors; inspect
them before repeating explicit maintenance. Current snapshots and their
required record versions remain protected. Auxiliary orphan cleanup uses a
grace period and persisted markers so successive batches can resume.

The former `db start`, `stop`, `backup`, `restore`, `upgrade`, `import-v1`,
`derived-index`, and SQL compaction commands are retired. Never copy a live
RocksDB directory as though it were an atomic backup. A supported online
checkpoint/restore command is not implemented in this checkout.

## Retrieval and capacity limits

Exact and graph queries use native keyed records. BM25 uses atomic field-aware
postings and corpus statistics with bounded forward records for document
updates and deletion. Legacy generations or a document exceeding the optional
vocabulary workspace use complete corpus search. The candidate workspace is
32 MiB and exhaustion remains explicit. Statistics cover the complete corpus;
result limits do not change ranking. Dense queries still read all matching
postings and hydrate only the selected documents from the same snapshot.

Semantic search retains exact cosine scoring. A single optional packed corpus
of at most 64 MiB and readiness results are fenced by generation, model, vector
writes, and summary writes. Snapshot-scoped native MultiGet batches cold vector
reads; oversized or busy cache builds use bounded streaming. There is no HNSW
index or engine AsyncIO claim. Both search paths fail explicitly on row, byte,
or time exhaustion and never search only an arbitrary prefix.

Native BM25 uses `k1 = 1.2`, `b = 0.75`, and token weights of four for
qualified names, one for code, and two for natural text. It retains complete
identifier tokens plus snake-case or camel-case parts, uses corpus-wide
weighted document lengths, and breaks equal scores by stable document ID.
Its ranking can differ from ParadeDB's per-field scorer. The frozen source
corpus keeps the same facts and digest; its native ranking pins are checked
against independently calculated corpus statistics.

This removes PostgreSQL service/WAL/index overhead but does not establish a
universal speedup. Common-term search and high-cardinality semantic workloads
need measured native indexes before stronger scaling claims are justified.
Use paired identical-corpus benchmarks for comparisons with PostgreSQL.
Historical PostgreSQL benchmark reports under `v2/benchmarks/` remain historical.

See [migration](v2/ROCKSDB-MIGRATION.md), [performance](PERF-TUNING.md), and
[architecture](v2/ARCHITECTURE.md) for the implementation and verification scope.

Projects with an explicit `maxGenerationBytes` always use complete generation
admission, including the parse and resolve stages, for changed source.
