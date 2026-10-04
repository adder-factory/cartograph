# Native language-coverage report

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Grammar provenance](GRAMMAR-ASSETS.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last release audit: 2026-10-04 (`v2.1.39`).

This report states what "supported" means for a language mode and where the
evidence for each admitted mode lives. For the per-language inventory, read the
[support matrix](SUPPORT-MATRIX.md).

## Current inventory

Cartograph v2.1.39 production-admits all 73 v1.1.33 language modes and all 163 v1
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
- v1 PostgreSQL import/freshness admission.

> [!IMPORTANT]
> Framework and cross-language resolver parity is audited separately from
> parser admission. A grammar loading successfully does not prove route, call,
> import, receiver, or bridge semantics.

## Validation evidence

Rust test surfaces include the focused extractor family suites under
`crates/cartograph-extract/tests/`, the frozen v1.1.33 oracle, the native corpus
supervisor under `crates/cartograph-indexer/tests/`, and live PostgreSQL/
ParadeDB language publication tests. These live suites run in
`.github/workflows/v2-rust.yml` against the pinned database image rather than
trusting a generated count alone, and `release.yml` refuses to publish a tag
without a green `v2-rust.yml` run for that exact commit SHA.
