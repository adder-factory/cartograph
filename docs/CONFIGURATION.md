# Configuration

[Documentation home](README.md) · [Project overview](../README.md) ·
[Storage and operations](STORAGE-BACKENDS.md) · [Troubleshooting](TROUBLESHOOTING.md)

Cartograph keeps non-secret project policy in `.cartograph/config.json`.
Database URLs and API credentials belong in the process environment or private
managed state, never in a committed file.

## Where settings live

| Concern | Location | Commit it? |
| --- | --- | ---: |
| Shared source admission, extraction, graph, and capacity policy | `.cartograph/config.json` | Yes, when the team shares the policy |
| Additional Git-style source exclusions | `.cartographignore` | Yes |
| Database URL, schema override, pool, and query deadlines | Process environment | No secrets |
| Optional embedding, reranking, and chat credentials | Process environment or Cartograph-managed private state | No |
| One-command experiment or recovery override | The exact CLI flag shown by command help | No persistent change unless documented |

## Project configuration example

The file is optional. A representative v2 configuration is:

```json
{
  "version": 2,
  "languages": ["typescript", "python", "rust"],
  "include": ["src/**", "tests/**"],
  "exclude": ["vendor/**", "generated/**"],
  "maxFileSize": 5242880,
  "maxAstDepth": 256,
  "maxGenerationBytes": 1073741824,
  "generationStorage": "auto",
  "maxSpillBytes": 137438953472,
  "maxSpillRows": 1000000000,
  "extractDocstrings": true,
  "trackCallSites": true,
  "enableCentrality": true,
  "enableBetweenness": true,
  "enableChurn": true,
  "enableCoChange": true,
  "enableBiomarkers": true,
  "enableIssueHistory": true,
  "enableConfigRefs": true,
  "enableSqlRefs": true,
  "enableBuildContextRefs": true,
  "enableStringImports": true,
  "duplicateCodePartialClones": false,
  "duplicateCodeAllowlist": ["generated/**"]
}
```

## Source and evidence policy

| Option | Meaning | Default |
| --- | --- | --- |
| `version` | Config contract version. Use `2` for new files | `2` when Cartograph writes a file |
| `languages` | Stable language-mode allowlist; empty means every supported mode | all |
| `include` | Project-relative glob allowlist; omitted means all admitted paths | omitted |
| `exclude` | Additional project-relative glob exclusions | `[]` plus built-in exclusions |
| `maxFileSize` | Per-source byte ceiling, 1 byte through 32 MiB | runtime default |
| `maxAstDepth` | Defensive syntax-tree depth ceiling, 64 through 1024; overflow retains a partial file and reports its exact path | `256` |
| `maxGenerationBytes` | Cartograph-process canonical/resolver ceiling, 1 byte through the hard maximum of 8 GiB; out-of-range errors name this field and range | 1 GiB |
| `generationStorage` | Native working-set strategy: `auto`, `memory`, or `postgres` | `auto` |
| `maxSpillBytes` | Logical sort-key/payload quota for one incomplete PostgreSQL spill, through 1 TiB | 128 GiB |
| `maxSpillRows` | Raw extraction/fact row quota for one incomplete PostgreSQL spill, through 10 billion | 1 billion |
| `extractDocstrings` | Retain safe structural documentation evidence | `true` |
| `trackCallSites` | Retain reference-site provenance | `true` |
| `indexSubmodules` | Include Git submodules | `true` |
| `indexEmbeddedRepos` | Include detected nested repositories; disabled when submodules are disabled | `true` |
| `enableCentrality` | Compute native PageRank | `true` |
| `enableBetweenness` | Compute bounded sampled Brandes betweenness | `true` |
| `enableChurn` | Derive bounded Git churn evidence | `true` |
| `enableCoChange` | Derive bounded Git co-change evidence | `true` |
| `enableBiomarkers` | Compute deterministic code-health findings | `true` |
| `enableIssueHistory` | Derive issue-tagged symbol history | `true` |
| `enableConfigRefs` | Add configuration/environment-reference evidence | `true` |
| `enableSqlRefs` | Add embedded SQL relation evidence | `true` |
| `enableBuildContextRefs` | Add build/container context evidence | `true` |
| `enableStringImports` | Add bounded import-shaped literal evidence | `true` |
| `duplicateCodePartialClones` | Enable the wider Type-3 partial-clone band | `false` |
| `duplicateCodeAllowlist` | Globs exempt from duplicate-code findings | `[]` |

