# Cartograph v3 architecture

[Documentation home](../README.md) · [Project overview](../../README.md) ·
[Native extraction](EXTRACTION.md) · [Language matrix](../SUPPORT-MATRIX.md)

Last release audit: 2026-10-19 (`v3.0.0`).

Cartograph v3 is a native Rust code-intelligence server for AI coding agents.
Each project owns an embedded RocksDB 11.8.1 store. Rust implements
code-aware BM25 and model-scoped exact cosine retrieval. Exact,
lexical, graph, review, freshness, and affected-test workflows require no LLM.

Read this page for the trust boundaries, storage contracts, and bounds behind
each subsystem. Operator commands live in the [CLI reference](../CLI-REFERENCE.md)
and the [storage guide](../STORAGE-BACKENDS.md).

## Hard boundaries

- Native Rust owns extraction, storage, retrieval, and transport. No PostgreSQL,
  SQLX, SQLite, Bun, or Node runtime is retained.
- One project primary owns `.cartograph/data`; every typed feature service
  shares it. Unix owner IPC routes additional CLI/MCP processes to the primary.
  Windows cross-process proxying is not implemented yet.
- External input and source evidence have task/item/byte/deadline admission.
  Blocking native calls run on owned workers; cancellation retains work until
  its actual completion is known. A committed WAL sync is never reported as a
  rollback solely because the caller cancelled.
- Parallel extraction is reduced deterministically before digest/publication.
  Identities, confidence, unresolved references, and freshness stay explicit.
- Current source remains authoritative. Optional LLM decisions cannot replace
  typed structural truth. Model failures preserve explicit degraded states.

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
| `cartograph-config` | Project source/capacity policy, native migration markers, bounded atomic configuration I/O |
| `cartograph-extract` | Bounded discovery/read/hash, native tree-sitter parsing, declarations/references, deterministic resolution |
| `cartograph-db` | Native engine, worker admission, writer fences, atomic revisions, snapshots, typed feature state/retrieval, retention |
| `cartograph-indexer` | Bounded parallel stages, deterministic reduction, supervisor/cancellation/reaping, corpus-aware workers |
| `cartograph-search` | Exact/BM25/hybrid evidence, typed intent, graph traversal, RRF, affected tests, trust/abstention |
| `cartograph-scip` | Bounded zero-runtime protobuf codec, exact typed-edge extension, deterministic export, per-file replacement overlay |
| `cartograph-llm` | Bounded redacted embedding/reranker/chat/decision clients, provider config, shell-free `apiKeyCommand` credential sources, model identity, and local backend supervision contracts |
| `cartograph-agent` | Project runtime, freshness, indexing, structural summaries, embedding sweeps, optional decision-guided navigation, Git review, source excerpts, working-tree overlay |
| `cartograph-mcp` | Bounded stdio JSON-RPC/MCP protocol, profiles, cancellation, stable errors |
| `cartograph-cli` | Native command routing, database operations, MCP adapter, project-local agent installation |

### Dependency direction

Dependencies point inward through typed contracts. MCP/CLI consume typed feature APIs;
database code does not read arbitrary project files; extractors do not know
about persistence or transport.

`scripts/test-workspace-dependencies.sh` enforces the reviewed production/build
dependency directions, rejects cycles and forbidden production dependencies, and requires an explicit boundary decision for a new workspace crate.
Neutral project configuration is owned by `cartograph-config`; the LLM crate
retains provider interpretation and compatibility wrappers. Structural symbol
summary sweeps expose a typed agent service used by the CLI/MCP adapter.

### Shared source windows

Context source windows share one bounded source-manifest scan and captured file
set per request. They carry the expected generation and reject a generation
change before returning evidence. Per-file source hashes and source-policy
identity remain authoritative; concurrent requests still perform independent
validation. No time-based freshness cache is introduced.

The strict source-batch API rejects publication changes. Optional MCP context
windows distinguish that outcome from storage, capture, budget, and cancellation
failures, which remain errors. When publication invalidates optional windows,
the response keeps the original graph generation, discards all code windows,
and marks both the response and packet stale with low confidence and abstention.
`sourceWindowStatus` reports `unavailable` with reason `generation_changed`;
successful capture and skipped capture report `available` and `not_requested`.

### Model transport admission

Embedding and rerank clients share a process-bounded transport registry, keyed
privately by effective endpoint/model/credential/timeout settings. Chat
transports remain separate.

| Bound | Value |
| --- | --- |
| Active slots per HTTP origin | Four, with at most three occupied by background embeddings or bulk decisions |
| Queued requests | 32, including at most 16 background waiters |
| Retained transports | At most 32 |
| Retained origin gates | At most 32 |

Queueing consumes the same deadline as the HTTP request, and cancellation
releases admission.

