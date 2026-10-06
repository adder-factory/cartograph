# Interactive shell config.
source ./lib/aliases.zsh
. ./plugins/git-extras/git-extras.plugin.zsh

export EDITOR=vim
HISTSIZE=5000
setopt extended_glob

prompt_setup() {
  PROMPT='%n@%m %~ $(git_current_branch) %# '
}

precmd() {
  prompt_setup
}

for f in ~/.zsh/*.zsh(N); do
  source "$f"
done

git_sync
