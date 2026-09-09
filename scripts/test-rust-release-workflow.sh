#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VALIDATION="$ROOT/.github/workflows/v2-rust.yml"
RELEASE="$ROOT/.github/workflows/release.yml"
STABLE_CANARY="$ROOT/.github/workflows/stable-canary.yml"
CHANGE_CLASSIFIER="$ROOT/scripts/classify-v2-ci-change.sh"
WORKSPACE_DEPENDENCY_CONTRACT="$ROOT/scripts/test-workspace-dependencies.sh"
DOCUMENTATION_CONTRACT="$ROOT/crates/cartograph-cli/src/tests/documentation_contract.rs"
PULL_HELPER="$ROOT/scripts/pull-pinned-image.sh"
SMOKE_HELPER="$ROOT/scripts/smoke-rust-release.sh"
UNIX_BUILD_HELPER="$ROOT/scripts/build-rust-release.sh"
LINUX_BUILD_HELPER="$ROOT/scripts/build-linux-rust-release.sh"
UNIX_INSTALLER="$ROOT/install.sh"
WINDOWS_INSTALLER="$ROOT/install.ps1"
UPGRADE_SOURCE="$ROOT/crates/cartograph-cli/src/upgrade.rs"
CLI_SOURCE="$ROOT/crates/cartograph-cli/src/main.rs"
DENY_CONFIG="$ROOT/deny.toml"
RUST_TOOLCHAIN_CONFIG="$ROOT/rust-toolchain.toml"
SONAR_CONFIG="$ROOT/sonar-project.properties"

fail() {
  echo "release workflow contract failed: $1" >&2
  exit 1
}

job_block() {
  local workflow="$1"
  local job="$2"
  awk -v header="  $job:" '
    $0 == header { capture = 1; next }
    capture && /^  [[:alnum:]_-]+:$/ { exit }
    capture { print }
  ' "$workflow"
}

literal_count() {
  local needle="$1"
  awk -v needle="$needle" 'index($0, needle) { count += 1 } END { print count + 0 }' \
    "$VALIDATION" "$RELEASE"
}

