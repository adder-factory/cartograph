# Native CLI reference

[Documentation home](README.md) · [Project overview](../README.md) ·
[MCP usage](MCP-USAGE.md) · [Troubleshooting](TROUBLESHOOTING.md)

Last release audit: 2026-10-26 (`v3.0.6`).

The installed executable is `cartograph`. Run `cartograph <command> --help` for
the exact bounds in the installed version. Bounded maintenance and its
confirmation phrases are documented in
[Storage and operations](STORAGE-BACKENDS.md#usage-and-bounded-maintenance). This page
lists the complete top-level command inventory and selected high-use forms;
subcommand help remains the authority for every option and default.

**On this page:** [Find the right surface](#find-the-right-surface) ·
[High-use coding forms](#high-use-coding-forms) ·
[Command inventory](#complete-top-level-command-inventory) ·
[MCP and agent configuration](#mcp-and-agent-configuration) ·
[Upgrade](#upgrade) · [Database lifecycle](#database-lifecycle) ·
[LLM credentials](#llm-credentials-and-local-backend-state) ·
[Database selection](#database-selection) ·
[Exit and error behavior](#exit-and-error-behavior)

## Find the right surface

| Need | Use |
| --- | --- |
| Normal coding loop | [High-use coding forms](#high-use-coding-forms) |
| Read an index failure, cancellation, or JSON error object | [Index failure reporting](#index-failure-reporting) |
| Every public top-level command | [Complete command inventory](#complete-top-level-command-inventory) |
| Upgrade to a new release and read its report | [Upgrade](#upgrade) |
| Exact flags, defaults, or bounds | `cartograph <command> --help` from the installed version |
| Confirmation phrase for a destructive operation | [Bounded maintenance](STORAGE-BACKENDS.md#usage-and-bounded-maintenance) |
| Equivalent agent tools | [CLI and MCP alignment](cli-mcp-alignment.md) |
| Database ownership and lifecycle | [Native storage and operations](STORAGE-BACKENDS.md) |

## High-use coding forms

```text
cartograph index [PROJECT] [--exclude GLOB]...
cartograph sync-if-dirty [PROJECT] [--quiet] [--max-file-size SIZE]
cartograph status [PROJECT]
cartograph watch --project-path [PROJECT]
cartograph service start|stop|status --project-path [PROJECT]  # macOS login owner
cartograph embedding-status [PROJECT]
cartograph embed [PROJECT]
cartograph find <QUERY> --by auto|name|content|env|sql|build|path|reference|bm25|hybrid
  [--format text|json] [--compact]
cartograph context <TASK> [--exact-name NAME] [--exact-path PATH] [--exact-reference TEXT]
cartograph entry-points [--bucket routes|cli|cli-commands|mcp-tools|cli-files|public-exports]
  [--limit 20]
cartograph graph <SYMBOL_ID> --direction callers|callees|both|impact
cartograph graph <SYMBOL_ID> --direction path --to <TARGET_SYMBOL_ID>
  [--edge-kind calls|imports|references|implements|extends|tests|type-of|returns|instantiates|overrides|decorates|field-access|def-use|exports|contains]
cartograph graph <SYMBOL_ID> --direction similar
  [--k 5] [--min-score 0.3] [--same-language] [--model-id <UUID>]
  [--edge-kind similar-to]   # similar-to is valid only with --direction similar
cartograph affected [CHANGED_FILE ...] [--stdin | --files CHANGED_FILE ...]
  [--depth 5] [--max-nodes 200] [--limit 50]
cartograph affected --symbol-id <SYMBOL_ID> [--depth 5] [--max-nodes 200]
  [--limit 50]
cartograph show <SYMBOL_ID>
cartograph review --ref <GIT_REF>
cartograph doctor [PROJECT]
cartograph admin biomarkers-refresh [--no-dry-run --confirm]
  [--database-query-timeout-ms 240000]
cartograph admin scip-export [--out index.scip] [--maximum-rows 5000000]
cartograph admin scip-import [--in index.scip] [--maximum-rows 10000000]
  [--maximum-source-bytes 268435456] [--workers 16]
```

### Retrieval and output formats

Retrieval inputs and result counts are bounded. `context` selects a deterministic
typed task intent and can use exact anchors, native BM25, a ready matching
semantic model, graph expansion, affected tests, and a separate stale
working-tree overlay. JSON is the stable automation format; text favors concise
operator output.

`find` retains JSON as its compatibility default and accepts the uniform
`--format json|text` selector. `--format text` renders bounded name/kind/path
rows with freshness and truncation; `--compact` remains an independent exact-
name JSON payload modifier and is still passed unchanged to the retrieval tool.

### Index failure reporting

A non-recoverable file-local index failure keeps the previous generation visible
and reports one exact normalized project-relative path plus an allowlisted
reason. Text escapes control characters in the path. `index --format json` exits
nonzero and writes a structured `error` object to stderr:

| Field | Contents |
| --- | --- |
| `code` | Always the failure that ended the attempt |
| `message` | The failure message |
| `stage` | The index stage of the failure |
| `previous_generation_visible` | Read from the native current-revision pointer after the failure: `true` when a published generation is still current, `false` when the project has none (for example, before its first successful index), and `null` when that bounded lookup itself failed |
| `file_failure.path`, `file_failure.reason`, `file_failure.description` | The project-relative path, allowlisted reason, and description of a non-recoverable file-local failure |
| `cleanup_failure` | Optional secondary failure; see [Cleanup failures](#cleanup-failures) |

The object never renders the absolute checkout root, source text, literal
values, database URL, or parser/driver internals. MCP admin job status uses the
same bounded `fileFailure` evidence.

Invalid parser-recovery spans and parser stops without cancellation are instead
retained as partial files and listed in a successful index report with degraded
reason `extraction_invalid_span` or `extraction_parser_stopped`.

For a generation-capacity failure, text and JSON name `maxGenerationBytes`, the
`cartograph_process` scope, its hard 8 GiB maximum, and the bounded
generated-artifact-exclusion and finite native-build limits. An out-of-range
configuration names the field and exact inclusive range. Auto-sync performs
bounded failed-generation cleanup after each failed attempt and suppresses
itself after five capacity failures across source revisions; adjust the
reported setting and run an explicit index to clear that circuit. See
[Native generation reaches its capacity bound](TROUBLESHOOTING.md#native-generation-reaches-its-capacity-bound).

#### Failure codes

| `code` | Meaning |
| --- | --- |
| `lease_busy` | Retryable. Another admitted native writer owns the project. Wait for it to finish or cancel its exact admin job; reads continue against the last complete revision |
| `index_failed` with message `Cartograph project status is unavailable` | The bounded current-publication or live-source observation failed; freshness is unverified |
| `index_cleanup_failed` (as `code`, alone) | The pre-reservation recovery of abandoned staging generations itself failed for a reason other than contention |
| `request_cancelled` | The index request was interrupted; see [Index cancellation](#index-cancellation) |
| `source_changed_during_index` | An ordinary index whose checkout kept changing after it published rebuilt a bounded number of times and then failed |



#### Cleanup failures

In the `index --format json` failure object, `code` is always the failure that
ended the attempt. When bounded cleanup of the attempt's own staging generation
also fails afterward, and the attempt's final native cleanup does not
terminalize that generation either, the object adds a separate
`cleanup_failure` (`code: index_cleanup_failed`, `message`) instead of replacing
`code`; the next index retries that cleanup. MCP admin job status reports the
same object as `cleanupFailure`. For a cancelled index, `cleanup_failure`
instead reflects reconciliation of admitted native work described in
[Index cancellation](#index-cancellation).

### Successful index report: `live_source`

A successful `index --format json` report includes `live_source`:

| `live_source` | Meaning |
| --- | --- |
| `matched` | The live checkout still matched the report's `source_revision` at the request's last source check (for an unchanged no-op, that check is the one made during preparation) |
| `changed_after_publication` | This request published a complete generation, but the checkout changed before the request finished, so that current generation is not fresh |
| `unverified` | Reported only by the supervised child of `upgrade --apply` when it published but cancellation or a scan failure cut the post-publication check short; the upgrade's next-process status then decides freshness |

An ordinary index never reports `changed_after_publication`: when the checkout
changes after it publishes, it rebuilds a bounded number of times and then fails
with `source_changed_during_index`. Only the supervised child of
`upgrade --apply` publishes once and reports the change.

### Index cancellation

Once indexing starts, the first SIGINT or SIGTERM cancels it cooperatively.
Before publication, the previous complete graph remains visible. Native work
keeps its writer admission until the actual source, extractor, or storage task
finishes; a timeout is not evidence that a native operation has stopped. WAL
and the atomic current-revision pointer preserve a complete graph on reopen.
A publication that already committed remains current; its actual commit result
is preserved while post-publication freshness and maintenance are reported
independently. A second interrupt ends the process immediately.

CLI jobs routed to a Unix owner cancel and poll the same job on the same owner
instance. If that owner restarts, the old job outcome is unverified; a reused
numeric job ID cannot authorize cancellation on the replacement process.
MCP cancellation retains admission and drains actual native completion while
unrelated ping requests remain available. Background jobs have their own finite
controls, and cancelling the request that started a job does not replace the
explicit job cancellation action.


### Admin background jobs

Generated admin commands poll the admitted job until its terminal result.
Native jobs have a five-minute storage-operation ceiling; shorter request and
stage budgets still apply. Cancellation changes a running job to `cancelling`
until its actual work drains. A bounded shutdown wait that expires reports
unverified cleanup and retains the admitted operation instead of marking it
finished. Completed reports are retained in a bounded eight-job history.

Unix CLI polling and cancellation bind to the owner instance that started the
job. Owner replacement is a conflict with an unverified prior result. It cannot
redirect an old cancellation to a new job with the same numeric ID.

### Embedding sweeps

`embed` carries forward matching content-addressed vectors before calling the
configured endpoint. Its report distinguishes:

| Field | Counts |
| --- | --- |
| `corpus_documents` | The complete corpus |
| `reused_documents` | Pre-existing vectors carried forward |
| `endpoint_documents` | Documents newly submitted to the endpoint |
| `documents` | Legacy field: remains the endpoint-work count for wire compatibility |

`reused_documents` and `endpoint_documents` together cover the corpus when
readiness is complete. `embedding-status` is read-only and should be used before
requesting an explicit sweep.

### Hook-driven sync: `sync-if-dirty`

`sync-if-dirty` skips a clean, current checkout. Otherwise it runs a bounded
reconciliation. On Unix it routes through the running project owner when one
exists; otherwise it opens the native project itself. A competing writer is
never stolen. After contention, it checks whether the other writer already
published the exact current source before retrying its own index. SIGINT and
SIGTERM cancel an owner-routed job and wait for its actual terminal report.

Like `index`, a failure preserves the primary category and independent cleanup
diagnostics. `--quiet` suppresses successful hook output, not failures.

### SCIP import and export

- `scip-export` requires a fresh generation and writes atomically inside the
  project. It emits standard SCIP plus a forward-compatible Cartograph extension
  for every exact edge kind and represented site count.
- `scip-import` validates a bounded project-local artifact, installs it at
  `.cartograph/scip/overlay.scip`, and forces a new generation. Covered files use
  SCIP facts; uncovered files retain native extraction.
- The overlay digest participates in freshness, so changing it cannot leave an
  apparently current generation.

> [!WARNING]
> A forced index that fails or is cancelled restores the prior overlay when the
> importer still owns the installed bytes. If that restore fails as well, the
> job keeps its `failure` (or its `cancelled` status) and any `cleanupFailure`,
> and adds a separate `overlayRollbackFailure`
> (`code: scip_overlay_rollback_failed`, `message`): the requested artifact may
> still be installed, and the next index would use it.

### Entry points

`entry-points` reads typed structural facts rather than asking BM25 to infer an
API boundary. It returns stable pages for routes, CLI commands, exported MCP
tool definitions, conventional CLI source, and exported declarations with no
in-tree calls/references/type-use. Every page includes its exact pre-limit total
and truncation flag. V2 includes exported constants, types, enums, traits,
modules, components, and resources in the public surface in addition to v1's
function/class categories.

### Doctor JSON

`doctor --json` separates hard capability health from completed onboarding.

| Field | Contents |
| --- | --- |
| `ready` (legacy) and `capabilitiesReady` | Booleans that preserve the existing exit-code contract |
| `projectReadiness` | Independent `database`, `index`, `freshness`, `deterministicRetrieval`, `semanticRetrieval`, `registration`, `liveTransport`, and `overall` states |
| `nextActions` | Next steps that use placeholders rather than absolute paths or database settings |

- A missing generation is `not_indexed`; a published generation can be `stale`
  without being confused with absence.
- Optional semantic configuration does not gate deterministic retrieval.
- `--no-project-checks` returns `not_checked` for the skipped layers.

## Complete top-level command inventory

This inventory contains every non-hidden native top-level command advertised
by `cartograph --help`. Hidden compatibility adapters and Clap's generated
`help` command are intentionally excluded.

<!-- CARTOGRAPH_TOP_LEVEL_COMMANDS_START -->

- Project and runtime: `index`, `status`, `embed`, `embedding-status`, `show`,
  `export`, `similar`, `sync-if-dirty`, `install-hooks`, `mcp-budget`,
  `completions`, `guide`, `doctor`.
- Code intelligence and agent state: `ask`, `blame`, `changed-since`, `context`,
  `compare-to-ref`, `digest`, `explore`, `find`, `node`, `files`,
  `entry-points`, `at-range`, `graph`, `affected`, `tests-for`, `biomarkers`,
  `numerical`, `coverage`, `dead-code`, `deps`, `hotspots`, `host`, `history`,
  `imports`, `note`, `propose-rename`, `role`, `session`, `summaries`,
  `trace-to-culprits`, `verify`, `review`, `playbook`, `admin`.
- Configuration and lifecycle: `backend`, `llm`, `upgrade`, `install`,
  `uninstall`, `serve`, `watch`, `service`, `db`.

<!-- CARTOGRAPH_TOP_LEVEL_COMMANDS_END -->

## MCP and agent configuration

```text
cartograph serve --mcp [--profile coding|core|full|read-only|review] [--no-startup-sync] [--no-auto-sync]
cartograph install --yes --target <TARGET[,TARGET...]> --location local
cartograph uninstall --target <TARGET[,TARGET...]> --location local
```

The 19 concrete host target IDs are:

<!-- CARTOGRAPH_INSTALL_TARGETS_START -->

`claude`, `cursor`, `codex`, `codebuddy`, `copilot`, `codewhale`, `zed`,
`opencode`, `hermes`, `gemini`, `antigravity`, `kiro`, `factory`, `rovo`,
`qoder`, `bob`, `kimi`, `pi`, and `reasonix`.

<!-- CARTOGRAPH_INSTALL_TARGETS_END -->

### Target selectors and locations

- `--target` also accepts the selectors `auto`, `all`, and `none`.
- All concrete targets support global configuration. Project-local
  configuration is supported for every target except `hermes`, `antigravity`,
  and `reasonix`.
- A project-local selection skips those targets without writing elsewhere; text
  output prints a warning, while JSON omits a report for each skipped target.
- `uninstall` takes no `--yes` flag; without `--target` it removes entries for
  every known target.

### What install and uninstall change

- Install/uninstall preserves unrelated entries and pins the absolute native
  executable.
- A versioned native installation is registered through the stable
  `~/.cartograph-cli/current/bin/cartograph` launcher, whose target changes
  atomically on upgrade.
- With `--location local`, installation modifies only project-local agent
  configuration. Each project uses its own native storage owner.

- A local install then indexes the project through its native storage owner
  and installs Git hooks unless
  `--no-hooks`. If that index fails or is interrupted, install exits nonzero
  before installing hooks, in either output format; with `--format json` the
  index failure object precedes install's own error line on stderr.

> [!IMPORTANT]
> Restart the host after a configuration or binary change.

### Merging an existing registration

Rewriting an existing `cartograph` entry merges instead of replacing it.

| Cartograph owns and rewrites | Kept as written |
| --- | --- |
| `command`, its own server flags in `args` (`serve`, `--mcp`, `--project-path`, the native owner flags), and the target's transport keys | `env`, `cwd`, other host-specific keys, and extra server arguments such as `--profile` |

An entry that launches Cartograph through a wrapper (`/usr/bin/env`,
`op run --`, `doppler run --`, `aws-vault exec`, `direnv exec`, or a custom
helper), meaning a non-Cartograph `command` whose arguments name a Cartograph
executable followed by `serve`, is never replaced: installation repins only that
embedded executable argument, and only when it is an absolute path. A `PATH`
name through a wrapper is left as written. To replace a wrapper with a direct
registration, uninstall first.

### Stdio protocol and profiles

The stdio server is dual-era: MCP `2026-07-28` clients use stateless
`server/discover` and per-request metadata, while existing clients can continue
using the `2024-11-05` initialize handshake. Profiles and exact disabled tools
form a process-lifetime authorization ceiling (see
[MCP profiles](MCP-USAGE.md#profiles)). Modern `tools/list` is stable,
deterministically ordered, and private-cacheable for one hour; task-local schema
selection belongs in the host rather than a connection-mutating dispatcher.

## Upgrade

`cartograph upgrade --project-path <PATH>` audits published release and host
registration state; `--apply` installs a checksum-verified published release
and reconciles the native project. For the v2-to-v3 cutover, first install
v3 and run `cartograph db migrate --to rocksdb --project-path <PATH>`; legacy
PostgreSQL user state is preserved separately, not imported into RocksDB.

Read `completed`, `projectReconciliation`, and `restartRequired` separately.
Stop the existing project owner before upgrading: on macOS use `service stop`,
then `upgrade --apply`, then `service start`. For manually managed owners, use
the equivalent cooperative stop/start through their service manager. Verify
the new owner's status version separately from the client's `--version`.
A retryable reconciliation can be resumed with the same command. Reopen the
host after its executable/pin changes; a no-op later invocation cannot prove
that an already attached child has restarted.

## Native database operations

```sh
cartograph db migrate --to rocksdb --project-path . --format json
cartograph db usage --project-path . --format json
cartograph db prune --project-path . --keep-superseded 2 \
  --maximum-deletions 4 --maximum-cascade-rows 10000 \
  --confirm prune-old-generations --format json
```

Migration rebuilds source facts, verifies current native evidence, and records
a completion marker. It preserves old PostgreSQL resources; legacy-only user
state is not imported. Usage is read-only and does not initialize absent native
storage. It reports file allocation, SST, WAL, memtables, and pending compaction
separately from logical records.

Pruning is bounded, resumable, and protects current readers. Inspect phase
errors/deferred work before repeating it. `--maximum-deletions` admits 1..100,
`--keep-superseded` admits 0..32, and `--maximum-cascade-rows` admits 1..10000.
Automatic reconciliation also attempts smaller maintenance batches. Physical
reclamation depends on later native compaction.

Managed PostgreSQL lifecycle, SQL queries, v1 database imports, and SQL index
compaction are retired. No online checkpoint/restore CLI is implemented yet.
See [storage](STORAGE-BACKENDS.md) for ownership, bounds, and current limits.

### Biomarker statistics

Full-generation biomarker statistics are computed once per exact input
fingerprint and then served from generation-fenced storage, so `status` only
ever reads the stored relation and never evaluates the detector cascade behind
its five-second deadline. The fingerprint covers:

- the current generation;
- the superseded generation the growth detector compares against;
- imported coverage;
- materialised similarity;
- the calendar day bounding the growth window;
- the detector contract compiled into the binary.

Any change marks the stored relation uncomputed until an explicit refresh
replaces it.

| State | Meaning |
| --- | --- |
| `featureReadiness.biomarkers` `state: pending`, `reason: not_computed` | Before the first computation. `cartograph biomarkers` returns the same typed state without mutation, and requested status rollups return `biomarkers: []` plus `biomarkerRollupState: not_computed` instead of failing |
| `state: unavailable` with `reason: timeout` or `reason: database_error` | Remains reserved for a storage read that genuinely failed |

`cartograph admin biomarkers-refresh` is dry-run-first. Execute mode requires
`--no-dry-run --confirm` and accepts a native operation deadline through
`--database-query-timeout-ms`, from 1 through 300000. The old
`--timeout-ms` spelling remains an exclusive compatibility alias. Dry-run and
execution output report `statementTimeoutMs` and `statementTimeoutSource`; the
caller deadline bounds the operation even when the selected inner timeout is longer.

### Dead code and digest

The deterministic dead-code query applies framework/test/fixture exemptions and
materializes a PageRank-prioritized `maxCandidates` orphan window before
outgoing-edge aggregation and source lookup.

- Test code includes Rust `tests` module segments and sibling `tests.rs` files.
- A symbol named as a parameter, return or field type counts as used.
- It is a one-hop orphan check: a cluster of symbols that only use each other is
  not reported, while functions passed as values and trait methods reached only
  through dispatch can still appear.
- A genuine statement timeout is reported as `dead_code_query_timeout` with
  bounded retry guidance instead of a generic tool failure.

`digest` runs its five bounded sections concurrently and returns each section's
`ready`, `timeout`, or `unavailable` status; one failed section receives a safe
empty/null fallback and sets `degraded: true` without discarding the other four.

## LLM credentials and local backend state

```text
cartograph llm migrate-credentials [PROJECT] [--tier-env TIER=ENV]
  [--apply --confirm migrate-inline-credentials]
cartograph llm setup [PROJECT] --preset custom --tier <TIER> --endpoint URL --model MODEL
  [--api-key-env ENV | --api-key-command EXE [--api-key-arg ARG]... | --clear-credentials]
cartograph llm setup [PROJECT] --preset jev
  [--api-key-env ENV | --api-key-command EXE [--api-key-arg ARG]...]
  [--jev-features explore,context,roles,rename] | [--clear-credentials]
cartograph llm setup [PROJECT] --preset cli-bridge --tier <chat|local|ask|classify>
  --model MODEL --command EXECUTABLE [--arg ARG]... --input <stdin|arg>
  [--prompt-template TEMPLATE] --response-format <raw|json-path|claude>
  [--response-path PATH]
cartograph backend cleanup [PROJECT] [--minimum-age-hours 24]
  [--apply --confirm cleanup-backend-junk]
```

`llm migrate-credentials` and `backend cleanup` are both dry-run-first, bounded,
and emit secret-free JSON. Credential migration requires an exact
environment-value match, serializes Cartograph writers with a private lock, and
aborts if the config bytes changed after the proof was made.
`--preset cli-bridge` requires `--model MODEL`.

### Credential sources for setup

- Custom setup clears retained credentials automatically when the
  provider/endpoint origin changes; `--clear-credentials` is the explicit
  same-origin removal path and conflicts with `--api-key-env` and
  `--api-key-command`.
- `--api-key-command` stores a credential helper's argv (repeat `--api-key-arg`
  for each argument) instead of a variable name; the serving process runs it
  without a shell on the tier's first use, and the two sources are mutually
  exclusive.
- Every preset that accepts `--api-key-env` accepts it; the MCP
  `cartograph_admin` `llm-apply` action takes the same argv as `apiKeyCommand`.
  See [credential sources](CONFIGURATION.md#credential-sources).
- `doctor` and `llm smoke` run a configured command and report only whether it
  produced a credential.

### Jev features

`--jev-features` applies only to the Jev preset and writes the decision tier's
`features` list; without it an existing list is preserved and a new tier permits
exploration only. See
[Optional Jev navigation](CONFIGURATION.md#optional-jev-navigation).

### Backend cleanup

Backend cleanup considers only generated-name rotated logs and invalid
current-version PID state; it preserves current logs, valid or active
processes, and state written by an unsupported newer/older format. Cleanup JSON
exposes only validated project-relative entry names and stable path-free reason
codes; unsafe/control-character names are never echoed.

## Native storage selection

Every project uses `.cartograph/data` RocksDB 11.8.1. `storageBackend` is a
verified migration marker, not an optional backend. Retired database flags are
rejected, and old PostgreSQL environment variables do not choose a runtime.
One primary owns a project; Unix CLI/MCP clients route to the active MCP owner.
Windows currently requires closing the other primary before a new process opens
that project. Never delete locks or state to bypass ownership.

## Exit and error behavior

Invalid inputs, missing capabilities, stale revision or ownership fences, unavailable
database/source/Git state, and operation failures return nonzero. Public errors
omit database URLs, query text, source literals, and absolute project paths.
Machine consumers should inspect JSON fields and stable MCP error codes rather
than parse human prose.

### Persistent project owner

`cartograph watch --project-path .` keeps the existing RocksDB owner, watcher,
and bounded maintenance alive independently of MCP stdin on Unix. SIGINT and
SIGTERM drain admitted requests, watcher work, and native storage before exit.
On macOS, `cartograph service start --project-path .` registers the invoking
executable with launchd at login. `service status` reports private registration,
loaded state, and IPC availability separately. `service stop` stops only that
project's verified owned service and removes its login registration. An existing
owner keeps the primary; launchd retries after it exits. Additional CLI/MCP
processes retain their own authorization ceilings when using the owner.
