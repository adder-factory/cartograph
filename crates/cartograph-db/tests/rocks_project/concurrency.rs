//! Concurrent native access and async request supervision regressions.

use std::sync::{Arc, mpsc};

#[path = "phase.rs"]
mod phase;
use phase::Barrier;

use cartograph_db::rocks::{AsyncProjectStore, StorageRequest, StorageRuntime, WorkerPolicy};

use super::{
    Collection, Duration, GenerationDigestVersion, ProjectStore, ReadBudget, RecordChange,
    RecordKey, RocksError, StorePolicy, TestResult, budget, generation, identity, key, publish,
    store,
};

#[test]
fn concurrent_staging_preserves_every_row_and_exact_atomic_totals() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    let writer = store.begin_revision(generation(1), identity(1), Duration::from_secs(30))?;
    let start = Arc::new(Barrier::new(9));
    let counts = std::thread::scope(|scope| {
        let mut tasks = Vec::new();
        for worker in 0..8_u64 {
            let store = &store;
            let writer = &writer;
            let start = Arc::clone(&start);
            tasks.push(scope.spawn(move || -> Result<(), RocksError> {
                start.wait();
                for batch in 0..20_u64 {
                    let mut changes = Vec::new();
                    for row in 0..4_u64 {
                        let index = worker * 80 + batch * 4 + row;
                        changes.push(RecordChange::put(
                            Collection::Graph,
                            RecordKey::new(format!("symbol/{index:04}").into_bytes())?,
                            index.to_be_bytes().to_vec(),
                        ));
                    }
                    store.stage(writer, &changes)?;
                    // Exact retry overlaps the same keys but adds no versions.
                    store.stage(writer, &changes)?;
                }
                Ok(())
            }));
        }
        start.wait();
        tasks
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    for count in counts {
        count.map_err(|_| "staging worker panicked")??;
    }
    let ready = store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
    assert_eq!(ready.changed_rows, 640);
    // 11 logical ASCII bytes + 2 terminator + 8 sequence + 1 tag + 8 value.
    assert_eq!(ready.changed_bytes, 640 * 30);
    store.publish(&writer)?;
    let snapshot = store.current_snapshot(None)?;
    for index in 0..640_u64 {
        assert_eq!(
            snapshot.get(
                Collection::Graph,
                &key(format!("symbol/{index:04}").as_bytes())?,
                &mut budget()?
            )?,
            Some(index.to_be_bytes().to_vec()),
        );
    }
    Ok(())
}

#[test]
fn conflicting_parallel_replays_acknowledge_exactly_one_value() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    let writer = store.begin_revision(generation(1), identity(1), Duration::from_secs(30))?;
    let start = Barrier::new(3);
    let results = std::thread::scope(|scope| {
        let mut tasks = Vec::new();
        for value in [b"one".to_vec(), b"two".to_vec()] {
            let store = &store;
            let writer = &writer;
            let start = &start;
            tasks.push(scope.spawn(move || {
                start.wait();
                store.stage(
                    writer,
                    &[RecordChange::put(
                        Collection::Graph,
                        RecordKey::new(b"shared".to_vec())?,
                        value,
                    )],
                )
            }));
        }
        start.wait();
        tasks
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    let mut successes = 0;
    let mut conflicts = 0;
    for result in results {
        match result.map_err(|_| "replay worker panicked")? {
            Ok(revision) => {
                assert_eq!(revision.changed_rows, 1);
                successes += 1;
            }
            Err(RocksError::ReplayConflict) => conflicts += 1,
            Err(error) => return Err(error.into()),
        }
    }
    assert_eq!((successes, conflicts), (1, 1));
    assert_eq!(
        store
            .seal(&writer, identity(2), GenerationDigestVersion::V23)?
            .changed_rows,
        1
    );
    Ok(())
}

#[test]
fn parallel_readers_pin_complete_graph_and_search_during_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(&store, 1, &revision_changes(1)?)?;
    let phase = Barrier::new(5);
    let results = std::thread::scope(|scope| {
        let mut readers = Vec::new();
        for _ in 0..4 {
            let phase = &phase;
            let store = &store;
            readers.push(scope.spawn(move || observe_publications(store, phase)));
        }
        for sequence in 2..=31 {
            let writer =
                store.begin_revision(generation(sequence), identity(1), Duration::from_secs(30))?;
            phase.wait();
            store.stage(&writer, &revision_changes(sequence)?)?;
            store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
            store.publish(&writer)?;
            phase.wait();
            phase.wait();
        }
        Ok::<_, Box<dyn std::error::Error>>(
            readers
                .into_iter()
                .map(std::thread::ScopedJoinHandle::join)
                .collect::<Vec<_>>(),
        )
    })?;
    for result in results {
        result.map_err(|_| "reader worker panicked")??;
    }
    Ok(())
}

fn observe_publications(store: &ProjectStore, phase: &Barrier) -> Result<(), RocksError> {
    let key = RecordKey::new(b"revision".to_vec())?;
    for previous in 1..=30_u64 {
        let old = store.current_snapshot(None)?;
        phase.wait();
        let mut budget = ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?;
        for collection in [Collection::Graph, Collection::Search] {
            assert_eq!(
                old.get(collection, &key, &mut budget)?,
                Some(previous.to_be_bytes().to_vec())
            );
        }
        phase.wait();
        let current = store.current_snapshot(None)?;
        for collection in [Collection::Graph, Collection::Search] {
            assert_eq!(
                current.get(collection, &key, &mut budget)?,
                Some((previous + 1).to_be_bytes().to_vec())
            );
        }
        phase.wait();
    }
    Ok(())
}

fn revision_changes(sequence: u64) -> TestResult<Vec<RecordChange>> {
    [Collection::Graph, Collection::Search]
        .into_iter()
        .map(|collection| {
            Ok(RecordChange::put(
                collection,
                key(b"revision")?,
                sequence.to_be_bytes().to_vec(),
            ))
        })
        .collect()
}

fn request() -> Result<StorageRequest, RocksError> {
    StorageRequest::new(Duration::from_secs(10))
}

fn async_budget() -> TestResult<ReadBudget> {
    Ok(ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?)
}

#[tokio::test(flavor = "current_thread")]
async fn busy_native_read_leaves_async_executor_and_another_reader_responsive() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_path_buf(),
        identity(7),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let writer = store
        .begin_revision(
            generation(1),
            identity(1),
            Duration::from_secs(30),
            &request()?,
        )
        .await?;
    store
        .stage(&writer, revision_changes(1)?, &request()?)
        .await?;
    store
        .seal(
            &writer,
            identity(2),
            GenerationDigestVersion::V23,
            &request()?,
        )
        .await?;
    store.publish(&writer, &request()?).await?;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, held) = mpsc::sync_channel(1);
    let busy_store = store.clone();
    let busy_request = request()?;
    let busy_budget = async_budget()?;
    let busy = tokio::spawn(async move {
        busy_store
            .read(None, busy_budget, &busy_request, move |snapshot, _| {
                entered.send(()).map_err(|()| RocksError::Cancelled)?;
                held.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| RocksError::Cancelled)?;
                Ok(snapshot.revision().sequence)
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), started).await??;
    let other = store
        .read(None, async_budget()?, &request()?, |snapshot, budget| {
            snapshot.get(
                Collection::Search,
                &RecordKey::new(b"revision".to_vec())?,
                budget,
            )
        })
        .await?;
    assert_eq!(other, Some(1_u64.to_be_bytes().to_vec()));
    let drain_deadline = StorageRequest::new(Duration::from_millis(20))?;
    assert_eq!(
        runtime.shutdown(&drain_deadline).await,
        Err(RocksError::Limit {
            resource: "operation_deadline"
        })
    );
    // The unfinished worker still owns its pinned native handle after timeout.
    assert!(super::store(root.path()).is_err());
    release.send(())?;
    assert_eq!(busy.await??, 1);
    runtime.shutdown(&request()?).await?;
    assert!(matches!(
        store
            .read(None, async_budget()?, &request()?, |snapshot, _| Ok(
                snapshot.revision().sequence
            ))
            .await,
        Err(RocksError::Closed)
    ));
    // The native handle closed on a worker even though the async facade lives.
    let reopened = super::store(root.path())?;
    assert_eq!(reopened.current_snapshot(None)?.revision().sequence, 1);
    Ok(())
}
