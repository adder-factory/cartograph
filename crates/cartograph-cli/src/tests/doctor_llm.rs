//! Doctor's LLM-tier checks: what blocks readiness and what only warns.

use super::super::*;

const REMOTE_ENDPOINT: &str = "https://example.test/v1";

fn doctor_project(llm: &Value) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
    fs::create_dir(root.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("state directory failed: {error}"));
    fs::write(
        root.path().join(".cartograph/config.json"),
        serde_json::json!({"version": 2, "llm": llm}).to_string(),
    )
    .unwrap_or_else(|error| panic!("config failed: {error}"));
    root
}

async fn llm_checks(root: &Path) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    check_llm_configuration(root, &mut checks).await;
    checks
}

fn find<'checks>(checks: &'checks [DoctorCheck], id: &str) -> &'checks DoctorCheck {
    checks
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("doctor check {id} missing from {checks:#?}"))
}

/// A variable name unique to this test process and certainly unset in it.
fn unset_variable(purpose: &str) -> String {
    let name = format!("CARTOGRAPH_DOCTOR_{purpose}_{}", std::process::id());
    assert!(env::var_os(&name).is_none());
    name
}

#[tokio::test]
async fn optional_tier_credentials_missing_from_this_shell_warn_without_blocking_readiness() {
    let decision_variable = unset_variable("JEV_KEY");
    let summary_variable = unset_variable("CHAT_KEY");
    let root = doctor_project(&serde_json::json!({
        "enabled": true,
        "embeddingLlm": {"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT, "model": "embed"},
        "decisionLlm": {"provider": "typesafe", "model": cartograph_llm::JEV_MODEL,
            "apiKeyEnv": decision_variable},
        "summarizeLlm": {"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT,
            "model": "chat", "apiKeyEnv": summary_variable},
    }));
    let checks = llm_checks(root.path()).await;
    assert!(
        checks
            .iter()
            .all(|check| check.status != DoctorStatus::Fail),
        "an optional tier blocked readiness: {checks:#?}"
    );
    let decision = find(&checks, "llm-decision-credential");
    assert_eq!(decision.status, DoctorStatus::Warn);
    assert_eq!(
        decision.message,
        format!(
            "{decision_variable} is not set in this shell; the MCP server needs it in its own environment. Explore uses native retrieval until then."
        )
    );
    let remediation = decision.remediation.as_deref().unwrap_or_default();
    assert!(remediation.contains(&format!("Supply {decision_variable} to the MCP server")));
    assert!(remediation.contains("--preset jev --api-key-command <exe>"));
    assert!(!remediation.contains("TYPESAFE_API_KEY"));
    assert!(!checks.iter().any(|check| check.id == "llm-decision-config"));

    let summary = find(&checks, "llm-summarize-credential");
    assert_eq!(summary.status, DoctorStatus::Warn);
    assert!(
        summary
            .message
            .starts_with(&format!("{summary_variable} is not set"))
    );
    assert!(
        summary
            .message
            .ends_with("The summarize tier is unavailable until then.")
    );
}

#[tokio::test]
async fn an_invalid_decision_tier_and_a_required_tier_without_its_credential_still_fail() {
    let decision_variable = unset_variable("INVALID_JEV_KEY");
    let embedding_variable = unset_variable("EMBEDDING_KEY");
    let root = doctor_project(&serde_json::json!({
        "enabled": true,
        "embeddingLlm": {"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT,
            "model": "embed", "apiKeyEnv": embedding_variable},
        "decisionLlm": {"provider": "typesafe", "model": "latest", "apiKeyEnv": decision_variable},
    }));
    let checks = llm_checks(root.path()).await;
    let decision = find(&checks, "llm-decision-config");
    assert_eq!(decision.status, DoctorStatus::Fail);
    assert!(
        decision
            .remediation
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("--preset jev --api-key-env {decision_variable}"))
    );
    let embedding = find(&checks, "llm-embedding-credential");
    assert_eq!(embedding.status, DoctorStatus::Fail);
    assert!(
        embedding
            .message
            .starts_with(&format!("{embedding_variable} is not set"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn doctor_runs_credential_commands_and_reports_only_their_outcome() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = doctor_project(&Value::Null);
    let helper = |name: &str, body: &str| {
        let path = root.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))
            .unwrap_or_else(|error| panic!("helper failed: {error}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("helper chmod failed: {error}"));
        path.to_str()
            .unwrap_or_else(|| panic!("helper path is not UTF-8"))
            .to_owned()
    };
    let working = helper("jev-helper", "printf 'doctor-token-value'");
    let locked = helper(
        "chat-helper",
        "printf 'doctor-token-value'\nprintf 'stderr-token-value' >&2\nexit 5",
    );
    fs::write(
        root.path().join(".cartograph/config.json"),
        serde_json::json!({"version": 2, "llm": {
            "enabled": true,
            "decisionLlm": {"provider": "typesafe", "model": cartograph_llm::JEV_MODEL,
                "apiKeyCommand": [working]},
            "summarizeLlm": {"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT,
                "model": "chat", "apiKeyCommand": [locked, "--vault", "private-vault"]},
        }})
        .to_string(),
    )
    .unwrap_or_else(|error| panic!("config failed: {error}"));
    let checks = llm_checks(root.path()).await;
    assert!(
        checks
            .iter()
            .all(|check| check.status != DoctorStatus::Fail)
    );
    let decision = find(&checks, "llm-decision-credential");
    assert_eq!(decision.status, DoctorStatus::Pass);
    assert!(
        decision
            .message
            .contains("`jev-helper` produced a credential")
    );
    assert_eq!(
        find(&checks, "llm-decision-config").status,
        DoctorStatus::Pass
    );
    let summary = find(&checks, "llm-summarize-credential");
    assert_eq!(summary.status, DoctorStatus::Warn);
    assert_eq!(
        summary.message,
        "The summarize tier's credential command `chat-helper` exited with status 5. The summarize tier is unavailable until then."
    );
    let rendered = serde_json::to_string(&checks)
        .unwrap_or_else(|error| panic!("checks failed to serialize: {error}"));
    assert!(
        !rendered.contains("token-value") && !rendered.contains("private-vault"),
        "{rendered}"
    );
}
