source ./lib/aliases.zsh

readonly GIT_EXTRAS_DIR=${0:A:h}
export GIT_PAGER=less

git_current_branch() {
  git rev-parse --abbrev-ref HEAD 2>/dev/null
}

function git_sync {
  local branch=$(git_current_branch)
  git fetch --all
  git rebase "origin/$branch"
  mkcd ./tmp-sync
}

autoload -Uz compinit
compinit
