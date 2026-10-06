# AGENTS.md — install and use Cartograph v2 from an AI coding agent

Cartograph is this workspace's native code-intelligence MCP server. These
instructions are deliberately mechanical: execute a step, verify its result,
then continue.

> Working on the Cartograph repository itself? Read `AGENTS.local.md` before
> changing code. It contains the private development conventions, architecture
> gates, Sonar workflow, reviewer step, and release procedure. A fresh public
> clone does not contain that git-ignored file.

## Existing project rule

If `.cartograph/` or a project-local Cartograph MCP registration exists, use
Cartograph for relationship-aware exploration:

- `cartograph context` for broad coding-task evidence;
- `cartograph find` for exact name/path/reference or BM25 lookup;
- `cartograph graph` for callers, callees, and reverse impact;
- `cartograph affected` for test selection;
- `cartograph review` for compare-to-ref and dirty-worktree evidence;
- `cartograph status` before relying on the index.

If the index is stale, run `cartograph index <project>` or call the MCP
`cartograph_admin` index action. Do not present stale evidence as current.

If the project has no Cartograph setup, ask:

> I notice this project does not have Cartograph initialized. Would you like me
> to start its PostgreSQL/ParadeDB database, index it, and add project-local MCP
> configuration?

## Architecture and hard requirements

Cartograph v2 is:

- a native Rust executable;
- PostgreSQL 18.4 or newer within major version 18;
- code-aware BM25 through ParadeDB `pg_search` 0.26.0;
- pgvector 0.8.4 or newer (0.8.7 recommended for external PostgreSQL; the
  managed ParadeDB 0.26.0 image bundles `pg_search` 0.26.0 and pgvector 0.8.6);
- useful without an LLM through exact, lexical, graph, review, and test-impact
  retrieval.

There is no SQLite runtime, importer, optional feature, or fallback. Never
recommend SQLite for v2 and never attempt to open an old SQLite graph.

## Step 0 — install the binary

macOS/Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/adder-factory/cartograph/main/install.sh | sh
cartograph --version
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/adder-factory/cartograph/main/install.ps1 | iex
cartograph --version
```

The installer verifies the selected native archive against the release
`SHA256SUMS`. If installation from source is required:

```sh
git clone https://github.com/adder-factory/cartograph.git
cd cartograph
cargo build --locked --release -p cartograph-cli
```

The pinned Rust toolchain is loaded from `rust-toolchain.toml`. A C compiler
is also required (MSVC on Windows): the Tree-sitter runtime, the native grammar
crates, `ring`, and `blake3` compile C sources through `cc`.

## Step 1 — choose database ownership

### Managed local database (macOS/Linux)

Use this when the user has a local Docker daemon and wants the normal local
developer experience:

```sh
cd /absolute/path/to/project
cartograph db start --project-path .
cartograph doctor .
```

`db start` must report a private credential, a project-owned container/volume,
the migrated schema version, and passing hard checks for PostgreSQL 18,
`pg_search`, pgvector, preload, BM25, and code tokenization.

Cartograph pins a local Docker endpoint and binds PostgreSQL to loopback. It
refuses foreign resources with colliding names. Do not weaken those checks.
New managed containers also use the documented 2 GiB memory, four-CPU,
256-process, bounded worker/memory, and bounded WAL/checkpoint policy (see
`docs/AGENT-INSTALL.md`); older containers are reported rather than silently
replaced.

### External PostgreSQL (all supported platforms)

The database administrator must install PostgreSQL 18.4 or newer within major
version 18, `pg_search` 0.26.0, and pgvector 0.8.4 or newer, then create pgvector
before creating `pg_search`. Supply the URL through the environment:

```sh
export CARTOGRAPH_DATABASE_URL='postgresql://<user>:<password>@127.0.0.1:5432/cartograph'
export CARTOGRAPH_DATABASE_SCHEMA='cartograph'
cartograph doctor /absolute/path/to/project
```

Never write the database URL into committed project files or echo it in chat.
Cartograph intentionally redacts it from errors.

Windows release binaries support external PostgreSQL. The managed lifecycle is
not enabled there until credential ACL handling can prove equivalent privacy.

## Step 2 — index and verify

```sh
cartograph index /absolute/path/to/project
cartograph status /absolute/path/to/project
cartograph context 'explain the primary request flow' \
  --project-path /absolute/path/to/project
