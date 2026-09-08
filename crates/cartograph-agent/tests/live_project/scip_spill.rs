use super::{IndexOptions, ProjectCancellation, ProjectRuntime};

pub(super) async fn assert_overlay_storage_parity(
    runtime: &ProjectRuntime,
    root: &std::path::Path,
) {
    let mut expected = None;
    let mut expected_centrality = None;
    for storage in ["memory", "postgres"] {
        std::fs::write(root.join(".cartograph/config.json"), format!(
            r#"{{"generationStorage":"{storage}","enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}}"#
        )).unwrap_or_else(|error| panic!("SCIP storage policy failed: {error}"));
        for workers in [1, 2, 4, 8, 16] {
            let report = runtime
                .index(
                    IndexOptions::default()
                        .with_force(true)
                        .with_history_refresh(false)
                        .with_max_workers(workers)
                        .unwrap_or_else(|error| panic!("SCIP workers failed: {error}")),
                )
                .await
                .unwrap_or_else(|error| panic!("SCIP {storage}/{workers} failed: {error}"));
            assert_eq!(
                report
                    .native
                    .as_ref()
                    .and_then(|native| native.scip_overlay)
                    .map(|overlay| overlay.covered_documents),
                Some(1)
            );
            let current = runtime
                .database()
                .project_snapshot_by_root(runtime.root_identity())
                .await
                .unwrap_or_else(|error| panic!("SCIP snapshot failed: {error}"))
                .and_then(|snapshot| snapshot.current)
                .unwrap_or_else(|| panic!("SCIP current missing"));
            let scores = centrality(runtime, &report).await;
            if let Some(expected) = &expected_centrality {
                assert_eq!(&scores, expected, "SCIP centrality {storage}/{workers}");
            } else {
                expected_centrality = Some(scores);
            }
            if let Some(digest) = &expected {
                assert_eq!(&current.content_digest, digest, "SCIP {storage}/{workers}");
            } else {
                expected = Some(current.content_digest);
            }
            let cancellation = ProjectCancellation::new();
            cancellation.cancel();
            assert!(
                runtime
                    .index_with_cancellation(IndexOptions::default().with_force(true), cancellation)
                    .await
                    .is_err()
            );
        }
    }
}

pub(super) fn large_overlay_source() -> String {
    use std::fmt::Write as _;
    let mut source = "pub fn caller() { callee(); }\npub fn callee() {}\n".to_owned();
    for index in 0..1200 {
        writeln!(source, "pub fn overlay_batch_{index}() {{}}")
            .unwrap_or_else(|error| panic!("overlay source failed: {error}"));
    }
    source
}

async fn centrality(runtime: &ProjectRuntime, report: &super::IndexReport) -> serde_json::Value {
    let snapshot = runtime
        .database()
        .current_interchange_snapshot(super::InterchangeSnapshotRequest {
            project_id: &report.project_id,
            maximum_rows: 10_000,
            statement_timeout: super::Duration::from_secs(30),
        })
        .await
        .unwrap_or_else(|error| panic!("SCIP identities failed: {error}"));
    assert!(
        snapshot.symbols.len() > 1024,
        "fixture must cross the overlay batch boundary"
    );
    let ids = snapshot
        .symbols
        .into_iter()
        .map(|symbol| symbol.symbol_id)
        .collect::<Vec<_>>();
    let mut ranks = Vec::new();
    let mut bridges = Vec::new();
    for chunk in ids.chunks(200) {
        ranks.extend(
            runtime
                .database()
                .current_symbol_pagerank(&report.project_id, &report.generation_id, chunk)
                .await
                .unwrap_or_else(|error| panic!("SCIP ranks failed: {error}")),
        );
        bridges.extend(
            runtime
                .database()
                .current_symbol_betweenness(&report.project_id, &report.generation_id, chunk)
                .await
                .unwrap_or_else(|error| panic!("SCIP bridges failed: {error}")),
        );
    }
    assert!(ranks.iter().all(|rank| rank.score.is_some()));
    serde_json::json!({"ranks": ranks, "bridges": bridges})
}