V1 config files remain readable. For a legacy `version` below 2, Cartograph
adds `.pyi` and `.toml` admission when an old explicit include list would
otherwise hide v2's additive coverage. New configuration should use version 2.

Discovery follows Git-compatible ignore behavior, then applies explicit
Cartograph policy. There are three ways to exclude a path, and all of them apply
to `sync-if-dirty` and the managed Git hooks, because an exclusion that stops
holding the moment the index refreshes is not an exclusion.

- The `exclude` array in `.cartograph/config.json` is the durable, shared form.
- `cartograph index --exclude <GLOB>` is repeatable and scoped to the generation
  it publishes. That generation stores the policy privately so status,
  `changed-since`, source reads, auto-sync, and upgrade reconcile the same
  admitted tree. A later explicit index without `--exclude` clears the
  generation-scoped list; automatic reconciliation inherits it.
- A `.cartographignore` file carrying patterns is honored with gitignore
  semantics.

Both `exclude` forms take globs — `benches/**`, `**/*_generated.rs` — and a `!`
prefix re-includes a path an earlier exclusion matched, so excluding a directory
to dodge one file does not throw away everything else in it:

```json
{ "exclude": ["benches/**", "!benches/src/lib.rs"] }
```

An *empty* `.cartographignore` keeps its original meaning: it excludes its whole
directory tree, and at the project root it opts the entire checkout out of
indexing. A `.cartographignore` with patterns is instead a gitignore-semantics
ignore file, which means it also inherits git's own rule that a file cannot be
re-included once a parent directory has been excluded — use the `exclude` array
when you need that re-include.

Exclusions are never silent. Native index metrics report `excluded_paths`, the
files a configured exclusion skipped, and `excluded_trees`, the directory
subtrees skipped without being descended into. A pruned tree is deliberately not
expanded into a file count: walking it to produce one would defeat the pruning
that keeps discovery bounded on vendored and generated directories.

`generationStorage: "auto"` keeps the lower-latency memory path for small
manifests and selects PostgreSQL spill when any of these conservative signals
is reached: 64 Cargo manifests, 10,000 supported files, 64 MiB of indexed
source, or a 16x source expansion estimate at or above `maxGenerationBytes`.
`memory` and `postgres` force the respective path. The selected strategy and fixed-size spill
accounting are returned in native index metrics. This physical working-set
choice does not change the logical source digest, so changing it does not make
an otherwise fresh generation stale; use `cartograph index --force` when you
want to rebuild unchanged source with a different strategy.

Persistent SCIP overlays support all three choices. They use the same
source-verified replacement rules in memory and PostgreSQL, with bounded
compiler-fact batches before spill reduction. The overlay's native basis and
imported payload remain subject to `maxGenerationBytes` working limits.

On the memory path, `maxGenerationBytes` bounds the reduced canonical
generation; resolve and validation have separately measured working allowances
of up to four times that value. On the PostgreSQL path, bulky per-file
extraction and resolved facts do not accumulate in one Rust generation payload.
`maxGenerationBytes` instead remains the independent bound for a file-local
batch plus the compact project-wide resolution, clone, and centrality indexes.
`maxSpillBytes` and `maxSpillRows` bound the whole durable unordered payload.
The byte value is logical payload accounting, not a prediction of PostgreSQL
heap/index/WAL/temporary-disk use. Keep database storage and temporary-space
headroom above it.

