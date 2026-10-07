# Frozen v1 resolved-target oracle

This is the index-time counterpart of
`cartograph-extract/tests/v1_parity_oracle.rs`. The test module is
`cartograph-indexer/src/native_pipeline/tests/v1_resolution_oracle.rs`, registered
inside `native_pipeline::tests` so it can use the production private resolver and
canonical reduction boundary. There is no shipped test API, database, legacy
binary invocation, network operation or new source corpus.

The input is all 73 immutable v1.1.33 corpora and captures under
`cartograph-extract/tests/fixtures/v1_parity/`. A length-framed BLAKE3 seal fixes
all corpus paths, source bytes and capture bytes. Provenance, membership, the
8,231 original records, and 1,730 non-containment resolution records are checked.
Six repeated edges share dispositions, leaving 1,724 distinct selectors.

## Exact evidence

A unique frozen owner and target declaration supply exact kind, fully qualified
name and declaration start. Target file is always part of identity. File roots
use their exact relative path at line 1. A captured occurrence line is mandatory
when present. Native evidence is an actual canonical graph edge; its recorded
sites must come from references with the same resolved source, target and edge
kind. The production `Inherits` to graph-kind conversion is retained when
projecting reference sites. Unresolved reference names never carry a target.

Native keys retain both endpoint end lines for explicit canonical pins. Identity
omits only those end lines and still requires uniquely readable declarations.
Native key collisions are retained; no nearest name, suffix, proximity or runtime
alignment inference participates in matching. Diagnostic suggestions are limited
to three same-file pins. Alignments are literal conjunctive pins with evidence;
corrected v1 misresolutions are intentional exclusions with source evidence.

The oracle owns its alignment/ledger/reason tables. Existing extraction S/R pins
were inspected to establish declaration and owner correspondences when authoring
these rows; the test never reads extraction policy to guess a resolved target.
In particular, MyBatis extraction scope exclusions are checked as real resolved
relationships here. All six captured `tests` edges match exact targets.

`alignments.jsonl` and `divergences.jsonl` each store one typed `Row` per line.
An alignment has `status: aligned`, canonical `pins`, and a reason `id`.
A divergence is `pending` with wave 2/3 and a gap ID, or `intentional` with a
reason ID. `reasons.jsonl` stores each explanation once. Existing Wave 2 IDs come
from `wave2-tracks.json`; newly named resolution gaps and Wave 3 extraction or
representation gaps are documented in the reason table.

Hygiene rejects absent captured selectors, duplicate dispositions, empty or
undefined evidence, unused reasons, wrong waves, empty/duplicate/overlapping
pins, ambiguous pins, and redundant policy rows after an exact identity succeeds.
Frozen ambiguous endpoints require their specific ambiguity reason and cannot
be aligned. `gate_cases.jsonl` exercises these rules through the same gate.

## Running and updating

```sh
cargo test --locked -p cartograph-indexer --lib v1_resolution_oracle -- --nocapture
# Read-only inventory of frozen selectors, resolved graph pins and native facts:
cargo test --locked -p cartograph-indexer --lib v1_resolution_oracle::resolution_inventory -- --ignored --nocapture
```

A resolver fix makes a corresponding pending/aligned row fail as stale when exact
identity succeeds. Remove that row and any now-unused reason. For a genuine shape
difference, inspect source, captured declarations, reference sites and the real
native resolved edge, then record complete canonical pins. Do not change captures
or the corpus seal, widen matching, or convert a corrected target into parity.

## Measurement for the v2.1.42 release

| Disposition | Distinct facts |
| --- | ---: |
| Matched exactly | 679 |
| Aligned by exact pins | 386 |
| Pending Wave 3 | 96 |
| Intentional | 563 |
| Total | 1,724 |

At the Wave 2 base (`e820b5ea`) the same facts were 517 exact, 243 aligned,
833 pending and 131 intentional; v2.1.41 shipped 602 / 295 / 421 / 406.
Intentional facts include captures without a unique target or owner, external
imports v2 leaves targetless, v1 import-node and callable def-use
representations, and v1 misresolutions with source evidence. A passing oracle
means every fact is accounted for; 96 facts remain pending. It does not
assert complete resolution parity.

Mutation testing samples one carried edge per corpus/kind/disposition, preferring
cross-file cases. It deletes or retargets the real native `EdgeInput`, reprojects
the graph and requires a failure for that specific original fact using the same
committed policy. At this release all **470/470** mutants failed (100%
kill rate): 235 sampled edges, including 150 cross-file and 92 aligned edges.

The pending inventory below lists every gap, sorted by fact count and gap ID.

| Gap ID | Wave | Pending facts |
| --- | ---: | ---: |
| `instance-receiver-member-resolution` | 3 | 19 |
| `static-qualified-member-calls` | 3 | 17 |
| `unqualified-project-call-target-resolution` | 3 | 13 |
| `intra-class-receiver-calls` | 3 | 12 |
| `jvm-wildcard-import-blocks-fallback` | 3 | 8 |
| `native-qualified-type-target-resolution` | 3 | 7 |
| `dart-imported-constructor-target-resolution` | 3 | 4 |
| `jvm-explicit-import-resolution` | 3 | 4 |
| `ts-default-public-methods-invisible` | 3 | 4 |
| `module-qualified-call-target-resolution` | 3 | 2 |
| `native-bridge-physical-target-identity` | 3 | 2 |
| `rust-workspace-path-target-resolution` | 3 | 2 |
| `commonjs-callable-import-ownership` | 3 | 1 |
| `qualified-name-suffix-match` | 3 | 1 |
