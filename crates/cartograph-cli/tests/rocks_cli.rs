//! Black-box native CLI proof. Invalid PostgreSQL/Docker settings catch cross-routing.

mod dependency_ownership;

use serde_json::Value;
use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Output},
};

type TestResult = Result<(), Box<dyn Error>>;

fn cli(root: &Path, arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_cartograph"))
        .args(arguments)
        .current_dir(root)
        .env(
            "CARTOGRAPH_DATABASE_URL",
            "invalid-postgresql-must-not-be-used",
        )
        .env("DOCKER_HOST", "unix:///cartograph-rocks-cli-no-docker.sock")
        .output()?)
}

fn json(root: &Path, arguments: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = cli(root, arguments)?;
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn project() -> Result<tempfile::TempDir, Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("src"))?;
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"native_cli_probe\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        root.path().join("src/lib.rs"),
        "mod provider; pub fn caller() { provider::target(); }\n",
    )?;
    fs::write(root.path().join("src/provider.rs"), "pub fn target() {}\n")?;
    Ok(root)
}

#[test]
fn rocksdb_is_default_and_a_legacy_marker_cannot_select_postgresql() -> TestResult {
    let project = project()?;
    let root = project.path();
    let indexed = json(root, &["index", "--format", "json"])?;
    assert!(indexed["generation_id"].is_string());
    assert_eq!(indexed["live_source"], "matched");
    assert!(!root.join(".cartograph/config.json").exists());
    fs::write(
        root.join(".cartograph/config.json"),
        r#"{"storageBackend":"postgresql","custom":{"preserve":true}}"#,
    )?;
    assert_eq!(json(root, &["status", "--json"])?["storage"], "rocksdb");
    assert!(!cli(root, &["db", "start"])?.status.success());
    let migrated = json(
        root,
        &["db", "migrate", "--to", "rocksdb", "--format", "json"],
    )?;
    assert_eq!(migrated["storage"], "rocksdb");
    let config: Value = serde_json::from_slice(&fs::read(root.join(".cartograph/config.json"))?)?;
    assert_eq!(config["storageBackend"], "rocksdb");
    assert_eq!(config["custom"]["preserve"], true);
    Ok(())
}

#[test]
fn native_index_tracks_oversized_files_without_extracting_them() -> TestResult {
    let project = project()?;
    let root = project.path();
    fs::create_dir(root.join(".cartograph"))?;
    fs::write(
        root.join(".cartograph/config.json"),
        r#"{"maxFileSize":1024}"#,
    )?;
    fs::write(root.join("src/oversized.rs"), "// oversized\n".repeat(512))?;
    let indexed = json(root, &["index", "--format", "json"])?;
    assert_eq!(indexed["live_source"], "matched");
    assert_eq!(indexed["native"]["files"], 3);
    assert_eq!(
        json(root, &["index", "--format", "json"])?["published"],
        false
    );
    fs::write(root.join("src/oversized.rs"), "// larger\n".repeat(1024))?;
    assert_eq!(
        json(root, &["status", "--json"])?["project"]["fresh"],
        false
    );
    assert_eq!(
        json(root, &["index", "--format", "json"])?["live_source"],
        "matched"
    );
    Ok(())
}

#[test]
fn installer_initializes_native_graph_without_a_postgresql_service() -> TestResult {
    let project = project()?;
    let root = project.path();
    let output = Command::new(env!("CARGO_BIN_EXE_cartograph"))
        .args([
            "install",
            "--yes",
            "--target",
            "codex",
            "--location",
            "local",
            "--no-hooks",
        ])
        .current_dir(root)
        .env_remove("CARTOGRAPH_DATABASE_URL")
        .env("DOCKER_HOST", "unix:///cartograph-rocks-cli-no-docker.sock")
        .output()?;
    assert!(
        output.status.success(),
        "native install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(root, &["status", "--json"])?["storage"], "rocksdb");
    assert_eq!(json(root, &["status", "--json"])?["project"]["fresh"], true);
    let host = fs::read_to_string(root.join(".codex/config.toml"))?;
    assert!(!host.contains("managed-database-port"));
    assert!(
        cli(root, &["llm", "install", "--no-models"])?
            .status
            .success()
    );
    let failure = cli(
        root,
        &[
            "llm",
            "install",
            "--no-models",
            "--database-url",
            "invalid-retired-database",
        ],
    )?;
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("unexpected argument"));
    Ok(())
}

