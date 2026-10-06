# Native code-health detectors

[Documentation home](README.md) · [Project overview](../README.md) ·
[CLI reference](CLI-REFERENCE.md) · [Architecture](v2/ARCHITECTURE.md)

Cartograph v2 derives deterministic, generation-fenced findings from native
extraction metrics, graph structure, Git/coverage evidence, architecture
policy, and duplicate-code analysis. The detector path does not require an
LLM. LLM-based dead-code judging is a separately labeled advisory workflow and
never authorizes deletion.

**On this page:** [Computing findings](#computing-findings) ·
[Detector catalog](#detector-catalog) · [Evidence contract](#evidence-contract) ·
[Detector rules](#detector-rules) · [Duplicate-code tiers](#duplicate-code-tiers) ·
[Privacy and safety](#privacy-and-safety)

## Computing findings

Biomarker reads are deliberately non-mutating. Inspect or build the detector
relation through the dry-run-first admin boundary:

```sh
cartograph admin biomarkers-refresh --json
cartograph admin biomarkers-refresh --no-dry-run --confirm \
  --database-query-timeout-ms 240000 --json
```

- The first command reports whether the exact relation would be recomputed.
- The second is the explicit confirmed mutation. Its timeout is caller-bounded
  from 1 through 1800000 milliseconds and is applied as the inner PostgreSQL
  statement timeout. The response reports both `statementTimeoutMs` and
  `statementTimeoutSource`; the caller deadline must be longer.
- The legacy `--timeout-ms`/`timeoutMs` spelling remains accepted when the
  explicit field is absent, and passing both is rejected.
- Indexing publishes canonical graph facts but does not implicitly compute this
  optional derived relation.

Before the exact relation has been computed, reads report it as missing instead
of computing it:

| Read | Before computation |
| --- | --- |
| `cartograph biomarkers` | Returns `state: not_computed` |
| Status | Returns a pending readiness state |
| Inline biomarker rollup | Returns an empty array with `biomarkerRollupState: not_computed` |
| Review-risk | Labels its structural-findings lens `not_computed` |

None of those reads start the whole-generation detector cascade.

<details>
<summary>Details: how the stored relation is fingerprinted and invalidated</summary>

The complete detector relation is evaluated once per exact input fingerprint
and stored generation-fenced, so repeated reads never re-evaluate it. The
fingerprint covers:

- the current generation;
- the superseded generation the growth detector compares against;
- imported coverage;
- materialised similarity;
- the calendar day bounding the `recently_grew` window;
- the detector contract compiled into the binary.

A detector-SQL change therefore invalidates every stored relation instead of
serving findings the shipped rules no longer produce.

</details>

## Detector catalog

The public detector names are:

| Family | Detectors |
| --- | --- |
| Size and complexity | `large_method`, `complex_method`, `nested_complexity`, `complex_conditional`, `brain_method`, `long_parameter_list`, `god_class`, `feature_envy` |
| Maintainability | `magic_number`, `hardcoded_url`, `recently_grew`, `stale_doc`, `unused_export`, `duplicate_code`, `incomplete_marker`, `empty_function_body` |
| Correctness and performance | `accidental_quadratic`, `empty_catch`, `sync_io_in_async`, `forof_await`, `unsafe_json_parse`, `http_no_timeout` |
| Type and agent residue | `ts_any_cast`, `ts_ignore_suppression`, `agent_debug_log` |
| Security | `secrets_handling`, `dynamic_eval`, `insecure_hash`, `random_for_security`, `sql_string_concat`, `env_no_validation` |
| Evidence and architecture | `low_coverage`, `illegal_import`, `high_fan_out`, `unresolved_reference_pressure` |

Some detectors need inputs that are not part of every index:

| Detector or signal | Required input |
| --- | --- |
| `low_coverage` | Exists only when a generation-fenced LCOV source is loaded |
| `recently_grew` and issue/churn signals | Bounded Git history |
| `illegal_import` | Explicit project `layers` policy |

Absence of those inputs is reported as unavailable/not configured, not a clean
finding.

## Evidence contract

Each finding carries its detector, severity, metric name/value, threshold,
symbol/file identity, generation, and graph pressure used for ranking. Findings
are filtered in PostgreSQL before the response cap. Rollup mode is untruncated;
ranked mode returns an exact pre-limit count and explicit truncation.

Thresholds are native release behavior. They live in
`crates/cartograph-db/src/finding_ctes.sql`, which
`crates/cartograph-db/src/insights.rs` loads at compile time; `illegal_import`
is evaluated against the project `layers` policy in
`crates/cartograph-agent/src/layering.rs`. Extraction metrics are produced under
`crates/cartograph-extract/src/walk/`; graph/history/coverage inputs remain
separately attributable. Do not duplicate threshold tables in another runtime.

All biomarker detectors operate on production symbol documents. Files in a
modeled test/fixture path and Rust declarations owned by `#[cfg(test)]`, a test
attribute, or an inline test module are published as test documents and stay
queryable, but do not enter the production finding population.

## Detector rules

Detector-specific eligibility and abstention rules, in catalog order.

### Size and complexity rules

**`large_method` and `brain_method`.** `large_method` and the size component
of `brain_method` use AST-owned code lines. Comment blocks and opaque multiline
literals count once, and nested callable bodies belong to their own symbols.
Static JSX tags, attributes, and text are presentation scaffolding; embedded
JSX expressions still contribute their executable syntax. Physical source spans
remain the navigation and history boundary.

**`nested_complexity`.** Direct `else if` and `elif` branches are treated as a
flat decision ladder. They still add cyclomatic branches; a nested `if` inside
an `else` block still increases depth.

**`long_parameter_list`.** Counts caller-supplied parameters, excluding an
explicit language receiver such as Rust `self`, TypeScript's erased `this`
parameter, and conventional Python method `self`/`cls` receivers.
Framework-owned route and entry-point signatures reuse the same convention
classification as `unused_export` and abstain; an ordinary authored function
remains eligible.

**`god_class`.** Measures production behavioral methods rather than accessors
or test-only helpers. A method contributes to the size metric only when it owns
at least five code lines, and a container must also own at least five methods
with cyclomatic complexity of three or more. This keeps record accessors,
settings objects, and fluent request builders out of the finding population
while still surfacing stateful containers that combine a broad API with
substantial logic.

### Maintainability rules

**`magic_number`.** Normalizes language numeric suffixes before applying the
benign 0/1/2 rules, so typed spellings such as Rust `0_u32` retain their
numeric meaning.

**`hardcoded_url`.** Context classified:

- Request/client destinations and values assigned to endpoint-like
  configuration are actionable and expose `request_destination`,
  `service_configuration`, or `mixed_endpoint_use` in their detail.
- Form placeholders, navigation/documentation links, presentation or vendor
  asset `src` values, URL-validation examples, and plain data literals abstain.
- The response includes request/configuration/presentation counts so an agent
  can explain the classification instead of treating every URL-shaped string
  as a deployment endpoint.

**`stale_doc`.** Compares declaration values only with documentation that
makes an explicit value claim through a cue such as `default`, `limit`,
`threshold`, or an equivalent assignment phrase. Illustrative numbers without
such a cue abstain.

**`unused_export`.** Deliberately conservative at external boundaries:

- Explicit public API declarations and files modeled as test/fixture sources
  are not claimed unused merely because the indexed checkout cannot contain
  their outside consumer.
- Internal/module exports are actionable only with no credible incoming use;
  one resolved type-only or runtime consumer is enough to abstain. Use rooted
  in another exported declaration in the same file also counts, covering
  schema, configuration, and ORM object assembly.
- Declaration-only type members never enter the export finding population.
- Ambiguous targetless receiver calls conservatively count as potential
  incoming use for any same-named non-private exported method; static member
  calls and literal static subscript keys retain that evidence while computed
  dynamic keys abstain.
- Framework-owned exports retain their convention-specific entry-point rules,
  including methods in `*.config.*` objects and default-export platform
  handlers such as `fetch`, `scheduled`, `queue`, `email`, `alarm`, and
  WebSocket callbacks.
- React `lazy(() => import(...))` records a default consumer, and TypeScript
  `typeof` queries retain typed value-consumer edges.
- Python's implicit module visibility is not explicit export intent, so Python
  declarations currently abstain from confident `unused_export` findings.

### Correctness and performance rules

**`forof_await`.** Specific to JavaScript-family `for ... of` loops whose own
body contains an `await`.

- Rust and other language loops, `for ... in`, `for await ... of`, nested
  callbacks, and nested loops do not contribute to the enclosing loop's metric.
- It also abstains when an awaited assignment feeds the next iteration, when
  control can `break` or `return` after the await, or when the loop contains
  the explicit `cartograph: serial-await` intent marker.
- A finding therefore represents independent iterations with no detected
  ordering dependency; detail reports the awaited-expression ownership and each
  abstention category.

### Security rules

**`secrets_handling`.** Retains a nonzero privacy-safe evidence mask for
sensitive identifiers and cryptographic handling, but that evidence alone is
not actionable.

- Finding eligibility additionally requires literal secret material, an
  environment-secret read, or an exposure operation such as logging the
  sensitive value.
- Environment access and exposure must contain the sensitive identifier in
  that same AST expression boundary; unrelated identifiers and log calls
  elsewhere in the symbol do not qualify.
- The evidence score and actionable classification remain separate persisted
  facts.

**`sql_string_concat`.** Requires dynamic text and a statement-shaped keyword
pair: `INSERT ... INTO`, `UPDATE ... SET`, or `DELETE ... FROM`.
`SELECT ... FROM` additionally needs SQL punctuation (`*`, `,`, `;`, or `=`) or
a downstream clause such as `WHERE`, `JOIN`, `GROUP`, `ORDER`, `LIMIT`,
`OFFSET`, or `HAVING`. Presentation prose such as an accessibility label
containing “select ... from” therefore abstains.

### Evidence and architecture rules

**`high_fan_out`.** Measures distinct resolved target symbols.

- Repeated sites to the same dependency, recursive self-edges, and targets
  already owned through a direct `contains` edge do not inflate the finding.
  Calls to sibling or external symbols remain dependencies.
- Detail groups dependencies into calls, modules, types, composition, data
  flow, and other edges.
- Cohesive functions that only assemble and return a facade of focused
  one-call delegates abstain; mixed orchestration remains actionable.
- The rule applies to executable and container symbols, not declarative
  constant tables or framework-owned convention exports.

**`unresolved_reference_pressure`.** Cartograph graph-resolution coverage, not
a compiler diagnostic. Its detail sets `compilerDiagnostic: false` and directs
the caller to confirm source correctness with the language toolchain before an
edit. It counts unresolved references that could still denote project
declarations.

- Expected macro-expansion, dynamic receiver, member-access,
  language-intrinsic (including Python built-ins), explicit non-local import,
  shell-command, and manifest-dependency boundaries retain distinct unresolved
  provenance but do not inflate that project-actionable metric.
- Python receiver calls remain targetless dynamic-dispatch evidence.
- Static embedded-SQL table references resolve to indexed SQL declarations when
  possible; otherwise their typed read/write/DDL provenance remains an explicit
  external-schema boundary rather than project-code symbol pressure.
- Relative and project-local imports stay actionable.
- The references remain queryable; this classification does not fabricate a
  target or convert ambiguity into a resolved edge.

## Duplicate-code tiers

Duplicate analysis distinguishes:

1. exact normalized body copies;
2. identifier/literal-normalized shape clones;
3. optional wider partial clones (`duplicateCodePartialClones: true`);
4. generation/model-scoped semantic peers when matching embeddings exist.

- **Allowlist.** Path globs in `duplicateCodeAllowlist` exempt deliberate
  generated/vendor copies.
- **Identifier overlap.** Identifier/literal-normalized and partial candidates
  additionally need a privacy-safe identifier-fingerprint overlap. Cross-file
  candidates use a strict overlap floor; same-file candidates may use their
  shared module context with a smaller but nonzero overlap. Cross-domain
  shapes with renamed callees therefore abstain.
- **Provenance.** The selected tier, peer provenance, line floor, and class
  members remain visible so a semantic resemblance is never presented as an
  exact duplicate.

<details>
<summary>Details: clone-class representatives and sizes</summary>

Each class emits one representative finding with `recordScope: clone_class`;
partial-clone representatives and class sizes come from the complete in-memory
overlap component rather than the bounded ten-peer display list. Semantic
representatives, class sizes, maximum scores, and bounded member lists come
from a current-generation connected component rather than a local-neighbor
minimum. Symmetric per-member rows are suppressed.

</details>

## Privacy and safety

Extraction stores privacy-safe metrics and category masks, not secret literals.
Signatures/search documents reject literal-bearing or credential-shaped values.

> [!IMPORTANT]
> Findings point an agent to exact source it must inspect; they are not proof
> of a vulnerability, dead code, or safe automated edit.

Use:

```sh
cartograph biomarkers --help
cartograph admin biomarkers-refresh --help
cartograph review agent-audit --help
cartograph hotspots --help
cartograph coverage --help
cartograph dead-code --help
```

MCP mirrors are `cartograph_biomarkers`, `cartograph_review`,
`cartograph_hotspots`, `cartograph_coverage`, and `cartograph_dead_code`; the
`cartograph_admin` `biomarkers-refresh` action mirrors the admin refresh.
