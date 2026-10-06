#!/usr/bin/env bash
# Logging helpers.

readonly LOG_PREFIX="[deploy]"
LOG_LEVEL=info
declare -a LOG_TARGETS=(stdout)

log_info() {
  echo "$LOG_PREFIX info: $*"
}

function log_error {
  echo "$LOG_PREFIX error: $*" >&2
}

function log_debug() {
  local msg="$1"
  if [[ "$LOG_LEVEL" == "debug" ]]; then
    printf '%s debug: %s\n' "$LOG_PREFIX" "$msg"
  fi
}
