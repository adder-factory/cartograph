# PostgreSQL to RocksDB cutover

Cartograph `v3.0.3` replaces PostgreSQL with native RocksDB. Install the v3
release first; earlier v2 binaries and already attached MCP children continue
to use PostgreSQL until explicitly replaced.

The Rust runtime now uses per-project RocksDB 11.8.1 exclusively. The old SQL
facade, managed database lifecycle, SQLX runtime dependencies, SQL query tool,
and PostgreSQL generation builder are removed. Existing extraction and typed
agent services use the native owner: context, exact/BM25/hybrid retrieval,
graph traversal, review, coverage, numerical evidence, artifacts, history,
SCIP, embeddings, and sessions retain their public evidence boundaries.

Run the installed v3 executable:

```sh
cartograph --version
cartograph db migrate --to rocksdb --project-path . --format json
cartograph upgrade --apply --project-path . --json
cartograph doctor . --format json
cartograph status . --format json
cartograph context 'explain the project runtime' --project-path .
```

Migration is a source rebuild and verified marker update. It preserves the
legacy database and does not import its user-authored state. It does not stop
old MCP sessions, install a binary, rewrite host registrations, or delete data.

One primary owns each project. The Unix owner service routes additional CLI
and MCP clients to the active runtime, including export and embedding actions.
Foreground reads/writes run on dedicated bounded workers with shared deadlines,
cancellation, retained completion, synchronous WAL commits, immutable snapshot
reads, changed-record publication, packed adjacency, and resumable maintenance.

Known limits: Windows has no cross-process owner proxy yet; BM25 uses atomic
postings with a bounded scan fallback, and exact cosine search scans bounded
vectors; whole-generation extraction
still has a finite memory/capacity ceiling; online checkpoint/restore commands
are not implemented. Native async Rust scheduling does not imply RocksDB native
AsyncIO or io_uring. No universal speedup is claimed from historical benchmarks.

The current source and executed native gates outrank old PostgreSQL release
notes. See [storage operations](../STORAGE-BACKENDS.md) for command semantics,
[architecture](ARCHITECTURE.md) for bounds, and [licensing](LICENSING.md) for
the libraries now compiled into the executable.

## Native owner upgrades

Stop the existing native owner before reconciliation. A newly installed client
reuses a running Unix owner, so its own `--version` does not prove that owner
has upgraded. On macOS, use `cartograph service stop --project-path .` before
`cartograph upgrade --apply --project-path .`, then `cartograph service start
--project-path .`. On Linux, stop/start `cartograph watch` through its user
service manager. For a foreground owner, stop it cooperatively and relaunch it
with the selected executable. Start the new owner before reconnecting clients.
Verify status version, source freshness, and one real query through a fresh MCP
process; reopen already attached hosts after an owner or executable change.
