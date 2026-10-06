# Architecture decision records

[Documentation home](../README.md) · [Project overview](../../README.md) ·
[Architecture rules](../ARCHITECTURE.md) · [V2 architecture](../v2/ARCHITECTURE.md)

Use this directory for durable architecture decisions whose tradeoffs should be
visible after the original review context is gone.

## Records

| Record | Status | Last reviewed |
| --- | --- | --- |
| [0001 — Keep llama.cpp as the managed local default; keep other providers external](0001-mlx-vs-llama-cpp-backends.md) | Accepted for Cartograph v2 | 2026-10-04 |

## When to add one

Add a decision record when a change chooses or preserves a project posture for:

- runtime, backend, or packaging defaults
- storage and migration direction
- feature-slice ownership or public boundaries
- architecture gates and verification policy
- third-party dependencies with long-term operational cost

Do not add a record for routine implementation details that are already clear
from `docs/ARCHITECTURE.md`, local code, or tests.

## Format

Name files as `NNNN-short-kebab-title.md`, starting at `0001`.

Each record should include:

- status
- date
- scope
- context
- decision
- consequences
- revisit criteria, when useful

`docs/ARCHITECTURE.md` remains the current standing-rule document. Decision
records explain why a rule or posture exists; when a decision changes a standing
rule, update both documents in the same change.