```

A valid first index publishes one complete generation. Running the same index
again should report an unchanged/no-op generation. If source changes,
`cartograph status` must say stale until a new complete generation publishes.

Do not call a setup ready solely because files or a container exist. Require:

1. `cartograph doctor` succeeds;
2. `cartograph index` succeeds;
3. `cartograph status` reports a fresh current generation;
4. one real `find` or `context` query returns generation-scoped evidence.

## Step 3 — install MCP configuration

Cartograph writes project-scoped configuration and pins the absolute native
executable. Most targets write files inside the project; Claude Code's
project-scoped entry is stored in a home-directory file:

```sh
# OpenAI Codex: .codex/config.toml
cartograph install --yes --target codex --location local \
  --project-path /absolute/path/to/project

# Claude Code: ~/.claude.json, under projects["<absolute project path>"].mcpServers.cartograph
cartograph install --yes --target claude --location local \
  --project-path /absolute/path/to/project

# Cursor: .cursor/mcp.json
cartograph install --yes --target cursor --location local \
  --project-path /absolute/path/to/project
```

For Claude Code the installer also writes project files `CLAUDE.local.md`,
`.claude/skills/cartograph/SKILL.md`, and, unless `--no-permissions` is passed,
`.claude/settings.local.json`. A local install adds the project files it wrote
to `.gitignore`.

The installer preserves unrelated MCP entries and refuses a symlink/non-file or
oversized configuration. Restart the agent host after the write.

Manual MCP specification:

```json
{
  "command": "/absolute/path/to/cartograph",
  "args": [
    "serve",
    "--mcp",
    "--project-path",
    "/absolute/path/to/project"
  ]
}
```

Use stdio transport. Profiles are `coding`, `core`, `full`, `read-only`, and
`review`. They are process-lifetime authorization ceilings, not mutable
task-local tool sets. Modern MCP `2026-07-28` clients use stateless
`server/discover` and per-request metadata; legacy clients retain the
`2024-11-05` initialize path. Hosts may dynamically defer task-irrelevant tool
schemas, but the server's authorization-scoped `tools/list` remains stable and
deterministic as required by modern MCP.

## Step 4 — use Cartograph while coding

Route questions by evidence type:

| Need | Command/tool |
| --- | --- |
| Broad task context | `cartograph context` / `cartograph_context` |
| Exact declaration/path/reference | `cartograph find` / `cartograph_find` |
| Lexical code relevance | `find --by bm25` |
| Call or dependency shape | `cartograph graph` / `cartograph_graph` |
| Tests likely affected | `cartograph affected` / `cartograph_affected` |
| Numerical hazards and static coverage | `cartograph numerical` / `cartograph_numerical` |
| Review current changes | `cartograph review --ref main` / `cartograph_review` |
| Freshness and counts | `cartograph status` / `cartograph_status` |
| Explicit refresh | `cartograph index` / `cartograph_admin` |

Cartograph evidence narrows inspection; it does not replace reading the exact
changed source or running the project test suite. Preserve confidence,
freshness, truncation, and abstention fields in agent reasoning.

## Managed database operations

Read-only/idempotent:

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

Restore, upgrade, derived-index rebuild, and remove are destructive or
replacement operations. They require an exact `--confirm` phrase. `--help`
prints the phrase for restore, import-v1, prune, and compact, but not for
upgrade, remove, or derived-index rebuild, so use this table:

| Operation | Confirmation phrase |
| --- | --- |
| `cartograph db restore` | `restore-managed-database` |
| `cartograph db upgrade` | `upgrade-managed-database` |
| `cartograph db remove` | `remove-managed-database` |
| `cartograph db derived-index --rebuild` | `rebuild-managed-derived-indexes` |
| `cartograph db import-v1` (mutation) | `import-v1-postgres` |
| `cartograph db prune` | `prune-old-generations` |
| `cartograph db compact --apply` | `compact-online-indexes`, or `compact-heap-relations` with `--heap` |

Do not infer authorization for these operations from a read-only diagnostic
request.

## V1 migration

V2 imports only from a v1.1.33 PostgreSQL schema. The importer is resumable,
validates stable identities/counts/relations, and rebuilds BM25. It does not
import or inspect SQLite.

The v1 source and v2 destination must be distinct schemas in the same database.
Set `CARTOGRAPH_DATABASE_SCHEMA` to the v2 destination, preserve a database
backup, quiesce index/sync/hook/rebuild writers for the project, and preflight
the exact source checkout before authorizing mutation. Use `--source-checkout`
when that byte-exact historical tree is separate from the initialized current
project; this preserves dirty work without changing destination identity. The
preflight requires the exact v1.1.33-compatible checkout path/content set and
excludes every additive v2 mode (for example `.pyi`, TOML, WGSL/Metal/Slang/WESL
shaders, Ada/VHDL, and the game-scripting modes). Missing, extra, or
substituted v1 files fail before destination mutation. The
destination may already have a current generation; import publishes a new
immutable one:

```sh
cartograph db import-v1 \
  --project-path /absolute/path/to/current-project \
  --source-checkout /absolute/path/to/exact-v1-checkout \
  --source-schema cartograph_v1 \
  --dry-run \
  --format json

