# Agent-assisted installation

[Documentation home](README.md) · [Project overview](../README.md) ·
[Language matrix](SUPPORT-MATRIX.md) · [Troubleshooting](TROUBLESHOOTING.md)

Use this guide when a coding agent should own setup from installation through
live transport verification. For a manual setup, start with the project
[quick start](../README.md#quick-start).

**On this page:** [Copy-paste task](#copy-paste-task) ·
[Install the native executable](#install-the-native-executable) ·
[Upgrade an existing installation](#upgrade-an-existing-installation) ·
[Database bootstrap](#database-bootstrap) ·
[Index a large repository](#index-a-large-repository) ·
[Register an agent host](#register-an-agent-host) ·
[Optional LLM capabilities](#optional-llm-capabilities) ·
[What v2 does not include](#what-v2-does-not-include)

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
archive against the release `SHA256SUMS`.

| Platform | Supported native release |
| --- | --- |
| macOS | macOS 26 on Apple Silicon |
| Linux | Current Linux with glibc 2.41 or newer on arm64/x64 |
| Windows | Windows 11 25H2 or newer, and Windows Server 2025 or newer |
| Unsupported | Intel macOS and every 32-bit target |

Cartograph v2 has no TypeScript/Bun or SQLite runtime.

### Build from source

Building from source requires the pinned Rust toolchain from
`rust-toolchain.toml` and a C compiler: the Tree-sitter runtime, every native
grammar crate (the `arborium-*` and `tree-sitter-*` families), `ring`, and
`blake3` compile C sources through `cc`. On Windows, use the MSVC toolchain.

```sh
git clone https://github.com/adder-factory/cartograph.git /tmp/cartograph
cd /tmp/cartograph
cargo build --locked --release -p cartograph-cli
install -m 0755 target/release/cartograph "$HOME/.local/bin/cartograph"
```

## Upgrade an existing installation

From an already initialized project, use the single resumable upgrade rather
than replacing the binary or editing host configuration by hand:

```sh
cartograph upgrade --apply --project-path . --json
```

The operation verifies and smoke-tests the release, applies safe schema
migrations, reconciles a current generation, runs `doctor`, and repairs stale
owned host pins.

1. Require `completed: true`. `completed` does not require a fresh index.
2. Read `projectReconciliation.state` and `projectReconciliation.retryable`,
   then act on the table below.
3. If it reports that the managed database must be replaced, perform only the
   exact backup and confirmed upgrade steps it prints, then rerun the same
   command.
4. Reopen the agent host only when `restartRequired` is true; a process already
   attached to an older MCP child cannot hot-load the new binary.

| Report value | Meaning | What to do |
| --- | --- | --- |
| `completed: true` | The upgrade finished. It does not require a fresh index. | Check `projectReconciliation.state`. |
| `projectReconciliation.state: ready` | Every step passed and status is fresh. | Nothing further. |
| `projectReconciliation.state: source_changed` | The installed binary published a complete generation and only later edits make it stale. | Run `cartograph index .` once edits pause instead of rerunning the upgrade. |
| `projectReconciliation.state: blocked` | A step stopped the reconciliation; a step reported as `not_run` was skipped because an earlier step stopped it. | Rerun the same command when `retryable` is true; otherwise follow the reported step and next-step messages. |
| `projectReconciliation.retryable: true` | Rerunning is the next step, for example after `index.state: another_writer_active` or `timed_out`, or a `blocked` verification because another writer replaced the generation this upgrade published or confirmed. | Rerun the same command. |
| `another_writer_active`, or a replaced generation | Another writer was active or replaced the generation. | Wait until the other writer finishes, then rerun the same command. |
| `timed_out` | A bounded wait or timeout ended without a failure verdict. A database `timed_out` step is a cold-pull/readiness timeout. | Rerun the same command; a timeout is not permission to replace a container. |
| `reason: schema_busy` on the database or index step | Another Cartograph process (often an MCP server started from an older binary) held the schema's PostgreSQL locks. | Restart or stop it before rerunning. |
| `restartRequired: true` | This invocation replaced the binary or repaired a host pin. | Reopen the agent host. |
| `commandState: wrapped` | A registration launches Cartograph through a wrapper. | Nothing: it keeps its wrapper, arguments, and `env`; only its embedded absolute Cartograph path is repinned, and `registrationRepair.changes` lists every changed entry. |

<details>
<summary>Details: managed-database replacement and restart semantics</summary>

- If the database command fails after attempting the extension update, the new
  image remains the resumable candidate and the old image remains stopped; do
  not start the old container by hand.
- An interruption after the old container is renamed but before the new
  candidate exists is resumed by repeating the same confirmed command; do not
  rename the rollback slot manually.
- `restartRequired` describes changes made by the current invocation; `false`
  on a later no-op rerun does not prove that a host left open across an earlier
  upgrade loaded the replacement child.

</details>

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
using the pinned upstream ParadeDB 0.26.0 image. PostgreSQL 18.4 or newer within
major version 18, `pg_search` 0.26.0, pgvector 0.8.4 or newer, preload, ParadeDB
index access, BM25, and source-code tokenization are hard checks. External
administrators create pgvector before `pg_search`. The managed image bundles
pgvector 0.8.6; external PostgreSQL installations should use pgvector 0.8.7.

Newly created managed containers have explicit resource ceilings and this WAL
policy:

| Setting | Value |
| --- | --- |
| Memory ceiling | 2 GiB |
| CPU ceiling | Four CPUs |
| Process ceiling | 256 processes |
| Checkpoint interval | 15 minutes |
| Soft maximum WAL size | 2 GiB (`max_wal_size=2GB`) |
| Recycled-WAL floor | 256 MiB (`min_wal_size=256MB`) |
| WAL compression | `wal_compression=lz4` |

The checkpoint interval, soft maximum WAL size, and recycled-WAL floor bound
repeated checkpoint pressure during indexing bursts. WAL left after a busy
period tracks the soft maximum and an idle database does not checkpoint it
away, so that cap is also each project's steady-state WAL footprint; lz4
compression offsets the extra full-page images a smaller cap causes. `cartograph db status` and `doctor` expose whether an older owned
container needs the confirmed backup-and-upgrade path to adopt that policy.

For external PostgreSQL, create both extensions and pass secrets through the
process environment:

```sh
export CARTOGRAPH_DATABASE_SCHEMA='cartograph_project'
# CARTOGRAPH_DATABASE_URL must already be loaded from the shell or secret manager.
cartograph doctor .
cartograph index .
```

> [!WARNING]
> Never write the URL into a committed config. See
> [PostgreSQL storage and operations](STORAGE-BACKENDS.md).

## Index a large repository

Leave `generationStorage` at its default `auto` for the first run. Cartograph
automatically selects PostgreSQL spill at any of these thresholds:

- 10,000 supported files;
- 64 MiB of indexed source;
- 64 Cargo manifests;
- its conservative 16x source-expansion estimate reaching
  `maxGenerationBytes`.

A dense smaller repository can force the same path in
`.cartograph/config.json`:

```json
{
  "version": 2,
  "generationStorage": "postgres"
}
```

The spill path parses lazily in work items of at most 64 files and 64 MiB of
combined source, reuses parsers by language, and publishes extraction,
resolution, and reduction progress without weakening facts or references.
PostgreSQL spill bounds bulky per-file state, but compact project-wide
resolution, clone, and centrality structures remain bounded.

> [!IMPORTANT]
> Do not raise memory or spill limits before the reported stage names a
> capacity boundary and host/database headroom has been measured.

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

It pins the absolute native executable, writes project-scoped MCP
configuration, and can install managed Git hooks. A local install also adds
the project files it wrote to `.gitignore`. For Claude Code, the project-scoped
entry is stored in a home-directory file,
`projects["<absolute project path>"].mcpServers.cartograph` in
`~/.claude.json`, next to project files `CLAUDE.local.md`,
`.claude/skills/cartograph/SKILL.md`, and (unless `--no-permissions`)
`.claude/settings.local.json`.

- **Managed database port.** A local install without `CARTOGRAPH_DATABASE_URL`
  resolves the port itself and pins that non-secret loopback port in the
  generated server arguments for every host format. An explicit
  `--managed-database-port <PORT>` or `CARTOGRAPH_MANAGED_DATABASE_PORT` wins;
  otherwise the installer discovers the published port of this project's
  existing managed container; otherwise it uses the default 55432. Add
  `--managed-database-port <PORT>` only to choose a non-default port before
  the container exists. A selected port that differs from an existing
  container's published port fails and names the discovered port.
- **Hooks.** Use `--no-hooks` to omit hook changes. Run
  `cartograph install-hooks --remove` to remove only Cartograph-owned blocks.
- **Re-running.** Re-running the installer over an existing entry keeps `env`,
  `cwd`, other host-specific keys, and extra server arguments. An entry that
  wraps Cartograph in another launcher (for example a secret manager that
  supplies an `apiKeyEnv` credential) is never replaced; only its absolute
  Cartograph path is repinned.

> [!IMPORTANT]
> Restart the host after registration or binary replacement. A shell `status`
> call validates the CLI/database path; it does not prove that an
> already-running MCP process was replaced.

## Optional LLM capabilities

Exact lookup, ParadeDB BM25, graph traversal, affected tests, review, structural
summaries, roles, and code-health analysis work without an LLM. Embeddings,
reranking, generated summaries/classifications, `ask`, local chat, and Jev
retrieval decisions are optional tiers.

```sh
cartograph llm setup
cartograph backend start .       # only for configured local llama-server tiers
cartograph llm smoke .
cartograph doctor .
```

| Tier | Supported providers |
| --- | --- |
| Chat tiers (summaries, classification, `ask`, local chat) | OpenAI-compatible HTTP, Anthropic Messages API, the bounded local Claude CLI bridge, and the generic bounded shell-free `cli-bridge` (`cartograph llm setup --preset cli-bridge`) |
| Embedding and reranker | OpenAI-compatible HTTP |
| Optional Jev decision tier | Typesafe Jev (`cartograph llm setup --preset jev`) |

Credentials should be resolved from environment variables or a credential
command, not stored inline. See
[credential sources](CONFIGURATION.md#credential-sources).

It is valid to configure only embeddings and reranking. Intentionally absent
summarize, ask, local-chat, and classification tiers are reported as skipped by
`llm smoke` and do not make doctor unhealthy.

### Optional Jev retrieval navigation

For optional Jev retrieval navigation, use
`cartograph llm setup . --preset jev --api-key-env TYPESAFE_API_KEY` and provide
that key in the MCP host's environment, or let the server fetch it on first use
with `cartograph llm setup . --preset jev --api-key-command <exe> --api-key-arg <arg>`
so the host registration stays a plain `cartograph serve --mcp`.

- `doctor` warns, without becoming unhealthy, when the configured variable is
  unset in its own shell; it runs a configured credential command and reports
  whether it produced a key.
- `cartograph_explore` then permits bounded parallel Jev decisions over the
  question and source evidence; `decision: "native"` bypasses the provider.
- An absent or unavailable Jev tier preserves native retrieval.

See [configuration](CONFIGURATION.md#optional-jev-navigation) for disclosure,
limits and fallback reporting.

## What v2 does not include

The browser visual-graph viewer is the only v1 capability not present in v2.
Typed graph data, paths, impact, similarity, JSON/DOT/Mermaid/Cytoscape export,
and SCIP interchange remain available to agents and tools.
