//! Live CLI integration coverage across the public command surface.

mod dependency_ownership;

use std::{
    fmt::Write as _,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

#[test]
fn public_cli_exercises_native_agent_backend_and_optional_llm_routes() {
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    let missing_model = project
        .path()
        .join(".cartograph/missing-model.gguf")
        .to_string_lossy()
        .into_owned();

    let scenario = LiveCliScenario {
        project: &project,

        project_path: &project_path,
        missing_model: &missing_model,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| run_live_cli_scenario(&scenario)));

    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
fn doctor_exposes_layered_readiness_before_the_first_generation() {
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    write_project_fixture(project.path());

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        json_success(
            project.path(),
            &[
                "llm",
                "install",
                &project_path,
                "--no-models",
                "--minimal",
                "--json",
            ],
        );
        let report = json_success(project.path(), &["doctor", &project_path, "--json"]);
        assert_eq!(report["ready"], true);
        assert_eq!(report["capabilitiesReady"], true);
        assert_eq!(report["projectReadiness"]["database"], "ready");
        assert_eq!(report["projectReadiness"]["index"], "not_indexed");
        assert_eq!(report["projectReadiness"]["freshness"], "unavailable");
        assert_eq!(
            report["projectReadiness"]["deterministicRetrieval"],
            "unavailable"
        );
        assert_eq!(report["projectReadiness"]["registration"], "not_checked");
        assert_eq!(report["projectReadiness"]["liveTransport"], "not_checked");
        assert_eq!(report["projectReadiness"]["overall"], "incomplete");
        assert!(
            report["nextActions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| {
                    action
                        .as_str()
                        .is_some_and(|action| action.contains("cartograph index <path>"))
                }))
        );

        let skipped = json_success(
            project.path(),
            &["doctor", &project_path, "--no-project-checks", "--json"],
        );
        assert_eq!(skipped["projectReadiness"]["index"], "not_checked");
        assert_eq!(skipped["projectReadiness"]["freshness"], "not_checked");
        assert_eq!(
            skipped["projectReadiness"]["deterministicRetrieval"],
            "not_checked"
        );
        assert_eq!(
            skipped["projectReadiness"]["semanticRetrieval"],
            "not_checked"
        );
        assert_eq!(skipped["projectReadiness"]["overall"], "not_checked");

        let text = success(project.path(), &["doctor", &project_path]);
        let rendered = String::from_utf8_lossy(&text.stdout);
        assert!(rendered.contains("Readiness layers:"));
        assert!(rendered.contains("- overall onboarding: incomplete"));
        assert!(rendered.contains("Next actions:"));

        let encoded = serde_json::to_string(&report)
            .unwrap_or_else(|error| panic!("doctor report encoding failed: {error}"));
        assert!(!encoded.contains(&project_path));
    }));

    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
fn parse_failure_json_names_relative_input_and_preserves_the_current_generation() {
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    write_project_fixture(project.path());

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let initial = json_success(
            project.path(),
            &["index", &project_path, "--format", "json"],
        );
        let initial_generation = initial["generation_id"]
            .as_str()
            .unwrap_or_else(|| panic!("initial index omitted its generation identity"));

        let mut excessive = String::new();
        for index in 0..20_000 {
            writeln!(excessive, "int v{index};")
                .unwrap_or_else(|error| panic!("failure fixture write failed: {error}"));
        }
        std::fs::write(project.path().join("src/fatal-output.c"), excessive)
            .unwrap_or_else(|error| panic!("failure fixture source write failed: {error}"));

        let output = failure(
            project.path(),
            &["index", &project_path, "--format", "json"],
        );
        assert_eq!(output.stdout, b"");
        let report: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
            panic!(
                "index failure stderr was not standalone JSON: {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(
            report["error"]["code"],
            "parse_extraction_output_limit_exceeded"
        );
        assert_eq!(report["error"]["stage"], "parse");
        assert_eq!(report["error"]["previous_generation_visible"], true);
        assert_eq!(
            report["error"]["file_failure"]["path"],
            "src/fatal-output.c"
        );
        assert_eq!(
            report["error"]["file_failure"]["reason"],
            "extraction_output_limit_exceeded"
        );
        let encoded = String::from_utf8_lossy(&output.stderr);
        assert!(!encoded.contains(&project_path));
        assert!(!encoded.contains("int v19999"));

        let status = json_success(
            project.path(),
            &["status", &project_path, "--format", "json"],
        );
        assert_eq!(
            status["project"]["snapshot"]["current"]["generation_id"],
            initial_generation
        );
        assert_eq!(status["project"]["fresh"], false);
    }));

    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[derive(Clone, Copy)]
