# Cartograph documentation

[Project overview](../README.md) · [Quick start](../README.md#quick-start) ·
[CLI reference](CLI-REFERENCE.md) · [Troubleshooting](TROUBLESHOOTING.md) ·
[Changelog](RELEASES.md)

Use this page to find the shortest path to an answer. The root
[README](../README.md) explains the product and the first successful setup; the
guides below own the detailed operational and implementation contracts.

> [!TIP]
> Installed command help is authoritative for exact flags, defaults, and
> bounds: `cartograph <command> --help`. Confirmation phrases for destructive
> operations are listed in
> [Storage and operations](STORAGE-BACKENDS.md#destructive-operations-and-confirmation-phrases).

## Find the right guide

| I want to… | Start here |
| --- | --- |
| Install Cartograph myself | [Quick start](../README.md#quick-start) |
| Ask a coding agent to install and verify it | [Agent-assisted installation](AGENT-INSTALL.md) |
| Upgrade an existing installation | [Upgrade an existing installation](AGENT-INSTALL.md#upgrade-an-existing-installation) |
| Look up a command or JSON behavior | [CLI reference](CLI-REFERENCE.md) |
| Connect or debug an MCP host | [MCP usage](MCP-USAGE.md) |
| Translate between a CLI command and an MCP tool | [CLI/MCP alignment](cli-mcp-alignment.md) |
| Check language, extension, or framework support | [Language support matrix](SUPPORT-MATRIX.md) |
| Configure source policy, limits, retrieval, or models | [Configuration](CONFIGURATION.md) |
| Choose managed or external PostgreSQL | [Storage and operations](STORAGE-BACKENDS.md) |
| Diagnose an error code, failed setup, or stale index | [Troubleshooting](TROUBLESHOOTING.md) |
| Tune a large or memory-sensitive repository | [Performance tuning](PERF-TUNING.md) |
| Understand code-health findings | [Native code-health detectors](BIOMARKER-DETECTORS.md) |
| Export the graph to another tool | [Graph export formats](GRAPH-EXPORT-FORMATS.md) |
| Add a language, grammar, resolver, or framework bridge | [Extending extraction and resolution](EXTENDING-EXTRACTORS-RESOLVERS.md) |
| Understand architecture and trust boundaries | [Cartograph v2 architecture](v2/ARCHITECTURE.md) |
| Inspect scaling and task-quality evidence | [Verification and benchmarks](v2/benchmarks/README.md) |
| See what changed between releases | [Changelog](RELEASES.md) |

## Recommended reading paths

<table>
<tr>
<td valign="top" width="50%">

**🚀 First successful setup**

1. Follow the [quick start](../README.md#quick-start).
2. Run `doctor`, publish the first index, and require fresh `status`.
3. Make one real deterministic or hybrid query.
4. Register the host through [MCP usage](MCP-USAGE.md), reopen it, and prove
   one live MCP status/query pair.

</td>
<td valign="top" width="50%">

**🤖 Coding-agent integration**

1. Copy the task from [Agent-assisted installation](AGENT-INSTALL.md).
2. Select the narrowest appropriate MCP profile in
   [MCP usage](MCP-USAGE.md#profiles).
3. Use the [CLI/MCP alignment](cli-mcp-alignment.md) when translating between
   operator commands and agent tools.
4. Follow the freshness → context → impact → review loop in the
   [agent workflow](../README.md#agent-workflow).

</td>
</tr>
<tr>
<td valign="top">

**🛠️ Operations and large repositories**

1. Choose database ownership in [Storage and operations](STORAGE-BACKENDS.md).
2. Review source admission and hard limits in [Configuration](CONFIGURATION.md).
3. Measure before changing workers or timeouts with
   [Performance tuning](PERF-TUNING.md).
4. Use [Troubleshooting](TROUBLESHOOTING.md) for typed failure states and safe
   recovery paths.

</td>
<td valign="top">

**🧩 Language and extractor development**

1. Check the current product boundary in the
   [language support matrix](SUPPORT-MATRIX.md).
2. Review the measured [language-coverage report](LANGUAGE-COVERAGE-REPORT.md)
   and [grammar provenance](GRAMMAR-ASSETS.md).
3. Choose the smallest correct implementation mechanism in
   [Extending extraction and resolution](EXTENDING-EXTRACTORS-RESOLVERS.md).
4. Preserve the deterministic and privacy boundaries in the
   [native extraction contract](v2/EXTRACTION.md).

</td>
</tr>
</table>

## Guide catalog

### Setup and daily use

| Guide | What it covers |
| --- | --- |
| [Agent-assisted installation](AGENT-INSTALL.md) | End-to-end installation, upgrade, indexing, registration, and verification task |
| [CLI reference](CLI-REFERENCE.md) | Complete top-level command inventory and stable automation behavior |
| [MCP usage](MCP-USAGE.md) | Registration, profiles, protocol behavior, tool selection, transport proof, and reliable agent loops |
| [CLI/MCP alignment](cli-mcp-alignment.md) | One-to-one and family mappings between the two public surfaces |
| [Configuration](CONFIGURATION.md) | Project policy, database environment, optional model tiers, and bounded runtime settings |

### Languages and code intelligence

| Guide | What it covers |
| --- | --- |
| [Language support matrix](SUPPORT-MATRIX.md) | Languages, extensions, extractor depth, framework signals, and embedded DSLs |
| [Language-coverage report](LANGUAGE-COVERAGE-REPORT.md) | Current validation and coverage evidence |
| [Grammar provenance](GRAMMAR-ASSETS.md) | Pinned native grammar ownership and review boundary |
| [Game scripting coverage](v2/GAME-SCRIPTING-LANGUAGES.md) | Researched and testable game/modding language boundary |
| [Code-health detectors](BIOMARKER-DETECTORS.md) | Native finding contracts, evidence levels, privacy, and interpretation |
| [Graph export formats](GRAPH-EXPORT-FORMATS.md) | JSON, Cytoscape, DOT, Mermaid, and SCIP interchange |

### Operations and reliability

| Guide | What it covers |
| --- | --- |
| [Storage and operations](STORAGE-BACKENDS.md) | PostgreSQL ownership, capabilities, migration, backup, recovery, retention, and compaction |
| [Performance tuning](PERF-TUNING.md) | Worker selection, storage strategy, measurement, and safe tuning boundaries |
| [Troubleshooting](TROUBLESHOOTING.md) | Symptom-oriented diagnosis and bounded recovery |
| [Distribution and licensing](v2/LICENSING.md) | Cartograph/ParadeDB packaging and deployment boundary |

### Architecture and contribution

| Guide | What it covers |
| --- | --- |
| [Cartograph v2 architecture](v2/ARCHITECTURE.md) | Implemented crate, database, generation, retrieval, lease, MCP, and release architecture |
| [Native extraction contract](v2/EXTRACTION.md) | Discovery, parsing, facts, resolution, publication, and test routing |
| [Extending extraction and resolution](EXTENDING-EXTRACTORS-RESOLVERS.md) | Implementation checklist and required gates |
| [Adding a language](ADDING-A-LANGUAGE.md) | Concise entry point for selecting an extension mechanism |
| [Standing architecture rules](ARCHITECTURE.md) | Durable repository rules |
| [Architecture decision records](decisions/README.md) | Decisions and their long-lived tradeoffs |

### Evidence and history

| Record | What it covers |
| --- | --- |
| [Changelog](RELEASES.md) | Compact summary of every stable release, with links to full notes |
| [GitHub releases](https://github.com/adder-factory/cartograph/releases) | Signed tags, native archives, checksums, and provenance |
| [Verification and benchmarks](v2/benchmarks/README.md) | What each benchmark proves, its status, and how to reproduce it |
| [Dependency audits](RELEASES.md#dependency-audits) | Dated dependency selection and maintenance reviews |
| [Architecture improvement record](v2/ARCHITECTURE-IMPROVEMENTS.md) | Scope and acceptance evidence for the 2026-09-08 architecture review |

## Documentation conventions

These rules keep the guide set navigable and trustworthy:

- Every guide opens with a navigation line back to this page and its closest
  neighbors. Long guides add an **On this page** list after the introduction.
- Release-audited guides identify their version; historical benchmarks remain
  explicitly labeled and are not presented as current reruns.
- Each section leads with what to do; deep edge-case semantics live in
  collapsible **Details** blocks rather than being removed.
- Destructive or replacement operations retain dry-run, backup, ownership, and
  exact-confirmation boundaries in both documentation and implementation, and
  are flagged with a caution callout.
- Database URLs, credentials, private paths, source literals, and internal
  recovery state do not belong in public examples or release artifacts.
- Documentation changes should pass the implementation-backed contract tests;
  every internal file and heading link must resolve before publication.
