# Changelog

[Documentation home](README.md) · [Project overview](../README.md) ·
[Latest GitHub release](https://github.com/adder-factory/cartograph/releases/latest) ·
[Verification evidence](v2/benchmarks/README.md)

One line per stable release, newest first. Each version links to its compact
release notes, which are also the description of the matching
[GitHub release](https://github.com/adder-factory/cartograph/releases) and link
to the original detailed notes. Published binaries, signed tags, checksums, and
provenance live on GitHub Releases.

> [!TIP]
> Upgrading between any two v2 releases is one resumable command:
> `cartograph upgrade --apply --project-path <PATH>`. Require `completed: true`
> and follow any backup or confirmation step it prints.

**On this page:** [Release index](#release-index) ·
[Reading the index](#reading-the-index) · [Dependency audits](#dependency-audits) ·
[Earlier releases](#earlier-releases) · [Writing release notes](#writing-release-notes)

## Release index

| Version | Date | Schema · Contract | Headline |
| --- | --- | --- | --- |
| [v2.1.42](releases/v2.1.42.md) | 2026-10-07 | 50 ↑ · V23 ↑ | v1 framework and bridge parity; typed receivers; oracles nearly complete |
| [v2.1.41](releases/v2.1.41.md) | 2026-10-06 | 49 ↑ · V22 ↑ | v1 cross-file resolution parity; exact v1 resolution oracle |
| [v2.1.40](releases/v2.1.40.md) | 2026-10-06 | 48 ↑ · V21 ↑ | v1 per-file extraction parity for every language; exact v1 parity oracle; credential screening |
| [v2.1.39](releases/v2.1.39.md) | 2026-10-04 | 47 · V20 | ParadeDB 0.26.0 with pgvector 0.8.6; Rust dependency refresh |
| [v2.1.38](releases/v2.1.38.md) | 2026-10-02 | 47 ↑ · V20 ↑ | Rust turbofish calls name and resolve their function |
| [v2.1.37](releases/v2.1.37.md) | 2026-10-02 | 46 · V19 | Retryable `schema_busy` migrations; large rebuilds no longer fail in `reduce` |
| [v2.1.36](releases/v2.1.36.md) | 2026-10-02 | 46 ↑ · V19 ↑ | Rust references inside macro arguments are recorded |
| [v2.1.35](releases/v2.1.35.md) | 2026-10-02 | 45 · V18 | Fixes CLI admin index hang; `lease_busy` contention; Rust 1.99.0 |
| [v2.1.34](releases/v2.1.34.md) | 2026-10-01 | 45 · V18 | Large embedded projects re-index again; `apiKeyCommand` credentials |
| [v2.1.33](releases/v2.1.33.md) | 2026-10-01 | 45 ↑ · V18 | Retention keeps up with failed indexing; faster BM25; ParadeDB 0.25.11 |
| [v2.1.32](releases/v2.1.32.md) | 2026-09-25 | 44 · V18 | Automatic index retry recovery; Jev fixes; ParadeDB 0.25.10 |
| [v2.1.31](releases/v2.1.31.md) | 2026-09-22 | 44 ↑ · V18 ↑ | Optional Jev-guided retrieval for `explore` |
| [v2.1.30](releases/v2.1.30.md) | 2026-09-14 | 43 ↑ · V17 ↑ | ParadeDB 0.25.9 and Rust dependency refresh |
| [v2.1.29](releases/v2.1.29.md) | 2026-09-10 | 42 · V16 | ParadeDB 0.25.7 with extended upgrade recovery coverage |
| [v2.1.28](releases/v2.1.28.md) | 2026-09-08 | 42 ↑ · V16 ↑ | Resumable generation retention; Rust 1.98.1, Tree-sitter 0.27, ParadeDB 0.25.6 |
| [v2.1.27](releases/v2.1.27.md) | 2026-08-25 | 40 ↑ · V15 ↑ | Ada/SPARK and VHDL support; explicit biomarker refresh timeout |
| [v2.1.26](releases/v2.1.26.md) | 2026-08-23 | 39 · V14 | Corrects the SonarQube project version |
| [v2.1.25](releases/v2.1.25.md) | 2026-08-23 | 39 · V14 | Shell-free `cli-bridge` LLM provider; managed migration diagnostics |
| [v2.1.24](releases/v2.1.24.md) | 2026-08-18 | 39 · V14 | Workspace dependency inheritance enforced in CI |
| [v2.1.23](releases/v2.1.23.md) | 2026-08-18 | 39 · V14 | Explicit non-blocking biomarker refresh; endpoint probe and dead-code fixes |
| [v2.1.22](releases/v2.1.22.md) | 2026-08-18 | 39 ↑ · V14 | Run-scoped exclusions stay fresh; `db compact --heap`; `find --format` |
| [v2.1.21](releases/v2.1.21.md) | 2026-08-17 | 38 · V14 | Auto-sync capacity circuit breaker; ParadeDB 0.25.3 |
| [v2.1.20](releases/v2.1.20.md) | 2026-08-15 | 38 · V14 | Slang span fix; large Rust workspaces stream via PostgreSQL |
| [v2.1.19](releases/v2.1.19.md) | 2026-08-15 | 38 · V14 | Tag-only candidate, superseded by v2.1.20 (no GitHub release) |
| [v2.1.18](releases/v2.1.18.md) | 2026-08-15 | 38 · V14 | Parse failures name the file and a specific reason |
| [v2.1.17](releases/v2.1.17.md) | 2026-08-14 | 38 · V14 | ParadeDB `pg_search` 0.25.2; resumable managed database upgrade |
| [v2.1.16](releases/v2.1.16.md) | 2026-08-14 | 38 · V14 | Clears eleven code-health findings through internal refactoring |
| [v2.1.15](releases/v2.1.15.md) | 2026-08-13 | 38 ↑ · V14 ↑ | Recoverable indexing; Slang and WESL; reused embeddings |
| [v2.1.14](releases/v2.1.14.md) | 2026-08-10 | 37 · V13 | Content-search path filter, symbol-batch, and `sync-if-dirty` fixes |
| [v2.1.13](releases/v2.1.13.md) | 2026-08-09 | 37 · V13 | Prompt auto-sync; managed database resource ceilings; `pg_search` 0.25.1 |
| [v2.1.12](releases/v2.1.12.md) | 2026-08-05 | 37 ↑ · V13 | Stored structural findings; flexible exclusions; WGSL and Metal |
| [v2.1.11](releases/v2.1.11.md) | 2026-08-04 | 36 ↑ · V13 ↑ | PostgreSQL-backed streaming pipeline for large generations |
| [v2.1.10](releases/v2.1.10.md) | 2026-08-03 | 31 ↑ · V12 ↑ | Actionable large-index capacity errors; opt-in `maxGenerationBytes` |
| [v2.1.9](releases/v2.1.9.md) | 2026-08-03 | 30 ↑ · V11 ↑ | Large Rust closures no longer block publication; review schema fixes |
| [v2.1.8](releases/v2.1.8.md) | 2026-08-03 | 29 · V10 | Dependency cleanup; no schema or contract change |
| [v2.1.7](releases/v2.1.7.md) | 2026-08-02 | 29 ↑ · V10 ↑ | 52 game-scripting languages; resumable `cartograph upgrade --apply` |
| [v2.1.6](releases/v2.1.6.md) | 2026-08-02 | 28 ↑ · V9 ↑ | Eight code-health false-positive fixes (issue #114) |
| [v2.1.5](releases/v2.1.5.md) | 2026-08-02 | 27 ↑ · V8 ↑ | Auto-sync backoff, upgrade pin repair, detector precision fixes |
| [v2.1.4](releases/v2.1.4.md) | 2026-08-02 | 26 ↑ · V7 ↑ | New `cartograph numerical` command and MCP tool |

## Reading the index

- **Schema** is the PostgreSQL schema version the release migrates to; an
  upgrade applies every pending append-only migration in order.
- **Contract** is the generation digest contract. When it changes (↑), existing
  generations report stale and an ordinary index (or `upgrade --apply`)
  republishes them once, even when source is unchanged.
- ↑ marks a value that changed in that release. Values are read from each
  tagged release's source.
- v2.1.19 was a tag-only candidate that was never published as a GitHub
  release; v2.1.20 ships the same fixes.

Release notes are historical records. For the current command surface and
limits, use the installed `cartograph <command> --help` and the current
[CLI reference](CLI-REFERENCE.md); confirmation phrases are listed in
[Storage and operations](STORAGE-BACKENDS.md#destructive-operations-and-confirmation-phrases).

## Dependency audits

Dated records of dependency selection and maintenance reviews.

| Date | Shipped in | Record | Scope |
| --- | --- | --- | --- |
| 2026-10-04 | [v2.1.39](releases/v2.1.39.md) | [Dependency update](v2/DEPENDENCY-UPDATE-2026-10-04.md) | 87 direct dependencies; ParadeDB 0.26.0 with pgvector 0.8.6 |
| 2026-10-02 | [v2.1.35](releases/v2.1.35.md) | [Dependency update](v2/DEPENDENCY-UPDATE-2026-10-02.md) | Rust 1.99.0 toolchain and build images |
| 2026-10-01 | [v2.1.33](releases/v2.1.33.md) | [Dependency update](v2/DEPENDENCY-UPDATE-2026-10-01.md) | 86 direct dependencies; ParadeDB 0.25.11 |
| 2026-09-22 | [v2.1.31](releases/v2.1.31.md) | [Dependency update](v2/DEPENDENCY-UPDATE-2026-09-22.md) | 86 direct dependencies; refreshed build image digests |
| 2026-09-14 | [v2.1.30](releases/v2.1.30.md) | [Dependency update](v2/DEPENDENCY-UPDATE-2026-09-14.md) | 86 direct dependencies; ParadeDB 0.25.9 |
| 2026-09-08 | [v2.1.28](releases/v2.1.28.md) | [Dependency and retention update](v2/DEPENDENCY-UPDATE-2026-09-08.md) | Rust 1.98.1, Tree-sitter 0.27.0, resumable retention |
| 2026-09-08 | [v2.1.28](releases/v2.1.28.md) | [Dependency maintenance review](v2/DEPENDENCY-MAINTENANCE-2026-09-08.md) | Abandonment review of the full locked graph |

The [architecture improvement record](v2/ARCHITECTURE-IMPROVEMENTS.md) documents
the 2026-09-08 architecture review that shipped alongside the v2.1.28 update.

## Earlier releases

Releases before v2.1.4 (v2.0.0–v2.1.3) and the v1.x line keep their notes only
on [GitHub Releases](https://github.com/adder-factory/cartograph/releases). V2
imports only from a v1.1.33 PostgreSQL schema; see
[Import from v1.1.33 PostgreSQL](STORAGE-BACKENDS.md#import-from-v1133-postgresql).

## Writing release notes

`release.yml` publishes `docs/releases/<tag>.md` verbatim as the GitHub release
description. The file must start with `# Cartograph <tag>` and must not contain
unfinished-work marker words, or publication fails. Keep it compact (about 15–30
lines) and use this shape:

```markdown
# Cartograph vX.Y.Z

One sentence saying what this release is about.

## Changes

- 3–6 bullets: user-visible behavior, new commands/flags/tools/config keys,
  fixed failure modes (name the error code), operator-relevant dependency or
  image changes.

## Upgrade

Run `cartograph upgrade --apply --project-path <PATH>`, plus any extra action
(managed database replacement, re-index, host restart, config change).

**Compatibility:** schema N · contract VN · PostgreSQL / `pg_search` / pgvector requirements.

**Full changelog:** [vX.Y.(Z-1)...vX.Y.Z](https://github.com/adder-factory/cartograph/compare/vX.Y.(Z-1)...vX.Y.Z)
```

Use absolute `https://github.com/adder-factory/cartograph/blob/main/...` links;
repository-relative links do not resolve in a GitHub release description. Put
long-form detail in the linked dependency audit, the guides, or the pull request,
and add the release to the [release index](#release-index) in the same change.