command -v ruby >/dev/null 2>&1 || fail 'Ruby with the standard YAML parser is required'
rust_toolchain="$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' "$RUST_TOOLCHAIN_CONFIG")"
[[ "$rust_toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || \
  fail 'the pinned Rust toolchain is not an exact stable release'
grep -Fq "rust-version = \"$rust_toolchain\"" "$ROOT/Cargo.toml" || \
  fail 'the workspace minimum Rust version does not match the pinned toolchain'
workspace_version="$(
  awk '
    $0 == "[workspace.package]" { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && /^version = "[^"]+"$/ {
      version = $0
      sub(/^version = "/, "", version)
      sub(/"$/, "", version)
      print version
      exit
    }
  ' "$ROOT/Cargo.toml"
)"
[[ -n "$workspace_version" ]] || fail 'the workspace package version is missing'
[[ "$(grep -c '^sonar\.projectVersion=' "$SONAR_CONFIG")" == 1 ]] || \
  fail 'Sonar must declare exactly one project version'
sonar_project_version="$(sed -n 's/^sonar\.projectVersion=//p' "$SONAR_CONFIG")"
[[ "$sonar_project_version" == "$workspace_version" ]] || \
  fail 'the Sonar project version does not match the Cargo workspace version'
ruby - "$VALIDATION" <<'RUBY'
require "yaml"

def contract_failure(message)
  abort("release workflow contract failed: #{message}")
end

def parse_workflow(source, label)
  document = YAML.safe_load(
    source,
    permitted_classes: [],
    permitted_symbols: [],
    aliases: false
  )
  contract_failure("#{label} did not parse as a YAML mapping") unless document.is_a?(Hash)
  document
rescue Psych::Exception => error
  contract_failure("#{label} is invalid YAML: #{error.message}")
end

def quality_job(document, label)
  jobs = document["jobs"]
  contract_failure("#{label} has no jobs mapping") unless jobs.is_a?(Hash)
  quality = jobs["quality"]
  contract_failure("#{label} has no quality job mapping") unless quality.is_a?(Hash)
  steps = quality["steps"]
  contract_failure("#{label} quality job has no steps sequence") unless steps.is_a?(Array)
  [quality, steps]
end

def nonblocking_quality?(source, label)
  quality, steps = quality_job(parse_workflow(source, label), label)
  quality.key?("continue-on-error") || steps.any? do |step|
    step.is_a?(Hash) && step.key?("continue-on-error")
  end
end

nonblocking_fixtures = {
  "quoted job-level continue-on-error fixture" => <<~'YAML',
    jobs:
      quality:
        "continue-on-error": true
        steps:
          - run: scripts/gate.sh
  YAML
  "whitespace-before-colon fixture" => <<~'YAML',
    jobs:
      quality:
        continue-on-error : true
        steps:
          - run: scripts/gate.sh
  YAML
  "explicit-key fixture" => <<~'YAML',
    jobs:
      quality:
        ? continue-on-error
        : true
        steps:
          - run: scripts/gate.sh
  YAML
  "escaped-key fixture" => <<~'YAML',
    jobs:
      quality:
        "continue-on-\u0065rror": true
        steps:
          - run: scripts/gate.sh
  YAML
  "quoted step-level continue-on-error fixture" => <<~'YAML',
    jobs:
      quality:
        steps:
          - run: scripts/gate.sh
            'continue-on-error': true
  YAML
}
nonblocking_fixtures.each do |label, fixture|
  contract_failure("#{label} bypassed parsed workflow validation") unless \
    nonblocking_quality?(fixture, label)
end

blocking_fixture = <<~'YAML'
  jobs:
    quality:
      steps:
        - run: scripts/gate.sh
YAML
contract_failure("blocking workflow fixture was incorrectly rejected") if \
  nonblocking_quality?(blocking_fixture, "blocking fixture")

validation_path = ARGV.fetch(0)
if nonblocking_quality?(File.read(validation_path), "validation workflow")
  contract_failure("quality job and its required steps may not continue on error")
end
RUBY

grep -Fq 'attest-main-gate:' "$VALIDATION" || fail 'main-gate attestation job is missing'
grep -Fq 'actions/attest-build-provenance@' "$VALIDATION" || fail 'main-gate provenance is missing'
# GitHub evaluates this expression; the local contract test needs the literal bytes.
# shellcheck disable=SC2016
grep -Fq 'v2-main-gate-${{ github.sha }}' "$VALIDATION" || fail 'main-gate artifact is not SHA-bound'
grep -Eq '^  pull_request:$' "$VALIDATION" || fail 'pull-request validation trigger is missing'
if grep -Eq '^    paths(-ignore)?:' "$VALIDATION"; then
  fail 'required validation workflow must classify jobs instead of path-filtering the workflow'
fi
push_branches="$(awk '
  /^  push:$/ { in_push = 1; next }
  in_push && /^    branches:$/ { in_branches = 1; next }
  in_push && in_branches && /^    [[:alnum:]_-]+:$/ { exit }
  in_push && /^  [[:alnum:]_-]+:$/ { exit }
  in_push && in_branches { print }
' "$VALIDATION" | sed '/^[[:space:]]*$/d')"
[[ "$push_branches" == '      - main' ]] || fail 'push validation must be scoped only to main'
grep -Fq 'classify:' "$VALIDATION" || fail 'validation scope classifier job is missing'
grep -Fq 'name: Classify validation scope' "$VALIDATION" || \
  fail 'validation scope classifier name changed'
grep -Fq 'fetch-depth: 0' "$VALIDATION" || \
  fail 'change classifier cannot inspect the complete event diff'
grep -Fq "git diff --no-renames --name-only -z \"\$diff_range\"" "$VALIDATION" || \
  fail 'change classifier is not NUL-safe or does not expose deletions separately'
[[ -x "$CHANGE_CLASSIFIER" ]] || fail 'change classifier is missing or not executable'
[[ -x "$WORKSPACE_DEPENDENCY_CONTRACT" ]] || \
  fail 'workspace dependency contract is missing or not executable'
[[ -f "$DOCUMENTATION_CONTRACT" ]] || fail 'documentation contract source is missing'
for documentation_test in \
  public_cli_reference_tracks_commands_targets_and_affected_forms \
  public_mcp_inventory_and_bundled_skill_track_current_contracts \
  release_docs_identify_current_audit_and_historical_benchmarks; do
  grep -Fq "fn $documentation_test()" "$DOCUMENTATION_CONTRACT" || \
    fail "documentation contract test is missing: $documentation_test"
done

job_ids="$(awk '
  $0 == "jobs:" { in_jobs = 1; next }
  in_jobs && /^  [[:alnum:]_-]+:$/ {
    job = $0
    sub(/^  /, "", job)
    sub(/:$/, "", job)
    print job
  }
' "$VALIDATION")"
expected_job_ids=$'classify\nquality\nwindows-portability\nmacos-portability\nlinux-release-portability\nparadedb\nattest-main-gate'
[[ "$job_ids" == "$expected_job_ids" ]] || \
  fail 'validation jobs changed without an explicit documentation-scope contract'

classify_paths() {
  if (( $# > 0 )); then
    printf '%s\0' "$@"
  fi | "$CHANGE_CLASSIFIER"
}

assert_scope() {
  local expected="$1"
  shift
  local actual
  actual="$(classify_paths "$@")"
  [[ "$actual" == "$expected" ]] || \
    fail "unexpected validation scope for paths: $*"
}

docs_scope=$'full=false\ndocs_only=true'
full_scope=$'full=true\ndocs_only=false'
assert_scope "$docs_scope" README.md ACKNOWLEDGEMENTS.md docs/CONFIGURATION.md
assert_scope "$docs_scope" docs/v2/ARCHITECTURE.md
assert_scope "$docs_scope" crates/example/tests/fixtures/corpus/README.md
assert_scope "$full_scope"
assert_scope "$full_scope" 'docs/path with spaces/example.rs'
assert_scope "$full_scope" docs/releases/v9.9.9.md
assert_scope "$full_scope" crates/cartograph-cli/src/main.rs
assert_scope "$full_scope" crates/cartograph-cli/src/tests/documentation_contract.rs
assert_scope "$full_scope" crates/cartograph-cli/assets/cartograph-skill.md
assert_scope "$full_scope" .github/workflows/v2-rust.yml
assert_scope "$full_scope" docs/CONFIGURATION.md Cargo.lock

quality_block="$(job_block "$VALIDATION" quality)"
workspace_contract_count="$(awk '
  $0 == "      - name: Workspace dependency contract" { count += 1 }
  END { print count + 0 }
' <<<"$quality_block")"
[[ "$workspace_contract_count" == 1 ]] || \
  fail 'quality job must contain exactly one workspace dependency contract step'
workspace_contract_step="$(awk \
  -v header='      - name: Workspace dependency contract' '
  $0 == header {
    capture = 1
  }
  capture && $0 != header && /^      - / {
    exit
  }
  capture {
    print
  }
' <<<"$quality_block" | sed '/^[[:space:]]*$/d')"
expected_workspace_contract_step=$'      - name: Workspace dependency contract\n        if: needs.classify.outputs.full == \'true\'\n        run: scripts/test-workspace-dependencies.sh'
[[ "$workspace_contract_step" == "$expected_workspace_contract_step" ]] || \
  fail 'workspace dependency contract step must be exact, full-gate-only, and non-optional'
workspace_contract_run_count="$(awk '
  $0 == "        run: scripts/test-workspace-dependencies.sh" { count += 1 }
  END { print count + 0 }
' "$VALIDATION")"
[[ "$workspace_contract_run_count" == 1 ]] || \
  fail 'workspace dependency contract command must appear exactly once'
grep -Fq 'needs: classify' <<<"$quality_block" || \
  fail 'required quality result does not depend on change classification'
if grep -Eq '^    (container|services):' <<<"$quality_block"; then
  fail 'documentation-only quality job cannot allocate a container or service'
fi
grep -Fq 'if: always()' <<<"$quality_block" || \
  fail 'required quality result can disappear when classification fails'
grep -Fq 'name: Documentation contract' <<<"$quality_block" || \
  fail 'documentation-only validation is missing'
grep -Fq 'tests::documentation_contract::' <<<"$quality_block" || \
  fail 'documentation-only validation does not run the isolated contract tests'

validate_quality_step() {
  local step="$1"
  [[ -n "$step" ]] || return 0
  if grep -Eq \
      'name: (Require successful change classification|Resolve pinned Rust toolchain|Restore Rust dependency cache|Fetch locked dependencies|Release workflow contract)' \
      <<<"$step" || grep -Fq -- '- uses: actions/checkout@' <<<"$step"; then
    return 0
  fi
  if grep -Fq 'name: Documentation contract' <<<"$step"; then
    grep -Fxq "        if: needs.classify.outputs.docs_only == 'true'" <<<"$step" || \
      fail 'documentation contract lost its docs-only condition'
  elif grep -Fq 'name: Save Rust dependency cache' <<<"$step"; then
    grep -Fxq "          needs.classify.outputs.full == 'true' &&" <<<"$step" || \
      fail 'quality cache save lost its leading full-gate condition'
  elif ! grep -Fxq "        if: needs.classify.outputs.full == 'true'" <<<"$step"; then
    fail "quality step is not explicitly scoped: $(head -n 1 <<<"$step")"
  fi
}

in_quality_steps=false
quality_step=''
while IFS= read -r line; do
  if [[ "$line" == '    steps:' ]]; then
    in_quality_steps=true
    continue
  fi
  [[ "$in_quality_steps" == true ]] || continue
  if [[ "$line" == '      - '* ]]; then
    validate_quality_step "$quality_step"
    quality_step="$line"$'\n'
  else
    quality_step+="$line"$'\n'
  fi
done <<<"$quality_block"
validate_quality_step "$quality_step"

for parallel_job in windows-portability macos-portability linux-release-portability paradedb; do
  if job_block "$VALIDATION" "$parallel_job" | grep -Fq 'needs: quality'; then
    fail "independent gate $parallel_job is serialized behind quality"
  fi
done
for skipped_job in windows-portability macos-portability linux-release-portability; do
  skipped_block="$(job_block "$VALIDATION" "$skipped_job")"
  grep -Fq 'needs: classify' <<<"$skipped_block" || \
    fail "documentation-only scope cannot control $skipped_job"
  grep -Fq "if: needs.classify.outputs.full == 'true'" <<<"$skipped_block" || \
    fail "documentation-only scope does not skip $skipped_job"
done
paradedb_block="$(job_block "$VALIDATION" paradedb)"
grep -Fq 'needs: classify' <<<"$paradedb_block" || \
  fail 'live PostgreSQL matrix does not depend on change classification'
if grep -Eq '^    (container|services):' <<<"$paradedb_block"; then
  fail 'documentation-only PostgreSQL contexts cannot allocate a container or service'
fi
grep -Fq 'name: Documentation-only required-check fast path' <<<"$paradedb_block" || \
  fail 'live PostgreSQL required contexts disappear for documentation-only changes'
grep -Fq "if: needs.classify.outputs.docs_only == 'true'" <<<"$paradedb_block" || \
  fail 'live PostgreSQL documentation fast path is not scope-gated'

validate_paradedb_step() {
  local step="$1"
  [[ -n "$step" ]] || return 0
  if grep -Fq 'name: Documentation-only required-check fast path' <<<"$step"; then
    grep -Fq "needs.classify.outputs.docs_only == 'true'" <<<"$step" || \
      fail 'live PostgreSQL documentation fast path lost its docs-only condition'
  elif ! grep -Fq "needs.classify.outputs.full == 'true'" <<<"$step"; then
    fail "live PostgreSQL step is not full-gate scoped: $(head -n 1 <<<"$step")"
  fi
}

in_paradedb_steps=false
paradedb_step=''
while IFS= read -r line; do
  if [[ "$line" == '    steps:' ]]; then
    in_paradedb_steps=true
    continue
  fi
  [[ "$in_paradedb_steps" == true ]] || continue
  if [[ "$line" == '      - '* ]]; then
    validate_paradedb_step "$paradedb_step"
    paradedb_step="$line"$'\n'
  else
    paradedb_step+="$line"$'\n'
  fi
done <<<"$paradedb_block"
validate_paradedb_step "$paradedb_step"
for database_policy in \
  '--memory 2147483648' \
  '--memory-reservation 1073741824' \
  '--cpus 4' \
  '--pids-limit 256' \
  '-c checkpoint_timeout=15min' \
  '-c max_wal_size=4GB' \
  '-c min_wal_size=512MB' \
  '-c max_connections=96' \
  '-c max_parallel_workers=4'; do
  grep -Fq -- "$database_policy" <<<"$paradedb_block" || \
    fail "live PostgreSQL matrix does not prove managed database policy: $database_policy"
done

attestation_block="$(job_block "$VALIDATION" attest-main-gate)"
grep -Fq "needs.classify.outputs.full == 'true'" <<<"$attestation_block" || \
  fail 'documentation-only validation can create release gate attestation'
grep -Fq 'cargo doc --locked --workspace --all-features --no-deps' "$VALIDATION" || \
  fail 'strict all-feature rustdoc validation is missing'
grep -Fq 'cargo test --locked --workspace --all-features' "$VALIDATION" || \
  fail 'unit tests do not exercise all features'
grep -Fq '.evidence.packet.abstention == null' "$VALIDATION" || \
  fail 'candidate self-index context does not reject an abstaining packet'
grep -Fq '(.evidence.packet.evidence | type == "array")' "$VALIDATION" || \
  fail 'candidate self-index context does not require the packet evidence array'
grep -Fq '(.evidence.packet.evidence | length) > 0' "$VALIDATION" || \
  fail 'candidate self-index context does not require real retrieval evidence'
if grep -Fq '(.evidence | length) > 0' "$VALIDATION"; then
  fail 'candidate self-index context still accepts a merely nonempty response envelope'
fi
self_context_filter='.
  | .freshness == "current"
  and .evidence.packet.abstention == null
  and (.evidence.packet.evidence | type == "array")
  and (.evidence.packet.evidence | length) > 0'
if jq -e "$self_context_filter" <<<'{"freshness":"current","evidence":{"packet":{"abstention":null,"evidence":[]}}}' \
  >/dev/null; then
  fail 'candidate self-index context accepts an empty evidence packet'
fi
if jq -e "$self_context_filter" <<<'{"freshness":"current","evidence":{"packet":{"abstention":"no-match","evidence":[{}]}}}' \
  >/dev/null; then
  fail 'candidate self-index context accepts an abstaining packet'
fi
jq -e "$self_context_filter" \
  <<<'{"freshness":"current","evidence":{"packet":{"abstention":null,"evidence":[{}]}}}' \
  >/dev/null || fail 'candidate self-index context rejects real current evidence'
grep -Fq 'macos-portability:' "$VALIDATION" || fail 'macOS strict portability gate is missing'
grep -Fq 'cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings' \
  "$STABLE_CANARY" || fail 'latest-stable strict compatibility canary is missing'
grep -Fq 'Verify the reviewed production pin tracks latest stable' "$STABLE_CANARY" || \
  fail 'latest-stable canary does not detect a stale production toolchain pin'
canary_clippy_line="$(grep -nF 'name: All-target, all-feature Clippy on latest stable' \
  "$STABLE_CANARY" | cut -d: -f1)"
canary_tests_line="$(grep -nF 'name: All-feature tests on latest stable' \
  "$STABLE_CANARY" | cut -d: -f1)"
canary_rustdoc_line="$(grep -nF 'name: Strict all-feature rustdoc on latest stable' \
  "$STABLE_CANARY" | cut -d: -f1)"
canary_pin_line="$(grep -nF 'name: Verify the reviewed production pin tracks latest stable' \
  "$STABLE_CANARY" | cut -d: -f1)"
[[ "$canary_pin_line" -gt "$canary_clippy_line" && \
  "$canary_pin_line" -gt "$canary_tests_line" && \
  "$canary_pin_line" -gt "$canary_rustdoc_line" ]] || \
  fail 'stale-pin reporting can skip latest-stable compatibility evidence'
if grep -Fq 'continue-on-error:' "$STABLE_CANARY"; then
  fail 'latest-stable compatibility or stale-pin reporting was made non-blocking'
fi
if grep -Eiq 'nightly|miri|sanitizer|cargo[- ]fuzz' "$STABLE_CANARY"; then
  fail 'production compatibility canary must use stable Rust only'
fi
grep -Fq 'strategy:' "$VALIDATION" || fail 'live PostgreSQL shards are missing'
for shard in database runtime operations; do
  grep -Fq -- "- $shard" "$VALIDATION" || fail "live shard $shard is missing"
done

if grep -Fq 'uses: ./.github/workflows/v2-rust.yml' "$RELEASE"; then
  fail 'tag workflow still reruns the full validation workflow'
fi
grep -Fq "tags: ['v*']" "$RELEASE" || fail 'release workflow tag trigger is missing'
if grep -Eq '^[[:space:]]{2}workflow_dispatch:' "$RELEASE"; then
  fail 'release workflow manual dispatch duplicates the tag-specific native builds'
fi
grep -Fq 'verify-main-gate:' "$RELEASE" || fail 'release main-gate verification job is missing'
grep -Fq 'gh attestation verify' "$RELEASE" || fail 'release does not verify main-gate provenance'
grep -Fq -- '--source-ref refs/heads/main' "$RELEASE" || fail 'release does not bind evidence to main'
grep -Fq "NOTES=\"docs/releases/\$TAG.md\"" "$RELEASE" || \
  fail 'release notes are not loaded from the tracked versioned file'
grep -Fq -- "--notes-file \"\$NOTES\"" "$RELEASE" || \
  fail 'tracked release notes are not supplied to GitHub'
release_note_marker_pattern='(^|[[:space:][:punct:]])(TBD|TODO|PLACEHOLDER)($|[[:space:][:punct:]])'
grep -Fq "grep -Eiq '$release_note_marker_pattern' \"\$NOTES\"" "$RELEASE" || \
  fail 'release-note marker validation is not token-aware'
if grep -Eiq "$release_note_marker_pattern" \
  <<<'Grammar-recovery placeholders can no longer create invalid durable spans.'; then
  fail 'release-note marker validation rejects legitimate inflected prose'
fi
for marker in TBD TODO PLACEHOLDER; do
  grep -Eiq "$release_note_marker_pattern" <<<"Release note $marker: replace before publishing." || \
    fail "release-note marker validation misses $marker"
done
if grep -Fq -- '--generate-notes' "$RELEASE"; then
  fail 'release publication can still replace detailed notes with generated changelog text'
fi
grep -Fq 'needs: verify-main-gate' "$RELEASE" || fail 'release builds do not depend on exact main evidence'
grep -Fq 'cache_key: release-windows-x64' "$RELEASE" || \
  fail 'Windows release artifacts do not use a release-profile cache'
if grep -Fq 'cache_key: v2-windows' "$RELEASE"; then
  fail 'Windows release build still shares the dev and Clippy cache'
fi
grep -Fq 'os: macos-26' "$RELEASE" || fail 'Apple Silicon release does not target macOS 26'
grep -Fq 'rust_target: aarch64-apple-darwin' "$RELEASE" || \
  fail 'Apple Silicon release target is missing'
if grep -Fq 'darwin-x64' "$RELEASE" "$UNIX_BUILD_HELPER" "$UPGRADE_SOURCE"; then
  fail 'Intel macOS remains in the native release contract'
fi
if grep -Fq 'x86_64-apple-darwin' "$RELEASE" "$UNIX_BUILD_HELPER" "$UPGRADE_SOURCE" "$DENY_CONFIG"; then
  fail 'Intel macOS remains in a compiled or audited target set'
fi
if grep -Eq '(i[3-6]86|armv7|thumbv7|windows-x86|linux-x86)' \
  "$RELEASE" "$UNIX_BUILD_HELPER" "$LINUX_BUILD_HELPER" "$DENY_CONFIG"; then
  fail 'a 32-bit native release target is configured'
fi
grep -Fq '#[cfg(not(target_pointer_width = "64"))]' "$CLI_SOURCE" || \
  fail 'the native CLI does not fail closed on 32-bit targets'
grep -Fq 'Cartograph v2 supports only 64-bit operating systems.' "$CLI_SOURCE" || \
  fail 'the 32-bit compile failure is not actionable'
grep -Fq "rust:1.98.0-trixie@sha256:" "$VALIDATION" || \
  fail 'Linux validation does not build on current stable Debian'
grep -Fq 'rustup toolchain install "$CARTOGRAPH_RELEASE_TOOLCHAIN" --profile minimal' "$LINUX_BUILD_HELPER" || \
  fail 'Linux builds do not install the exact reviewed compiler independently of the base image'
grep -Fq '"rustc $CARTOGRAPH_RELEASE_TOOLCHAIN "*' "$LINUX_BUILD_HELPER" || \
  fail 'Linux builds do not verify the exact compiler after bootstrap'
grep -Fq "rust-version: '$rust_toolchain'" "$VALIDATION" || \
  fail 'cargo-deny does not use the pinned stable Rust toolchain'
grep -Fq 'debian:13-slim@sha256:' "$VALIDATION" || \
  fail 'Linux validation does not smoke on current stable Debian'
grep -Fq "rust:1.98.0-trixie@sha256:" "$RELEASE" || \
  fail 'Linux releases do not build on current stable Debian'
grep -Fq 'debian:13-slim@sha256:' "$RELEASE" || \
  fail 'Linux releases do not smoke on current stable Debian'
if grep -Eq '(bookworm|debian:12|Debian 12|glibc 2\.36)' \
  "$VALIDATION" "$RELEASE" "$LINUX_BUILD_HELPER"; then
  fail 'the old Debian 12 compatibility floor remains configured'
fi
grep -Fq 'MACOSX_DEPLOYMENT_TARGET=26.0' "$UNIX_BUILD_HELPER" || \
  fail 'macOS release binaries do not require the current major version'
grep -Fq 'minos 26.0' "$UNIX_BUILD_HELPER" || \
  fail 'macOS release binaries do not verify their minimum OS contract'
grep -Fq "TARGET_ROOT=\"\${CARGO_TARGET_DIR:-target}\"" "$UNIX_BUILD_HELPER" || \
  fail 'release builds do not honor an isolated Cargo target directory'
grep -Fq -- '--env CARGO_TARGET_DIR=/tmp/cartograph-target' "$LINUX_BUILD_HELPER" || \
  fail 'Linux container builds can follow a host target symlink'
grep -Fq 'requires glibc 2.41 or newer' "$UNIX_INSTALLER" || \
  fail 'the Unix installer does not reject old Linux before download'
grep -Fq 'requires macOS 26 or newer' "$UNIX_INSTALLER" || \
  fail 'the Unix installer does not reject old macOS before download'
grep -Fq "{ 26100 } else { 26200 }" "$WINDOWS_INSTALLER" || \
  fail 'the Windows installer does not reject old Windows before download'
if grep -Fq 'Swatinem/rust-cache@' "$VALIDATION" "$RELEASE"; then
  fail 'Rust cache still bundles the deprecated core-punycode dependency path'
fi
CACHE_ACTION_SHA='55cc8345863c7cc4c66a329aec7e433d2d1c52a9'
restore_count="$(literal_count "actions/cache/restore@$CACHE_ACTION_SHA")"
[[ "$restore_count" == 5 ]] || fail 'every Rust cache restore must use pinned actions/cache v6.1.0'
save_count="$(literal_count "actions/cache/save@$CACHE_ACTION_SHA")"
[[ "$save_count" == 4 ]] || fail 'bounded main, Windows, macOS, and release cache saves are missing'
for cache_path in \
  '.cargo/registry/index/' \
  '.cargo/registry/cache/' \
  '.cargo/git/db/' \
  'target/'; do
  path_count="$(literal_count "$cache_path")"
  [[ "$path_count" == 9 ]] || fail "Rust cache path coverage changed for $cache_path"
done
grep -Fq 'steps.rust-cache.outputs.cache-primary-key' "$VALIDATION" || \
  fail 'main cache saves are not bound to their exact restored primary key'
grep -Fq 'steps.rust-cache.outputs.cache-primary-key' "$RELEASE" || \
  fail 'release cache saves are not bound to their exact restored primary key'
grep -Fq 'scripts/pull-pinned-image.sh' "$VALIDATION" || fail 'ParadeDB pulls do not use bounded retries'
grep -Fq 'PostgreSQL init process complete; ready for start up.' "$VALIDATION" || \
  fail 'ParadeDB readiness can accept the temporary bootstrap server'
grep -Fq -- "--command 'SELECT 1;'" "$VALIDATION" || \
  fail 'ParadeDB readiness does not prove a live SQL connection'
grep -Fq -- '-v2-ubuntu-' "$VALIDATION" || \
  fail 'live shards do not reuse the compiled Ubuntu workspace cache'

unpinned_actions="$({
  grep -hE '^[[:space:]]*(- )?uses:' "$VALIDATION" "$RELEASE" "$STABLE_CANARY"
} | grep -Ev 'uses: (\./|[^[:space:]]+@[0-9a-f]{40}([[:space:]]|$))' || true)"
if [[ -n "$unpinned_actions" ]]; then
  printf 'Unpinned release action references:\n%s\n' "$unpinned_actions" >&2
  fail 'every external action must use a full commit digest'
fi

unpinned_status=0
CARTOGRAPH_DOCKER_BIN=/usr/bin/false "$PULL_HELPER" example.invalid/cartograph:latest 1 \
  >/dev/null 2>&1 || unpinned_status=$?
[[ "$unpinned_status" -eq 2 ]] || fail 'unpinned image input was not rejected'

fixture="$(mktemp -d "${TMPDIR:-/tmp}/cartograph-pull-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

unsupported_release_status=0
"$UNIX_BUILD_HELPER" x86_64-apple-darwin darwin-x64 \
  >"$fixture/unsupported-release.out" 2>"$fixture/unsupported-release.err" || \
  unsupported_release_status=$?
[[ "$unsupported_release_status" -eq 2 ]] || fail 'Intel macOS release pairing was not rejected'

fake_uname_dir="$fixture/fake-uname"
mkdir -p "$fake_uname_dir"
cat >"$fake_uname_dir/uname" <<'EOF'
#!/usr/bin/env sh
case "${1:-}" in
  -s) printf '%s\n' "${CARTOGRAPH_FAKE_UNAME_S:?}" ;;
  -m) printf '%s\n' "${CARTOGRAPH_FAKE_UNAME_M:?}" ;;
  *) exit 2 ;;
