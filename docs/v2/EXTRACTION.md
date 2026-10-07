# Native extraction contract

[Documentation home](../README.md) · [Project overview](../../README.md) ·
[Language matrix](../SUPPORT-MATRIX.md) ·
[Extension guide](../EXTENDING-EXTRACTORS-RESOLVERS.md)

Cartograph v2 extracts every v1.1.33 language mode in Rust. The extractor owns
bounded project discovery, exact source snapshots, grammar or custom structural
parsing, structural facts, deterministic resolution, and canonical generation
input. It does not depend on PostgreSQL, MCP, or an LLM.

**On this page:** [Language families](#supported-language-families) ·
[Source boundary](#source-boundary) ·
[Source revision](#complete-source-revision) ·
[Parsing and facts](#parsing-and-facts) · [Resolution](#resolution) ·
[Canonical generation](#deterministic-canonical-generation) ·
[Parallel pipeline](#parallel-pipeline) ·
[Search documents](#search-document-boundary) · [Test routing](#test-routing) ·
[Locked verification](#locked-verification)

## Supported language families

The authoritative, exhaustively tested manifest is
`cartograph_domain::SourceLanguage::ALL`: all 73 v1.1.33 modes and their 163
extensions, plus additive `.pyi`, native TOML, 52 dedicated textual
game-scripting modes, the WGSL, Metal, Slang, and WESL shader additions, and
the Ada/SPARK and VHDL additions: 132 production-admitted modes in total. The
implementation is divided into:

| Family | Languages |
| --- | --- |
| Grammar-backed structural walkers | JavaScript/TypeScript, Rust/Python/Go, query-tag, C-family, shell, managed (Java/C#), JVM-dynamic, shader, and Ada/VHDL walkers |
| Dedicated v1-parity families | PHP, Pascal/Delphi, Objective-C, Swift, Dart, F#, ArkTS, Ruby, Lua/Luau/KHN, R, Nix, Clojure/Common Lisp, Lean, ReScript, Solidity, VB.NET, Apex, and HCL/Terraform |
| Astro family (Astro grammar + embedded JavaScript-family walker) | Astro components: frontmatter and template expressions |
| Conservative grammar-backed structural walker | ABAP, GraphQL, HTML, Prisma, SQL, and YAML (GraphQL, Prisma, and SQL have schema-aware slices) |
| Parser-only structural file documents | CSS, embedded templates, JSDoc, JSON/Jupyter, and regex sources |
| Custom host + embedded JavaScript-family walker | Svelte and Vue: the host scans markup; `<script>` blocks and template expressions run through the JavaScript/TypeScript walker |
| Bounded custom scanners | Aura, BG3 Anubis/resources/stats, Liquid, Osiris, properties, TOML, VB6, Visualforce, MyBatis XML, Delphi `.dfm`/`.fmx` form files (inside the Pascal mode), and the [researched game-scripting inventory](GAME-SCRIPTING-LANGUAGES.md) |

The strategy for every mode is listed in the
[coverage report](../LANGUAGE-COVERAGE-REPORT.md#strategy-families), and what
each dedicated family extracts is in the
[support matrix](../SUPPORT-MATRIX.md#language-family-details). The per-file
facts of all 73 v1 modes are checked against v1.1.33's own output by the
[v1 parity oracle](../LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle).

### Embedded component scripts

Astro frontmatter, Vue and Svelte `<script>` blocks, and template expressions
are extracted by the same walker as standalone scripts, not by a line scanner.
One embedded-script mechanism
(`crates/cartograph-extract/src/walk/embedded_script.rs`) parses each region
with the native JavaScript/TypeScript grammar restricted to its exact byte
range through tree-sitter included ranges over the full host bytes:

- every node already carries host byte offsets and host line/column positions,
  so no span is remapped after the fact;
- the regions are walked by the ordinary JavaScript-family walker over a
  dialect view of the host snapshot, sharing the host's symbol ordinals and
  per-file output budget;
- script programs run the complete walker pipeline, and declarations keep
  module-scope qualified names while the file-level component is their graph
  parent and the owner of top-level references;
- template expressions run only the usage traversal: they declare nothing,
  their references belong to the component, names they bind for themselves are
  never resolved against the script, and their cost is proportional to the
  expressions alone.

The Astro family keeps scanning markup nested inside an expression with the
Astro grammar; the Vue/Svelte host scans markup with a lexically aware
template scanner that stays linear in the file size: its structured scan and
its plain fallback each get a byte budget proportional to the source, and a
template that exhausts the structured budget is reported as partial.

### GPU and shader sources

Shaders are where a large share of a renderer's logic lives, and they are
exactly the code that is hardest to navigate by text search. CUDA, GLSL, and
HLSL parse through the C-family walker, and Metal and Slang reuse the same
slice because their shading languages are C++-family languages. Slang adds
module/import resolution, interfaces, generics, and `[shader("stage")]` entry
points. WGSL and WESL share the WGSL grammar slice covering:

- functions, structs and struct members, module-scope `var`/`override`
  bindings, and type aliases;
- entry points typed by pipeline stage, so a `@vertex`, `@fragment`, or
  `@compute` function is a public boundary reachable from host code rather than
  an ordinary internal function;
- intra-file calls and declared-type edges, so callers/callees and impact work
  inside a shader;
- `naga_oil` `#define_import_path` and `#import module::path`, which form the
  shader module graph;
- bounded WESL `import`, nested collection, alias, `package::`, and `super::`
  resolution across `.wesl` and `.wgsl` modules.

Two boundaries are explicit rather than silently absent:

- The pinned grammar accepts `#import module::path` but not the quoted-file
  form, so a quoted import yields a recoverable diagnostic and a partial parse
  instead of a file that looks successfully empty.
- `@group`/`@binding` indices are not spelled into a signature, because a
  literal-bearing signature is rejected before persistence and would blank the
  declared type with it; a binding's declared type is carried as a typed
  reference edge. Correlating those indices with a host-side layout entry needs
  a structured channel and is not yet implemented.

Unknown extensions are excluded by discovery and rejected when explicitly read.
They do not produce a misleading “successful empty graph.” The v1 PostgreSQL
importer applies the same language boundary.

## Source boundary

`SourceRoot` canonicalizes and validates a project directory. A source read:

1. accepts a validated project-relative `NormalizedPath`;
2. verifies the extension is supported;
3. resolves inside the canonical root;
4. requires a regular file;
5. streams bounded chunks while polling cancellation;
6. enforces per-file bytes before and during the read;
7. validates UTF-8 across chunk boundaries;
8. computes the exact BLAKE3 content digest;
9. produces an immutable `SourceSnapshot` with language, path-derived file ID,
   byte size, digest, and source.

Discovery follows Git-compatible ignore rules and has file/path/manifest byte
ceilings. Local Cartograph state, Git internals, build outputs, and ignored
paths do not enter the supported-source revision.

## Complete source revision

Freshness is the digest of an exact file count followed by normalized
path/content-digest pairs in deterministic order. The encoding lives in
`cartograph-domain::SourceManifestDigestBuilder` and is shared by agent status,
source context, indexing, and v1 import. An exact set mismatch fails closed.

Generation freshness additionally requires the current native generation-digest
contract. Contract V22 (migration 49) fences wave 2 of v1 parity: cross-file
resolution across languages, building on V21's per-file extraction parity.
A generation published by an older binary no longer matches what this binary
builds, so an unchanged V21 project reports stale once and publishes new facts.

After an upgrade from an older contract, unchanged source remains stale until a
normal index publishes current-contract facts. This contract check stays
separate from the source-manifest digest so v1 import still compares exact
checkout bytes rather than a binary-specific identity.

<details>
<summary>Details: generation-digest contract history (V5–V22)</summary>

| Contract | What it fences |
| --- | --- |
| V22 | v1 cross-file resolution parity across languages; forces an unchanged V21 project to publish new facts |
| V21 | v1 language parity: dedicated extraction families replace the generic walker for 22 v1 modes, Astro/Vue/Svelte scripts run through the JavaScript/TypeScript walker, and the TypeScript/JavaScript, Python, Go, Rust, JVM, and .NET walkers restore v1's per-file facts; forces an unchanged V20 project to publish new facts |
| V20 | Rust turbofish calls (`f::<T>(..)`, `a::f::<T>(..)`, `x.f::<T>(..)`), which name and resolve their function rather than keeping the type arguments, inside macro arguments too; forces an unchanged V19 project to publish new facts |
| V19 | Rust references inside macro arguments (calls, paths, receiver calls, nested invocations, and std format-string captures); forces an unchanged V18 project to publish new facts |
| V18 | The CUDA 0.21.2 grammar and updated Unicode identifier semantics; forces an unchanged V17 project to publish new facts |
| V17 | The refreshed ArkTS 0.3 and OCaml 0.26 grammar semantics; forces an unchanged V16 project to publish new facts |
| V16 | Nominal Rust self-receiver ownership: syntax-proven receiver types resolve through lexical and import paths, and unresolved or ambiguous self calls never fall back to unrelated project-wide methods |
| V15 | Ada/VHDL unit resolution and guarded numerical precision |
| V14 | First-class Slang/WESL module semantics and JavaScript static dynamic-dispatch evidence |
| V13 | Named TypeScript and JavaScript construction targets |
| V12 | Stable bounded Go and Python anonymous call-target normalization |
| V11 | Anonymous Rust call-target normalization |
| V10 | Call-target-precise secret exposure and incomplete-implementation evidence |
| V9 | JSX executable-line ownership, SQL/document/secret health precision, Python intrinsic and receiver provenance, React lazy default consumers, and TypeScript `typeof` value consumers |
| V8 | Context-classified URL and serial-loop evidence, facade roles, and semantic clone compatibility |
| V7 | Added generation-scoped static numerical sites |
| V6 | Rust Cargo-workspace crate and named re-export resolution semantics |
| V5 | Native-index framework, resolver, and test-ownership evidence |

</details>

## Parsing and facts

`NativeExtractor` chooses either the pinned tree-sitter grammar or the bounded
custom structural scanner from the snapshot's typed language. Grammar-backed
paths use bounded parse callbacks; every path polls cancellation and enforces
fact/string/modeled-output limits. Grammar-backed malformed syntax returns
recoverable diagnostics instead of panicking.

Syntax-tree depth is independently bounded by `maxAstDepth` (default `256`,
range `64..=1024`). A file that exceeds it is retained as a partial file with
the `nesting_limit_exceeded` degraded reason and its exact normalized
path; the remaining project continues through resolution and publication.
Increase the bound only for authored source that legitimately needs deeper
syntax. Generated outputs should normally be excluded through project ignore
rules or `.cartograph/config.json` rather than forcing the whole corpus to
accept their depth.

Optional enrichment facts never make a file unextractable. The Rust, Python,
and Go parity facts (package bindings, struct fields and embedding, literal
instantiations, constant reads, declared-type uses, supertraits, attributes and
decorators, module variables) can multiply the output of dense generated code.
When a pass that recorded any of them exceeds the file's output limit (32 times
source bytes plus a fixed allowance, shared with framework and test-name
enrichment), the file is extracted again without them. It keeps exactly the
facts the extractor produced before those facts existed and carries the
non-degrading `optional_facts_omitted` diagnostic, added only when it fits too;
such a file is not reported as degraded.

<details>
<summary>Details: file-local failures, recoverable partial files, and fatal errors</summary>

An unrecoverable file-local read or extraction failure crosses the public
boundary only as one validated project-relative `NormalizedPath` and an
allowlisted reason. Source drift, unavailable grammar, parser stop,
cancellation, nesting policy, nesting exhaustion, and modeled-output exhaustion
remain distinct. Direct text escapes the relative path, JSON returns a
structured file failure, and MCP admin status retains the same bounded evidence.
Absolute checkout paths, source/parser text, literals, database URLs, and driver
errors are discarded before the indexer supervisor boundary.

An invalid span produced by one grammar-recovery placeholder or a parser stop
without cancellation is recoverable: the file remains in the generation as
explicitly partial with no unsafe facts, and the bounded degraded-file report
records `extraction_invalid_span` or `extraction_parser_stopped`. Missing or
zero-width C-family/Slang declarators are ignored before fact creation, so a
valid neighboring declaration is still extracted. Systemic grammar mismatch or
unavailability, cancellation, invalid policy, and output exhaustion remain
fatal.

</details>

### What the walker emits

The walker emits:

- file and declaration identities;
- symbol kind/name/qualified name/visibility/export/async/static state;
- exact one-based line and byte spans;
- import bindings and module specifiers;
- typed references and containment;
- privacy-safe Rust numerical sites with exact span, operation, potential
  hazard, visible precision, deterministic confidence/provenance, and explicit
  unknowns;
- safe callable signatures and body-search text;
- parser diagnostics.

Callable signatures are normalized to exclude literal-bearing bodies/values.
Search text is intentionally useful for code identifiers without becoming a
secret or source-literal dump.

### Stable targets and bounded names

Immediately invoked Rust closures have no stable declaration target, so their
source bodies are never retained as named call references. Calls inside the
closure remain ordinary typed evidence.

The same stable-target rule applies to immediately invoked Go function
literals and Python lambdas. For an oversized Go call expression,
composite/nested/unary targets are omitted instead of retaining an unstable
source-sized name; an oversized selector retains only its bounded stable field
and remains dynamic-dispatch evidence.

A synthesized qualified or reference name that exceeds its canonical storage
bound is shortened rather than fatal: one ordinary construct — a long method
chain, or a re-export group naming a module's whole public surface — must never
cost the whole index. The file then carries a `canonical_name_truncated`
diagnostic, so a shortened identity is never mistaken for the complete one.

<details>
<summary>Details: shortened-name format and rejected storage fields</summary>

The shortened form keeps a prefix cut on a character boundary, a `~` marker,
and a digest of the exact original name, so it is deterministic across
re-extraction and keeps distinct originals distinct.

A bounded field with no safe shortening still fails canonical reduction, and the
failure now names the exact rejected storage field —
`canonical_field_rejected(<field>)` — without rendering the name, source,
project path, database URL, or driver text. Naming the field is what turns a
whole-corpus bisection into a single run.

</details>

### Numerical and structural-health evidence

The numerical contract is `rust_ast_v1`. These are bounded static heuristics.
The persisted expression identity is a source-version-fenced digest; source
expressions and literal values are not persisted. Runtime observations and
formal proof are separate future adapters and remain explicitly
`not_configured` in current status/tool responses.

<details>
<summary>Details: what <code>rust_ast_v1</code> detects and which guards it recognizes</summary>

It detects arithmetic before a widening cast, epsilon-like absolute-only
tolerance comparisons, low-precision reductions, unguarded domain-sensitive
functions, NaN-sensitive ordering, and narrowing before accumulation. General
`abs(x) <= bound` shapes are retained as non-hazard magnitude-bound evidence
rather than being mislabeled as tolerance. Finite numeric `clamp`/`min`/`max`
guards and direct or same-block immutable clamp/floor inputs to `asin`, `acos`,
log, and square-root calls remain visible with explicit unknowns but use
`none_observed`; unguarded forms remain hazards.

</details>

Structural-health extraction also retains bounded context, not raw literals:

- URL sites are separated into request destinations, endpoint configuration,
  and presentation/validation/data abstentions;
- JavaScript awaited loops retain loop-carried dependency, post-await exit, and
  explicit serial-intent abstentions;
- returned-object facade factories retain their delegate count;
- clone profiles retain domain-separated identifier fingerprints only.

These V8 facts let PostgreSQL findings explain why a site was actionable or
abstained without persisting a URL, identifier, or source expression.

## Resolution

Resolution runs after extraction. It prefers exact module/path and qualified
scope evidence and keeps ambiguous or dynamic cases unresolved.

Unresolved evidence remains typed by provenance. In particular, Rust macro
invocations that would require expansion, dynamic receiver/member access,
language intrinsics, and explicit non-local imports remain targetless rather
than being guessed as project declarations. Static embedded-SQL references
resolve to indexed SQL tables when available and otherwise retain typed
external-schema read/write/DDL provenance. Project-actionable unresolved
pressure is computed separately from those expected language boundaries.

Implemented language-level behavior includes:

- relative TypeScript/JavaScript module specifiers and common extension/index
  probing;
- import bindings, named/default/namespace shapes represented by typed facts;
- lexical/containment-aware local references;
- Rust/Python/Go and the remaining production families' declaration and
  call/member reference shapes;
- Cargo-workspace Rust crate roots, public inline-module paths, and named
  `pub use` facades, with package-scoped cross-crate resolution that preserves
  private-module and ambiguous-package boundaries;
- Rust macro arguments, which the grammar keeps as an unexpanded token tree:
  calls, receiver calls, and paths inside them publish the same references as
  direct code, and nested invocations are macro calls. Inside std formatting
  and assertion macros, whose arguments are known expressions, constant-shaped
  names and inline `{NAME}`/`NAME$` format-string captures are also value
  references. Other bare identifiers (usually locals), constant-shaped tokens
  in other macros (which may be DSL keys or types), attribute bodies,
  `macro_rules!` templates, metavariables, and string text publish nothing, and
  one invocation exceeding its reference bound fails the file's output limit;
- Rust patterns in `matches!`, `assert_matches!`, and `debug_assert_matches!`
  (v2.1.36), which publish what a `match` arm publishes: a qualified
  tuple-struct or variant path such as `Shape::Circle(r)` is a path reference,
  not a call, a single-segment `Some(x)`, binding, or constant pattern
  publishes nothing, and the `if` guard is an ordinary expression;
- Rust turbofish calls (v2.1.38): `f::<T>(..)`, `a::f::<T>(..)`, and
  `x.f::<T>(..)` record the function they call (`f`, `a::f`, or `x.f`) rather
  than keeping the type arguments, in direct code and inside macro arguments,
  where the turbofish type arguments publish nothing;
- PHP compile-time names: statically named class, function, member, and
  `new` references carry an exact lookup (namespace, `use` aliases, class
  context) that binds only to the declaration with that qualified name and
  symbol space, comparing names as PHP does (constants case-sensitive,
  everything else ASCII case-insensitive), and never falls back to a short
  name;
- Pascal scope: calls a file can bind by Pascal's own rules (nested routines,
  implicit `Self`, in-file types, the file's unit-level routines) resolve
  within the file, `uses` units resolve by declared unit name, and
  runtime-library names never bind to a same-named project declaration;
- Swift and Objective-C: a wildcard import of an external module
  (`import Foundation`, `#import <UIKit/UIKit.h>`) never vetoes project
  resolution; an Objective-C keyword send resolves to the one class declaring
  its full selector, and an unqualified Swift call looks among the enclosing
  type's methods first;
- HCL/Terraform addresses (`var.x`, `module.x`, `data.TYPE.NAME`), which
  resolve by exact qualified name;
- JavaScript/TypeScript bare value reads of a nested function, class, or
  component: when another symbol of the file shares the nested declaration's
  qualified name (an embedded script's file component, for example), the read
  resolves only by walking its owner's enclosing scopes outward, never by that
  qualified name, imports, or project names;
- component/template, Salesforce markup, MyBatis, VB6, properties, Liquid, and
  BG3/Osiris domain semantics;
- bounded npm/Composer/Cargo package and workspace manifest facts, including
  dependency-section provenance, workspace membership/exclusions, and
  target-specific Cargo dependencies;
- Fastify object-form routes and NestJS HTTP, GraphQL, message-pattern, and
  WebSocket handler relationships after deterministic framework detection;
- v1 cross-file resolution parity (v2.1.41): TypeScript/JavaScript config
  `extends`, conventional and workspace-package aliases, barrel re-exports and
  member calls; Python absolute/package imports; Java/Kotlin explicit and
  wildcard imports, nested types and static members; C# namespaces and
  `using`; explicitly typed receivers (Python, Go, `this` fields); Rust
  `crate`/`self`/`super` and workspace-crate paths; shell `source` calls;
  Elixir module calls; current-class and recursive calls; Salesforce, Play,
  PHP and CodeIgniter route targets. Each new path abstains on shadowing,
  aliases, overloads or unproven scope and lets the existing resolver run, and
  heuristic fallbacks carry a lower confidence and their own provenance;
- edge kinds required by current graph retrieval, with confidence, provenance,
  and represented site count.

> [!NOTE]
> Language-mode admission is complete. Framework and cross-language resolver
> parity remains a separate release gate; a language being native never implies
> that every framework hook for that language is complete. Expansion follows the
> [native extension guide](../EXTENDING-EXTRACTORS-RESOLVERS.md) and must add the
> complete discovery/parser/resolution/search/import/freshness contract.

## Deterministic canonical generation

The indexer converts extracted files into canonical facts:

- project and generation identity;
- files, symbols, typed edges, exact/coarse references, static numerical sites;
- search documents for file/symbol code/name/natural text;
- deterministic row ordering and logical digest;
- literal row/count/byte admission reports.

Resolution output has one stable logical reduction contract and two physical
paths. The memory path reduces in Rust before bounded COPY. The PostgreSQL path
lazily forms parse work as it enters the bounded scheduler, admitting at most
64 files and 64 MiB of combined source per item. Parallel worker completion
order cannot affect IDs, rows, digest, or BM25 document identity.

<details>
<summary>Details: PostgreSQL reduction path, replay identity, and parser reuse</summary>

A file is never split across items (the 32 MiB per-file ceiling keeps every
file below the item bound), and queued work receives its deadline only when
admitted. The path references immutable cached payloads
when possible, validates file-local output, and COPY-publishes typed unordered
facts behind the exact staging/lease fence. It reduces 64 deterministic UUID
partitions for each of the six canonical relations in four-partition
transaction groups. Each group proves conflicts and cross-relations before
atomically replacing raw evidence with canonical rows. Extracted batch identity
and file/byte windows use logical payload digests rather than inline-versus-cache
storage, so either representation replays idempotently; different bytes for an
existing sequence fail closed.

One spill parse item reuses one `NativeExtractor` per encountered language,
matching the extractor's reusable-parser contract while keeping the item and
payload bounds unchanged. Hot AST cancellation polls read monotonic watch
versions atomically and retain exact parent/stage/deadline behavior.

</details>

## Parallel pipeline

```text
discover
  -> bounded read/hash
  -> bounded tree-sitter parse/extract batches -> cache-backed spill
  -> compact resolution preparation -> parallel per-file resolve -> typed COPY
  -> memory canonical reduce, or PostgreSQL partitioned reduce
  -> exact streamed digest / publish
```

The supervisor and stage runner enforce:

- file-count- and source-byte-aware 1/2/4/8/16 worker selection with
  caller/hardware caps;
- bounded queues, tasks, per-item bytes, retained output, and total operation
  memory model;
- distinct per-file extraction limits: completed retained output is capped at
  32 times source bytes plus a fixed allowance, while transient parser/fact
  construction is reserved at 64 times source bytes plus its fixed allowance;
- input sequence numbers and ordered reduction;
- item/stage/operation/COPY/heartbeat/cancellation deadlines;
- cancellation polling inside discovery, reads, parser callbacks, resolution,
  reduction, and database work;
- abort/reap/poison behavior when a worker panics, hangs, times out, or its
  caller future is dropped;
- exact lease ownership and rollback before publication.

`generationStorage: "auto"` keeps small projects on the memory path and selects
PostgreSQL for a large file count, indexed-source size, or conservative
source-to-generation expansion estimate. PostgreSQL spill removes the complete
extraction/resolver/canonical payload from Rust memory and applies independent
logical byte/row quotas.

<details>
<summary>Details: spill progress, compact native structures, and SCIP overlays</summary>

Parsing, fact publication, canonical reduction, and digesting all make durable
bounded progress rather than retaining a generation payload. Resolve also
advances supervision only after exact pages, committed fact/score batches, and
completed deterministic derived work, so one large outer work item cannot hide
healthy progress. The project-wide resolution lookup, clone profile, and
centrality graph remain explicitly bounded compact native structures because
exact cross-file resolution needs the full declaration domain; an extreme graph
can still be rejected before unsafe allocation or publication.

Persistent SCIP overlays use the same source-verified replacement plan in both
strategies. The spill path loads a bounded basis for covered files, filters
each native file and derived batch, then appends compiler facts in bounded
batches before centrality and canonical reduction. Uncovered facts, unambiguous
native IDs, edge multiplicity, numerical sites, and explicit unresolved targets
follow the same replacement rules as memory. Overlay source bytes, replacement
rules, and imported facts still have native working bounds; spill does not
imply an unbounded compiler artifact. Ordinary spill quotas, batch replay
checks, cancellation, relation validation, and publication fencing also apply
to compiler facts.

</details>

<details>
<summary>Details: parse-cache fingerprint inputs</summary>

The parse-cache fingerprint retains the complete workspace lockfile alongside
extractor/domain source, all workspace manifests, the pinned toolchain,
repository Cargo config, and compiler target/feature inputs. An otherwise
unrelated dependency can change unified features of a shared parser or
serialization dependency. A forward-only lockfile closure cannot prove safe
cache reuse, so narrower dependency and language-specific fingerprints remain
deferred until that independence is established.

</details>

## Search document boundary

Each published file/symbol can produce a stable search document containing:

- normalized path, language, document kind;
- qualified name;
- safe code/identifier text;
- natural documentation text;
- file/symbol/generation/project identity.

ParadeDB indexes qualified name and code with `pdb.source_code`; natural text
uses its text tokenizer. A language slice is not complete until a live
PostgreSQL corpus test proves the expected current-generation BM25 hit.

## Test routing

Test path detection is language-aware and platform-independent. It recognizes
common `test`, `tests`, and `__tests__` directory segments (plus `spec`,
`specs`, `__specs__`, `fixture`, `fixtures`, `test-bed`, `test-beds`, and
`testdata`) and language-owned filename conventions (for example `.test/.spec`, Python `test_`, and Go
`_test`). Affected-test selection still requires graph evidence; a test-looking
path alone is not proof of impact.

## Locked verification

Unit/golden tests cover path admission, UTF-8 chunk boundaries, size/cancellation,
grammar mismatch, malformed syntax, IDs, spans, declarations, references,
resolution ambiguity, search documents, test routing, and digest stability.
The per-language [v1 parity oracle](../LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle)
requires every fact v1.1.33 extracted from each v1 mode's fixture corpus to
have a unique exact per-file identity, committed exact alignment pins, or a
pending/intentional ledger entry. Ambiguous identities never match, and stale
rows fail; the linked report owns the table formats and current counts.

The Rust-owned live corpus then proves discovery through PostgreSQL publication
and ParadeDB search at 1, 2, 4, 8, and 16 workers. Every run must retain:

- identical logical digest;
- literal file/symbol/edge/reference/document rows;
- identical edge-kind and diagnostic sets;
- identical ordered BM25 IDs;
- completed task/publication state;
- released leases and zero staged residue;
- bounded task/RSS measurements.

Committed reports:

- [synthetic COPY/index scaling](benchmarks/INDEX-SCALING.md)
- [native corpus scaling](benchmarks/NATIVE-CORPUS-SCALING.md)
- [patch-task evaluation](benchmarks/PATCH-TASK-EVALUATION.md)