struct LiveCliScenario<'a> {
    project: &'a tempfile::TempDir,

    project_path: &'a str,
    missing_model: &'a str,
}

struct NativeSymbols {
    root_id: String,
    leaf_id: String,
}

fn run_live_cli_scenario(scenario: &LiveCliScenario<'_>) {
    prepare_repository_and_verify_read_only(scenario);
    index_and_verify_storage(scenario);
    let symbols = verify_native_retrieval(scenario);
    verify_native_file_surfaces(scenario);
    verify_native_graph_surfaces(scenario, &symbols);
    verify_agent_compatibility_surfaces(scenario);
    verify_operational_surfaces(scenario);
    verify_analysis_surfaces(scenario);
    verify_contextual_surfaces(scenario);
    verify_collaboration_surfaces(scenario);
    verify_install_surfaces(scenario);
    verify_invalid_agent_inputs(scenario);
    verify_invalid_database_inputs(scenario);
    verify_llm_configuration_surfaces(scenario);
    verify_backend_surfaces(scenario);
    verify_llm_runtime_surfaces(scenario);
}

fn prepare_repository_and_verify_read_only(scenario: &LiveCliScenario<'_>) {
    let root = scenario.project.path();
    write_project_fixture(root);
    git(root, &["init", "--initial-branch=main"]);
    git(
        root,
        &["config", "user.email", "cli-surface@example.invalid"],
    );
    git(root, &["config", "user.name", "CLI Surface Fixture"]);
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "-m", "CG-99 establish public CLI surface"],
    );
    let before = std::fs::read_dir(root.join(".cartograph"))
        .unwrap_or_else(|error| panic!("state directory: {error}"))
        .count();
    let report = json_success(
        root,
        &[
            "db",
            "usage",
            "--project-path",
            scenario.project_path,
            "--format",
            "json",
        ],
    );
    assert_eq!(report["storage"], "rocksdb");
    assert_eq!(
        std::fs::read_dir(root.join(".cartograph"))
            .unwrap_or_else(|error| panic!("state directory: {error}"))
            .count(),
        before,
        "read-only storage telemetry created state"
    );
}

