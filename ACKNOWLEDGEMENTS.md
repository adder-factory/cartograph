# Acknowledgements

Cartograph stands on other people's open-source work. Cartograph itself is
released under the MIT License; every dependency and external service retains
its own license and terms.

## Origin project

Cartograph began as a fork of
[codegraph](https://github.com/colbymchenry/codegraph) by Colby Mchenry, used
under the MIT License (Copyright (c) 2026 Colby Mchenry). Version 2 is a native
Rust/PostgreSQL architecture and no longer ships the original TypeScript/Bun
runtime, but the project's product direction and many code-intelligence
concepts grew from that foundation.

## Rust runtime libraries

The native executable uses these principal projects:

| Project | License | Use |
| --- | --- | --- |
| [Rust](https://www.rust-lang.org/) | MIT OR Apache-2.0 | Language, standard library, and toolchain |
| [Tokio](https://tokio.rs/) | MIT | Bounded asynchronous runtime, process supervision, deadlines, and cancellation |
| [SQLx](https://github.com/launchbadge/sqlx) (`sqlx-core`, `sqlx-postgres`) | MIT OR Apache-2.0 | PostgreSQL protocol, pools, transactions, COPY, and typed row decoding |
| [Serde](https://serde.rs/) / [serde_json](https://github.com/serde-rs/json) | MIT OR Apache-2.0 | Validated JSON protocol and persistence boundaries |
| [clap](https://github.com/clap-rs/clap) | MIT OR Apache-2.0 | Native CLI parsing and help |
| [Tree-sitter](https://tree-sitter.github.io/tree-sitter/) | MIT | Incremental concrete syntax parsing |
| [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | Stable identities, source revisions, and logical generation digests |
| [reqwest](https://github.com/seanmonstar/reqwest) | MIT OR Apache-2.0 | HTTP client for optional LLM, embedding, and Jev providers and for `cartograph upgrade` release downloads |
| [rustls](https://github.com/rustls/rustls) | Apache-2.0 OR ISC OR MIT | TLS used by PostgreSQL connections and HTTPS requests |
| [ring](https://github.com/briansmith/ring) | Apache-2.0 AND ISC | Cryptography provider for rustls |
| [ignore](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) | Unlicense OR MIT | Git-compatible bounded source discovery |
| [notify](https://github.com/notify-rs/notify) | CC0-1.0 | Native recursive filesystem watching with a bounded polling fallback |
| [secrecy](https://github.com/iqlusioninc/crates/tree/main/secrecy) | Apache-2.0 OR MIT | Secret-bearing database URL wrappers |
| [toml_edit](https://github.com/toml-rs/toml) | MIT OR Apache-2.0 | Format-preserving Codex MCP configuration updates |
| [tempfile](https://github.com/Stebalien/tempfile) | MIT OR Apache-2.0 | Private temporary files and atomic configuration persistence |
| [thiserror](https://github.com/dtolnay/thiserror) | MIT OR Apache-2.0 | Structured, redacted error contracts |

Crate licenses are the SPDX expressions recorded for the locked versions.

The release gate runs `cargo deny` against the locked dependency graph for
advisories, licenses, banned SQLite crates, duplicate exceptions, and source
provenance. `Cargo.lock` is the authoritative version inventory for a release.

## Native Tree-sitter grammars

Cartograph links the Rust grammar crates listed below. It does not bundle the
old v1 WebAssembly grammar collection. `Cargo.lock` (exact versions) and
`deny.toml` (the enforced license allowlist) remain the authoritative
inventory.

| Grammar family | Upstream | License |
| --- | --- | --- |
| TypeScript and TSX | [tree-sitter/tree-sitter-typescript](https://github.com/tree-sitter/tree-sitter-typescript) | MIT |
| JavaScript and JSX | [tree-sitter/tree-sitter-javascript](https://github.com/tree-sitter/tree-sitter-javascript) | MIT |
| Rust | [tree-sitter/tree-sitter-rust](https://github.com/tree-sitter/tree-sitter-rust) | MIT |
| Python | [tree-sitter/tree-sitter-python](https://github.com/tree-sitter/tree-sitter-python) | MIT |
| Go | [tree-sitter/tree-sitter-go](https://github.com/tree-sitter/tree-sitter-go) | MIT |
| OCaml (`tree-sitter-ocaml`) | [tree-sitter/tree-sitter-ocaml](https://github.com/tree-sitter/tree-sitter-ocaml) | MIT |
| Embedded templates (`tree-sitter-embedded-template`) | [tree-sitter/tree-sitter-embedded-template](https://github.com/tree-sitter/tree-sitter-embedded-template) | MIT |
| 42 `arborium-*` grammar crates (41 declared directly, plus `arborium-javascript`, pulled in by `arborium-html`) | [bearcove/arborium](https://github.com/bearcove/arborium) | MIT, except `arborium-clojure` and `arborium-fish` (Unlicense) and `arborium-elixir` and `arborium-hcl` (Apache-2.0) |
| ABAP (`tree-sitter-abap-sqry`, `sqry-tree-sitter-support`) | [verivus-oss/sqry](https://github.com/verivus-oss/sqry) | MIT |
| Ada (`tree-sitter-ada`) | [briot/tree-sitter-ada](https://github.com/briot/tree-sitter-ada) | MIT |
| ArkTS (`tree-sitter-arkts`) | [harmony-contrib/tree-sitter-arkts](https://github.com/harmony-contrib/tree-sitter-arkts) | MIT |
| Astro (`tree-sitter-astro-next`) | [PRRPCHT/tree-sitter-astro-next](https://github.com/PRRPCHT/tree-sitter-astro-next) | MIT OR Apache-2.0 |
| CUDA (`tree-sitter-cuda`) | [tree-sitter-grammars/tree-sitter-cuda](https://github.com/tree-sitter-grammars/tree-sitter-cuda) | MIT |
| Luau (`tree-sitter-luau`) | [tree-sitter-grammars/tree-sitter-luau](https://github.com/tree-sitter-grammars/tree-sitter-luau) | MIT |
| Pascal (`tree-sitter-pascal`) | [Isopod/tree-sitter-pascal](https://github.com/Isopod/tree-sitter-pascal) | MIT |
| Prisma (`tree-sitter-prisma-io`) | [victorhqc/tree-sitter-prisma](https://github.com/victorhqc/tree-sitter-prisma) | MIT |
| Salesforce Apex (`tree-sitter-sfapex`) | [aheber/tree-sitter-sfapex](https://github.com/aheber/tree-sitter-sfapex) | MIT |
| Slang (`tree-sitter-slang`) | [theHamsta/tree-sitter-slang](https://github.com/theHamsta/tree-sitter-slang) | MIT |
| WGSL (`tree-sitter-wgsl-bevy`) | [tree-sitter-grammars/tree-sitter-wgsl-bevy](https://github.com/tree-sitter-grammars/tree-sitter-wgsl-bevy) | MIT |

The `arborium-*` crates cover Bash, C, C#, Clojure, Common Lisp, C++, CSS,
Dart, Elixir, Fish, F#, GLSL, GraphQL, Groovy, Haskell, HCL, HLSL, HTML, Java,
JavaScript, JSDoc, JSON, Julia, Kotlin, Lean, Lua, Nix, Objective-C, PHP,
PowerShell, R, regular expressions, ReScript, Ruby, Scala, Solidity, SQL,
Swift, Visual Basic, Verilog, VHDL, and YAML.

Max Brunsfeld and Tree-sitter contributors created and maintain the parsing
runtime; the grammar repositories are maintained by their respective
communities.

## External database services

Cartograph requires separately installed services and extensions:

- [PostgreSQL](https://www.postgresql.org/) — PostgreSQL License;
- [ParadeDB / `pg_search`](https://github.com/paradedb/paradedb) — AGPL-3.0
  or commercial terms published by ParadeDB;
- [pgvector](https://github.com/pgvector/pgvector) — PostgreSQL License.

These projects are not copied into or redistributed with Cartograph's native
release archives. `cartograph db start` instructs the user's local Docker daemon
to pull an upstream, digest-pinned ParadeDB image as a separate service. The
enforced distribution and supported-use boundary ships in each release archive
as `share/cartograph/PARADEDB-NOTICE.md`; its source is
[`docs/v2/LICENSING.md`](https://github.com/adder-factory/cartograph/blob/main/docs/v2/LICENSING.md).

## Build and quality tools

Cartograph's development and release gates also use:

- [Clippy](https://github.com/rust-lang/rust-clippy) and rustfmt from the pinned
  Rust toolchain;
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), Apache-2.0 / MIT;
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov), Apache-2.0 / MIT;
- [SonarQube](https://www.sonarsource.com/products/sonarqube/) for independent
  static analysis and coverage gates;
- GitHub Actions maintained by GitHub and their named upstream authors, pinned
  by commit in `.github/workflows/`.

If a project or author is missing or miscredited, please open an issue or pull
request so the acknowledgement can be corrected.
