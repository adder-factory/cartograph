---
name: cartograph
description: Explore a codebase through Cartograph v2's native RocksDB code graph: intent-aware context, exact/BM25/hybrid search, callers/callees/impact, affected tests, live source, review, freshness, and bounded index administration.
---

# Cartograph v2

Cartograph is a native Rust MCP server for coding agents. Its durable graph lives
in each project's `.cartograph/data` RocksDB database. There is no SQLite
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
> to run `cartograph admin init -i` to build a code knowledge graph?

```sh
cartograph index .
cartograph doctor .
cartograph status .
cartograph context 'explain the primary request flow' --project-path .
cartograph install --yes --target codex --location local --project-path .
```

Setup is ready only after doctor, index, status/freshness, and one real context
or find query pass. Restart the agent host after writing MCP configuration.
One primary owns a project; on Unix other CLI/MCP clients reuse that owner.
Never remove database locks or files while an owner is alive.

## Earlier database cutover

`cartograph db migrate --to rocksdb --project-path .` rebuilds and verifies
source facts, then records migration completion. Legacy PostgreSQL data remains
untouched. Notes, sessions, embeddings, and other legacy-only state are not
imported. No PostgreSQL runtime or SQL query tool remains in this checkout.
