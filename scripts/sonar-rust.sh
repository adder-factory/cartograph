#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SONAR_OUTPUT_ROOT="${CARTOGRAPH_SONAR_OUTPUT_ROOT:-$ROOT/target}"
CLIPPY_DIRECTORY="$SONAR_OUTPUT_ROOT/sonar"
CLIPPY_REPORT="$CLIPPY_DIRECTORY/clippy.json"
CLIPPY_TEMPORARY="$CLIPPY_REPORT.tmp.$$"
COVERAGE_DIRECTORY="$SONAR_OUTPUT_ROOT/llvm-cov"
COVERAGE_BUILD_DIRECTORY="$COVERAGE_DIRECTORY/build"
COVERAGE_REPORT="$COVERAGE_DIRECTORY/lcov.info"

cleanup() {
  rm -f "$CLIPPY_TEMPORARY"
}
trap cleanup EXIT

coverage_test() {
  cargo test --locked --quiet --all-features "$@"
}

cd "$ROOT"
mkdir -p "$CLIPPY_DIRECTORY" "$COVERAGE_DIRECTORY"
rm -f "$CLIPPY_REPORT"

cargo clippy --locked --workspace --all-targets --all-features \
  --message-format=json -- -D warnings >"$CLIPPY_TEMPORARY"
mv "$CLIPPY_TEMPORARY" "$CLIPPY_REPORT"

# The actual native store is exercised by ordinary process/database tests.
# Keep obsolete endpoint variables absent for deterministic negative inputs.
unset CARTOGRAPH_DATABASE_URL CARTOGRAPH_DATABASE_SCHEMA CARTOGRAPH_TEST_DATABASE_URL

export CARGO_TARGET_DIR="$COVERAGE_BUILD_DIRECTORY"
eval "$(cargo llvm-cov show-env --sh)"
cargo llvm-cov clean --workspace
cargo test --locked --quiet --workspace --all-features

coverage_test -p cartograph-db --test rocks_graph \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-indexer --test live_supervisor \
  -- --ignored --nocapture --test-threads=1

coverage_test -p cartograph-cli --test rocks_owner \
  -- --ignored --nocapture --test-threads=1

cargo llvm-cov report --locked --lcov \
  --output-path "$COVERAGE_REPORT"

sonar scan \
  -Dsonar.rust.clippyReport.reportPaths="$CLIPPY_REPORT" \
  -Dsonar.rust.lcov.reportPaths="$COVERAGE_REPORT" \
  -Dsonar.qualitygate.wait=true \
  -Dsonar.qualitygate.timeout="${SONAR_QUALITYGATE_TIMEOUT:-300}" \
  "$@"
