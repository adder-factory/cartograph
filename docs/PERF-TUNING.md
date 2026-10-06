# Performance tuning

[Documentation home](README.md) · [Project overview](../README.md) ·
[Configuration](CONFIGURATION.md) · [Benchmark evidence](v2/benchmarks/README.md)

Cartograph v2 chooses bounded parallelism from supported-file count, exact
indexed source bytes, hardware, and the caller cap. The corpus selector requests
the tiers 1, 2, 4, 8, and 16, then applies caller and detected-hardware caps; the
reported final count can therefore be intermediate, such as 14 on a 14-core
host. A deterministic reducer must produce identical logical facts, digests,
and ordered retrieval evidence at every admitted count. The release identity
matrix explicitly exercises 1, 2, 4, 8, and 16 workers; faster output is never
allowed to change meaning.

> [!IMPORTANT]
> Tune only after measuring the failing phase. More workers, connections,
> memory, or time cannot repair a stale fence, lost lease, blocked database,
> unsupported source boundary, or unhealthy derived index; use
> [Troubleshooting](TROUBLESHOOTING.md) for those states first.

**On this page:** [Operator controls](#operator-controls) ·
[Worker selection](#worker-selection) ·
[Generation storage and spill](#generation-storage-and-spill) ·
[Database pool and WAL](#database-pool-and-wal) ·
[Auto-sync watcher](#auto-sync-watcher) ·
[Automatic failure handling](#automatic-failure-handling) ·
[Backend log cleanup](#backend-log-cleanup) ·
[Measurement gates](#measurement-gates)

## Operator controls

```sh
cartograph index . --workers 8

# External database only (CARTOGRAPH_DATABASE_URL set):
export CARTOGRAPH_DATABASE_MAX_CONNECTIONS=8
export CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS=5000
export CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS=120000
```

- More parse workers help only when corpus size and CPU justify them. Database
  COPY, derived-index build, and publication remain bounded phases.
- Do not increase timeouts to hide a lost lease, stale fence, blocked database,
  or oversized corpus. Inspect task/lease status and the failing phase first.
- Semantic HNSW indexes are per model; unused model generations should be
  audited and cleaned with explicit semantic-maintenance commands.
- ParadeDB BM25 is generation-local derived state. Inspect/rebuild it through
  `cartograph db derived-index`, not ad-hoc DDL.

### Worker selection

The adaptive selector promotes above 8/16/32/128 supported files or above
128 KiB/512 KiB/1 MiB/4 MiB indexed source bytes, using whichever dimension
requests more parallelism, then applying caller and hardware caps.

| Workers requested | By supported files | By indexed source bytes |
| ---: | --- | --- |
| 1 | Up to 8 | Up to 128 KiB |
| 2 | Up to 16 | Up to 512 KiB |
| 4 | Up to 32 | Up to 1 MiB |
| 8 | Up to 128 | Up to 4 MiB |
| 16 | More than 128 | More than 4 MiB |

Files and bytes each request a tier independently; the larger request wins.
For example, 5 files totalling 10 MiB request 16 workers.

The measured 34-file/1.05 MiB native corpus selects eight workers; the
256-file/5.86 MiB synthetic corpus selects 16. Automatic structural indexing is
capped at four native workers (see [Auto-sync watcher](#auto-sync-watcher)).

### Generation storage and spill

Native generation storage defaults to `auto`. It selects PostgreSQL spill at
64 Cargo manifests, 10,000 supported files, 64 MiB of indexed source, or when
a conservative 16x expansion estimate reaches `maxGenerationBytes`.

| Setting | Use it when |
| --- | --- |
| `generationStorage: "memory"` | Only when measured headroom favors the faster in-memory reducer |
| `generationStorage: "postgres"` | To force the durable path for a known dense corpus |

> [!WARNING]
> PostgreSQL spill trades database I/O, WAL, heap/index allocation, and
> temporary-sort space for a much smaller Rust payload. `maxSpillBytes` is
> logical accounting, not physical disk reservation. Measure `db usage`, free
> space, temporary-file behavior, and stage timings before raising its 128 GiB
> default.

| Spill bound | Value |
| --- | --- |
| Files per parse work item | At most 64 |
| Combined source per parse work item | At most 64 MiB, whichever boundary is reached first |
| Per-file ceiling | `maxFileSize`, at most 32 MiB; a single file is never split across items |
| Reduction partitions | 64 deterministic UUID partitions for each of six relations |
| Partitions committed per transaction | Four contiguous partitions |
| Embedding carry-forward page | 4,096 documents |

> [!CAUTION]
> Exact retries resume from the durable reduction cursor. Do not manually delete
> raw rows or advance it.

<details>
<summary>Details: parse items, resolver COPY groups, reduction, and publication</summary>

- The PostgreSQL path lazily admits at most 64 files and 64 MiB of combined
  source per parse work item, whichever boundary is reached first. A single
  file is never split; the per-file ceiling is 32 MiB. Later item deadlines
  start only when the bounded scheduler admits them.
  Each item reuses one tree-sitter extractor per encountered language instead
  of rebuilding parser/query state for every file. Cooperative cancellation
  still checks parent, sibling, and item-deadline state at every requested poll;
  monotonic watch signals use their atomic version rather than a read lock.
  Cacheable extraction payloads are written once to the immutable parse cache
  and the staging generation retains a foreign-key-protected reference; an
  inline payload is the bounded fallback when a cache write fails.
- Resolver workers publish validated typed rows through bounded COPY groups.
  A group is capped by rows, logical bytes, and retained Rust bytes, so faster
  publication cannot become a hidden whole-generation allocation.
- Spill reduction retains 64 deterministic UUID partitions for each of six
  relations and commits four contiguous partitions per transaction. It runs
  `ANALYZE` on each raw spill table before the first partition group of its
  relation checks it, and on each canonical table after the group that finishes
  filling it, so the planner sees statistics that include this generation. It
  checks conflicts and cross-relation integrity before deleting those raw rows
  and advancing the durable cursor. Exact retries resume from that cursor. A
  reduce statement that outlives its timeout reports
  `reduce_deadline_exceeded`.
- Document conflict detection probes the typed reduction index for another row
  with the same document identity. Large code/natural-text fields are compared
  exactly only for an actual duplicate identity; unique documents are never
  materialized into a generation-wide `DISTINCT` hash merely for validation.
- Publication carries unchanged documents' embeddings forward in keyset pages
  of 4,096 documents and advances prepare progress after each page, which lets
  fully embedded large projects re-index within the prepare timeout.

</details>

### Database pool and WAL

The pool and timeout variables apply only to an external database, that is,
when `CARTOGRAPH_DATABASE_URL` is set; that variable also takes precedence over
a managed database. Keep the database pool large enough for the selected
operation but below the 64-connection hard cap. Local agent use normally needs
no manual change.

| Setting | External database | Managed database |
| --- | --- | --- |
| Pool size | `CARTOGRAPH_DATABASE_MAX_CONNECTIONS`, default 8, hard cap 64 | Fixed 8 connections |
| Acquire timeout | `CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS`, default 5000 ms | Fixed 10,000 ms |
| Query timeout | `CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS`, default 120000 ms | Default 120,000 ms |
| TLS | `CARTOGRAPH_DATABASE_REQUIRE_SSL`, default false | Not forced |

Newly created managed databases keep synchronous durability while using these
WAL settings:

| Managed setting | Value |
| --- | --- |
| Checkpoint interval | 15 minutes |
| Soft `max_wal_size` | 2 GiB |
| `min_wal_size` | 256 MiB |
| `wal_compression` | `lz4` |

`cartograph doctor` warns when a container predates these settings;
`cartograph db upgrade --confirm upgrade-managed-database` recreates it on the
same data volume. `db usage` and free-space checks remain the operator boundary.

<details>
<summary>Details: why the WAL ceiling is 2 GiB</summary>

Immutable-generation COPY and BM25 publication can exhaust PostgreSQL's 1 GiB
default repeatedly during rapid editor bursts, forcing overlapping checkpoints
and increasing foreground latency, so the ceiling stays above it. It is also
each project's steady-state WAL footprint: WAL left after a busy period is kept
up to the ceiling, and an idle database does not checkpoint it away. The earlier
4 GiB ceiling left 4.1 GiB of WAL in each recently busy project on a shared
Docker disk. On this repository, a forced re-index wrote 440-480 MiB of WAL at
4 GiB and 510-770 MiB at 2 GiB with lz4 (570-920 MiB without it), because more
checkpoints fall inside the run.

</details>

### Auto-sync watcher

Native MCP auto-sync uses a recursive OS watcher where available, debounces
bursts, and periodically reconciles missed events. If native watching cannot be
established it uses the bounded polling watcher. Status exposes watcher events,
reconciliations, attempts, publications, and errors without project paths.

| Watcher bound | Value |
| --- | --- |
| Quiet window | 750 ms default; `CARTOGRAPH_WATCH_DEBOUNCE_MS` accepts 50 through 60,000 ms |
| Hard coalescing deadline | Two seconds, so a continuous stream of editor writes cannot postpone the next attempt forever |
| Quiet window above two seconds | That explicit quiet window is also the hard minimum latency bound |
| Automatic structural indexing | Capped at four native workers |

Watcher admission uses the same default/project include, exclude, and language
policy as indexing, reloads after `config.json` changes, and ignores access-only,
build-output, dependency-cache, and private `.cartograph` churn. Configuration
and SCIP-overlay events remain explicit reconciliation triggers.

<details>
<summary>Details: what an automatic index skips and reuses</summary>

An admitted filesystem event goes directly through the indexer's own complete
manifest/no-op fence instead of performing a second full status manifest scan
first. Automatic structural indexing is capped at four native workers and skips
the independent Git churn, co-change, and issue-history refreshes. An explicit
`cartograph index` retains the normal corpus-aware worker ceiling and refreshes
those auxiliary Git channels. When HEAD, shallowness, the number of reachable
commits within the bound, the commit bound, the enabled channels and the mining
version all match the last stored refresh, churn and co-change
evidence is reused without rescanning Git or rewriting its rows, and the index
report marks the history `reused: true`. `cartograph history --mode refresh` always
rescans.

Ready generations record their exact fact counts and source bytes, so status,
freshness and storage summaries read them instead of counting every fact table
on each call. Generations published before schema 45 fall back to counting
until the next unchanged index records their counts once. Periodic missed-event
reconciliation still uses a complete status scan as its correctness boundary.

</details>

### Automatic failure handling

An unchanged source revision that fails automatic indexing is not retried in a
tight loop. Auto-sync records its stable stage code and retries according to
the failure class:

| Failure class | Retry delay | Circuit behavior |
| --- | --- | --- |
| Transient: `lease_busy`, `retention_backlog`, and source-changed failures | 2 seconds, doubling to 30 seconds | Does not count toward the persistent per-revision limit or the repeated-failure circuit |
| Persistent failure of a known revision | 30 seconds, doubling to a 15-minute cap | After five failed automatic attempts, the revision is suppressed until the supported source revision changes |
| Same stable failure code, any revision | Persistent delay | Five consecutive occurrences trip a cross-revision circuit, even if editor changes keep producing new source revisions |
| Generation capacity | Persistent delay | Independent cross-revision circuit breaker after five capacity failures |
| Indexing and status both unavailable | Same backoff, applied to subsequent watcher events | A typed unknown-revision failure bucket keeps one bounded recovery probe at the capped interval instead of suppressing database recovery forever |

**Backlog drain before admission.** An automatic attempt whose project already
has more than one failed or partially retired generation first runs one bounded
retention pass. While that drain makes progress but the backlog stays over the
bound, the attempt is deferred with `retention_backlog` (a transient failure).
A drain that commits nothing needs operator action, so the attempt proceeds
instead of freezing automatic indexing. Explicit requests are never deferred.

Automatic failures run the same bounded terminal-generation retention path
before returning, so a failing watcher does not leave one new failed spill
generation per attempt.

**Recovering from a capacity failure.** Select PostgreSQL spill when
appropriate; `maxGenerationBytes` cannot exceed 8 GiB, so at that ceiling
exclude generated/compiled artifacts and then run an explicit
`cartograph index`. The next fresh status clears the circuit.

Explicit/manual index requests remain available. A new source revision clears
ordinary revision suppression but never bypasses an unresolved repeated-failure
or capacity circuit. A successful manual or automatic index clears both
circuits.

Structured status exposes these fields:

| Field | Meaning |
| --- | --- |
| `lastErrorCode` | Stable code of the latest failed automatic index |
| `lastCleanupFailureCode` | `index_cleanup_failed` when bounded cleanup of that attempt's own staging generation also failed; `lastErrorCode` keeps the primary code |
| `lastFailureAt` | Unix-millisecond time of the latest failed automatic index |
| `nextRetryAt` | Unix-millisecond deadline for the next automatic retry |
| `failedRevisionAttempts` | Automatic failures recorded for the current known or unavailable-revision bucket |
| `retrySuppressed` | The current revision, or a cross-revision circuit, is exhausted |
| `repeatedFailureAttempts` | Consecutive automatic failures with the same stable code across revisions |
| `repeatedFailureRetrySuppressed` | The repeated-failure circuit is tripped |
| `capacityFailureAttempts` | Generation-capacity failures since the last successful automatic index |
| `capacityRetrySuppressed` | The capacity circuit is tripped |
| `capacityLimit`, `capacityScope`, `capacityNextAction` | The capacity limit/scope/next action of an unresolved capacity failure |

### Recovery host

Start a recovery host with `cartograph serve --mcp --no-auto-sync` when an
operator needs filesystem watching and periodic reconciliation fully paused
while an explicit prune drains a historical backlog. `--no-startup-sync`
continues to suppress only the initial catch-up.

### Backend log cleanup

Managed local LLM logs rotate at 32 MiB and retain one `.1` file. Inspect stale
rotated logs and invalid PID state without mutation, then apply only a bounded
age-qualified batch with the exact confirmation:

```sh
cartograph backend cleanup . --json
cartograph backend cleanup . --apply --confirm cleanup-backend-junk --json
```

Cleanup never removes current logs, valid process state, or any active backend.

## Measurement gates

Committed reports under `docs/v2/benchmarks/` cover synthetic COPY/index
scaling, native corpus 1/2/4/8/16-worker identity, patch-task retrieval, and
the [large public corpus streaming run](v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md).
The benchmark executables live under `crates/cartograph-indexer/benches/` and
Rust integration tests. Measure a representative corpus before changing
defaults, and retain raw bounds/digests with the report.

The live release workflow also verifies cancellation, timeout, panic, caller
drop, database fault, lease takeover, rollback, and zero staged residue. A
throughput improvement is not releasable if any cleanup or deterministic-output
gate regresses.
