# PostgreSQL storage and operations

[Documentation home](README.md) · [Project overview](../README.md) ·
[Configuration](CONFIGURATION.md) · [Troubleshooting](TROUBLESHOOTING.md)

Cartograph v2 has one storage engine: PostgreSQL 18.4 or newer within major
version 18 with ParadeDB `pg_search` 0.26.0 and pgvector 0.8.4 or newer.
Pgvector 0.8.7 is recommended for external PostgreSQL; the managed upstream
ParadeDB 0.26.0 image bundles `pg_search` 0.26.0 and pgvector 0.8.6. SQLite is
not a backend, fallback, migration target, importer, feature, or test utility.

Read this page to choose who owns the database, run lifecycle and recovery
commands safely, and understand how generations, retention, and compaction use
storage.

**On this page:** [Choose database ownership](#choose-database-ownership) ·
[Capability contract](#capability-contract) ·
[Managed local database](#managed-local-database) ·
[External database](#external-database) ·
[Destructive operations and confirmation phrases](#destructive-operations-and-confirmation-phrases) ·
[Generation model](#generation-model) ·
[Derived BM25 and vector state](#derived-bm25-and-vector-state) ·
[Import from v1.1.33 PostgreSQL](#import-from-v1133-postgresql) ·
[Bounded retention](#bounded-retention) ·
[Storage measurement and online compaction](#storage-measurement-and-online-compaction) ·
[Distribution boundary](#distribution-boundary)

## Choose database ownership

| Path | Best for | Ownership | Start with |
| --- | --- | --- | --- |
| Managed local database | One developer or coding agent on macOS/Linux with local Docker | Cartograph-owned loopback container, volume, credential, and lifecycle | `cartograph db start --project-path .` |
| External PostgreSQL | Windows, shared infrastructure, or administrator-operated databases | The database administrator owns service, extensions, backup, and access policy | Load `CARTOGRAPH_DATABASE_URL`, then run `cartograph doctor .` |

Both paths use the same PostgreSQL-only schema and generation contract. Managed
mode is not a bundled database: it asks the user's local Docker daemon to pull
the pinned upstream image and refuses foreign or remotely owned resources.

| Concern | Managed local database | External PostgreSQL |
| --- | --- | --- |
| Platforms | macOS/Linux with a local Docker daemon; a remote Docker context is refused. Disabled in Windows release binaries. | Any; Windows release binaries use external PostgreSQL. |
| Service and extensions | Cartograph pulls the pinned upstream ParadeDB image. | The administrator installs PostgreSQL, `pg_search`, and pgvector. |
| Credential | Generated private local credential. | `CARTOGRAPH_DATABASE_URL` in the process environment. |
| Connection pool | Fixed: 8 connections, 10-second acquire timeout. | [`CARTOGRAPH_DATABASE_*` pool, timeout, and TLS variables](#external-database). |
| Schema selection | `CARTOGRAPH_DATABASE_SCHEMA` | `CARTOGRAPH_DATABASE_SCHEMA` |
| Container resources and server settings | Set by Cartograph; see [managed container policy](#managed-container-policy). | The administrator. |
| `db start`, `status`, `stop`, `logs`, `backup`, `restore`, `upgrade`, `remove`, `derived-index` | Available. | Not available: these commands operate only on the project-owned container. |
| `db usage`, `db prune`, `db compact`, `db import-v1` | Available. | Available. |
| Compaction free-space headroom | Measured from the validated project-owned data volume; `--available-headroom-bytes` is rejected. | The operator must supply `--available-headroom-bytes`. |
| `pgstattuple` for heap measurement | Installed in new managed databases. | The operator must install it. |

## Capability contract

`cartograph doctor` fails closed unless the selected database proves:

- PostgreSQL 18.4 or newer within major version 18;
- the expected `pg_search` version and preload state;
- the `paradedb` access method and `pdb.source_code` tokenizer behavior;
- pgvector 0.8.4 or newer;
- the complete append-only Cartograph migration ledger;
- bounded read/write/DDL capability in the selected schema.

Cartograph never silently degrades to PostgreSQL built-in FTS or a local file.

## Managed local database

On macOS/Linux with a local Docker daemon:

```sh
cartograph db start --project-path .
cartograph db status --project-path .
cartograph doctor .
```

`db start` exits with status 75 when its schema migration ends as
[`schema_busy`](#schema-migrations), and with status 1 for every other failure.

The native lifecycle creates a project-owned container and volume, generates a
private local credential, binds PostgreSQL to loopback, and pulls the pinned
upstream ParadeDB image. It refuses a remote Docker context, foreign resources
with colliding names, wrong labels/mounts, a public bind, and an unproved
database capability.

Common read/idempotent operations:

```sh
cartograph db status --project-path .
cartograph db logs --project-path . --tail 200
cartograph db derived-index --project-path .
cartograph db stop --project-path .
```

Backup:

```sh
cartograph db backup ./cartograph.backup --project-path .
```

Windows release binaries use external PostgreSQL. Managed lifecycle remains
disabled there until private credential ACL behavior can be proved equivalent.

### Managed container policy

New and upgraded containers reserve 256 MiB of shared memory and HNSW creation
disables parallel maintenance workers so vector-index construction stays
bounded. Newly created and upgraded containers run with these limits and WAL
settings:

| Setting | Value |
| --- | --- |
| Memory limit (hard) | 2 GiB |
| Memory reservation (soft) | 1 GiB |
| CPUs | 4 |
| Process limit | 256 PIDs |
| Shared memory | 256 MiB |
| `checkpoint_timeout` | 15 minutes (`15min`) |
| `max_wal_size` | `2GB` |
| `min_wal_size` | `256MB` |
| `wal_compression` | `lz4` |

`db status` reports `resource_limits_match` and `postgres_settings`:

| `postgres_settings` | Meaning |
| --- | --- |
| `current` | The container runs the current PostgreSQL server settings. |
| `outdated` | An older container keeps working; `db upgrade` applies the current settings. |
| `absent` | No managed container exists. |

### Schema migrations

Cartograph schema migrations run transactionally; the managed start bounds
each statement with a 60-second PostgreSQL deadline. Each attempt waits at most
two seconds for any one lock, so a schema change queued behind another
session's long transaction never stalls that session's new readers for longer.
A contended attempt rolls back whole and is retried after a one-second pause
for up to five minutes, after which the migration reports the retryable
`schema_busy` with nothing applied (see
[Troubleshooting](TROUBLESHOOTING.md#schema-migration-reports-schema_busy)).

If an older ledger still cannot advance, runtime and doctor output name the
recorded version, required version, and exact next pending migration
(MCP structured failure reports use the stable reason
`schema_migration_blocked`); doctor does not run a project-status query against
columns that migration has not yet proved.

### Upgrade the managed container

`db upgrade` replaces an owned container when any of these holds:

- its image is not the pinned exact digest (an older managed image);
- it uses the same image, but its shared-memory allocation is below the current
  HNSW requirement (a same-image legacy container). `doctor` reports that
  condition before embedding work reaches index creation;
- it uses the same image, but its resource limits differ from the
  [managed container policy](#managed-container-policy);
- it uses the same image, but its PostgreSQL/WAL settings are outdated
  (`postgres_settings: outdated`).

> [!CAUTION]
> `db upgrade` replaces the managed container. Before replacing an older
> managed image, create and retain a verified backup, then use the explicit
> upgrade capability:

```sh
cartograph db backup ./cartograph.backup --project-path .
cartograph db upgrade --project-path . \
  --confirm upgrade-managed-database
```

Required free space is the greater of 64 MiB or the configured schema's
complete index allocation plus ten percent of the current database allocation.
An unavailable storage snapshot or insufficient headroom fails before image or
container cutover. To resume an interrupted or failed upgrade, repeat the same
confirmed command.

<details>
<summary>Details: headroom measurement, extension reconciliation, and resumable recovery</summary>

Before the old container is renamed, the upgrade reads free space from the
validated project-owned data mount and queries bounded current allocation from
the healthy PostgreSQL database. Required headroom is the greater of 64 MiB or
the configured schema's complete index allocation plus ten percent of the
current database allocation. This reserves one replacement copy of every index
plus bounded WAL/catalog scratch for extension upgrades. An unavailable storage
snapshot or insufficient headroom fails before image or container cutover.

The upgrade starts the exact digest against the retained volume, reconciles
pgvector and then `pg_search` transactionally before calling extension-defined
functions, and requires capability plus Cartograph migration proof before it
discards the old container. The ParadeDB 0.26.0 image upgrades `pg_search` to
0.26.0 and retains the legacy `bm25` access method, so existing
derived indexes remain valid and queryable; Cartograph
creates replacement/new generation indexes with the current `paradedb` access
method and accepts both catalog names during this upgrade boundary.

Failures before the extension transaction restore the old container. Once an
extension-catalog mutation has been attempted, Cartograph instead retains the
new exact-digest container as the only runnable candidate and keeps the old
container stopped. Repeating the same confirmed upgrade resumes verification
and removes the stopped rollback container only after every proof passes; it
never restarts an old image against a possibly newer extension catalog.
If the client is interrupted after the stopped old container is renamed but
before the candidate exists, the next confirmed upgrade validates that sole
owned rollback slot, recovers its canonical name, and continues the cutover.
Foreign, running, or otherwise ambiguous rollback topologies fail closed.

</details>

Restore, upgrade, derived-index rebuild, and removal replace or delete state and
require an exact confirmation phrase; see
[Destructive operations and confirmation phrases](#destructive-operations-and-confirmation-phrases).
The lifecycle validates archive/resource identity before mutation and tests
rollback/recovery paths against the pinned service.

## External database

The database administrator installs PostgreSQL 18.4 or newer within major
version 18, `pg_search` 0.26.0, and pgvector 0.8.4 or newer (0.8.7
recommended), and creates pgvector before `pg_search`. Supply secrets only
through the process environment:

```sh
export CARTOGRAPH_DATABASE_URL='postgresql://<user>:<password>@127.0.0.1:5432/cartograph'
export CARTOGRAPH_DATABASE_SCHEMA='cartograph_project'
cartograph doctor /absolute/path/to/project
cartograph index /absolute/path/to/project
```

For an existing external database, quiesce Cartograph writers, retain a verified
database backup, install the new extension binaries, restart PostgreSQL, and
update both catalogs before running `cartograph doctor`:

```sql
ALTER EXTENSION vector UPDATE TO '0.8.7';
ALTER EXTENSION pg_search UPDATE TO '0.26.0';
```

Optional bounded pool, timeout, and TLS controls:

```sh
export CARTOGRAPH_DATABASE_MAX_CONNECTIONS=8
export CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS=5000
```

| Variable | Default | Accepted values | Controls |
| --- | --- | --- | --- |
| `CARTOGRAPH_DATABASE_MAX_CONNECTIONS` | 8 | 1–64 | Connection-pool size |
| `CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS` | 5000 | 1–120000 | Pool and initial-connect acquisition timeout |
| `CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS` | 120000 | 1–600000 | PostgreSQL session statement timeout |
| `CARTOGRAPH_DATABASE_REQUIRE_SSL` | `false` | `true`/`1` or `false`/`0` | Forces TLS at the driver boundary even when the URL omits `sslmode=require` |

> [!NOTE]
> These pool, timeout, and TLS variables apply only when
> `CARTOGRAPH_DATABASE_URL` is set. Managed mode uses a fixed 8-connection pool
> with a 10-second acquire timeout. `CARTOGRAPH_DATABASE_SCHEMA` applies in both
> modes.

> [!WARNING]
> Do not commit or print a database URL. Public errors and debug output are
> required to omit credentials, query text, and absolute project paths.

## Destructive operations and confirmation phrases

Every command that replaces or deletes state requires `--confirm` with an exact
phrase. Command help names the phrase only for `db restore`, `db import-v1`,
`db prune`, and `db compact`; `db upgrade` and `db remove` help says only
"Exact acknowledgement for the requested destructive operation", and
`db derived-index` help says only "Exact acknowledgement required with
--rebuild".

> [!CAUTION]
> Take and retain a verified backup before any of these operations.

| Operation | Command | `--confirm` phrase | Named in `--help` | Database |
| --- | --- | --- | --- | --- |
| Replace managed contents from a verified archive | `cartograph db restore <archive>` | `restore-managed-database` | Yes | Managed only |
| Replace the managed container | `cartograph db upgrade` | `upgrade-managed-database` | No | Managed only |
| Permanently remove the project-owned resources | `cartograph db remove` | `remove-managed-database` | No | Managed only |
| Rebuild generation-local BM25 relations | `cartograph db derived-index --rebuild` | `rebuild-managed-derived-indexes` | No | Managed only |
| Import a v1.1.33 PostgreSQL schema | `cartograph db import-v1` (without `--dry-run`) | `import-v1-postgres` | Yes | Managed or external |
| Delete old generations | `cartograph db prune` | `prune-old-generations` | Yes | Managed or external |
| Rebuild bloated B-tree indexes online | `cartograph db compact --apply` | `compact-online-indexes` | Yes | Managed or external |
| Rewrite heap relations offline | `cartograph db compact --heap --apply` | `compact-heap-relations` | Yes | Managed or external |

## Generation model

Indexing stages a complete immutable generation, validates files/symbols/edges/
references/search documents, and then builds derived search state from those
canonical rows. Only a fully validated generation can become `ready`, and
publication is a short current-pointer swap. Re-indexing an identical
supported-source manifest is a no-op unless `--force` is explicit.

<details>
<summary>Details: BM25 relation build and publication</summary>

For BM25, indexing populates an immutable physical
`search_g_<generation UUID>` table, creates that table's `_bm25` index, verifies
source/relation/distinct row counts plus catalog health, and records the project,
generation, content digest, count, and format version in
`generation_search_relations`. Migration 11 enforces generation UUIDs as
globally unique because the trusted physical identifier is generation-derived.
Only then can the generation become `ready`.
The publication transaction requires that exact relation again before it swaps
the project's current pointer. Any table/index build failure rolls back with the
staging transaction and can never publish a partial generation.

</details>

Writers use project/operation advisory locks, explicit leases, PostgreSQL-clock
heartbeats, exact fencing tokens, bounded statement deadlines, and rollback on
lost/expired ownership.

### PostgreSQL spill

Large native generations can use a staging-only PostgreSQL spill in the same
database/schema. The spill tables are children of one immutable generation and
are inaccessible to current-generation readers. The project settings
`generationStorage`, `maxSpillBytes`, and `maxSpillRows` control selection and
logical quotas:

| Setting | Default | Hard maximum |
| --- | --- | --- |
| `generationStorage` | `auto` | — |
| `maxSpillBytes` | 128 GiB | 1 TiB |
| `maxSpillRows` | one billion rows | ten billion rows |

Logical bytes exclude PostgreSQL/WAL/index/temporary-space amplification. An
abandoned generation is removed through the ordinary generation
cascade/retention path.

> [!WARNING]
> Never delete individual spill relations manually.

<details>
<summary>Details: spill writes, partitioned reduction, and the canonical digest</summary>

Every mutation rechecks the exact index lease and generation sequence.
File-local extraction payloads are streamed in bounded batches. Cacheable
payloads are stored once in the immutable parse cache and spill rows retain
foreign-key-protected references; an inline digest-checked payload is used only
when cache publication is unavailable. Resolver output is row-validated and
COPY-published into typed raw files/symbols/edges/references/numerical-sites/
documents tables without a JSONB fact-batch intermediary.

After resolution seals exact raw counts, Cartograph reduces 64 UUID partitions
per typed relation in four-partition transaction groups. Each transaction
detects identity conflicts, proves the group's file/symbol/span
cross-relations, aggregates edge multiplicity/confidence, inserts canonical
generation rows, removes those raw rows, and advances a durable cursor. The
document relation uses an indexed exact duplicate-identity probe, so large
text fields are compared only when two raw rows claim the same document ID.
This changes no conflict semantics and avoids materializing unique document
text into a `DISTINCT` aggregate. The
canonical V22 digest streams exact canonical row bytes from PostgreSQL in the
same table/key order as the memory reducer. The final ready transaction checks
the lease/state, the durable `canonicalized` phase that only validated groups
can reach, digest capability, and canonical counts, builds the generation
search relation, removes the spill run, and marks the generation ready.
Publication remains the later short current-pointer swap.

</details>

## Derived BM25 and vector state

Relational graph/search-document rows are source-of-truth data. ParadeDB BM25
and model-scoped HNSW indexes are rebuildable derived state. BM25 is generation
local rather than one global corpus, so documents in another project or a
ready/superseded generation cannot perturb current-generation scores or order.
A bounded read validates one expected current generation in a repeatable-read
transaction and queries only its verified physical relation. Inspect aggregate
derived-index health with:

```sh
cartograph db derived-index --project-path .
```

When recovery is required, rebuild with the exact confirmation phrase (command
help does not print it):

> [!CAUTION]
> A rebuild replaces derived state. Never describe the Community BM25 index as
> WAL-crash-durable or use a rebuild to conceal loss of relational source rows.

```sh
cartograph db derived-index --project-path . \
  --rebuild --confirm rebuild-managed-derived-indexes
```

> [!NOTE]
> `db derived-index` works only with a managed database. External PostgreSQL
> has no CLI equivalent for inspecting or rebuilding derived-index health.

Migration and startup reconciliation prioritize unhealthy current/ready
relations, repair at most 64 per invocation from canonical `search_documents`,
and fail closed if another bounded pass is required. It also removes at most 64
orphan tables whose names strictly decode as generation relation identifiers.
Builds, repairs, and retention drops share a generation-specific transactional
advisory lock. Healthy catalog/table/index tuples are left unchanged.

## Import from v1.1.33 PostgreSQL

V2 never opens SQLite. If the only v1 graph is SQLite, either rebuild from the
checkout or first use the v1.1.33 binary to migrate it to PostgreSQL.

The v1 source and v2 destination must be different schemas in the same database.
The destination may already contain a current generation; import publishes a
new immutable generation.

> [!IMPORTANT]
> Back up the database, quiesce project index/sync/hook/rebuild writers, and
> select the v2 destination through `CARTOGRAPH_DATABASE_SCHEMA`.

`--project-path` identifies the initialized v2 destination; `--source-checkout`
may point at a detached byte-exact historical tree so current dirty work never
needs to be rewound. Preflight that exact checkout first:

```sh
cartograph db import-v1 \
  --project-path /absolute/path/to/current-project \
  --source-checkout /absolute/path/to/exact-v1-checkout \
  --source-schema cartograph_v1 \
  --dry-run \
  --format json
```

The preflight validates schema history, bounded rows/bytes/JSON, supported
languages, repository/source identity, current checkout bytes, relations,
coordinates, body/content hashes, and canonical output without writing the
destination. It independently discovers the exact v1.1.33-compatible checkout
path/content set, excluding only additive v2 `.pyi` and TOML modes, and rejects
any missing, extra, or substituted v1 file before mutation. It reports that raw
verified manifest as the imported generation revision.

| Bound | Flag | Default | Maximum |
| --- | --- | --- | --- |
| Aggregate source/metadata bytes | `--maximum-source-bytes` | 512 MiB | 512 MiB; the flag can only lower the ceiling |
| Rows | `--maximum-rows` | 10,000,000 | 100,000,000 |

After a clean report:

```sh
cartograph db import-v1 \
  --project-path /absolute/path/to/current-project \
  --source-checkout /absolute/path/to/exact-v1-checkout \
  --source-schema cartograph_v1 \
  --confirm import-v1-postgres \
  --format json
```

The importer persists monotonic `staged`, `ready`, `bm25_rebuilt`, and
`complete` checkpoints. Repeat the identical command after a reported
interruption; changed source identity or inconsistent checkpoint state fails
closed. Reference/edge multiplicity is preserved. Exact spans remain exact only
when v1 plus current source proves the token; otherwise the span is marked
coarse. A SCIP placeholder hash proves its path-derived placeholder identity,
not historical bytes v1 never stored.

Additive v2 file types or source-policy inputs can make the imported v1
generation correctly report stale. Run a normal v2 index before the final
status/retrieval verification in that case.

The error "another Cartograph writer published during v1 import; retry after it
is idle" (internally `ConcurrentPublication`) is retryable only after writers
are quiesced. The failed attempt atomically releases its stale generation;
repeat the identical confirmed command to reset that durable run and reserve a
newer generation.

Before retiring v1, keep the source schema and backup and verify:

```sh
cartograph status /absolute/path/to/checkout
cartograph context 'trace the primary request flow' --project-path /absolute/path/to/checkout
cartograph db derived-index --project-path /absolute/path/to/checkout  # managed database only
```

The `db derived-index` check works only with a managed database; external
PostgreSQL has no CLI equivalent, so verify with `status` and `context` there.

## Bounded retention

Retention is a resumable drain of independently committed transactions. A
failed or cancelled later batch cannot undo earlier committed progress, and the
current complete generation remains the reader's source of truth. Repeat
bounded cleanup until `retiring_remaining` and the eligible backlog reach zero,
and inspect `batches_committed`, `cascade_rows_removed`, and `deferred_reason`
on every pass.

### Automatic retention

Successful index and no-op reconciliation requests remove up to 32 generations
while preserving the two newest superseded generations. Failed automatic
indexes attempt the same maintenance before returning. Generation cleanup and
parse-cache cleanup use separate transactions under one exact migration lease:
a failed generation pass still permits an independently fenced cache pass.

Before an automatic index reserves a generation, a project with more than one
failed or partially retired generation first runs one bounded retention pass.
If that pass committed cleanup and more than one such generation still remains,
the attempt is deferred with `retention_backlog` and the previous generation
stays visible. If the pass made no progress (a held project, a search-relation
budget, or a catalog mismatch), the attempt proceeds rather than freezing
automatic indexing, and its retention outcome remains visible in `db usage`.
Explicit requests are never deferred.

Cache retention protects the exact running parsing-policy contract, including
its AST-depth limit, and spill-pinned rows. Protected rows may exceed these
policy targets:

| Parse-cache policy target | Default |
| --- | --- |
| Recent older parsing-policy contracts retained | At most one |
| Rows | 20,000 |
| Logical payload | 2 GiB |
| Deletions per pass | 10,000 |

Cache hits touch `last_used_at` at most hourly. Automatic maintenance stores
its latest outcome, including failures during failed indexing, in one bounded
project row; `db usage` reports both phase outcomes and consecutive failures
through `retentionMaintenance`. A `cache_only` indexing outcome means cache
eviction committed while generation cleanup failed.

### Explicit pruning

Explicit pruning accepts larger audited budgets after a verified backup.

> [!CAUTION]
> `db prune` permanently deletes generations. Take a verified backup first.

```sh
cartograph db prune \
  --project-path /absolute/path/to/checkout \
  --keep-superseded 2 \
  --maximum-deletions 100 \
  --maximum-cascade-rows 5200000 \
  --confirm prune-old-generations \
  --format json
```

| Budget | Flag (MCP) | Default | Hard limit |
| --- | --- | --- | --- |
| Newest superseded generations retained | `--keep-superseded` | 2 | — |
| Generations deleted across committed batches | `--maximum-deletions` | 100 | 10,000 |
| Canonical rows actually deleted across committed batches | `--maximum-cascade-rows` | 5,000,000 | 100 million |
| Generation search-relation bytes | `--maximum-search-relation-bytes` (`maximumSearchRelationBytes`) | 8 GiB | 64 GiB |
| Search-relation drops | — | 64 | 64 |

An oversized generation can now make progress without increasing the
five-million-row limit. Generation limits remain independent of DDL limits, so
failed generations without derived relations can exceed the 64-drop cap. Large
relations still require a sufficient explicit `--maximum-search-relation-bytes`
budget (MCP `maximumSearchRelationBytes`) before they can be dropped. Raise it
only after verifying filesystem/WAL headroom.

### Drain bounds and order

| Bound | Value |
| --- | --- |
| Canonical rows per transaction | At most 10,000 |
| Candidate generations per transaction | At most 32 |
| Transactions per invocation | At most 512, within its existing time budget |
| Transaction ceiling | Ten seconds |
| Final generation-row delete | About two seconds in a savepoint, clamped to the time left in its transaction |
| Minimum age before removal | Staging work ten minutes; ready work 24 hours |

The invocation time budget includes connection acquisition, setup,
commit/rollback, and post-retention maintenance. A timed-out transaction's
connection is discarded before independent cache maintenance.

Each relation of a generation is drained in key order, and the next batch
resumes after the last deleted key, so a large failed generation is deleted in
one linear index walk instead of re-scanning the rows earlier batches removed.
Children are deleted before FK parents, with statement-local bounded tuple
selection; the parent is removed only after its descendants have been drained.
Deleting the final generation row runs every cascading foreign-key check inside
that bounded savepoint. If the checks outlast it, the row stays `retiring`, the
rows already drained stay deleted, and the report says
`deferred_reason: "parent_delete_deferred"`; a later prune or autovacuum lets
it finish.

<details>
<summary>Details: admission, protected generations, and the <code>retiring</code> state</summary>

There is no full fact-table row census before admission. Physical
search-relation metadata is checked separately, and individually over-budget
relations do not hide later eligible candidates. Exhausted DDL allowances also
filter relation-bearing work before pagination; `search_relation_ddl_budget`
reports that deferral without blocking later relation-free generations.

Staging work must be at least ten minutes old and ready work at least 24 hours
old. Current pointers, live leases, incomplete import recovery, and the retained
superseded histories are protected before a generation enters `retiring` state.
A retiring generation cannot acquire a writer lease, resume indexing, or publish.
Its original state is preserved for final removal counts.

</details>

<details>
<summary>Details: locks, fencing, and catalog verification</summary>

Every batch reacquires the schema/publication/retention locks and exact live
migration fence. Relation locks exclude FK-changing DDL while the complete
cross-schema cascade catalog, deletion order, and project/generation key mappings
are verified. Catalog drift fails closed. Expiry or takeover aborts the current
batch. The current complete generation remains the reader's source of truth.

</details>

<details>
<summary>Details: vacuum after retention</summary>

Automatic cleanup delegates dead-row reclamation to table-specific autovacuum.
Explicit `db prune` retains thresholded table-scoped maintenance after 100,000
deleted rows: `VACUUM`/`ANALYZE` uses `SKIP_LOCKED`, forced index cleanup, and
disabled truncation. Maintenance failure cannot roll back committed retention.
Physical heap/TOAST or index allocation remains a separate measured compaction
step. Empty spill heaps with large index allocation produce
`empty_spill_index_allocation`; inspect `db compact` for a bounded rebuild plan.

</details>

### Retained-byte reporting and recovery

Storage reports distinguish `databaseCatalogBytes`, covering non-shared relation
forks across all database schemas, from `unattributedDatabaseBytes`. The latter
may include auxiliary, transient, or historical unowned files; it does not prove
a particular leak and never authorizes deleting raw PGDATA files. Use supported
PostgreSQL/ParadeDB diagnostics and a verified backup/restore when physical
recovery is necessary.

Status and doctor expose generation-state counts plus a conservative retained
byte lower bound (source bytes plus physical generation search tables/indexes).
That lower bound deliberately excludes shared fact-table heaps and B-trees,
embeddings, and reusable dead space. Routine `status` also includes compact
whole-database and schema heap/index/TOAST totals; use `db usage` for the full
relation/cache/generation report.
Inspect the report and request another explicit batch if row/byte bounds leave
work.

> [!TIP]
> To quiesce a failing watcher during recovery, start the MCP host with
> `--no-auto-sync`, drain the bounded failed backlog, adjust the reported
> capacity setting, and prove one explicit index before restoring automatic
> watching.

## Storage measurement and online compaction

Use the read-only report before deciding that the database is bloated, then
choose a compaction mode.

| Mode | Plan (read-only) | Apply | Locking | Confirmation |
| --- | --- | --- | --- | --- |
| Online B-tree | `db compact`: `pgstatindex` per B-tree large enough to matter; size-only without `pgstattuple` | `REINDEX INDEX CONCURRENTLY`, one index at a time, outside a transaction | Schema advisory lock and per-index deadline; online | `compact-online-indexes` |
| Heap (offline) | `db compact --heap`: `pgstattuple_approx` on allowlisted Cartograph-owned main and TOAST heaps | `VACUUM FULL`, one table at a time, outside a transaction | `ACCESS EXCLUSIVE` per table; schema-wide maintenance gate rejects new project-operation leases | `compact-heap-relations` |

| Compaction flag | Default | Range |
| --- | --- | --- |
| `--maximum-indexes` | 32 | 1–64 |
| `--maximum-relations` | 8 | 1–32 |
| `--maximum-candidate-bytes` | 16 GiB | up to 64 GiB; at least `--minimum-reclaimable-bytes` |
| `--minimum-index-bytes` | 8 MiB | up to 64 GiB |
| `--minimum-reclaimable-bytes` | 64 MiB | up to 64 GiB |
| `--timeout-seconds` (each rebuild or rewrite) | 900 | 1–86400 |
| `--available-headroom-bytes` | — | External PostgreSQL only; rejected in managed mode |

### Measure storage

```sh
cartograph db usage --project-path . --limit 64 --format json
cartograph db usage --project-path . --limit 64 --table-offset 64 --index-offset 64 --format json
```

Both `db usage` and the default `db compact` plan verify the exact current
migration ledger without creating or upgrading the selected schema.

Table and index lists have independent offsets, complete catalog counts, and
truncation flags. `--limit` is at most 128; offsets are at most 100,000. Each
page is ordered by allocation and identity. Concurrent DDL or allocation changes
can reorder later pages; these are bounded observations rather than a durable
inventory cursor.

<details>
<summary>Details: <code>statistics</code> fields and their limits</summary>

`statistics` records the observation time, database-wide reset time, optional
statistics snapshot time, and `trackCounts` setting. Estimated live/dead rows
are nullable: no observed table counters or vacuum/analyze history means unknown,
rather than a measured empty table. Manual and automatic vacuum/analyze dates
are reported separately. PostgreSQL counters remain estimates and may lag or
reset independently; a database reset timestamp does not prove table-counter
history. An allocated table without observed statistics emits
`unobserved_table_statistics` without claiming that its space is reclaimable.

</details>

<details>
<summary>Details: allocation breakdown, parse-cache evidence, and duplicate content</summary>

It separates whole-database bytes from this schema's heap, B-tree/all-index,
TOAST, generation-search, and parse-cache allocations. Parse-cache evidence
separates uncompressed logical payload, live compressed payload for the selected
project and whole schema, total relation allocation, and the remaining physical
overhead. A bounded `parse_cache_physical_amplification` warning identifies a
large high-water relation without claiming that every overhead byte is safely
reclaimable. It also reports bounded
largest-table/index rows, estimated live/dead tuples, autovacuum evidence,
stale ready generations, invalid concurrent-index artifacts with exact total
and truncation evidence, and duplicate generation-content potential. Duplicate
ranking consistently considers only ready, current, and superseded generations;
failed/staging work cannot distort the estimate. Duplicate content is
assessment-only: mutation
stays disabled until a normalized content-addressed fact schema can preserve
immutable generation identity, project isolation, cascades, and freshness.

Migration 23 applies table-specific autovacuum/analyze thresholds to the
highest-churn generation and cache relations. It adds `payload_bytes` as a
PostgreSQL 18 virtual generated column, so byte accounting occupies no per-row
storage.

</details>

### Online B-tree compaction

Ordinary `VACUUM` makes deleted space reusable inside PostgreSQL but usually
does not return it to the filesystem. This is especially visible for a
high-churn parse-cache TOAST relation: zero dead tuples can coexist with a large
reusable high-water file. Cartograph therefore offers a dry-run
online B-tree plan for recoverable index bloat:

> [!CAUTION]
> `--apply` rebuilds indexes. Review the dry-run plan and verify free-space
> headroom first.

```sh
cartograph db compact --project-path . --format json

cartograph db compact --project-path . \
  --apply \
  --confirm compact-online-indexes \
  --format json
```

The plan measures each B-tree large enough to matter with `pgstatindex` and
selects only indexes whose estimated reclaim reaches `--minimum-reclaimable-bytes`
(default 64 MiB), largest reclaim first. The required free-space minimum for
apply is twice the largest candidate plus 64 MiB.

<details>
<summary>Details: reclaim estimate, headroom, and apply safety</summary>

The estimate is the leaf pages above the
density a rebuild packs to (the index fill factor, 90 by default) plus empty and
deleted pages, so an index that was just rebuilt is not selected again. Each
candidate reports `estimatedReclaimableBytes`, and the plan reports
`reclaimMeasured`. Measuring reads every index at or above the threshold, bounded
by `--timeout-seconds`. Without the `pgstattuple` extension the plan falls back
to size-only selection and reports `reclaimMeasured: false`. The reclaim threshold
must not exceed `--maximum-candidate-bytes`.

Apply rebuilds one eligible B-tree at a time with `REINDEX INDEX CONCURRENTLY`
outside a transaction, under a schema advisory lock and per-index deadline. It
is bounded by index count and candidate bytes, is resumable after a partial
failure, and never auto-drops `_ccnew`, `_ccold`, invalid, BM25, or exclusion-
constraint artifacts. Managed mode measures free bytes from the validated
project-owned data volume and rejects `--available-headroom-bytes`; external
PostgreSQL requires that operator-supplied value. The required minimum is twice the largest
candidate plus 64 MiB because concurrent rebuilds temporarily need both index
copies and working space. The advisory lock and session timeout live on a
dedicated close-on-drop connection so cancellation cannot contaminate the pool
or strand a session lock.

</details>

### Heap compaction

When heap or TOAST high-water allocation remains after pruning, request the
separate read-only heap plan:

> [!CAUTION]
> Heap apply is offline maintenance: each table takes `ACCESS EXCLUSIVE`.
> Quiesce attached MCP/index/hook writers, take a verified backup, and schedule
> a maintenance window.

```sh
cartograph db compact --heap --project-path . --format json

cartograph db compact --heap --project-path . \
  --apply \
  --confirm compact-heap-relations \
  --format json
```

New managed databases install `pgstattuple`; an external PostgreSQL operator
must install that extension before requesting heap measurement.

<details>
<summary>Details: heap plan estimates and apply behavior</summary>

The plan uses `pgstattuple_approx` on a fixed allowlist of Cartograph-owned main
and TOAST heaps. It reports dead, reusable-free, estimated-rewritten,
estimated-reclaimable, total, headroom, truncation, and
`requiresAccessExclusive` evidence per bounded relation. The rewritten estimate
packs the live tuples at the table fill factor (TOAST at no fewer than four
chunks per page); reclaim is the allocation beyond it, capped by the measured
dead and free bytes. Free space a rewrite cannot remove, such as the unused tail
of a packed page, therefore does not make a freshly rewritten table a candidate
again.

Apply is intentionally offline maintenance, not an online repack. It refuses
live Cartograph operation leases, requires the distinct
`compact-heap-relations` confirmation and verified free-space headroom, then
runs `VACUUM FULL` on one allowlisted table at a time outside a transaction.
A schema-wide maintenance gate rejects new project-operation leases for the
bounded rewrite window, and apply refuses to start while an existing lease is
live. Each table takes `ACCESS EXCLUSIVE`; quiesce attached MCP/index/hook writers,
take a verified backup, and schedule a maintenance window. Managed mode
measures its validated data volume; external PostgreSQL must supply
`--available-headroom-bytes`. Partial completion names the table and stable
stop reason and can be resumed with a fresh plan.

</details>

### Failed-generation spill cleanup

A failed manual `cartograph index` can leave a large spill in place until the
bounded retention drain runs. Heap compaction remains the explicit path for
returning an already-allocated high-water file to the filesystem.

<details>
<summary>Details: when a failed generation's spill is deleted</summary>

Failed generation cleanup deletes its exact spill root and cascaded staging
payload in the same fenced terminal transaction when that cascade fits the
cleanup statement deadline, so PostgreSQL can reuse those pages before another
attempt. A larger spill, such as a full rebuild of a large project, rolls back
only that delete: the generation is still failed and its lease released, and
the spill rows stay with it, like its already-staged canonical rows, until the
bounded retention drain removes them. That drain runs right after a failed
automatic index, after the next successful index, or through `db prune`; a
failed manual `cartograph index` therefore leaves a large spill in place until
one of those runs. Before, the timed-out cascade failed the cleanup too and left
the generation staging with its lease held until the lease expired.

</details>

## Distribution boundary

Native Cartograph archives contain no PostgreSQL, ParadeDB, pgvector, image,
extension binary, SQL dump, or database credential. The managed command pulls
the separately distributed upstream image. See [licensing](v2/LICENSING.md) for
the local Community and hosted/shared deployment boundary.
