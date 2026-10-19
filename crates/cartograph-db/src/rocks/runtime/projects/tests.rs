use std::{sync::mpsc, time::Duration};

use super::*;
use crate::rocks::{AsyncProjectStore, ReadBudget, StorageRuntime, WorkerPolicy};
use cartograph_domain::{GenerationDigestVersion, GenerationId};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn request() -> Result<StorageRequest> {
    StorageRequest::new(Duration::from_secs(10))
}

fn identity(byte: u8) -> ContentDigest {
    ContentDigest::from_bytes([byte; 32])
}

async fn hold_open(
    runtime: &StorageRuntime,
    root: &Path,
) -> TestResult<(
    mpsc::SyncSender<()>,
    tokio::task::JoinHandle<Result<ProjectAccess>>,
)> {
    let (release, released) = mpsc::sync_channel(1);
    let (entered, started) = tokio::sync::oneshot::channel();
    let pool = Arc::clone(&runtime.pool);
    let projects = Arc::clone(&runtime.projects);
    let root = root.to_path_buf();
    let control = request()?;
    let task = tokio::spawn(async move {
        pool.run(&control, 64 * 1_024, true, move |control| {
            projects.open_with(
                &root,
                identity(8),
                StorePolicy::default(),
                &control,
                |root, identity, policy, control| {
                    let store = ProjectStore::open_controlled(
                        root,
                        identity,
                        policy,
                        Some(control.clone()),
                    )?;
                    let _entered = entered.send(());
                    released
                        .recv_timeout(Duration::from_secs(10))
                        .map_err(|_| crate::rocks::operation("test_open_gate"))?;
                    Ok(store)
                },
            )
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), started).await??;
    Ok((release, task))
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_and_waiting_project_opens_leave_existing_readers_responsive() -> TestResult {
    let current_root = tempfile::tempdir()?;
    let opening_root = tempfile::tempdir()?;
    let waiting_root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy {
        threads: 2,
        ..WorkerPolicy::default()
    })?;
    let current = AsyncProjectStore::open(
        &runtime,
        current_root.path().to_path_buf(),
        identity(7),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let writer = current
        .begin_revision(
            GenerationId::from_uuid_v8([1; 16]),
            identity(1),
            Duration::from_secs(30),
            &request()?,
        )
        .await?;
    current
        .seal(
            &writer,
            identity(2),
            GenerationDigestVersion::V23,
            &request()?,
        )
        .await?;
    current.publish(&writer, &request()?).await?;
    let (release, opening) = hold_open(&runtime, opening_root.path()).await?;
    let waiting_request = request()?;
    let mut waiting = Box::pin(AsyncProjectStore::open(
        &runtime,
        waiting_root.path().to_path_buf(),
        identity(9),
        StorePolicy::default(),
        &waiting_request,
    ));
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    let read_request = request()?;
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        current.read(
            None,
            ReadBudget::new(1, 1_024, Duration::from_secs(5))?,
            &read_request,
            |snapshot, _| Ok(snapshot.revision().sequence),
        ),
    )
    .await;
    // Release the real native open and drain every owned job even when the
    // assertion below reports the original cross-project blocking regression.
    release.send(())?;
    opening.await??;
    waiting.await?;
    runtime.shutdown(&request()?).await?;
    assert_eq!(read??, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_during_native_open_rejects_late_insertion_and_releases_the_primary() -> TestResult
{
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy {
        threads: 2,
        ..WorkerPolicy::default()
    })?;
    let (release, opening) = hold_open(&runtime, root.path()).await?;
    let drain_request = request()?;
    let mut draining = Box::pin(runtime.shutdown(&drain_request));
    assert!(futures_util::poll!(draining.as_mut()).is_pending());
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .projects
                .state
                .try_lock()
                .is_ok_and(|state| state.closed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    release.send(())?;
    let result = opening.await?;
    draining.await?;
    closed?;
    assert!(matches!(result, Err(RocksError::Closed)));
    assert!(
        runtime
            .projects
            .state
            .lock()
            .map_err(|_| "registry poisoned")?
            .entries
            .is_empty()
    );
    let root = root.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        ProjectStore::open(&root, &identity(8), StorePolicy::default()).map(drop)
    })
    .await??;
    Ok(())
}
