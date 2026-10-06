# Troubleshooting Cartograph v2

[Documentation home](README.md) · [Project overview](../README.md) ·
[MCP usage](MCP-USAGE.md) · [Storage and operations](STORAGE-BACKENDS.md)

Use this page when a Cartograph command, MCP host, or index reports a failure.
Find the symptom or the stable error code in the tables below. Each entry says
what the failure means and what to do, and keeps the full semantics in a
collapsible block.

**On this page:** [Fast diagnosis](#fast-diagnosis) ·
[By symptom](#by-symptom) · [By error code](#by-error-code)

## Fast diagnosis

Start with the exact native executable and project:

```sh
command -v cartograph
cartograph --version
cartograph doctor /absolute/path/to/project
cartograph status /absolute/path/to/project
```

> [!IMPORTANT]
> Do not infer runtime health from installed files alone. A real doctor/status
> or MCP call is the control evidence.

### By symptom

| Symptom | Go to |
| --- | --- |
| Database capability or extension check fails | [PostgreSQL capability failure](#postgresql-capability-failure) |
| `db start` fails, or Docker, port, ownership, or lifecycle-lock checks refuse it | [Managed database cannot start](#managed-database-cannot-start) |
| Shell commands work but the agent cannot connect | [Doctor works in a shell but MCP cannot connect](#doctor-works-in-a-shell-but-mcp-cannot-connect) |
| `upgrade --apply` is blocked, times out, or asks for a restart | [Run the resumable upgrade](#run-the-resumable-upgrade) |
| `projectReconciliation.index` is not `ready` | [Read `projectReconciliation.index`](#read-projectreconciliationindex) |
| `cartograph index` was interrupted with SIGINT or SIGTERM | [Interrupted index (`request_cancelled`)](#interrupted-index-request_cancelled) |
| Startup says the database schema is newer than the binary | [Database schema is newer than the binary](#database-schema-is-newer-than-the-binary) |
| MCP startup reports managed-image or HNSW shared-memory incompatibility | [Managed image or HNSW shared memory is incompatible](#managed-image-or-hnsw-shared-memory-is-incompatible) |
| Status reports stale source | [Index is stale](#index-is-stale) |
| Index reports `lease_busy` or `index_cleanup_failed` | [Index reports `lease_busy` or `index_cleanup_failed`](#index-reports-lease_busy-or-index_cleanup_failed) |
| `db start`, `index`, or `upgrade --apply` reports `schema_busy` | [Schema migration reports `schema_busy`](#schema-migration-reports-schema_busy) |
| SCIP import reports `overlayRollbackFailure` | [SCIP import reports `overlayRollbackFailure`](#scip-import-reports-overlayrollbackfailure) |
| Index fails in the parse stage and names one file | [Index fails during the parse stage](#index-fails-during-the-parse-stage) |
| Extraction reports degraded paths for deeply nested source | [A deeply nested file degrades extraction](#a-deeply-nested-file-degrades-extraction) |
| `node`/`show` returns metadata without a source excerpt | [Source excerpt is omitted](#source-excerpt-is-omitted) |
| A large index reaches a hard bound | [Native generation reaches its capacity bound](#native-generation-reaches-its-capacity-bound) |
| Retention reports `project_busy` after a successful index | [Retention reports `project_busy` after an index](#retention-reports-project_busy-after-an-index) |
| A stage reports `*_progress_stalled` or `*_deadline_exceeded` | [Native stage reports `progress_stalled`](#native-stage-reports-progress_stalled) |
| Hybrid retrieval skips semantic search | [Semantic search is skipped](#semantic-search-is-skipped) |
| Doctor warns that an LLM credential is not set | [An LLM credential is missing from doctor's shell](#an-llm-credential-is-missing-from-doctors-shell) |
| BM25 is unhealthy after a crash | [ParadeDB derived index is unhealthy after a crash](#paradedb-derived-index-is-unhealthy-after-a-crash) |
| `db import-v1` fails | [V1 import fails](#v1-import-fails) |
| `db prune` or automatic retention fails, defers, or rolls back | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |
| `review` cannot run Git | [Git review is unavailable](#git-review-is-unavailable) |
| Install or release checksum verification fails | [Release archive or install checksum fails](#release-archive-or-install-checksum-fails) |

### By error code

| Code or state | Where it appears | Go to |
| --- | --- | --- |
| `lease_busy` | `index`, `sync-if-dirty`, MCP admin jobs, `autoSync.lastErrorCode` | [Index reports `lease_busy` or `index_cleanup_failed`](#index-reports-lease_busy-or-index_cleanup_failed) |
| `lease_failed` | `index` | [Index reports `lease_busy` or `index_cleanup_failed`](#index-reports-lease_busy-or-index_cleanup_failed) |
| `index_cleanup_failed`, `cleanup_failure`, `cleanupFailure`, `lastCleanupFailureCode` | `index --format json`, MCP admin job status, `autoSync` | [Index reports `lease_busy` or `index_cleanup_failed`](#index-reports-lease_busy-or-index_cleanup_failed) |
| `previous_generation_visible` | `index --format json` failures | [Index reports `lease_busy` or `index_cleanup_failed`](#index-reports-lease_busy-or-index_cleanup_failed) |
| `request_cancelled` | `index` after SIGINT/SIGTERM; the `upgrade --apply` index child | [Interrupted index (`request_cancelled`)](#interrupted-index-request_cancelled) |
| `schema_busy` | `db start` (exit status 75), `index`, `upgrade --apply`, MCP tools, `db import-v1` | [Schema migration reports `schema_busy`](#schema-migration-reports-schema_busy) |
| `schema_version_ahead` | Startup against a schema newer than the binary | [Database schema is newer than the binary](#database-schema-is-newer-than-the-binary) |
| `schema_migration_blocked` | An older schema whose next migration could not be applied | [Database schema is newer than the binary](#database-schema-is-newer-than-the-binary) |
| `another_writer_active` | `upgrade --apply` database step or `projectReconciliation.index` | [Run the resumable upgrade](#run-the-resumable-upgrade), [Read `projectReconciliation.index`](#read-projectreconciliationindex) |
| `timed_out` | `upgrade --apply` database, index, doctor, or verification step | [Run the resumable upgrade](#run-the-resumable-upgrade), [Read `projectReconciliation.index`](#read-projectreconciliationindex), [Doctor and verification step states](#doctor-and-verification-step-states) |
| `source_changed`, `blocked`, `not_run` | `upgrade --apply` reconciliation steps | [Read `projectReconciliation.index`](#read-projectreconciliationindex), [Doctor and verification step states](#doctor-and-verification-step-states) |
| `restartRequired`, `commandState: wrapped`, `outcome: manual` | `upgrade --apply` report | [Run the resumable upgrade](#run-the-resumable-upgrade) |
| `source_changed_during_index` | `index`, sync | [Index is stale](#index-is-stale) |
| `source_changed_during_parse`, `extraction_*` reasons | `error.file_failure.reason`, MCP `fileFailure` | [Index fails during the parse stage](#index-fails-during-the-parse-stage) |
| `parse_source_changed` and other `parse_*` codes | `index --format json` `error.code` | [Index fails during the parse stage](#index-fails-during-the-parse-stage) |
| `parse_generation_capacity_exceeded`, `resolve_generation_capacity_exceeded`, `reduce_generation_capacity_exceeded` | `index --format json` `error.code` | [Native generation reaches its capacity bound](#native-generation-reaches-its-capacity-bound) |
| `generation_capacity_exceeded` | MCP admin job `failureDetail.reason` | [Native generation reaches its capacity bound](#native-generation-reaches-its-capacity-bound) |
| `project_busy` | Automatic retention report after a successful index | [Retention reports `project_busy` after an index](#retention-reports-project_busy-after-an-index) |
| `*_progress_stalled` (`parse_progress_stalled`, `resolve_progress_stalled`, `relational_merge_progress_stalled`) | CLI, MCP admin, auto-sync | [Native stage reports `progress_stalled`](#native-stage-reports-progress_stalled) |
| `*_deadline_exceeded` (for example `reduce_deadline_exceeded`) | CLI, MCP admin, auto-sync | [Native stage reports `progress_stalled`](#native-stage-reports-progress_stalled) |
| `overlayRollbackFailure` (`scip_overlay_rollback_failed`) | MCP admin job status for `scip-import` | [SCIP import reports `overlayRollbackFailure`](#scip-import-reports-overlayrollbackfailure) |
| `not_configured`, `not_indexed`, `stale`, `unavailable` | Semantic part of a hybrid packet | [Semantic search is skipped](#semantic-search-is-skipped) |
| `llm-<tier>-credential`, `llm-decision-config` | `doctor` checks | [An LLM credential is missing from doctor's shell](#an-llm-credential-is-missing-from-doctors-shell) |
| `credential_unavailable` | Jev `providerError` | [An LLM credential is missing from doctor's shell](#an-llm-credential-is-missing-from-doctors-shell) |
| "another Cartograph writer published during v1 import; retry after it is idle" | `db import-v1` | [V1 import fails](#v1-import-fails) |
| `search_relation_byte_budget` | Prune `deferred_reason` | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |
| `parent_delete_deferred` | Prune and retention `deferred_reason` | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |
| `batch-deadline` | Prune `deferred_reason` | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |
| `retention_backlog` | Automatic indexing | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |
| `cache_only` | Automatic indexing retention outcome | [Generation prune fails or rolls back](#generation-prune-fails-or-rolls-back) |

## PostgreSQL capability failure

**What it means:** `doctor` could not prove the PostgreSQL version, extension
versions, or ParadeDB behavior Cartograph requires. There is no SQLite or
plain-FTS fallback.

**What to do:**

1. Compare the database with the requirements below.
2. Upgrade or correct the external service, or use the pinned managed database
   on macOS/Linux.
3. Rerun `cartograph doctor /absolute/path/to/project`.

| Component | Requirement |
| --- | --- |
| PostgreSQL | 18.4 or newer within major version 18 |
| `pg_search` | 0.26.0 with the expected preload state/ParadeDB access method/BM25 tokenizer behavior |
| pgvector | 0.8.4 or newer; pgvector 0.8.7 is recommended for external PostgreSQL |

The managed ParadeDB 0.26.0 image bundles `pg_search` 0.26.0 and pgvector 0.8.6.

## Managed database cannot start

**What it means:** The managed lifecycle refused to start, or could not prove
that it owns the container, volume, port, or lock it needs.

**What to do:**

1. Start a local Docker daemon; remote Docker contexts are rejected.
2. Inspect `cartograph db status` and bounded `db logs` output:

   ```sh
   cartograph db status --project-path .
   cartograph db logs --project-path . --tail 200
   ```

3. If the default loopback port is occupied, select another consistently with
   command help/environment configuration.
4. If `db start` exits with status 75, the schema migration reported
   `schema_busy`; see
   [Schema migration reports `schema_busy`](#schema-migration-reports-schema_busy).

> [!WARNING]
> Do not delete a same-name container/volume unless Cartograph proves its exact
> project ownership. Foreign resources are intentionally refused. Do not remove
> a live lifecycle lock. Verify its recorded owner first.

## Doctor works in a shell but MCP cannot connect

**What it means:** The agent host may have an older absolute command,
environment, managed port, or process. CLI success proves the native/database
control path, not the old MCP transport.

**What to do:**

1. [Re-register the host](#re-register-the-host) from the working shell and
   restart it.
2. On a versioned installation, [run the resumable upgrade](#run-the-resumable-upgrade)
   instead of stitching steps together by hand.
3. If the upgrade report is blocked, read it with the subsections below.

### Re-register the host

Re-run the project-local installer from the working shell:

```sh
cartograph install --yes --target <codex|claude|cursor> --location local \
  --project-path /absolute/path/to/project \
  --managed-database-port <PORT>
```

Omit `--managed-database-port` when the project uses the default `55432`.
Restart the host. An open session is not assumed to hot-register a replaced MCP
server.

### Run the resumable upgrade

Versioned installation registers
`~/.cartograph-cli/current/bin/cartograph`. Run the resumable upgrade sequence
instead of manually stitching the binary, database, index, and registration
steps together:

```sh
cartograph upgrade --apply --project-path /absolute/path/to/project --json
```

| Report field | Meaning | What to do |
| --- | --- | --- |
| `completed: true` | Success requires this. | Nothing. |
| `applied: false` | Can simply mean the newest binary was already installed and the remaining project steps were reconciled. | Check `completed`. |
| `restartRequired: true` | The run changed the binary or a host pin. | Close and reopen the host once. |
| `restartRequired: false` | An idempotent rerun changed neither the binary nor a host pin; that run-local result does not claim that a process left open across an earlier upgrade has been inspected. | Nothing for this run. |
| Blocked report | A named boundary stopped the run. | Inspect `projectReconciliation`, `registrationRepair`, and the bounded `nextSteps`; after fixing the named boundary, rerun the same command to resume. |
| `registrationRepair.changes` | Names every registration the run touched, with the old and new executable path. | Review it. |
| `outcome: manual` | The entry carries the exact `manualStep`. | Apply that step by hand. |
| `commandState: wrapped` | The registration launches Cartograph through a wrapper and is never re-installed. | Nothing; see below. |

A registration that launches Cartograph through a wrapper such as `op run --`,
`doppler run --`, `aws-vault exec`, `direnv exec`, or `/usr/bin/env` is reported
as `commandState: wrapped` and is never re-installed: only its embedded absolute
Cartograph path is repinned, so the wrapper and the `env` block that supply an
`apiKeyEnv` credential survive the upgrade.

Database step states:

| Database step | Meaning | What to do |
| --- | --- | --- |
| `state: timed_out` | The 15-minute cold image-pull/readiness budget expired, not that incompatibility was detected. | Rerun the same command without invoking the destructive database replacement path. |
| `state: another_writer_active` with `reason: schema_busy` (`retryable: true`) | Another Cartograph process held PostgreSQL locks that the pending schema migration needs for its whole 5-minute contention budget. | See [Schema migration reports `schema_busy`](#schema-migration-reports-schema_busy). |

### Read `projectReconciliation.index`

On a large or continuously edited project, read `projectReconciliation.index`:

| State | Meaning | What to do |
| --- | --- | --- |
| `source_changed` | The installed binary published a complete generation once, then saw the checkout change because another session kept editing it; it reports that instead of rebuilding. The index is not fresh. | Run `cartograph index <path>` once edits pause, or let MCP auto-sync reconcile it. |
| `another_writer_active` (`retryable: true`) | Another Cartograph operation, usually an MCP server's auto-sync in another session (or a schema maintenance step), kept the project busy for the whole 30-minute wait and this run published nothing. | Rerun the same upgrade command after it finishes; pause edits in the other session (or stop its MCP server) before rerunning. |
| `another_writer_active` with `reason: schema_busy` instead of `lease_busy` | The index child itself had to apply a schema migration (an external database) and another process's locks kept it from applying. | See [Schema migration reports `schema_busy`](#schema-migration-reports-schema_busy). |
| `timed_out` (`retryable: true`, `reason: no_progress` or `ceiling`) | The index reported no progress for 15 minutes (its database connection and schema migration count as progress for their first 30 minutes), or reached the 210-minute ceiling. | Rerun. If the timeout repeats, run `cartograph index <path>` directly to see the stage that is not advancing. |
| `blocked` with a `reason` | The index failed with that stable code. | Run `cartograph index <path> --format json` for the full failure. |
| `not_run` | The database step did not finish as `ready`, so the index never started. | Read `projectReconciliation.database`. |

<details>
<summary>Details: how <code>source_changed</code>, <code>another_writer_active</code>, and <code>timed_out</code> complete</summary>

- **`source_changed`:** The upgrade then completes as
  `projectReconciliation.state: source_changed` only if `doctor` and the
  next-process status also pass (a status that finds the checkout fresh by then
  reports `ready` instead), so confirm with `projectReconciliation.state` and
  the top-level `completed`.
- **`another_writer_active`:** A writer that starts during the index's source
  scan is awaited too (the index's attempt reports `lease_busy` and is
  retried), but each such collision repeats the scan, so an auto-sync that
  re-syncs continuously can use up the whole wait.
- **`timed_out`:** Its stdin was closed so that it stopped cooperatively. The
  message says whether it confirmed releasing its lease (`request_cancelled`
  without a `cleanup_failure`), exited without confirming it, or was killed
  after 4 minutes; in the last two cases the lease can remain for up to its
  5-minute TTL. A rerun waits for that instead of failing with `lease_busy`.

</details>

### Doctor and verification step states

| Step state | Meaning | What to do |
| --- | --- | --- |
| `doctor` or `verification` with `state: timed_out` | That rescan of the checkout exceeded its 10-minute budget; it is retryable and says nothing about the project's health. | Rerun. |
| `blocked` verification | Also retryable when another writer replaced the generation this upgrade published or confirmed. | Rerun after that writer finishes. |
| `not_run` on `doctor` or `verification` | An earlier step stopped the reconciliation. | Read the earlier step. |

### Interrupted index (`request_cancelled`)

Once its index request has started, `cartograph index` stops cooperatively on
SIGINT or SIGTERM and releases its lease before exiting with
`request_cancelled`; a second interrupt ends it at once by that signal and
leaves the lease to expire.

<details>
<summary>Details: interrupts before a lease, after publication, and cleanup evidence</summary>

- An interrupt while it still connects or migrates the schema ends it at once
  as well, before it holds a lease.
- An interrupt that arrives after the index already published leaves that
  generation current and can still end in success when the post-publication
  recheck had already matched the checkout.
- With `--format json`, a `request_cancelled` failure without `cleanup_failure`
  means PostgreSQL confirmed this index's own cleanup; another session's lease
  or staging generation on the same project does not add a `cleanup_failure`.

</details>

### Database schema is newer than the binary

**What it means:** Startup found a database schema newer than the running
binary can interpret. The error reports the running binary version, database
schema version, and maximum supported schema version. Startup exits nonzero; it
never serves against a schema it cannot interpret. MCP structured failure
reports use the stable reason `schema_version_ahead`.

**What to do:** Do not retry the old process.

1. Upgrade the native binary.
2. Repair the registration.
3. Restart the host.
4. Verify the newly loaded MCP version.

> [!NOTE]
> The opposite case, an older schema whose next append-only migration could not
> be applied for a reason other than lock contention, reports
> `schema_migration_blocked`. Its message names the recorded schema version,
> the required version, and the pending migration; see
> [Schema migrations](STORAGE-BACKENDS.md#schema-migrations). Lock contention
> is reported as [`schema_busy`](#schema-migration-reports-schema_busy)
> instead.

### Managed image or HNSW shared memory is incompatible

**What it means:** If MCP startup reports managed-image or HNSW shared-memory
incompatibility, `upgrade --apply` stops before replacement and prints the named
fresh-backup and exact confirmed `db upgrade` commands. `doctor` is the
readiness authority for that boundary.

**What to do:**

1. Run the printed commands. They take this form:

   ```sh
   cartograph db backup ./cartograph-pre-upgrade.backup --project-path <path> --port <PORT>
   cartograph db upgrade --project-path <path> --port <PORT> --confirm upgrade-managed-database
   ```

2. Rerun `upgrade --apply` before restarting the host.
3. If a confirmed database upgrade fails after attempting the extension update,
   or was interrupted, repeat the same confirmed command.

> [!CAUTION]
> `db upgrade` replaces the managed container. Do not rename containers by
> hand; ambiguous recovery topology fails closed.

<details>
<summary>Details: why MCP is stricter than status, and how a failed upgrade resumes</summary>

This preflight is intentionally stricter than read-only `status` or an ordinary
relational index: an MCP process exposes semantic maintenance paths that may
need HNSW, so it refuses an older 64 MiB managed container even when non-vector
reads still work.

If a confirmed database upgrade fails after attempting the extension update,
repeat the same command: the new image is retained for resumable verification
and the old image stays stopped so it cannot load a possibly newer catalog.
An interruption after the old container is renamed but before the candidate is
created is also resumable: repeat the same confirmed command and let Cartograph
validate the stopped rollback slot and continue.

</details>

## Index is stale

**What it means:** `status.fresh` is true only when the complete
supported-source manifest matches the current immutable generation. Context
packets may include a separate changed-source overlay while stale; they still
lower confidence and retain the stale abstention.

**What to do:**

1. Run a bounded `cartograph index` (or explicit MCP admin job), then re-check
   status:

   ```sh
   cartograph index /absolute/path/to/project
   cartograph status /absolute/path/to/project
   ```

2. If automatic sync races ongoing edits or another writer owns the project
   lease, leave the live writer running; automatic sync catches up after the
   source settles or the lease is released.
3. If `source_changed_during_index` repeats, let edits settle and retry.

<details>
<summary>Details: no-op decisions, publication races, auto-sync retries, and staging recovery</summary>

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
suppress the watcher after five attempts. Persistent parse/publication failures
and generation-capacity limits still stop repeated failing work and require
inspecting the reported error.

Before that no-op decision, index/sync also terminalizes every unleased
`staging` generation for the project under a bounded project lock. A staging
generation protected by a live lease is preserved. This lets an unchanged
retry recover work abandoned by an interrupted client without forcing a full
re-index; normal retention may subsequently remove the failed row. While
another operation holds a live project lease, or still holds the project lock
after the bounded five-second wait, or a lock keeps the read of the project's
leases waiting past that bound, that recovery is deferred to a later attempt:
an unchanged checkout still returns its no-op, and a changed checkout returns
`lease_busy` without reserving a generation.

</details>

## Index reports `lease_busy` or `index_cleanup_failed`

**What it means:** `lease_busy` is retryable contention: another writer owns
the project. `index_cleanup_failed` means the bounded cleanup of staging state
failed. The current generation stays published; index failures never
unpublish the current generation.

**What to do:**

1. For `lease_busy`, wait for the writer to finish and retry.
2. For a `cleanup_failure` beside another failure, retry normally: the next
   index retries that cleanup.
3. For `code: index_cleanup_failed` on its own, inspect PostgreSQL health and
   generation retention before retrying.
4. For `lease_failed` or `Cartograph project status is unavailable`, check
   PostgreSQL health.

> [!WARNING]
> `admin unlock` removes only database-clock-expired leases and cannot clear a
> live writer.

| Code or field | Meaning |
| --- | --- |
| `lease_busy` | Another operation owns a live project lease, another writer still holds the project lock, or a lock keeps the read of the project's leases waiting past its bounded five-second wait. |
| `cleanup_failure: index_cleanup_failed` (`index --format json`), `cleanupFailure` (MCP admin job status), `lastCleanupFailureCode` (`autoSync`) | The attempt failed and the bounded cleanup of its own staging generation also failed afterward. The first failure is kept as `code`, `failure`, or `lastErrorCode`. |
| `code: index_cleanup_failed` on its own | The pre-reservation staging recovery failed for a reason other than contention. |
| `lease_failed` | The attempt could not establish, keep, or confirm ownership of its own lease. |
| `Cartograph project status is unavailable` | A read of the project's leases failed before reservation for a reason other than that bounded wait. |
| `previous_generation_visible` | What PostgreSQL shows after the failure: `true` when a published generation is still current, `false` when the project has none yet, and `null` when that bounded lookup failed. |

<details>
<summary>Details: when contention reserves a generation, and how cleanup failures are reported</summary>

`lease_busy` is retryable contention. Another operation owns a live project
lease, or another writer (for example, an MCP server's automatic sync inside
its long prepare/COPY transaction) still holds the project lock, or a lock (for
example, a concurrent schema change) keeps the read of the project's leases
waiting past its bounded five-second wait. Contention seen before reservation
reserves no generation. A writer that wins after that check is still rejected
at lease acquisition; the attempt's reserved generation is failed at once only
if the project lock frees within five seconds. Otherwise the failure also
carries `cleanup_failure`, and that generation stays `staging` (never current)
until the next index that finds the project free fails it in its staging
recovery. The current generation stays published. `sync-if-dirty` already
waits, up to five minutes in total, for every live lease on the project (index,
sync, hook, migration, or rebuild) before it retries, pauses first when no live
lease explains the collision, and reports `lease_busy` if a writer outlasts
that wait; automatic sync schedules the retry.

When an attempt fails and the bounded cleanup of its own staging generation
also fails afterward (for example, because its own interrupted transaction
still holds the project lock), `index --format json` keeps the first failure as
`code` and reports the cleanup as `cleanup_failure` with
`index_cleanup_failed`; MCP admin job status reports it as `cleanupFailure`
beside its unchanged `failure`, and `autoSync` as `lastCleanupFailureCode`
beside its unchanged `lastErrorCode`. A cancelled index (`request_cancelled`)
reports `cleanup_failure` unless PostgreSQL confirms that no generation it
reserved is still `staging` or `ready` and that its lease names none of them.
The next index retries that cleanup.

`lease_failed` covers an acquisition that failed for a reason other than
contention, or a lost or unconfirmed heartbeat. A read of the project's leases
that fails before reservation for a reason other than that bounded wait is a
project-status failure (`Cartograph project status is unavailable`), never
`lease_failed`: the attempt held no lease to lose.

</details>

## Schema migration reports `schema_busy`

**What it means:** `schema_busy` is retryable contention, not an incompatible
or damaged database. A pending append-only migration needs PostgreSQL locks
that another session holds, and nothing was applied.

**What to do:**

1. Restart or stop the other process (for example, close the agent host that
   runs the older MCP server, or let the other migration finish).
2. Rerun the same command. A rerun without stopping it can also succeed once
   that process is idle.

> [!IMPORTANT]
> This error never calls for `doctor --fix` or the confirmed managed database
> upgrade.

| Command | How it reports `schema_busy` |
| --- | --- |
| `cartograph db start` | Prints the message with `(reason: schema_busy)` and exits with status 75. Every other `db start` failure exits with status 1. |
| `cartograph index` | `--format json` reports `code: schema_busy`, and text output ends with `(reason: schema_busy)`. |
| `cartograph upgrade --apply` | Reports the database step (managed database) or the index step (external database) as `another_writer_active` with `reason: schema_busy` and `retryable: true`. |
| MCP tools, including `admin migrate` | Return `unavailable` with the same next step. |
| `db import-v1` | The destination migration reports that nothing was imported. |

<details>
<summary>Details: who holds the locks and how long a migration waits</summary>

Usually the session holding the locks belongs to a long-running MCP server,
often one still running an older binary in another agent session, whose
automatic sync or status transactions keep reading the table the migration
alters. It can also be another process that is applying the same migration.

Each migration attempt waits at most two seconds for any one lock (less when
the connection's statement timeout is under four seconds), so a queued schema
change never stalls the other process's new readers for longer. A contended
attempt rolls back whole, pauses for one second, and tries again, for up to
five minutes in total. If the locks are still held after that, nothing was
applied and the command reports `schema_busy`.

</details>

## SCIP import reports `overlayRollbackFailure`

**What it means:** `overlayRollbackFailure` (`code: scip_overlay_rollback_failed`)
in the admin job status means `scip-import` could not restore the previous
overlay after its forced index failed or was cancelled. Nothing retries the
restore, so the requested artifact may still be installed and the next index,
including an automatic sync, would use it.

**What to do:**

1. Handle the job's `failure`, or its `cancelled` status, and any
   `cleanupFailure` as described above; they still describe the forced index.
2. Fix the directory or path, for example because `.cartograph/scip/` is not
   writable or `overlay.scip` is no longer a regular file.
3. Restore the overlay you want, delete `.cartograph/scip/overlay.scip` to
   return to native extraction, or run `scip-import` again.

<details>
<summary>Details: how the overlay is installed and rolled back</summary>

`scip-import` installs the requested artifact at `.cartograph/scip/overlay.scip`
before its forced index. When that index fails or is cancelled, the import puts
the previous overlay back, or removes the new one if there was none.
`overlayRollbackFailure` means that restore failed too.

</details>

## Index fails during the parse stage

**What it means:** File-local parse failures name one escaped normalized
project-relative input and a fixed reason. The prior generation remains
queryable throughout.

**What to do:**

1. For automation, request structured stderr (the text form is concise):

   ```sh
   cartograph index /absolute/path/to/project --format json
   ```

2. For `source_changed_during_parse`, let rapid writes settle and retry.
3. A stable repeated reason points to the named source/extractor boundary; fix
   or deliberately ignore that input, then rerun the ordinary index.

| `file_failure.reason` | `error.code` | Meaning |
| --- | --- | --- |
| `source_changed_during_parse` | `parse_source_changed` | The named file no longer matches the exact manifest entry read at the start of that attempt. |
| `extraction_grammar_unavailable` | `parse_grammar_unavailable` | The statically linked parser grammar was unavailable. |
| `extraction_parser_stopped` | `parse_parser_stopped` | The parser stopped before producing a syntax tree. |
| `extraction_invalid_span` | `parse_invalid_span` | The parser produced a source span outside the durable contract. |
| `extraction_output_limit_exceeded` | `parse_extraction_output_limit_exceeded` | The extracted file facts exceeded the configured output limit. |

<details>
<summary>Details: structured failure fields</summary>

The nonzero result contains `error.code`, `error.stage`, and an
`error.file_failure` object with `path`, `reason`, and a credential-safe
`description`. The path is relative to the project; absolute roots, source
text, literals, database URLs, and raw parser/driver messages remain omitted.
MCP admin status exposes the same evidence as `fileFailure`.

</details>

## A deeply nested file degrades extraction

**What it means:** Grammar-backed extraction defaults to `maxAstDepth: 256`.
Exceeding the bound is recoverable: the report names up to 32 exact normalized
degraded paths, reports how many additional paths were truncated, retains each
affected file as partial, and continues the rest of the generation.

**What to do:**

1. Prefer ignore rules or project `exclude` globs for generated/build output.
2. Configure a value from 64 through 1024 only when authored source
   legitimately needs it:

   ```json
   {
     "maxAstDepth": 512
   }
   ```

A larger global bound should not be used to hide an unexpected generated-source
tree.

## Source excerpt is omitted

**What it means:** `cartograph node/show` returns source only when the complete
live manifest still matches the generation owning the symbol's line range. On
stale or racing source, metadata remains but the excerpt is omitted rather than
slicing the wrong bytes.

**What to do:** Re-index and retry.

## Native generation reaches its capacity bound

**What it means:** The index reached the Cartograph process's
`maxGenerationBytes` policy. This is a real admission boundary, not a
database-health diagnosis, and is not the managed PostgreSQL container's 2 GiB
memory ceiling. `index --format json` reports the stage-qualified code
`parse_generation_capacity_exceeded`, `resolve_generation_capacity_exceeded`,
or `reduce_generation_capacity_exceeded`; an MCP admin job reports
`generation_capacity_exceeded` as its `failureDetail.reason`.

**What to do:**

1. Inspect the exact stage/reason and the returned native metrics. Direct JSON
   and MCP admin failures name that limit, its `cartograph_process` scope, and
   the next action.
2. With the default `generationStorage: "auto"`, large source manifests and
   workspaces with at least 64 Cargo manifests select PostgreSQL spill
   automatically. For a dense smaller manifest, force it in
   `.cartograph/config.json`:

   ```json
   {
     "generationStorage": "postgres",
     "maxSpillBytes": 137438953472,
     "maxSpillRows": 1000000000
   }
   ```

3. Before raising quotas, verify PostgreSQL data/WAL/temporary-disk headroom;
   logical spill bytes are not physical storage estimates.
4. When the error occurs at the maximum `maxGenerationBytes` (8 GiB,
   `8589934592` bytes), exclude generated metadata, compiled artifacts, or
   other machine-produced paths with `index --exclude`, project `exclude`, or
   `.cartographignore`, then run an explicit index.

<details>
<summary>Details: spill limits, compact bounds that spill does not lift, and SCIP overlays</summary>

A spill-specific byte or row limit leaves the current generation visible and
the failed staging work eligible for bounded cleanup. Lease loss, cancellation,
and a byte-different retry also fail closed. Exact retained retries reuse
immutable batches and the durable canonical partition cursor.

PostgreSQL spill does not make every native structure unlimited. Resolution
lookups, clone profiles, and the centrality graph retain a separate compact
bound based on `maxGenerationBytes`. The accepted maximum is 8 GiB
(`8589934592` bytes); it cannot be raised beyond that process-safety ceiling.
Invalid values now name `maxGenerationBytes` and its exact inclusive range
instead of making status/index fail with an opaque options message.

SCIP overlays support both storage strategies and no longer force `auto` into
memory. Their covered-source basis and imported facts remain bounded by native
working limits; reduce an oversized overlay or source admission policy when
that independent bound fails.

</details>

For a measured example with parser, resolver, publication, memory, row-count,
and no-op timings kept separate, see the published
[large public corpus streaming benchmark record](v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md).

### Retention reports `project_busy` after an index

**What it means:** Automatic retention can report `reason: "project_busy"`
after a successful index when another live writer owns the migration lease.
That report is historical and retryable; it does not make the index
unsuccessful.

**What to do:** Wait for the named writer to finish, then retry index or a
bounded prune. `admin unlock` removes database-clock-expired leases only and
cannot clear a live `project_busy` outcome.

## Native stage reports `progress_stalled`

**What it means:** The supervisor cancels an operation when its active stage
produces no durable work inside the configured progress watchdog. The prior
generation remains visible, and a failed staging generation is handled by
normal bounded cleanup.

**What to do:**

1. Inspect the named stage, bounded database logs, host memory/CPU, and
   PostgreSQL I/O or lock pressure.
2. Retry only after identifying transient resource pressure or a fixed defect.
3. For an MCP admin index job, poll the job while it runs to tell a busy host
   from a stalled stage (see details).

| Reason | Meaning |
| --- | --- |
| `*_progress_stalled`, such as `parse_progress_stalled`, `resolve_progress_stalled`, or `relational_merge_progress_stalled` | The watchdog observed no completed work checkpoint. |
| `*_deadline_exceeded` | An item or whole-stage execution horizon elapsed, or a stage database statement outlived its statement timeout (for example a spilled `reduce_deadline_exceeded`). |

Direct CLI, MCP admin, and auto-sync output retain the qualified privacy-safe
reason; source paths, SQL, database URLs, and driver text are not included.

<details>
<summary>Details: live job progress, heartbeats, and cancellation grace</summary>

For an MCP admin index job, polling the job now returns live supervisor
progress while it runs: stage, completed items/bytes, heartbeat count, idle
time, completed stage timings, total elapsed time, and cancellation state. A
busy host is therefore distinguishable from a stalled stage without exposing
source or database text.

The index lease is renewed by its own task, independently of how pipeline work
is scheduled: the heartbeat count keeps advancing while a stage performs long
CPU work, and polling status can no longer stall the pipeline's progress
updates. A heartbeat count that stops advancing while a job is active therefore
points at PostgreSQL or lease trouble, or a starved host, rather than a busy
stage. Renewal pauses during a cancellation grace period, so a supervisor state
that stays `cancelling` (or `wedged`, after a progress stall) while the
heartbeat count advances means the cancelled work is still finishing a long
synchronous section. The job waits for that section for at most one COPY
timeout after the grace, three minutes for index jobs. It then fails its
staging generation and releases the lease, or, if the section is still
running, ends unreaped: renewal stops, and the job leaves both for lease expiry
and recovery.

</details>

## Semantic search is skipped

**What it means:** Hybrid mode requires a reachable OpenAI-compatible embedding
endpoint and a model registration whose fingerprint, dimension,
current-generation coverage, HNSW index, and query probe all pass. Otherwise
the packet reports `not_configured`, `not_indexed`, `stale`, or `unavailable`
and falls back explicitly to lexical evidence. It never labels BM25-only
results as hybrid.

**What to do:** Read the reported state, restore the failing requirement (the
endpoint, model registration, current-generation coverage, or HNSW index), and
retry the query.

<details>
<summary>Details: embedding sweep counters</summary>

An embedding sweep reports the complete `corpus_documents` alongside
`reused_documents` and `endpoint_documents`. Unchanged current-generation
documents reuse matching content-addressed vectors before endpoint work; only
documents whose rendered embedding input changed are submitted. The legacy
`documents` counter remains the endpoint-work count for compatibility.

</details>

## An LLM credential is missing from doctor's shell

**What it means:** `doctor` reads the project configuration in its own shell,
but the MCP server reads a tier's `apiKeyEnv` variable from its own
environment. A variable that is unset in doctor's shell is reported as
`llm-<tier>-credential`.

**What to do:**

1. Confirm that the MCP server's own environment supplies the named variable;
   doctor's shell can differ from it.
2. To supply the key to the server without a plaintext secret in a host
   configuration and without wrapping `cartograph serve` in a secret-manager
   launcher, configure a [credential command](CONFIGURATION.md#credential-sources):

   ```sh
   cartograph llm setup . --preset jev \
     --api-key-command /path/to/secret-helper --api-key-arg get --api-key-arg typesafe-api-key
   ```

| Check | Severity |
| --- | --- |
| `llm-<tier>-credential` for an optional tier (decision/Jev, summarize, local, ask, classify, reranker) | Warning; never makes doctor or onboarding unready. The message names the configured variable. |
| `llm-<tier>-credential` for the required embedding tier | Failure. |
| Invalid tier, for example `llm-decision-config` for an unexpected Jev model, endpoint or timeout | Failure. |

<details>
<summary>Details: how the credential command runs</summary>

`doctor` and `llm smoke` then run that command, bounded and without a shell,
and report only whether it produced a credential, naming the program and exit
status on failure. The server runs the same command on the tier's first use. If
it fails there, Jev reports `providerError: "credential_unavailable"` and
explore keeps native retrieval. The server tries the command again 30 seconds
later, without a host restart.

</details>

## ParadeDB derived index is unhealthy after a crash

**What it means:** Community BM25 is rebuildable local derived state. Treat
relational graph and search-document rows as source of truth.

**What to do:**

1. Inspect:

   ```sh
   cartograph db derived-index --project-path .
   ```

2. Rebuild with the exact confirmation phrase (command help says only "Exact
   acknowledgement required with --rebuild"):

   ```sh
   cartograph db derived-index --project-path . \
     --rebuild --confirm rebuild-managed-derived-indexes
   ```

> [!NOTE]
> `db derived-index` works only with a managed database. External PostgreSQL
> has no CLI equivalent.

> [!WARNING]
> Do not claim Community BM25 is WAL-crash-durable or use a rebuild to hide
> relational data loss.

## V1 import fails

**What it means:** The importer fails closed on missing prerequisites and on
unsupported or inconsistent v1 data. V2 never opens the SQLite file.

**What to do:**

1. Check the prerequisites:
   - The source must be a v1.1.33 PostgreSQL schema in the same database as a
     distinct v2 destination schema.
   - The destination may already have a current generation, but project
     index/sync/hook/rebuild writers should be quiesced during the import.
2. Run `--dry-run` first against the exact checkout represented by v1.
3. Repeat the identical confirmed command only when the error says the durable
   run is resumable.

| Failure | What to do |
| --- | --- |
| Unsupported languages, mismatched bytes/hashes, invalid required symbol coordinates, orphan relations, oversized JSON, malformed required data, incomplete schema history, or an inconsistent checkpoint | These fail closed; correct the source or checkout. Malformed optional JSON evidence is treated as unavailable rather than imported. |
| "another Cartograph writer published during v1 import; retry after it is idle" (internally `ConcurrentPublication`): another writer won publication | Quiesce those writers and repeat the identical confirmed command; Cartograph has already failed/released the stale generation and will reserve a newer one. |
| v1 exists only in SQLite | Rebuild from source or use v1.1.33 to migrate it to PostgreSQL first. |

See [PostgreSQL operations](STORAGE-BACKENDS.md#import-from-v1133-postgresql)
for the exact sequence.

## Generation prune fails or rolls back

**What it means:** Prune requires the exact `prune-old-generations`
confirmation and a live project-wide migration lease. Publication/retention
locks and a final PostgreSQL-clock fence check intentionally roll the active
transaction back if ownership expires or changes. Earlier committed batches
remain durable.

**What to do:**

1. Inspect `batches_committed`, `retiring_remaining`, and `deferred_reason`.
2. Inspect current operations, then retry the same bounded command; never
   bypass the fence.

| Reason or outcome | Meaning | What to do |
| --- | --- | --- |
| `search_relation_byte_budget` | Eligible search relations exceed the remaining byte budget. A large old relation does not prevent cleanup of later relations within budget. | Inspect `db usage` and the backup before using an audited `--maximum-search-relation-bytes` override (hard maximum 64 GiB). |
| `deferred_reason: "parent_delete_deferred"` | The final generation row's cascading foreign-key checks exceeded their short bound; that row was left `retiring` without rolling back the rows already drained. | A later prune or autovacuum lets it finish. |
| Repeated `batch-deadline` on a very large, bloated schema | Usually dead tuples from a previous invocation are unvacuumed. | `VACUUM` the fact tables, rerun the prune, then compact indexes online. |
| `retention_backlog` (automatic indexing) | Bounded cleanup is still removing generations but more than one remains; the previous generation stays visible. | Nothing; the watcher retries after 2–30 seconds. |
| `cache_only` (automatic indexing) | Generation cleanup failed but parse-cache eviction committed. | Inspect `retentionMaintenance` in `db usage` and run a bounded prune. |

<details>
<summary>Details: keyset draining, the deferred parent delete, and post-prune vacuum</summary>

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
to autovacuum.

</details>

<details>
<summary>Details: the automatic-indexing backlog drain and <code>retentionMaintenance</code></summary>

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

</details>

## Git review is unavailable

**What it means:** Review requires a Git worktree and a valid non-option
revision. Git execution is shell-free, output-bounded, deadline-bounded, and
noninteractive. A missing/invalid ref, unavailable Git, output limit, and
timeout are distinct redacted failures.

**What to do:** Confirm the ref exists locally and the project root is a
repository.

## Release archive or install checksum fails

**What it means:** The downloaded archive does not match the published
`SHA256SUMS` entry.

**What to do:** Do not bypass a mismatch.

1. Download `SHA256SUMS` and the archive from the same immutable release.
2. Verify the tag/version/asset name.
3. Retry the download.

Release archives should contain only the native binary and allowlisted notices/
documentation—never PostgreSQL, ParadeDB, pgvector, SQLite, or credentials.
