# Configuration

[Documentation home](README.md) · [Project overview](../README.md) ·
[Storage and operations](STORAGE-BACKENDS.md) · [Troubleshooting](TROUBLESHOOTING.md)

Cartograph keeps non-secret project policy in `.cartograph/config.json`.
API credentials belong in the process environment or private
managed state, never in a committed file. This page is the reference for every
project-configuration key and environment variable; read it before you change
source admission, capacity limits, native storage, or optional model
tiers.

**On this page:** [Settings at a glance](#settings-at-a-glance) ·
[Where settings live](#where-settings-live) ·
[Project configuration example](#project-configuration-example) ·
[Source and evidence policy](#source-and-evidence-policy) ·
[Native storage](#native-storage) ·
[Auto-sync and MCP routing](#auto-sync-and-mcp-routing) ·
[Optional LLM tiers](#optional-llm-tiers)

## Settings at a glance

### Project configuration keys

Every key is optional. Byte values are plain integers in the file.

| Key | Default | Accepted values | Details |
| --- | --- | --- | --- |
| `version` | `2` when Cartograph writes a file | `2` for new files; below `2` is a legacy v1 file | [Legacy v1 files](#legacy-v1-files) |
| `languages` | all supported modes | Stable language-mode allowlist; empty means all | [Source policy](#source-and-evidence-policy) |
| `include` | omitted (all admitted paths) | At most 4,096 globs of at most 4,096 bytes each | [Excluding paths](#excluding-paths) |
| `exclude` | `[]` plus built-in exclusions | At most 4,096 globs of at most 4,096 bytes each; `!` re-includes | [Excluding paths](#excluding-paths) |
| `maxFileSize` | 32 MiB | 1 byte through 32 MiB | [Source policy](#source-and-evidence-policy) |
| `maxAstDepth` | `256` | 64 through 1024 | [Source policy](#source-and-evidence-policy) |
| `maxGenerationBytes` | 1 GiB | 1 byte through 8 GiB | [Generation storage](#generation-storage) |
| `generationStorage` | legacy input | Accepted for old configuration compatibility; no runtime backend selection | [Generation storage](#generation-storage) |
| `maxSpillBytes` | legacy input | Retired spill setting; no effect in native generation construction | [Generation storage](#generation-storage) |
| `maxSpillRows` | legacy input | Retired spill setting; no effect in native generation construction | [Generation storage](#generation-storage) |
| `extractDocstrings`, `trackCallSites`, `indexSubmodules`, `indexEmbeddedRepos` | `true` | boolean | [Source policy](#source-and-evidence-policy) |
| `enableCentrality`, `enableBetweenness`, `enableChurn`, `enableCoChange`, `enableBiomarkers`, `enableIssueHistory` | `true` | boolean | [Source policy](#source-and-evidence-policy) |
| `enableConfigRefs`, `enableSqlRefs`, `enableBuildContextRefs`, `enableStringImports` | `true` | boolean | [Source policy](#source-and-evidence-policy) |
| `duplicateCodePartialClones` | `false` | boolean | [Source policy](#source-and-evidence-policy) |
| `duplicateCodeAllowlist` | `[]` | At most 4,096 globs of at most 4,096 bytes each | [Source policy](#source-and-evidence-policy) |
| `dependenciesAllowlist` or `analysis.dependenciesAllowlist` | none | See command help | [Analysis policy](#analysis-policy) |
| `layers`, `layerExceptions` | none | Bounded schema; see command help | [Analysis policy](#analysis-policy) |
| `llm.enabled` | `true` | boolean; `false` disables every project tier | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.summarize` | `true` | boolean | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.summarizeEagerLimit` | `600` | Negative means uncapped; at most 10,000,000 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.minBodyLines` | `4` | 0 through 1,000,000 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.minBodyLinesByKind` | `{"route": 1}` | At most 128 kinds of at most 64 bytes; values 0 through 1,000,000 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.embeddingLlm`, `summarizeLlm`, `localLlm`, `askLlm`, `classifyLlm`, `rerankerLlm` | absent | Tier object or `null` | [Model tiers](#embedding-reranker-and-chat-tiers) |
| `llm.decisionLlm` | absent | Typesafe Jev tier | [Jev navigation](#optional-jev-navigation) |
| Tier `timeoutMs` | 120,000 (embedding; chat except `anthropic-api`); 60,000 (`anthropic-api` chat, reranker); 5,000 (`decisionLlm`) | 1 through 600,000; `decisionLlm` at most 30,000 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| Tier `concurrency` | operation default | 1 through 16 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| Tier `summaryBatchSize` | 1 (`openai-compat`); 3 (bridges, `anthropic-api`) | 1 through 16 | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| Tier `askModel` | none | Model used when the ask tier falls back to this tier | [LLM policy keys](#llm-policy-keys-and-tier-limits) |
| `llm.decisionLlm.features` | absent (exploration only) | At most 16 names | [Jev features](#jev-features) |

### Environment variables

| Variable | Default | Accepted values | Details |
| --- | --- | --- | --- |
| `CARTOGRAPH_WATCH_DEBOUNCE_MS` | 150 | 50..60000 ms; an invalid value uses 150 | [Auto-sync and MCP routing](#auto-sync-and-mcp-routing) |
| `CARTOGRAPH_ALLOWED_PROJECTS` | unset (no extra restriction) | OS path list of project directories | [Auto-sync and MCP routing](#auto-sync-and-mcp-routing) |
| `CARTOGRAPH_EMBEDDING_ENDPOINT`, `CARTOGRAPH_EMBEDDING_MODEL` | unset | Set both or neither; base URL or complete `/v1/embeddings` URL; model at most 256 bytes | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_EMBEDDING_API_KEY` | unset | At most 8 KiB | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_EMBEDDING_TIMEOUT_MS` | 120000 | 1..600000 | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_EMBEDDING_MAX_BATCH` | 32 | 1..128 inputs per request | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_EMBEDDING_MAX_INPUT_BYTES` | 2 MiB | 1 byte..16 MiB per request | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_EMBEDDING_MAX_RESPONSE_BYTES` | 16 MiB | 1 byte..64 MiB | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_ENDPOINT`, `CARTOGRAPH_CHAT_MODEL` | unset | Set both or neither; model at most 256 bytes | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_API_KEY` | unset | At most 8 KiB | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_TIMEOUT_MS` | 120000 | 1..600000 | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_MAX_INPUT_BYTES` | 512 KiB | 1 byte..4 MiB | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_MAX_RESPONSE_BYTES` | 2 MiB | 1 byte..16 MiB | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `CARTOGRAPH_CHAT_MAX_OUTPUT_TOKENS` | 4096 | 1..32768 | [Environment overrides](#environment-overrides-for-embeddings-and-chat) |
| `TYPESAFE_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` | unset | Default credential variables when a tier names no source | [Credential sources](#credential-sources) |

## Where settings live

| Concern | Location | Commit it? |
| --- | --- | ---: |
| Shared source admission, extraction, graph, and capacity policy | `.cartograph/config.json` | Yes, when the team shares the policy |
| Additional Git-style source exclusions | `.cartographignore` | Yes |
| Native graph and agent state | `.cartograph/data` | No |
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

### Concurrent edits and file limits

- Cartograph rewrites `.cartograph/config.json` (for example during
  `cartograph llm setup`) while holding a lock on `.cartograph/config.lock`. A
  second writer waits up to 2 seconds for that lock, then fails with
  `Cartograph project configuration changed concurrently`.
- The same error is returned, and nothing is written, when the file changed on
  disk after Cartograph read it. Re-run the command to apply your change on top
  of the new content.
- The file is capped at 1 MiB; a larger file fails with
  `Cartograph project configuration is too large`.
- Writes are atomic (a synced temporary file replaces the original) and leave
  the file with mode `0600`.

## Source and evidence policy

| Option | Meaning | Default |
| --- | --- | --- |
| `version` | Config contract version. Use `2` for new files | `2` when Cartograph writes a file |
| `languages` | Stable language-mode allowlist; empty means every supported mode | all |
| `include` | Project-relative glob allowlist; omitted means all admitted paths. At most 4,096 patterns of at most 4,096 bytes each | omitted |
| `exclude` | Additional project-relative glob exclusions. At most 4,096 patterns of at most 4,096 bytes each | `[]` plus built-in exclusions |
| `maxFileSize` | Per-source byte ceiling, 1 byte through 32 MiB | 32 MiB (the maximum) |
| `maxAstDepth` | Defensive syntax-tree depth ceiling, 64 through 1024; overflow retains a partial file and reports its exact path | `256` |
| `maxGenerationBytes` | Cartograph-process canonical/resolver ceiling, 1 byte through the hard maximum of 8 GiB; out-of-range errors name this field and range | 1 GiB |
| `generationStorage` | Legacy compatibility input; ignored by the native builder | retired |
| `maxSpillBytes` | Legacy compatibility input; no native spill quota is selected | retired |
| `maxSpillRows` | Legacy compatibility input; no native spill row quota is selected | retired |
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
| `duplicateCodeAllowlist` | Globs exempt from duplicate-code findings. At most 4,096 patterns of at most 4,096 bytes each | `[]` |

### Legacy v1 files

V1 config files remain readable. For a legacy `version` below 2, Cartograph
adds `.pyi` and `.toml` admission when an old explicit include list would
otherwise hide v2's additive coverage. New configuration should use version 2.

### Excluding paths

Discovery follows Git-compatible ignore behavior, then applies explicit
Cartograph policy. There are three ways to exclude a path, and all of them apply
to `sync-if-dirty` and the managed Git hooks, because an exclusion that stops
holding the moment the index refreshes is not an exclusion.

| Form | Scope | Semantics |
| --- | --- | --- |
| `exclude` array in `.cartograph/config.json` | Durable, shared | Globs; `!` re-includes |
| `cartograph index --exclude <GLOB>` (repeatable) | The generation it publishes | Globs; `!` re-includes |
| `.cartographignore` carrying patterns | Its directory tree | gitignore semantics |

- `--exclude` is scoped to the generation it publishes. That generation stores
  the policy privately so status, `changed-since`, source reads, auto-sync, and
  upgrade reconcile the same admitted tree. A later explicit index without
  `--exclude` clears the generation-scoped list; automatic reconciliation
  inherits it.
- Both `exclude` forms take globs — `benches/**`, `**/*_generated.rs` — and a
  `!` prefix re-includes a path an earlier exclusion matched, so excluding a
  directory to dodge one file does not throw away everything else in it:

  ```json
  { "exclude": ["benches/**", "!benches/src/lib.rs"] }
  ```

- An *empty* `.cartographignore` keeps its original meaning: it excludes its
  whole directory tree, and at the project root it opts the entire checkout out
  of indexing.
- A `.cartographignore` with patterns is instead a gitignore-semantics ignore
  file, which means it also inherits git's own rule that a file cannot be
  re-included once a parent directory has been excluded — use the `exclude`
  array when you need that re-include.

Exclusions are never silent. Native index metrics report `excluded_paths`, the
files a configured exclusion skipped, and `excluded_trees`, the directory
subtrees skipped without being descended into. A pruned tree is deliberately not
expanded into a file count: walking it to produce one would defeat the pruning
that keeps discovery bounded on vendored and generated directories.

### Generation storage

RocksDB is the only durable runtime. `maxGenerationBytes` bounds canonical
construction; resolve and validation have separate finite working allowances.
Native queue, request, record, and mutation budgets remain independent, so an
8 GiB accepted configuration ceiling is not a promise that every such graph
fits the runtime. Oversized work fails explicitly without replacing current
published evidence.

`generationStorage`, `maxSpillBytes`, and `maxSpillRows` are accepted only for
legacy configuration compatibility. They no longer select PostgreSQL spill or
change the native generation builder. Remove them from new configurations.
`storageBackend: "rocksdb"` records a verified migration, not a backend choice.

See [storage operations](STORAGE-BACKENDS.md) for the per-project directory,
worker/cache budgets, retention, and current scan-based retrieval limits.

### Analysis policy

Dependency audit allowlists are read from `dependenciesAllowlist` or
`analysis.dependenciesAllowlist`. Architecture-layer policy uses `layers` and
`layerExceptions`; see command help and emitted validation errors for its
bounded schema.

## Native storage

Every project uses `.cartograph/data` with one primary owner. There is no SQL
connection, schema, port, connection pool, or TLS configuration. Retired
`CARTOGRAPH_DATABASE_*` and `CARTOGRAPH_MANAGED_DATABASE_PORT` variables do not
select a backend. Old database CLI flags are rejected.

On Unix, clients reuse a live MCP primary through its private same-user owner
socket. Windows currently requires one process at a time for a given project.
Native I/O uses bounded dedicated workers with finite deadlines/cancellation.
See [storage operations](STORAGE-BACKENDS.md) for policy values and limits.

## Auto-sync and MCP routing

| Variable | Effect |
| --- | --- |
| `CARTOGRAPH_WATCH_DEBOUNCE_MS` | MCP auto-sync quiet window, 50 through 60,000 ms; an unset, unparsable, or out-of-range value uses the 150 ms default. See [Performance tuning](PERF-TUNING.md#auto-sync-watcher) for the coalescing deadline |
| `CARTOGRAPH_ALLOWED_PROJECTS` | Restricts the MCP `projectPath` argument. When set, it is an OS path list (`:`-separated on Unix, `;` on Windows) of directories; a routed project must be the server's own project, a directory below it, or one of the listed directories or below it. Entries that do not resolve to an existing directory are ignored. Other projects fail with `projectPath is outside CARTOGRAPH_ALLOWED_PROJECTS`. When unset, any initialized project can be routed |

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

Without `decisionLlm`, exploration stays native. `--decision native`, summary
and low-token exploration also skip Jev.

#### Supplying the key to an MCP host

MCP hosts start `cartograph serve --mcp` without your interactive shell's
environment. Instead of putting the key in the host's `env` block or wrapping the
server in a secret-manager launcher, you can let the server fetch it with a
[credential command](#credential-sources):

```sh
cartograph llm setup . --preset jev \
  --api-key-command /path/to/secret-helper --api-key-arg get --api-key-arg typesafe-api-key
```

```json
"decisionLlm": {
  "provider": "typesafe",
  "endpoint": "https://api.typesafe.ai/v1/systemone",
  "model": "jev-1.13.0",
  "apiKeyCommand": ["/path/to/secret-helper", "get", "typesafe-api-key"]
}
```

The host registration then stays a plain `cartograph serve --mcp`. If the
helper fails (a locked store, an expired session), only Jev is affected.

### Jev data sharing and navigation bounds

> [!NOTE]
> Enabling the tier permits sending the exploration question, candidate metadata
> (name, kind, bounded signature, path and lines) and bounded source excerpts to
> Typesafe.

Each request asks Jev's parallel questions against shared state:

- one chooses the next allowed operation;
- one assesses source sufficiency; and
- one judges each unread candidate's relevance (up to 24 per round).

Candidates judged relevant are read together in the same round as the chosen
operation, and navigation finishes as soon as sufficiency reaches 0.85 with
source present. Most explorations therefore need one or two provider round
trips. The model and endpoint are pinned; Jev uses its typed decision API, not a
chat endpoint.

| Bound | Value |
| --- | --- |
| Candidates judged per round | Up to 24 |
| Relevance that triggers a read | Probability at least 0.5 |
| Relevant candidates read per round | Up to four |
| Sufficiency that ends navigation | 0.85, with source present |
| Request timeout (`timeoutMs`) | Five seconds by default; at most 30,000 |
| Provider state | Under 60 KiB |
| Operations | Seven |
| Candidate identities | 40 |
| Additional source window | 4 KiB each |
| Native windows passed to navigation | Up to 16 KiB |
| Deadline | 30 seconds, covering navigation and its freshness checks |

<details>
<summary>Details: reranking, freshness, cancellation, and the <code>navigation</code> object</summary>

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
reason. Candidates carry the latest advisory `relevance` probability once
judged. Assistance is bounded to seven operations, 40 candidate identities,
4 KiB per additional source window and a 30-second deadline covering navigation
and its freshness checks. On expiry, Cartograph cancels and joins
navigation-owned work; joining an active filesystem read may add cleanup
latency. `maxFiles` bounds the original source windows; the separate navigation
supplement has its own seven-operation bound.

Navigation receives up to 16 KiB of the native windows already returned to the
caller and reports `nativeSourceWindows` (the included count) and
`nativeSourcesTruncated`. Complete included windows are not offered for a
redundant read. Candidates include current-generation kinds and line ranges;
callers/callees actions are offered only for functions and methods. Native
windows and additional windows both contribute to the sufficiency question.

</details>

### Jev failures and diagnostics

| Outcome | Cause | What to do |
| --- | --- | --- |
| `provider_unavailable` with a redacted `providerError` | Missing keys, invalid responses, HTTP failures and rate limits | Check the key and provider status; native evidence is still returned |
| `providerError: "credential_missing"` | The key is absent; the safe `providerErrorDetail` names the configured environment variable | Set that variable in the MCP server process (or its secret-manager launcher), not only an unrelated shell, or use a credential command |
| `providerError: "credential_unavailable"` | A credential command failed; its detail names only the program and exit status | Fix the helper (for example unlock the store) |
| Step limit or abstention | Navigation stopped before sufficiency | Evidence already captured is retained |
| Request aborted | Source or generation changes, or a deadline that prevents final freshness verification | Retry after the change settles |

- Model confidence and sufficiency are advisory scores, not proof that the
  question is answered.
- Smoke output retains the configured model and endpoint on failure.
- `cartograph doctor` reports a configured variable that is unset in its own
  shell as a warning, because the server reads its own environment; an invalid
  tier still fails.

### Jev features

The tier's optional `features` list selects which surfaces may consult Jev.
Without it, only exploration does. Add `context` to let `context` rank its
retrieval candidates, `roles` to classify symbol roles, and `rename` to triage
rename mentions:

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

| Feature | Surface | What is sent |
| --- | --- | --- |
| `explore` | `explore` / `cartograph_explore` (the default when `features` is absent) | Question, candidate metadata, bounded source excerpts |
| `context` | [Context ranking](#jev-context-ranking) | Task text and candidate metadata; never indexed source |
| `roles` | [Role classification](#jev-role-classification) | Symbol metadata with signatures up to 160 bytes; never function bodies |
| `rename` | [Rename triage](#jev-rename-triage) | Source text: each mention's source line up to 200 bytes |

The list holds at most 16 names of at most 64 bytes (lowercase ASCII letters
and `_`); unknown names are kept so a newer configuration stays readable, and
are ignored. An empty `features` list disables every surface while keeping the
tier configured. `find`, `graph`, indexing and test selection keep their
existing policies.

### Jev context ranking

Context ranking sends the task text (up to 1,024 bytes, which can include
anything pasted into the task) and, for up to 24 BM25/semantic candidates, their
qualified name, document kind, path and line range; it never sends indexed
source. One request judges every candidate's relevance in parallel. When
context ranking is enabled the local cross-encoder is skipped.
`mode: deterministic` never consults Jev.

<details>
<summary>Details: reordering, reported fields, and provider failure</summary>

Candidates are reordered by that advisory probability within the positions
retrieval candidates already occupied, so exact anchors and graph expansion keep
their places, and each judged item reports `decision_relevance`. The packet's
`decision_rank` block records the model, outcome (`applied`, `no_candidates`
or `provider_unavailable` with a redacted `provider_error`) and judged count.
Unless an exact anchor selected them, primary edit candidates become the files of
the relevant judged items (probability at least 0.5) in ranked order, with basis
`decision_relevance`. Compact and plan projections report `decisionRank` and a
per-item `relevance`; their `rank` remains the retrieval fusion rank. If the
provider then fails, the packet is rebuilt through the configured reranker and
keeps the `provider_unavailable` outcome, so an outage adds at most the request
timeout (`timeoutMs`, five seconds by default) to the default ranking.

</details>

### Jev decision reuse and accounting

Project-scoped decision clients share a process-local cache of validated
responses. It retains at most 256 entries and 4 MiB of response bytes for five
minutes from insertion; reads do not extend expiry. Nothing is written to the
project database or disk. Keys privately hash the canonical project scope,
endpoint, effective credential identity, and complete versioned request,
including instructions and criteria. Context and navigation requests also
include their generation identity. Source validation still runs independently
when cached decisions are used. A changed model, credential, question, or
supplied evidence cannot match the old key.

At most 32 in-flight producers and 32 coalesced waiters are retained.
Excess requests bypass reuse and remain subject to ordinary transport admission.
Failures are not cached. A cancelled producer closes its flight and releases
waiters to the normal explicit fallback; it never starts detached provider work.
One request timeout covers credentials, cache waiting, transport queueing and
HTTP. A coalesced waiter keeps its own deadline. Clients constructed directly
from `JevSettings::from_config`, without a project scope, bypass the cache;
the provider smoke continues to make a real request.

Successful responses expose locally measured accounting through
`decision_rank.request_metrics` for context, `decisionRequests` for navigation,
`decisionTriage.requests` for rename plans, and `jevUsage.samples` for bulk role
classification. Each record includes `cache` (`miss`, `hit`, `coalesced`, or
`bypassed`), `elapsedMs`, `providerRequests`, `inputTokens`, `outputTokens`, and
`background`. Hits and coalesced waiters report zero provider requests and null
token counts, so they do not duplicate the producer's billed usage. Missing
provider token counts remain null. Credential renewal can make two HTTP
attempts; usage describes the successful response, not an estimated invoice.
Bulk reports cover validated responses observed in their returned batch prefix;
cancelled or failed requests have no invented token counts.
Bulk `jevUsage` retains at most 64 request samples and reports `sampleLimit`
and `omittedSamples`. Aggregate decision, HTTP-attempt and cache-outcome counts
cover all observed responses. Input and output token totals separately report
known tokens, provider responses with and without usage, and any numeric
saturation; reuse is not counted as an unknown provider response.

Bulk Jev role classification uses background transport admission: at most three
background requests occupy an origin's four active slots. Interactive decisions
use foreground admission. Decision reuse and background enrichment do not gate
native graph publication or structural freshness.

The opt-in Rust test `live_fixed_candidate_context_ranking_pilot` compares Jev
ordering with the current native ordering on the same retrieval candidate pool
for 24 frozen repository questions. It requires `CARTOGRAPH_JEV_EVAL_ALLOW_METADATA=1`,
`CARTOGRAPH_JEV_EVAL_PROJECT`, `CARTOGRAPH_JEV_EVAL_BINARY`, and a new private
`CARTOGRAPH_JEV_EVAL_OUTPUT` path. Run it with
`cargo test --locked -p cartograph-agent --lib live_fixed_candidate_context_ranking_pilot -- --ignored --nocapture`.
The project must be fresh, have Jev configured, and keep `context` decisions
disabled for its native control. The pilot sends task text and candidate metadata
under the explicit test opt-in without changing configuration. It records
generation and case fingerprints, expected implementation ranks, provider usage,
and cold/warm decision timings. Those timings measure the added decision,
separately from the complete native query; the pilot does not measure a full
replacement pipeline or establish thresholds for enabling a default.

### Jev role classification

Add `roles` to let role classification consult Jev when no `classify` chat
tier is configured.

- High-confidence structural rules still decide test code (test directories,
  test file names and `tests` modules), routes, framework declarations and data
  declarations (types, enum members, fields and constants).
- For every other symbol, `admin classify` and post-index enrichment send its
  qualified name, kind, project-relative path, language, declaration signature
  (up to 160 bytes, which can contain literals such as default values) and
  export flag, in requests of 24 symbols. Function bodies and other source are
  never sent.
- A role is accepted only when Jev gives it at least 0.6 probability;
  otherwise the name, location and export heuristics apply, then `unknown`.
- Accepted roles record `via: jev`, the probability and model
  `jev-1.13.0+roles-v1`; `role` with `via: auto` uses the same path for symbols
  without a structural role.

<details>
<summary>Details: retries, rejected batches, and measured accuracy</summary>

A failed request is retried once. If the provider still rejects a batch (an
HTTP 4xx or an invalid answer set) while other batches were judged, its symbols
keep the heuristic role with a `jev_rejected_` reason and the sweep continues.
If every batch is rejected, or the provider is unavailable (including HTTP 5xx),
the sweep keeps what it judged, reports `jevError`, and leaves the rest for the
next sweep. A rules-only sweep never replaces roles that Jev or a chat model
already judged, so turning a model off keeps its results; a different model
re-judges them. On this repository, structural rules alone cut unknown roles
from 61% to 25% of 24,369 symbols, and Jev cut them to 9.5%; 58 of 60 sampled
Jev roles were correct on review.

</details>

### Jev rename triage

Add `rename` to let `propose_rename` triage its textual mentions. Word-boundary
mentions outside the graph's exact references are review-only; with `rename`,
Jev judges whether each returned mention refers to the renamed symbol.

> [!NOTE]
> Unlike the other surfaces, this sends source text: the symbol's qualified
> name, kind, path, line and signature (up to 160 bytes), and for each mention
> its path, line, enclosing symbol and source line (up to 200 bytes), in
> requests of 24.

Each mention gains `decisionProbability` and `triage`: `likely_other` at 0.3 or
below, otherwise `textual_review_required`.

<details>
<summary>Details: why only the negative label is offered, and reported fields</summary>

Only the negative label is offered: on hand-labelled plans, mentions at 0.3 or
below were other symbols or generic words in 29 of 30 cases, while high
probabilities mixed the renamed symbol with same-named helpers and string
labels. The plan's `decisionTriage` records the model, outcome (`applied`,
`no_mentions` or `provider_unavailable` with a redacted `providerError`) and
judged count; a failed request is retried once, and on failure mentions keep
only their review label.

</details>

### Verify or disable Jev

`llm smoke` can verify the configured key with a small real request. Disable
Jev with:

```sh
cartograph llm setup . --preset jev --clear-credentials
```

This removes the decision tier. It does not clear other provider tiers.

### Low-token exploration

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

| Provider | Use |
| --- | --- |
| `openai-compat` | Local or cloud OpenAI-compatible HTTP |
| `anthropic-api` | The Anthropic Messages API |
| `cli-bridge` | A bounded shell-free local command |
| `claude-bridge` | Backward-compatible legacy Claude CLI configuration |

Embedding and reranker tiers require OpenAI-compatible HTTP. Optional tier
fields include bounded `timeoutMs`, `concurrency`, `summaryBatchSize`,
`apiKeyEnv` or `apiKeyCommand`, legacy `claudeBin`, generic `command`/`args`/`input`/
`promptTemplate`/`responseFormat`/`responsePath`, `llamaServerArgs`, and
`externallyManaged` where applicable. Inline legacy keys are read for
compatibility but an environment reference or credential command is the safe
configuration.

#### Generic CLI bridge

A generic bridge names an executable and an ordered argv template. Only
`{model}` and `{prompt}` are substituted, directly into argv without a shell.

- `input: "stdin"` writes the rendered prompt to stdin and forbids `{prompt}` in
  argv; `input: "arg"` requires exactly one `{prompt}` token.
- `promptTemplate` is optional and defaults to
  `# System\n{system}\n\n# User\n{user}`.
- Bounded stdout can be decoded as trimmed `raw` text, through a validated
  `json-path` such as `.messages[-1].content`, or through the legacy `claude`
  envelope:

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

### LLM policy keys and tier limits

| Key | Default | Bound and meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` disables every project-configured tier |
| `summarize` | `true` | Generated summaries; also off when `enabled` is `false`. Without any `llm` object, summaries are off |
| `summarizeEagerLimit` | `600` | Symbols summarized per post-index enrichment sweep, and the limit of a summary sweep that names none; `0` skips post-index summaries; a negative value means uncapped; at most 10,000,000 |
| `minBodyLines` | `4` | Minimum body lines for a symbol to be a summary candidate, 0 through 1,000,000 |
| `minBodyLinesByKind` | `{"route": 1}` | Per-kind overrides of `minBodyLines`: at most 128 kinds, each name at most 64 bytes, values 0 through 1,000,000 |
| Tier `timeoutMs` | 120,000 (embedding; chat except `anthropic-api`); 60,000 (`anthropic-api` chat, reranker); 5,000 (`decisionLlm`) | 1 through 600,000; `decisionLlm` accepts at most 30,000 |
| Tier `concurrency` | operation default | 1 through 16 |
| Tier `summaryBatchSize` | 1 (`openai-compat`); 3 (`cli-bridge`, `claude-bridge`, `anthropic-api`) | 1 through 16 |
| Tier `askModel` | none | Model used when the ask tier falls back to this (summarize) tier. Without it, `anthropic-api` and `claude-bridge` use `claude-sonnet-4-6` and other providers use the tier's `model` |
| `decisionLlm.features` | absent (exploration only) | At most 16 names; see [Jev features](#jev-features) |

When `model` is omitted, `anthropic-api` and `claude-bridge` default to
`claude-haiku-4-5` for `summarizeLlm` and `claude-sonnet-4-6` for other tiers;
`openai-compat` and `cli-bridge` require a model. When `endpoint` is omitted,
`openai-compat` uses `https://api.openai.com` and `anthropic-api` uses
`https://api.anthropic.com`.

### Environment overrides for embeddings and chat

The process environment can supply an OpenAI-compatible embedding or chat
endpoint without editing the project file. The environment pair is read before
the project `llm` object.

- Setting both `CARTOGRAPH_EMBEDDING_ENDPOINT` and `CARTOGRAPH_EMBEDDING_MODEL`
  overrides the project `embeddingLlm` tier.
- Setting both `CARTOGRAPH_CHAT_ENDPOINT` and `CARTOGRAPH_CHAT_MODEL` overrides
  every chat tier (`summarizeLlm`, `localLlm`, `askLlm`, `classifyLlm`). It does
  not affect `rerankerLlm` or `decisionLlm`.
- Setting only one variable of a pair fails closed with an incomplete
  configuration error.
- Endpoints must use HTTPS, or HTTP to a loopback host, and must not contain
  user information, a query, or a fragment.
- The `*_API_KEY`, `*_TIMEOUT_MS`, and `*_MAX_*` variables are read only with an
  environment endpoint/model pair; a project tier uses its own `timeoutMs` and
  credential source and the default request limits. Numeric values must be
  integers from 1 through the maximum in the
  [environment variable table](#environment-variables); anything else fails
  closed.

### Credential sources

A remote tier (`openai-compat`, `anthropic-api` or `typesafe`) takes its
credential from at most one source. Configuration naming more than one is
rejected, and the shell-free bridges accept none:

| Source | Behavior |
| --- | --- |
| `apiKeyEnv` | Names an environment variable read by the process that uses the tier: the MCP server for `cartograph_*` tools, your shell for CLI commands. Without it, Jev reads `TYPESAFE_API_KEY`, `anthropic-api` reads `ANTHROPIC_API_KEY`, and `openai-compat` without an `endpoint` reads `OPENAI_API_KEY` |
| `apiKeyCommand` | An argv array, for example `["/path/to/secret-helper", "get", "typesafe-api-key"]`. Only the argv is stored. The serving process runs it directly, without a shell, the first time the tier needs a credential, not at startup |
| Legacy inline `apiKey` | Still read; move it with `cartograph llm migrate-credentials` |

`apiKeyCommand` bounds:

- The argv has the CLI-bridge bounds: a non-empty program and at most 128
  arguments of up to 4 KiB each and 32 KiB in total.
- The command gets no stdin, its stderr is discarded, and it must finish within
  10 seconds and print at most 4 KiB.
- Trailing whitespace is trimmed; the rest must be non-empty, control-free
  UTF-8.

> [!WARNING]
> Like a CLI-bridge command, a credential command is trusted project
> configuration: Cartograph and `doctor` run whatever program
> `.cartograph/config.json` names, so review that file before using a checkout
> you do not trust.

<details>
<summary>Details: how a command credential is cached, rotated, and reported</summary>

A resolved command credential is kept only in that process's memory. It is
never written to configuration, logs, session history or diagnostics. When the
provider rejects it (HTTP 401 or 403), the command runs once more and the
request is resent only if the value changed, so a rotated key is picked up
without a restart. Such a re-run happens at most once every 30 seconds, even
when each run prints a new value. A failed run reports `credential_unavailable`
with the program's file name and exit status, never its output, and is
remembered for 30 seconds before a later use runs the command again. A failure
affects only that tier: Jev falls back to native retrieval exactly as for a
missing variable, and every other tool keeps working.

</details>

Setup preserves the configured source while the provider/endpoint origin is
unchanged, clears it when the origin changes, and removes it with
`--clear-credentials`. `cartograph doctor` and `cartograph llm smoke` run a
configured command with the same bounds and report only whether it produced a
credential.

### Low-load deployments

A low-load deployment may configure only `embeddingLlm` and `rerankerLlm` and
set `summarizeLlm`, `askLlm`, `localLlm`, and `classifyLlm` to `null`.
`cartograph llm smoke` tests configured tiers and reports those absent
generative tiers as explicit skips. Reranking scores the deduplicated lexical and
eligible semantic shortlist after reciprocal-rank fusion, up to 100 candidates.
Each source-bearing input is capped at 1,536 UTF-8 bytes, with name and intent
before code, and is never included in Cartograph's serialized search response.
Original channel scores remain visible beside the separate `rerank_score`.
Reranker failure retains fusion ordering and reports the exact outcome.

### Migrate inline credentials

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

### Setup wizard and presets

Use the native planner/wizard instead of hand-writing provider details:

```sh
cartograph llm setup
cartograph llm smoke .
cartograph doctor .
```

Without `--preset`, the planner detects available endpoints and recommends a
configuration. `--preset` applies one of these non-interactively:

| `--preset` | Configures |
| --- | --- |
| `jev` | The optional Jev decision tier ([Optional Jev navigation](#optional-jev-navigation)) |
| `install-llama-cpp` (alias `local-llama-cpp`) | llama.cpp |
| `install-ollama` (alias `ollama`) | Ollama |
| `install-mlx` | An MLX OpenAI-compatible endpoint, from `--tier`, `--endpoint`, and `--model` |
| `cloud-open-ai` | Cloud OpenAI |
| `cloud-open-ai-compat` | A cloud OpenAI-compatible endpoint, from `--tier`, `--endpoint`, and `--model` |
| `cli-bridge` | The generic CLI bridge |
| `hybrid-claude-bridge` | Hybrid Claude bridge |
| `hybrid-anthropic-api` | Hybrid Anthropic API |
| `custom` | A custom OpenAI-compatible endpoint, from `--tier`, `--endpoint`, and `--model` |
| `skip` | Skips provider setup |

`cartograph backend` manages only explicitly configured local `llama-server`
processes; external providers remain operator-owned.

The MCP process reloads LLM configuration at the next LLM operation. Agent-host
MCP registration or binary replacement still requires a host restart.
