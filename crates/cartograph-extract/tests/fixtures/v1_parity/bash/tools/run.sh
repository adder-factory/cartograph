#!/bin/sh
. ./scripts/lib/log.sh

usage() {
  echo "usage: run.sh <cmd>"
}

if [ $# -eq 0 ]; then
  usage
  exit 1
fi
log_info "running $1"