The spill is tied to the exact staging generation and live lease. File-local
parse batches either reference immutable cache payloads or retain a bounded
inline fallback. Resolved typed relation batches are immutable and
digest-fenced; exact replay is idempotent, while a different retry fails
closed. PostgreSQL reduces six relations through 64 deterministic UUID
partitions each, commits four contiguous partitions at a time, proves
cross-relations within those transactions, can use its own temporary storage
for grouping/sorting, and streams exact V18 row bytes from final canonical
rows. The final ready transaction rechecks the fence, completed-validation
phase (`canonicalized`), counts, and digest capability before it deletes spill
state. Only the later short publication transaction changes the current pointer.

The memory path still publishes with bounded COPY statements (100,000 rows or
64 MiB of encoded data per statement). PostgreSQL spill writes canonical rows
before ready and therefore skips that redundant COPY payload. Neither strategy
removes the compact global resolution/clone/centrality bound; extreme symbol or
call-graph cardinality can still fail safely with
`generation_capacity_exceeded`. Auto-sync reports `maxGenerationBytes`, its
Cartograph-process scope, and the recovery action; after five capacity failures
across any source revisions it suppresses further automatic attempts until an
explicit index succeeds. Persistent SCIP replacement overlays currently
remain on the memory path in `auto`; forcing `postgres` while an overlay exists
is rejected rather than silently changing overlay semantics.

Dependency audit allowlists are read from `dependenciesAllowlist` or
`analysis.dependenciesAllowlist`. Architecture-layer policy uses `layers` and
`layerExceptions`; see command help and emitted validation errors for its
bounded schema.

## PostgreSQL settings

Cartograph v2 is PostgreSQL-only. Database ownership, connection, schema, pool,
and TLS selection are not project-config options; use the environment or
private managed state. `generationStorage` only chooses the native construction
working-set strategy. It does not select a different durable database engine.

```sh
export CARTOGRAPH_DATABASE_URL='postgresql://cartograph:secret@127.0.0.1:5432/cartograph'
export CARTOGRAPH_DATABASE_SCHEMA='cartograph_project'
export CARTOGRAPH_DATABASE_MAX_CONNECTIONS=8
export CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS=5000
export CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS=120000
export CARTOGRAPH_DATABASE_REQUIRE_SSL=true
```

| Variable | Bound/default |
| --- | --- |
| `CARTOGRAPH_DATABASE_URL` | Required for an external database; `postgres`/`postgresql` URL with a host |
| `CARTOGRAPH_DATABASE_SCHEMA` | ASCII identifier, 1..63 bytes; default `cartograph` |
| `CARTOGRAPH_DATABASE_MAX_CONNECTIONS` | 1..64; default 8 |
| `CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS` | 1..120000; default 5000 |
| `CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS` | 1..600000; default 120000 |
| `CARTOGRAPH_DATABASE_REQUIRE_SSL` | `true`/`false` or `1`/`0`; default false |

When `cartograph db start` owns the database, the runtime resolves the private
project-local credential instead. There is no SQLite provider, importer,
migration target, or pgvector-off mode.

## Optional LLM tiers

The project `llm` object controls optional embeddings, reranking, generated
summaries/roles, ask, and local chat. Structural graph and retrieval features
remain usable without it.

### Optional Jev navigation

Jev can choose retrieval actions for `cartograph explore` / `cartograph_explore`.
The calling assistant sends the code question to Cartograph; Cartograph asks
Jev to select bounded source, callers, callees, exact-name and file-outline
lookups, executes those lookups locally, and returns the evidence to the
assistant. Jev does not generate the final answer or replace the code index.

Bring your own Typesafe API key through the environment of the CLI or MCP host:

```sh
# Supply TYPESAFE_API_KEY through your shell or secret manager first.
cartograph llm setup . --preset jev --api-key-env TYPESAFE_API_KEY
cartograph explore 'how is request cancellation handled?'
cartograph explore 'how is request cancellation handled?' --decision native
```

Setup writes only an environment-variable reference. It preserves existing
embedding, reranker and chat settings, and adds this optional tier:

