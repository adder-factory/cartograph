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
| 3 | declarations, references, exact/coarse spans, and diagnostics | `crates/cartograph-extract/src/walk.rs` and `walk/` |
| 4 | deterministic module/name resolution with explicit ambiguity | `crates/cartograph-indexer/src/native_pipeline.rs` |
| 5 | safe search documents and framework/cross-language edges where applicable | canonical generation building; `crates/cartograph-extract/src/framework*.rs` |
| 6 | cancellation and input/output/nesting limits | every extraction path |
| 7 | v1-import and freshness admission updates | `crates/cartograph-db/src/v1_import.rs` |
| 8 | deterministic 1/2/4/8/16-worker publication and live ParadeDB BM25 proof | `.github/workflows/v2-rust.yml` live suites |
| 9 | support-matrix and acknowledgement updates | [`SUPPORT-MATRIX.md`](SUPPORT-MATRIX.md), `ACKNOWLEDGEMENTS.md` |

The [native language checklist](EXTENDING-EXTRACTORS-RESOLVERS.md#native-language-checklist)
lists every registration point, count assertion, and test that each step
touches.

> [!IMPORTANT]
> Unknown source remains unsupported until the complete contract passes. An
> empty file node is valid only for the deliberately documented structural-file
> modes; it is not a substitute for an extractor.

The current architecture and language inventory are documented in
[native extraction](v2/EXTRACTION.md) and the [support matrix](SUPPORT-MATRIX.md).
