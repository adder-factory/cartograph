//! Live CLI integration coverage across the public command surface.

mod dependency_ownership;

use std::{
    fmt::Write as _,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::Path,
    process::{Command, Output},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cartograph_db::{CartographDatabase, LeaseOwner, LeaseRequest, LeaseTarget};
use cartograph_domain::{ProjectId, ProjectOperation};
use serde_json::Value;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

#[test]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
fn public_cli_exercises_native_agent_backend_and_optional_llm_routes() {
    let database_url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("live CLI database is not configured"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_cli_public_surface_{}_{}", std::process::id(), nanos);
    let read_only_schema = format!("{schema}_read_only");
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    let missing_model = project
        .path()
        .join(".cartograph/missing-model.gguf")
        .to_string_lossy()
        .into_owned();

    let scenario = LiveCliScenario {
        project: &project,
        database_url: &database_url,
        schema: &schema,
        read_only_schema: &read_only_schema,
        project_path: &project_path,
        missing_model: &missing_model,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| run_live_cli_scenario(&scenario)));

    cleanup_schema(&database_url, &schema);
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
fn doctor_exposes_layered_readiness_before_the_first_generation() {
    let database_url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("live CLI database is not configured"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_cli_doctor_layers_{}_{}", std::process::id(), nanos);
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    write_project_fixture(project.path());

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        json_success(
            project.path(),
            &database_url,
            &schema,
            &[
                "llm",
                "install",
                &project_path,
                "--no-models",
                "--minimal",
                "--json",
            ],
        );
        let report = json_success(
            project.path(),
            &database_url,
            &schema,
            &["doctor", &project_path, "--json"],
        );
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
            &database_url,
            &schema,
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

        let text = success(
            project.path(),
            &database_url,
            &schema,
            &["doctor", &project_path],
        );
        let rendered = String::from_utf8_lossy(&text.stdout);
        assert!(rendered.contains("Readiness layers:"));
        assert!(rendered.contains("- overall onboarding: incomplete"));
        assert!(rendered.contains("Next actions:"));

        let encoded = serde_json::to_string(&report)
            .unwrap_or_else(|error| panic!("doctor report encoding failed: {error}"));
        assert!(!encoded.contains(&project_path));
        assert!(!encoded.contains(&database_url));
    }));

    cleanup_schema(&database_url, &schema);
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
fn doctor_warns_for_uninitialized_behind_schema_and_fails_for_real_state() {
    let database_url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("live CLI database is not configured"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_cli_doctor_behind_{}_{}", std::process::id(), nanos);
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        prepare_schema_one_version_behind(&database_url, &schema);

        let uninitialized = json_success(
            project.path(),
            &database_url,
            &schema,
            &["doctor", &project_path, "--json"],
        );
        assert_eq!(uninitialized["ready"], true);
        assert_eq!(
            doctor_check(&uninitialized, "project-state")["status"],
            "warn"
        );
        assert_eq!(
            doctor_check(&uninitialized, "project-index")["status"],
            "warn"
        );
        assert!(doctor_check_optional(&uninitialized, "schema-migrations").is_none());

        std::fs::create_dir(project.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("Cartograph fixture directory failed: {error}"));
        let initialized = invoke(
            project.path(),
            &database_url,
            &schema,
            &["doctor", &project_path, "--json"],
        );
        assert_eq!(initialized.status.code(), Some(2));
        let initialized: Value = serde_json::from_slice(&initialized.stdout)
            .unwrap_or_else(|error| panic!("doctor failure report was not JSON: {error}"));
        assert_eq!(initialized["ready"], false);
        let migration = doctor_check(&initialized, "schema-migrations");
        assert_eq!(migration["status"], "fail");
        assert!(
            migration["message"]
                .as_str()
                .is_some_and(|message| message.contains(
                    "database schema version 44 is below required version 45; next pending migration is 45"
                ))
        );
    }));

    cleanup_schema(&database_url, &schema);
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
fn parse_failure_json_names_relative_input_and_preserves_the_current_generation() {
    let database_url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("live CLI database is not configured"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_cli_parse_failure_{}_{}", std::process::id(), nanos);
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    write_project_fixture(project.path());

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let initial = json_success(
            project.path(),
            &database_url,
            &schema,
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
            &database_url,
            &schema,
            &["index", &project_path, "--format", "json"],
        );
        assert!(output.stdout.is_empty());
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
            &database_url,
            &schema,
            &["status", &project_path, "--format", "json"],
        );
        assert_eq!(
            status["project"]["snapshot"]["current"]["generation_id"],
            initial_generation
        );
        assert_eq!(status["project"]["fresh"], false);
    }));

    cleanup_schema(&database_url, &schema);
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[test]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
fn sync_if_dirty_waits_for_a_competing_index_lease() {
    let database_url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("live CLI database is not configured"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let schema = format!("cg_cli_sync_lease_{}_{}", std::process::id(), nanos);
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    let project_path = project.path().to_string_lossy().into_owned();
    write_project_fixture(project.path());

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_sync_if_dirty_lease_scenario(&project, &database_url, &schema, &project_path);
    }));

    cleanup_schema(&database_url, &schema);
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