fn index_and_verify_storage(scenario: &LiveCliScenario<'_>) {
    let root = scenario.project.path();
    let path = scenario.project_path;
    success(root, &["index", path, "--workers", "2", "--format", "json"]);
    let status = json_success(root, &["status", path, "--json", "--verbose"]);
    assert_eq!(status["databaseStorage"]["state"], "ready");
    assert_eq!(status["storage"], "rocksdb");
    assert!(
        status["databaseStorage"]["native"]["fileBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        status["databaseStorage"]["humanReadable"]["files"]
            .as_str()
            .is_some_and(|text| text.ends_with('B'))
    );
    let usage = json_success(
        root,
        &["db", "usage", "--project-path", path, "--format", "json"],
    );
    assert_eq!(usage["storage"], "rocksdb");
    assert!(
        usage["native"]["fileBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    success(
        root,
        &[
            "db",
            "prune",
            "--project-path",
            path,
            "--maximum-cascade-rows",
            "100",
            "--confirm",
            "prune-old-generations",
            "--format",
            "json",
        ],
    );
}

fn verify_native_retrieval(scenario: &LiveCliScenario<'_>) -> NativeSymbols {
    let project = scenario.project;
    let project_path = scenario.project_path;
    let root = json_success(
        project.path(),
        &[
            "find-native",
            "root",
            "--by",
            "name",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    let leaf = json_success(
        project.path(),
        &[
            "find-native",
            "leaf",
            "--by",
            "name",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    let root_id = symbol_id(&root, "root")
        .unwrap_or_else(|| panic!("native find did not return root: {root}"));
    let leaf_id = symbol_id(&leaf, "leaf")
        .unwrap_or_else(|| panic!("native find did not return leaf: {leaf}"));

    for (by, query_text) in [
        ("auto", "order service"),
        ("hybrid", "order service"),
        ("path", "src/lib.rs"),
        ("reference", "leaf"),
        ("bm25", "order service"),
    ] {
        success(
            project.path(),
            &[
                "find-native",
                query_text,
                "--by",
                by,
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
        );
    }
    for anchor in [
        ["--exact-name", "root"],
        ["--exact-path", "src/lib.rs"],
        ["--exact-reference", "leaf"],
    ] {
        success(
            project.path(),
            &[
                "context-native",
                "change root order behavior",
                anchor[0],
                anchor[1],
                "--mode",
                "deterministic",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
        );
    }
    NativeSymbols { root_id, leaf_id }
}

fn verify_native_file_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    success(
        project.path(),
        &[
            "files-native",
            "--dir",
            "src",
            "--language",
            "rust",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &[
            "entry-points-native",
            "--bucket",
            "public-exports",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &[
            "at-range-native",
            "src/lib.rs",
            "1",
            "30",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
}

fn verify_native_graph_surfaces(scenario: &LiveCliScenario<'_>, symbols: &NativeSymbols) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    let root_id = &symbols.root_id;
    let leaf_id = &symbols.leaf_id;
    for direction in ["callers", "callees", "both", "impact"] {
        success(
            project.path(),
            &[
                "graph-native",
                root_id,
                "--direction",
                direction,
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
        );
    }
    success(
        project.path(),
        &[
            "graph-native",
            root_id,
            "--direction",
            "path",
            "--to",
            leaf_id,
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &[
            "show",
            root_id,
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &[
            "affected-native",
            root_id,
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &[
            "review-native",
            "--ref",
            "HEAD",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    success(
        project.path(),
        &["export", project_path, "--format", "json", "--limit", "100"],
    );
}

fn verify_agent_compatibility_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    let find_text = success(
        project.path(),
        &[
            "find",
            "root",
            "--by",
            "name",
            "--format",
            "text",
            "--project-path",
            project_path,
        ],
    );
    let find_text = String::from_utf8_lossy(&find_text.stdout);
    assert!(find_text.contains("Cartograph find (current)"));
    assert!(find_text.contains("root [function]"));
    let find_env_text = success(
        project.path(),
        &[
            "find",
            "CARTOGRAPH_TOKEN",
            "--by",
            "env",
            "--format",
            "text",
            "--project-path",
            project_path,
        ],
    );
    let find_env_text = String::from_utf8_lossy(&find_env_text.stdout);
    assert!(find_env_text.contains("CARTOGRAPH_TOKEN — src/index.ts:1"));
    assert!(!find_env_text.contains("- match"));
    let find_json = json_success(
        project.path(),
        &[
            "find",
            "root",
            "--by",
            "name",
            "--format",
            "json",
            "--compact",
            "--project-path",
            project_path,
        ],
    );
    assert_eq!(find_json["freshness"], "current");
    all_succeed(
        project.path(),
        &[
            vec![
                "find",
                "root",
                "--by",
                "name",
                "--project-path",
                project_path,
            ],
            vec![
                "context",
                "change root order behavior",
                "--mode",
                "deterministic",
                "--project-path",
                project_path,
            ],
            vec![
                "files",
                "--format",
                "symbols",
                "--file",
                "src/lib.rs",
                "--project-path",
                project_path,
            ],
            vec![
                "graph",
                "root",
                "--direction",
                "callees",
                "--project-path",
                project_path,
            ],
            vec!["review", "risk", "--project-path", project_path],
        ],
    );
}

fn verify_operational_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec!["sync-if-dirty", project_path, "--quiet"],
            vec!["install-hooks", project_path, "--dry-run"],
            vec!["mcp-budget", "--profile", "coding", "--json"],
            vec!["completions", "bash"],
            vec!["__complete", "cartograph", "st"],
            vec!["guide"],
            vec!["doctor", project_path, "--json"],
            vec![
                "db",
                "usage",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
            vec![
                "admin",
                "embedding-status",
                "--project-path",
                project_path,
                "--json",
            ],
            vec![
                "admin",
                "embedding-audit",
                "--project-path",
                project_path,
                "--json",
            ],
            vec![
                "admin",
                "embedding-cleanup",
                "--project-path",
                project_path,
                "--json",
            ],
            vec![
                "admin",
                "embedding-cleanup",
                "--project-path",
                project_path,
                "--confirm",
                "--json",
            ],
            vec![
                "admin",
                "llm-plan",
                "--project-path",
                project_path,
                "--json",
            ],
        ],
    );
}

fn verify_analysis_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec![
                "biomarkers",
                "--mode",
                "stats",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
            vec![
                "coverage",
                "--mode",
                "structural",
                "--project-path",
                project_path,
            ],
            vec!["dead-code", "--via", "rule", "--project-path", project_path],
            vec!["deps", "--mode", "coverage", "--project-path", project_path],
            vec![
                "hotspots",
                "--category",
                "all",
                "--project-path",
                project_path,
            ],
            vec![
                "host",
                "--mode",
                "diagnostics",
                "--location",
                "local",
                "--project-path",
                project_path,
            ],
            vec!["history", "--mode", "files", "--project-path", project_path],
            vec!["imports", "--source", "all", "--project-path", project_path],
            vec![
                "verify",
                "--ref",
                "HEAD",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
            vec!["playbook", "--project-path", project_path],
        ],
    );
}

fn verify_contextual_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec![
                "compare-to-ref",
                "--ref",
                "HEAD",
                "--include-edges",
                "--include-biomarkers",
                "--project-path",
                project_path,
            ],
            vec!["digest", "--project-path", project_path],
            vec![
                "explore",
                "order service",
                "--mode",
                "deterministic",
                "--summary",
                "--project-path",
                project_path,
            ],
            vec![
                "node",
                "root",
                "--include-callers",
                "--include-callees",
                "--include-tests",
                "--include-biomarkers",
                "--project-path",
                project_path,
            ],
        ],
    );
}

fn verify_collaboration_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec![
                "note",
                "add",
                "--symbol",
                "root",
                "--kind",
                "note",
                "--text",
                "Public CLI fixture note",
                "--author",
                "release-gate",
                "--project-path",
                project_path,
            ],
            vec!["note", "list", "--project-path", project_path],
            vec![
                "propose-rename",
                "root",
                "renamed_root",
                "--project-path",
                project_path,
            ],
            vec![
                "role",
                "--symbol",
                "root",
                "--via",
                "rule",
                "--project-path",
                project_path,
            ],
            vec![
                "session",
                "create",
                "--objective",
                "Verify the public command surface",
                "--label",
                "release-gate",
                "--project-path",
                project_path,
            ],
            vec!["session", "list", "--project-path", project_path],
            vec!["session", "usage", "--project-path", project_path],
            vec!["summaries", "pending", "--project-path", project_path],
            vec!["tests-for", "root", "--project-path", project_path],
            vec![
                "tests-for",
                "--files",
                "src/lib.rs",
                "--project-path",
                project_path,
            ],
            vec![
                "trace-to-culprits",
                "at root (src/lib.rs:5:1)",
                "--project-path",
                project_path,
            ],
            vec!["blame", "root", "--project-path", project_path],
            vec!["changed-since", "--project-path", project_path],
            vec![
                "db",
                "prune",
                "--confirm",
                "prune-old-generations",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
        ],
    );
}

fn verify_install_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec![
                "install",
                "--yes",
                "--target",
                "codex",
                "--location",
                "local",
                "--project-path",
                project_path,
                "--no-permissions",
                "--no-hooks",
                "--format",
                "json",
            ],
            vec![
                "uninstall",
                "--target",
                "codex",
                "--location",
                "local",
                "--project-path",
                project_path,
                "--format",
                "json",
            ],
        ],
    );
}

fn verify_invalid_agent_inputs(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_fail(
        project.path(),
        &[
            vec!["admin", "status", "--project-path", project_path, "--json"],
            vec![
                "similar",
                "root",
                "--min-score",
                "2",
                "--project-path",
                project_path,
            ],
            vec!["mcp-budget", "--disable-tool", "cartograph_missing"],
            vec![
                "ask",
                "How does root call leaf?",
                "--mode",
                "code",
                "--retrieval-mode",
                "deterministic",
                "--project-path",
                project_path,
            ],
        ],
    );
}

fn verify_invalid_database_inputs(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    let missing_backup = project
        .path()
        .join("missing.backup")
        .to_string_lossy()
        .into_owned();
    all_fail(
        project.path(),
        &[
            vec![
                "db",
                "restore",
                &missing_backup,
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "remove",
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "upgrade",
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "derived-index",
                "--rebuild",
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "import-v1",
                "--source-schema",
                "cartograph_v1",
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "prune",
                "--confirm",
                "wrong-confirmation",
                "--project-path",
                project_path,
            ],
            vec![
                "db",
                "compact",
                "--project-path",
                project_path,
                "--apply",
                "--confirm",
                "wrong-confirmation",
            ],
            vec![
                "db",
                "compact",
                "--heap",
                "--project-path",
                project_path,
                "--apply",
                "--confirm",
                "compact-online-indexes",
            ],
        ],
    );
}

fn verify_llm_configuration_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    let missing_model = scenario.missing_model;
    success(
        project.path(),
        &[
            "llm",
            "setup",
            project_path,
            "--preset",
            "custom",
            "--tier",
            "chat",
            "--endpoint",
            "http://127.0.0.1:65534",
            "--model",
            missing_model,
            "--yes",
            "--json",
        ],
    );
    success(
        project.path(),
        &["llm", "migrate-credentials", project_path, "--json"],
    );
    seed_inline_llm_credential(project.path());
    all_fail(
        project.path(),
        &[
            vec![
                "llm",
                "migrate-credentials",
                project_path,
                "--tier-env",
                "summarize=CARTOGRAPH_LIVE_CLI_MISSING_KEY",
            ],
            vec![
                "llm",
                "migrate-credentials",
                project_path,
                "--tier-env",
                "summarize=CARTOGRAPH_LIVE_CLI_MISSING_KEY",
                "--json",
            ],
            vec![
                "llm",
                "migrate-credentials",
                project_path,
                "--apply",
                "--confirm",
                "wrong-confirmation",
            ],
        ],
    );
}

fn verify_backend_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        &[
            vec!["backend", "status", project_path, "--json"],
            vec!["backend", "stop", project_path, "--json"],
            vec!["backend", "logs", project_path, "--lines", "20", "--json"],
            vec![
                "backend",
                "cleanup",
                project_path,
                "--minimum-age-hours",
                "0",
                "--maximum-deletions",
                "2",
            ],
            vec![
                "backend",
                "cleanup",
                project_path,
                "--minimum-age-hours",
                "0",
                "--maximum-deletions",
                "2",
                "--json",
            ],
            vec![
                "backend",
                "cleanup",
                project_path,
                "--apply",
                "--confirm",
                "cleanup-backend-junk",
                "--minimum-age-hours",
                "0",
                "--maximum-deletions",
                "2",
                "--json",
            ],
        ],
    );
    all_fail(
        project.path(),
        &[
            vec!["backend", "start", project_path, "--dry-run", "--json"],
            vec!["backend", "restart", project_path, "--dry-run", "--json"],
            vec![
                "backend",
                "cleanup",
                project_path,
                "--apply",
                "--confirm",
                "wrong-confirmation",
            ],
        ],
    );
}

fn verify_llm_runtime_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let project_path = scenario.project_path;
    success(
        project.path(),
        &[
            "llm",
            "install",
            project_path,
            "--no-models",
            "--minimal",
            "--json",
        ],
    );
    failure(
        project.path(),
        &[
            "llm",
            "smoke",
            project_path,
            "--timeout-ms",
            "100",
            "--json",
        ],
    );
}

fn write_project_fixture(root: &Path) {
    std::fs::create_dir_all(root.join(".cartograph"))
        .unwrap_or_else(|error| panic!("Cartograph fixture directory failed: {error}"));
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("source fixture directory failed: {error}"));
    std::fs::create_dir_all(root.join("tests"))
        .unwrap_or_else(|error| panic!("test fixture directory failed: {error}"));
    std::fs::write(
        root.join("src/lib.rs"),
        r"pub fn leaf(value: i32) -> i32 {
    value + 1
}

pub fn root(value: i32) -> i32 {
    leaf(value)
}

pub struct OrderService;
impl OrderService {
    pub fn execute(value: i32) -> i32 { root(value) }
}
",
    )
    .unwrap_or_else(|error| panic!("Rust fixture write failed: {error}"));
    std::fs::write(
        root.join("src/server.ts"),
        r#"import express from "express";
export const app = express();
export function handleOrder(id: string): string { return id.trim(); }
app.get("/orders/:id", (request, response) => response.send(handleOrder(request.params.id)));
"#,
    )
    .unwrap_or_else(|error| panic!("TypeScript fixture write failed: {error}"));
    std::fs::write(
        root.join("src/index.ts"),
        "process.env.CARTOGRAPH_TOKEN;\nexport const marker = 1;\n",
    )
    .unwrap_or_else(|error| panic!("TypeScript barrel fixture write failed: {error}"));
    std::fs::write(
        root.join("src/consumer.ts"),
        "import { marker } from '.';\nexport const copiedMarker = marker;\n",
    )
    .unwrap_or_else(|error| panic!("TypeScript directory import fixture write failed: {error}"));
    std::fs::write(
        root.join("tests/server.test.ts"),
        "import { handleOrder } from '../src/server';\ntest('order', () => expect(handleOrder('42')).toBe('42'));\n",
    )
    .unwrap_or_else(|error| panic!("test fixture write failed: {error}"));
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"cli-surface","private":true,"scripts":{"test":"vitest run"},"dependencies":{"express":"5.0.0"},"devDependencies":{"vitest":"3.0.0"}}"#,
    )
    .unwrap_or_else(|error| panic!("manifest fixture write failed: {error}"));
}

