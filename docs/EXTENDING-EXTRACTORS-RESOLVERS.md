# Extending native extraction and resolution

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Native extraction](v2/EXTRACTION.md) · [Grammar provenance](GRAMMAR-ASSETS.md)

Cartograph v2 extracts code in Rust and publishes typed, generation-scoped facts
to PostgreSQL. This guide covers the registration points and failure modes for
adding a language, resolver, framework fact, or cross-language bridge.

> [!WARNING]
> Do not call the v1 TypeScript runtime, load its WASM grammars, introduce
> SQLite, or shell out to a parser to make an extension appear complete.
> Unsupported source must remain explicit until the complete native contract is
> implemented.

**On this page:** [Choose a mechanism](#choose-the-smallest-correct-mechanism) ·
[Native language checklist](#native-language-checklist) ·
[Dedicated families](#dedicated-extraction-families) ·
[v1 parity oracle](#8-v1-parity-oracle) ·
[Framework facts and bridges](#framework-facts-and-bridges) ·
[Silent-failure traps](#silent-failure-traps) ·
[Required gates](#required-gates)

## Choose the smallest correct mechanism

| Graph gap | Mechanism |
| --- | --- |
| Syntax/declarations/references from a new grammar | Native language slice |
| An existing language that the generic walker covers too thinly | Dedicated extraction family (see [Dedicated extraction families](#dedicated-extraction-families)) |
| Script regions embedded in a host file (component frontmatter, `<script>` blocks, template expressions) | Embedded-script regions walked by an existing family (`walk/embedded_script.rs`) |
| Another extension using identical syntax/semantics | Existing language slice plus extension mapping |
| Import/package/receiver resolution inside one language | Deterministic language resolver |
| Literal route, command, resource, or component declaration | Framework fact extraction after explicit detection |
| Reference in one language to a declaration in another | Cross-language bridge over typed facts |
| Convention relating two already extracted nodes | Deterministic generation build step |
| Static numerical operation, precision, or hazard evidence | Numerical site extractor with an explicit analyzer contract |

A framework resolver should not become a second parser. A cross-language bridge
should not fabricate declarations. A derived edge should not be added after
publication, because that would make the visible generation differ from its
digest.

## Native language checklist

Work through these steps in order. Each links to the detailed rules below.

1. **Identity.** Add a row to the `source_languages!` table in
   `crates/cartograph-domain/src/source.rs`. A game-scripting mode is also
   added to `SourceLanguage::is_game_scripting` in the same file.
   See [§1](#1-stable-language-identity).
2. **Detection.** Resolve shared or ambiguous extensions in the `detect_*`
   helpers of `crates/cartograph-domain/src/source.rs`, and route
   language-specific test filenames in
   `crates/cartograph-extract/src/snapshot.rs`.
   See [§2](#2-discovery-and-snapshot-admission).
3. **Registry tests.** In the `source.rs` tests, update the count assertions
   (132 modes / 73 v1 modes / 52 game modes), the frozen
   `v2_language_additions_digest` value, and `representative_path` /
   `representative_source` when the default sample path does not route to the
   new mode. See [§1](#1-stable-language-identity).
4. **Grammar.** Pin the crate in the workspace `Cargo.toml`, add it to
   `crates/cartograph-extract/Cargo.toml`, and register it in
   `crates/cartograph-extract/src/grammars.rs`. Custom-scanner modes skip this
   step. See [§3](#3-grammar-registration).
5. **Extraction strategy.** Register the language in
   `crates/cartograph-extract/src/language.rs` (`grammar_strategy` or
   `fallback_strategy`). See [§3](#3-grammar-registration).
6. **Walker or scanner.** Extend `crates/cartograph-extract/src/walk.rs` and
   `walk/`, or add a bounded scanner in `crates/cartograph-extract/src/custom.rs`
   / `custom/`. A language that needs its own walker gets a
   [dedicated extraction family](#dedicated-extraction-families). See
   [§4](#4-declarations-and-references).
7. **Resolution.** Add deterministic module/name rules in
   `crates/cartograph-indexer/src/native_pipeline.rs`.
   See [§5](#5-resolution).
8. **Search documents.** Prove a live BM25 hit. See [§6](#6-search-documents).
9. **Import and freshness.** Update importer preflight in
   `crates/cartograph-db/src/v1_import.rs` and the live cutover tests.
   See [§7](#7-import-and-freshness-boundary).
10. **v1 parity.** For a v1.1.33 mode, keep the v1 parity oracle green: fix
    dropped facts, pin verified shape differences, or ledger missing facts as
    `pending` (gap id and wave) or `intentional` (reason id). See
    [§8](#8-v1-parity-oracle).
11. **Generation contract.** When the change alters the facts an existing
    language publishes, add a new generation-digest contract and its
    migration. See [§9](#9-generation-digest-contract).
12. **Documentation.** Update [`SUPPORT-MATRIX.md`](SUPPORT-MATRIX.md), the
    [coverage report](LANGUAGE-COVERAGE-REPORT.md), and `ACKNOWLEDGEMENTS.md`
    when a grammar is added.
13. **Gates.** Run the [required gates](#required-gates).

### 1. Stable language identity

Add the language to `cartograph-domain::SourceLanguage` with a stable serialized
and database value: a row in the `source_languages!` table in
`crates/cartograph-domain/src/source.rs` gives the variant, its `stable` id, its
`v1_extensions`, its additive `additions`, and its `native` flag. Update the
domain round-trip tests. These values are part of generation facts and
migration compatibility; renaming one is a schema change.

The registry is guarded by tests in the same file:

| Test | What it pins | What to change |
| --- | --- | --- |
| `language_registry_preserves_v1_contract_and_tracks_v2_additions_separately` | Sorted, distinct stable ids (132), v1 modes (73), game-scripting modes (52), and the 163 v1 extensions | Bump the total, plus the game count for a game mode. The v1 counts are frozen. |
| `language_registry_matches_independently_frozen_manifests` | BLAKE3 digests of the v1 manifest (`v1_language_registry_digest`) and of the additive extensions (`v2_language_additions_digest`) | A new extension in `additions` changes the frozen v2 digest; update it deliberately. The v1 digest must not change. |
| The first test's stable-id and detection round trip | `from_stable_str` and `SourceLanguage::detect(representative_path(..), Some(representative_source(..)))` for every language | Add a `representative_path` / `representative_source` entry when `src/sample<first extension>` with empty source does not route to the new mode (path- or content-gated modes). |

A game-scripting mode must also be listed in `SourceLanguage::is_game_scripting`.
That predicate routes it to the bounded custom strategy; any other language that
is in neither `grammar_strategy` nor `fallback_strategy` panics with
`game scripting registry drifted` when its `LanguageSpec` is built.

### 2. Discovery and snapshot admission

Extension ownership comes from the `source_languages!` table in
`crates/cartograph-domain/src/source.rs`. Shared or ambiguous extensions are
resolved by its `detect_*` helpers (for example `detect_source_language`,
`detect_content_gated_language`, `detect_colliding_game_extension`, and
`detect_header_language`), which gate a mode by path or content without
changing the frozen v1 classifier. `crates/cartograph-extract/src/snapshot.rs`
only routes language-specific test filenames.

- map canonical extensions to the language (`source.rs`);
- route language-specific test filenames (`snapshot.rs`);
- cover case normalization and compound extensions;
- prove unsupported extensions still fail explicitly.

`SourceRoot` performs project-root validation and bounded chunked reads.
`SourceSnapshot` owns UTF-8, language, size, path, file identity, and exact
content digest. New code must consume that boundary rather than reading an
arbitrary path directly.

### 3. Grammar registration

Pin the tree-sitter crate in the workspace `Cargo.toml` and add it to
`crates/cartograph-extract/Cargo.toml`. Then select its grammar in
`crates/cartograph-extract/src/grammars.rs`:

- add a `NativeGrammar` variant;
- add its stable id to `NATIVE_GRAMMAR_IDS`, its constructor to
  `NATIVE_GRAMMAR_FACTORIES`, and the variant to `NativeGrammar::ALL`. The three
  arrays share one length and stay in sorted stable-id order;
- map the language to the grammar in the matching `grammar_group_*` function;
- keep the tests passing: `every_admitted_native_grammar_is_unique_sorted_and_abi_compatible`
  loads every grammar, and `grammar_mapping_covers_exact_v1_grammar_and_custom_counts`
  pins the v1 grammar/custom split (61/12) and requires that a mode has a
  grammar exactly when its strategy is not `CustomStructural`.

Register the extraction strategy in `crates/cartograph-extract/src/language.rs`:
a dedicated family in `grammar_strategy`, or `GenericStructural` /
`CustomStructural` in `fallback_strategy`. `NativeExtractor`
(`crates/cartograph-extract/src/native.rs`) loads the grammar and strategy from
that registration.

Create a fresh parser per bounded operation or use the established worker-local
pattern; never share mutable tree-sitter state across concurrent workers.

Add tests for:

- grammar/language mismatch;
- malformed but recoverable syntax;
- cancellation;
- maximum input and output bounds;
- zero-symbol files that are legitimately empty versus unsupported files.

### 4. Declarations and references

Extend the walker dispatch in `walk.rs` and focused modules under `walk/`. A
custom (non-grammar) mode instead adds a branch to `extract_existing_custom` in
`custom.rs`; game-scripting modes use `custom/game_scripting.rs` (Rhai has
`custom/rhai.rs`). Produce typed facts with:

- deterministic symbol/reference IDs;
- one-based lines and exact byte ranges;
- explicit symbol/reference kinds and visibility;
- safe literal-free callable signatures;
- explicit confidence, provenance, and site multiplicity;
- recoverable diagnostics instead of panics.

Do not resolve during syntax walking. Retain enough unambiguous typed evidence
for the resolver; leave dynamic or ambiguous references unresolved.

<details>
<summary>Details: numerical evidence requirements</summary>

Numerical evidence additionally requires a stable site ID, exact owner/file
span, bounded machine-token categories, a privacy-safe expression digest,
confidence independent from evidence level, explicit unknowns, and an analyzer
contract included in freshness. Never persist the source expression or literal,
and never label static syntax as observed or formally proven behavior.

</details>

A grammar-backed mode can also hand specific files to a bounded scanner:
`custom::scans_snapshot` in `custom.rs` routes Delphi `.dfm` / `.fmx` form
files, which belong to the Pascal mode, to `custom/pascal_form.rs`.

### Dedicated extraction families

When the generic structural walker covers a language too thinly, give it a
dedicated family rather than adding language branches to `generic_family.rs`.
`AdaFamily` (Ada and VHDL) is the precedent, and the v1-parity families (PHP,
Pascal, Objective-C, Swift, Dart, F#, ArkTS, Ruby, Lua, R, Nix, Lisp, Lean,
ReScript, Solidity, VB.NET, Apex, HCL, Astro) follow it:

1. **Strategy.** Add an `ExtractionStrategy` variant in
   `crates/cartograph-extract/src/language.rs`, list it in `is_executable`, and
   register the language for it in the `GRAMMAR_STRATEGIES` table (removing it
   from `fallback_strategy`).
2. **Walker family.** In `crates/cartograph-extract/src/walk.rs`, give the
   family a language list such as `ADA_LANGUAGES` and remove the language from
   `GENERIC_LANGUAGES`. Language lists are disjoint: a language selects at most
   one family.
3. **Family slice.** Add one entry to the `FAMILY_SLICES` table, which pairs the
   language list with the family's entry points so a family cannot be wired
   into one traversal and forgotten in the other. A module that exposes
   `visit_declaration(builder, node, depth) -> Result<bool, ExtractError>` and
   `capture_usage(builder, node) -> Result<(), ExtractError>` uses
   `FamilySlice::capturing`, whose usage pass records the module's own usage
   facts before visiting the node's children. A family whose declaration pass
   records every fact (Pascal, HCL, Lisp, Lean, Astro) uses
   `FamilySlice::declarative`; one that drives its own usage traversal uses
   `FamilySlice::custom`. Name both functions as paths in the entry: that is
   also how the code graph sees they are used.
4. **Module.** Implement the family in `walk/<language>_family.rs` (or a
   directory module for larger families). Reuse the shared helpers:
   `walk/family_support.rs` for declaration emission (Lisp, Lean, ReScript,
   Solidity) and `walk/script_support.rs` for load-style imports (Ruby, Lua,
   R, Nix). Per-walk family state belongs in one builder slot (the scripting
   families share `ScriptState`), not in globals.
5. **Tests.** Add a focused suite under `crates/cartograph-extract/tests/`
   (for example `ada_family.rs`), keep the [v1 parity oracle](#8-v1-parity-oracle)
   green for a v1 mode, and update any frozen indexer digest the new facts
   move.

A family that only adds constructs to an existing walker can delegate to it:
Apex runs the managed (Java) family, Objective-C delegates its C subset to the
C family, ArkTS runs the JavaScript-family walker, and Solidity extends the
generic walker.

### 5. Resolution

Resolution must be deterministic and module-aware. Prefer exact module/path and
qualified-name evidence before any name-only fallback. If more than one target
remains plausible, retain unresolved evidence instead of choosing by worker or
database order.

Every new resolution rule needs fixtures for:

- the exact success path;
- same-name private declarations in different modules;
- ambiguous candidates;
- missing modules;
- import aliases/re-exports relevant to the language;
- identical output under reversed input order and every supported worker count.

### 6. Search documents

Ensure canonical generation building emits safe search documents for new files
and symbols. Code/name fields are tokenized by ParadeDB's `pdb.source_code`;
natural documentation uses the text field. A new language is not end-to-end
complete until a live PostgreSQL publication returns its expected BM25 hit.

### 7. Import and freshness boundary

The v1 PostgreSQL importer may admit only languages the v2 runtime can index and
include in its complete source revision. Update importer preflight and live
cutover tests when expanding support. Otherwise an imported generation could
appear fresh while unsupported files change or disappear on the next index.

### 8. v1 parity oracle

Every v1.1.33 mode has a fixture corpus and the facts the real v1.1.33 binary
extracted from it, and `crates/cartograph-extract/tests/v1_parity_oracle.rs`
(part of `cargo test`) requires one disposition for every frozen fact:

- **Identity:** a unique exact per-file key, including qualified names, kinds
  and recorded lines. An ambiguous identity never matches.
- **Alignment:** unique readable native pins in
  `tests/fixtures/v1_parity/alignments.jsonl` for an established shape difference
  or documented correction. Every pin in a group is required.
- **Ledger:** `divergences.jsonl` records a missing fact as `pending` (gap id and
  wave) or `intentional` (reason id). `reasons.jsonl` stores shared evidence.

Read the failure's candidate suggestions, then inspect the source, capture and
native extraction before fixing extraction or updating rows. Suggestions never
satisfy the gate. Delete stale or redundant rows and unused reason definitions.
`gate_cases.jsonl` and `counterexamples.jsonl` replay regressions through the
same gate; reference pins always retain the owner's kind and declaration site.

```sh
cargo test --locked -p cartograph-extract --test v1_parity_oracle
```

Keep the corpus and captured `expected/<mode>.json` unchanged when resolving a
failure. Creating a capture requires the maintainer-only
`scripts/capture-v1-parity-oracle.sh` and verified v1.1.33 release binary.
The key contract, correction policy and current disposition counts are in the
[coverage report](LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle).

### 9. Generation-digest contract

Generation freshness includes the native generation-digest contract, so a
change to the facts an existing language publishes needs a new contract;
otherwise an unchanged checkout keeps reporting fresh with the old facts. The
V21 contract (migration 48) is the latest example:

- add the `GenerationDigestVersion` variant and move `CURRENT` in
  `crates/cartograph-domain/src/lib.rs`;
- add its digest domain in `crates/cartograph-db/src/ingest/digest.rs`;
- add a migration in `crates/cartograph-db/src/migrations.rs` that widens
  `index_generations_digest_version_check`, with its pinned checksum;
- update the frozen digests in `crates/cartograph-indexer/src/native_pipeline.rs`
  and the live fixtures that pin the schema version;
- record the contract in the
  [native extraction contract history](v2/EXTRACTION.md#complete-source-revision)
  and the [migration ledger](v2/ARCHITECTURE.md#migration-ledger).

## Framework facts and bridges

Framework extraction runs only after an explicit, deterministic detection
signal. It must have hard file/node/byte limits and emit ordinary typed facts so
the same canonical reducer, digest, COPY, validation, and publication rules
apply.

A cross-language bridge consumes existing facts from both languages. It must:

- state the exact convention it recognizes;
- scope candidates by module/package/framework identity;
- preserve ambiguity;
- cap fan-out;
- attach confidence and provenance;
- prove that input order and worker count cannot change the result.

> [!IMPORTANT]
> Do not add a mutable post-publication hook. Derived relationships belong in
> the staged generation before validation and atomic publication.

## Silent-failure traps

| Mistake | What happens |
| --- | --- |
| Extension mapped but grammar selection missing | Discovery succeeds and parse fails for every file. |
| Grammar selected but walker dispatch missing | Files look successfully empty. |
| Dedicated strategy registered, but the language left in `GENERIC_LANGUAGES` in `walk.rs` | The generic walker keeps running and the new family never executes. |
| Extraction output changed without a new generation-digest contract | Unchanged checkouts keep reporting fresh with the previous facts. |
| v1 fact dropped, an ambiguous pin selected, or a redundant policy row retained | `v1_parity_oracle` fails with an unmatched, ambiguous or stale fact. |
| Non-game language registered in neither `grammar_strategy` nor `fallback_strategy` | Building its `LanguageSpec` panics with `game scripting registry drifted`. |
| Declarations added without search documents | Exact graph exists but BM25 cannot find it. |
| Resolver uses global name-only matching | Private same-name symbols acquire false edges. |
| Unsupported language admitted by v1 import | Status can misreport freshness and reindex drops data. |
| Per-worker map iteration reaches the digest | Output changes with scheduling. |
| Literal-bearing signatures enter search | Secrets or source literals can leak into evidence. |
| New edge kind omitted from bulk relation validation | Invalid staged rows may reach publication or valid rows may fail late. |

## Required gates

At minimum:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked -p cartograph-domain -p cartograph-extract -p cartograph-indexer
```

The `cartograph-extract` tests include the
[v1 parity oracle](#8-v1-parity-oracle).

Then run the live native-corpus supervisor and 1/2/4/8/16-worker benchmark from
`.github/workflows/v2-rust.yml`. The locked corpus must retain identical logical
digest, row counts, edge kinds, diagnostics, ordered BM25 IDs, terminal task
state, and cleanup. Finish with the full workspace, PostgreSQL/ParadeDB, Sonar,
structural, archive, and independent-review gates.

See [native extraction](v2/EXTRACTION.md) and
[v2 architecture](v2/ARCHITECTURE.md) for the current implemented boundary.