cartograph db import-v1 \
  --project-path /absolute/path/to/current-project \
  --source-checkout /absolute/path/to/exact-v1-checkout \
  --source-schema cartograph_v1 \
  --confirm import-v1-postgres \
  --format json
```

An interrupted exact run resumes when the same command is repeated. Never
change schemas or checkout bytes to force a checkpoint forward. Keep the v1
schema and backup until `status`, one real `context` query, and derived-index
health all pass. Run a normal v2 index first when additive v2 file types make
the imported v1 generation correctly report stale. Imported reference site
counts preserve v1 multiplicity; spans
remain explicitly coarse where v1 cannot prove an exact token. SCIP placeholder
hashes cannot prove historical bytes that v1 did not retain.

The error "another Cartograph writer published during v1 import; retry after
it is idle" (internally `ConcurrentPublication`) means another writer published
first. The importer atomically fails/releases its stale generation; with
writers quiesced, repeat the identical confirmed command so it can reset the
failed run and reserve a newer generation.

If a user needs data that exists only in a v1 SQLite index, give two choices:

1. rebuild v2 from the source checkout; or
2. use the v1.1.33 binary to migrate SQLite to PostgreSQL first, then run the v2
   PostgreSQL importer.

Do not install SQLite tooling into v2 to shorten this workflow.

### Generation retention and pruning

Every successful index/no-op reconciliation attempts a small automatic bounded
cleanup, and failed automatic indexing attempts clean terminal generations
before returning:

- It keeps the two newest superseded generations, terminalizes failed
  pre-lease work, and can collect staging rows only after they have been
  unleased for at least ten minutes.
- It can also reconcile ready work only after 24 hours when it is unleased,
  non-current, and not part of incomplete import recovery.
- Cleanup claims eligible generations as `retiring` and drains child rows
  before parents in transactions of at most 10,000 rows. Earlier committed
  batches survive a later timeout; retry the same bounded operation after
  inspecting its `retiring_remaining` and `deferred_reason`. A retiring
  generation cannot admit writers, resume an import, or publish.
- The same lease independently bounds the parse cache by the exact current
  parsing-policy contract plus one older contract, 20,000 rows, 2 GiB logical
  payload, and a 10,000-row deletion batch. Generation cleanup failure still
  permits cache cleanup. Protected current and spill-pinned cache rows can
  exceed those targets.

After backup and import verification, an explicit larger bounded batch is:

```sh
cartograph db prune \
  --project-path /absolute/path/to/checkout \
  --keep-superseded 2 \
  --maximum-deletions 100 \
  --confirm prune-old-generations \
  --format json
