use std::fmt::Write as _;

use super::{
    Duration, IndexOptions, IndexReport, ProjectRuntime, drop_schema, live_project_fixture,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector; timings are observations, never gates"]
async fn repeated_edit_workload_matches_clean_rebuilds_and_reports_stage_costs() {
    let mut observations = Vec::new();
    for storage in ["memory", "postgres"] {
        let (schema, settings, project) = live_project_fixture("8");
        write_fixture(project.path(), storage);
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("workload connect: {error}"));
        let initial = index(&runtime, false).await;
        observations.push(observe(&runtime, storage, "cold", &initial).await);
        let no_op = index(&runtime, false).await;
        assert!(!no_op.published);
        observations.push(observe(&runtime, storage, "no_op", &no_op).await);
        for edit in ["body", "rename", "delete_export", "admission_policy"] {
            apply_edit(project.path(), edit, storage);
            let cached = index(&runtime, false).await;
            assert!(
                cached.published,
                "{storage}/{edit} must publish changed source/policy"
            );
            if edit == "body" {
                let cache = &cached
                    .native
                    .as_ref()
                    .unwrap_or_else(|| panic!("workload native report missing"))
                    .parse_cache;
                assert!(
                    cache.hits >= 60,
                    "unchanged files should reuse parse payloads"
                );
                assert_eq!(cache.parsed_files, 1);
            }
            let digest = current_digest(&runtime).await;
            observations.push(observe(&runtime, storage, edit, &cached).await);
            let clean = index(&runtime, true).await;
            assert!(clean.published);
            assert_eq!(
                current_digest(&runtime).await,
                digest,
                "{storage}/{edit}: cache reuse must equal a clean parse and full resolution"
            );
        }
        runtime.close().await;
        drop_schema(&settings, &schema).await;
    }
    println!(
        "ARCHITECTURE_WORKLOAD_V1 {}",
        serde_json::json!({
            "fixture": "rust-repeated-edit-v1", "files": 65, "workers": 4,
            "retention": {"keepSuperseded": 2}, "samplesPerCase": 1,
            "timingsAreGates": false, "observations": observations,
        })
    );
}

fn write_fixture(root: &std::path::Path, storage: &str) {
    for directory in ["src", ".cartograph"] {
        std::fs::create_dir_all(root.join(directory))
            .unwrap_or_else(|error| panic!("workload directory: {error}"));
    }
    let mut modules = String::new();
    for index in 0..64 {
        writeln!(modules, "mod leaf_{index:02};")
            .unwrap_or_else(|error| panic!("workload module: {error}"));
        let mut source = format!("pub fn target_{index}(value: u32) -> u32 {{ value + 1 }}\n");
        for caller in 0..10 {
            writeln!(
                source,
                "pub fn caller_{index}_{caller}(value: u32) -> u32 {{ target_{index}(value) }}"
            )
            .unwrap_or_else(|error| panic!("workload source: {error}"));
        }
        std::fs::write(root.join(format!("src/leaf_{index:02}.rs")), source)
            .unwrap_or_else(|error| panic!("workload file: {error}"));
    }
    modules.push_str("pub fn entry(value: u32) -> u32 { leaf_00::target_0(value) }\n");
    std::fs::write(root.join("src/lib.rs"), modules)
        .unwrap_or_else(|error| panic!("workload root: {error}"));
    write_policy(root, storage, false);
}

fn write_policy(root: &std::path::Path, storage: &str, exclude: bool) {
    let exclude = if exclude {
        vec!["src/leaf_63.rs"]
    } else {
        Vec::new()
    };
    let config = serde_json::json!({"generationStorage": storage, "exclude": exclude,
        "enableChurn": false, "enableCoChange": false, "enableIssueHistory": false});
    std::fs::write(root.join(".cartograph/config.json"), config.to_string())
        .unwrap_or_else(|error| panic!("workload policy: {error}"));
}

fn apply_edit(root: &std::path::Path, edit: &str, storage: &str) {
    match edit {
        "body" => replace(root, "src/leaf_00.rs", "value + 1", "value + 2"),
        "rename" => {
            std::fs::rename(root.join("src/leaf_00.rs"), root.join("src/renamed.rs"))
                .unwrap_or_else(|error| panic!("workload rename: {error}"));
            replace(root, "src/lib.rs", "leaf_00", "renamed");
        }
        "delete_export" => replace(
            root,
            "src/renamed.rs",
            "pub fn target_0(value: u32) -> u32 { value + 2 }",
            "",
        ),
        "admission_policy" => write_policy(root, storage, true),
        _ => panic!("unknown workload edit"),
    }
}

fn replace(root: &std::path::Path, path: &str, from: &str, to: &str) {
    let path = root.join(path);
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("workload read: {error}"));
    assert!(source.contains(from));
    std::fs::write(path, source.replace(from, to))
        .unwrap_or_else(|error| panic!("workload edit: {error}"));
}

async fn index(runtime: &ProjectRuntime, force: bool) -> IndexReport {
    runtime
        .index(
            IndexOptions::default()
                .with_force(force)
                .with_profile(true)
                .with_history_refresh(false)
                .with_max_workers(4)
                .unwrap_or_else(|error| panic!("workload workers: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("workload index: {error}"))
}

async fn current_digest(runtime: &ProjectRuntime) -> super::ContentDigest {
    runtime
        .database()
        .project_snapshot_by_root(runtime.root_identity())
        .await
        .unwrap_or_else(|error| panic!("workload snapshot: {error}"))
        .and_then(|snapshot| snapshot.current)
        .unwrap_or_else(|| panic!("workload current missing"))
        .content_digest
}

async fn observe(
    runtime: &ProjectRuntime,
    storage: &str,
    case: &str,
    report: &IndexReport,
) -> serde_json::Value {
    let usage = runtime
        .database()
        .storage_usage(&report.project_id, 128, Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("workload storage: {error}"));
    serde_json::json!({"storage": storage, "case": case, "profile": report.profile,
        "parseCache": report.native.as_ref().map(|native| native.parse_cache), "usage": usage})
}
