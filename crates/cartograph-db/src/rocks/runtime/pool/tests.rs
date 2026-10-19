use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use super::*;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

mod reclamation;

fn request() -> Result<StorageRequest> {
    StorageRequest::new(Duration::from_secs(10))
}

fn drain_request() -> Result<StorageRequest> {
    StorageRequest::new(Duration::from_secs(10))
}

fn pool() -> Result<Arc<StoragePool>> {
    Ok(Arc::new(StoragePool::new(
        WorkerPolicy {
            threads: 2,
            pending_jobs: 4,
            pending_bytes: 1_048_576,
        },
        || {},
    )?))
}

struct Held {
    release: mpsc::SyncSender<()>,
    task: tokio::task::JoinHandle<Result<u64>>,
}

impl Held {
    async fn finish(self) -> Result<u64> {
        self.release.send(()).map_err(|_| operation("test_gate"))?;
        self.task.await.map_err(|_| operation("test_task"))?
    }
}

async fn hold(pool: &Arc<StoragePool>, mutation: bool) -> Result<Held> {
    hold_reserving(pool, mutation, 1).await
}

async fn hold_reserving(pool: &Arc<StoragePool>, mutation: bool, reservation: u32) -> Result<Held> {
    let (send_started, started) = oneshot::channel();
    let (release, released) = mpsc::sync_channel(1);
    let pool = Arc::clone(pool);
    let request = request()?;
    let task = tokio::spawn(async move {
        pool.run(&request, reservation, mutation, move |_| {
            let _notified = send_started.send(());
            released
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| operation("test_gate"))?;
            Ok(42)
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .map_err(|_| operation("test_start"))?
        .map_err(|_| operation("test_start"))?;
    Ok(Held { release, task })
}

#[tokio::test(flavor = "current_thread")]
async fn queued_cancellation_is_prompt_and_does_not_execute_the_operation() -> TestResult {
    let pool = pool()?;
    let first = hold(&pool, false).await?;
    let second = hold(&pool, false).await?;
    let called = Arc::new(AtomicBool::new(false));
    let invoked = Arc::clone(&called);
    let request = request()?;
    let mut queued = Box::pin(pool.run(&request, 1, false, move |_| {
        invoked.store(true, Ordering::Release);
        Ok(99)
    }));
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    request.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), queued).await;
    assert_eq!(first.finish().await?, 42);
    assert_eq!(second.finish().await?, 42);
    pool.close();
    pool.drain(&drain_request()?).await?;
    assert_eq!(result?, Err(RocksError::Cancelled));
    assert!(!called.load(Ordering::Acquire));
    assert_eq!(pool.slots.available_permits(), 4);
    assert_eq!(pool.bytes.available_permits(), 1_048_576);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_a_queued_future_releases_its_job_and_memory_admission() -> TestResult {
    let pool = pool()?;
    let first = hold(&pool, false).await?;
    let second = hold(&pool, false).await?;
    let request = request()?;
    let mut queued = Box::pin(pool.run(&request, 4_096, false, |_| Ok(99)));
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    assert_eq!(pool.slots.available_permits(), 1);
    drop(queued);
    assert_eq!(pool.slots.available_permits(), 2);
    assert_eq!(pool.bytes.available_permits(), 1_048_574);
    first.finish().await?;
    second.finish().await?;
    pool.close();
    pool.drain(&drain_request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_writers_leave_worker_and_queue_capacity_for_reads() -> TestResult {
    let pool = pool()?;
    let first = hold(&pool, true).await?;
    let request = request()?;
    let mut waiting_writer = Box::pin(pool.run(&request, 1, true, |_| Ok(7)));
    assert!(futures_util::poll!(waiting_writer.as_mut()).is_pending());
    let read = pool.run(&request, 1, false, |_| Ok(99)).await;
    first.finish().await?;
    assert_eq!(waiting_writer.await?, 7);
    pool.close();
    pool.drain(&drain_request()?).await?;
    assert_eq!(read?, 99);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_started_native_work_preserves_its_actual_result() -> TestResult {
    let pool = pool()?;
    let (entered, started) = oneshot::channel();
    let (release, released) = mpsc::sync_channel(1);
    let request = request()?;
    let task_request = request.clone();
    let task_pool = Arc::clone(&pool);
    let task = tokio::spawn(async move {
        task_pool
            .run(&task_request, 1, true, move |_| {
                let _entered = entered.send(());
                released
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| operation("test_gate"))?;
                Ok(42)
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), started).await??;
    request.cancel();
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    release.send(())?;
    assert_eq!(task.await??, 42);
    pool.close();
    pool.drain(&drain_request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_admission_is_released_after_cancellation_and_oversize_is_rejected() -> TestResult {
    let pool = pool()?;
    let request = request()?;
    assert_eq!(
        pool.run(&request, 1_048_577, false, |_| Ok(1)).await,
        Err(RocksError::Limit {
            resource: "worker_bytes"
        })
    );
    let (admitted, returned) = oneshot::channel();
    let held = hold_reserving(&pool, false, 1_048_576).await?;
    let cancel = StorageRequest::new(Duration::from_secs(1))?;
    let mut waiting = Box::pin(pool.run(&cancel, 1, false, move |_| {
        let _admitted = admitted.send(());
        Ok(99)
    }));
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    cancel.cancel();
    assert_eq!(waiting.await, Err(RocksError::Cancelled));
    assert_eq!(held.finish().await?, 42);
    assert!(returned.await.is_err());
    assert_eq!(pool.run(&request, 1_048_576, false, |_| Ok(7)).await?, 7);
    pool.close();
    pool.drain(&drain_request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_drains_started_work_and_runs_finalization_on_a_storage_thread() -> TestResult {
    let finalized = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finalized);
    let pool = Arc::new(StoragePool::new(WorkerPolicy::default(), move || {
        flag.store(
            thread::current()
                .name()
                .is_some_and(|name| name.starts_with("cartograph-storage-")),
            Ordering::Release,
        );
    })?);
    let held = hold(&pool, false).await?;
    pool.close();
    let shutdown_request = drain_request()?;
    let mut draining = Box::pin(pool.drain(&shutdown_request));
    assert!(futures_util::poll!(draining.as_mut()).is_pending());
    assert_eq!(held.finish().await?, 42);
    draining.await?;
    assert!(finalized.load(Ordering::Acquire));
    assert_eq!(
        pool.run(&request()?, 1, false, |_| Ok(1)).await,
        Err(RocksError::Closed)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_shutdown_retains_workers_and_can_be_retried() -> TestResult {
    let pool = pool()?;
    let held = hold(&pool, false).await?;
    pool.close();
    let deadline = StorageRequest::new(Duration::from_millis(20))?;
    assert_eq!(
        pool.drain(&deadline).await,
        Err(RocksError::Limit {
            resource: "operation_deadline"
        })
    );
    assert!(
        !pool
            .workers
            .lock()
            .map_err(|_| "worker mutex poisoned")?
            .is_empty()
    );
    assert_eq!(held.finish().await?, 42);
    pool.drain(&drain_request()?).await?;
    assert!(
        pool.workers
            .lock()
            .map_err(|_| "worker mutex poisoned")?
            .is_empty()
    );
    Ok(())
}
