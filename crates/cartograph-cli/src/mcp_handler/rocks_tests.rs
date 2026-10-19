use super::*;
use cartograph_db::rocks::StorageRequest;
use cartograph_mcp::{
    MODERN_PROTOCOL_VERSION, ProtocolServer, ServerConfig, ServerLimits, ServerMetadata,
    ToolProfile,
};
use serde_json::{Value, json};
use std::{error::Error, fmt::Write as _, time::Duration};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_role_sweep_reports_truncation_and_completes_across_cached_pages() -> TestResult
{
    use super::super::{
        ProjectCancellation, ProjectRuntime, RoleClassificationSweepRequest, RoleClassifier,
        STRUCTURAL_ROLE_MODEL, run_role_classification_sweep,
    };
    let root = tempfile::tempdir()?;
    let mut source = String::new();
    for number in 0..600 {
        writeln!(source, "pub fn worker_{number}() -> u32 {{ {number} }}")?;
    }
    std::fs::write(root.path().join("lib.rs"), source)?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    native
        .index(
            cartograph_agent::RocksIndexOptions::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
    let runtime = Arc::new(ProjectRuntime::from_native(native.clone()));
    for (limit, candidates, truncated) in [(1, 1, true), (1000, 599, false), (1, 0, false)] {
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            run_role_classification_sweep(RoleClassificationSweepRequest {
                runtime: runtime.clone(),
                classifier: RoleClassifier::Structural,
                model: STRUCTURAL_ROLE_MODEL.to_owned(),
                cancellation: ProjectCancellation::new(),
                limit,
                concurrency: 2,
            }),
        )
        .await??;
        assert_eq!(report["candidates"], candidates);
        assert_eq!(report["classified"], candidates);
        assert_eq!(report["truncated"], truncated);
        assert_eq!(report["backendRequests"], 0);
    }
    native.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_after_name_page_read_retries_the_whole_evidence_read() -> TestResult {
    let root = tempfile::tempdir()?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
    let native = RocksProjectRuntime::open(root.path(), &request).await?;
    native
        .index(cartograph_agent::RocksIndexOptions::default(), &request)
        .await?;
    let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
    let pause = Arc::new(super::super::TracePause {
        stage: "find-name-complete",
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
    let reading = handler.clone();
    let work = tokio::spawn(async move {
        reading
            .call(
                ToolCall {
                    name: "cartograph_find".to_owned(),
                    arguments: serde_json::from_value(json!({"by":"name","query":"replacement"}))?,
                },
                ToolCallContext::local(Duration::from_secs(30)),
            )
            .await
            .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })
    });
    tokio::time::timeout(Duration::from_secs(3), pause.entered.notified()).await?;
    std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n")?;
    native
        .index(cartograph_agent::RocksIndexOptions::default(), &request)
        .await?;
    *handler.handler.session_trace.pause.lock().await = None;
    pause.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), work).await???;
    let evidence = result.structured_content().ok_or("evidence missing")?;
    assert_eq!(evidence["freshness"], "current");
    assert_eq!(evidence["evidence"]["rows"][0]["name"], "replacement");
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn name_lookup_retry_checks_the_actual_returned_generations_live_freshness() -> TestResult {
    for allow_stale in [false, true] {
        let root = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
        let native = RocksProjectRuntime::open(root.path(), &request).await?;
        native
            .index(cartograph_agent::RocksIndexOptions::default(), &request)
            .await?;
        let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
        let pause = Arc::new(super::super::TracePause {
            stage: "find-name-lookup",
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
        let reading = handler.clone();
        let work = tokio::spawn(async move {
            reading
                .call(
                    ToolCall {
                        name: "cartograph_find".to_owned(),
                        arguments: serde_json::from_value(
                            json!({"by":"name","query":"replacement","allowStale":allow_stale}),
                        )?,
                    },
                    ToolCallContext::local(Duration::from_secs(30)),
                )
                .await
                .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })
        });
        tokio::time::timeout(Duration::from_secs(3), pause.entered.notified()).await?;
        std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n")?;
        native
            .index(cartograph_agent::RocksIndexOptions::default(), &request)
            .await?;
        // The retry can read G1, but these newer bytes are not in that graph.
        std::fs::write(root.path().join("lib.rs"), "pub fn still_newer() {}\n")?;
        *handler.handler.session_trace.pause.lock().await = None;
        pause.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), work).await??;
        if allow_stale {
            let result = result?;
            let evidence = result.structured_content().ok_or("evidence missing")?;
            assert_eq!(evidence["freshness"], "stale");
            assert_eq!(evidence["evidence"]["rows"][0]["name"], "replacement");
        } else {
            assert!(
                result.is_err(),
                "stale replacement generation was labeled current"
            );
        }
        handler.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_drains_cancelled_trace_initialization_without_a_late_session()
