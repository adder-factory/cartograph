#!/usr/bin/env bash
# Capture the Cartograph v1.1.33 extraction oracle for one fixture corpus.
#
# Usage: capture-v1-parity-oracle.sh <v1-cartograph-binary> <corpus-dir> <language> <out.json>
#
# Maintainer-only. The v2 product, its tests and CI never run this script or the
# v1 binary; crates/cartograph-extract/tests/v1_parity_oracle.rs only reads the
# JSON documents it writes.
#
# Getting a verified v1 binary: download cartograph-darwin-arm64.tar.gz and
# SHA256SUMS from the v1.1.33 release, then check the archive before unpacking it:
#   grep ' cartograph-darwin-arm64.tar.gz$' SHA256SUMS | shasum -a 256 -c -
# The expected archive digest is
# 276b5fb5262f60ee7e59bfe08eda1f72e03ae1223e073efb88120cfa84116377. The binary is
# cartograph-darwin-arm64/bin/cartograph. Each oracle records the binary's own
# SHA-256 as binary_sha256, and the test pins it, so a capture made with any other
# build fails the coverage test.
#
# Regenerating every language, from the repository root:
#   fixtures=crates/cartograph-extract/tests/fixtures/v1_parity
#   for corpus in "$fixtures"/*/; do
#     language="$(basename "$corpus")"
#     [[ "$language" == expected ]] && continue
#     scripts/capture-v1-parity-oracle.sh "$v1_binary" "$corpus" "$language" \
#       "$fixtures/expected/$language.json"
#   done
# Then run `cargo test --locked -p cartograph-extract --test v1_parity_oracle` and
# update v1_parity/{alignments,divergences,reasons}.jsonl for changed dispositions.
#
# The corpus is copied to a private temporary checkout, indexed by the v1.1.33
# binary with an isolated HOME (local SQLite storage, no models), and the
# resulting graph is projected to a sorted, path-relative JSON document:
# symbols, resolved edges, and unresolved references. The `files` list omits
# dotfiles; the oracle test walks the corpus itself, so dotfile corpora such as
# zsh's are still extracted and checked.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <v1-cartograph-binary> <corpus-dir> <language> <out.json>" >&2
  exit 2
fi
binary="$1"
corpus="$2"
language="$3"
out="$4"

for tool in sqlite3 jq shasum; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }
done
[[ -x "$binary" ]] || { echo "v1 binary is not executable: $binary" >&2; exit 2; }
[[ -d "$corpus" ]] || { echo "corpus directory is missing: $corpus" >&2; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/home" "$work/checkout"
cp -R "$corpus/." "$work/checkout/"
git -C "$work/checkout" init -q
git -C "$work/checkout" add -A
git -C "$work/checkout" -c user.name=oracle -c user.email=oracle@invalid commit -qm corpus

(
  cd "$work/checkout"
  HOME="$work/home" XDG_CONFIG_HOME="$work/home/.config" \
    CARTOGRAPH_NO_UPDATE_CHECK=1 timeout 300 "$binary" index . >"$work/index.log" 2>&1
) || { echo "v1 index failed; log follows" >&2; tail -40 "$work/index.log" >&2; exit 1; }

db="$work/checkout/.cartograph/cartograph.db"
[[ -f "$db" ]] || { echo "v1 produced no database" >&2; exit 1; }

query() { sqlite3 -readonly -json "$db" "$1"; }

symbols="$(query "SELECT file_path AS file, kind, name, qualified_name, start_line, end_line
  FROM nodes ORDER BY file_path, start_line, start_column, kind, name")"
edges="$(query "SELECT s.file_path AS file, e.kind, s.kind AS source_kind, s.qualified_name AS source,
    t.kind AS target_kind, t.qualified_name AS target, t.file_path AS target_file, e.line
  FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
  ORDER BY s.file_path, e.line, e.kind, s.qualified_name, t.qualified_name")"
unresolved="$(query "SELECT file_path AS file, reference_kind AS kind, reference_name AS name, line
  FROM unresolved_refs ORDER BY file_path, line, reference_kind, reference_name")"
files="$(cd "$corpus" && find . -type f ! -name '.*' | sed 's#^\./##' | LC_ALL=C sort | jq -R . | jq -s .)"
sha="$(shasum -a 256 "$binary" | cut -d' ' -f1)"

jq -n \
  --arg language "$language" --arg sha "$sha" \
  --argjson files "$files" \
  --argjson symbols "${symbols:-[]}" --argjson edges "${edges:-[]}" \
  --argjson unresolved "${unresolved:-[]}" \
  '{baseline: "v1.1.33", binary_sha256: $sha, language: $language, files: $files,
    symbols: $symbols, edges: $edges, unresolved: $unresolved}' >"$out"
echo "captured $(jq '.symbols | length' "$out") symbols, $(jq '.edges | length' "$out") edges, $(jq '.unresolved | length' "$out") unresolved -> $out"