fn seed_inline_llm_credential(root: &Path) {
    let path = root.join(".cartograph/config.json");
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("LLM fixture config read failed: {error}"));
    let mut config: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("LLM fixture config parse failed: {error}"));
    let summarize = config
        .get_mut("llm")
        .and_then(Value::as_object_mut)
        .and_then(|llm| llm.get_mut("summarizeLlm"))
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("LLM fixture summarize tier is missing"));
    summarize.insert(
        "apiKey".to_owned(),
        Value::String("cartograph-live-cli-test-key".to_owned()),
    );
    let encoded = serde_json::to_vec_pretty(&config)
        .unwrap_or_else(|error| panic!("LLM fixture config encode failed: {error}"));
    std::fs::write(path, encoded)
        .unwrap_or_else(|error| panic!("LLM fixture config write failed: {error}"));
}

fn all_succeed(root: &Path, commands: &[Vec<&str>]) {
    for arguments in commands {
        success(root, arguments);
    }
}

fn all_fail(root: &Path, commands: &[Vec<&str>]) {
    for arguments in commands {
        failure(root, arguments);
    }
}

fn success(root: &Path, arguments: &[&str]) -> Output {
    let output = invoke(root, arguments);
    assert!(
        output.status.success(),
        "cartograph {arguments:?} failed with status {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn failure(root: &Path, arguments: &[&str]) -> Output {
    let output = invoke(root, arguments);
    assert!(
        !output.status.success(),
        "cartograph {arguments:?} unexpectedly succeeded"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("invalid-retired-database"));
    output
}

fn json_success(root: &Path, arguments: &[&str]) -> Value {
    let output = success(root, arguments);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "cartograph {arguments:?} returned invalid JSON: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn invoke(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cartograph"))
        .arg("--no-color")
        .args(arguments)
        .current_dir(root)
        .env("CARTOGRAPH_DATABASE_URL", "invalid-retired-database")
        .env("DOCKER_HOST", "unix:///no-native-test-docker.sock")
        .env_remove("CARTOGRAPH_LIVE_CLI_MISSING_KEY")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("cartograph process failed to start: {error}"))
}

fn symbol_id(value: &Value, expected_name: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            let name = object
                .get("qualified_name")
                .or_else(|| object.get("qualifiedName"))
                .or_else(|| object.get("simple_name"))
                .or_else(|| object.get("simpleName"))
                .and_then(Value::as_str);
            if name == Some(expected_name)
                && let Some(id) = object
                    .get("symbol_id")
                    .or_else(|| object.get("symbolId"))
                    .and_then(Value::as_str)
            {
                return Some(id.to_owned());
            }
            object
                .values()
                .find_map(|child| symbol_id(child, expected_name))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| symbol_id(child, expected_name)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn git(root: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap_or_else(|error| panic!("git fixture command failed: {error}"));
    assert!(
        status.success(),
        "git fixture command failed: {arguments:?}"
    );
}
