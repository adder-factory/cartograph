//! Mutable evidence shares the native owner and survives a clean reopen.

#[path = "rocks_state/cache.rs"]
mod cache;
mod dependency_ownership;

use cartograph_db::{
    McpMacroStep, McpSessionCallsQuery, McpSessionLookup, McpToolCallData, McpToolCallInput,
    McpToolCallWrite, NativeDatabase, NewMcpMacro, NewMcpSession,
    rocks::{AsyncProjectStore, StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy},
};
use cartograph_domain::{ContentDigest, ProjectId};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use serde_json::{Map, json};
use std::{error::Error, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_macros_and_concurrent_trace_steps_are_atomic_and_durable()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let identity = ContentDigest::from_bytes([42; 32]);
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request,
    )
    .await?;
    let database = NativeDatabase::new(store, &identity);
    let project = database.project_id().clone();
    let session = database
        .create_mcp_session(
            &project,
            &NewMcpSession::named("investigation", "keep graph current")?,
        )
        .await?;
    assert!(
        database
            .current_generation_record(&project)
            .await?
            .is_none()
    );
    concurrent_trace(&database, &project, session.session_id()).await?;
    let calls = database
        .mcp_calls_for_session(McpSessionCallsQuery {
            project_id: &project,
            session_id: session.session_id(),
            limit: 3,
        })
        .await?;
    assert_eq!(
        calls
            .iter()
            .map(cartograph_db::McpToolCallRecord::step)
            .collect::<Vec<_>>(),
        vec![6, 7, 8]
    );
    verify_trace_usage(&database, &project).await?;
    let recipe = NewMcpMacro::new(
        "resume",
        vec![McpMacroStep::new("cartograph_status", Map::new())?],
    )?;
    database.save_mcp_macro(&project, &recipe).await?;
    database.mark_mcp_macro_run(&project, "resume").await?;
    let wrong_project = ProjectId::from_uuid_v8([1; 16]);
    assert!(
        database
            .get_mcp_macro(&wrong_project, "resume")
            .await
            .is_err()
    );
    runtime.shutdown(&request).await?;

    let reopened_runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let reopened = NativeDatabase::new(
        AsyncProjectStore::open(
            &reopened_runtime,
            root.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request,
        )
        .await?,
        &identity,
    );
    let restored = reopened
        .find_mcp_session(McpSessionLookup {
            project_id: &project,
            session_id: Some(session.session_id()),
            label: None,
            require_calls: true,
        })
        .await?
        .ok_or("session did not survive reopen")?;
    assert_eq!(restored.tool_count(), 8);
    let recipe = serde_json::to_value(
        reopened
            .get_mcp_macro(&project, "resume")
            .await?
            .ok_or("macro did not survive reopen")?,
    )?;
    assert_eq!(recipe["runCount"], 1);
    assert!(
        reopened
            .delete_mcp_session(&project, session.session_id())
            .await?
    );
    let usage = serde_json::to_value(reopened.mcp_trace_usage(&project).await?)?;
    assert_eq!(usage["sessionCount"], 0);
    assert_eq!(usage["toolCallCount"], 0);
    assert!(reopened.delete_mcp_macro(&project, "resume").await?);
    assert!(!reopened.delete_mcp_macro(&project, "resume").await?);
    reopened_runtime.shutdown(&request).await?;
    Ok(())
}

async fn verify_trace_usage(
    database: &NativeDatabase,
    project: &ProjectId,
) -> Result<(), Box<dyn Error>> {
    let usage = serde_json::to_value(database.mcp_trace_usage(project).await?)?;
    assert_eq!(usage["sessionCount"], 1);
    assert_eq!(usage["toolCallCount"], 8);
    assert_eq!(usage["errorCount"], 1);
    assert_eq!(usage["tools"][0]["p50DurationMs"], 40);
    assert_eq!(usage["tools"][0]["p95DurationMs"], 80);
    Ok(())
}

async fn concurrent_trace(
    database: &NativeDatabase,
    project: &ProjectId,
    session: &str,
) -> Result<(), Box<dyn Error>> {
    let mut writes = FuturesUnordered::new();
    for duration in [10, 20, 30, 40, 50, 60, 70, 80] {
        let database = database.clone();
        let project = project.clone();
        let session = session.to_owned();
        writes.push(async move {
            let input = McpToolCallInput::new(McpToolCallData {
                tool_name: "cartograph_find",
                arguments: json!({"redacted": true}),
                result_summary: "bounded evidence",
                success: duration != 80,
                duration_ms: duration,
            })?;
            database
                .record_mcp_tool_call(McpToolCallWrite {
                    project_id: &project,
                    session_id: &session,
                    input: &input,
                })
                .await
        });
    }
    let mut steps = Vec::new();
    while let Some(record) = writes.next().await {
        steps.push(record?.step());
    }
    steps.sort_unstable();
    assert_eq!(steps, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    Ok(())
}