esac
EOF
chmod 0755 "$fake_uname_dir/uname"
intel_install_status=0
PATH="$fake_uname_dir:$PATH" HOME="$fixture/home" \
  CARTOGRAPH_FAKE_UNAME_S=Darwin CARTOGRAPH_FAKE_UNAME_M=x86_64 "$UNIX_INSTALLER" \
  >"$fixture/intel-install.out" 2>"$fixture/intel-install.err" || intel_install_status=$?
[[ "$intel_install_status" -eq 1 ]] || fail 'Intel macOS installer request was not rejected'
grep -Fq 'Intel macOS is not supported' "$fixture/intel-install.err" || \
  fail 'Intel macOS installer failure is not actionable'

cat >"$fake_uname_dir/ldd" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' 'ldd (Debian GLIBC 2.36-9+deb12u13) 2.36'
EOF
cat >"$fake_uname_dir/sw_vers" <<'EOF'
#!/usr/bin/env sh
[ "${1:-}" = -productVersion ]
printf '%s\n' '15.7.6'
EOF
cat >"$fake_uname_dir/curl" <<'EOF'
#!/usr/bin/env sh
echo 'installer reached the network unexpectedly' >&2
exit 99
EOF
chmod 0755 "$fake_uname_dir/ldd" "$fake_uname_dir/sw_vers" "$fake_uname_dir/curl"
old_macos_install_status=0
PATH="$fake_uname_dir:$PATH" HOME="$fixture/home" \
  CARTOGRAPH_FAKE_UNAME_S=Darwin CARTOGRAPH_FAKE_UNAME_M=arm64 "$UNIX_INSTALLER" \
  >"$fixture/old-macos-install.out" 2>"$fixture/old-macos-install.err" || \
  old_macos_install_status=$?