fn run_sync_if_dirty_lease_scenario(
    project: &tempfile::TempDir,
    database_url: &str,
    schema: &str,
    project_path: &str,
) {
    git(project.path(), &["init", "--initial-branch=main"]);
    git(
        project.path(),
        &["config", "user.email", "sync-lease@example.invalid"],
    );
    git(
        project.path(),
        &["config", "user.name", "Sync Lease Fixture"],
    );
    git(project.path(), &["add", "."]);
    git(project.path(), &["commit", "-m", "establish sync fixture"]);

    let indexed = json_success(
        project.path(),
        database_url,
        schema,
        &["index", project_path, "--format", "json"],
    );
    let project_id = indexed["project_id"]
        .as_str()
        .and_then(|project_id| ProjectId::parse(project_id).ok())
        .unwrap_or_else(|| panic!("index report omitted a valid project identity: {indexed}"));
    std::fs::write(
        project.path().join("src/index.ts"),
        "export const marker = 2;\n",
    )
    .unwrap_or_else(|error| panic!("stale source fixture write failed: {error}"));

    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let lease_database_url = database_url.to_owned();
    let lease_schema = schema.to_owned();
    let lease_holder = thread::spawn(move || {
        let settings = cartograph_config::DatabaseSettings::parse(
            &lease_database_url,
            Some("2"),
            Some("10000"),
        )
        .and_then(|settings| settings.with_schema(&lease_schema))
        .unwrap_or_else(|error| panic!("lease settings failed: {error}"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("lease runtime failed: {error}"));
        runtime.block_on(async {
            let pool = cartograph_db::connect(&settings)
                .await
                .unwrap_or_else(|error| panic!("lease connection failed: {error}"));
            let database = CartographDatabase::new(pool, settings.schema().clone());
            let lease = database
                .acquire_lease(LeaseRequest::new(
                    LeaseTarget::new(project_id, ProjectOperation::Index, None),
                    LeaseOwner::new(std::process::id(), "live-cli-competing-sync"),
                    Duration::from_secs(5),
                ))
                .await
                .unwrap_or_else(|error| panic!("competing lease acquisition failed: {error}"));
            ready_sender
                .send(())
                .unwrap_or_else(|error| panic!("lease-ready signal failed: {error}"));
            tokio::time::sleep(Duration::from_millis(750)).await;
            database
                .release_lease(&lease)
                .await
                .unwrap_or_else(|error| panic!("competing lease release failed: {error}"));
            database.close().await;
        });
    });
    ready_receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("competing lease did not become ready: {error}"));

    let sync = invoke(
        project.path(),
        database_url,
        schema,
        &["sync-if-dirty", project_path],
    );
    lease_holder
        .join()
        .unwrap_or_else(|payload| resume_unwind(payload));
    assert!(
        sync.status.success(),
        "sync-if-dirty failed instead of waiting: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        String::from_utf8_lossy(&sync.stdout)
            .contains("Synced changed source into a new generation")
    );
    let status = json_success(
        project.path(),
        database_url,
        schema,
        &["status", project_path, "--json"],
    );
    assert_eq!(status.pointer("/project/fresh"), Some(&Value::Bool(true)));
}

