use super::*;
use cartograph_agent::{RocksIndexOptions, RocksProjectRuntime, RocksQuery};
use cartograph_db::rocks::StorageRequest;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn wait_for_name(runtime: &Arc<RocksProjectRuntime>, name: &str) -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let request = StorageRequest::new(Duration::from_secs(5))?;
            if let Ok(result) = runtime
                .query(
                    RocksQuery::Name {
                        query: name.to_owned(),
                        limit: 5,
                    },
                    false,
                    &request,
                )
                .await
                && result.fresh
                && result.evidence["results"]
                    .as_array()
                    .is_some_and(|rows| !rows.is_empty())
            {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_watcher_publishes_an_edit_and_removes_the_old_declaration() -> TestResult {
    let project = tempfile::tempdir()?;
    let source = project.path().join("lib.rs");
    std::fs::write(&source, "pub fn original() {}\n")?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let runtime = RocksProjectRuntime::open(project.path(), &request).await?;
    let original = runtime
        .index(RocksIndexOptions::default(), &request)
        .await?;
    let watcher = ProjectAutoSync::start(runtime.clone(), false)?;
    std::fs::write(source, "pub fn replacement() {}\n")?;
    wait_for_name(&runtime, "replacement").await?;
    // The published generation is queryable before the watcher records its
    // completed index attempt, including maintenance and publication counters.
    tokio::time::timeout(Duration::from_secs(10), async {
        while watcher.status().publications == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let status = runtime.status(&request).await?;
    assert_ne!(
        status.current.ok_or("current graph missing")?.generation_id,
        original.revision.generation_id
    );
    let result = runtime
        .query(
            RocksQuery::Name {
                query: "original".to_owned(),
                limit: 5,
            },
            false,
            &request,
        )
        .await?;
    assert_eq!(result.evidence["results"], serde_json::json!([]));
    let activity = watcher.status();
    assert!(activity.active);
    assert!(activity.events > 0);
    assert!(activity.publications > 0);
    watcher.shutdown().await;
    assert!(!watcher.status().active);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_startup_reconciliation_builds_an_unindexed_project() -> TestResult {
    let project = tempfile::tempdir()?;
    std::fs::write(
        project.path().join("lib.rs"),
        "pub fn startup_target() {}\n",
    )?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let runtime = RocksProjectRuntime::open(project.path(), &request).await?;
    let watcher = ProjectAutoSync::start(runtime.clone(), true)?;
    wait_for_name(&runtime, "startup_target").await?;
    assert!(watcher.status().reconciliations > 0);
    watcher.shutdown().await;
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_watcher_reports_a_proven_local_replacement_and_its_duration() -> TestResult {
    let project = tempfile::tempdir()?;
    let source = project.path().join("lib.rs");
    std::fs::write(&source, "pub fn current_value() -> u32 { 1 + 1 }\n")?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let runtime = RocksProjectRuntime::open(project.path(), &request).await?;
    let before = runtime
        .index(RocksIndexOptions::default(), &request)
        .await?;
    let watcher = ProjectAutoSync::start(runtime.clone(), false)?;
    std::fs::write(&source, "pub fn current_value() -> u32 { 2 + 1 }\n")?;
    wait_for_name(&runtime, "current_value").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while watcher.status().publications == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let activity = watcher.status();
    assert_eq!(activity.last_incremental_files, 1);
    let profile = activity
        .last_index_profile
        .ok_or("index phase timings missing")?;
    assert!(profile.total_millis >= profile.preparation_millis);
    assert!(
        activity
            .last_sync_duration_millis
            .is_some_and(|millis| millis > 0)
    );
    let current = runtime.status(&request).await?;
    assert_ne!(
        current
            .current
            .ok_or("missing current graph")?
            .generation_id,
        before.revision.generation_id
    );
    watcher.shutdown().await;
    runtime.shutdown().await?;
    Ok(())
}
