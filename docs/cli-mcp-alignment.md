# CLI and MCP alignment

[Documentation home](README.md) · [CLI reference](CLI-REFERENCE.md) ·
[MCP usage](MCP-USAGE.md) · [Project overview](../README.md)

Last release audit: 2026-10-02 (`v2.1.36`).

Cartograph exposes one native Rust feature surface through human CLI commands
and 36 bounded MCP tools. Shared schemas generate ordinary CLI adapters where
possible; hand-curated family commands retain deliberate positional and
subcommand shapes.

The v1.1.33 public contracts are frozen in:

- `crates/cartograph-cli/src/v1_1_33_mcp_contract.json`;
- `crates/cartograph-cli/src/v1_1_33_cli_contract.json`.

Permanent Rust tests verify every v1 MCP tool/property/required field/type/
enum/bound and every v1 CLI command/alias/option/positional. The only product
capability exemption is the browser visual-graph viewer. The only storage-
contract exemptions are the intentionally removed SQLite provider and
pgvector-off mode; PostgreSQL graph functionality remains.

## Public MCP tools

<!-- CARTOGRAPH_MCP_TOOLS_START -->

```text
admin              affected            ask
at_range           biomarkers          blame
changed_since      compare_to_ref       context
coverage           dead_code            deps
digest             entry_points         explore
files              find                 graph
history            host                 hotspots
imports            node                 note
numerical          playbook             propose_rename
review             role                 session
sql                status               summaries
tests_for          trace_to_culprits    verify
```

<!-- CARTOGRAPH_MCP_TOOLS_END -->

Each appears on the wire with the `cartograph_` prefix. Profiles (`coding`,
`core`, `full`, `read-only`, and `review`) deliberately expose subsets; a
hidden tool cannot be called by name.

## Family mappings

Mode/subcommand families preserve one coherent tool instead of multiplying MCP
startup schemas:

| MCP family | CLI family examples |
| --- | --- |
| `cartograph_admin` | `admin init/index/sync/summarize/embed/classify/scip-export/scip-import/...` |
| `cartograph_find` | `find --by name/path/reference/bm25/hybrid` plus source/env/SQL/build-context modes |
| `cartograph_graph` | `graph --direction callers/callees/both/impact/path/similar` |
| `cartograph_files` | `files --format tree/flat/grouped/summary/deps/symbols/module/read` |
| `cartograph_numerical` | `numerical sites/coverage/explain/plan` |
| `cartograph_review` | `review context/neighbors/risk/agent-audit/numerical/trust` |
| `cartograph_session` | investigation, audit/usage, and macro subcommands |
| `cartograph_summaries` | `summaries pending/save` |

The native CLI retains all v1 family actions and adds v2 PostgreSQL generation,
retention, embedding-readiness, SCIP, and agent-workflow operations.

## Intentional CLI-only operations

- `serve` is the MCP transport itself.
- `install`, `uninstall`, and `install-hooks` mutate host/repository setup.
- `db` owns local database lifecycle and destructive confirmation boundaries.
- `llm setup/smoke` and `backend` are operator configuration/process checks.
- `doctor`, `guide`, `mcp-budget`, `completions`, and `upgrade` are operator
  workflows; MCP uses admin/playbook/status equivalents where appropriate.
- `sync-if-dirty` is the Git-hook compatibility entry point.
- `similar` is an ergonomic shortcut for graph direction `similar`.

These do not hide a coding capability from MCP. Long MCP admin operations use
bounded jobs with explicit status/cancel instead of blocking transport.
Biomarker reads are non-mutating on both surfaces; the explicit
`biomarkers-refresh` admin action is dry-run-first and requires confirmation to
compute the generation-fenced relation. Its `databaseQueryTimeoutMs` field and
generated `--database-query-timeout-ms` flag control the same inner PostgreSQL
statement timeout through 30 minutes; `timeoutMs` is retained only as an
exclusive legacy alias. Generated CLI execution extends its caller deadline
beyond the selected statement timeout. Dead-code statement timeouts use the
same typed error on CLI and MCP, while digest preserves successful sections and
labels an incomplete section independently.
Non-recoverable file-local index errors share the same typed project-relative
path and fixed reason across both surfaces: direct CLI JSON uses
`error.file_failure`, while admin job status uses `fileFailure`. A failed
cleanup of the attempt's own staging generation is the same secondary
`code`/`message` object beside the primary failure: `error.cleanup_failure` in
direct CLI JSON and `cleanupFailure` in admin job status. A SCIP import whose
overlay restore also failed adds the same-shaped `overlayRollbackFailure` to
that one job status, which CLI `admin scip-import` and MCP `scip-import` share.
Invalid spans and non-cancelled parser stops instead publish an empty partial
file with a stable degraded reason. Generation-capacity failures name
`maxGenerationBytes`, its Cartograph-process scope, and a bounded next action in
direct CLI output and MCP `failureDetail`. Text rendering escapes control
characters, and neither surface accepts arbitrary parser/driver text at that
boundary.
Auto-sync status additionally exposes its cross-revision capacity failure count,
circuit state, and the same limit/scope/next action. `serve --no-auto-sync`
provides the CLI process-lifetime recovery boundary; it does not mutate the MCP
authorization profile or create a task-local tool mode.

## Verification

Run the actual native surfaces:

```sh
cargo test --locked -p cartograph-cli \
  v1_1_33_cli_tree_remains_available_except_browser_viewer
cargo test --locked -p cartograph-cli \
  v1_1_33_mcp_input_contract_remains_accepted_except_retired_storage_modes
cargo test --locked -p cartograph-mcp
target/debug/cartograph --help
target/debug/cartograph serve --help
```

Then run the full workspace and live PostgreSQL/ParadeDB gates. Contract tests
catch accepted input shape; focused handler/live tests must still prove that
every mode consumes its arguments and produces real behavior.
