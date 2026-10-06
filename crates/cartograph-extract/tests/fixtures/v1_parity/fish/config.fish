set VERSION 1.2.3
set -x EDITOR vim
set -gx PAGER less
set -U fish_greeting ""
set --export LANG en_US.UTF-8
set -e OLD_VAR
set -q MAYBE_VAR

source conf.d/aliases.fish
source 'functions/greet.fish'
. "functions/deploy.fish"
source $HOME/.local.fish

if status is-interactive
    greet world
    setup_aliases
end
