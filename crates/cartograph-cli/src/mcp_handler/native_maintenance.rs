//! Retain native maintenance through cancellation and preserve committed outcomes.
use super::{ADMIN_MAINTENANCE_TIMEOUT, ProjectCancellation, ProjectError};
use cartograph_db::NativeDatabase;

pub(super) async fn run<T>(
    cancellation: &ProjectCancellation,
    operation: impl std::future::Future<Output = Result<T, ProjectError>>,
) -> Result<T, ProjectError> {
    let control = NativeDatabase::request_control(ADMIN_MAINTENANCE_TIMEOUT)
        .map_err(|_| ProjectError::RequestCancelled)?;
    let operation = NativeDatabase::with_request(control.clone(), operation);
    tokio::pin!(operation);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            control.cancel();
            operation.await.map_err(|_| ProjectError::RequestCancelled)
        },
        result = &mut operation => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_handler::{AdminAction, AdminJobRequest, AdminJobStatus, AdminJobs};
    use cartograph_db::rocks::{
        AsyncProjectStore, ReadBudget, RocksError, StorageRequest, StorageRuntime, StorePolicy,
        WorkerPolicy,
    };
    use cartograph_domain::ContentDigest;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_maintenance_job_stays_cancelling_until_its_native_worker_finishes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let store = AsyncProjectStore::open(
            &runtime,
            root.path().to_owned(),
            ContentDigest::from_bytes([42; 32]),
            StorePolicy::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
        let jobs = AdminJobs::new();
        let cancellation = ProjectCancellation::new();
        let control = cancellation.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let job = jobs
            .start(AdminJobRequest {
                action: AdminAction::BuildSimilarityEdges,
                cancellation,
                operation: async move {
                    run(&control, async move {
                        let request = NativeDatabase::request_control(Duration::from_secs(30))
                            .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
                        store
                            .state(
                                None,
                                ReadBudget::new(100, 1_048_576, Duration::from_secs(30))
                                    .map_err(|_| ProjectError::EmbeddingOperationFailed)?,
                                0,
                                0,
                                &request,
                                move |_, budget| {
                                    let _entered = entered.send(());
                                    released
                                        .recv_timeout(Duration::from_secs(10))
                                        .map_err(|_| RocksError::Cancelled)?;
                                    budget.checkpoint()?;
                                    Ok(serde_json::json!({"done":true}))
                                },
                            )
                            .await
                            .map_err(|_| ProjectError::EmbeddingOperationFailed)
                    })
                    .await
                },
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        assert_eq!(
            jobs.cancel(Some(job.job_id)).await?.status,
            AdminJobStatus::Cancelling
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        let before_release = jobs.status(Some(job.job_id)).await?.status;
        release.send(())?;
        let terminal = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = jobs.status(Some(job.job_id)).await?;
                if status.status != AdminJobStatus::Cancelling {
                    return Ok::<_, cartograph_mcp::ToolError>(status);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        assert_eq!(before_release, AdminJobStatus::Cancelling);
        assert_eq!(terminal.status, AdminJobStatus::Cancelled);
        jobs.shutdown().await;
        runtime
            .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
            .await?;
        Ok(())
    }
}
