# Troubleshooting Cartograph v2

[Documentation home](README.md) · [Project overview](../README.md) ·
[MCP usage](MCP-USAGE.md) · [Storage and operations](STORAGE-BACKENDS.md)

## Fast diagnosis

Start with the exact native executable and project:

```sh
command -v cartograph
cartograph --version
cartograph doctor /absolute/path/to/project
cartograph status /absolute/path/to/project
```

Do not infer runtime health from installed files alone. A real doctor/status or
MCP call is the control evidence.

| Symptom | Go to |
| --- | --- |
| Database capability or extension check fails | [PostgreSQL capability failure](#postgresql-capability-failure) |
| Shell commands work but the agent cannot connect | [Doctor works in a shell but MCP cannot connect](#doctor-works-in-a-shell-but-mcp-cannot-connect) |
| Status reports stale source | [Index is stale](#index-is-stale) |
| A large index reaches a hard bound | [Native generation reaches its capacity bound](#native-generation-reaches-its-capacity-bound) |
| Hybrid retrieval skips semantic search | [Semantic search is skipped](#semantic-search-is-skipped) |
| Doctor warns that an LLM credential is not set | [An LLM credential is missing from doctor's shell](#an-llm-credential-is-missing-from-doctors-shell) |
| BM25 is unhealthy after a crash | [ParadeDB derived index is unhealthy after a crash](#paradedb-derived-index-is-unhealthy-after-a-crash) |
| Install or release checksum verification fails | [Release archive or install checksum fails](#release-archive-or-install-checksum-fails) |

## PostgreSQL capability failure

Cartograph requires PostgreSQL 18.4 or newer within major version 18,
`pg_search` 0.25.11 with the expected preload state/ParadeDB access method/BM25
tokenizer behavior, and pgvector 0.8.4 or newer. Pgvector 0.8.6 is recommended
for external PostgreSQL; the managed ParadeDB 0.25.11 image bundles
`pg_search` 0.25.11 and pgvector 0.8.4.
Upgrade or correct the external service, or use the pinned managed database on
macOS/Linux. There is no SQLite or plain-FTS
fallback.

## Managed database cannot start

- Start a local Docker daemon; remote Docker contexts are rejected.
- Inspect `cartograph db status` and bounded `db logs` output.
- If the default loopback port is occupied, select another consistently with
  command help/environment configuration.
- Do not delete a same-name container/volume unless Cartograph proves its exact
  project ownership. Foreign resources are intentionally refused.
- Do not remove a live lifecycle lock. Verify its recorded owner first.

## Doctor works in a shell but MCP cannot connect

The agent host may have an older absolute command, environment, managed port,
or process. Re-run the project-local installer from the working shell:

```sh
cartograph install --yes --target <codex|claude|cursor> --location local \
  --project-path /absolute/path/to/project \
  --managed-database-port <PORT>
```

Omit `--managed-database-port` when the project uses the default `55432`.
Restart the host. An open session is not assumed to hot-register a replaced MCP
server. CLI success proves the native/database control path, not the old MCP
transport.

Versioned installation registers
`~/.cartograph-cli/current/bin/cartograph`. Run the resumable upgrade sequence
instead of manually stitching the binary, database, index, and registration
steps together:

```sh
cartograph upgrade --apply --project-path /absolute/path/to/project --json
```

Success requires `completed: true`; `applied: false` can simply mean the newest
binary was already installed and the remaining project steps were reconciled.
If `restartRequired` is true, close and reopen the host once. If the report is
blocked, inspect `projectReconciliation`, `registrationRepair`, and the bounded
`nextSteps`; after fixing the named boundary, rerun the same command to resume.
`registrationRepair.changes` names every registration the run touched, with the
old and new executable path; an entry with `outcome: manual` carries the exact
`manualStep` to apply by hand. A registration that launches Cartograph through
a wrapper such as `op run --`, `doppler run --`, `aws-vault exec`, `direnv exec`,
or `/usr/bin/env` is reported as `commandState: wrapped` and is never
re-installed: only its embedded absolute Cartograph path is repinned, so the
wrapper and the `env` block that supply an `apiKeyEnv` credential survive the
upgrade.
An idempotent rerun reports `restartRequired: false` when it changed neither the
binary nor a host pin; that run-local result does not claim that a process left
open across an earlier upgrade has been inspected. A database step with
`state: timed_out` means the 15-minute cold image-pull/readiness budget expired,
not that incompatibility was detected; rerun the same command without invoking
the destructive database replacement path.

If startup says the database schema is newer than the binary, do not retry the
old process. The error reports the running binary version, database schema
version, and maximum supported schema version. Upgrade the native binary,
repair the registration, restart the host, and verify the newly loaded MCP
version. Startup exits nonzero; it never serves against a schema it cannot
interpret.

If MCP startup reports managed-image or HNSW shared-memory incompatibility,
`upgrade --apply` stops before replacement and prints the named fresh-backup and
exact confirmed `db upgrade` commands. Run them, then rerun `upgrade --apply`
before restarting the host. This preflight is
intentionally stricter than read-only `status` or an ordinary relational index:
an MCP process exposes semantic maintenance paths that may need HNSW, so it
refuses an older 64 MiB managed container even when non-vector reads still
work. `doctor` is the readiness authority for that boundary.
If a confirmed database upgrade fails after attempting the extension update,
repeat the same command: the new image is retained for resumable verification
and the old image stays stopped so it cannot load a possibly newer catalog.
An interruption after the old container is renamed but before the candidate is
created is also resumable: repeat the same confirmed command and let Cartograph
validate the stopped rollback slot and continue. Do not rename containers by
hand; ambiguous recovery topology fails closed.

## Index is stale

`status.fresh` is true only when the complete supported-source manifest matches
the current immutable generation. Run a bounded `cartograph index` (or explicit
MCP admin job), then re-check status. Context packets may include a separate
changed-source overlay while stale; they still lower confidence and retain the
stale abstention.

An unchanged no-op is decided by the exact supported-source and index-policy
scan that prepared the request; it does not repeat that full read. A newly
published generation gets one final reconciliation scan. If files race a
publication, Cartograph retries reconciliation twice and then returns
`source_changed_during_index`; it never reports success for a generation that
the final scan already knows is stale. Unsupported editor metadata such as
`.editorconfig` does not enter the source revision.

When automatic sync races ongoing edits or another writer owns the project
lease, `autoSync.lastErrorCode` identifies the interruption and `nextRetryAt`
reports a bounded recovery retry. These interruptions do not permanently
suppress the watcher after five attempts. Leave the live writer running;
automatic sync catches up after the source settles or the lease is released.
Persistent parse/publication failures and generation-capacity limits still
stop repeated failing work and require inspecting the reported error.

Before that no-op decision, index/sync also terminalizes every unleased
`staging` generation for the project under a bounded project lock. A staging
generation protected by a live lease is preserved. This lets an unchanged
retry recover work abandoned by an interrupted client without forcing a full
re-index; normal retention may subsequently remove the failed row.

## Index fails during the parse stage

File-local parse failures name one escaped normalized project-relative input
and a fixed reason such as `source_changed_during_parse`,
`extraction_grammar_unavailable`, `extraction_parser_stopped`,
`extraction_invalid_span`, or `extraction_output_limit_exceeded`. The text form
is concise. For automation, request structured stderr:

```sh
cartograph index /absolute/path/to/project --format json
```

The nonzero result contains `error.code`, `error.stage`, and an
`error.file_failure` object with `path`, `reason`, and a credential-safe
`description`. The path is relative to the project; absolute roots, source
text, literals, database URLs, and raw parser/driver messages remain omitted.
MCP admin status exposes the same evidence as `fileFailure`.

`source_changed_during_parse` means the named file no longer matches the exact
manifest entry read at the start of that attempt. Let rapid writes settle and
retry. A stable repeated reason points to the named source/extractor boundary;
fix or deliberately ignore that input, then rerun the ordinary index. The prior
generation remains queryable throughout.

## A deeply nested file degrades extraction

Grammar-backed extraction defaults to `maxAstDepth: 256`. Exceeding the bound
is recoverable: the report names up to 32 exact normalized degraded paths,
reports how many additional paths were truncated, retains each affected file as
partial, and continues the rest of the generation. Configure a value from 64
through 1024 only when authored source legitimately needs it:

```json
{
  "maxAstDepth": 512
}
```

Prefer ignore rules or project `exclude` globs for generated/build output. A
larger global bound should not be used to hide an unexpected generated-source
tree.

## Source excerpt is omitted

`cartograph node/show` returns source only when the complete live manifest still
matches the generation owning the symbol's line range. On stale or racing
source, metadata remains but the excerpt is omitted rather than slicing the
wrong bytes. Re-index and retry.

## Native generation reaches its capacity bound

Inspect the exact stage/reason and the returned native metrics. A
`generation_capacity_exceeded` result is a real admission boundary, not a
database-health diagnosis. It is bounded by the Cartograph process's
`maxGenerationBytes` policy and is not the managed PostgreSQL container's 2 GiB
memory ceiling. Direct JSON and MCP admin failures name that limit, its
`cartograph_process` scope, and the next action. With the default
`generationStorage: "auto"`, large source manifests and workspaces with at least
64 Cargo manifests select PostgreSQL spill automatically. For a dense smaller
manifest, force it in `.cartograph/config.json`:

```json
{
  "generationStorage": "postgres",
  "maxSpillBytes": 137438953472,
  "maxSpillRows": 1000000000
}
```

Before raising quotas, verify PostgreSQL data/WAL/temporary-disk headroom;
logical spill bytes are not physical storage estimates. A spill-specific byte
or row limit leaves the current generation visible and the failed staging work
eligible for bounded cleanup. Lease loss, cancellation, and a byte-different
retry also fail closed. Exact retained retries reuse immutable batches and the
durable canonical partition cursor.

PostgreSQL spill does not make every native structure unlimited. Resolution
lookups, clone profiles, and the centrality graph retain a separate compact
bound based on `maxGenerationBytes`. The accepted maximum is 8 GiB
(`8589934592` bytes); it cannot be raised beyond that process-safety ceiling.
When the error occurs at the maximum, exclude generated metadata, compiled
artifacts, or other machine-produced paths with `index --exclude`, project
`exclude`, or `.cartographignore`, then run an explicit index. Invalid values
now name `maxGenerationBytes` and its exact inclusive range instead of making
status/index fail with an opaque options message. SCIP overlays support both
storage strategies and no longer force `auto` into memory. Their covered-source
basis and imported facts remain bounded by native working limits; reduce an
oversized overlay or source admission policy when that independent bound fails.

For a measured example with parser, resolver, publication, memory, row-count,
and no-op timings kept separate, see the published
[large public corpus streaming benchmark record](v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md).

Automatic retention can report `reason: "project_busy"` after a successful
index when another live writer owns the migration lease. That report is
historical and retryable; it does not make the index unsuccessful. `admin
unlock` removes database-clock-expired leases only and cannot clear a live
`project_busy` outcome. Wait for the named writer to finish, then retry index or
a bounded prune.

## Native stage reports `progress_stalled`

The supervisor cancels an operation when its active stage produces no durable
work inside the configured progress watchdog. Direct CLI, MCP admin, and
auto-sync output retain a qualified privacy-safe reason such as
`parse_progress_stalled`, `resolve_progress_stalled`, or
`relational_merge_progress_stalled`; source paths, SQL, database URLs, and
driver text are not included. This differs from `*_deadline_exceeded`: a
deadline is an item or whole-stage execution horizon, while a progress stall
means the watchdog observed no completed work checkpoint.

Inspect the named stage, bounded database logs, host memory/CPU, and PostgreSQL
I/O or lock pressure. Retry only after identifying transient resource pressure
or a fixed defect. The prior generation remains visible, and a failed staging
generation is handled by normal bounded cleanup.

For an MCP admin index job, polling the job now returns live supervisor
progress while it runs: stage, completed items/bytes, heartbeat count, idle
time, completed stage timings, total elapsed time, and cancellation state. A
busy host is therefore distinguishable from a stalled stage without exposing
source or database text.

## Semantic search is skipped

Hybrid mode requires a reachable OpenAI-compatible embedding endpoint and a
model registration whose fingerprint, dimension, current-generation coverage,
HNSW index, and query probe all pass. The packet reports `not_configured`,
`not_indexed`, `stale`, or `unavailable` and falls back explicitly to lexical
evidence. It never labels BM25-only results as hybrid.

An embedding sweep reports the complete `corpusDocuments` alongside
`reusedDocuments` and `endpointDocuments`. Unchanged current-generation
documents reuse matching content-addressed vectors before endpoint work; only
documents whose rendered embedding input changed are submitted. The legacy
`documents` counter remains the endpoint-work count for compatibility.

## An LLM credential is missing from doctor's shell

`doctor` reads the project configuration in its own shell, but the MCP server
reads a tier's `apiKeyEnv` variable from its own environment. A variable that is
unset in doctor's shell is reported as `llm-<tier>-credential`: a warning for
the optional tiers (decision/Jev, summarize, local, ask, classify, reranker),
which never makes doctor or onboarding unready, and a failure only for the
required embedding tier. The message names the configured variable. An invalid
tier still fails, for example `llm-decision-config` for an unexpected Jev
model, endpoint or timeout.

To supply the key to the server without a plaintext secret in a host
configuration and without wrapping `cartograph serve` in a secret-manager
launcher, configure a [credential command](CONFIGURATION.md#credential-sources):

```sh
cartograph llm setup . --preset jev \
  --api-key-command /path/to/secret-helper --api-key-arg get --api-key-arg typesafe-api-key
```

`doctor` and `llm smoke` then run that command, bounded and without a shell,
and report only whether it produced a credential, naming the program and exit
status on failure. The server runs the same command on the tier's first use. If
it fails there, Jev reports `providerError: "credential_unavailable"` and
explore keeps native retrieval. The server tries the command again 30 seconds
later, without a host restart.

## ParadeDB derived index is unhealthy after a crash

Treat relational graph and search-document rows as source of truth. Inspect:

```sh
cartograph db derived-index --project-path .
```

Use the exact confirmed rebuild form from help. Community BM25 is rebuildable
local derived state; do not claim it is WAL-crash-durable or use a rebuild to
hide relational data loss.

## V1 import fails

- The source must be a v1.1.33 PostgreSQL schema in the same database as a
  distinct v2 destination schema.
- The destination may already have a current generation, but project
  index/sync/hook/rebuild writers should be quiesced during the import.
- Run `--dry-run` first against the exact checkout represented by v1.
- Unsupported languages, mismatched bytes/hashes, invalid required symbol
  coordinates, orphan relations, oversized JSON, malformed required data, incomplete schema
  history, or an inconsistent checkpoint fail closed. Malformed optional JSON
  evidence is treated as unavailable rather than imported.
- Repeat the identical confirmed command only when the error says the durable
  run is resumable.
- `ConcurrentPublication` means another writer won publication. Quiesce those
  writers and repeat the identical confirmed command; Cartograph has already
  failed/released the stale generation and will reserve a newer one.
- If v1 exists only in SQLite, rebuild from source or use v1.1.33 to migrate it
  to PostgreSQL first. V2 never opens the SQLite file.

See [PostgreSQL operations](STORAGE-BACKENDS.md) for the exact sequence.

## Generation prune fails or rolls back

Prune requires the exact `prune-old-generations` confirmation and a live
project-wide migration lease. Publication/retention locks and a final
PostgreSQL-clock fence check intentionally roll the active transaction back if
ownership expires or changes. Earlier committed batches remain durable. Inspect
`batches_committed`, `retiring_remaining`, and `deferred_reason`, then retry the
same bounded command after inspecting current operations; never bypass the fence.

`search_relation_byte_budget` means eligible search relations exceed the
remaining byte budget. Inspect `db usage` and the backup before using an audited
`--maximum-search-relation-bytes` override (hard maximum 64 GiB). A large old
relation does not prevent cleanup of later relations within budget.

Retention drains each relation of a generation in key order and resumes after
the last deleted key, so a large failed generation is deleted in one linear
index walk instead of re-scanning the rows earlier batches removed. The final
generation row runs every cascading foreign-key check; on a heavily bloated
schema those checks can exceed their short bound, which is also clamped to the
time left in the drain transaction. That row is then left `retiring` without
rolling back the rows already drained, the report says
`deferred_reason: "parent_delete_deferred"`, and a later prune or autovacuum
lets it finish. An explicit prune vacuums the fact tables only when it removed
enough rows and runs with immediate maintenance; automatic retention delegates
to autovacuum. A repeated `batch-deadline` on a very large, bloated schema
usually means dead tuples from a previous invocation are unvacuumed: `VACUUM`
the fact tables, rerun the prune, then compact indexes online.

Automatic indexing drains failed and retiring generations before it reserves a
new one. While that bounded cleanup is still removing generations but more than
one remains, the attempt is deferred with `retention_backlog`, the previous
generation stays visible, and the watcher retries after 2–30 seconds, so
repeated automatic failures cannot outpace cleanup. When cleanup makes no
progress (another operation holds the project, a search-relation budget or
catalog check blocks it), the attempt proceeds rather than freezing automatic
indexing; inspect `retentionMaintenance` in `db usage` and run a bounded prune.

Automatic indexing can report `cache_only` when generation cleanup failed but
parse-cache eviction committed. `db usage` exposes the persisted latest phase
outcomes and consecutive failure count through `retentionMaintenance`, including
maintenance attempted after failed indexing. Empty spill heaps with large B-tree
files are a separate allocation problem: inspect the online compaction plan.
Unattributed database bytes require catalog and filesystem investigation; an
extension upgrade does not prove historical orphaned files were reclaimed.

## Git review is unavailable

Review requires a Git worktree and a valid non-option revision. Git execution is
shell-free, output-bounded, deadline-bounded, and noninteractive. Confirm the
ref exists locally and the project root is a repository. A missing/invalid ref,
unavailable Git, output limit, and timeout are distinct redacted failures.

## Release archive or install checksum fails

Do not bypass a mismatch. Download `SHA256SUMS` and the archive from the same
immutable release, verify the tag/version/asset name, and retry the download.
Release archives should contain only the native binary and allowlisted notices/
documentation—never PostgreSQL, ParadeDB, pgvector, SQLite, or credentials.
