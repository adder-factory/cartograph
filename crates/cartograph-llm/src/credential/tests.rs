#[cfg(unix)]
use std::assert_matches;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;
#[cfg(unix)]
use crate::ProjectLlmCredentialSource;
use crate::{
    ProjectLlmCredentialWriteAction, ProjectLlmTier, ProjectLlmTierInput,
    load_project_llm_credential_environment, load_project_llm_tier,
    write_project_llm_configuration_with_report,
};

const CONFIG_PATH: &str = ".cartograph/config.json";
const REMOTE_ENDPOINT: &str = "https://example.test/v1";
#[cfg(unix)]
const SHORT_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(unix)]
const SLOW_HELPER_SECONDS: u64 = 5;

/// Write an executable `/bin/sh` helper and return its path.
#[cfg(unix)]
fn helper(root: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let path = root.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))
        .unwrap_or_else(|error| panic!("helper write failed: {error}"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("helper chmod failed: {error}"));
    path
}

fn command(argv: &[&str]) -> CredentialCommand {
    CredentialCommand::new(argv.iter().map(|part| (*part).to_owned()).collect())
        .unwrap_or_else(|error| panic!("credential command rejected: {error}"))
}

#[cfg(unix)]
fn command_at(path: &Path) -> CredentialCommand {
    command(&[path
        .to_str()
        .unwrap_or_else(|| panic!("helper path is not UTF-8"))])
}

/// Helper that counts its runs next to itself and prints `<prefix>-<run>`.
#[cfg(unix)]
fn counting_helper(root: &Path, prefix: &str) -> (CredentialCommand, PathBuf) {
    let path = helper(
        root,
        prefix,
        &format!(
            "echo run >> \"$0.runs\"\nprintf '{prefix}-%s\\n\\n' \"$(wc -l < \"$0.runs\" | tr -d ' ')\""
        ),
    );
    (command_at(&path), path.with_extension("runs"))
}

#[cfg(unix)]
fn runs(counter: &Path) -> usize {
    std::fs::read_to_string(counter).map_or(0, |text| text.lines().count())
}

#[cfg(unix)]
fn exposed(secret: &SecretString) -> &str {
    secret.expose_secret()
}

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
    std::fs::create_dir(root.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("state directory failed: {error}"));
    root
}

fn write_config(root: &Path, value: &Value) {
    std::fs::write(root.join(CONFIG_PATH), value.to_string())
        .unwrap_or_else(|error| panic!("config write failed: {error}"));
}

fn read_config(root: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(root.join(CONFIG_PATH))
            .unwrap_or_else(|error| panic!("config read failed: {error}")),
    )
    .unwrap_or_else(|error| panic!("config JSON failed: {error}"))
}

#[test]
fn argv_uses_the_cli_bridge_bounds_and_debug_names_only_the_program() {
    assert!(CredentialCommand::new(Vec::new()).is_err());
    assert!(CredentialCommand::new(vec![String::new()]).is_err());
    assert!(CredentialCommand::new(vec!["helper".to_owned(), "two\nlines".to_owned()]).is_err());
    let too_many = std::iter::once("helper".to_owned())
        .chain(
            std::iter::repeat_n(
                "argument".to_owned(),
                crate::project_config::MAXIMUM_CLI_ARGUMENTS,
            )
            .chain(std::iter::once("one-more".to_owned())),
        )
        .collect();
    assert!(CredentialCommand::new(too_many).is_err());

    let accepted = command(&["/opt/tools/op", "read", "op://vault/typesafe-item"]);
    assert_eq!(accepted.program_name(), "op");
    assert_eq!(accepted.args(), ["read", "op://vault/typesafe-item"]);
    let rendered = format!("{accepted:?}");
    assert!(rendered.contains("\"op\"") && rendered.contains("argument_count: 2"));
    assert!(!rendered.contains("vault") && !rendered.contains("/opt/tools"));
    let credential = TierCredential::Static(SecretString::from("tier-secret-value"));
    assert!(!format!("{credential:?}").contains("tier-secret-value"));
}

