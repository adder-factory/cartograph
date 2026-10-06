#!/usr/bin/env bash
set -euo pipefail

source ./scripts/lib/log.sh
. ./scripts/lib/fs.bash
source "$(dirname "$0")/lib/dynamic.sh"
. "./scripts/lib/log.sh"

export DEPLOY_ENV=staging
export PATH=/usr/local/bin:$PATH
readonly RELEASE_DIR=./build/release
typeset COUNT=0
BUILD_ID=$(date +%s)

build() {
  log_info "building $BUILD_ID"
  make all
}

package() {
  copy_tree ./build "$RELEASE_DIR"
  tar -czf release.tgz -C "$RELEASE_DIR" .
}

deploy() {
  local target="$1"
  case "$target" in
    staging) log_info "to staging" ;;
    prod) log_error "prod requires approval"; return 1 ;;
    *) log_debug "unknown $target" ;;
  esac
}

cat <<EOT
Deploying to $DEPLOY_ENV
EOT

main() {
  build
  package
  deploy "$DEPLOY_ENV" | tee deploy.log
  COUNT=$((COUNT + 1))
}

main "$@"
