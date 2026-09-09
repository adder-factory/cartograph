#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${1:-}"
ASSET_TARGET="${2:-}"
BUILD_IMAGE="${3:-}"
RUNTIME_IMAGE="${4:-}"
BUILD_TOOLCHAIN="$(sed -n 's/^channel = "\([^" ]*\)"$/\1/p' "$ROOT/rust-toolchain.toml")"
if [[ ! "$BUILD_TOOLCHAIN" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "the release compiler must be an exact stable toolchain" >&2
  exit 2
fi

if [[ -z "$TARGET" || -z "$ASSET_TARGET" || -z "$BUILD_IMAGE" || -z "$RUNTIME_IMAGE" ]]; then
  echo "usage: $0 <rust-target-triple> <asset-target> <pinned-build-image> <pinned-runtime-image>" >&2
  exit 2
fi

case "$TARGET:$ASSET_TARGET" in
  x86_64-unknown-linux-gnu:linux-x64) EXPECTED_MACHINE="x86_64" ;;
  aarch64-unknown-linux-gnu:linux-arm64) EXPECTED_MACHINE="aarch64" ;;
  *)
    echo "unsupported Linux release target pairing: $TARGET / $ASSET_TARGET" >&2
    exit 2
    ;;
esac

if [[ ! "$BUILD_IMAGE" =~ ^rust:1\.98\.0-trixie@sha256:[0-9a-f]{64}$ ]]; then
  echo "Linux release build image must pin rust:1.98.0-trixie by digest" >&2
  exit 2
fi
if [[ ! "$RUNTIME_IMAGE" =~ ^debian:13-slim@sha256:[0-9a-f]{64}$ ]]; then
  echo "Linux release runtime image must pin debian:13-slim by digest" >&2
  exit 2
fi
if ! docker info >/dev/null 2>&1; then
  echo "Linux release builds require a local Docker daemon" >&2
  exit 1
fi

"$ROOT/scripts/pull-pinned-image.sh" "$BUILD_IMAGE"
"$ROOT/scripts/pull-pinned-image.sh" "$RUNTIME_IMAGE"

docker run --rm --pull never \
  --user "$(id -u):$(id -g)" \
  --volume "$ROOT:/workspace" \
  --workdir /workspace \
  --env HOME=/tmp/cartograph-home \
  --env CARGO_HOME=/tmp/cartograph-cargo \
  --env RUSTUP_HOME=/tmp/cartograph-rustup \
  --env CARTOGRAPH_RELEASE_TOOLCHAIN="$BUILD_TOOLCHAIN" \
  --env CARGO_TARGET_DIR=/tmp/cartograph-target \
  --env CARTOGRAPH_RELEASE_TARGET="$TARGET" \
  --env CARTOGRAPH_RELEASE_ASSET_TARGET="$ASSET_TARGET" \
  --env CARTOGRAPH_RELEASE_MACHINE="$EXPECTED_MACHINE" \
  "$BUILD_IMAGE" \
  bash -c '
    set -euo pipefail
    mkdir -p "$HOME" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$RUSTUP_HOME"
    machine="$(uname -m)"
    if [[ "$machine" != "$CARTOGRAPH_RELEASE_MACHINE" ]]; then
      echo "release container architecture mismatch: expected $CARTOGRAPH_RELEASE_MACHINE, got $machine" >&2
      exit 1
    fi
    # The published 1.98.0 image supplies Debian/build utilities. Install the
    # exact reviewed compiler independently; a lagging image must not select rustc.
    rustup toolchain install "$CARTOGRAPH_RELEASE_TOOLCHAIN" --profile minimal \
      --component clippy,rustfmt --no-self-update
    if [[ "$(rustc --version)" != "rustc $CARTOGRAPH_RELEASE_TOOLCHAIN "* ]]; then
      echo "release container did not provide the pinned Rust compiler" >&2
      rustc --version >&2
      exit 1
    fi
    scripts/build-rust-release.sh "$CARTOGRAPH_RELEASE_TARGET" "$CARTOGRAPH_RELEASE_ASSET_TARGET"
  '

VERSION="$(
  sed -n '/^\[workspace\.package\]$/,/^\[/ {
    s/^version = "\([^"]*\)"$/\1/p
  }' "$ROOT/Cargo.toml" | head -n 1
)"
if [[ -z "$VERSION" ]]; then
  echo "could not resolve the workspace release version" >&2
  exit 1
fi

docker run --rm --pull never \
  --volume "$ROOT/release:/release:ro" \
  --env CARTOGRAPH_EXPECTED_MACHINE="$EXPECTED_MACHINE" \
  --env CARTOGRAPH_EXPECTED_VERSION="cartograph $VERSION" \
  --env CARTOGRAPH_RUNTIME_BINARY="/release/cartograph-$ASSET_TARGET" \
  "$RUNTIME_IMAGE" \
  sh -c '
    set -eu
    machine="$(uname -m)"
    if [ "$machine" != "$CARTOGRAPH_EXPECTED_MACHINE" ]; then
      echo "runtime container architecture mismatch: expected $CARTOGRAPH_EXPECTED_MACHINE, got $machine" >&2
      exit 1
    fi
    glibc="$(ldd --version 2>&1 | head -n 1)"
    case "$glibc" in
      *" 2.41") ;;
      *)
        echo "runtime smoke did not execute on the Debian 13 glibc 2.41 baseline: $glibc" >&2
        exit 1
        ;;
    esac
    version="$($CARTOGRAPH_RUNTIME_BINARY --version)"
    if [ "$version" != "$CARTOGRAPH_EXPECTED_VERSION" ]; then
      echo "runtime smoke version mismatch: expected $CARTOGRAPH_EXPECTED_VERSION, got $version" >&2
      exit 1
    fi
  '

echo "[rust-release] Debian 13 / glibc 2.41 runtime smoke passed for $ASSET_TARGET"