```json
{
  "llm": {
    "decisionLlm": {
      "provider": "typesafe",
      "endpoint": "https://api.typesafe.ai/v1/systemone",
      "model": "jev-1.13.0",
      "apiKeyEnv": "TYPESAFE_API_KEY"
    }
  }
}
```

Enabling the tier permits sending the exploration question, candidate metadata
(name, kind, bounded signature, path and lines) and bounded source excerpts to
Typesafe. Each request asks Jev's parallel questions against shared state: one
chooses the next allowed operation, one assesses source sufficiency, and one
judges each unread candidate's relevance (up to 24 per round). Candidates judged
relevant (probability at least 0.5) are read together, up to four per round, in
the same round as the chosen operation, and navigation finishes as soon as
sufficiency reaches 0.85 with source present. Most explorations therefore need
one or two provider round trips. The model and endpoint are pinned; Jev uses its
typed decision API, not a chat endpoint. The default request timeout is five
seconds; `timeoutMs` accepts at most 30,000.

When navigation will consult a usable decision tier, exploration skips the local
cross-encoder reranker: Jev judges and reads the candidates itself, and the
packet reports reranking as not requested. If the provider then fails, the
packet keeps its unreranked vector order and navigation reports
`provider_unavailable`. `--decision native`, summary and low-token exploration
keep the configured reranker. Navigation discloses evidence captured under the
exploration request's own freshness check; its closing source check rejects the
result if files changed while it ran. Provider state is kept under 60 KiB by
first omitting signatures and then older additional source windows.

Exploration always retains its native packet and source windows. The additional
`navigation` object records the model, generation, decisions (each with its
provider `round`), confidence, candidate truncation, source windows and stop
reason. Candidates carry the latest advisory `relevance` probability once judged. Assistance is bounded to
seven operations, 40 candidate identities, 4 KiB per additional source window
and a 30-second deadline covering navigation and its freshness checks. On expiry,
Cartograph cancels and joins navigation-owned work; joining an active filesystem
read may add cleanup latency. `maxFiles` bounds the original source
windows; the separate navigation supplement has its own seven-operation bound.
Navigation receives up to 16 KiB of the native windows already returned to the
caller and reports `nativeSourceWindows` (the included count) and
`nativeSourcesTruncated`. Complete included windows are not offered for a
redundant read. Candidates include current-generation kinds and line ranges;
callers/callees actions are offered only for functions and methods. Native
windows and additional windows both contribute to the sufficiency question.
Missing keys, invalid responses, HTTP failures and rate limits report
`provider_unavailable` with a redacted `providerError`. Step limit and abstention
retain evidence already captured. Source or generation changes, or a deadline
that prevents final freshness verification, abort the request. Model confidence and
sufficiency are advisory scores, not proof that the question is answered.
An absent key reports `providerError: "credential_missing"`; the safe
`providerErrorDetail` names the configured environment variable. Smoke output
retains the configured model and endpoint on failure. Set that variable in the
MCP server process (or its secret-manager launcher), not only an unrelated shell.

Without `decisionLlm`, exploration stays native. `--decision native`, summary
and low-token exploration also skip Jev.

The tier's optional `features` list selects which surfaces may consult Jev.
Without it, only exploration does. Add `context` to let `context` rank its
retrieval candidates, and `roles` to classify symbol roles (below):

```sh
cartograph llm setup . --preset jev --api-key-env TYPESAFE_API_KEY \
  --jev-features explore,context
```

```json
"decisionLlm": {
  "provider": "typesafe",
  "model": "jev-1.13.0",
  "apiKeyEnv": "TYPESAFE_API_KEY",
  "features": ["explore", "context"]
}
```

