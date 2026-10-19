#!/usr/bin/env bash

set -euo pipefail

BINARY="${1:-}"
if [[ -z "$BINARY" || ! -x "$BINARY" ]]; then
  echo "usage: $0 <cartograph-binary>" >&2
  exit 2
fi

VERSION="$("$BINARY" --version)"
EXPECTED_VERSION="cartograph $(cargo metadata --no-deps --format-version 1 | sed -n 's/.*\"name\":\"cartograph-cli\",\"version\":\"\([^\"]*\)\".*/\1/p' | head -n 1)"
if [[ "$VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "release binary version mismatch: expected '$EXPECTED_VERSION', got '$VERSION'" >&2
  exit 1
fi

HELP="$("$BINARY" --help)"
for COMMAND in index status find context graph affected review install uninstall serve watch service doctor db; do
  if ! grep -Eq "^[[:space:]]+$COMMAND([[:space:]]|$)" <<<"$HELP"; then
    echo "release binary help is missing '$COMMAND'" >&2
    exit 1
  fi
done

SMOKE_TEMP="$(mktemp -d "${TMPDIR:-/tmp}/cartograph-release-smoke.XXXXXX")"
SMOKE_STDOUT="$SMOKE_TEMP/stdout"
SMOKE_STDERR="$SMOKE_TEMP/stderr"
cleanup() {
  rm -f "$SMOKE_STDOUT" "$SMOKE_STDERR"
  rmdir "$SMOKE_TEMP"
}
trap cleanup EXIT

if "$BINARY" serve >"$SMOKE_STDOUT" 2>"$SMOKE_STDERR"; then
  echo "serve without --mcp unexpectedly succeeded" >&2
  exit 1
fi
if ! grep -Fq 'serve requires --mcp' "$SMOKE_STDERR"; then
  echo "serve failure did not preserve the stable transport remediation" >&2
  exit 1
fi

echo "[rust-release] native CLI smoke passed ($VERSION)"
