set -g DEPLOY_TARGET staging

function deploy -d "deploy the app" --wraps git
    set -l branch (git rev-parse --abbrev-ref HEAD)
    greet deployer
    run_step build
    run_step "push $DEPLOY_TARGET"
end

function run_step
    log_info "step: $argv"
    eval $argv
end
