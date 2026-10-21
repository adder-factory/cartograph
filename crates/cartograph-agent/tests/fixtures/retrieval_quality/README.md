# Repository retrieval quality pilot

This manifest contains 60 repository questions with explicit source-symbol and
path targets: 40 development cases and 20 held-out cases. It includes the six
original diagnostic questions and questions across storage, indexing, source
navigation, coverage, configuration, and retrieval. The held-out cases were
written before running this pilot and were not used to tune the changes.

These are manually labeled questions about this repository, not a representative
sample of all user tasks or languages. A correct entry point can still fail an
exact target label. Keep the frozen five-task patch evaluation separate; its
metrics and purpose are different.

Run the same manifest against two binaries on an isolated source snapshot with
the same source admission policy and model configuration. Index and embed with
the baseline executable first. After its measurement process exits, index and
embed with the candidate executable and measure again. Keep the source bytes
fixed. Do not copy a live RocksDB directory, replace an active MCP executable,
or use a client that forwards to an older owner for this comparison.

```sh
python3 scripts/evaluate-retrieval.py \
  --binary /absolute/path/to/cartograph \
  --project /absolute/path/to/isolated/source \
  --output /absolute/path/to/private/measurement.json \
  --label candidate
```

The runner starts one MCP process with startup synchronization and automatic
watching disabled, validates labels before observing rankings, warms both query
modes, and sends sequential requests. It refuses stale source, generation changes,
or a hybrid request without complete semantic readiness. It changes no source,
configuration, model, or index. Closing the runner closes only its own process.

Each observation retains the returned symbol/path order, first correct rank,
elapsed request time, generation, readiness, fallback, and reranker state.
Reports include hit@5, mean reciprocal rank, target recall within the returned
bounded pool, and nearest-rank p50/p95 request latency. `--limit` defaults to 40;
`--repeat` can collect additional latency samples. The result limit can influence
the CLI's channel candidate window, so keep it identical for both measurements.
Latency includes MCP, native storage, query embedding, and reranking. It does not
separately attribute time to each stage or establish cold-start performance.

Both reports record the exact manifest hash and binary hash. Keep the held-out
partition and labels fixed when comparing implementations. Resolve an absent
label against source and admission policy before timing, not by selecting a new
target from observed results. `artifact-replace` deliberately retains one known
coverage miss under this workspace's `**/artifacts/**` exclusion; it stays in the
denominator and is disclosed by `target_coverage`. A project that admits that
source can satisfy the same case without changing the manifest.

Store full reports outside the source tree or under ignored `target/`; they
include local status evidence. No machine-specific result is a general quality
guarantee or a replacement for the ordinary native fault and patch-task gates.
