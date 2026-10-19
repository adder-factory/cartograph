use std::sync::mpsc;

use super::*;
use crate::rocks::{Collection, RecordKey};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

mod reclamation;

#[tokio::test]
async fn stage_signals_preserve_request_controls_and_reject_rebinding() -> TestResult {
    let original = request()?;
    let (parent, parent_signal) = watch::channel(false);
    let (stage, stage_signal) = watch::channel(false);
    let controlled = original.with_cancellation_signals(
        [parent_signal.clone(), stage_signal.clone()],
        Instant::now() + Duration::from_secs(30),
    )?;
    assert_eq!(controlled.deadline, original.deadline);
    assert!(
        controlled
            .with_cancellation_signals(
                [parent_signal, stage_signal],
                Instant::now() + Duration::from_secs(30)
            )
            .is_err()
    );
    parent.send(true)?;
    assert_eq!(controlled.check(), Err(RocksError::Cancelled));
    assert_eq!(controlled.stopped().await, RocksError::Cancelled);
    assert!(original.check().is_ok());
    drop(stage);
    let (parent, parent_signal) = watch::channel(false);
    let (stage, stage_signal) = watch::channel(false);
    let child = original.with_cancellation_signals(
        [parent_signal, stage_signal],
        Instant::now() + Duration::from_secs(30),
    )?;
    original.cancel();
    assert_eq!(child.check(), Err(RocksError::Cancelled));
    drop((parent, stage));
    Ok(())
}

#[test]
fn initially_stopped_stage_does_not_cancel_the_original_and_expiry_stays_distinct() -> TestResult {
    let original = request()?;
    let (_parent, parent_signal) = watch::channel(true);
    let (_stage, stage_signal) = watch::channel(false);
    assert!(matches!(
        original.with_cancellation_signals(
            [parent_signal, stage_signal],
            Instant::now() + Duration::from_secs(30)
        ),
        Err(RocksError::Cancelled)
    ));
    assert!(original.check().is_ok());
    let (_parent, parent_signal) = watch::channel(false);
    let (_stage, stage_signal) = watch::channel(false);
    assert!(matches!(
        original.with_cancellation_signals([parent_signal, stage_signal], Instant::now()),
        Err(RocksError::Limit {
            resource: "operation_deadline"
        })
    ));
    assert!(original.check().is_ok());
    Ok(())
}

fn request() -> Result<StorageRequest> {
    StorageRequest::new(Duration::from_secs(10))
}

fn identity(byte: u8) -> ContentDigest {
    ContentDigest::from_bytes([byte; 32])
}

fn generation(byte: u8) -> GenerationId {
    GenerationId::from_uuid_v8([byte; 16])
}

async fn open(runtime: &StorageRuntime, root: &std::path::Path) -> Result<AsyncProjectStore> {
    AsyncProjectStore::open(
        runtime,
        root.to_path_buf(),
        identity(7),
        StorePolicy::default(),
        &request()?,
    )
    .await
}