Project-scoped Jev clients reuse exact validated responses in a process-local
cache bounded by 256 entries, 4 MiB of response bytes, and five minutes from
insertion. Private keys bind canonical project scope, endpoint, credential and
the complete model/question/evidence request. Generation-dependent context and
navigation include generation identity; cache hits never bypass live-source
fences. There is no durable decision-cache growth. At most 32 producers and
32 coalesced waiters share work. The requesting future owns each producer;
cancellation closes the flight, releases its slot and preserves native fallback.
Every waiter retains its original deadline, including credentials and admission.
Only successful validated responses are cached, and responses obtained after
credential renewal are not stored under the rejected credential's identity.
Source-free request accounting distinguishes provider work from cache reuse;
unreported usage remains unknown. Bulk role decisions use background admission
and never gate structural graph publication.

## Native engine and record schema

The exact engine and Rust binding are pinned in Cargo.lock. Column families
separate revision metadata, versioned facts, packed graph blocks, source/cache
records, and mutable feature state. Internal schema/version checks fail closed;
there is no SQL migration ledger or alternate engine connection.

Native feature services validate project/generation/model identity before reads
and atomic state writes. Notes, sessions, coverage, generated artifacts, history,
SCIP, semantic state, and summary priority share the same primary owner.
Records and batches have independent byte/row ceilings. Public errors redact
source/query data, credentials, driver internals, and absolute checkout paths.

## Project identity and source freshness

The canonical checkout path is hashed under a domain separator. The native store
binds that identity to its per-project database. Live source is compared against
the complete publication's recorded source-admission policy and content
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
native publication and BM25 tests. Unknown discovery
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
The V23 generation-digest contract fences these families,
v1 framework/bridge detail, and deferred resolution parity, so generations
from older binaries report stale once and republish.

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
contract (anything before the current V23) remain explicitly stale until an
ordinary index republishes them; for generations older than V7, which predate
numerical sites, that republication is also what publishes numerical evidence.

See [native extraction](EXTRACTION.md) and the
[extension guide](../EXTENDING-EXTRACTORS-RESOLVERS.md).

## Bounded parallel indexing

Discovery and source hashing feed native parse/resolve/reduce stages with
bounded tasks and bytes, monotonic progress, deadlines, cancellation, and
retained cleanup. Extraction workers admit 1..16; frozen corpus checks require
identical counts, identities, and digests at 1/2/4/8/16 workers.

After a complete build, the existing project writer can retain its bounded
canonical allocation without copying it. A warm local path re-extracts up to
16 small supported-language files and compares typed resolution inputs exhaustively.
XML template-role body hints also fence reuse. When
identities, spans, topology and global clone effects remain unchanged, it
validates replacement relations against the merged symbol view, recomputes the
ordinary complete digest and publishes only the file record changes. Base
generation and content digest both fence sparse publication. Unsupported or
unproven edits and explicit `maxGenerationBytes` ceilings use the complete builder; the warm allocation is dropped before
that build. A new process warms only after its first complete build.

Complete source-only scans hash at most four files concurrently with bounded
queues and source buffers, then reduce observations in deterministic path order.
An incomplete scan may restart from discovery at most twice under the original
deadline and cancellation control; partial observations are discarded.
A bounded observed manifest can feed the complete pipeline's supervised read
stage without repeating discovery and hashing. Parse cache hits still validate
identities, misses read exact bytes, and complete source checks fence publication.

The complete builder still reduces a finite whole-generation working set. Legacy
`generationStorage` and spill settings are compatibility inputs with no native
backend-selection effect. `maxGenerationBytes` and independent queue/record/
mutation budgets fail oversized work safely; RocksDB does not remove global
resolution/centrality memory bounds.

The publisher compares canonical records against the prior revision, writes
changed values and tombstones in bounded batches, builds packed adjacency
and optional name postings under the same workspace limit,
validates counts/digest, and atomically changes the current pointer with a
synchronous WAL commit. Failed staging revisions cannot become current.

Four dedicated native workers isolate blocking I/O from Tokio. Item and byte
admission reserve read capacity while mutations run. CLI pending reservations
are 512 MiB; this is not a process RSS limit. Cache/write-buffer policy is
shared across column families. Snapshots preserve complete prior revisions
while current publication proceeds.

Source verification before/after publication detects concurrent edits. Watcher
retries are bounded and report capacity suppression separately from ordinary
source changes. Native async scheduling does not enable engine AsyncIO or
io_uring automatically.

## SCIP interchange without a visual viewer

Export reads one immutable current-generation snapshot and then requires
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
- centrality is recomputed, and canonical validation runs before publication.