Context ranking sends the task text (up to 1,024 bytes, which can include
anything pasted into the task) and, for up to 24 BM25/semantic candidates, their
qualified name, document kind, path and line range; it never sends indexed
source. One request judges every candidate's relevance in parallel.
Candidates are reordered by that advisory probability within the positions
retrieval candidates already occupied, so exact anchors and graph expansion keep
their places, and each judged item reports `decision_relevance`. The packet's
`decision_rank` block records the model, outcome (`applied`, `no_candidates`
or `provider_unavailable` with a redacted `provider_error`) and judged count.
Unless an exact anchor selected them, primary edit candidates become the files of
the relevant judged items (probability at least 0.5) in ranked order, with basis
`decision_relevance`. Compact and plan projections report `decisionRank` and a
per-item `relevance`; their `rank` remains the retrieval fusion rank. When
context ranking is enabled the local cross-encoder is skipped. If the provider
then fails, the packet is rebuilt through the configured reranker and keeps the
`provider_unavailable` outcome, so an outage adds at most the request timeout
(`timeoutMs`, five seconds by default) to the default ranking. `mode: deterministic` never consults Jev.

Add `roles` to let role classification consult Jev when no `classify` chat
tier is configured. High-confidence structural rules still decide test code
(test directories, test file names and `tests` modules), routes, framework
declarations and data declarations (types, enum members, fields and
constants). For every other symbol, `admin classify` and post-index enrichment
send its qualified name, kind, project-relative path, language, declaration
signature (up to 160 bytes, which can contain literals such as default values)
and export flag, in requests of 24 symbols. Function bodies and other source are
never sent. A role is accepted only when Jev gives it at least 0.6 probability;
otherwise the name, location and export heuristics apply, then `unknown`.
Accepted roles record `via: jev`, the probability and model
`jev-1.13.0+roles-v1`; `role` with `via: auto` uses the same path for symbols
without a structural role.

A failed request is retried once. If the provider still rejects a batch (an
HTTP 4xx or an invalid answer set) while other batches were judged, its symbols
keep the heuristic role with a `jev_rejected_` reason and the sweep continues.
If every batch is rejected, or the provider is unavailable (including HTTP 5xx),
the sweep keeps what it judged, reports `jevError`, and leaves the rest for the
next sweep. A rules-only sweep
never replaces roles that Jev or a chat model already judged, so turning a model
off keeps its results; a different model re-judges them. On this repository,
structural rules alone cut unknown roles from 61% to 25% of 24,369 symbols, and
Jev cut them to 9.5%; 58 of 60 sampled Jev roles were correct on review.

An empty `features` list disables every surface while keeping the tier
configured.
`find`, `graph`, indexing and test selection keep their existing policies. `llm smoke` can verify the
configured key with a small real request. Disable Jev with:

```sh
cartograph llm setup . --preset jev --clear-credentials
```

This removes the decision tier. It does not clear other provider tiers.

Low-token exploration reports `low_tokens_requested` and returns a minified
packet with at most eight evidence items and a 16 KiB evidence budget. It
retains generation, confidence, abstention, retrieval/fallback/reranker status,
and explicit omission counts, without repeating the full retrieval candidate
list or source windows. `summary` remains a separate source-free presentation.

### Embedding, reranker and chat tiers

```json
{
  "version": 2,
  "llm": {
    "enabled": true,
    "summarize": true,
    "summarizeEagerLimit": 600,
    "minBodyLines": 4,
    "minBodyLinesByKind": { "route": 1 },
    "embeddingLlm": {
      "provider": "openai-compat",
      "endpoint": "http://127.0.0.1:8080",
      "model": "jina-embeddings-v2-base-code"
    },
    "summarizeLlm": {
      "provider": "openai-compat",
      "endpoint": "http://127.0.0.1:8081",
      "model": "/absolute/path/to/chat.gguf",
      "concurrency": 1,
      "summaryBatchSize": 4,
      "llamaServerArgs": ["-c", "8192"]
    },
    "askLlm": {
      "provider": "anthropic-api",
      "endpoint": "https://api.anthropic.com",
      "model": "claude-sonnet-4-6",
      "apiKeyEnv": "ANTHROPIC_API_KEY"
    },
    "rerankerLlm": {
      "provider": "openai-compat",
      "endpoint": "http://127.0.0.1:8083",
      "model": "local-cross-encoder"
    }
  }
}
```