[[ "$old_macos_install_status" -eq 1 ]] || fail 'old macOS installer request was not rejected'
grep -Fq 'requires macOS 26 or newer' "$fixture/old-macos-install.err" || \
  fail 'old macOS installer failure is not actionable or happened after network access'

old_linux_install_status=0
PATH="$fake_uname_dir:$PATH" HOME="$fixture/home" \
  CARTOGRAPH_FAKE_UNAME_S=Linux CARTOGRAPH_FAKE_UNAME_M=x86_64 "$UNIX_INSTALLER" \
  >"$fixture/old-linux-install.out" 2>"$fixture/old-linux-install.err" || \
  old_linux_install_status=$?
[[ "$old_linux_install_status" -eq 1 ]] || fail 'old Linux installer request was not rejected'
grep -Fq 'requires glibc 2.41 or newer' "$fixture/old-linux-install.err" || \
  fail 'old Linux installer failure is not actionable or happened after network access'

fake_docker="$fixture/docker"
cat >"$fake_docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state="${CARTOGRAPH_FAKE_DOCKER_STATE:?}"
case "${1:-}:${2:-}" in
  image:inspect)
    [[ -f "$state/available" ]]
    ;;
  pull:*)
    count=0
    [[ ! -f "$state/count" ]] || count="$(<"$state/count")"
    count=$((count + 1))
    printf '%s\n' "$count" >"$state/count"
    if (( count >= 3 )); then
      : >"$state/available"
      exit 0
    fi
    exit 1
    ;;
  *)
    exit 2
    ;;