#[derive(Clone, Copy)]
struct LiveCliScenario<'a> {
    project: &'a tempfile::TempDir,
    database_url: &'a str,
    schema: &'a str,
    read_only_schema: &'a str,
    project_path: &'a str,
    missing_model: &'a str,
}

struct NativeSymbols {
    root_id: String,
    leaf_id: String,
}

fn run_live_cli_scenario(scenario: &LiveCliScenario<'_>) {
    ensure_pgstattuple(scenario.database_url, scenario.schema);
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
    let project = scenario.project;
    let database_url = scenario.database_url;
    let read_only_schema = scenario.read_only_schema;
    let project_path = scenario.project_path;
    write_project_fixture(project.path());
    git(project.path(), &["init", "--initial-branch=main"]);
    git(
        project.path(),
        &["config", "user.email", "cli-surface@example.invalid"],
    );
    git(
        project.path(),
        &["config", "user.name", "CLI Surface Fixture"],
    );
    git(project.path(), &["add", "."]);
    git(
        project.path(),
        &["commit", "-m", "CG-99 establish public CLI surface"],
    );

    failure(
        project.path(),
        database_url,
        read_only_schema,
        &[
            "db",
            "usage",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    failure(
        project.path(),
        database_url,
        read_only_schema,
        &[
            "db",
            "compact",
            "--heap",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    failure(
        project.path(),
        database_url,
        read_only_schema,
        &[
            "db",
            "compact",
            "--project-path",
            project_path,
            "--format",
            "json",
        ],
    );
    assert!(
        !schema_exists(database_url, read_only_schema),
        "read-only database usage created its missing schema"
    );
}

fn index_and_verify_storage(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    success(
        project.path(),
        database_url,
        schema,
        &["index", project_path, "--workers", "2", "--format", "json"],
    );
    let status = json_success(
        project.path(),
        database_url,
        schema,
        &["status", project_path, "--json", "--verbose"],
    );
    let database_storage = &status["databaseStorage"];
    assert_eq!(database_storage["state"], "ready");
    assert!(
        database_storage["databaseBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        database_storage["schemaBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    for field in ["database", "schema", "heap", "index", "btreeIndex", "toast"] {
        assert!(
            database_storage["humanReadable"][field]
                .as_str()
                .is_some_and(|display| display.ends_with('B')),
            "status omitted the human-readable {field} allocation"
        );
    }
    success(
        project.path(),
        database_url,
        schema,
        &[
            "db",
            "usage",
            "--project-path",
            project_path,
            "--limit",
            "16",
            "--format",
            "json",
        ],
    );
    verify_compaction_surfaces(scenario);
}

fn verify_compaction_surfaces(scenario: &LiveCliScenario<'_>) {
    let commands = [
        vec![
            "db",
            "compact",
            "--project-path",
            scenario.project_path,
            "--maximum-indexes",
            "2",
            "--minimum-index-bytes",
            "1048576",
            "--timeout-seconds",
            "60",
            "--format",
            "json",
        ],
        vec![
            "db",
            "compact",
            "--heap",
            "--project-path",
            scenario.project_path,
            "--maximum-relations",
            "2",
            "--minimum-reclaimable-bytes",
            "1",
            "--timeout-seconds",
            "60",
            "--format",
            "json",
        ],
        vec![
            "db",
            "compact",
            "--project-path",
            scenario.project_path,
            "--apply",
            "--confirm",
            "compact-online-indexes",
            "--maximum-indexes",
            "2",
            "--minimum-index-bytes",
            "1048576",
            "--timeout-seconds",
            "60",
            "--available-headroom-bytes",
            "1073741824",
            "--format",
            "json",
        ],
    ];
    all_succeed(
        scenario.project.path(),
        scenario.database_url,
        scenario.schema,
        &commands,
    );
}

fn verify_native_retrieval(scenario: &LiveCliScenario<'_>) -> NativeSymbols {
    let project = scenario.project;
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    let root = json_success(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
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
            database_url,
            schema,
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
            database_url,
            schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    success(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    let root_id = &symbols.root_id;
    let leaf_id = &symbols.leaf_id;
    for direction in ["callers", "callees", "both", "impact"] {
        success(
            project.path(),
            database_url,
            schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
        &["export", project_path, "--format", "json", "--limit", "100"],
    );
}

fn verify_agent_compatibility_surfaces(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    let find_text = success(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
                "status",
                "--project-path",
                project_path,
                "--port",
                "55432",
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
            vec!["sql", "--schema", "--project-path", project_path],
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
            vec![
                "db",
                "stop",
                "--project-path",
                project_path,
                "--port",
                "55432",
            ],
        ],
    );
}

fn verify_invalid_agent_inputs(scenario: &LiveCliScenario<'_>) {
    let project = scenario.project;
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_fail(
        project.path(),
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    let missing_backup = project
        .path()
        .join("missing.backup")
        .to_string_lossy()
        .into_owned();
    all_fail(
        project.path(),
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    let missing_model = scenario.missing_model;
    success(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
        &["llm", "migrate-credentials", project_path, "--json"],
    );
    seed_inline_llm_credential(project.path());
    all_fail(
        project.path(),
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    all_succeed(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
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
    let database_url = scenario.database_url;
    let schema = scenario.schema;
    let project_path = scenario.project_path;
    success(
        project.path(),
        database_url,
        schema,
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
        database_url,
        schema,
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

fn all_succeed(root: &Path, database_url: &str, schema: &str, commands: &[Vec<&str>]) {
    for arguments in commands {
        success(root, database_url, schema, arguments);
    }
}

fn all_fail(root: &Path, database_url: &str, schema: &str, commands: &[Vec<&str>]) {
    for arguments in commands {
        failure(root, database_url, schema, arguments);
    }
}

fn success(root: &Path, database_url: &str, schema: &str, arguments: &[&str]) -> Output {
    let output = invoke(root, database_url, schema, arguments);
    assert!(
        output.status.success(),
        "cartograph {arguments:?} failed with status {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn failure(root: &Path, database_url: &str, schema: &str, arguments: &[&str]) -> Output {
    let output = invoke(root, database_url, schema, arguments);
    assert!(
        !output.status.success(),
        "cartograph {arguments:?} unexpectedly succeeded"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(database_url),
        "a failing CLI command exposed the database URL"
    );
    output
}

fn json_success(root: &Path, database_url: &str, schema: &str, arguments: &[&str]) -> Value {
    let output = success(root, database_url, schema, arguments);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "cartograph {arguments:?} returned invalid JSON: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn invoke(root: &Path, database_url: &str, schema: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cartograph"))
        .arg("--no-color")
        .args(arguments)
        .current_dir(root)
        .env("CARTOGRAPH_DATABASE_URL", database_url)
        .env("CARTOGRAPH_DATABASE_SCHEMA", schema)
        .env("CARTOGRAPH_DATABASE_MAX_CONNECTIONS", "8")
        .env("CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS", "10000")
        .env_remove("CARTOGRAPH_LIVE_CLI_MISSING_KEY")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("cartograph process failed to start: {error}"))
}

fn prepare_schema_one_version_behind(database_url: &str, schema: &str) {
    const GENERATION_FACT_COUNTS_SCHEMA_VERSION: i64 = 45;
    let settings =
        cartograph_config::DatabaseSettings::parse(database_url, Some("2"), Some("10000"))
            .and_then(|settings| settings.with_schema(schema))
            .unwrap_or_else(|error| panic!("schema-behind settings failed: {error}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("schema-behind runtime failed: {error}"));
    runtime.block_on(async {
        assert_eq!(
            cartograph_db::latest_schema_version(),
            GENERATION_FACT_COUNTS_SCHEMA_VERSION,
            "schema-behind fixture must track the current migration"
        );
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("schema-behind connection failed: {error}"));
        let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
        database
            .migrate()
            .await
            .unwrap_or_else(|error| panic!("schema-behind migration failed: {error}"));
        for statement in [
            format!(
                r#"ALTER TABLE "{schema}"."index_generations"
                    DROP CONSTRAINT index_generations_fact_counts_check,
                    DROP COLUMN fact_files, DROP COLUMN fact_symbols, DROP COLUMN fact_edges,
                    DROP COLUMN fact_references, DROP COLUMN fact_numerical_sites,
                    DROP COLUMN fact_documents, DROP COLUMN fact_source_bytes"#
            ),
            format!(r#"DROP TABLE "{schema}"."history_refreshes""#),
        ] {
            query(AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("schema-behind rollback failed: {error}"));
        }
        query(AssertSqlSafe(format!(
            r#"DELETE FROM "{schema}"."schema_migrations" WHERE version = $1"#
        )))
        .bind(GENERATION_FACT_COUNTS_SCHEMA_VERSION)
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("schema-behind ledger rollback failed: {error}"));
        database.close().await;
    });
}

fn doctor_check<'report>(report: &'report Value, id: &str) -> &'report Value {
    doctor_check_optional(report, id)
        .unwrap_or_else(|| panic!("doctor report omitted the {id} check: {report}"))
}

fn doctor_check_optional<'report>(report: &'report Value, id: &str) -> Option<&'report Value> {
    report["checks"]
        .as_array()?
        .iter()
        .find(|check| check["id"] == id)
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

fn cleanup_schema(database_url: &str, schema: &str) {
    let settings =
        cartograph_config::DatabaseSettings::parse(database_url, Some("2"), Some("10000"))
            .and_then(|settings| settings.with_schema(schema))
            .unwrap_or_else(|error| panic!("cleanup settings failed: {error}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("cleanup runtime failed: {error}"));
    runtime.block_on(async {
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("cleanup connection failed: {error}"));
        query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
        )))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("cleanup failed: {error}"));
        pool.close().await;
    });
}

fn ensure_pgstattuple(database_url: &str, schema: &str) {
    let settings =
        cartograph_config::DatabaseSettings::parse(database_url, Some("2"), Some("10000"))
            .and_then(|settings| settings.with_schema(schema))
            .unwrap_or_else(|error| panic!("pgstattuple settings failed: {error}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("pgstattuple runtime failed: {error}"));
    runtime.block_on(async {
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("pgstattuple connection failed: {error}"));
        query("CREATE EXTENSION IF NOT EXISTS pgstattuple")
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("pgstattuple installation failed: {error}"));
        pool.close().await;
    });
}

fn schema_exists(database_url: &str, schema: &str) -> bool {
    let settings =
        cartograph_config::DatabaseSettings::parse(database_url, Some("2"), Some("10000"))
            .and_then(|settings| settings.with_schema(schema))
            .unwrap_or_else(|error| panic!("schema inspection settings failed: {error}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("schema inspection runtime failed: {error}"));
    runtime.block_on(async {
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("schema inspection connection failed: {error}"));
        let exists =
            query("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1)")
                .bind(schema)
                .fetch_one(&pool)
                .await
                .and_then(|row| row.try_get::<bool, _>(0))
                .unwrap_or_else(|error| panic!("schema inspection failed: {error}"));
        pool.close().await;
        exists
    })
}