Tier keys retained from v1.1.33 are `embeddingLlm`, `summarizeLlm`, `localLlm`,
`askLlm`, `classifyLlm`, and `rerankerLlm`. Ask/local/classify may fall back to
the summarize tier. Chat providers are:

- `openai-compat` for local or cloud OpenAI-compatible HTTP;
- `anthropic-api` for the Anthropic Messages API;
- `cli-bridge` for a bounded shell-free local command; and
- `claude-bridge` for backward-compatible legacy Claude CLI configuration.

A generic bridge names an executable and an ordered argv template. Only
`{model}` and `{prompt}` are substituted, directly into argv without a shell.
`input: "stdin"` writes the rendered prompt to stdin and forbids `{prompt}` in
argv; `input: "arg"` requires exactly one `{prompt}` token. `promptTemplate`
is optional and defaults to `# System\n{system}\n\n# User\n{user}`. Bounded
stdout can be decoded as trimmed `raw` text, through a validated `json-path`
such as `.messages[-1].content`, or through the legacy `claude` envelope:

```json
{
  "summarizeLlm": {
    "provider": "cli-bridge",
    "command": "some-agent-cli",
    "args": ["-p", "{prompt}", "--model", "{model}"],
    "input": "arg",
    "responseFormat": "raw",
    "model": "some-model",
    "timeoutMs": 60000
  }
}
```

The bridge retains kill-on-drop, a wall-clock deadline, bounded stdout and
stderr, response-size checks, and nonzero-exit rejection. Existing
`claude-bridge`/`claudeBin` blocks continue to load. The
`hybrid-claude-bridge` preset now writes the generic bridge with the historical
Claude argv, stdin prompt bytes, and Claude response decoder.

Embedding and reranker tiers require OpenAI-compatible HTTP. Optional tier
fields include bounded `timeoutMs`, `concurrency`, `summaryBatchSize`,
`apiKeyEnv`, legacy `claudeBin`, generic `command`/`args`/`input`/
`promptTemplate`/`responseFormat`/`responsePath`, `llamaServerArgs`, and
`externallyManaged` where applicable. Inline legacy keys are read for
compatibility but environment lookup is the safe configuration.

A low-load deployment may configure only `embeddingLlm` and `rerankerLlm` and
set `summarizeLlm`, `askLlm`, `localLlm`, and `classifyLlm` to `null`.
`cartograph llm smoke` tests configured tiers and reports those absent
generative tiers as explicit skips. Reranking applies only to bounded semantic
Top-K candidates before reciprocal-rank fusion. The source-bearing candidate
text sent to that operator-configured endpoint is capped and is never included
in Cartograph's serialized search response; reranker failure retains cosine
ordering and reports the exact outcome.

Audit and migrate legacy inline tier keys without printing them:

```sh
cartograph llm migrate-credentials . --json
cartograph llm migrate-credentials . \
  --tier-env summarize=MY_CHAT_API_KEY \
  --apply \
  --confirm migrate-inline-credentials \
  --json
```

The dry run defaults OpenAI-compatible tiers to `OPENAI_API_KEY` and Anthropic
tiers to `ANTHROPIC_API_KEY`; `--tier-env tier=NAME` overrides one tier. Apply
removes an inline value only when the current process proves the named variable
contains the exact same value. The atomic report contains tier, variable name,
and status, never credential material.

Use the native planner/wizard instead of hand-writing provider details:

```sh
cartograph llm setup
cartograph llm smoke .
cartograph doctor .
```

Planner presets cover detected endpoints, llama.cpp, Ollama, MLX/custom
OpenAI-compatible endpoints, cloud OpenAI, the generic CLI bridge, hybrid
Claude bridge, hybrid Anthropic API, and skip. `cartograph backend` manages only explicitly
configured local `llama-server` processes; external providers remain
operator-owned.

The MCP process reloads LLM configuration at the next LLM operation. Agent-host
MCP registration or binary replacement still requires a host restart.
