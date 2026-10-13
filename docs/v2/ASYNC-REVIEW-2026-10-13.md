# Async execution review — 2026-10-13

This source review covers runtime I/O, subprocess waits, background work, and
CPU work across the twelve workspace crates. It identifies ownership and
cancellation boundaries; it does not claim measured latency improvements for
every operation. The implemented changes ship in v2.1.44.

| Area | Existing execution | Decision |
| --- | --- | --- |
| PostgreSQL queries, leases, publication, retention | Async SQLx operations, exact fences, statement/invocation budgets | Keep transactional ordering and existing caps. Parallel child deletion would violate cascade ordering and enlarge lock pressure. |
| Periodic retention | Previously depended on index/no-op/failure completion; fresh watcher reconciliation skipped it | Run a separate periodic future alongside file-event handling inside the existing supervised auto-sync task. Its lease serializes database mutation across hosts. |
| Maintenance policy reads | Bounded synchronous project configuration | Move the new periodic read through the existing source worker and admission permit. |
| Automatic retention admission | No-progress drains admitted new work; failed/retiring counts omitted excess superseded history | Recount eligible work after bounded cleanup and defer unresolved work. Back off retries independently of the persistent-index circuit. |
| Sync command Git dirtiness | Synchronous child stdout read and wait on an async worker | Use the agent's existing bounded async Git subprocess owner, with a two-second deadline and early output cap. An unavailable observation requests reconciliation. |
| Local backend PID, identity, and signal probes | Synchronous OS subprocess output/waits inside async status/stop/restart paths | Use bounded async subprocess probes with capped output, deadlines, kill-on-drop, and bounded reaping. Unavailable PID observations fail closed before process management. |
| Managed Docker lifecycle | Async subprocesses with command/archive deadlines and pinned local endpoint | Keep this owner and its resource/credential checks. |
| Model HTTP requests and embedding/rerank background work | Async HTTP, per-origin active/queued admission, shared deadlines | Keep the existing transport and foreground/background admission. |
| Discovery, native parsing/reduction, source windows and working-tree overlays | Blocking CPU/filesystem workers with bounded source/pipeline admission and cancellation checkpoints | Keep work on these workers. Async syntax alone does not make parsing nonblocking. |
| MCP request dispatch and configuration writes | Async protocol supervision; short atomic config operations use `block_in_place` with private file locks | Keep file locks and atomic writes together under the request owner. |
| Backup, restore, upgrade, explicit pruning and compaction | Async operations with user confirmation, leases, ordered durable steps and recovery | Preserve completion and recovery ordering. Routine retention does not perform blocking heap rewrites. |
| Domain types, extraction facts, SCIP codecs, deterministic ranking | Pure bounded computation or supervised pipeline work | Preserve deterministic reduction and serialization. |

The review also found bounded synchronous metadata/state-file reads in project
routing, local-backend inventory, and CLI installation, plus Git-log parsing on
the async history path. These are candidates for profiling and moving to their
existing blocking owners if they dominate response latency. They do not need
additional detached tasks or separate scheduler infrastructure.

The regression suite proves idle retention without publication, unresolved
backlog admission without generation growth, independently capped maintenance
backoff, interrupted-drain phase/relation evidence, multi-transaction keyset
cleanup over 101,003 tied references, and bounded subprocess execution while an
unrelated async timer remains responsive. Live PostgreSQL, MCP, supervisor,
managed lifecycle, and deterministic worker gates remain required.