Import forces publication and restores the previous artifact when
publication fails and no competing writer replaced the requested bytes. If the
forced index fails or is cancelled and restoring the previous overlay also
fails, the requested artifact may still be installed for the next index; that
failure is reported beside the primary failure (or the cancellation) as
`overlayRollbackFailure` (`code: scip_overlay_rollback_failed`) without
replacing it. See
[troubleshooting](../TROUBLESHOOTING.md#scip-import-reports-overlayrollbackfailure).

## Writer fencing, snapshots, and retention

Each project owns one primary with an exclusive native lock. Writer fences
reserve a revision; stale or cancelled work cannot publish over a newer writer.
Readers hold immutable snapshots and protected revision visibility. Publication
and mutable-state transactions recheck their original request controls.

Graph retention drains obsolete versions in finite resumable batches while
preserving the current revision and live snapshots. Auxiliary retention tracks
orphan grace markers, validates embedded record identity before deletion, and
removes marker state when an explicit delete finishes. Parse-cache and derived
cleanup have independent phase reports; a later failure does not erase earlier
committed progress. Automatic failures remain visible in maintenance telemetry.

## BM25 and exact retrieval

Exact identities, paths, references, and packed adjacency use native keyed
records. BM25 stores exact field-aware term postings, forward document records,
and complete weighted corpus statistics atomically with canonical documents.
Sparse replacement updates only affected terms and statistics; deletion removes
obsolete terms. Bootstrap derives postings from every canonical document. The
index does not change canonical identities or digests. Older generations and
vocabularies exceeding the optional per-document workspace use complete bounded
corpus search. Exact query terms read all matching postings into a 32 MiB
candidate workspace, then hydrate bounded top-K from the same snapshot. Capacity,
ID/term allocations, native rows, bytes, cancellation, and deadline remain
bounded. Common-term work scales with matching postings, and exhaustion is
explicit.

## Semantic and hybrid retrieval

Models fix provider/name/fingerprint, dimension, and normalization. Native
vectors remain model- and generation-scoped, validated, and exact-cosine scored
under finite read/workspace/time budgets. There is no HNSW index in this
implementation. Model readiness proves active identity, matching dimensions,
and document coverage separately from structural graph freshness.

A shared owner cache retains readiness and at most one optional 64 MiB packed
corpus. Its key includes the exact generation, validated model selector, and
vector/summary epochs from one native snapshot. All corresponding mutations
advance those epochs. Native pinned MultiGet reads batches through that same
snapshot, with bounded native values and copied bytes. Only one builder is
admitted, and a replacement waits until old readers release their corpus;
busy or oversized acceleration falls back to exact streaming. Warm scoring
still computes exact cosine distances with stable document-ID ties.

Readiness is `ready`, `not_configured`, `not_indexed`, `stale`, or `unavailable`.
Budget exhaustion and missing model evidence remain explicit. Historical
pgvector/HNSW performance is not a claim about this path.

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
The same-user owner transport preserves a bounded, validated request tool
ceiling. Nested macros and project-scoped handlers intersect it with their own
immutable definitions without replacing the original deadline or cancellation.
Product handlers call the agent/search services; transport never reaches
through to SQL or filesystem internals.

The CLI exposes the same typed services plus native storage maintenance and project-local
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

## Storage operations, durability, and distribution

RocksDB 11.8.1 is compiled into the executable with LZ4/Zstandard support.
There is no database server/container lifecycle. Native WAL sync, atomic batch
publication, and snapshot readers provide local crash-consistent revisions;
physical reclamation follows LSM compaction after logical deletion.

`db usage` reports files, WAL, SSTs, memtables, and backlog. `db prune` is bounded
and confirmed. No online checkpoint/restore CLI is implemented yet. Source
migration preserves legacy PostgreSQL resources and does not import their
user-authored state; see [migration](ROCKSDB-MIGRATION.md).

Unix owner IPC validates same-user peers, private paths, bounded connections,
frames, replies, and deadlines. Completed admin job reports are retained in a
bounded history, even while a later job runs. Cancellation/shutdown preserves
active ownership until work actually completes.

Native libraries now form part of each release executable; their license text
and notices are packaged and allowlist-audited. See [licensing](LICENSING.md).

## Release evidence

Stable release requires format, strict Clippy, workspace tests, cargo-deny,
SQLite-free dependency/source proofs, native RocksDB/native storage capability and
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
| `quality` | Format, Clippy, rustdoc, unit tests, dependency and release/documentation contracts, native-only dependency and source proofs |
| `windows-portability` | Windows strict compile portability |
| `macos-portability` | macOS strict compile portability |
| `linux-release-portability` | Linux x64 Debian 13 current-stable build, audit, and smoke |
| `rocksdb/database`, `rocksdb/runtime`, `rocksdb/operations` | Sharded native database, worker, owner, maintenance, and CLI suites |

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

- [native storage repository and license](https://github.com/paradedb/paradedb)
- [Create a BM25 index](https://docs.paradedb.com/documentation/indexing/create-index)
- [Code tokenizer](https://docs.paradedb.com/documentation/tokenizers/source-code)
- [Relevance boosts](https://docs.paradedb.com/documentation/sorting/boost)
- [BM25 scoring](https://docs.paradedb.com/documentation/sorting/score)
- [RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)