async fn hold_writer(
    runtime: &StorageRuntime,
) -> Result<(mpsc::SyncSender<()>, tokio::task::JoinHandle<Result<()>>)> {
    let (release, released) = mpsc::sync_channel(1);
    let (entered, started) = tokio::sync::oneshot::channel();
    let pool = Arc::clone(&runtime.pool);
    let request = request()?;
    let task = tokio::spawn(async move {
        pool.run(&request, 1, true, move |_| {
            let _entered = entered.send(());
            released
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| super::super::operation("test_gate"))?;
            Ok(())
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .map_err(|_| super::super::operation("test_start"))?
        .map_err(|_| super::super::operation("test_start"))?;
    Ok((release, task))
}

fn changes(index: u8) -> Result<Vec<RecordChange>> {
    Ok(vec![RecordChange::put(
        Collection::Graph,
        RecordKey::new(vec![b's', index])?,
        vec![index],
    )])
}

#[tokio::test(flavor = "current_thread")]
async fn state_admission_does_not_renew_an_inherited_request_deadline() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = open(&runtime, root.path()).await?;
    let database = crate::NativeDatabase::new(store, &identity(7));
    let (release_first, first) = hold_writer(&runtime).await?;
    let (release_second, second) = hold_writer(&runtime).await?;
    let control = StorageRequest::new(Duration::from_millis(100))?;
    let mut work = Box::pin(crate::NativeDatabase::with_request(
        control,
        database.state_with_timeout(None, 1024, 1, Duration::from_secs(60), |transaction, _| {
            transaction.put(super::super::StateNamespace::Metadata, b"queued", vec![42])
        }),
    ));
    assert!(futures_util::poll!(work.as_mut()).is_pending());
    let result = tokio::time::timeout(Duration::from_secs(2), work).await?;
    release_first.send(())?;
    release_second.send(())?;
    first.await??;
    second.await??;
    assert!(matches!(
        result,
        Err(crate::StorageError::StatementTimeout { .. })
    ));
    assert_eq!(
        database
            .state(None, 0, 0, |transaction, budget| {
                transaction.get(super::super::StateNamespace::Metadata, b"queued", budget)
            })
            .await?,
        None
    );
    runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sealing_drains_all_admitted_staging_jobs_before_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = open(&runtime, root.path()).await?;
    let request = request()?;
    let writer = store
        .begin_revision(
            generation(1),
            identity(1),
            Duration::from_secs(30),
            &request,
        )
        .await?;
    let (release_first, first) = hold_writer(&runtime).await?;
    let (release_second, second) = hold_writer(&runtime).await?;
    let inputs = (0..8).map(changes).collect::<Result<Vec<_>>>()?;
    let mut staged = Box::pin(futures_util::future::join_all(
        inputs
            .into_iter()
            .map(|changes| store.stage(&writer, changes, &request)),
    ));
    assert!(futures_util::poll!(staged.as_mut()).is_pending());
    let mut sealed =
        Box::pin(store.seal(&writer, identity(2), GenerationDigestVersion::V23, &request));
    assert!(futures_util::poll!(sealed.as_mut()).is_pending());
    release_first.send(())?;
    release_second.send(())?;
    let (staged, sealed) = tokio::join!(staged, sealed);
    first.await??;
    second.await??;
    for result in staged {
        result?;
    }
    assert_eq!(sealed?.changed_rows, 8);
    assert_eq!(store.publish(&writer, &request).await?.changed_rows, 8);
    assert_eq!(
        store.stage(&writer, changes(10)?, &request).await,
        Err(RocksError::Transition)
    );
    let count = store
        .read(
            None,
            ReadBudget::new(100, 1_048_576, Duration::from_secs(5))?,
            &request,
            |snapshot, budget| {
                Ok(snapshot
                    .scan_prefix(Collection::Graph, b"s", None, 100, budget)?
                    .rows
                    .len())
            },
        )
        .await?;
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(10))?)
        .await?;
    assert_eq!(count, 8);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_staging_job_blocks_sealing_and_allows_owned_failure_cleanup() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = open(&runtime, root.path()).await?;
    let writer = store
        .begin_revision(
            generation(1),
            identity(1),
            Duration::from_secs(30),
            &request()?,
        )
        .await?;
    let cancelled = request()?;
    cancelled.cancel();
    assert_eq!(
        store.stage(&writer, changes(1)?, &cancelled).await,
        Err(RocksError::Cancelled)
    );
    assert_eq!(
        store
            .seal(
                &writer,
                identity(2),
                GenerationDigestVersion::V23,
                &request()?
            )
            .await,
        Err(RocksError::Transition)
    );
    assert_eq!(
        store.publish(&writer, &request()?).await,
        Err(RocksError::Transition)
    );
    store.fail(&writer, &request()?).await?;
    let next = store
        .begin_revision(
            generation(2),
            identity(1),
            Duration::from_secs(30),
            &request()?,
        )
        .await?;
    assert_eq!(next.sequence(), 2);
    store.fail(&next, &request()?).await?;
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(10))?)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_project_open_shares_one_primary_and_failure_keeps_other_projects_alive()
-> TestResult {
    let root = tempfile::tempdir()?;
    let foreign = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let first = open(&runtime, root.path()).await?;
    let second = open(&runtime, root.path()).await?;
    assert!(matches!(
        open(&runtime, foreign.path()).await,
        Err(RocksError::ProjectMismatch)
    ));
    let writer = first
        .begin_revision(
            generation(1),
            identity(1),
            Duration::from_secs(30),
            &request()?,
        )
        .await?;
    assert!(matches!(
        second
            .begin_revision(
                generation(2),
                identity(1),
                Duration::from_secs(30),
                &request()?
            )
            .await,
        Err(RocksError::WriterBusy)
    ));
    first.fail(&writer, &request()?).await?;
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(10))?)
        .await?;
    assert!(!foreign.path().join(".cartograph").exists());
    Ok(())
}
