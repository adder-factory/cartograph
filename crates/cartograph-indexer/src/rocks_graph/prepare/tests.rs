use std::{error::Error, sync::Mutex, time::Duration};

use cartograph_db::{GenerationFacts, GenerationValidationLimits, validate_generation_facts};
use cartograph_domain::{ContentDigest, GenerationId};
use tokio::time::Instant;

use super::*;
use crate::{ScopedTaskError, stage::test_stage_runner};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_preparation_retains_its_worker_and_bytes_until_actual_completion()
-> Result<(), Box<dyn Error>> {
    let maximum_bytes = 256 * 1_048_576;
    let (runner, tasks, cancellation) = test_stage_runner(8, maximum_bytes).await;
    let (entered, started) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let (noticed, cancelled) = tokio::sync::oneshot::channel();
    let noticed = Mutex::new(Some(noticed));
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let (facts, _) = validate_generation_facts(
        GenerationFacts::default(),
        GenerationValidationLimits::new(1_048_576, 4 * 1_048_576)?,
        || false,
    )?;
    let work = tokio::spawn(async move {
        run_with(
            &runner,
            facts,
            GraphRevision {
                generation_id: GenerationId::from_uuid_v8([1; 16]),
                source_revision: ContentDigest::from_bytes([1; 32]),
                base: cartograph_db::rocks::GraphBase::Empty,
            },
            Policy {
                graph: GraphPolicy::default(),
                maximum_bytes,
                deadlines: NativePipelineDeadlines::new(
                    Duration::from_secs(10),
                    Duration::from_secs(20),
                    Duration::from_millis(50),
                )
                .map_err(|_| RocksError::Cancelled)?,
            },
            &StorageRequest::new(Duration::from_secs(30))?,
            move |identity, facts, policy, bytes, request| {
                let _ = entered
                    .lock()
                    .map_err(|_| RocksError::Cancelled)?
                    .take()
                    .ok_or(RocksError::Cancelled)?
                    .send(());
                while request.check().is_ok() {
                    std::thread::yield_now();
                }
                let _ = noticed
                    .lock()
                    .map_err(|_| RocksError::Cancelled)?
                    .take()
                    .ok_or(RocksError::Cancelled)?
                    .send(());
                released
                    .lock()
                    .map_err(|_| RocksError::Cancelled)?
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|_| RocksError::Cancelled)?;
                GraphPublicationInput::prepare(identity, facts, policy, bytes, request)
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), started).await??;
    assert_eq!(
        tasks.probe_admission(1),
        Err(ScopedTaskError::ByteCapacityExceeded)
    );
    cancellation.send(true)?;
    tokio::time::timeout(Duration::from_secs(5), cancelled).await??;
    let retained = tasks
        .close_abort_and_reap_retained(Instant::now() + Duration::from_millis(20))
        .await;
    assert!(!retained.all_joined);
    assert_eq!(retained.active_tasks, 1);
    release.send(())?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), work)
            .await??
            .is_err()
    );
    let reaped = tasks
        .close_abort_and_reap_retained(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(reaped.all_joined);
    assert_eq!(reaped.active_tasks, 0);
    Ok(())
}
