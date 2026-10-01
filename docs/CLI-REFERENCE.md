# Native CLI reference

[Documentation home](README.md) · [Project overview](../README.md) ·
[MCP usage](MCP-USAGE.md) · [Troubleshooting](TROUBLESHOOTING.md)

Last release audit: 2026-10-01 (`v2.1.33`).

The installed executable is `cartograph`. Run `cartograph <command> --help` for
the exact bounds and confirmation phrases in the installed version. This page
lists the complete top-level command inventory and selected high-use forms;
subcommand help remains the authority for every option and default.

## Find the right surface

| Need | Use |
| --- | --- |
| Normal coding loop | [High-use coding forms](#high-use-coding-forms) |
| Every public top-level command | [Complete command inventory](#complete-top-level-command-inventory) |
| Exact flags, defaults, bounds, or confirmation phrases | `cartograph <command> --help` from the installed version |
| Equivalent agent tools | [CLI and MCP alignment](cli-mcp-alignment.md) |
| Database ownership and lifecycle | [PostgreSQL storage and operations](STORAGE-BACKENDS.md) |

## High-use coding forms

```text
cartograph index [PROJECT] [--exclude GLOB]...
cartograph sync-if-dirty [PROJECT] [--quiet] [--max-file-size SIZE]
cartograph status [PROJECT]
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
cartograph affected [CHANGED_FILE ...] [--stdin | --files CHANGED_FILE ...]
  [--max-depth 5] [--max-nodes 40] [--limit 40]
cartograph affected --symbol-id <SYMBOL_ID> [--max-depth 5] [--max-nodes 40]
  [--limit 40]
cartograph show <SYMBOL_ID>
cartograph review --ref <GIT_REF>
cartograph doctor [PROJECT]
cartograph admin biomarkers-refresh [--no-dry-run --confirm]
  [--database-query-timeout-ms 240000]
cartograph admin scip-export [--out index.scip] [--maximum-rows 5000000]
cartograph admin scip-import [--in index.scip] [--maximum-rows 10000000]
  [--maximum-source-bytes 268435456] [--workers 16]
```

Retrieval inputs and result counts are bounded. `context` selects a deterministic
typed task intent and can use exact anchors, ParadeDB BM25, a ready matching
semantic model, graph expansion, affected tests, and a separate stale
working-tree overlay. JSON is the stable automation format; text favors concise
operator output.

`find` retains JSON as its compatibility default and accepts the uniform
`--format json|text` selector. `--format text` renders bounded name/kind/path
rows with freshness and truncation; `--compact` remains an independent exact-
name JSON payload modifier and is still passed unchanged to the retrieval tool.

A non-recoverable file-local index failure keeps the previous generation visible
and reports one exact normalized project-relative path plus an allowlisted reason. Text
escapes control characters in the path. `index --format json` exits nonzero and
writes a structured `error` object to stderr with `code`, `message`, `stage`,
`previous_generation_visible`, and `file_failure.path`, `reason`, and
`description`. It never renders the absolute checkout root, source text,
literal values, database URL, or parser/driver internals. MCP admin job status
uses the same bounded `fileFailure` evidence.

Invalid parser-recovery spans and parser stops without cancellation are instead
retained as partial files and listed in a successful index report with degraded
reason `extraction_invalid_span` or `extraction_parser_stopped`.
For a generation-capacity failure, text and JSON name `maxGenerationBytes`, the
`cartograph_process` scope, its hard 8 GiB maximum, and the bounded
PostgreSQL-spill/generated-artifact-exclusion next action. An out-of-range
configuration names the field and exact inclusive range. Auto-sync performs
bounded failed-generation cleanup after each failed
attempt and suppresses itself after five capacity failures across source
revisions; adjust the reported setting and run an explicit index to clear that
circuit.

`embed` carries forward matching content-addressed vectors before calling the
configured endpoint. Its report distinguishes the complete
`corpusDocuments`, pre-existing `reusedDocuments`, and newly submitted
`endpointDocuments`; their latter two counts cover the corpus when readiness is
complete. The legacy `documents` field remains the endpoint-work count for
wire compatibility. `embedding-status` is read-only and should be used before
requesting an explicit sweep.

`sync-if-dirty` skips a clean, current checkout. If another native watcher or
manual index owns the project's index lease, it observes that lease for a
bounded five minutes instead of stealing it or immediately returning
`lease_failed`. After the competing writer releases, the command succeeds when
that writer published the now-current source revision; otherwise it retries its
own complete index.

`scip-export` requires a fresh generation and writes atomically inside the
project. It emits standard SCIP plus a forward-compatible Cartograph extension
for every exact edge kind and represented site count. `scip-import` validates a
bounded project-local artifact, installs it at
`.cartograph/scip/overlay.scip`, and forces a new generation. Covered files use
SCIP facts; uncovered files retain native extraction. A failed publication
restores the prior overlay when the importer still owns the installed bytes.
The overlay digest participates in freshness, so changing it cannot leave an
apparently current generation.

`entry-points` reads typed structural facts rather than asking BM25 to infer an
API boundary. It returns stable pages for routes, CLI commands, exported MCP
tool definitions, conventional CLI source, and exported declarations with no
in-tree calls/references/type-use. Every page includes its exact pre-limit total
and truncation flag. V2 includes exported constants, types, enums, traits,
modules, components, and resources in the public surface in addition to v1's
function/class categories.

`doctor --json` separates hard capability health from completed onboarding.
The legacy `ready` boolean and new `capabilitiesReady` boolean preserve the
existing exit-code contract. `projectReadiness` independently reports
`database`, `index`, `freshness`, `deterministicRetrieval`,
`semanticRetrieval`, `registration`, `liveTransport`, and `overall` states.
A missing generation is `not_indexed`; a published generation can be `stale`
without being confused with absence; optional semantic configuration does not
gate deterministic retrieval. `--no-project-checks` returns `not_checked` for
the skipped layers. `nextActions` uses placeholders rather than absolute paths
or database settings.

## Complete top-level command inventory

This inventory contains every non-hidden v2.1.33 top-level command advertised
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
  `imports`, `note`, `propose-rename`, `role`, `session`, `summaries`, `sql`,
  `trace-to-culprits`, `verify`, `review`, `playbook`, `admin`.
- Configuration and lifecycle: `backend`, `llm`, `upgrade`, `install`,
  `uninstall`, `serve`, `db`.

<!-- CARTOGRAPH_TOP_LEVEL_COMMANDS_END -->

## MCP and agent configuration

```text
cartograph serve --mcp [--managed-database-port PORT] [--profile coding|core|full|read-only|review] [--no-startup-sync] [--no-auto-sync]
cartograph install --yes --target <TARGET[,TARGET...]> --location local [--managed-database-port PORT]
cartograph uninstall --yes --target <TARGET[,TARGET...]> --location local
```

The 19 concrete host target IDs are:

<!-- CARTOGRAPH_INSTALL_TARGETS_START -->

`claude`, `cursor`, `codex`, `codebuddy`, `copilot`, `codewhale`, `zed`,
`opencode`, `hermes`, `gemini`, `antigravity`, `kiro`, `factory`, `rovo`,
`qoder`, `bob`, `kimi`, `pi`, and `reasonix`.

<!-- CARTOGRAPH_INSTALL_TARGETS_END -->

`--target` also accepts the selectors `auto`, `all`, and `none`. All concrete
targets support global configuration. Project-local configuration is supported
for every target except `hermes`, `antigravity`, and `reasonix`. A project-local
selection skips those targets without writing elsewhere; text output prints a
warning, while JSON omits a report for each skipped target.

Install/uninstall preserves unrelated entries and pins the absolute native
executable. A versioned native installation is registered through the stable
`~/.cartograph-cli/current/bin/cartograph` launcher, whose target changes
atomically on upgrade. With `--location local`, installation modifies only
project-local agent configuration. For a non-default managed port, it also pins
the non-secret loopback port in the portable server arguments. Restart the host
after a configuration or binary change.

Rewriting an existing `cartograph` entry merges instead of replacing it.
Cartograph owns only `command`, its own server flags in `args` (`serve`,
`--mcp`, `--project-path`, `--managed-database-port`), and the target's
transport keys; `env`, `cwd`, other host-specific keys, and extra server
arguments such as `--profile` are kept. An entry that launches Cartograph
through a wrapper (`/usr/bin/env`, `op run --`, `doppler run --`,
`aws-vault exec`, `direnv exec`, or a custom helper), meaning a non-Cartograph
`command` whose arguments name a Cartograph executable followed by `serve`, is
never replaced: installation repins only that embedded executable argument, and
only when it is an absolute path. A `PATH` name through a wrapper is left as
written. To replace a wrapper with a direct registration, uninstall first.

The stdio server is dual-era: MCP `2026-07-28` clients use stateless
`server/discover` and per-request metadata, while existing clients can continue
using the `2024-11-05` initialize handshake. Profiles and exact disabled tools
form a process-lifetime authorization ceiling. Modern `tools/list` is stable,
deterministically ordered, and private-cacheable for one hour; task-local schema
selection belongs in the host rather than a connection-mutating dispatcher.

`cartograph upgrade --project-path <PATH>` is a read-only release and
registration audit. The canonical version-to-version operation is:

```sh
cartograph upgrade --apply --project-path <PATH>
```

That command is safe to repeat. It checksum-verifies and smoke-tests the latest
native release, atomically switches the stable launcher, starts or reuses the
project-owned managed database when applicable, applies safe append-only schema
migrations, reconciles a complete current generation, runs `doctor`, and uses
the installed executable to require an exact installed-version/fresh-generation
status. It then repairs stale owned Codex, Claude, and Cursor registrations in
local and global locations, preserving unrelated configuration and managed-port
arguments. Running `--apply` when the binary is already current resumes or
heals the project and registration steps instead of returning early.

Each audited registration reports a `commandState`. A direct pin, whose
`command` is itself a Cartograph executable (file name `cartograph`, or under
`~/.cartograph-cli/versions/*/bin/` or `current/bin/`), is `path_lookup`,
`absolute_unchecked`, `current_absolute`, or `stale_absolute`. A wrapper
registration is `wrapped`, and `wrappedExecutableState` reports its embedded
executable in the same vocabulary. Any other launcher is `custom_command` and
is never repaired. Only `stale_absolute` and a `wrapped` entry whose embedded
executable is `stale_absolute` are repaired. A stale direct pin is reinstalled
through the normal installer; a stale wrapped entry is repinned in place, so
its wrapper command, other arguments, `env`, and every other key are unchanged.
`registrationRepair.changes` lists every attempted entry with `target`,
`location`, `configPath`, `field` (`command` or `args[N]`), the unchanged
`wrapper`, `from` and `to` executable paths, and `outcome` (`repinned` or
`manual`). A `manual` entry carries a `manualStep` naming the exact edit; the
report never includes wrapper arguments or `env` values.

The command never replaces an incompatible managed container implicitly. It
keeps the verified binary installed, reports `completed: false`, and emits the
exact private-backup and `--confirm upgrade-managed-database` commands. After
that approval-gated replacement, rerun the same `upgrade --apply` command to
resume. JSON distinguishes `currentVersion` (the process that began the
operation), `latestVersion` (the published release), `installedVersion`,
`applied`, `completed`, and `restartRequired`; `projectReconciliation` reports
database, index, doctor, verification, freshness, generation, port, and any
required confirmation independently. Registration failures likewise leave the
already-verified binary installed and report only the remaining repair.

`restartRequired` is deliberately run-local: it is true only when a completed
invocation changed the installed binary or repaired a configured host pin. A
pure idempotent project reconciliation does not request another reopen, while
false still does not prove the version of a process that remained attached
across an earlier invocation. Managed `db start` has a separate 15-minute cold
image-pull/readiness budget. Exceeding it reports the database step as
`timed_out`, draws no compatibility conclusion, and asks the caller to rerun
the same command; only a bounded status probe with positive image/shared-memory
incompatibility evidence can emit the destructive confirmation path.

An already-open host cannot hot-load the new child. When `restartRequired` is
true, close and reopen it once, then prove `server/discover` (or legacy
`initialize`), `tools/list`, `cartograph_status`, and one real query on the new
transport.

## Database lifecycle

```text
cartograph db start
cartograph db stop
cartograph db status
cartograph db logs
cartograph db backup <OUTPUT>
cartograph db restore <ARCHIVE>
cartograph db upgrade
cartograph db derived-index
cartograph db remove
cartograph db import-v1
cartograph db prune
cartograph db usage
cartograph db compact
```

Storage inventory accepts `--limit` (1–128), `--table-offset`, and
`--index-offset` (0–100,000) for independent table/index pages. JSON includes
complete catalog counts, per-page truncation, statistics observation/reset
context, and nullable live/dead row estimates when counters are unobserved.
It also reports the latest automatic retention attempt, retiring generations,
and database allocation outside the observed relation catalog. This allocation
gap does not identify safely deletable files.

Prune accepts `--maximum-cascade-rows` (1–100,000,000; default 5,000,000) and
`--maximum-search-relation-bytes` (1–68,719,476,736; default 8 GiB). These are
invocation budgets. Canonical rows drain in resumable transactions of at most
10,000 rows; `batches_committed`, `retiring_remaining`, and `deferred_reason`
distinguish durable progress from pending work. Inspect the report before
repeating the confirmed command. MCP exposes the same limits as
`maximumCascadeRows` and `maximumSearchRelationBytes` on `prune-generations`.

Managed lifecycle is supported on macOS/Linux with local Docker. Windows uses
external PostgreSQL. Restore, upgrade, derived-index rebuild, remove, v1 import,
and prune require explicit operation-specific confirmation. `db usage` is
read-only: it verifies the exact current append-only migration ledger and fails
with migration guidance instead of creating or upgrading a schema. `db compact`
is a dry run unless `--apply --confirm compact-online-indexes` is supplied; with
`pgstattuple` installed it selects only B-trees whose measured reclaim reaches
`--minimum-reclaimable-bytes` (default 64 MiB), and otherwise falls back to size.
`db compact --heap` is a separate reclaimable-heap/TOAST plan; apply requires
`--confirm compact-heap-relations`, no live operation leases, and accepts the
`ACCESS EXCLUSIVE` lock taken by one bounded `VACUUM FULL` at a time. Managed
mode always verifies filesystem headroom and rejects an operator override,
while external PostgreSQL requires `--available-headroom-bytes` and an
installed `pgstattuple` extension. See
[PostgreSQL operations](STORAGE-BACKENDS.md).

Without an explicit `--port` or `CARTOGRAPH_MANAGED_DATABASE_PORT`, managed
commands inspect the deterministic project-owned container and reuse its actual
loopback port before falling back to `55432`. New/replaced containers reserve
256 MiB of Docker shared memory, use a 2 GiB hard memory limit with a 1 GiB
reservation, are limited to four CPUs and 256 processes, and use bounded
PostgreSQL memory/parallel-worker settings. A 15-minute checkpoint interval,
4 GiB soft maximum WAL size, and 512 MiB recycled-WAL floor absorb bursty
immutable-generation publication without repeated 1 GiB WAL checkpoints;
durability remains fully synchronous. `doctor`, MCP preflight, and
structured `db status` report both HNSW shared-memory and resource-policy
compatibility. Older containers require backup plus the confirmed managed
upgrade; status inspection never recreates them.
`status` includes compact allocated database/schema/heap/index/TOAST totals in
readable IEC units such as MiB and GiB. Structured JSON retains exact `*Bytes`
integers and adds `databaseStorage.humanReadable` display strings; `db usage`
retains the relation, cache, generation, and maintenance detail.
Full-generation biomarker statistics are computed once per exact input
fingerprint and then served from generation-fenced storage, so `status` only
ever reads the stored relation and never evaluates the detector cascade behind
its five-second deadline. The fingerprint covers the current generation, the
superseded generation the growth detector compares against, imported coverage,
materialised similarity, the calendar day bounding the growth window, and the
detector contract compiled into the binary; any change marks the stored relation
uncomputed until an explicit refresh replaces it.
Before the first computation `featureReadiness.biomarkers` reports
`state: pending` and `reason: not_computed`; `cartograph biomarkers` returns the
same typed state without mutation, and requested status rollups return
`biomarkers: []` plus `biomarkerRollupState: not_computed` instead of failing.
`cartograph admin biomarkers-refresh` is dry-run-first. Execute mode requires
`--no-dry-run --confirm` and accepts an explicit inner PostgreSQL statement
timeout through `--database-query-timeout-ms`, from 1 through 1800000. The old
`--timeout-ms` spelling remains an exclusive compatibility alias. Dry-run and
execution output report `statementTimeoutMs` and `statementTimeoutSource`; the
caller deadline must exceed the selected statement timeout. `state: unavailable` with `reason: timeout` or
`reason: database_error` remains reserved for a storage read that genuinely
failed.

The deterministic dead-code query applies framework/test/fixture exemptions
and materializes a PageRank-prioritized `maxCandidates` orphan window before
outgoing-edge aggregation and source lookup. Test code includes Rust `tests`
module segments and sibling `tests.rs` files. A symbol named as a parameter,
return or field type counts as used. It is a one-hop orphan check: a cluster
of symbols that only use each other is not reported, while functions passed
as values and trait methods reached only through dispatch can still appear. A genuine statement timeout is
reported as `dead_code_query_timeout` with bounded retry guidance instead of a
generic tool failure. `digest` runs its five bounded sections concurrently and
returns each section's `ready`, `timeout`, or `unavailable` status; one failed
section receives a safe empty/null fallback and sets `degraded: true` without
discarding the other four.

## LLM credentials and local backend state

```text
cartograph llm migrate-credentials [PROJECT] [--tier-env TIER=ENV]
  [--apply --confirm migrate-inline-credentials]
cartograph llm setup custom [--api-key-env ENV
  | --api-key-command EXE [--api-key-arg ARG]... | --clear-credentials]
cartograph llm setup [PROJECT] --preset jev
  [--api-key-env ENV | --api-key-command EXE [--api-key-arg ARG]...]
  [--jev-features explore,context,roles,rename] | [--clear-credentials]
cartograph llm setup [PROJECT] --preset cli-bridge --tier <chat|local|ask|classify>
  --command EXECUTABLE [--arg ARG]... --input <stdin|arg>
  [--prompt-template TEMPLATE] --response-format <raw|json-path|claude>
  [--response-path PATH] [--model MODEL]
cartograph backend cleanup [PROJECT] [--minimum-age-hours 24]
  [--apply --confirm cleanup-backend-junk]
```

Both commands are dry-run-first, bounded, and emit secret-free JSON. Credential
migration requires an exact environment-value match, serializes Cartograph
writers with a private lock, and aborts if the config bytes changed after the
proof was made. Custom setup clears retained credentials automatically when the
provider/endpoint origin changes; `--clear-credentials` is the explicit
same-origin removal path and conflicts with `--api-key-env` and
`--api-key-command`. `--api-key-command` stores a credential helper's argv
(repeat `--api-key-arg` for each argument) instead of a variable name; the
serving process runs it without a shell on the tier's first use, and the two
sources are mutually exclusive. Every preset that accepts `--api-key-env`
accepts it; the MCP `cartograph_admin` `llm-apply` action takes the same argv as
`apiKeyCommand`. See [credential sources](CONFIGURATION.md#credential-sources).
`doctor` and `llm smoke` run a configured command and report only whether it
produced a credential. `--jev-features`
applies only to the Jev preset and writes the decision tier's `features` list;
without it an existing list is preserved and a new tier permits exploration
only. Backend cleanup
considers only generated-name rotated logs and
invalid current-version PID state; it preserves current logs, valid or active
processes, and state written by an unsupported newer/older format. Cleanup JSON
exposes only validated project-relative entry names and stable path-free reason
codes; unsafe/control-character names are never echoed.

## Database selection

External database settings are environment-only:

```sh
export CARTOGRAPH_DATABASE_URL='postgresql://cartograph:secret@127.0.0.1:5432/cartograph'
export CARTOGRAPH_DATABASE_SCHEMA='cartograph_project'
export CARTOGRAPH_DATABASE_MAX_CONNECTIONS=8
export CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS=5000
```

The URL is secret and must not be committed or echoed. Without an external URL,
the project-local managed credential is resolved for a database started by
`cartograph db start`.

## Exit and error behavior

Invalid inputs, missing capabilities, stale/lost lease fences, unavailable
database/source/Git state, and operation failures return nonzero. Public errors
omit database URLs, query text, source literals, and absolute project paths.
Machine consumers should inspect JSON fields and stable MCP error codes rather
than parse human prose.