```

This preserves the current generation, recent or leased staging/ready work,
incomplete import recovery state, and the two newest superseded generations.

- The requested generation batch can exceed the independent
  64-derived-relation drop cap when failed generations own no derived relation.
- The default search relation byte budget is 8 GiB;
  `--maximum-search-relation-bytes` admits an audited override up to 64 GiB.
  Oversized relations do not block later smaller work.
- Inspect each report before requesting another batch. Use
  `serve --mcp --no-auto-sync` to keep a recovery host quiescent while draining
  a capacity-failure backlog.

### Storage usage and compaction

Use `cartograph db usage --project-path . --format json` before diagnosing
bloat.

- Treat `generationStorage.estimated_retained_bytes` only as its documented
  source-plus-generation-BM25 lower bound; shared fact heaps, B-trees,
  embeddings, and reusable space are accounted by the full storage report.
- Compare parse cache logical, stored, schema-stored, and physical-overhead
  bytes before attributing its allocated TOAST file to live payload.
- `retentionMaintenance` records the latest automatic cleanup outcomes and
  consecutive failures.
- `unattributedDatabaseBytes` is an allocation gap, not proof that files can be
  deleted. Never remove PostgreSQL data files by name.
- `cartograph db compact` is a read-only online B-tree plan by default; apply
  requires `--confirm compact-online-indexes`, verified free-space headroom,
  and rebuilds one index at a time.
- `db compact --heap` is a separate read-only main/TOAST heap plan; its apply
  path requires `--confirm compact-heap-relations`, no live operation leases,
  verified headroom, and one `ACCESS EXCLUSIVE` `VACUUM FULL` rewrite at a
  time.
- Neither mode drops invalid concurrent-reindex artifacts.

## Common failures

### Managed database says Docker is unavailable

Start a local Docker daemon or use external PostgreSQL. Cartograph intentionally
rejects remote Docker contexts for managed secrets/resources.

### PostgreSQL is too old

Cartograph requires PostgreSQL 18. Upgrade the external service or use the
pinned managed image.

### `pg_search`, pgvector, preload, BM25, or tokenizer check fails

Treat this as blocking. Install/enable the exact capabilities and rerun
`cartograph doctor`; never silently reduce the engine to plain PostgreSQL FTS.

### Managed port 55432 is occupied

Choose another loopback port consistently:

```sh
cartograph db start --project-path . --port 55433
export CARTOGRAPH_MANAGED_DATABASE_PORT=55433
cartograph install --yes --target <host> --location local \
  --project-path . --managed-database-port 55433
