use std::assert_matches;

use super::{
    CurrentGenerationLookup, ExactTextLookup, IndexOptions, ProjectCancellation, ProjectError,
    ProjectRuntime, SourceContextOptions, SourceContextRequest, SymbolId, drop_schema,
    live_project_fixture,
};

fn requests(ids: &[SymbolId]) -> Vec<SourceContextRequest> {
    ids.iter()
        .map(|id| SourceContextRequest::new(id.clone(), SourceContextOptions::default()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn source_batches_scan_once_and_preserve_source_and_generation_fences() {
    let (schema, settings, project) = live_project_fixture("8");
    for index in 0..12 {
        std::fs::write(
            project.path().join(format!("file{index}.rs")),
            format!("pub fn window{index}() {{}}\n"),
        )
        .unwrap_or_else(|error| panic!("source batch fixture failed: {error}"));
    }
    let runtime = ProjectRuntime::connect(project.path(), &settings)
        .await
        .unwrap_or_else(|error| panic!("source batch runtime failed: {error}"));
    let report = runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|error| panic!("source batch index failed: {error}"));
    let ids = source_window_ids(&runtime, &report).await;
    let before = runtime.source_scan_observations();
    let batch = runtime
        .source_context_batch_with_cancellation(
            &report.generation_id,
            requests(&ids),
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("source batch failed: {error}"));
    assert_eq!(runtime.source_scan_observations(), before + 1);
    assert_eq!(batch.len(), ids.len());
    let value = serde_json::to_value(&batch)
        .unwrap_or_else(|error| panic!("source batch JSON failed: {error}"));
    for index in 0..12 {
        assert_eq!(value[index]["fresh"], true);
        assert_eq!(
            value[index]["excerpt"]["text"],
            format!("pub fn window{index}() {{}}\n")
        );
        assert_eq!(
            value[index]["liveSourceRevision"],
            value[0]["liveSourceRevision"]
        );
    }

    std::fs::write(project.path().join("file0.rs"), "pub fn replaced() {}\n")
        .unwrap_or_else(|error| panic!("source batch edit failed: {error}"));
    let stale = runtime
        .source_context_batch_with_cancellation(
            &report.generation_id,
            requests(&ids),
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("stale source batch failed: {error}"));
    let stale = serde_json::to_value(stale)
        .unwrap_or_else(|error| panic!("stale source batch JSON failed: {error}"));
    assert_eq!(stale[0]["fresh"], false);
    assert!(stale[0]["excerpt"].is_null());
    assert_eq!(stale[1]["fresh"], false);
    assert_eq!(stale[1]["excerpt"]["text"], "pub fn window1() {}\n");

    let cancellation = ProjectCancellation::new();
    cancellation.cancel();
    let before = runtime.source_scan_observations();
    assert_matches!(
        runtime
            .source_context_batch_with_cancellation(
                &report.generation_id,
                requests(&ids),
                cancellation
            )
            .await,
        Err(ProjectError::RequestCancelled)
    );
    assert_eq!(runtime.source_scan_observations(), before);
    runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|error| panic!("source batch reindex failed: {error}"));
    let before = runtime.source_scan_observations();
    assert_matches!(
        runtime
            .source_context_batch_with_cancellation(
                &report.generation_id,
                requests(&ids),
                ProjectCancellation::new()
            )
            .await,
        Err(ProjectError::SourceContextUnavailable)
    );
    assert_eq!(runtime.source_scan_observations(), before);
    runtime.close().await;
    drop_schema(&settings, &schema).await;
}

async fn source_window_ids(runtime: &ProjectRuntime, report: &super::IndexReport) -> Vec<SymbolId> {
    let mut ids = Vec::new();
    for index in 0..12 {
        let symbols = runtime
            .database()
            .exact_current_symbols_by_name(ExactTextLookup::new(
                CurrentGenerationLookup::new(&report.project_id, &report.generation_id),
                &format!("window{index}"),
                10,
            ))
            .await
            .unwrap_or_else(|error| panic!("source batch lookup failed: {error}"));
        assert_eq!(symbols.len(), 1);
        ids.push(symbols[0].symbol_id().clone());
    }
    ids
}
