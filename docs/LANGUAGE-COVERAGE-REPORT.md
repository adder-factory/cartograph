# Native language-coverage report

[Documentation home](README.md) · [Language matrix](SUPPORT-MATRIX.md) ·
[Grammar provenance](GRAMMAR-ASSETS.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last release audit: 2026-10-04 (`v2.1.39`).

This report states what "supported" means for a language mode and where the
evidence for each admitted mode lives. For the per-language inventory, read the
[support matrix](SUPPORT-MATRIX.md).

**On this page:** [Current inventory](#current-inventory) ·
[Strategy families](#strategy-families) ·
[Admission contract](#admission-contract) ·
[v1 parity oracle](#v1-parity-oracle) ·
[Validation evidence](#validation-evidence)

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
extraction with what the real v1.1.33 release binary extracted from the same
files.

| Part | Location |
| --- | --- |
| Test target | `crates/cartograph-extract/tests/v1_parity_oracle.rs` (runs in `cargo test --locked --workspace`) |
| Fixture corpus, one per v1 mode | `crates/cartograph-extract/tests/fixtures/v1_parity/<mode>/` |
| Captured v1.1.33 facts | `crates/cartograph-extract/tests/fixtures/v1_parity/expected/<mode>.json` |
| Divergence ledger | `crates/cartograph-extract/tests/fixtures/v1_parity/divergences.json` |
| Capture script (maintainer-only) | `scripts/capture-v1-parity-oracle.sh` |

### What the oracle checks

The test runs v2's `NativeExtractor` over every corpus file, using the
corpus-relative path as the virtual path, and requires every v1 fact (symbols,
resolved edges, and unresolved references) to have a per-file v2 counterpart
under the mapping rules documented at the top of the test file: matching kinds,
the same start line, and names that are equal or share a final segment. It
checks that the per-file fact exists; it does not assert that a reference
binds to the same cross-file target as in v1.

Three tests make up the target:

| Test | Fails when |
| --- | --- |
| `v2_extraction_carries_every_v1_parity_fact_or_a_ledgered_divergence` | A v1 fact has no v2 match and no ledger entry, a ledger entry is stale, or v2's classifier assigns a file that v1 attributed to the mode to a different mode |
| `every_v1_language_mode_has_a_complete_parity_corpus` | One of the 73 v1 modes has no corpus or captured oracle, an oracle was not captured from v1.1.33 by the pinned binary (its SHA-256 is recorded), the corpus differs from the captured file list, a fact names a file outside the corpus, no corpus file is attributed to the mode, or a v1 kind has no mapping |
| `divergence_ledger_entries_are_well_formed_and_unique` | An entry names an unknown mode, is duplicated, lacks a required reason, has a blank gap id or a wave other than 1, 2, or 3, or gives an edge fact no source (or a non-edge fact one) |

To list every unmatched fact, set `CARTOGRAPH_V1_PARITY_REPORT` to a file path
whose directory exists (use an absolute path; the test runs in the crate
directory). The test writes the misses there as JSON in the ledger's fact
shape before it asserts:

```sh
CARTOGRAPH_V1_PARITY_REPORT="$HOME/v1-parity-misses.json" \
  cargo test --locked -p cartograph-extract --test v1_parity_oracle
```

### Ledger rules

A v1 fact that v2 does not reproduce is either fixed or recorded in
`divergences.json`. Each entry names one fact (mode, category, file, line,
kind, name, and the source for edges) and has:

| Field | Rule |
| --- | --- |
| `status` | `pending` (the fact returns when its port lands) or `intentional` (v2 deliberately differs) |
| `gap` | Optional audit gap id tracking the port; never blank |
| `wave` | Optional port wave: 1, 2, or 3 |
| `reason` | Why the fact diverges; required for `intentional` entries |

Unknown fields are rejected. An entry whose fact now matches, or no longer
exists in the oracle, fails the test, so the ledger can only shrink honestly:
fixing a gap means deleting its entries in the same change. Remaining v1 gaps
are scheduled as wave 2 (cross-file resolution) and wave 3 (framework and
bridge detail).

### Regenerating the fixtures

The capture script is maintainer-only. The v2 product, its tests, and CI never
run it or the v1 binary; the test only reads the JSON it writes. It needs the
v1.1.33 darwin-arm64 release binary (`cartograph-darwin-arm64.tar.gz`, checked
against that release's `SHA256SUMS` before unpacking) plus `sqlite3`, `jq`, and
`shasum`. It copies a corpus into a private temporary Git checkout, indexes it
with the v1 binary under an isolated `HOME`, and projects v1's graph to a
sorted, path-relative JSON document.

```sh
# From the repository root; v1_binary is the verified v1.1.33 cartograph binary.
fixtures=crates/cartograph-extract/tests/fixtures/v1_parity
for corpus in "$fixtures"/*/; do
  language="$(basename "$corpus")"
  [[ "$language" == expected ]] && continue
  scripts/capture-v1-parity-oracle.sh "$v1_binary" "$corpus" "$language" \
    "$fixtures/expected/$language.json"
done
cargo test --locked -p cartograph-extract --test v1_parity_oracle
```

Then update `divergences.json` for every fact whose status changed. A capture
made with any other build fails the coverage test, because each oracle records
the binary's SHA-256 and the test pins it.

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
