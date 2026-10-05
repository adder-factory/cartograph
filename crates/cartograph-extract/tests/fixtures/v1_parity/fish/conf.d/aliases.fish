function setup_aliases --description 'install aliases'
    alias ll 'ls -la'
    abbr --add gs git status
    log_info "aliases ready"
end

function log_info
    set -l prefix "[info]"
    echo $prefix $argv
end