#[test]
fn migrated_cli_reopens_current_graph_rejects_stale_and_replaces_cross_file_calls() -> TestResult {
    let project = project()?;
    let root = project.path();
    let migration = json(
        root,
        &["db", "migrate", "--to", "rocksdb", "--format", "json"],
    )?;
    assert_eq!(migration["completed"], true);
    assert_eq!(migration["storage"], "rocksdb");
    let status = json(root, &["status", "--json"])?;
    assert_eq!(status["storage"], "rocksdb");
    assert_eq!(status["project"]["fresh"], true);
    let original_generation = status["project"]["snapshot"]["current"]["generation_id"].clone();
    let caller = json(
        root,
        &["find", "caller", "--by", "name", "--format", "json"],
    )?;
    let symbol = caller["evidence"]["rows"][0]["id"]
        .as_str()
        .ok_or("caller missing")?;
    let graph = json(root, &["graph", symbol, "--direction", "callees"])?;
    let nodes = graph["evidence"]["graph"]["nodes"]
        .as_array()
        .ok_or("graph nodes missing")?;
    assert_eq!(
        nodes.iter().filter(|node| node["depth"] == 1).count(),
        1,
        "{graph}"
    );
    let noop = json(root, &["index", "--format", "json"])?;
    assert_eq!(noop["published"], false);
    assert_eq!(noop["generation_id"], original_generation);
    fs::write(root.join("src/provider.rs"), "pub fn replacement() {}\n")?;
    assert_eq!(
        json(root, &["status", "--json"])?["project"]["fresh"],
        false
    );
    assert!(
        !cli(
            root,
            &["find", "target", "--by", "name", "--format", "json"]
        )?
        .status
        .success()
    );
    let stale = json(
        root,
        &[
            "find",
            "target",
            "--by",
            "name",
            "--allow-stale",
            "--format",
            "json",
        ],
    )?;
    assert_eq!(stale["freshness"], "stale");
    assert_eq!(
        stale["evidence"]["rows"]
            .as_array()
            .ok_or("stale results missing")?
            .len(),
        1
    );
    let updated = json(root, &["index", "--format", "json"])?;
    assert_eq!(updated["live_source"], "matched");
    assert_ne!(updated["generation_id"], original_generation);
    let removed = json(
        root,
        &["find", "target", "--by", "name", "--format", "json"],
    )?;
    assert_eq!(
        removed["evidence"]["rows"]
            .as_array()
            .ok_or("removed results missing")?
            .len(),
        0
    );
    let graph = json(root, &["graph", symbol, "--direction", "callees"])?;
    let nodes = graph["evidence"]["graph"]["nodes"]
        .as_array()
        .ok_or("updated graph nodes missing")?;
    assert_eq!(
        nodes.iter().filter(|node| node["depth"] == 1).count(),
        0,
        "{graph}"
    );
    verify_native_retrieval(root)
}

fn verify_native_retrieval(root: &Path) -> TestResult {
    let lexical = json(
        root,
        &["find", "replacement", "--by", "bm25", "--format", "json"],
    )?;
    assert!(
        lexical["evidence"]
            .as_array()
            .ok_or("lexical hits missing")?
            .iter()
            .any(|hit| hit["qualified_name"] == "replacement"),
        "the native BM25 response omitted the newly published function: {lexical}"
    );
    assert_eq!(lexical["freshness"], "current");
    let context = json(
        root,
        &["context", "replacement function", "--format", "json"],
    )?;
    assert_eq!(context["freshness"], "current");
    let hybrid = json(
        root,
        &["find", "replacement", "--by", "hybrid", "--format", "json"],
    )?;
    assert_eq!(hybrid["freshness"], "current");
    assert_eq!(
        json(root, &["doctor", "--json"])?["capabilitiesReady"],
        true
    );
    Ok(())
}

#[test]
fn failed_native_migration_preserves_original_backend_and_unrelated_config() -> TestResult {
    let project = project()?;
    let root = project.path();
    fs::create_dir(root.join(".cartograph"))?;
    fs::write(
        root.join(".cartograph/config.json"),
        r#"{"maxFileSize":0,"custom":{"preserve":true}}"#,
    )?;
    let output = cli(
        root,
        &["db", "migrate", "--to", "rocksdb", "--format", "json"],
    )?;
    assert!(!output.status.success());
    let config: Value = serde_json::from_slice(&fs::read(root.join(".cartograph/config.json"))?)?;
    assert_eq!(config["custom"]["preserve"], true);
    assert!(config.get("storageBackend").is_none());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(root.to_string_lossy().as_ref()));
    Ok(())
}
