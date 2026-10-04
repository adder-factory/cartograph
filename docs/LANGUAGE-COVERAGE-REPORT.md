# Native language-coverage report

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Grammar provenance](GRAMMAR-ASSETS.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last release audit: 2026-10-04 (`v2.1.39`).

Cartograph v2.1.39 production-admits all 73 v1.1.33 language modes and all 163 v1
extensions, plus additive Python `.pyi`, native TOML, and 52 dedicated textual
game-scripting modes, the WGSL and Metal shader modes added in v2.1.12, and
Slang and WESL added in v2.1.15, plus Ada/SPARK and VHDL added in v2.1.27: 132
modes total. Sixty-six modes use pinned native tree-sitter grammars and 66
mixed-markup, configuration, domain-specific, shader, or game-scripting modes
use bounded Rust structural scanners or an existing family slice.

The authoritative inventory is `cartograph_domain::SourceLanguage::ALL` plus
the extension mapping in `cartograph-extract`. The human-readable inventory is
the [support matrix](SUPPORT-MATRIX.md). The dedicated game-language research
boundary and source trail are recorded in
[game scripting language coverage](v2/GAME-SCRIPTING-LANGUAGES.md).

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

Framework and cross-language resolver parity is audited separately from parser
admission. A grammar loading successfully does not prove route, call, import,
receiver, or bridge semantics.

Rust test surfaces include the focused extractor family suites under
`crates/cartograph-extract/tests/`, the frozen v1.1.33 oracle, the native corpus
supervisor under `crates/cartograph-indexer/tests/`, and live PostgreSQL/
ParadeDB language publication tests. The release workflow runs these against
the pinned database image rather than trusting a generated count alone.
