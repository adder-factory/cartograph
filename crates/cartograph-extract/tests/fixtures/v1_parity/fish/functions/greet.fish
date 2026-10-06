function greet --argument-names name
    set -l who $name
    if test -z "$who"
        set who stranger
    end
    log_info "greeting"
    echo "hello $who"
    format_name $who
end

function format_name
    string upper -- $argv[1]
end
