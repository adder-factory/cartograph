# Filesystem helpers.
source ./scripts/lib/log.sh

ensure_dir() {
  local dir="$1"
  if [ ! -d "$dir" ]; then
    mkdir -p "$dir"
    log_info "created $dir"
  fi
}

copy_tree() {
  ensure_dir "$2"
  cp -R "$1/." "$2/"
}
