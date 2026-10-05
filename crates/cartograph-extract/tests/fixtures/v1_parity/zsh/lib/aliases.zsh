# Aliases and small helpers.
alias ll='ls -lah'
alias gs='git status'

typeset -g ALIAS_VERSION=3

function mkcd() {
  mkdir -p "$1" && cd "$1"
}

up() {
  local n=${1:-1}
  repeat $n cd ..
}