-> TestResult {
    cancel_trace_stage("initialize").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_drains_cancelled_trace_recording_and_keeps_completed_tool_result()
-> TestResult {
    cancel_trace_stage("record").await
}

async fn cancel_trace_stage(stage: &'static str) -> TestResult {
    let root = tempfile::tempdir()?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
    let pause = Arc::new(super::super::TracePause {
        stage,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
    let protocol = ProtocolServer::from_shared(
        ServerConfig::new(
            ServerMetadata::cartograph(),
            ToolProfile::Full,
            ServerLimits::default(),
        ),
        handler.clone(),
    )?;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_task = tokio::spawn(async move { protocol.serve(server_read, server_write).await });
    let (client_read, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":MODERN_PROTOCOL_VERSION,"io.modelcontextprotocol/clientInfo":{"name":"native-lifecycle","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}});
    writer.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":"traced","method":"tools/call","params":{"_meta":meta,"name":"cartograph_playbook","arguments":{}}})).as_bytes()).await?;
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified()).await?;
    writer.write_all(format!("{}\n{}\n",json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"traced","reason":"test cancellation"}}),json!({"jsonrpc":"2.0","id":"ping","method":"ping","params":{"_meta":meta}})).as_bytes()).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line)).await??;
    let ping: Value = serde_json::from_str(&line)?;
    assert_eq!(
        ping["id"], "ping",
        "native call completed before its admitted work was released"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(25),
            reader.read_line(&mut String::new())
        )
        .await
        .is_err()
    );
    pause.release.notify_one();
    line.clear();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    let actual: Value = serde_json::from_str(&line)?;
    assert_eq!(actual["id"], "traced");
    if stage == "record" {
        assert_ne!(
            actual["result"]["isError"], true,
            "a completed business result was lost: {actual}"
        );
    } else {
        assert!(
            actual["error"].is_object() || actual["result"]["isError"] == true,
            "cancelled trace initialization started new business work: {actual}"
        );
    }
    let sessions = native
        .database()
        .list_mcp_sessions(native.database().project_id(), 10)
        .await?;
    assert_eq!(sessions.len(), usize::from(stage == "record"));
    for session in sessions {
        assert_eq!(session.tool_count(), 0);
    }
    writer.shutdown().await?;
    tokio::time::timeout(Duration::from_secs(5), server_task).await???;
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_cancelled_cold_admission_keeps_cached_project_responsive() -> TestResult
{
    let root = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    std::fs::create_dir(other.path().join(".cartograph"))?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    let handler = Arc::new(RocksHandler::new(native, false)?);
    let opening = handler.handler.project_cache.opening.lock().await;
    let context = ToolCallContext::local(Duration::from_secs(5));
    let cancelled = context.clone();
    let routed = handler.clone();
    let arguments = serde_json::Map::from_iter([(
        "projectPath".to_owned(),
        json!(other.path().to_str().ok_or("other path")?),
    )]);
    let waiter = tokio::spawn(async move {
        routed
            .call(
                ToolCall {
                    name: "cartograph_playbook".to_owned(),
                    arguments,
                },
                cancelled,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    let cached = tokio::time::timeout(
        Duration::from_secs(1),
        handler.call(
            ToolCall {
                name: "cartograph_playbook".to_owned(),
                arguments: serde_json::Map::new(),
            },
            ToolCallContext::local(Duration::from_secs(5)),
        ),
    )
    .await??;
    assert!(!cached.is_error());
    context.cancellation().cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await??
            .is_err()
    );
    assert!(!other.path().join(".cartograph/data/CURRENT").exists());
    drop(opening);
    let expired = ToolCallContext::local(Duration::from_nanos(1));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let rejected = handler
        .call(
            ToolCall {
                name: "cartograph_playbook".to_owned(),
                arguments: serde_json::Map::from_iter([(
                    "projectPath".to_owned(),
                    json!(other.path().to_str().ok_or("other path")?),
                )]),
            },
            expired,
        )
        .await;
    assert!(rejected.is_err());
    assert!(!other.path().join(".cartograph/data/CURRENT").exists());
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_control_replies_bypass_trace_initialization_and_recording() -> TestResult {
    use super::super::{
        AdminAction, AdminJobRequest, ProjectCancellation, ProjectError, TracePause,
        TraceQueryTools,
    };
    for stage in ["initialize", "record"] {
        let root = tempfile::tempdir()?;
        let native =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
                .await?;
        let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
        if stage == "record" {
            assert!(
                TraceQueryTools(&handler.handler)
                    .ensure_trace_session()
                    .await
                    .is_some()
            );
        }
        let pause = Arc::new(TracePause {
            stage,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
        let cancellation = ProjectCancellation::new();
        let wait = cancellation.clone();
        let started = handler
            .handler
            .admin_jobs
            .start(AdminJobRequest {
                action: AdminAction::Index,
                cancellation,
                operation: async move {
                    wait.cancelled().await;
                    Err::<Value, ProjectError>(ProjectError::RequestCancelled)
                },
            })
            .await?;
        for (action, expected) in [("status", "running"), ("cancel", "cancelling")] {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                handler.call(
                    ToolCall {
                        name: "cartograph_admin".to_owned(),
                        arguments: serde_json::from_value(
                            json!({"action":action, "jobId":started.job_id}),
                        )?,
                    },
                    ToolCallContext::local(Duration::from_secs(5)),
                ),
            )
            .await??;
            let value: Value =
                serde_json::from_str(result.primary_text().ok_or("control result missing")?)?;
            assert_eq!(value["status"], expected);
        }
        let sessions = native
            .database()
            .list_mcp_sessions(native.database().project_id(), 10)
            .await?;
        assert_eq!(sessions.len(), usize::from(stage == "record"));
        assert!(sessions.iter().all(|session| session.tool_count() == 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), pause.entered.notified())
                .await
                .is_err()
        );
        *handler.handler.session_trace.pause.lock().await = None;
        handler.close().await?;
    }
    Ok(())
}