#[cfg(unix)]
#[tokio::test]
async fn output_is_trimmed_cached_and_rerun_only_after_a_rejection() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let (command, counter) = counting_helper(root.path(), "rotating");
    let first = command
        .resolve()
        .await
        .unwrap_or_else(|error| panic!("first resolve failed: {error}"));
    let cached = command
        .resolve()
        .await
        .unwrap_or_else(|error| panic!("cached resolve failed: {error}"));
    assert_eq!(
        (exposed(&first), exposed(&cached)),
        ("rotating-1", "rotating-1")
    );
    assert_eq!(runs(&counter), 1);

    let rotated = command
        .refresh_rejected(&first)
        .await
        .unwrap_or_else(|error| panic!("refresh failed: {error}"))
        .unwrap_or_else(|| panic!("a changed credential was not returned"));
    assert_eq!(exposed(&rotated), "rotating-2");
    assert_eq!(runs(&counter), 2);

    // A request that was rejected with the old value reuses the replacement
    // another request already fetched instead of running the helper again.
    let concurrent = command
        .refresh_rejected(&first)
        .await
        .unwrap_or_else(|error| panic!("stale refresh failed: {error}"))
        .unwrap_or_else(|| panic!("the replacement was not shared"));
    assert_eq!(exposed(&concurrent), "rotating-2");
    assert_eq!(runs(&counter), 2);
    let current = command
        .resolve()
        .await
        .unwrap_or_else(|error| panic!("resolve after rotation failed: {error}"));
    assert_eq!(exposed(&current), "rotating-2");
}

