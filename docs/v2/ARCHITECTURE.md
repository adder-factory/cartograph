# Cartograph v2 architecture

[Documentation home](../README.md) · [Project overview](../../README.md) ·
[Native extraction](EXTRACTION.md) · [Language matrix](../SUPPORT-MATRIX.md)

Last implementation review: 2026-10-06 (`v2.1.40`).

Cartograph v2 is a native Rust code-intelligence server for AI coding agents.
PostgreSQL 18 is its only durable store, ParadeDB `pg_search` provides
code-aware BM25, and pgvector provides model-scoped semantic retrieval. Exact,
lexical, graph, review, freshness, and affected-test workflows require no LLM.

Read this page for the trust boundaries, storage contracts, and bounds behind
each subsystem. Operator commands live in the [CLI reference](../CLI-REFERENCE.md)
and the [storage guide](../STORAGE-BACKENDS.md).

**On this page:** [Hard boundaries](#hard-boundaries) ·
[Crate ownership](#crate-ownership) ·
[Database capability and schema](#database-capability-and-schema) ·
[Project identity and source freshness](#project-identity-and-source-freshness) ·
[Native extraction](#native-extraction) ·
[Bounded parallel indexing](#bounded-parallel-indexing) ·
[SCIP interchange](#scip-interchange-without-a-visual-viewer) ·
[Leases and fencing](#leases-and-fencing) ·
[BM25 and exact retrieval](#bm25-and-exact-retrieval) ·
[Semantic and hybrid retrieval](#semantic-and-hybrid-retrieval) ·
[Evidence packets](#intent-graph-policy-and-evidence-packets) ·
[Working-tree overlay and review](#working-tree-overlay-and-review) ·
[MCP and CLI boundary](#mcp-and-cli-boundary) ·
[Managed PostgreSQL](#managed-postgresql) ·
[V1 import and retention](#v1-import-and-retention) ·
[Security, durability, and licensing](#security-durability-and-licensing) ·
[Release evidence](#release-evidence)

## Hard boundaries

- No SQLite driver, file, fallback, compatibility mode, importer, optional
  feature, or test utility is shipped.
- No Bun/Node/TypeScript runtime is shipped. The v1.1.33 implementation is only
  a historical behavior/migration oracle and is removed from the v2 tree.
- The browser visual-graph viewer is the sole intentional v1 capability
  removal. The underlying typed graph, callers/callees, paths, impact,
  similarity, dependency/import queries, and machine-readable interchange stay
  release requirements; no other feature may be dropped to declare parity.
- Cartograph archives contain only the native executable and allowlisted MIT/
  third-party notices. PostgreSQL, ParadeDB, pgvector, and images remain
  separately installed software.
- Database URLs are environment/local-state secrets. Public errors, debug
  output, MCP responses, archives, and project records do not render them or
  absolute checkout paths.
- Generative output is optional and cannot replace structural truth. Embedding
  and reranker tiers use OpenAI-compatible HTTP; chat tiers support
  OpenAI-compatible HTTP, Anthropic Messages API, and the bounded local Claude
  bridge. Ask, generated summaries/roles, and LLM dead-code judging preserve
  evidence/model provenance and explicit failure/fallback states.
- Optional `decisionLlm` uses Typesafe's pinned Jev decision API. The agent
  navigation service asks parallel choice/sufficiency questions, executes only
  allowlisted native retrieval operations, and returns generation-fenced source
  evidence. Provider failure preserves the native exploration packet; source
  changes and cancellation remain errors. The tier's `features` list limits
  which surfaces may consult Jev (below). See the
  [configuration guide](../CONFIGURATION.md#optional-jev-navigation) for
  disclosure, bounds and native bypass behavior.

### Decision-tier features

| Feature | Surface | Sent to the provider |
| --- | --- | --- |
| `explore` | Exploration navigation; the only surface a tier without a `features` list permits | Question, candidate metadata, and bounded source |
| `context` | Context ranking of BM25/semantic retrieval candidates (see [decision ranking](#optional-decision-ranking)) | Question and candidate metadata, never source |
| `roles` | Symbol role classification | Symbol metadata, never source |
| `rename` | Rename-mention triage | Symbol metadata and each mention's source line |

Unknown feature names are ignored. An empty `features` list disables every
surface while keeping the tier configured.

## Crate ownership

| Crate | Responsibility |
| --- | --- |
| `cartograph-domain` | Branded IDs, enums, source language/path/digest contracts, project identity, canonical manifest digest |
| `cartograph-config` | Secret database settings, pool/timeouts, neutral project source/storage policy, bounded atomic configuration I/O |
| `cartograph-extract` | Bounded discovery/read/hash, native tree-sitter parsing, declarations/references, deterministic resolution |
| `cartograph-db` | Capabilities, migrations, leases/fences, COPY, publication, retrieval, semantic storage, v1 import, retention, managed lifecycle |
| `cartograph-indexer` | Bounded parallel stages, deterministic reduction, supervisor/cancellation/reaping, corpus-aware workers |
| `cartograph-search` | Exact/BM25/hybrid evidence, typed intent, graph traversal, RRF, affected tests, trust/abstention |
| `cartograph-scip` | Bounded zero-runtime protobuf codec, exact typed-edge extension, deterministic export, per-file replacement overlay |
| `cartograph-llm` | Bounded redacted embedding/reranker/chat/decision clients, provider config, shell-free `apiKeyCommand` credential sources, model identity, and local backend supervision contracts |
| `cartograph-agent` | Project runtime, freshness, indexing, structural summaries, embedding sweeps, optional decision-guided navigation, Git review, source excerpts, working-tree overlay |
| `cartograph-mcp` | Bounded stdio JSON-RPC/MCP protocol, profiles, cancellation, stable errors |
| `cartograph-cli` | Native command routing, database operations, MCP adapter, project-local agent installation |
| `cartograph-test-support` | Test-only, unpublished live-PostgreSQL test-schema lifecycle helpers shared by the live suites; admitted only as a dev-dependency and never in a shipped dependency tree |

### Dependency direction

Dependencies point inward through typed contracts. MCP/CLI do not issue SQL;
database code does not read arbitrary project files; extractors do not know
about PostgreSQL or transport.

`scripts/test-workspace-dependencies.sh` enforces the reviewed production/build
dependency directions, rejects cycles and test-support dependencies in shipped
crates, and requires an explicit boundary decision for a new workspace crate.
Neutral project configuration is owned by `cartograph-config`; the LLM crate
retains provider interpretation and compatibility wrappers. Structural symbol
summary sweeps expose a typed agent service used by the CLI/MCP adapter.

### Shared source windows

Context source windows share one bounded source-manifest scan and captured file
set per request. They carry the expected generation and reject a generation
change before returning evidence. Per-file source hashes and source-policy
identity remain authoritative; concurrent requests still perform independent
validation. No time-based freshness cache is introduced.

### Model transport admission

Embedding and rerank clients share a process-bounded transport registry, keyed
privately by effective endpoint/model/credential/timeout settings. Chat
transports remain separate.

| Bound | Value |
| --- | --- |
| Active slots per HTTP origin | Four, with at most three occupied by background embeddings |
| Queued requests | 32, including at most 16 background waiters |
| Retained transports | At most 32 |
| Retained origin gates | At most 32 |

Queueing consumes the same deadline as the HTTP request, and cancellation
releases admission.

## Database capability and schema

Before migration or normal work, Cartograph proves:

- PostgreSQL 18.4 or newer within major version 18;
- `pg_search` 0.26.0, expected preload state, the `paradedb` access method, and
  exact `pdb.source_code` token behavior;
- pgvector 0.8.4 or newer, with 0.8.7 recommended for external PostgreSQL;
- bounded DML/DDL capability in the selected safely quoted schema.

The managed image supplies PostgreSQL 18.6, `pg_search` 0.26.0, and pgvector
0.8.6; see [Managed PostgreSQL](#managed-postgresql).

### Migration ledger

The append-only migration ledger currently owns forty-nine versions, recorded
in `schema_migrations`. Each migration attempt is one atomic transaction that
bounds its own lock waits:

| Contention rule | Value |
| --- | --- |
| Wait for any single lock (`lock_timeout`) | At most 2 seconds, and at most half the effective statement timeout |
| Pause before retrying a lock timeout or deadlock | 1 second |
| Retry budget per migration call | 5 minutes, then `schema_busy` |
| `db start` exit status on `schema_busy` | 75 (`EX_TEMPFAIL`) |

See [troubleshooting](../TROUBLESHOOTING.md#schema-migration-reports-schema_busy)
for recovery.

<details>
<summary>Details: migrations 23–49</summary>

| Migration | Digest admitted | Change |
| --- | --- | --- |
| 49 | V22 | v1 cross-file resolution parity across languages |
| 48 | V21 | Dedicated v1-parity extraction families and v1 per-file facts across languages |
| 47 | V20 | Rust turbofish calls that name their function |
| 46 | V19 | Rust references inside macro arguments |
| 45 | — | Records each ready generation's exact fact counts and source bytes, so status and freshness stop counting fact tables, and keys Git churn/co-change refreshes by their inputs (`history_refreshes`) so an unchanged HEAD reuses stored history |
| 44 | V18 | The refreshed CUDA grammar and Unicode identifier semantics |
| 43 | V17 | The refreshed ArkTS and OCaml grammars |
| 42 | — | Resumable generation retirement (the `retiring` state) and bounded maintenance telemetry |
| 41 | V16 | Nominal Rust self-receiver ownership, including private parent methods called across split implementation files |
| 40 | V15 | Ada/VHDL unit resolution and guarded numerical precision |
| 39 | — | Stores the privacy-preserving run-scoped exclusion policy on each generation so freshness and reconciliation replay the admission policy that built it |
| 38 | V14 | First-class Slang/WESL module semantics and JavaScript static dynamic-dispatch evidence |
| 37 | — | Adds the generation-fenced structural finding relation and its input fingerprint, so a readiness probe reads stored findings instead of evaluating every detector over a whole generation |
| 36 | V13 | Named TypeScript/JavaScript construction targets |
| 35 | — | Retains canonical search-document metadata text for exact SQL-streamed V13 digests |
| 34 | — | Lets generation spill reference immutable parse-cache payloads without storing a second copy |
| 33 | — | Adds the fenced symbol identity lookup used by streamed centrality updates |
| 32 | — | Adds generation-fenced native extraction/fact spill and deterministic PostgreSQL canonical reduction |
| 31 | V12 | Stable bounded Go and Python anonymous call-target normalization |
| 30 | V11 | Anonymous Rust call-target normalization |
| 29 | V10 | Call-target-precise secret exposure and incomplete-implementation evidence |
| 28 | V9 | Precision-fenced executable-line, resolver-provenance, import-consumer, and biomarker evidence |
| 27 | V8 | Context-classified structural diagnostics and clone compatibility |
| 26 | V7 | Adds immutable numerical sites |
| 25 | — | Preserves exact directory-import names while deriving a non-empty indexed simple name |
| 24 | V6 | Cargo-workspace crate/re-export semantics |
| 23 | — | Adds virtual parse-cache payload accounting and table-local autovacuum policy for high-churn generation, fact, and cache relations |

All structural finding/status/rollup reads remain non-mutating and expose
`not_computed`; only the dry-run-first, confirmed `biomarkers-refresh`
boundary may replace the exact derived relation.

</details>

### Core relations

| Relation | Purpose |
| --- | --- |
| `projects` | Privacy-preserving project root identity and current generation pointer, plus retention attempt/outcome/consecutive-failure telemetry (migration 42) |
| `index_generations` | Staging/ready/current/superseded/failed/retiring lifecycle (`retiring` since migration 42), sequence, source/content digests, and exact fact counts and source bytes (migration 45) |
| `files` | Generation-scoped normalized path, language, content hash, parse status |
| `symbols` | Generation-scoped declaration identity, kind, range, safe signature, visibility, export/default-export, async/static, and declaration-only semantics |
| `edges` | Typed symbol relationship, confidence, provenance, represented site count |
| `references` | Exact/coarse source evidence, owner/target, byte span, multiplicity |
| `numerical_sites` | Generation-scoped static numerical operation/hazard/precision sites with exact spans, evidence level, confidence, provenance, and explicit unknowns |
| `search_documents` | Canonical durable code/name/natural-text documents and stable document identity |
| `generation_search_relations` | Verified catalog for immutable generation-local BM25 tables and indexes |
| `project_operation_leases` | Observable PostgreSQL-clock ownership and fencing |
| `embedding_models` | Model fingerprint, provider/name, dimension, lifecycle/readiness metadata |
| `document_embeddings` | Model- and generation-scoped vectors |
| `v1_import_runs/checkpoints` | Exact resumable v1.1.33 PostgreSQL cutover state |
| `coverage_sources/symbol_coverage` | Generation-fenced LCOV provenance and symbol coverage |
| `file_history/file_cochanges/symbol_issues` | Bounded Git churn, co-change, and issue-tagged symbol evidence |
| `history_refreshes` | Per-project input key and counts of the last Git churn/co-change refresh (HEAD commit, commit budget, algorithm version), so an unchanged HEAD reuses stored history (migration 45) |
| `issue_history_refreshes` | Per-project, generation-bound record of the last issue-tagged history refresh: HEAD commit, scan and attribution counts, truncation (migration 18) |
| `agent_artifacts/mcp_sessions/mcp_tool_calls/mcp_macros` | Durable notes/summaries/roles, investigations, usage audit, and macros |
| `symbol_similarity_edges/symbol_similarity_builds` | Model-scoped materialized similarity with exact build provenance |
| `native_parse_cache` | PostgreSQL-backed deterministic parse-result cache |
| `native_generation_spills` | Generation-fenced spill root: phase and logical byte/row quotas for native extraction/fact spill (migration 32) |
| `native_generation_spill_*` | Spill batches, raw rows, and typed fact groups (`_files`, `_symbols`, `_edges`, `_references`, `_numerical_sites`, `_documents`) awaiting deterministic canonical reduction (migration 32) |
| `summary_priority_queue` | Generation/evidence-fenced agent-demand summary priority |
| `structural_finding_runs/structural_findings` | Generation-fenced detector relation with its exact input fingerprint |
| `schema_migrations` | Append-only migration ledger: version, name, checksum, and application time |

Generation foreign keys cascade only through explicit generation deletion.
Ordinary indexes support identity/filter joins. BM25 ranking is intentionally
not global: each ready/current generation owns a derived physical
`search_g_<generation UUID>` table and matching `_bm25` index. The identifier is
derived only from a validated globally unique generation UUID (enforced by
migration 11), and the catalog binds it to the project, content digest, row
count, and relation-format version.

## Project identity and source freshness

The canonical checkout path is hashed under a domain separator. PostgreSQL
stores `project:<digest>`, never the path. Agent runtime and v1 importer call the
same domain helper, preventing invisible duplicate projects or path disclosure.

The complete supported-source revision is built from an exact count plus
ordered normalized path/content-digest pairs through one shared domain builder.
Indexer, status, source context, and importer use the same encoding. The importer
builds its caller-owned manifest from the exact checkout through the frozen
v1.1.33 path boundary, excluding every additive v2 extension and mode. An
imported schema must contain exactly that checkout path/content set; missing,
extra, or substituted paths fail before durable mutation.

`fresh=true` means the current generation recorded exactly that complete live
manifest under the native generation-digest contract emitted by the running
binary and under its recorded source-admission policy. Run-scoped exclusion
globs are stored with the generation but serialized only as a pattern count;
status, drift, source context, automatic synchronization, and upgrade replay
them without exposing path-shaped policy. An explicit index can replace or
clear the run-scoped list. A newer extractor, resolver, or test-ownership contract therefore marks
an unchanged checkout stale and makes an ordinary index publish a replacement;
it cannot reuse older graph semantics as a source-only no-op. Unknown or stale
state lowers confidence and is never treated as a clean result.

## Native extraction

Current production admission covers 132 modes in total:

| Mode family | Modes |
| --- | ---: |
| v1.1.33 language modes | 73 |
| Native TOML | 1 |
| Dedicated textual game-scripting modes | 52 |
| WGSL, Metal, Slang, and WESL shader additions | 4 |
| Ada/SPARK and VHDL | 2 |
| **Total** | **132** |

Of those, 67 are pinned grammar-backed modes and 65 use bounded Rust custom
scanners. The 163-extension v1 manifest is
exact; `.pyi` and every dedicated game-script extension are additive
improvements. Every family has
literal-safety and cancellation tests, deterministic 1-vs-4 worker facts, and
live PostgreSQL/ParadeDB COPY, publication, and BM25 evidence. Unknown discovery
remains fail-closed. Framework and cross-language resolver parity is tracked as
a distinct release gate rather than being inferred from language admission.

Each mode has exactly one extraction strategy, registered in
`crates/cartograph-extract/src/language.rs`:

- grammar-backed dedicated families: JavaScript/TypeScript, Rust/Python/Go,
  C-family, shader, Ada/VHDL, shell, managed (Java/C#), JVM-dynamic, and the
  v1-parity families for PHP, Pascal/Delphi, Objective-C, Swift, Dart, F#,
  ArkTS, Astro, Ruby, Lua/Luau/KHN, R, Nix, Clojure/Common Lisp, Lean,
  ReScript, Solidity, VB.NET, Apex, and HCL/Terraform;
- tags queries (Elixir, Haskell, Julia, OCaml, Verilog), the conservative
  generic walker (ABAP, GraphQL, HTML, Prisma, SQL, YAML), and parser-only
  documents;
- bounded custom scanners, including the Vue and Svelte hosts, whose script
  regions run through the JavaScript/TypeScript walker over exact host
  positions.

The [coverage report](../LANGUAGE-COVERAGE-REPORT.md#strategy-families) lists
every mode's strategy. Its per-language
[v1 parity oracle](../LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle) requires
every per-file fact v1.1.33 extracted from each v1 mode's fixture corpus to
have a unique exact identity, committed exact alignment pins, or a
pending/intentional ledger entry. Ambiguous identities never match and stale
rows fail; the linked report owns current counts and the update procedure.
The V22 generation-digest contract (migration 49) fences these families and
v1 cross-file resolution parity, so generations from older binaries report
stale once and republish.

For each admitted file:

```text
discover -> bounded read/hash -> tree-sitter parse -> typed facts
         -> deterministic module resolution -> canonical reduce/digest
```

Facts include deterministic IDs, exact paths/ranges, symbol/reference kinds,
literal-free callable signatures, privacy-safe numerical sites, confidence,
provenance, diagnostics, and multiplicity. Ambiguous references remain
unresolved.

The numerical MVP is a separate generation-scoped evidence plane. Its
`rust_ast_v1` analyzer covers parsed or partial Rust files and records exact
site identity/span plus bounded operation, hazard, precision, expression
digest, confidence, provenance, evidence level, and facts that syntax could
not prove. It never stores the source expression or literal. Static heuristic
evidence is not relabeled as a runtime observation or formal proof;
`cartograph_numerical`, status, and numerical review report observation and
formal adapters as `not_configured`. Generations recorded under an older digest
contract (anything before the current V22) remain explicitly stale until an
ordinary index republishes them; for generations older than V7, which predate
numerical sites, that republication is also what publishes numerical evidence.

See [native extraction](EXTRACTION.md) and the
[extension guide](../EXTENDING-EXTRACTORS-RESOLVERS.md).

## Bounded parallel indexing

The supervisor selects a corpus-aware worker count from both supported-file
count and exact indexed source bytes, bounded by the caller, hardware, and the
measured 1/2/4/8/16 policy. Every stage has item/task/byte admission, ordered
envelopes, cooperative cancellation, stage/item/operation deadlines, and
retained-worker cleanup.

```text
discover -> read/hash -> parse/extract
         -> memory: retained facts -> resolve -> reduce -> bounded canonical COPY
         -> postgres: bounded cache-backed parse spill -> compact resolution preparation
                      -> per-file resolve -> typed COPY -> partitioned canonical rows
         -> exact relation/digest validation
         -> populate generation search table -> build/verify BM25
         -> carry forward unchanged embeddings/coverage/similarity
         -> scoped planner statistics -> ready
         -> validate exact relation again -> publish
```

Workers complete out of order; the reducer commits in input order. Canonical
facts and six PostgreSQL COPY streams are bounded and checked. Resolve measures
its unordered facts against the working-set allowance; deterministic reduction
then independently proves that the canonical output fits the configured final
generation ceiling. This distinction admits dense graphs that safely reduce
below the publication bound without weakening either limit.

### COPY statements and atomic publication

Each table's canonical order is retained across COPY statements. A new
statement starts at 100,000 rows or before an encoded batch would exceed 64 MiB;
one independently bounded row is indivisible. Every statement verifies its
exact row count, and all statements still run inside the same
generation-preparation transaction.

Successful statements and later prepare phases advance a
monotonic durable-progress observer. The supervisor extends its short generic
progress deadline only while preparation is running, only after real progress,
and never beyond the independent COPY deadline; a genuinely stuck transaction
is still cancelled. A failed COPY, search table, or index build rolls back with
the staging transaction and therefore cannot reach `ready`. Publication is
atomic: it first requires the exact generation relation and catalog to remain
valid, then one transaction swaps the current pointer and supersedes the prior
current generation.

### Planner statistics

After an actual COPY, preparation runs column-targeted `ANALYZE` only on the
six copied relations before the generation can become ready. The relations
are visited in one deterministic order, and a contended statistics lock waits
under the connection/prepare statement deadline instead of being silently
skipped; timeout or query failure rolls the generation preparation back. This
prevents immediate status/issue-history reads from racing PostgreSQL's first
autoanalyze while avoiding the v1 failure mode of database-wide maintenance on
an unchanged/no-op index.

### Carry-forward of unchanged evidence

Symbol and document identities are stable across generations, but every
structural fact table is generation-fenced. Inside the same bounded prepare
transaction, after the BM25 relation is built and before planner statistics,
preparation re-links content-addressed derived evidence so a successful
re-index does not erode it for unchanged code, without making the short
publication critical section scale with the corpus:

- **Embeddings** carry forward for documents whose rendered text is unchanged,
  one keyset page of 4,096 next-generation documents at a time in
  document-identity order. Prepare progress advances after every page, so the
  supervisor's durable deadline measures stalls rather than corpus size.
- **Coverage** carries forward for symbols whose structural digest is
  unchanged.
- **Materialized similarity** edges and build metadata carry forward only when
  the complete logical fact set (content digest and digest version) is
  identical; otherwise reads fall back to authoritative live pgvector search.

Changed source is deliberately left without derived evidence so callers cannot
mistake it for fresh data.

### Supervisor ownership and fault coverage

The public future owns the operation even when a caller cancels/drops it.
Supervisor tests cover queued/running cancellation, timeouts, slow/hung/panicked
work, database faults, lease uncertainty/takeover, caller-future drop, child/task
reaping, rollback, and publication cleanup. The committed worker matrices must
retain identical digest, rows, edge kinds, diagnostics, and ordered BM25 IDs.

### Memory and PostgreSQL spill strategies

Native construction has two physical working-set strategies with one logical
output contract. Small manifests use the original in-memory reducer. Large or
explicitly selected manifests lazily form parse batches as the bounded
scheduler admits them behind the exact generation/lease fence:

- Each batch is capped at 64 files and 64 MiB of combined source; a file is
  never split, and the 32 MiB per-file ceiling keeps every file below that
  bound. Item deadlines therefore cannot expire while still waiting in an
  unmaterialized manifest tail.
- An admitted batch reuses one native extractor per encountered language,
  including its parser and compiled queries.
- Cacheable extraction payloads are written once and referenced from the spill;
  inline and cached rows retain one representation-independent logical batch
  identity.
- Resolver workers COPY validated typed fact groups under independent row,
  logical-byte, and retained-memory bounds.
- PostgreSQL then reduces files, symbols, edges, references, numerical sites,
  and documents through 64 deterministic UUID partitions per relation,
  committing four contiguous partitions at a time. A completed raw group is
  deleted in the same transaction that inserts its canonical rows and advances
  the durable cursor.

PostgreSQL may spill grouping/sorting to its configured temporary storage;
Rust never reloads the complete canonical payload to compute the digest.

<details>
<summary>Details: spill planner statistics and identity with the memory path</summary>

A relation's first group runs column-targeted `ANALYZE` on its raw spill table,
and its last group does the same on the canonical table it filled, inside the
group's transaction. Later relations validate against this generation's files
and symbols, and a plan made from a never-analyzed table or from a sample of
only earlier or failed generations estimates the generation at about one row
and probes an index that matches only its prefix, walking the whole generation
per row. The reduce therefore never relies on autovacuum timing. As in
preparation, a contended statistics lock waits under the spill statement
deadline, and a timeout fails the stage as `reduce_deadline_exceeded`.

The PostgreSQL path retains deterministic identity with the memory path:
batch-local validation uses the same field contract, global conflicts and edge
multiplicity are reduced under database constraints, and each canonical
partition group proves its file/symbol/span cross-relations before its raw
evidence is removed. The durable completed phase makes a redundant final
generation-wide relation scan unnecessary. The V22 digest is streamed as exact
canonical row bytes in the memory reducer's table/key order. Centrality uses
the same pre-dedup calls/reference graph and is patched onto fenced raw symbols
before sealing. Exact batch replay and the canonical cursor make an interrupted
retained operation idempotent; changed bytes, quotas, phase misuse,
cancellation, or lease loss fail closed.

</details>

### Streamed Resolve progress

The streamed Resolve item reports work-derived progress after exact extracted
pages, committed fact batches, completed derived passes, centrality work, and
committed score batches. The supervisor therefore observes advancing durable
work before the outer item finishes; it does not use timer-only keepalives. A
real no-progress cancellation retains the active stage and the stable
`progress_stalled` reason in the public failure.

### Memory and capacity bounds

This is a hybrid rather than an unlimited-memory claim. Project-wide resolution
lookups, clone profiles, and the centrality graph remain compact native
structures with explicit bounds derived from `maxGenerationBytes`. The whole
database spill has independent logical byte/row quotas, and physical
heap/index/WAL/temporary-disk use remains an operator capacity concern. COPY
batching continues to bound the memory path's publication statements.
Every spill transaction acquires the exact mutation fence before work and runs
one joined database-clock lease plus staging-generation check immediately before
commit. The final check executes under the transaction's already-held advisory
locks, preserving the expiry fence without repeating lock round trips.

## SCIP interchange without a visual viewer

Export reads one repeatable-read current-generation snapshot and then requires
the live project bytes to match every stored file hash before writing an atomic
project-local artifact. Standard SCIP definitions, relationships, occurrences,
UTF-8 byte ranges, documentation, and readable stable symbols are emitted. A
protobuf-compatible private field on `SymbolInformation` additionally carries
every Cartograph edge kind and exact site count; foreign SCIP consumers ignore
the field, while Cartograph round-trips calls, imports, tests, containment,
type/use, framework, and cross-language edges without degrading them to generic
references.

Import is a persistent overlay, not a direct database mutation. The validated
artifact is atomically installed at `.cartograph/scip/overlay.scip`; its digest
is part of source freshness. During every subsequent index:

- matching documents replace native non-file facts for those files;
- native IDs are reused only on an unambiguous kind/name match;
- uncovered files remain native, and unresolved foreign targets stay explicit;
- centrality is recomputed, and canonical validation runs before COPY.

Import forces publication and restores the previous artifact when
publication fails and no competing writer replaced the requested bytes. If the
forced index fails or is cancelled and restoring the previous overlay also
fails, the requested artifact may still be installed for the next index; that
failure is reported beside the primary failure (or the cancellation) as
`overlayRollbackFailure` (`code: scip_overlay_rollback_failed`) without
replacing it. See
[troubleshooting](../TROUBLESHOOTING.md#scip-import-reports-overlayrollbackfailure).

## Leases and fencing

Write-bearing operations acquire a project/operation lease with:

- PostgreSQL-clock acquisition, heartbeat, and expiry;
- owner/process marker, operation, optional generation, and a unique
  per-acquisition fence: a random version-4 UUID lease ID compared by exact
  equality (not a monotonic counter);
- transaction/advisory locks for publication, import, retention, and derived
  index replacement;
- exact fence checks immediately before commit, where PostgreSQL checks the
  token, generation binding, and database-clock expiry under a row lock.

An expired/replaced lease fails the operation and rolls back. Cleanup/release
errors cannot mask the primary lost-fence error.

The indexer supervisor runs pipeline work, lease renewal, and its monitor as
separate tasks.

<details>
<summary>Details: heartbeat isolation, monitor precedence, and cancellation reaping</summary>

Renewal is woken only by its own interval timer, its bounded
heartbeat request, and a stop signal, so a long synchronous pipeline section
cannot delay the heartbeat. The monitor never polls work inline, so none of its
branches can wait on progress state that a suspended work future already holds
a pending acquisition for. An in-flight heartbeat verdict, a cancellation
request, the work deadline, and a progress stall still take precedence over
work completion, as before: whenever a heartbeat overlapped the monitor's wait
for the event it accepts, even one that finished just before the acceptance,
the monitor re-checks them once that heartbeat is done. Whole-graph CPU
sections of the spilled resolver run under `block_in_place` so they do not hold
an async worker.

Aborting a task cannot interrupt such a synchronous section. Cancelled work
gets the cooperative signal and its grace; if the work is still inside a
section after that, the supervisor waits up to one COPY timeout for the section
to end, never past the operation's reap ceiling (the operation deadline minus
the database finish reserve), before it reaps registered workers and runs the
normal owned cleanup. That is the reap allowance the
finish reserve keeps after the grace, and configuration validation keeps it,
with the grace, one heartbeat interval, and the database finish reserve, inside
one lease duration.

While it waits, the lease keeps being renewed when
publication or owned cleanup can still follow, but not after lease loss or an
uncertain heartbeat; registered workers are reaped as soon as the work is gone,
without waiting for that renewal to settle. If that renewal loses the lease or
cannot vouch for it, the cancellation stays the primary outcome: the cleanup
heartbeat re-verifies ownership before any mutation and, without a confirmed
token, cleanup only reconciles and reports its failure beside the cancellation.
Work that is still running when the allowance ends is reported as unreaped and
its owned cleanup is skipped, so renewal stops, the lease expires, and the next
writer recovers the staging generation.

</details>

## BM25 and exact retrieval

The covering search document includes project/generation/file/symbol identity,
path, language, document kind, qualified name, code, and natural text.
Qualified name and code use `pdb.source_code`, which separates snake_case and
camelCase. Natural documentation uses text tokenization.

Each bounded read starts a repeatable-read transaction, validates the caller's
expected generation against `projects.current_generation_id`, requires its
verified catalog/table/index, and queries only that physical generation table.
Other projects and ready/superseded generations therefore cannot change BM25
scores or ordering. A pointer change yields `CurrentGenerationChanged` rather
than mixed evidence. Stable tie-breaking uses the document key. Every hit
returns native score plus ordered field-component provenance. Typed task intent
selects explicit qualified-name/code/natural-text boosts; user text remains a
bound parameter, never interpolated SQL.

Natural-language code searches may append a small deterministic identifier
alias set inside the same 1 KiB query bound—for example, freshness can add
`project_status`/`source_revision`, while a checked BM25 table can add
`require`/`generation_search_relation`. The original query remains intact,
aliases are deduplicated, and no LLM or repository-specific file rule is used.

### Generation search relation reconciliation

Migration/startup reconciliation checks current and ready generation relations
with unhealthy relations ordered first. It:

- repairs at most 64 per invocation from canonical `search_documents`, and
  fails closed when further repair is needed;
- removes at most 64 strictly parsed unowned `search_g_...` tables.

Every build/drop holds a generation-specific transactional advisory lock.
Retention drops the physical table before deleting that terminal generation's
canonical cascades; row, relation-byte, DDL-count, and generation-count budgets
bound the whole cleanup invocation, and each drain transaction inside it has
its own smaller bounds (see [Generation retention](#generation-retention)).

Exact current-generation name, path, reference, symbol-ID, and graph queries are
separate bounded paths. Reference evidence retains exact versus coarse precision
and represented site counts.

## Semantic and hybrid retrieval

Embeddings are optional; pgvector capability is mandatory so the storage shape
is predictable. A model registration fixes provider, name/fingerprint,
dimension, and normalization. Vectors are isolated by model and generation,
with a dimension-validated model-scoped HNSW expression index. Managed
containers reserve 256 MiB of shared memory and index creation sets
`max_parallel_maintenance_workers=0`; an actual PostgreSQL shared-memory
allocation failure remains a distinct resumable HNSW phase rather than a
generic embedding failure. Managed container resource and WAL limits are
described under [Managed PostgreSQL](#container-resources-and-server-settings).

Before semantic Top-K, Cartograph proves the model is active, fingerprint and
dimension match, current-generation document coverage is complete, HNSW exists,
and a query probe succeeds. Readiness is one of `ready`, `not_configured`,
`not_indexed`, `stale`, or `unavailable` at the agent boundary.

### Hybrid fusion

For automatic/hybrid mode, BM25 and eligible semantic requests run concurrently
under shared cancellation/deadlines. Each channel has an explicit bounded
candidate window that is independent from the smaller fused result/output
limit, so a low-token response does not also become a low-recall search.
Reciprocal-rank fusion combines ranks—not raw BM25/cosine scales—and retains
channel rank/score provenance. If semantic evidence is not ready or empty, the
packet labels the exact lexical fallback.

Implementation, change-planning, and architecture queries that do not
explicitly ask for tests, fixtures, examples, or benchmarks use a disclosed
`production_definitions` result preference. Exact symbol kinds are read from
canonical generation-fenced symbol rows, then functions/declarations precede
parameters, imports, auxiliary paths, and tests without rewriting raw channel
scores. Explicit auxiliary-code tasks retain neutral RRF ordering.

### Reranking

When `rerankerLlm` is configured, the agent sends only the bounded query and
bounded source-bearing semantic Top-K candidates to its OpenAI-compatible
`/v1/rerank` endpoint. A complete finite response replaces semantic rank and
raw score before reciprocal-rank fusion; lexical rank and evidence remain
unchanged. Candidate source is never serialized in vector-search or retrieval
responses. Timeout, endpoint, configuration, missing-text, and malformed-result
paths retain cosine order and emit an explicit rerank outcome instead of
failing lexical retrieval.

### Optional decision ranking

When the decision tier enables `context`, a non-deterministic context request
skips the local cross-encoder, and Jev instead judges up to 24 fused
BM25/semantic retrieval candidates from the task text and candidate metadata,
never source. Candidates are reordered within the positions retrieval
candidates already occupied, so exact anchors and graph expansion keep their
places. Each judged item reports an advisory `decision_relevance`, and the
packet's `decision_rank` records the model, outcome, and judged count. Unless
an exact anchor selected them, primary edit candidates become the files of the
most relevant judged items. If the provider fails, the packet is rebuilt
through the ordinary reranker path and keeps the provider outcome as
provenance; only cancellation fails the request. `mode: deterministic` never
consults Jev.

### Model endpoints

Embedding, reranking, and model-catalog probes share one endpoint normalizer.
A configured base ending in `/v1`, or in a known OpenAI-compatible operation
path, is reduced before the target operation is appended. Doctor's model
catalog probe therefore requests exactly `/v1/models`, matching the endpoint
semantics used by smoke, embedding, and reranking clients.

Embedding and reranking can be the only configured model tiers. Generated
summaries, classification, `ask`, and local chat are independent optional
features; their absence does not degrade model smoke status or doctor health.

## Intent, graph policy, and evidence packets

Natural-language context is classified without an LLM into one of seven
intents. Intent selects independently bounded candidate/exact/evidence/
affected-test limits and graph behavior:

| Intent | Graph behavior |
| --- | --- |
| Symbol lookup | Avoids irrelevant expansion |
| Documentation lookup | Avoids irrelevant expansion |
| Implementation trace | Follows outgoing calls |
| Architecture survey | Follows outgoing calls |
| Change planning | Follows reverse impact with affected-test selection |
| Test selection | Follows reverse impact with affected-test selection |
| Error diagnosis | Follows reverse impact with affected-test selection |

Bounded term tables plus explicit diagnostic, test-question, change-action, and
implementation-question rules make classification deterministic without
letting a phrase such as `regression coverage` override the requested change.

Packet assembly adds exact anchors, BM25/semantic candidates, then bounded graph
evidence. It deduplicates and orders evidence by reason/path/line/identity. A
separate typed, bounded `editCandidates` set promotes explicit anchors first;
without one, it promotes only files tied for the strongest distinct code-aware
task-term concentration across qualified names and paths. The broad evidence is
retained for impact and uncertainty instead of being discarded by that primary
edit-site decision.

Packets return generation, intent, freshness, confidence, abstention, channel
and reranker provenance, primary edit candidates, affected tests, and
truncation; with [decision ranking](#optional-decision-ranking) they also carry
`decision_relevance` and `decision_rank`. MCP low-token projections keep
generation/freshness once, retain
follow-up symbol or document identities and compact provenance, and omit raw
score detail unless explanation was requested. Low-token and plan requests do
not collect project tool-usage telemetry unless the caller explicitly opts in.
No query or full source body is persisted as telemetry.

## Working-tree overlay and review

When durable source is stale, CLI/MCP context can inspect changed/untracked
supported files relative to `HEAD`. Git execution is shell-free,
noninteractive, output/deadline bounded, and reaped. The native source reader
then applies per-file/aggregate bytes, cancellation, supported-language, root,
UTF-8, and result bounds.

Matching live items carry path, Git change kind, exact content digest,
line-bounded UTF-8 excerpt, matched terms, and truncation. Overlay states are
`not_checked`, `clean`, `no_matches`, `used`, or `unavailable`. Overlay facts
remain separate from immutable graph evidence and do not upgrade stale
confidence.

`review --ref` separately resolves an immutable base commit and combines
committed/staged/unstaged/untracked paths with current-generation exact file,
reverse-impact, and affected-test evidence. It reports dirty state, freshness,
abstention, and per-stage truncation.

## MCP and CLI boundary

The MCP crate serves newline-delimited JSON-RPC over stdio as a dual-era server.
Modern MCP `2026-07-28` uses stateless `server/discover`, required per-request
version/capability metadata, explicit result discriminators/server identity,
and private TTL-cached stable tool lists. Legacy clients retain the exact
`2024-11-05` initialize path. Deterministic profiles are immutable
process-lifetime authorization ceilings; task-local schema selection belongs in
the host because modern MCP forbids connection-dependent tool-list mutation.
Both eras retain bounded input/output/concurrency, hard request deadlines,
cancellation and worker reaping, redacted internal failures, and stable errors.
Product handlers call the agent/search services; transport never reaches
through to SQL or filesystem internals.

The CLI exposes the same typed services plus managed database and project-local
MCP installation. Background admin work has explicit job IDs/status/cancel; it
is not an unbounded detached process.

Non-recoverable file-local parse failures retain one validated normalized
relative path and an allowlisted reason through the native worker, supervisor,
project runtime, and CLI/MCP adapters. Invalid grammar-recovery spans instead
retain an empty partial file plus bounded `extraction_invalid_span` degradation;
parser stops without cancellation use the same recovery boundary with
`extraction_parser_stopped`. One unsafe file therefore cannot block publication
of the remaining generation. Text
paths are escaped and JSON/MCP responses are structured; absolute roots,
source/parser text, literals, database URLs, and driver errors are discarded
before that public boundary.

## Managed PostgreSQL

On macOS/Linux, `db start` owns a pinned upstream ParadeDB container and private
volume/credential, binds only loopback, validates exact labels/mounts/identity,
and proves all capabilities. Status/logs/stop/backup are explicit. Restore,
upgrade, derived-index rebuild, remove, import, and prune require exact
operation-specific confirmation and have rollback/recovery tests.

The digest-pinned ParadeDB 0.26.0 image supplies PostgreSQL 18.6, `pg_search`
0.26.0, and pgvector 0.8.6. Extension initialization runs in one transaction
and updates pgvector before `pg_search`.

Windows supports external PostgreSQL; managed lifecycle is withheld until
credential ACL behavior can prove equivalent privacy.

### Container resources and server settings

New or confirmed-replacement managed containers have explicit Docker limits,
and PostgreSQL starts with bounded buffer, per-operation memory, connection,
and parallel-worker settings inside those limits.

| Setting | Value |
| --- | --- |
| Docker memory ceiling | 2 GiB |
| Docker memory reservation | 1 GiB |
| CPU quota | Four CPUs |
| Process ceiling | 256 |
| Shared memory | 256 MiB |
| Checkpoint interval (`checkpoint_timeout`) | 15 minutes |
| Soft maximum WAL size (`max_wal_size`) | 2 GB |
| Recycled-WAL floor (`min_wal_size`) | 256 MB |
| WAL compression (`wal_compression`) | `lz4` |

The checkpoint and WAL settings trade bounded disk and crash recovery time for
fewer checkpoint/full-page-write cycles during generation bursts while keeping
`fsync` and synchronous commit enabled. An idle database does not checkpoint
leftover WAL away, so `max_wal_size` is also each project's steady-state WAL
footprint; `lz4` compression offsets the extra full-page images a smaller cap
causes. Containers created before v2.1.33 keep the earlier 4 GB maximum WAL
size and 512 MB floor, without WAL compression, until they are replaced.

Read-only status surfaces the observed Docker values and whether they match the
supported policy, and reports `postgres_settings`:

| `postgres_settings` | Meaning | What to do |
| --- | --- | --- |
| `current` | The container runs the current server settings | Nothing |
| `outdated` | An older container keeps working with earlier settings | Run `db upgrade`, which replaces the container on the same data volume |
| `absent` | No managed container exists | Nothing to compare until `db start` creates one |

Adopting the policy for an older owned container remains a backup-gated,
explicitly confirmed replacement.

## V1 import and retention

### V1 import

V2 imports only from a v1.1.33 PostgreSQL schema in the same database as a
distinct v2 destination. That destination may already have a current
generation; import publishes a new immutable one. Operators quiesce project
index/sync/hook/rebuild writers to avoid wasted work. Dry-run validates source
schema/history, bounded streaming legacy JSON, supported language/path/content identity, rows,
coordinates, hashes, relations, canonical facts, and memory/output admission.
Mutation uses exact leases, staged/ready/BM25/complete checkpoints, rollback, and
same-input resume. It never reads SQLite.

A concurrent publisher cannot corrupt or strand an import: the stale
generation is failed/released atomically, the caller receives
`ConcurrentPublication`, and an identical retry can reset the failed durable
run and allocate a newer sequence after writers are quiesced.

Legacy multiplicity is retained. A span is exact only when the stored/current
source proves the full token; otherwise it is explicitly coarse. SCIP
placeholder hashes cannot prove historical bytes v1 never stored.

### Generation retention

Every successful index or no-op reconciliation attempts bounded generation and
parse-cache retention after publication/no-op detection. A failed automatic
index also attempts bounded terminal-generation retention, and a cross-revision
capacity circuit stops after five unresolved capacity failures.

**Resumable drain.** Retention marks each selected generation `retiring` and
drains it with bounded, independently committed transactions that walk each
large relation in key order, so committed progress survives a later timeout.
A keyset batch resumes strictly after the last key it deleted; small relations
without a key suffix use a plain bounded sweep.

| Drain bound | Value |
| --- | --- |
| Rows per transaction | At most 10,000 |
| Generations per transaction | At most 32 |
| Transaction deadline | 10 seconds |
| Transactions per invocation | At most 512 |
| Generation-row delete | Savepoint with a deadline of at most 2 seconds, clamped so 1.5 seconds remain to verify and commit child progress |

| Report outcome | Meaning |
| --- | --- |
| Generation stays `retiring` | Partially drained; a later invocation resumes it |
| `deferred_reason: "parent_delete_deferred"` | The generation-row delete, which runs every cascading foreign-key check, exceeded its savepoint bound; the savepoint rolled back without discarding the drained child rows, and a later prune or autovacuum lets it finish |
| `deferred_reason: "work_budget_reached"` | Retiring work remains, or a call-level budget (cascade rows, generation deletions, statement deadline, or transaction count) bounded the whole invocation |

A later transaction that fails after earlier commits records its error reason
instead of discarding that committed progress.

**Pre-admission backpressure.** Before an automatic index attempt reserves a
generation, it counts failed and retiring generations. When more than one
remains, it runs one bounded drain first; if that drain made progress and more
than one still remains, the attempt is deferred with retryable
`retention_backlog` and the previous generation stays visible. A drain that
commits nothing (another operation holds the project, or a search-relation
budget or catalog check blocks it) admits the attempt rather than freezing
automatic indexing. Explicit indexes are never deferred.

**Cache and maintenance.** The current extractor
contract is always cache-protected; one recent older contract plus independent
row/logical-byte/deletion caps prevent unbounded parser-version accumulation.
Automatic cleanup delegates thresholded relation reclamation to the tuned
autovacuum policy, keeping synchronous 27-table vacuuming out of the watcher
critical path. Explicit prune requests retain the thresholded table-scoped
maintenance step and report whether it completed or was deferred.

**Explicit prune.** `db prune` uses the same bounded engine for larger explicit
batches of stale staging/ready, failed, and old superseded generations, always
preserving current, recent/leased work, import recovery state, and configured
recent histories. Its generation-count limit is independent from the
64-derived-relation DDL cap, so relation-free failed backlogs can use the full
requested bounded batch. Retention locks publication, rechecks its exact
migration lease before each commit, drops selected derived BM25 relations
transactionally, and reports admitted cascade rows, relation count, and physical
relation bytes. Status and doctor expose all generation-state counts and a
conservative retained-byte estimate.

<details>
<summary>Details: terminal-failure cleanup and generation eligibility</summary>

Terminal failure cleanup also deletes the exact generation's PostgreSQL spill
root in the same fenced transaction when the cascade fits the cleanup statement
deadline, so its staging payload becomes reusable immediately. A cascade that
outlives the deadline rolls back to a savepoint instead of failing the cleanup:
the generation still fails and releases its lease, and the bounded retention
drain later removes its spill rows with its canonical rows.

Pre-supervisor failures attempt to
terminalize their exact staging generation under the same project advisory lock
used by lease acquisition; when that lock stays held past the bounded cleanup
wait, the generation stays `staging` and the next writer's recovery fails it.

A later batch can collect staging only when it is old, unleased, and not
referenced by an incomplete import. Ready work becomes eligible only after a
longer age floor when it is unleased, non-current, and outside import recovery.

</details>

### Storage usage and compaction

- Routine status reads compact whole-database and schema heap/index/TOAST
  totals under a separate five-second bound and preserves the rest of status if
  those totals are unavailable.
- `db usage` reads a repeatable, bounded storage snapshot with schema
  heap/index/TOAST, cache, generation, dead-row, autovacuum, invalid-index, and
  deduplication evidence. Content-addressed fact sharing is deliberately
  assessment-only until a schema migration can preserve immutable generation
  identity and cascades.
- `db compact` dry-runs by default and can rebuild eligible B-trees one at a
  time with PostgreSQL's concurrent reindex path after explicit confirmation and
  headroom proof.
- The separate `db compact --heap` plan measures reclaimable allocation and its
  explicitly confirmed apply rewrites one allowlisted relation at a time with
  `VACUUM FULL`, verified headroom, a schema maintenance gate, and an
  existing-operation lease preflight. Heap rewrites are never part of routine
  status, indexing, pruning, or automatic maintenance.

## Security, durability, and licensing

- Inputs, rows, bytes, tasks, output, deadlines, and retries have hard caps.
- Dynamic identifiers are parsed/quoted; user query text is always bound.
- Secret/query/path text is omitted from public errors and debug output.
- Source reads stay within a canonical project root and reject unsupported/
  oversized/non-UTF-8 data.
- Release archives are allowlisted and scanned for local paths/database bits.
- Community ParadeDB BM25 is treated as rebuildable local derived state, not
  crash-durable replicated production state.
- Shared/hosted/paid use requires a separate durability and AGPL/commercial
  licensing decision. Cartograph does not bundle the extension/image.

See [the distribution policy](LICENSING.md).

## Release evidence

Stable release requires format, strict Clippy, workspace tests, cargo-deny,
SQLite-free dependency/source proofs, live PostgreSQL/ParadeDB capability and
fault suites, semantic/import/retention/agent evaluations, deterministic worker
matrices, Rust LCOV plus Sonar quality gate, structural floor, independent
review, native archive privacy/smoke audits, four-platform 64-bit CI builds
(Linux x64 and arm64, macOS arm64, Windows x64), checksums,
provenance, signed tag, and exact tag/main/release SHA identity.

### Remote gate attestation

The complete remote gate runs once for the exact published `main` SHA. Its
required jobs produce a GitHub-attested SHA-bound manifest whose
`requiredJobs` set must be exactly:

| Required job | Coverage |
| --- | --- |
| `quality` | Format, Clippy, rustdoc, unit tests, dependency and release/documentation contracts, PostgreSQL-only source proofs |
| `windows-portability` | Windows strict compile portability |
| `macos-portability` | macOS strict compile portability |
| `linux-release-portability` | Linux x64 Debian 13 current-stable build, audit, and smoke |
| `paradedb/database`, `paradedb/runtime`, `paradedb/operations` | Sharded live PostgreSQL 18 + ParadeDB + pgvector suites |

A tag-triggered release must verify that
manifest's repository, workflow, source digest, `refs/heads/main` source ref,
GitHub-hosted runner provenance, and required-job set before any platform build.
The tag workflow then performs only release-specific four-platform build,
archive, checksum, provenance, and publication work; it never substitutes the
attestation for the local Sonar or independent-review requirements.

### Publication checks

Before it publishes, the tag workflow requires:

- the tag to equal `v` plus the `cartograph-cli` Cargo version;
- the tag commit to equal both the built SHA and the published `main` head;
- tracked, non-empty release notes at `docs/releases/<tag>.md` that name the
  tag in their `# Cartograph <tag>` heading and contain no placeholder markers;
- any existing release for the tag to still be a draft; it refuses to replace
  assets on a published release.

Primary upstream references:

- [ParadeDB repository and license](https://github.com/paradedb/paradedb)
- [Create a BM25 index](https://docs.paradedb.com/documentation/indexing/create-index)
- [Code tokenizer](https://docs.paradedb.com/documentation/tokenizers/source-code)
- [Relevance boosts](https://docs.paradedb.com/documentation/sorting/boost)
- [BM25 scoring](https://docs.paradedb.com/documentation/sorting/score)
- [pgvector HNSW](https://github.com/pgvector/pgvector#hnsw)
