# Performance and freshness tuning

This guide applies to Cartograph `v3.0.2` and its native RocksDB backend.
Published PostgreSQL benchmarks remain historical; they do not prove native
performance. See [storage](STORAGE-BACKENDS.md) for exact limits and commands.

## Measure the user-visible result

Measure source edit to current-query latency, not only initial indexing time.
Record source size, admitted files/facts, dirty-file count, workers, build
profile, machine, sample count, median/p95/max, peak memory, WAL/SST bytes, and
compaction backlog. Verify each query's generation and exact freshness.

Run cold initial indexing, unchanged no-op indexing, one-file edits, batched
edits, deletes, concurrent readers/writers, and repeated replacement/retention.
Include common-term BM25 and semantic queries at real document/vector counts.
A failed or stale sample stays a failure; do not time only successful requests.
Use the same corpus and operations for PostgreSQL/RocksDB comparisons.

## Workers and storage admission

`cartograph index . --workers 4` selects extraction parallelism (1..16).
Begin with defaults; extra workers can increase memory, I/O, and compaction
pressure without improving latency. Canonical counts and digests must remain
identical across worker counts.

Storage uses four dedicated foreground workers and finite task/byte admission.
Writes leave worker capacity for reads. Native caches and memtables are shared
across column families, not multiplied by each family. Admission reservations
are not a process RSS cap. Raising `maxGenerationBytes` does not raise every
independent native storage budget or eliminate global extraction working sets.

Native state reads derive their time budget from the caller's remaining original
request deadline before admission. Queue time counts against that deadline;
nested requests can shorten it and cannot renew it. Default state requests keep
their thirty-second limit, while explicitly longer operations retain their
accepted bound of at most five minutes. Cancellation and row, byte, workspace,
and write limits remain enforced independently.

## Freshness

Keep the source watcher enabled on the project owner. Its default 150 ms quiet window coalesces
bursts, with a two-second maximum coalescing interval; indexing failures retain backoff and diagnostic state. `sync-if-dirty`
provides an explicit hook/CLI reconciliation. Require fresh status and real
find/context evidence after editing; server liveness alone is insufficient.
Keep benchmark output outside the indexed checkout so evidence files do not
continuously invalidate the graph being measured.

The existing project writer retains one bounded canonical graph after a complete
build. For up to 16 changed supported-language files of at most 256 KiB each, it tries a
conservative local replacement. It re-extracts changed files and compares all
resolution inputs, declaration identities and source spans. An accepted edit
reuses unchanged topology and centrality, validates all replacement relations,
recomputes the complete canonical digest, and stages only changed file records.
`incremental_files` reports how many files used this path; zero means a full
build or an unchanged index. The retained graph shares the admitted canonical
allocation and is released before a full rebuild.

This path currently covers edits such as literals, numerical evidence and
same-span documentation when they preserve call resolution and clone analysis.
New/deleted files or declarations, changed calls/imports, shifted spans,
ambiguous clone changes, SCIP overlays, extraction-policy changes, forced
rebuilds and missing/corrupt extraction cache entries use full reconciliation.
A configured `maxGenerationBytes` selects the complete builder, which measures
all parse, resolve, and reduction stages against that custom ceiling.
A new owner has no warm graph until its first complete build. Neither the
150 ms debounce nor the local replacement guarantees sub-second publication
for every edit or project size.

Large graphs can prepare lossless 64 KiB publication blocks under the existing
native task reservation. This reduces retained storage input without changing
the canonical digest, extraction ceiling, 32 MiB sorting workspace, 4 MiB staging
batches or 512 MiB pending pool. Rows can span blocks and keep their independent
record/batch limits. Packed inputs release their full canonical tables before
storage admission and do not warm the sparse local-edit cache; subsequent
changes use complete reconciliation. Preparation is included in the supervised
`copy` stage, with the original cancellation signals and deadline policy.

Every publication still fences the exact base generation and digest and checks
the complete live source before/after publication. Query freshness uses live
source checks; it has no freshness TTL. Optional post-index model and structural
enrichment has a separate completion time from the base graph. Measure the
watcher's structural edit-to-current-query latency separately from an explicit
admin job's enrichment completion.

A file disappearing during discovery or reading can restart the complete source
observation at most twice, under the original deadline and cancellation. Each
attempt discards incomplete manifests and captured source. If a later code
window observation finds an edit, context downgrades both packet and response
freshness and confidence; it omits code whose indexed content hash differs.

## Space and reclamation

Use `cartograph db usage --project-path . --format json` before attributing
space to records. Logical graph bytes, native allocated bytes, SSTs, WAL,
memtables, and pending compaction represent different quantities. Repeated
revisions store record changes and retain snapshot-visible versions. Bounded
pruning removes obsolete versions; LSM compaction reclaims physical files later.
Inspect deferred maintenance and phase failures before repeating a batch.

## Current retrieval limits

Exact symbol-name lookup uses generation-scoped postings when their workspace
fits the admitted budget; older or oversized graphs use bounded scans. Exact
file lookup currently scans file records. Packed graph adjacency uses keyed
native records. BM25 persists field-aware term postings and exact corpus
statistics in the same publication as its documents. Forward records remove
obsolete terms on replacement or deletion. Legacy generations and documents
whose vocabulary exceeds the optional workspace select complete bounded corpus
search. Common terms still require reading all matching postings; the candidate
workspace remains 32 MiB. The canonical graph digest is unchanged by this
optional index.

Vocabulary maps share the configured workspace with live table sort entries or
sparse replacement data. Old vocabulary keys stream from the bounded encoded
record, including after lowering the workspace policy; only the new vocabulary
map allocates. Insufficient remaining workspace selects the complete scan.

