# Adding a language

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Coverage report](LANGUAGE-COVERAGE-REPORT.md) ·
[Full extension guide](EXTENDING-EXTRACTORS-RESOLVERS.md)

Cartograph v2 language support is native Rust. Do not add a TypeScript
extractor, WebAssembly grammar asset, external parser process, or parser-only
placeholder and call the language complete.

Use the maintained [extractor and resolver extension
guide](EXTENDING-EXTRACTORS-RESOLVERS.md). A production language slice must add
all of the following:

| # | Requirement | Main location |
| ---: | --- | --- |
| 1 | stable `SourceLanguage` identity and extension admission | `source_languages!` table in `crates/cartograph-domain/src/source.rs` |
| 2 | a pinned native tree-sitter grammar or bounded Rust structural scanner | `crates/cartograph-extract/src/grammars.rs` and `language.rs`, or `custom.rs` |
| 3 | declarations, references, exact/coarse spans, and diagnostics, in a dedicated extraction family when the generic walker is too thin | `crates/cartograph-extract/src/walk.rs` and `walk/<language>_family.rs` |
| 4 | deterministic module/name resolution with explicit ambiguity | `crates/cartograph-indexer/src/native_pipeline.rs` |
| 5 | safe search documents and framework/cross-language edges where applicable | canonical generation building; `crates/cartograph-extract/src/framework*.rs` |
| 6 | cancellation and input/output/nesting limits | every extraction path |
| 7 | v1-import and freshness admission updates | `crates/cartograph-db/src/v1_import.rs` |
| 8 | deterministic 1/2/4/8/16-worker publication and live ParadeDB BM25 proof | `.github/workflows/v2-rust.yml` live suites |
| 9 | support-matrix and acknowledgement updates | [`SUPPORT-MATRIX.md`](SUPPORT-MATRIX.md), `ACKNOWLEDGEMENTS.md` |
| 10 | a green v1 parity oracle when the change touches a v1.1.33 mode | `crates/cartograph-extract/tests/v1_parity_oracle.rs` |

The [native language checklist](EXTENDING-EXTRACTORS-RESOLVERS.md#native-language-checklist)
lists every registration point, count assertion, and test that each step
touches.

## Dedicated families

A language that needs more than the conservative generic walker gets its own
extraction family. Follow the `AdaFamily` precedent: an `ExtractionStrategy`
variant in `language.rs` registered in `GRAMMAR_STRATEGIES`, a language list and
`FAMILY_SLICES` entry in `walk.rs` (`FamilySlice::capturing` wires a module that
exposes `visit_declaration` and `capture_usage`), and the walker in
`walk/<language>_family.rs`. The
[dedicated extraction families](EXTENDING-EXTRACTORS-RESOLVERS.md#dedicated-extraction-families)
section lists every step and the shared helpers.

## Keep the v1 parity oracle green

Changing an existing v1.1.33 mode's extraction must keep
`crates/cartograph-extract/tests/v1_parity_oracle.rs` passing:

- require a unique exact identity for every frozen v1 fact, or pin its verified
  counterpart in `tests/fixtures/v1_parity/alignments.jsonl`;
- record missing facts in `divergences.jsonl` as `pending` (gap id and wave) or
  `intentional` (reason id), with shared evidence in `reasons.jsonl`;
- read candidate suggestions to investigate failures; they never count as
  matches. Remove stale or redundant rows after a fix and retain the
  `gate_cases.jsonl` and `counterexamples.jsonl` regressions;
- keep the corpus and `expected/<mode>.json` frozen. Creating a capture requires
  the maintainer-only `scripts/capture-v1-parity-oracle.sh` and verified
  v1.1.33 release binary.

The key contract, current disposition counts and update procedure are in the
[coverage report](LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle). A change that
alters published facts also needs a new
[generation-digest contract](EXTENDING-EXTRACTORS-RESOLVERS.md#9-generation-digest-contract).

> [!IMPORTANT]
> Unknown source remains unsupported until the complete contract passes. An
> empty file node is valid only for the deliberately documented structural-file
> modes; it is not a substitute for an extractor.

The current architecture and language inventory are documented in
[native extraction](v2/EXTRACTION.md) and the [support matrix](SUPPORT-MATRIX.md).
