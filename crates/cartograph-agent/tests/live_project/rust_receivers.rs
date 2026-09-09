use cartograph_db::{CurrentGraphLookup, GraphDirection};

use super::{
    CurrentGenerationLookup, ExactTextLookup, FileTestImpactQuery, IndexOptions, NormalizedPath,
    ProjectRuntime, drop_schema, live_project_fixture,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn rust_self_callers_and_test_impact_survive_both_storage_paths_and_worker_counts() {
    let (schema, settings, project) = live_project_fixture("8");
    write_receiver_fixture(project.path());
    let runtime = ProjectRuntime::connect(project.path(), &settings)
        .await
        .unwrap_or_else(|error| panic!("receiver runtime failed: {error}"));
    let mut expected_digest = None;
    for storage in ["memory", "postgres"] {
        std::fs::write(project.path().join(".cartograph/config.json"), format!(
            r#"{{"generationStorage":"{storage}","maxSpillBytes":67108864,"maxSpillRows":1000000,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}}"#
        )).unwrap_or_else(|error| panic!("receiver policy fixture failed: {error}"));
        for workers in [1, 2, 4, 8, 16] {
            let report = runtime
                .index(
                    IndexOptions::default()
                        .with_force(true)
                        .with_history_refresh(false)
                        .with_max_workers(workers)
                        .unwrap_or_else(|error| panic!("receiver worker policy failed: {error}")),
                )
                .await
                .unwrap_or_else(|error| panic!("receiver index failed: {error}"));
            let snapshot = runtime
                .database()
                .project_snapshot_by_root(runtime.root_identity())
                .await
                .unwrap_or_else(|error| panic!("receiver snapshot failed: {error}"))
                .and_then(|snapshot| snapshot.current)
                .unwrap_or_else(|| panic!("receiver generation missing"));
            if let Some(expected) = &expected_digest {
                assert_eq!(&snapshot.content_digest, expected, "{storage}/{workers}");
            } else {
                expected_digest = Some(snapshot.content_digest.clone());
            }
            let generation =
                CurrentGenerationLookup::new(&report.project_id, &report.generation_id);
            let targets = runtime
                .database()
                .exact_current_symbols_by_name(ExactTextLookup::new(
                    generation,
                    "Runtime::scan",
                    10,
                ))
                .await
                .unwrap_or_else(|error| panic!("receiver target lookup failed: {error}"));
            assert_eq!(targets.len(), 1);
            let edges = runtime
                .database()
                .current_graph_edges(CurrentGraphLookup::new(
                    generation,
                    std::slice::from_ref(targets[0].symbol_id()),
                    GraphDirection::Incoming,
                ))
                .await
                .unwrap_or_else(|error| panic!("receiver callers failed: {error}"));
            let mut counts = edges
                .iter()
                .filter(|edge| edge.edge_kind() == "calls")
                .map(cartograph_db::CurrentGraphEdge::site_count)
                .collect::<Vec<_>>();
            counts.sort_unstable();
            assert_eq!(counts, [1, 2], "{storage}/{workers}");
            let path = NormalizedPath::parse("src/lib.rs")
                .unwrap_or_else(|error| panic!("receiver path failed: {error}"));
            let impact = runtime
                .database()
                .current_file_test_impact(FileTestImpactQuery {
                    project_id: &report.project_id,
                    paths: &[path],
                    max_depth: 5,
                    max_nodes: 40,
                    limit: 40,
                    test_path_regex: None,
                })
                .await
                .unwrap_or_else(|error| panic!("receiver test impact failed: {error}"))
                .unwrap_or_else(|| panic!("receiver impact generation missing"));
            assert!(
                impact
                    .tests()
                    .iter()
                    .any(|test| test.path() == "src/child.rs")
            );
            assert!(!impact.nodes_truncated());
        }
    }
    runtime.close().await;
    drop_schema(&settings, &schema).await;
}

fn write_receiver_fixture(root: &std::path::Path) {
    for directory in ["src", ".cartograph"] {
        std::fs::create_dir_all(root.join(directory))
            .unwrap_or_else(|error| panic!("receiver fixture directory failed: {error}"));
    }
    for (path, source) in [
        (
            "src/lib.rs",
            "mod child; pub struct Runtime; impl Runtime { fn scan(&self) {} } pub struct Other; impl Other { pub fn scan(&self) {} }",
        ),
        (
            "src/child.rs",
            "use crate::Runtime; impl Runtime { fn symbol_context(&self) { self.scan(); self.scan(); } fn file_context(&self) { self.scan(); } } #[test] fn reads_context() { Runtime.file_context(); }",
        ),
    ] {
        std::fs::write(root.join(path), source)
            .unwrap_or_else(|error| panic!("receiver source fixture failed: {error}"));
    }
}