#[cfg(unix)]
#[tokio::test]
async fn an_unchanged_rejected_value_is_not_rerun_by_every_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let path = helper(
        root.path(),
        "constant",
        "echo run >> \"$0.runs\"\nprintf 'constant-key'",
    );
    let (command, counter) = (command_at(&path), path.with_extension("runs"));
    let key = command
        .resolve()
        .await
        .unwrap_or_else(|error| panic!("resolve failed: {error}"));
    for _ in 0..3 {
        assert!(
            command
                .refresh_rejected(&key)
                .await
                .unwrap_or_else(|error| panic!("refresh failed: {error}"))
                .is_none()
        );
    }
    assert_eq!(runs(&counter), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn a_new_value_the_provider_keeps_rejecting_is_rerun_once_per_interval() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let (command, counter) = counting_helper(root.path(), "rotating");
    let first = command
        .resolve()
        .await
        .unwrap_or_else(|error| panic!("resolve failed: {error}"));
    let second = command
        .refresh_rejected(&first)
        .await
        .unwrap_or_else(|error| panic!("refresh failed: {error}"))
        .unwrap_or_else(|| panic!("a re-run that printed a new value must return it"));
    assert_eq!(exposed(&second), "rotating-2");
    for _ in 0..3 {
        assert!(
            command
                .refresh_rejected(&second)
                .await
                .unwrap_or_else(|error| panic!("refresh failed: {error}"))
                .is_none()
        );
    }
    assert_eq!(runs(&counter), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn failures_name_the_program_and_status_but_never_output_or_stderr() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let exact = MAXIMUM_CREDENTIAL_OUTPUT_BYTES;
    let oversized = MAXIMUM_CREDENTIAL_OUTPUT_BYTES + 1;
    let cases = [
        (
            "nonzero",
            "printf 'stdout-secret'\nprintf 'stderr-secret' >&2\nexit 7".to_owned(),
            CredentialCommandFailure::Exited { code: Some(7) },
        ),
        (
            "signalled",
            "printf 'stdout-secret'\nkill -9 $$".to_owned(),
            CredentialCommandFailure::Exited { code: None },
        ),
        (
            "silent",
            "exit 0".to_owned(),
            CredentialCommandFailure::EmptyOutput,
        ),
        (
            "blank",
            "printf '  \\n\\t\\n'".to_owned(),
            CredentialCommandFailure::EmptyOutput,
        ),
        (
            "oversized",
            format!("head -c {oversized} /dev/zero | tr '\\000' x"),
            CredentialCommandFailure::OutputTooLarge,
        ),
        (
            "control",
            "printf 'secret\\001value'".to_owned(),
            CredentialCommandFailure::InvalidOutput,
        ),
        (
            "binary",
            "printf '\\377\\376'".to_owned(),
            CredentialCommandFailure::InvalidOutput,
        ),
    ];
    for (name, body, expected) in cases {
        let error = command_at(&helper(root.path(), name, &body))
            .verify()
            .await
            .err()
            .unwrap_or_else(|| panic!("{name} produced a credential"));
        assert_eq!(error.failure(), expected, "{name}");
        assert_eq!(error.program(), name);
        let rendered = format!("{error} {error:?}");
        assert!(rendered.contains(&format!("credential command `{name}`")));
        assert!(!rendered.contains("secret"), "{name} leaked: {rendered}");
    }
    let nonzero = command_at(&root.path().join("nonzero"))
        .verify()
        .await
        .err()
        .map(|error| error.to_string());
    assert_eq!(
        nonzero.as_deref(),
        Some("credential command `nonzero` exited with status 7")
    );
    let missing = command_at(&root.path().join("missing-helper"))
        .verify()
        .await;
    assert_eq!(
        missing.map_err(|error| error.failure()),
        Err(CredentialCommandFailure::SpawnFailed)
    );
    let limit = helper(
        root.path(),
        "exact",
        &format!("head -c {exact} /dev/zero | tr '\\000' x"),
    );
    assert!(command_at(&limit).verify().await.is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn a_hanging_helper_is_stopped_at_its_deadline() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let command = command_at(&helper(
        root.path(),
        "hanging",
        &format!("sleep {SLOW_HELPER_SECONDS}\nprintf 'late-key'"),
    ));
    let started = Instant::now();
    let outcome = run_credential_command(
        &command,
        CommandLimits {
            timeout: SHORT_TIMEOUT,
            ..CommandLimits::DEFAULT
        },
    )
    .await;
    assert_matches!(outcome, Err(CredentialCommandFailure::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(SLOW_HELPER_SECONDS));
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_run_is_remembered_instead_of_rerun_by_every_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let path = helper(root.path(), "locked", "echo run >> \"$0.runs\"\nexit 3");
    let (command, counter) = (command_at(&path), path.with_extension("runs"));
    assert!(command.recent_failure().is_none());
    let first = command.resolve().await.err();
    assert_eq!(
        first.as_ref().map(CredentialCommandError::failure),
        Some(CredentialCommandFailure::Exited { code: Some(3) })
    );
    assert_eq!(command.recent_failure(), first);
    assert_eq!(command.resolve().await.err(), first);
    assert_eq!(
        command
            .refresh_rejected(&SecretString::from("rejected"))
            .await
            .err(),
        first
    );
    assert_eq!(runs(&counter), 1);
}

#[cfg(unix)]
#[test]
fn config_stores_only_the_argv_and_loading_never_runs_the_command() {
    let root = project();
    let marker = root.path().join("ran");
    let path = helper(
        root.path(),
        "loader-helper",
        &format!("touch '{}'\nprintf 'key'", marker.display()),
    );
    let argv = [
        path.to_str()
            .unwrap_or_else(|| panic!("helper path is not UTF-8")),
        "get",
        "summary-item",
    ];
    let input = ProjectLlmTierInput::new(ProjectLlmTier::Summarize, REMOTE_ENDPOINT, "chat")
        .and_then(|input| input.with_api_key_env("CARTOGRAPH_REPLACED_KEY"))
        .and_then(|input| input.with_api_key_command(command(&argv)))
        .unwrap_or_else(|error| panic!("tier input failed: {error}"));
    let report = write_project_llm_configuration_with_report(root.path(), &[input], &[])
        .unwrap_or_else(|error| panic!("config write failed: {error}"));
    assert_eq!(
        report.credential_actions[0].action,
        ProjectLlmCredentialWriteAction::CommandReferenceSet
    );
    let written = read_config(root.path());
    assert_eq!(written["llm"]["summarizeLlm"]["apiKeyCommand"], json!(argv));
    assert!(written["llm"]["summarizeLlm"].get("apiKeyEnv").is_none());

    let loaded = load_project_llm_tier(root.path(), ProjectLlmTier::Summarize)
        .unwrap_or_else(|error| panic!("tier load failed: {error}"))
        .unwrap_or_else(|| panic!("tier missing"));
    assert_eq!(
        loaded.credential_source(),
        ProjectLlmCredentialSource::Command
    );
    assert_eq!(loaded.api_key_command(), Some(&command(&argv)));
    assert!(loaded.api_key().is_none() && loaded.api_key_env().is_none());
    assert!(!format!("{loaded:?}").contains("summary-item"));
    assert!(
        !marker.exists(),
        "loading the tier ran the credential command"
    );
    assert_eq!(
        load_project_llm_credential_environment(root.path(), ProjectLlmTier::Summarize),
        Ok(None)
    );
}

#[test]
fn same_origin_updates_keep_a_command_and_origin_changes_or_new_sources_replace_it() {
    let root = project();
    let argv = ["/opt/helper", "embedding-key"];
    let write = |input: ProjectLlmTierInput| {
        write_project_llm_configuration_with_report(root.path(), &[input], &[])
            .unwrap_or_else(|error| panic!("config write failed: {error}"))
            .credential_actions[0]
            .action
    };
    let input = |endpoint: &str| {
        ProjectLlmTierInput::new(ProjectLlmTier::Embedding, endpoint, "model")
            .unwrap_or_else(|error| panic!("tier input failed: {error}"))
    };
    let stored = || read_config(root.path())["llm"]["embeddingLlm"].clone();
    let with_command = input(REMOTE_ENDPOINT)
        .with_api_key_command(command(&argv))
        .unwrap_or_else(|error| panic!("command input failed: {error}"));
    assert_eq!(
        write(with_command.clone()),
        ProjectLlmCredentialWriteAction::CommandReferenceSet
    );
    assert_eq!(
        write(input("https://example.test/v2")),
        ProjectLlmCredentialWriteAction::Preserved
    );
    assert_eq!(stored()["apiKeyCommand"], json!(argv));
    assert_eq!(
        write(input("https://other.example.test/v1")),
        ProjectLlmCredentialWriteAction::ClearedOriginChange
    );
    assert!(stored().get("apiKeyCommand").is_none());

    write(with_command);
    let with_environment = input(REMOTE_ENDPOINT)
        .with_api_key_env("CARTOGRAPH_EMBEDDING_FIXTURE_KEY")
        .unwrap_or_else(|error| panic!("environment input failed: {error}"));
    assert_eq!(
        write(with_environment),
        ProjectLlmCredentialWriteAction::EnvironmentReferenceSet
    );
    assert!(stored().get("apiKeyCommand").is_none());
    assert_eq!(stored()["apiKeyEnv"], "CARTOGRAPH_EMBEDDING_FIXTURE_KEY");
}

#[test]
fn a_command_is_exclusive_with_other_sources_and_rejected_for_bridges() {
    let root = project();
    let remote = json!({"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT, "model": "chat"});
    let bridge = json!({"provider": "cli-bridge", "model": "chat", "command": "tool",
        "args": [], "input": "stdin", "responseFormat": "raw"});
    let load = |tier: &Value, extra: &Value| {
        let mut tier = tier.clone();
        if let (Some(tier), Some(extra)) = (tier.as_object_mut(), extra.as_object()) {
            tier.extend(extra.clone());
        }
        write_config(
            root.path(),
            &json!({"version": 2, "llm": {"enabled": true, "summarizeLlm": tier}}),
        );
        load_project_llm_tier(root.path(), ProjectLlmTier::Summarize).map(|tier| tier.is_some())
    };
    let command = json!({"apiKeyCommand": ["/opt/helper"]});
    assert_eq!(load(&remote, &command), Ok(true));
    assert_eq!(load(&bridge, &json!({})), Ok(true));
    for (tier, extra) in [
        (
            &remote,
            json!({"apiKeyCommand": ["/opt/helper"], "apiKeyEnv": "CARTOGRAPH_FIXTURE_KEY"}),
        ),
        (
            &remote,
            json!({"apiKeyCommand": ["/opt/helper"], "apiKey": "inline-fixture"}),
        ),
        (&remote, json!({"apiKeyCommand": []})),
        (&remote, json!({"apiKeyCommand": ["/opt/helper", 7]})),
        (&remote, json!({"apiKeyCommand": "/opt/helper get key"})),
        (&bridge, command.clone()),
    ] {
        assert_eq!(
            load(tier, &extra),
            Err(crate::ProjectLlmConfigError::InvalidTier),
            "{extra}"
        );
    }
    let bridge = crate::CliBridgeConfig::claude_compatible(None)
        .unwrap_or_else(|error| panic!("bridge failed: {error}"));
    assert!(
        ProjectLlmTierInput::cli_bridge(ProjectLlmTier::Summarize, "model", bridge)
            .and_then(|input| input.with_api_key_command(self::command(&["/opt/helper"])))
            .is_err()
    );
}

#[test]
fn diagnostics_name_the_configured_variable_without_reading_it() {
    let root = project();
    write_config(
        root.path(),
        &json!({"version": 2, "llm": {"enabled": true,
            "decisionLlm": {"provider": "typesafe", "model": crate::JEV_MODEL},
            "summarizeLlm": {"provider": "openai-compat", "endpoint": REMOTE_ENDPOINT,
                "model": "chat", "apiKeyEnv": "CARTOGRAPH_UNSET_DIAGNOSTIC_KEY"}}}),
    );
    let name = |tier| {
        load_project_llm_credential_environment(root.path(), tier)
            .unwrap_or_else(|error| panic!("diagnostic lookup failed: {error}"))
    };
    assert_eq!(
        name(ProjectLlmTier::Decision).as_deref(),
        Some("TYPESAFE_API_KEY")
    );
    assert_eq!(
        name(ProjectLlmTier::Summarize).as_deref(),
        Some("CARTOGRAPH_UNSET_DIAGNOSTIC_KEY")
    );
    assert_eq!(name(ProjectLlmTier::Reranker), None);
}
