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

require_live_database() {
  if [[ -z "${CARTOGRAPH_TEST_DATABASE_URL:-}" ]]; then
    echo 'CARTOGRAPH_TEST_DATABASE_URL is required for complete Sonar coverage.' >&2
    echo 'Point it at PostgreSQL 18 with the pinned pg_search and pgvector extensions.' >&2
    exit 1
  fi
}

cd "$ROOT"
mkdir -p "$CLIPPY_DIRECTORY" "$COVERAGE_DIRECTORY"
rm -f "$CLIPPY_REPORT"

cargo clippy --locked --workspace --all-targets --all-features \
  --message-format=json -- -D warnings >"$CLIPPY_TEMPORARY"
mv "$CLIPPY_TEMPORARY" "$CLIPPY_REPORT"

require_live_database

# Live tests consume only the dedicated test boundary. Runtime database
# configuration would invalidate negative tests that prove missing production
# credentials remain actionable.
unset CARTOGRAPH_DATABASE_URL CARTOGRAPH_DATABASE_SCHEMA

# Fail before the instrumented suite if the dedicated database is unreachable
# or missing any hard PostgreSQL/ParadeDB/pgvector capability.
cargo test --locked --quiet -p cartograph-db --test live_capabilities \
  -- --ignored

# Start with every ordinary workspace test, then merge the ignored live suites
# that exercise the production database, agent, MCP, and managed lifecycle
# paths. Each invocation reuses the instrumented build while retaining its own
# raw profile; the final report merges all profiles into one current LCOV file.
export CARGO_TARGET_DIR="$COVERAGE_BUILD_DIRECTORY"
eval "$(cargo llvm-cov show-env --sh)"
cargo llvm-cov clean --workspace
cargo test --locked --quiet --workspace --all-features

coverage_test -p cartograph-db --test live_capabilities \
  -- --ignored --nocapture
coverage_test -p cartograph-db --test live_generation \
  -- --ignored --nocapture
coverage_test -p cartograph-db --test live_storage_lifecycle \
  -- --ignored --nocapture
coverage_test -p cartograph-db --test live_ingest \
  -- --ignored --nocapture
coverage_test -p cartograph-db --test live_leases \
  -- --ignored --nocapture
coverage_test -p cartograph-db --test live_semantic \
  -- --include-ignored --nocapture --test-threads=1
coverage_test -p cartograph-db --test live_v1_import_retention \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-db --test live_project_purge \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-indexer --test live_supervisor \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-agent --test live_project \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-agent --lib navigation::tests::live_navigation \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-agent --lib decision_rerank::tests::live_context_ranking \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-agent --lib decision_rename::tests::live_rename \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-cli \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-agent --test patch_task_evaluation \
  -- --include-ignored --nocapture --test-threads=1
coverage_test -p cartograph-db --lib managed::tests:: \
  -- --ignored --nocapture --test-threads=1
coverage_test -p cartograph-db --lib managed::maintenance::tests:: \
  -- --ignored --nocapture --test-threads=1

cargo llvm-cov report --locked --lcov \
  --output-path "$COVERAGE_REPORT"

sonar scan \
  -Dsonar.rust.clippyReport.reportPaths="$CLIPPY_REPORT" \
  -Dsonar.rust.lcov.reportPaths="$COVERAGE_REPORT" \
  -Dsonar.qualitygate.wait=true \
  -Dsonar.qualitygate.timeout="${SONAR_QUALITYGATE_TIMEOUT:-300}" \
  "$@"