Semantic readiness and a packed vector corpus are cached by exact generation,
model identity, vector epoch, and summary epoch from the same native snapshot.
The owner retains at most one optional 64 MiB corpus, with one builder. Native
MultiGet loads vector batches under that snapshot, with a native soft value
limit and an 8 MiB limit on copied values. Warm queries retain exact cosine
scoring and stable ID ties. A busy or oversized cache uses the bounded exact
streaming path. Model, summary, vector, and generation changes invalidate the
corresponding evidence; there is no readiness TTL or HNSW claim.

Readiness retains an eligible packed corpus during its existing validation pass,
then publishes the complete report and corpus under the same input fence. The
first vector query can reuse those validated values. Incomplete, corrupt,
cancelled, or exhausted validation discards the partial allocation and releases
the single builder permit; the 64 MiB cache bound is unchanged.

Source-only freshness scans can hash four files concurrently, with at most four
8 MiB source buffers and bounded observation queues. Reduction preserves the
original path order and manifest digest. Captured source windows and larger
files use serial hashing. A complete index can reuse a bounded manifest from its
initial hash scan instead of repeating discovery and source reads. Parsing and
publication still verify source identities and the complete source fences.

## Persistent autosync and longer workloads

`cartograph watch --project-path .` runs the existing native owner independently
of MCP stdin on Unix. On macOS, `cartograph service start --project-path .`
installs that executable as a private project-scoped login service. Additional
CLI/MCP processes use its existing owner transport. `service status` reports
registration, launchd state, and owner availability separately; `service stop`
stops only the owned service and removes its registration. An existing owner
retains the primary; the login service retries after it exits.

The macOS registration uses launchd's `Interactive` process type because CLI
and editor response times depend on this Unix-socket service. The default
process type throttles CPU and I/O, and XPC-based adaptive scheduling cannot
infer these socket requests. See Apple's
[launchd process-type contract](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5).

`autoSync.recentFailures` retains the latest 16 redacted failure events even
after recovery. `failureActive` distinguishes a current retry condition from
historical `lastErrorCode`, `lastCleanupFailureCode`, and `lastFailureAt`.
`lastIndexProfile` retains preparation, pipeline, per-stage, history, and total
timings from the latest successful index/no-op. These exclude debounce and the
subsequent query's source verification and transport time.
When `sourceManifestReused` is true, preparation includes complete discovery
and byte hashing; the pipeline starts with admission of those observations in
the read stage and does not report a second discover stage. XML declaration-role
hints remain global resolver inputs, so changing a MyBatis template role selects
complete reconciliation.

Run the sustained native workload explicitly:

```sh
cargo test --locked --release -p cartograph-cli --test rocks_owner \
  sustained_headless_owner_mixed_edits_and_mcp_readers -- --ignored --nocapture
```

Its default duration is 30 minutes. It mixes literal and Python body edits,
file/declaration additions, deletion, renames, and call-topology changes, with
three real MCP readers. It samples storage and failure history every minute,
measures edit-to-current MCP evidence, verifies each added/deleted declaration,
rename path and changed call target, and fences every cycle by changed generation,
source revision and content digest after completed autosync. It drains the owner
and compares the final graph with a forced cold rebuild. `CARTOGRAPH_MIXED_SOAK_SECONDS` accepts
10..7200 seconds for explicit harness validation or longer runs. A short harness
run does not count as sustained performance or storage evidence.

The frozen worker matrix and concurrent freshness tests are correctness and
workload-specific performance evidence. They are not proof of a universal
speedup or of unlimited graph capacity.

Direct `cartograph index` and `sync-if-dirty` publish the base graph and requested
history through one native owner. Their behavior is the same when a project service already owns
RocksDB. The owner request explicitly sets `baseOnly: true`; it does not start a
summary or classification sweep. MCP admin index/sync keeps its existing
structural enrichment by default. `enrich: false` skips configured model work;
`baseOnly: true` explicitly skips both structural and model enrichment. Optional
phase failures remain visible in the admin job, and a direct CLI cannot silently
render such a failed tail as a complete index report.

Structural file and module summaries save in atomic batches of at most sixteen
scopes with a combined 1 MiB input bound. Each batch streams the graph once,
collecting selected-scope inventory and bounded evidence together, then checks every
digest and preserves current model summaries before committing. A stale member
rejects the complete batch. Job status and cancellation bypass durable trace
initialization and recording so an unrelated state writer cannot hold their
acknowledgements behind trace I/O.

Structural role rules stream at most 320 declarations per native cursor page.
They read declaration metadata without loading source documents. The cursor is
pinned to its generation and producer model, advances through cached pages,
and rejects continuation after a publication. Saves from these pages require
the same generation and preserve a current judged role saved after page selection.
The rules commit at most sixteen roles per atomic native batch, with a combined
1 MiB input bound; one invalid member rejects the batch before publication.
Model-backed role sweeps retain their priority-ordered,
source-rich candidate API.

`cartograph admin status` returns one observation of a running or terminal job.
It does not wait for completion or cancel the observed job when the observer
exits. Commands that start jobs keep their polling and cancellation ownership;
`admin cancel` waits for the requested cancellation to drain.

Project status reads inventory and language totals together, and summary and role
counts together, each pinned to the observed generation. Artifact scope and value
lookups use native batches of at most 32 keys, with the existing physical read,
workspace and response-byte bounds. This avoids repeated scans and reduces the
number of large storage reservations waiting for admission. A failed aggregate
names its static phase, elapsed time and redacted failure category; a changed
generation cannot be reported with counts from a different publication. After
all aggregates and rollups finish, a bounded current-pointer read checks the
original generation under the original request deadline and cancellation. A
concurrent publication rejects the response with a conflict instead of mixing
generations or restarting the request.
