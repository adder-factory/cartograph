# Troubleshooting Cartograph v3

[Documentation home](README.md) · [CLI reference](CLI-REFERENCE.md) ·
[Native storage](STORAGE-BACKENDS.md)

These commands describe the RocksDB backend in `v3.0.1`. Earlier v2 binaries
use PostgreSQL. Install v3 and rebuild the graph from source before using
the native commands below.

## Fast diagnosis

Use the same absolute executable and checkout as the failing host:

```sh
/absolute/path/to/cartograph --version
/absolute/path/to/cartograph doctor /absolute/path/to/project --json
/absolute/path/to/cartograph status /absolute/path/to/project --json
/absolute/path/to/cartograph db usage --project-path /absolute/path/to/project --format json
```

Capability readiness, a published graph, current source, model readiness, and
the host's loaded MCP process are independent checks. Retain the reported
failure, generation, freshness, truncation, and cleanup fields.

| Symptom | Action |
| --- | --- |
| `db start`, SQL, or managed-port options are unknown | Use the native CLI built from this checkout; server/container/SQL commands are retired |
| Native primary is occupied | On Unix CLI routes to the same-user project owner; otherwise wait for that process to close. Windows currently supports one process per project |
| Status is stale | Run a bounded index or inspect the owning MCP server's `autoSync` failure and retry counters |
| `lease_busy` | Another native writer is admitted; wait or cancel that exact job |
| Cancelled job remains `cancelling` | Actual native/source work is still draining; the owner retains admission |
| Owner changed during job polling | The previous job outcome is unverified; inspect current status with the new owner before starting new work |
| Missing `CURRENT` or no published generation | Run the explicit native migration or first index |
| `operation_deadline` or byte/row budget reached | Inspect the specific bound and reduce the requested work; never remove all bounds |
| Native ownership or format check fails | Preserve `.cartograph`; use the matching binary and inspect the safe error. Do not remove locks or edit RocksDB files |
| Vectors are unavailable | Configure the intended model and run an explicit embedding sweep; structural retrieval remains useful |

## Doctor works in a shell but MCP cannot connect

An already-attached host may still run an older executable. Verify the exact
absolute command in its project registration. Install the chosen local native
binary's registration, restart that host, then prove a fresh MCP status and
real query. A successful CLI call alone does not prove an attached host loaded
this source cutover.

```sh
/absolute/path/to/cartograph install --yes --target codex --location local \
  --project-path /absolute/path/to/project
```

Native projects do not use database URLs or managed ports. Old environment
variables cannot select the PostgreSQL backend because that runtime is removed.
Install v3 with the release installer before migrating a v2 project. The
`upgrade --apply` command then reconciles native state and recognized host pins.

## Index is stale

Compare status with the actual source. A complete manifest includes supported
source and source-admission policy; edits to either make a previous generation
stale. A new extractor contract can also require a new publication on unchanged
source. Run `cartograph index <project>` and require `live_source: matched`,
then a fresh status and generation-scoped query.

A serving MCP owner watches edits and periodically reconciles missed events.
Inspect `autoSync.active`, events, attempts, publications, errors, retry time,
and capacity suppression. Five repeated failures may suppress automatic work;
correct the reported condition and run one explicit index. `--no-auto-sync`
disables the watcher; `--no-startup-sync` disables the startup reconciliation.

## Index reports `lease_busy` or `index_cleanup_failed`

`lease_busy` is the retained public category for another admitted native writer.
Readers continue against the complete current revision. Cancel only the exact
admin job you intended, or wait for it to finish. CLI job polling is fenced to
the owner instance, so a restarted owner cannot receive an old cancellation for
a reused numeric job ID.

The first index SIGINT/SIGTERM requests cooperative cancellation. A second one
ends the process immediately. A pending native call keeps its task and writer
admission until actual completion. The WAL and atomic current-revision pointer
preserve a complete graph after crash/reopen. A commit that already completed
is reported independently of post-publication freshness and maintenance.

`cleanup_failure` or MCP `cleanupFailure` describes a secondary cleanup failure
and never replaces the original failure. A timeout is unverified cleanup, not
proof that work stopped. Preserve those diagnostics before retrying.

## Native storage keeps growing

Start with `db usage`. Compare allocated database/SST/WAL bytes, live logical
payload, pending compaction bytes, cache use, and retained revisions. Repeated
unchanged indexing should reuse the existing revision. Changed publications
write changed records under a new atomic revision pointer.

Automatic bounded collection retains the current and protected reader
snapshots and two superseded revisions. Auxiliary data uses cursor-based
collection and a grace interval. Explicit bounded pruning can advance eligible
work; inspect the report before requesting another batch. Old SST files are
reclaimed by RocksDB compaction after obsolete versions and snapshots permit
it. No command deletes PostgreSQL volumes or legacy data as a side effect.

There is currently no online native backup/restore or user-triggered compaction
CLI. Close the project owner before a complete directory copy. Do not copy an
actively changing directory and call it a verified backup.

## Native generation reaches its capacity bound

The extractor still has explicit whole-generation memory and pipeline budgets;
changing the database does not make extraction streaming or unlimited. Inspect
the exact capacity diagnostic, source bytes, modeled retained bytes, and
worker counts. Exclude generated/vendor/build output through project source
policy or a bounded run-scoped exclusion. Native graph publication also has
finite byte, row, and per-operation budgets. Increasing a configuration field
does not override a lower native publication bound.

## Semantic search is skipped

Read the model-scoped readiness and reason. Exact model identity, dimensions,
current document content, and embeddings must agree. Configure the desired
embedding endpoint/model and run `embed`, then check `embedding-status`.
This native implementation computes cosine similarity through a bounded exact
scan; it does not provide pgvector/HNSW acceleration. A missing model is an
explicit unavailable state, not a silent substitute model.

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

## Native stage reports `progress_stalled`

The supervisor requests cancellation when a stage produces no completed work
checkpoint within its watchdog. Inspect the named stage, progress counters,
finite queue/memory/byte budgets, and host resource pressure before retrying.
A running admin job reports stages, completed items/bytes, heartbeat counts,
elapsed and idle time, and cancellation state without exposing source text.

An item or stage `*_deadline_exceeded` means its finite execution horizon
elapsed. A progress heartbeat proves liveness, not completed work. A cancelling
or wedged native operation remains owned until its actual tasks finish; writer
admission is not released merely because a grace or shutdown timer expired.
The previous complete graph remains readable while an unpublished attempt
reconciles. Inspect independent cleanup diagnostics before another attempt.

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
