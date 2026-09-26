---
name: cartograph
description: Explore a codebase through Cartograph v2's PostgreSQL code graph: intent-aware context, exact/BM25/hybrid search, callers/callees/impact, affected tests, live source, review, freshness, and bounded index administration.
---

# Cartograph v2

Cartograph is a native Rust MCP server for coding agents. Its durable graph lives
only in PostgreSQL 18 with ParadeDB `pg_search` and pgvector. There is no SQLite
runtime, fallback, migration target, or optional feature.

## Existing project

When `.cartograph/` or a project-local Cartograph MCP registration exists:

1. Call `cartograph_status` before trusting graph evidence.
2. Use `cartograph_context` for a coding task. It classifies a typed intent,
   returns immutable-generation provenance, and keeps any stale working-tree
   overlay visibly separate.
3. Narrow with exact `cartograph_find`, `cartograph_node`, or
   `cartograph_graph` calls.
4. After edits, use `cartograph_review` against the intended base and
   `cartograph_affected` for structurally related tests.
5. Run the repository's real test/quality commands; Cartograph evidence guides
   verification but does not replace it.

Never present stale or unknown-freshness evidence as current. If the index is
stale, use the explicit `cartograph_admin` index/sync action and poll its job
status. Do not delete locks or database state while an owner is alive.

| Tool | Use |
| --- | --- |
| `cartograph_status` | Current generation, counts, and live-source freshness |
| `cartograph_context` | Intent-aware exact/BM25/hybrid evidence, graph context, affected tests, and live overlay |
| `cartograph_find` | Exact name/path/reference, BM25, or hybrid candidates |
| `cartograph_node` | Exact symbol metadata plus bounded source when line provenance is fresh |
| `cartograph_graph` | Bounded callers, callees, or reverse impact |
| `cartograph_affected` | Tests reachable through bounded reverse impact |
| `cartograph_numerical` | Generation-scoped static numerical evidence, coverage, explanation, and non-executing probe plans |
| `cartograph_review` | Git-ref plus staged/unstaged/untracked review evidence |
| `cartograph_admin` | Explicit index/sync/embed job start, status, and cancellation |

The MCP profiles are `coding`, `core`, `full`, `read-only`, and `review`. Use the
narrowest profile that supports the task. MCP requests have hard input/output
limits, deadlines, cancellation, and stable error codes; preserve those errors
rather than retrying an unbounded variant.

If MCP transport is unavailable, say so and use the equivalent native CLI as a
control path. A successful CLI call does not prove an already-open host has
hot-restarted its MCP process.

## New project

If no Cartograph setup exists, ask:

> I notice this project does not have Cartograph initialized. Would you like me
> to start its PostgreSQL/ParadeDB database, index it, and add project-local MCP
> configuration?

For a managed macOS/Linux setup:

```sh
cartograph db start --project-path .
cartograph doctor .
cartograph index .
cartograph install --yes --target codex --location local --project-path .
```

For external PostgreSQL, require PostgreSQL 18.4 or newer within major version
18, `pg_search` 0.25.10, and pgvector 0.8.4 or newer. Create pgvector before
`pg_search`, then pass the secret URL only through `CARTOGRAPH_DATABASE_URL` and
select the schema with `CARTOGRAPH_DATABASE_SCHEMA`. Never write or echo the URL
in committed project files.

Setup is ready only after doctor, index, status/freshness, and one real context
or find query pass. Restart the agent host after writing MCP configuration.

## V1 cutover

V2 imports only a v1.1.33 PostgreSQL schema through `cartograph db import-v1`.
It never reads SQLite. If the only v1 graph is SQLite, either rebuild v2 from
source or first use v1.1.33 to migrate that graph to PostgreSQL. Always run the
v2 importer with `--dry-run` before the exact `--confirm import-v1-postgres`
mutation.
