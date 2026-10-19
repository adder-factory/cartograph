//! Native read cancellation retains the caller until the actual worker is reaped.
use super::*;
use cartograph_db::rocks::{
    AsyncProjectStore, ReadBudget, RocksError, StorageRequest, StorageRuntime, StorePolicy,
    WorkerPolicy,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_project_native_read_drains_its_worker_before_returning()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let identity = ContentDigest::from_bytes([42; 32]);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity,
        StorePolicy::default(),
        &StorageRequest::new(Duration::from_secs(30))?,
    )
    .await?;
    let cancellation = ProjectCancellation::new();
    let cancelling = cancellation.clone();
    let reading = store.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let work = tokio::spawn(async move {
        cancellable_project_read(&cancelling, async move {
            let request = CartographDatabase::request_control(Duration::from_secs(30))
                .map_err(|_| ProjectError::RequestCancelled)?;
            reading
                .state(
                    None,
                    ReadBudget::new(100, 1_048_576, Duration::from_secs(30))
                        .map_err(|_| ProjectError::StatusFailed)?,
                    0,
                    0,
                    &request,
                    move |_, budget| {
                        let _entered = entered.send(());
                        released
                            .recv_timeout(Duration::from_secs(10))
                            .map_err(|_| RocksError::Cancelled)?;
                        budget.checkpoint()?;
                        Ok(())
                    },
                )
                .await
                .map_err(|_| ProjectError::StatusFailed)
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), started).await??;
    cancellation.cancel();
    tokio::time::sleep(Duration::from_millis(25)).await;
    let returned_early = work.is_finished();
    // Unrelated work can still use the other native worker during the drain.
    store
        .state(
            None,
            ReadBudget::new(100, 1_048_576, Duration::from_secs(3))?,
            0,
            0,
            &StorageRequest::new(Duration::from_secs(3))?,
            |_, _| Ok(()),
        )
        .await?;
    release.send(())?;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), work).await??,
        Err(ProjectError::RequestCancelled)
    ));
    assert!(
        !returned_early,
        "cancelled native read escaped before its worker finished"
    );
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}
