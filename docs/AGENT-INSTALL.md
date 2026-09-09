# Agent-assisted installation

[Documentation home](README.md) · [Project overview](../README.md) ·
[Language matrix](SUPPORT-MATRIX.md) · [Troubleshooting](TROUBLESHOOTING.md)

Use this guide when a coding agent should own setup from installation through
live transport verification. For a manual setup, start with the project
[quick start](../README.md#quick-start).

## Copy-paste task

Give the following task to a coding agent from the repository Cartograph should
index:

```text
Install Cartograph v2 for this repository and register its native MCP server.

1. If `cartograph --version` is unavailable, use the checksum-verifying native
   installer from the Cartograph release. Do not install Bun or an npm package.
2. On macOS/Linux with a local Docker daemon, run:
   cartograph db start --project-path .
   cartograph doctor .
   Otherwise configure an external PostgreSQL 18 database with pg_search and
   pgvector through CARTOGRAPH_DATABASE_URL, then run doctor.
3. Run `cartograph index .` followed by `cartograph status .` and one real
   `cartograph context` query.
4. Run `cartograph install --yes --target <current-host> --location local
   --project-path .`.
5. Report the exact files changed, database ownership, doctor result, current
   generation/freshness, real retrieval result, and whether the host must be
   restarted.

Do not print or commit database/API credentials. Do not claim MCP health from
CLI success alone; restart the host and make a live MCP call when possible.
```

## Install the native executable

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

The installers select one of the four 64-bit release platforms and verify the
archive against the release `SHA256SUMS`. Supported native releases are macOS
26 on Apple Silicon, current Linux with glibc 2.41 or newer on arm64/x64,
Windows 11 25H2 or newer, and Windows Server 2025 or newer. Intel macOS and
every 32-bit target are unsupported.
Building from source requires only the pinned
Rust toolchain:

```sh
git clone https://github.com/adder-factory/cartograph.git /tmp/cartograph
cd /tmp/cartograph
cargo build --locked --release -p cartograph-cli
install -m 0755 target/release/cartograph "$HOME/.local/bin/cartograph"
```

Cartograph v2 has no TypeScript/Bun or SQLite runtime.

## Upgrade an existing installation

From an already initialized project, use the single resumable upgrade rather
than replacing the binary or editing host configuration by hand:

```sh
cartograph upgrade --apply --project-path . --json
```

Require `completed: true`. The operation verifies and smoke-tests the release,
applies safe schema migrations, reconciles a fresh current generation, runs
`doctor`, and repairs stale owned host pins. If it reports that the managed
database must be replaced, perform only the exact backup and confirmed upgrade
steps it prints, then rerun the same command. If the database command fails
after attempting the extension update, the new image remains the resumable
candidate and the old image remains stopped; do not start the old container by
hand. An interruption after the old container is renamed but before the new
candidate exists is resumed by repeating the same confirmed command; do not
rename the rollback slot manually. Reopen the agent host only when
`restartRequired` is true; a process
already attached to an older MCP child cannot hot-load the new binary. That flag
describes changes made by the current
invocation; `false` on a later no-op rerun does not prove that a host left open
across an earlier upgrade loaded the replacement child.

## Database bootstrap

The normal local macOS/Linux path is:

```sh
cartograph db start --project-path .
cartograph doctor .
cartograph index .
cartograph status .
cartograph context 'explain the primary request flow' --project-path .
```

The managed lifecycle creates project-owned, loopback-only Docker resources
using the pinned upstream ParadeDB 0.25.6 image. PostgreSQL 18.4 or newer within
major version 18, `pg_search` 0.25.6, pgvector 0.8.4 or newer, preload, ParadeDB
index access, BM25, and source-code tokenization are hard checks. External
administrators create pgvector before `pg_search`. Newly created containers
also have explicit 2 GiB memory, four-CPU, and 256-process ceilings;
their 15-minute checkpoint interval, 4 GiB soft maximum WAL size, and 512 MiB
recycled-WAL floor bound repeated checkpoint pressure during indexing bursts.
`cartograph db status` and `doctor` expose whether an older owned container needs
the confirmed backup-and-upgrade path to adopt that policy.

For external PostgreSQL, create both extensions and pass secrets through the
process environment:

```sh
export CARTOGRAPH_DATABASE_SCHEMA='cartograph_project'
# CARTOGRAPH_DATABASE_URL must already be loaded from the shell or secret manager.
cartograph doctor .
cartograph index .
```

Never write the URL into a committed config. See
[PostgreSQL storage and operations](STORAGE-BACKENDS.md).

## Index a large repository

Leave `generationStorage` at its default `auto` for the first run. Cartograph
automatically selects PostgreSQL spill at 10,000 supported files, 64 MiB of
indexed source, 64 Cargo manifests, or when its conservative 16x
source-expansion estimate reaches `maxGenerationBytes`. A dense smaller
repository can force the same path in
`.cartograph/config.json`:

```json
{
  "version": 2,
  "generationStorage": "postgres"
}
```

The spill path parses lazily in work items of at most 64 files and 64 MiB of
combined source, reuses parsers by language, and publishes extraction,
resolution, and reduction progress without weakening facts or references. Do
not raise memory or spill limits before the reported stage names a capacity
boundary and host/database headroom has been measured. PostgreSQL spill bounds
bulky per-file state, but compact project-wide resolution, clone, and centrality
structures remain bounded.

Treat parser completion and complete graph publication as different timings.
The published
[large-corpus streaming benchmark record](v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md)
keeps both timings, maximum RSS, canonical digest, full fact counts, and an
unchanged-source no-op with its exact pre-release provenance. See
[performance tuning](PERF-TUNING.md) and
[capacity troubleshooting](TROUBLESHOOTING.md#native-generation-reaches-its-capacity-bound)
before changing defaults.

## Register an agent host

The installer supports 19 host targets and preserves unrelated configuration.
Common examples:

```sh
cartograph install --yes --target codex --location local --project-path .
cartograph install --yes --target claude --location local --project-path .
cartograph install --yes --target cursor --location local --project-path .
```

It pins the absolute native executable, writes project-local MCP configuration,
and can install managed Git hooks. If the managed database uses a non-default
port, add `--managed-database-port <PORT>`; the generated server arguments pin
that non-secret loopback port for every host format. Use `--no-hooks` to omit
hook changes. Run `cartograph install-hooks --remove` to remove only
Cartograph-owned blocks.

Restart the host after registration or binary replacement. A shell `status`
call validates the CLI/database path; it does not prove that an already-running
MCP process was replaced.

## Optional LLM capabilities

Exact lookup, ParadeDB BM25, graph traversal, affected tests, review, structural
summaries, roles, and code-health analysis work without an LLM. Embeddings,
reranking, generated summaries/classifications, `ask`, and local chat are
optional tiers.

```sh
cartograph llm setup
cartograph backend start .       # only for configured local llama-server tiers
cartograph llm smoke .
cartograph doctor .
```

Supported chat providers are OpenAI-compatible HTTP, Anthropic Messages API,
and the bounded local Claude CLI bridge. Embedding and reranker tiers use
OpenAI-compatible HTTP. Credentials should be resolved from environment
variables, not stored inline.

It is valid to configure only embeddings and reranking. Intentionally absent
summarize, ask, local-chat, and classification tiers are reported as skipped by
`llm smoke` and do not make doctor unhealthy.

The browser visual-graph viewer is the only v1 capability not present in v2.
Typed graph data, paths, impact, similarity, JSON/DOT/Mermaid/Cytoscape export,
and SCIP interchange remain available to agents and tools.
