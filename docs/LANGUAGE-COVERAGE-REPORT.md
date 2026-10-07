# Native language-coverage report

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Grammar provenance](GRAMMAR-ASSETS.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last release audit: 2026-10-06 (`v2.1.41`).

This report states what "supported" means for a language mode and where the
evidence for each admitted mode lives. For the per-language inventory, read the
[support matrix](SUPPORT-MATRIX.md).

**On this page:** [Current inventory](#current-inventory) ·
[Strategy families](#strategy-families) ·
[Admission contract](#admission-contract) ·
[v1 parity oracle](#v1-parity-oracle) ·
[Validation evidence](#validation-evidence)

## Current inventory

Cartograph v2.1.41 production-admits all 73 v1.1.33 language modes and all 163 v1
extensions, plus additive Python `.pyi`, native TOML, and 52 dedicated textual
game-scripting modes, the WGSL and Metal shader modes added in v2.1.12, and
Slang and WESL added in v2.1.15, plus Ada/SPARK and VHDL added in v2.1.27: 132
modes total.

| Parsing path | Modes | What it covers |
| --- | ---: | --- |
| Pinned native tree-sitter grammar | 67 | 61 native grammar bindings; Metal (C++), WESL (WGSL), Zsh (Bash), JSX (JavaScript), Jupyter (JSON), and BG3 KHN (Lua) share another mode's grammar |
| Bounded Rust structural scanner | 65 | The 12 custom v1 mixed-markup, configuration, and domain-specific modes, TOML, and the 52 game-scripting modes |

Where the inventory is defined:

- **Modes:** `cartograph_domain::SourceLanguage::ALL`.
- **Extension mapping and detection:** the `source_languages!` table and the
  `detect_*` helpers in `crates/cartograph-domain/src/source.rs`.
- **Grammar selection:** `crates/cartograph-extract/src/grammars.rs`; extractor
  strategy: `crates/cartograph-extract/src/language.rs`.
- **Human-readable inventory:** the [support matrix](SUPPORT-MATRIX.md).
- **Game-language research boundary and source trail:**
  [game scripting language coverage](v2/GAME-SCRIPTING-LANGUAGES.md).

## Strategy families

Every mode has exactly one `ExtractionStrategy`, chosen in
`crates/cartograph-extract/src/language.rs`. The walker dispatch for each
grammar-backed family lives in `crates/cartograph-extract/src/walk.rs` and
`walk/<family>_family.rs`.

| Strategy | Modes |
| --- | --- |
| `JavaScriptFamily` | TypeScript, TSX, JavaScript, JSX |
| `PolyglotStructural` | Rust, Python, Go |
| `CFamily` | C, C++, CUDA, GLSL, HLSL, Metal, Slang |
| `ShaderFamily` | WGSL, WESL |
| `AdaFamily` | Ada/SPARK, VHDL |
| `ShellFamily` | Bash, Fish, PowerShell, Zsh |
| `ManagedFamily` | Java, C# |
| `JvmDynamicFamily` | Kotlin, Scala, Groovy |
| `ApexFamily` | Apex (the managed walker plus triggers and SOQL/SOSL objects) |
| `VbNetFamily` | VB.NET |
| `PascalFamily` | Pascal/Delphi; `.dfm` / `.fmx` form files go to a bounded form scanner |
| `ObjcFamily` | Objective-C (with the C family for its C subset) |
| `SwiftFamily` | Swift |
| `PhpFamily` | PHP |
| `AstroFamily` | Astro (with the JavaScript/TypeScript walker for embedded scripts) |
| `DartFamily` | Dart |
| `FSharpFamily` | F# |
| `ArkTsFamily` | ArkTS (the TypeScript walker plus ArkUI additions) |
| `RubyFamily` | Ruby |
| `LuaFamily` | Lua, Luau, BG3 KHN |
| `RFamily` | R |
| `NixFamily` | Nix |
| `LispFamily` | Clojure, Common Lisp |
| `LeanFamily` | Lean |
| `ReScriptFamily` | ReScript |
| `SolidityFamily` | Solidity (on top of the generic walker) |
| `HclFamily` | HCL / Terraform / OpenTofu |
| `TagsQuery` | Elixir, Haskell, Julia, OCaml, OCaml Interface, Verilog |
| `GenericStructural` | ABAP, GraphQL, HTML, Prisma, SQL, YAML (GraphQL, Prisma, and SQL have schema-aware slices) |
| `ParserOnly` | CSS, ERB/EJS, JSDoc, JSON, Jupyter, Regex |
| `CustomStructural` | Aura, BG3 Anubis/Resource/Stats, Liquid, Osiris, Java Properties, Svelte, TOML, VB6, Visualforce, Vue, MyBatis XML, and the 52 game-scripting modes |

Before the v1 parity port, 28 v1 modes ran on `GenericStructural`, which kept
far fewer facts than v1 for most of them. 22 of those modes now have dedicated
families:
PHP, Pascal/Delphi, Objective-C, Swift, Dart, F#, ArkTS, Ruby, Lua, Luau, KHN,
R, Nix, Clojure, Common Lisp, Lean, ReScript, Solidity, VB.NET, Apex,
HCL/Terraform, and Astro. Vue and Svelte stay custom-scanner modes, but their
`<script>` blocks and template expressions now run through the native
JavaScript/TypeScript walker instead of a line scanner.

## Admission contract

Coverage is a capability contract, not only a parser smoke. Every admitted mode
must prove:

- deterministic file/declaration/reference facts or a deliberately documented
  structural-file floor;
- malformed-input behavior, cancellation, nesting/output limits, and literal
  safety;
- extension discovery and explicit rejection of unsupported source;
- module/resolver behavior relevant to the language and framework bridges;
- identical logical output under 1/2/4/8/16 workers;
- live PostgreSQL COPY/publication and expected ParadeDB BM25 retrieval;
- v1 PostgreSQL import/freshness admission;
- for the 73 v1 modes, every per-file fact v1.1.33 extracted from the mode's
  [parity corpus](#v1-parity-oracle), or a ledgered divergence.

> [!IMPORTANT]
> Framework and cross-language resolver parity is audited separately from
> parser admission. A grammar loading successfully does not prove route, call,
> import, receiver, or bridge semantics.

## v1 parity oracle

Admission proves robustness, not depth: a mode can parse, publish, and stay
deterministic while extracting far less than v1 did. The per-language v1
parity oracle closes that gap for every v1.1.33 mode by comparing v2's
per-file extraction with facts captured once from the real v1.1.33 release
binary over the same frozen source. The gate uses exact identities and explicit
pins; it has no runtime matching heuristics.

| Part | Location |
| --- | --- |
| Test target | `crates/cartograph-extract/tests/v1_parity_oracle.rs` (runs in `cargo test --locked --workspace`) |
| Fixture corpus, one per v1 mode | `crates/cartograph-extract/tests/fixtures/v1_parity/<mode>/` |
| Captured v1.1.33 facts | `crates/cartograph-extract/tests/fixtures/v1_parity/expected/<mode>.json` |
| Exact alignments | `crates/cartograph-extract/tests/fixtures/v1_parity/alignments.jsonl` |
| Missing-fact ledger | `crates/cartograph-extract/tests/fixtures/v1_parity/divergences.jsonl` |
| Shared gap and correction evidence | `crates/cartograph-extract/tests/fixtures/v1_parity/reasons.jsonl` |
| Small gate regressions | `crates/cartograph-extract/tests/fixtures/v1_parity/gate_cases.jsonl` |
| Adversarial review regressions | `crates/cartograph-extract/tests/fixtures/v1_parity/counterexamples.jsonl` |
| Capture script (maintainer-only) | `scripts/capture-v1-parity-oracle.sh` |

### What the oracle checks

The test runs v2's `NativeExtractor` over every corpus file, using the
corpus-relative path as the virtual path. Each frozen file, symbol, edge or
unresolved reference needs exactly one disposition:

- **Identity:** a unique exact readable key after the fixed kind and separator
  normalization. Symbols retain kind, full qualified name and declaration start;
  containments retain both typed endpoints and any recorded child line;
  references retain kind, typed owner, owner declaration start, exact target name
  and any recorded occurrence line. Verified symbol correspondences supply exact
  relationship endpoints. The index includes all native observations in the
  file; ambiguous keys never satisfy identity.
- **Alignment:** committed canonical native pins for a genuine shape difference,
  split representation or documented correction. A pin selects one observation
  by readable key, with an end line only when needed to disambiguate it. Every
  pin in a group must exist and select a distinct observation. No byte offsets,
  columns, leaf-name fallback or candidate ranking participates in matching.
- **Ledger:** an explicit missing fact, pending with a wave and gap id or
  intentional with a reason id. Shared evidence lives once in `reasons.jsonl`.

The captured 8,231 records across 73 modes contain duplicates that share a
disposition, giving 8,165 distinct selectors. The current disposition counts
after the extraction fixes are:

| Disposition | Distinct selectors |
| --- | ---: |
| Identity | 6,038 |
| Aligned | 1,957 |
| Pending, wave 2 | 16 |
| Pending, wave 3 | 109 |
| Intentional | 45 |

This gate proves per-file observations. Correction alignments marked
`v1-target-misresolution-*` preserve only the corrected source occurrence under
its exact typed owner and declaration/occurrence sites; they reject the captured
resolved target. They never claim resolved-target parity. Correct cross-file
target resolution requires separate resolver evidence.

Three tests make up the target:

| Test | Fails when |
| --- | --- |
| `exact_parity_carries_every_captured_fact_or_an_explicit_divergence` | A fact has no disposition, an identity or pin is ambiguous, policy hygiene fails, or capture provenance/corpus membership differs |
| `compact_tables_are_sorted` | Alignments, divergences or reason definitions are duplicated or out of deterministic order |
| `small_inputs_and_review_counterexamples_exercise_the_real_gate` | A small gate case or adversarial extraction mutation stops producing its required diagnostics |

### Resolving a failure

Run the target with diagnostics visible:

```sh
cargo test --locked -p cartograph-extract --test v1_parity_oracle -- --nocapture
```

Read the frozen selector and up to three nearby native candidates. Suggestions
never count as matches. Inspect the frozen source, capture and native extraction
to establish the entity and its ownership, then fix extraction or commit unique
pins for the established correspondence. Pin the complete group when the capture
represents multiple independently established relationships. Record a pending
gap or intentional reason only when the fact is not carried.

Each JSONL object occupies one line. An alignment names `language`, `file`, the
`v1` key, optional captured `end`, native `pins` and optional correction `reason`.
A divergence names `language`, `file`, `fact` and `disposition` (`pending` with
`wave` and `id`, or `intentional` with `id`). A reason definition names `id`,
source-specific `reason` evidence and a `wave` only for pending gaps. Small gate
cases and counterexamples name inputs, policy and required diagnostic fragments;
counterexamples may mutate actual extraction output. The test's module docs give
an example row for each table.

Remove rows whose facts now match by identity and reason definitions no longer
used. Unknown fields, nonexistent frozen selectors, conflicting dispositions,
empty/duplicate/overlapping pins, undefined or unused ids and inconsistent waves
fail hygiene. Preserve existing pending gap ids and waves when updating rows;
remaining work is scheduled as wave 2 (resolution) and wave 3 (framework/bridge
detail).

### Cross-file resolution oracle

The per-file gate cannot check which declaration a reference resolves to. The
companion resolution oracle
(`crates/cartograph-indexer/src/native_pipeline/tests/v1_resolution_oracle.rs`)
runs the native pipeline over the same 73 frozen corpora and requires each of
the 1,730 captured v1 cross-file edges (1,724 distinct facts) to reach the same
target file and declaration, a pinned alignment, or a ledger entry. Its
[README](../crates/cartograph-indexer/tests/fixtures/v1_resolution/README.md)
owns the tables, update procedure and pending inventory.

| Disposition (v2.1.41) | Distinct facts |
| --- | ---: |
| Matched exactly | 601 |
| Aligned by exact pins | 295 |
| Intentional | 406 |
| Pending, wave 3 | 422 |

Run it with
`cargo test --locked -p cartograph-indexer --lib v1_resolution_oracle -- --nocapture`.

### Frozen capture provenance

Keep `expected/<mode>.json` and the corpus source unchanged when resolving a
failure. The maintainer-only `scripts/capture-v1-parity-oracle.sh` creates
captures with the v1.1.33 darwin-arm64 binary verified against that release's
`SHA256SUMS`, using an isolated `HOME` and private temporary checkout. The gate
pins that binary's recorded SHA-256. V2, its tests and CI only read captured JSON;
they never run the historical binary.

## Validation evidence

Rust test surfaces include the focused extractor family suites under
`crates/cartograph-extract/tests/`, the frozen v1.1.33 oracle for TypeScript,
JavaScript, TSX, JSX, C, C++, Java, and C# (`tests/v1_oracle.rs`), the
per-language [v1 parity oracle](#v1-parity-oracle), the native corpus
supervisor under `crates/cartograph-indexer/tests/`, and live PostgreSQL/
ParadeDB language publication tests. These live suites run in
`.github/workflows/v2-rust.yml` against the pinned database image rather than
trusting a generated count alone, and `release.yml` refuses to publish a tag
without a green `v2-rust.yml` run for that exact commit SHA.