esac
EOF
chmod 0755 "$fake_docker"
mkdir -p "$fixture/state"

IMAGE="example.invalid/cartograph@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
CARTOGRAPH_DOCKER_BIN="$fake_docker" \
  CARTOGRAPH_FAKE_DOCKER_STATE="$fixture/state" \
  CARTOGRAPH_IMAGE_PULL_RETRY_DELAY_SECONDS=0 \
  "$PULL_HELPER" "$IMAGE" 4 >/dev/null
[[ "$(<"$fixture/state/count")" == 3 ]] || fail 'bounded pull retry count changed'

space_binary_directory="$fixture/release path with spaces"
space_binary="$space_binary_directory/cartograph"
mkdir -p "$space_binary_directory"
cat >"$space_binary" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  --version)
    printf 'cartograph %s\n' "${CARTOGRAPH_SMOKE_FIXTURE_VERSION:?}"
    ;;
  --help)
    cat <<'HELP'
  index
  status
  find
  context
  graph
  affected
  review
  install
  uninstall
  serve
  doctor
  db
HELP
    ;;
  serve)
    echo 'serve requires --mcp' >&2
    exit 1
    ;;
  *)
    exit 2
    ;;
esac
EOF
chmod 0755 "$space_binary"
fixture_version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "cartograph-cli") | .version')"
CARTOGRAPH_SMOKE_FIXTURE_VERSION="$fixture_version" "$SMOKE_HELPER" "$space_binary" >/dev/null

echo 'Release workflow attestation, sharding, and pinned-image retry contracts passed.'
