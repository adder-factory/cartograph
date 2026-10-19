# AGENTS.md — install and use Cartograph v3 from an AI coding agent

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
> to run `cartograph admin init -i` to build a code knowledge graph?

## Architecture and hard requirements

This checkout replaces PostgreSQL completely with per-project RocksDB 11.8.1.
Version `v3.0.0` ships this native backend. Existing v2 installations need
the v3 executable, a source rebuild, and host restart before using RocksDB.
Read `AGENTS.local.md` for development gates. Its historical PostgreSQL-specific
requirements are superseded by this authorized native migration.

Cartograph remains a native Rust executable with deterministic extraction,
generation provenance, exact/code-aware BM25/graph retrieval, and optional
model-scoped semantic evidence. No SQL runtime, PostgreSQL fallback, Docker
bootstrap, SQLite path, or SQL query tool remains.

## Build, index, and verify

```sh
cargo build --locked --release -p cartograph-cli
target/release/cartograph index /absolute/path/to/project
target/release/cartograph doctor /absolute/path/to/project --format json
target/release/cartograph status /absolute/path/to/project --format json
target/release/cartograph context 'explain the primary request flow' \
  --project-path /absolute/path/to/project
```

A C/C++ compiler and libclang are required to build native dependencies.
Require a complete fresh generation and nonempty real query evidence. An
unchanged second index should skip publication. Doctor's native capability,
index, freshness, model, registration, and transport dimensions remain separate.

`cartograph db migrate --to rocksdb --project-path .` is a verified source
rebuild and migration-marker update. It preserves legacy PostgreSQL resources
and does not import their user-authored state. Do not delete legacy databases,
containers, volumes, or live sessions as part of a migration.

## Storage ownership and MCP

Each project owns `.cartograph/data`. One primary process owns it; macOS/Linux
CLI and MCP clients reuse a running MCP primary through private same-user IPC.
Windows currently requires closing the other primary before opening that
project in a new process. Never bypass ownership by deleting locks.

Install the intended built executable with `cartograph install --yes --target
codex --location local --project-path .` (or target `claude` / `cursor`).
Restart the host after replacing its pinned executable. Stdio profiles are
`coding`, `core`, `full`, `read-only`, and `review`.

## Maintenance and limits

Use `cartograph db usage --project-path . --format json` before attributing
space. Explicit bounded pruning uses `cartograph db prune --project-path .
--maximum-deletions 4 --confirm prune-old-generations --format json`.
Current readers stay protected, deferred work remains resumable, and native
compaction may reclaim files after logical deletion. No online backup/restore
CLI has been implemented for this cutover.

Storage calls use bounded dedicated workers, deadlines, cancellation, retained
completion, WAL sync, immutable snapshots, changed-record writes, and packed
adjacency. Rust async does not imply RocksDB native AsyncIO. BM25 uses
generation-scoped postings with a bounded scan fallback; cosine retrieval
scans bounded vectors. Capacity exhaustion stays explicit. Require paired
measurements before claiming PostgreSQL speedups.

See [storage](docs/STORAGE-BACKENDS.md), [migration](docs/v2/ROCKSDB-MIGRATION.md),
[architecture](docs/v2/ARCHITECTURE.md), and [configuration](docs/CONFIGURATION.md).

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
reports the protected platform and RocksDB matrix contexts without starting
their expensive work, and emits no release attestation. `workflow_dispatch`
always runs the complete gate and must be used before releasing an exact `main`
SHA that has only documentation-only validation.

The native workflow additionally proves RocksDB atomic publication, snapshot
isolation, retention, cancellation/reaping, hostile-input handling, semantic
readiness, owner IPC, CLI faults, and frozen 1/2/4/8/16-worker fact identity.
An exact-SHA complete main run emits a GitHub-attested gate manifest. Stable
releases still require independent review, Sonar, four archive smokes,
checksums/provenance, and a signed tag equal to published main.
