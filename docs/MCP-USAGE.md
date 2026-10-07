# MCP usage for coding agents

[Documentation home](README.md) · [Project overview](../README.md) ·
[CLI reference](CLI-REFERENCE.md) · [Troubleshooting](TROUBLESHOOTING.md)

Last release audit: 2026-10-07 (`v2.1.42`).

Cartograph v2 exposes a compact native stdio MCP server. Its core returns
bounded, generation-scoped evidence and never makes the database a source of
truth over the live checkout. The optional `cartograph_ask`, role, summary, and
dead-code-judge branches, and the optional decision-tier (Jev) calls in
`cartograph_explore`, `cartograph_context`, and `cartograph_propose_rename`
(see [Optional Jev navigation](CONFIGURATION.md#optional-jev-navigation)), can
call configured LLM tiers; their model/evidence provenance, failure, and
fallback states remain explicit.

**On this page:** [Fast path](#fast-path) · [Registration](#registration) ·
[Auto-sync and admin jobs](#auto-sync-and-admin-job-reporting) ·
[Profiles](#profiles) ·
[Modern protocol](#modern-protocol-and-dynamic-tool-selection) ·
[Selected high-use tools](#selected-high-use-tools) ·
[Reliable agent loop](#reliable-agent-loop) ·
[Transport contract](#transport-contract) ·
[Modernization order](#modernization-order)

## Fast path

1. Register the project with `cartograph install --yes --target <HOST>
   --location local --project-path .`.
2. Reopen the host so it starts the newly registered MCP child.
3. Call `cartograph_status` and require a fresh current generation.
4. Call `cartograph_context` or `cartograph_find` once and require real
   generation-scoped evidence. Registration and CLI success alone do not prove
   the attached transport.

## Registration

Prefer the native installer because it writes project-local configuration and
pins the absolute stable launcher path for versioned native installs:

```sh
cartograph install --yes --target codex --location local --project-path .
cartograph install --yes --target claude --location local --project-path .
cartograph install --yes --target cursor --location local --project-path .
```

When the managed database uses a non-default loopback port, add
`--managed-database-port <PORT>` to the install command. The generated stdio
command carries that port directly; no host-specific environment table is
required.

### Manual server definition

```json
{
  "command": "/absolute/path/to/cartograph",
  "args": [
    "serve",
    "--mcp",
    "--project-path",
    "/absolute/path/to/project",
    "--managed-database-port",
    "55435"
  ]
}
```

Omit the final two arguments when the project uses the default port `55432` or
external PostgreSQL through `CARTOGRAPH_DATABASE_URL`.

### Restart and prove the transport

> [!IMPORTANT]
> Restart the host after installation. An already-open host is not assumed to
> hot-reload an upgraded MCP process.

`cartograph doctor --json` keeps on-disk registration and live MCP transport as
separate readiness states. The CLI does not claim either one from a successful
database or generation check: inspect the project-local registration, restart
the host when it changes, then make a fresh MCP status and real query call to
prove the loaded transport.

## Auto-sync and admin job reporting

### Managed preflight and source catch-up

Before serving, managed mode checks the owned image, HNSW shared-memory
allocation, and explicit CPU/memory/process policy. An incompatible container
fails with exact backup and confirmed upgrade commands. Source catch-up starts
through the native watcher after the stdio server is ready, so modern
`server/discover`/`tools/list` and the legacy `initialize` handshake do not wait
for a full index.

`autoSync` in `cartograph_status` exposes:

- attempts, publications, no-ops, and errors;
- stable stage-specific `lastErrorCode`;
- failure/retry times;
- unchanged-revision attempt count and retry suppression;
- cross-revision capacity-failure count;
- the exact capacity limit/scope/next action;
- the additive `lastCleanupFailureCode` (see below).

### Automatic retry policy

| Outcome | Code | Handling |
| --- | --- | --- |
| Persistent failure of an unchanged revision | — | Bounded exponential backoff; stops after five automatic attempts until source changes |
| Concurrent edits | `source_changed_during_index` or `parse_source_changed` | Recovery scheduled after 2–30 seconds; does not exhaust the persistent-failure circuit |
| Another live lease owner, a writer still inside its prepare transaction, or a lock that keeps the read of the project's leases waiting past its bounded wait | `lease_busy`, normally detected before reserving a generation; a writer that wins after that check is refused at lease acquisition | Recovery scheduled after 2–30 seconds; does not exhaust the persistent-failure circuit |
| An undrained failed-generation backlog | `retention_backlog` | Recovery scheduled after 2–30 seconds; does not exhaust the persistent-failure circuit |
| A lease the attempt could not establish, keep, or confirm: an acquisition that failed for a reason other than contention, or a lost or unconfirmed heartbeat | `lease_failed` | Retains its separate failure handling |
| Database errors, including a read of the project's leases that fails for another reason | `status_failed` for that lease read | Retain their separate failure handling |
| Generation-capacity failure | — | Five trip a separate circuit that new source revisions cannot bypass; adjust the reported capacity setting and run an explicit index to prove recovery and clear the circuit |
| PostgreSQL unavailable before a revision can be recovered | — | Watcher events share an unknown-revision backoff bucket and retain a capped recovery probe rather than retrying every event or becoming permanently suppressed |

- A retry timer runs even when no new filesystem event arrives; it does not wait
  for the 30-second missed-event reconciliation.
- Every automatic failure also attempts bounded cleanup of terminal failed
  generations.
- When an attempt's own staging cleanup fails after another failure,
  `lastErrorCode` keeps the first failure's code and its retry policy, and the
  additive `lastCleanupFailureCode` reports `index_cleanup_failed`; it is
  omitted otherwise.

### Watcher timing and sync flags

Watcher events use a 750 ms quiet window with a default two-second hard
coalescing deadline, then call the indexer's own manifest/no-op fence directly.
Automatic attempts cap native extraction at four workers and omit independent
Git-history enrichment; explicit indexing refreshes those auxiliary channels.

| `serve` flag | Effect |
| --- | --- |
| `--no-startup-sync` | Suppresses only the initial reconciliation |
| `--no-auto-sync` | Disables both native watching and periodic reconciliation for an operator-controlled recovery host while leaving explicit MCP/CLI operations available |

### Admin index job failures

A failed admin job's terminal status keeps the failure that ended it in
`failure`. Additive objects sit beside it:

| Object | When it appears | Contents |
| --- | --- | --- |
| `fileFailure` | A non-recoverable file-local admin index failure; the terminal job still exposes its stable failure category and the previous published generation remains untouched | The normalized project-relative `path`, fixed `reason`, and credential-safe `description` |
| `cleanupFailure` | An `index`, `sync`, `embed-only`, `scip-import`, or indexing `init` job failed and the bounded cleanup of its own staging generation also failed, or a cancelled job's cleanup was not confirmed by PostgreSQL (see details) | `code: index_cleanup_failed` and `message` |
| `overlayRollbackFailure` | A `scip-import` job's forced index failed or was cancelled and restoring the previous SCIP overlay failed too | `code: scip_overlay_rollback_failed` and `message` |
| `failureDetail` | A terminal generation-capacity failure | Guidance naming `maxGenerationBytes`, its `cartograph_process` scope, and the next action |

Absolute checkout paths, source/parser text, literals, database URLs, and driver
messages are not part of the MCP result. An invalid parser-recovery span or
parser stop without cancellation is a successful partial-file outcome with
`extraction_invalid_span` or `extraction_parser_stopped` in the bounded
degraded-file report.

<details>
<summary>Details: cleanup failures and cancelled jobs</summary>

When an `index`, `sync`, `embed-only`, `scip-import`, or indexing `init` admin
job fails and the bounded cleanup of its own staging generation also fails (for
`scip-import`, the staging generation of its forced index), its terminal status
adds an optional `cleanupFailure` object with `code: index_cleanup_failed` and
`message`, the same object direct `index --format json` reports as
`cleanup_failure`. It never replaces `failure`, which stays the failure that
ended the job. A cancelled job has no `failure`; like a cancelled direct index,
it adds `cleanupFailure` unless PostgreSQL confirms that no generation it
reserved is still `staging` or `ready` and that its lease names none of them. A
generation left `staging` is terminalized by the next index's staging preflight.

</details>

> [!WARNING]
> A `scip-import` job whose forced index fails or is cancelled restores the
> project's previous SCIP overlay. When that restore fails too, its terminal
> status adds an optional `overlayRollbackFailure` object of the same shape,
> with `code: scip_overlay_rollback_failed` and `message`. It sits beside
> `failure`, or the cancellation, and any `cleanupFailure`, and replaces none of
> them; a cancelled import still gets the PostgreSQL cleanup check above.
> Nothing retries the restore: `.cartograph/scip/overlay.scip` may still hold
> the requested artifact, which the next index, including an automatic one,
> would use.

## Profiles

Select a profile with `cartograph serve --mcp --profile <PROFILE>`; `core` is
the default. Only `cartograph_admin` is profile-restricted; every other tool
belongs to all five profiles.

| Profile | Advertised tools | Intended use |
| --- | --- | --- |
| `full` | All 36, including `cartograph_admin` | Every advertised tool, including bounded administration |
| `core` (default) | All 36, including `cartograph_admin` | Normal coding tools plus explicit bounded administration |
| `coding` | The 35 tools other than `cartograph_admin` | Lean retrieval, source, graph, test-selection, and review loop |
| `review` | The 35 tools other than `cartograph_admin` | Comparison and verification-oriented surface |
| `read-only` | The 35 tools other than `cartograph_admin` | Retrieval without write/admin operations: `serve` additionally refuses every mutating call branch |

`serve --no-write-tools` applies the same read-only call enforcement under any
profile.

Profiles are immutable authorization ceilings for one server process. Tool
lists are deterministic, and a tool hidden by the selected profile or an exact
`--disable-tool` cannot be called by name. Use the narrowest ceiling that
supports the workflow.

## Modern protocol and dynamic tool selection

Cartograph is a dual-era stdio server:

- modern clients use MCP `2026-07-28`, begin with `server/discover`, and send
  their protocol version and capabilities on every request;
- existing clients may continue to use the `2024-11-05` initialize handshake;
- modern successful results carry `resultType: "complete"` and server identity;
- modern `tools/list` is private-cacheable for one hour and returns the same
  deterministically ordered authorization-scoped catalog for the lifetime of
  the process.

MCP `2026-07-28` deliberately forbids changing `tools/list` per connection or
as a side effect of another request. Cartograph therefore does not implement a
generic hidden-tool dispatcher or a session-local activate/deactivate tool.
Those patterns would hide the selected operation's schema and annotations from
the host and weaken host-level confirmation policy.

Dynamic selection belongs in the agent host: it can retain Cartograph's
complete stable catalog, search the compact name/title/description metadata,
and place only task-relevant full schemas in the model's working context. The
profile remains the call-time authorization ceiling regardless of which schemas
the host currently presents to the model. A host that does not support deferred
schema loading can safely expose the complete profile catalog.

## Selected high-use tools

The full profile advertises 36 tools. The table below highlights the normal
coding loop; see the [complete CLI/MCP alignment inventory](cli-mcp-alignment.md#public-mcp-tools)
for all 36 wire contracts and their CLI families.

| Tool | Purpose |
| --- | --- |
| `cartograph_status` | Current generation, row counts, compact database/schema storage as exact bytes plus readable IEC units, complete supported-source freshness, and auto-sync state |
| `cartograph_context` | Intent-aware exact/BM25/hybrid packet, typed primary edit candidates, graph evidence, affected tests, trust, and live overlay |
| `cartograph_find` | Exact name/path/reference, bounded live-source regex, or BM25/hybrid candidates |
| `cartograph_files` | Bounded current-generation file inventory filtered by directory or language |
| `cartograph_entry_points` | Typed routes, CLI commands, MCP tools, CLI declarations, and public API boundaries with exact totals |
| `cartograph_at_range` | Exact symbols overlapping one source range or diff hunk |
| `cartograph_node` | Exact symbol metadata and bounded source only when indexed line provenance is fresh; batches retain partial results and identify unresolved or ambiguous inputs |
| `cartograph_graph` | Bounded callers/callees/impact, exact edge filters, shortest paths, or model-scoped pgvector symbol neighbors |
| `cartograph_affected` | Structurally connected test candidates; file and symbol modes both enforce `depth`, `maxNodes`, and `limit`, and file mode reports `impact.nodesTruncated` when its traversal budget is reached |
| `cartograph_numerical` | Generation-scoped static numerical sites, coverage, explanation, and non-executing probe plans with explicit evidence levels and unknowns |
| `cartograph_review` | Git-ref plus committed/staged/unstaged/untracked review packet |
| `cartograph_playbook` | Complete agent workflow, tool-routing map, evidence discipline, and anti-patterns |
| `cartograph_admin` | Start, inspect, or cancel bounded lifecycle, index, semantic, model, and SCIP interchange work |

### Graph traversal fields

Graph `via.from_symbol_id` and `via.to_symbol_id` describe traversal order:
the preceding and discovered symbols. `via.edge_source_symbol_id` and
`via.edge_target_symbol_id` always describe the stored relation direction.
For `A calls B`, an incoming traversal from B discovers A while retaining
edge source A and edge target B. These endpoint fields also appear in Jev
navigation graph results.

### Numerical analysis

`cartograph_numerical` currently uses the `rust_ast_v1` static analyzer for
parsed or partial Rust files.

| Mode | Returns |
| --- | --- |
| `sites` | Exact source spans and bounded categories without persisting expressions or literal values |
| `coverage` | Supported, analyzed, skipped/failed, and site-bearing files, separately |
| `explain` | Can attach graph-selected tests for one exact owner |
| `plan` | Probe steps without executing project code |

Static `heuristic`, future runtime observation, and formal-proof evidence remain
separate. Observation and formal adapters currently report `not_configured`, and
stale generations report `stale_static_evidence` even when stale reads are
explicitly allowed.

### Review lenses and context mode

`cartograph_review` risk mode binds every lens to the current generation and
returns a `lensStatus` for findings, hotspots, dead-code candidates, and
coverage. Each lens is independently bounded and reports `ready`, `timeout`, or
`unavailable` with stage, limit, and retry guidance. Counts derived from the
returned window are labeled `returned_rows_only`; partial non-Git or large
project evidence is never presented as a complete scan. The structural-findings
lens additionally reports `not_computed` with the explicit refresh action when
its exact relation is absent; an empty result is therefore never mislabeled as
a clean ready lens.

`cartograph_review` context mode accepts the shared `pathFilter` and
`allowStale` fields advertised by its schema:

- `pathFilter` is a validated project-relative segment prefix for both live Git
  comparison and supplied-diff evidence; sibling prefixes do not match.
- `allowStale` is a compatibility no-op in context mode because the Git review
  packet already reports immutable graph freshness separately, while non-Git
  review lenses continue to use it as an explicit stale-evidence opt-in.

### Biomarkers

- `cartograph_biomarkers` is read-only. When the current fingerprint has no
  stored complete relation it returns `state: not_computed`, `findings: []`, and
  the exact confirmed refresh action without starting detector computation.
- `cartograph_status` preserves the rest of its payload in that state and
  reports an empty inline rollup with `biomarkerRollupState: not_computed`.
- The `cartograph_admin` `biomarkers-refresh` action is dry-run-first; execution
  requires `dryRun: false`, `confirm: true`, and optionally accepts
  `databaseQueryTimeoutMs` from 1 through 1800000 as the exact inner PostgreSQL
  statement timeout. `timeoutMs` remains an exclusive legacy alias.
- Results name the effective timeout and its source; the MCP client deadline
  must be longer than that database deadline.

### Dead code and digest

`cartograph_dead_code` materializes its deterministic, exempted
`maxCandidates` orphan window before bounded edge/source enrichment. A database
statement timeout is a typed `dead_code_query_timeout`. `cartograph_digest`
isolates all five concurrent sections: `sectionStatus` records `ready`,
`timeout`, or `unavailable`, safe fallback values replace only failed sections,
and `degraded` states whether any section was incomplete.

### Find content mode

`cartograph_find` content mode treats `query` as a bounded Rust regular
expression. Its optional `pathFilter` is a case-sensitive literal substring of
the normalized project-relative path, not a glob or another regular
expression; a basename such as `lib.rs`, a directory segment, or a longer path
fragment therefore scopes the scanned inventory directly. Invalid expressions
return a safe parser category and zero-based byte offset without echoing the
query text.

### Node batches

`cartograph_node` accepts either one `symbol` or a batch of up to 20 exact
`symbols`. A batch preserves every resolvable result, lists missing inputs in
`unresolved`, and lists each ambiguous input in `ambiguous` with up to ten
candidate identities and an explicit `truncated` flag. A single-symbol request
continues to fail closed when its name is ambiguous.

### Context intents, Jev ranking, and the live overlay

`cartograph_context` classifies deterministic task intents such as symbol
lookup, implementation trace, change planning, test selection, error diagnosis,
architecture survey, and documentation lookup. Intent selects bounded candidate,
graph, evidence, and affected-test policy and is returned in the packet.

When the project's Jev decision tier lists the `context` feature, the packet's
BM25/semantic candidates are reordered by one metadata-only relevance request
(no source is sent). Anchors and graph expansion keep their positions; judged
items carry an advisory `decision_relevance`, primary edit candidates follow the
relevant judged files (basis `decision_relevance`), and `decision_rank` reports
the model, outcome and judged count in every output format. A provider failure
rebuilds the packet through the configured reranker.
`mode: deterministic` never consults the provider.

When the durable generation is stale, supported changed/untracked files may
contribute a separate live working-tree overlay. Overlay items include path,
Git change kind, exact content digest, line-bounded excerpt, matched terms, and
truncation. They are never relabeled as durable graph evidence.

## Reliable agent loop

1. `cartograph_status`.
2. `cartograph_context` with the concrete coding task and any known exact
   name/path/reference anchors.
3. Orient with `entry_points`, then focus with `files`, `at_range`, `find`,
   `node`, or `graph`; use
   `direction: path` with `toSymbolId` when the question is how two exact
   symbols connect; use `direction: similar` for stored-vector peers and keep
   the returned model ID and score provenance, then read the exact files before
   editing.
4. Make the change.
5. `cartograph_review` against the intended base and `cartograph_affected` for
   verification candidates.
6. Run the project's actual formatter, linter, type, test, and security gates.
7. Re-index only when current graph evidence is needed after source changes.

The modern discovery result and legacy initialize response contain the compact
version of this loop. Call `cartograph_playbook` for the complete on-demand
guide, or run `cartograph guide` outside MCP.

> [!IMPORTANT]
> Always preserve generation ID, freshness, confidence, abstention, component
> ranks, coarse reference precision, multiplicity, truncation, and overlay
> status in downstream reasoning. A candidate is evidence to inspect, not proof
> that a change is correct.

## Transport contract

The server uses newline-delimited JSON-RPC over stdio and writes no diagnostic
text to stdout. It enforces:

- bounded input and serialized output bytes;
- bounded concurrent requests;
- a hard wall-clock request deadline;
- cancellation with worker abort/reaping;
- stable public error codes and redacted internal failures;
- deterministic tool/schema ordering;
- dual-era modern per-request metadata and legacy initialization;
- modern private TTL caching without connection-dependent tool mutation.

> [!WARNING]
> Do not retry by removing bounds or wrapping the server with an unbounded
> queue.

### Long-running admin jobs

For long index work, use `cartograph_admin` to start a job and poll status;
cancel explicitly when the host/user abandons it. While an index job is running,
its job view includes a live `progress` packet with:

- the current stage;
- monotonic completed items/bytes;
- heartbeat count and progress-idle time;
- completed-stage timings and total elapsed time;
- cancellation state.

These counters expose actual work without source paths, source text, SQL, or
database settings.

### Deferred retention

Retention attached to a successful index (its `retention` object) can be
independently deferred:

| Field | Value |
| --- | --- |
| `state` | `"deferred"` |
| `reason` | `"project_busy"` |
| `retryable` | `true` |
| `unlock_applicable` | `false` |
| `next_action` | A wait-and-retry action |

`admin unlock` removes expired leases only; it does not retroactively clear that
historical outcome or steal a live writer.

### SCIP interchange jobs

SCIP interchange is also job-based. Use `action: "scip-export"` with a
project-relative `out`, or `action: "scip-import"` with a project-relative
`in`. Import persists a digest-fenced overlay, forces indexing, preserves files
the SCIP artifact does not cover, and reports exact typed-edge versus unresolved
foreign-link counts. The browser visualizer is intentionally absent; graph and
interchange data remain available to agents.

### When the transport closes

If the MCP transport closes, report that limitation and use the equivalent
native CLI as a control path. CLI success alone does not prove the host's MCP
registration or running process was refreshed.

## Modernization order

The next protocol work should be delivered in this order:

1. Add exact `outputSchema` contracts and validate every emitted
   `structuredContent` payload, starting with status, find/context, review, and
   admin job envelopes.
2. Expose immutable-generation resources and resource links for source windows,
   evidence packets, schemas, and durable artifacts. Resource URIs must carry
   project/generation identity and preserve freshness and privacy boundaries.
3. Map durable `cartograph_admin` jobs to the modern
   `io.modelcontextprotocol/tasks` extension with bounded TTL, polling,
   cooperative cancellation, and result retrieval; retain synchronous fallback
   for clients that do not advertise the extension.
4. Add rate-limited `notifications/progress` only for requests that provide a
   progress token, with monotonic phase/total evidence and no query/source text.
5. Add a small prompt surface for user-selected review, diagnosis, and change
   planning workflows where host support makes prompts materially useful.
6. Use multi-round-trip elicitation for non-secret operator choices and exact
   destructive confirmations only when the client advertises it. Credentials,
   tokens, and passwords must never use form elicitation.
7. Consider Streamable HTTP, OAuth, and routing headers only for an explicitly
   authorized remote/hosted product. Local Cartograph remains private stdio by
   default.

Do not newly build on MCP roots, client sampling, or protocol logging. They are
deprecated in `2026-07-28`; Cartograph already has explicit project ownership,
direct bounded LLM clients, stderr for local diagnostics, and can add
OpenTelemetry outside the model-facing protocol when structured observability
is needed.

Protocol references: [versioning and dual-era compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning),
[modern tools and stable catalogs](https://modelcontextprotocol.io/specification/2026-07-28/server/tools),
[tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview), and
[deprecated features](https://modelcontextprotocol.io/seps/2577-deprecate-roots-sampling-and-logging).
