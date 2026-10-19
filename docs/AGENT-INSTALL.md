# Install and use Cartograph v3

Cartograph `v3.0.1` stores each project in embedded RocksDB. Install the native
release with the [quick start](../README.md#quick-start), or build from source
with the pinned toolchain. No database server or Docker bootstrap is needed.

## Copy-paste task

> Install Cartograph v3, initialize the project's
> native graph, run doctor and status, prove one real current context query,
> and register the built executable with my coding host. Preserve old database
> files, containers, and user configuration.

## Build and index

Rust, a C/C++ compiler, and libclang are build prerequisites. RocksDB 11.8.1,
LZ4, and Zstandard compile into the executable; no database server is needed.

```sh
cargo build --locked --release -p cartograph-cli
export PATH="$PWD/target/release:$PATH"
cartograph --version
cartograph index /absolute/path/to/project
cartograph doctor /absolute/path/to/project --format json
cartograph status /absolute/path/to/project --format json
cartograph context 'explain the primary request flow' \
  --project-path /absolute/path/to/project
```

Require a complete fresh generation and nonempty generation-scoped evidence.
Doctor separates native capabilities, freshness, optional semantic models,
registration, and transport. A skipped/no-op second publication is expected.

## Migrate an existing project

```sh
cartograph db migrate --to rocksdb --project-path /absolute/path/to/project
```

The source rebuild verifies destination freshness and retrieval before writing
its migration marker. It preserves legacy PostgreSQL resources and does not
copy legacy-only notes, sessions, embeddings, or artifacts. See
[migration](v2/ROCKSDB-MIGRATION.md) and [storage](STORAGE-BACKENDS.md).

## Register an agent host

```sh
cartograph install --yes --target codex --location local --project-path .
cartograph install --yes --target claude --location local --project-path .
cartograph install --yes --target cursor --location local --project-path .
```

The installer pins the absolute executable and preserves unrelated MCP entries.
Restart the agent host after replacing an executable or registration. An already
attached child cannot hot-load a new binary. Installation alone does not prove
indexing or live transport readiness.

The stdio server command is:

```sh
cartograph serve --mcp --project-path /absolute/path/to/project
```

Profiles are `coding`, `core`, `full`, `read-only`, and `review`. One native
primary owns each project. Unix clients reuse it through private same-user IPC;
Windows currently requires closing another primary process for that project.
Do not delete lock files or state to bypass ownership.

## Keep the graph current

Use the owner's watcher and check status after edits. Explicitly reconcile with
`cartograph index .` or `cartograph sync-if-dirty --project-path .`. Report stale
or incomplete evidence plainly. Graph storage uses bounded multithreaded async
Rust calls; this does not imply RocksDB native AsyncIO.

`maxGenerationBytes` and independent native admission limits still apply.
Historical PostgreSQL performance reports, including
[LARGE-PUBLIC-CORPUS-STREAMING.md](v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md),
are not native performance claims. See [performance tuning](PERF-TUNING.md).

## Optional models

Exact, BM25, graph, review, and affected-test queries need no LLM. Configure
embedding, reranking, summary, chat, or Jev tiers only when needed, using the
[configuration guide](CONFIGURATION.md). Keep credentials in environment or
private state. Native semantic retrieval currently uses exact cosine scanning;
its readiness remains separate from structural graph readiness.