```

The environment variable selects the port for direct CLI commands. The
installer pins the same non-secret port in the MCP server arguments, so the
agent host does not need host-specific environment configuration. Once the
container exists, direct project commands and a local `install` without
`CARTOGRAPH_DATABASE_URL` discover its published port when neither the flag nor
the environment variable is set; an explicit port that differs fails and names
the discovered port.

### Project has no index

Run `cartograph index <project>`. An MCP configuration alone does not create a
generation.

### Index is stale

Run a bounded index/sync, then re-run the query. Do not call stale evidence
current.

### ParadeDB Community index is unusable after a crash

The local Community BM25 index is derived data. Inspect health, then use the
confirmed `db derived-index --rebuild` path. Relational source-of-truth rows and
backups remain separate.

### Agent cannot start the MCP server after upgrade

Run the single resumable operation from the project checkout:

```sh
cartograph upgrade --apply --project-path <project> --json
```

It installs the verified release when needed, reconciles safe database
migrations and a complete current generation, runs `doctor`, proves status
through the installed binary, and repairs stale owned host pins.

- Require `completed: true`; do not mistake `applied: false` for failure when
  `installedVersion` was already current.
- `projectReconciliation.state: source_changed` is a completed upgrade whose
  checkout was edited after publication; run `cartograph index <project>` once
  edits pause.
- When `projectReconciliation.retryable` is true (`another_writer_active`, a
  `timed_out` step, or a `blocked` verification because another writer
  replaced the generation this upgrade published or confirmed), rerun the same
  command; for `another_writer_active` or a replaced generation, wait until the
  other writer finishes.
- With `reason: schema_busy`, another Cartograph process (often an MCP server
  started from an older binary) held the schema's PostgreSQL locks, so restart
  or stop it first.
- A step reported as `not_run` was skipped because an earlier step stopped the
  reconciliation.
- If `projectReconciliation` requests `upgrade-managed-database`, run only its
  backup and exact confirmed replacement steps, then rerun the same command to
  resume. A failure after the extension update retains the new image for that
  retry and keeps the old image stopped; do not manually restart the old
  container against the possibly newer catalog. An interruption between
  renaming the stopped old container and creating the candidate is also
  resumed by the same confirmed command; do not rename the rollback slot by
  hand.
- Restart or reopen the host only when `restartRequired` is true, because an
  attached process cannot hot-load the new child. That flag describes binary
  or pin changes made by the current invocation; false on a no-op rerun does
  not prove the version of a process left attached across an earlier upgrade.
- Treat a database `timed_out` step as a retryable cold-pull/readiness timeout,
  not as permission to replace a container.
- If the database schema is newer than the loaded binary, use the reported
  binary and supported-schema versions to upgrade before retrying; startup
  fails closed.

## Development and release gates

For repository work, the minimum Rust gates are:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
cargo test --locked --workspace --all-features
cargo deny --all-features check
```

`cargo test` includes the per-language v1 parity oracle
(`crates/cartograph-extract/tests/v1_parity_oracle.rs`). Every frozen v1 fact
requires a unique exact v2 identity, committed exact pins in `alignments.jsonl`,
or a pending/intentional entry in `divergences.jsonl`. These tables live under
`crates/cartograph-extract/tests/fixtures/v1_parity/`; `reasons.jsonl` stores
shared evidence, and `gate_cases.jsonl` and `counterexamples.jsonl` exercise
the same gate. Ambiguous identities never match; stale or redundant rows fail.
Read the failure's candidate suggestions, verify correspondence against the
source and capture, then fix extraction or update the explicit rows. Keep the
captures and corpus unchanged. Only the maintainer-only
`scripts/capture-v1-parity-oracle.sh` creates captures, using the verified
v1.1.33 binary under an isolated `HOME`; v2 and CI never run that binary.
See [the coverage report](docs/LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle)
for the key contract, current disposition counts and update procedure.

These gates use the exact stable toolchain in `rust-toolchain.toml`. Do not use
nightly-only tools or diagnostic overrides to clear them. A separate scheduled
latest-stable canary detects future compiler and Clippy changes without making
production builds float away from the reviewed toolchain.

The required GitHub workflow always starts on pull requests and `main` pushes;
do not add trigger-level path filters because GitHub can leave required checks
pending when an entire workflow is skipped. Its fail-closed classifier selects
a documentation-only path only when every changed path is an approved Markdown
document, fixture README, or reviewer instruction. Empty, mixed, source,
test-code, dependency, workflow, bundled-skill, release-note, and unknown
changes run the complete gate. Documentation-only
validation runs the workflow contract and focused Rust documentation contract,
reports the protected platform and PostgreSQL matrix contexts without starting
their expensive work, and emits no release attestation. `workflow_dispatch`
always runs the complete gate and must be used before releasing an exact `main`
SHA that has only documentation-only validation.

The live workflow additionally proves PostgreSQL/ParadeDB capabilities,
migrations, deterministic COPY/publication, leases, fault handling,
1/2/4/8/16-worker output identity, retrieval, migration, database maintenance,
and cleanup. Its exact-SHA green `main` run emits a GitHub-attested gate
manifest; the tag workflow verifies that evidence rather than rerunning the
live suite. Stable releases still require independent review, Sonar/static
analysis, four native 64-bit archive smokes, checksums, provenance, a signed tag, and
a tag SHA equal to the published `main` head.
