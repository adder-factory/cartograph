//! Slow auxiliary readers neither block publication nor mix graph/state snapshots.

use super::*;
use cartograph_db::rocks::{AsyncProjectStore, StateNamespace, StorageRuntime, WorkerPolicy};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_readers_keep_one_snapshot_while_writer_publishes() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        digest(1),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    store
        .publish_graph(
            input(1, facts(2, vec![edge(1, 2, "before")])?)?,
            &request()?,
        )
        .await?;
    let mut tasks = tokio::task::JoinSet::new();
    let mut gates = Vec::new();
    let mut started = Vec::new();
    for _ in 0..2 {
        let (release, wait) = std::sync::mpsc::channel();
        let (ready, entered) = tokio::sync::oneshot::channel();
        gates.push(release);
        started.push(entered);
        let store = store.clone();
        tasks.spawn(async move {
            store
                .state(
                    Some(generation(1)),
                    ReadBudget::new(100, 1_048_576, Duration::from_secs(20))?,
                    0,
                    0,
                    &StorageRequest::new(Duration::from_secs(20))?,
                    move |transaction, budget| {
                        let before = transaction
                            .current()?
                            .ok_or(RocksError::GenerationUnavailable)?
                            .generation_id;
                        ready.send(()).map_err(|()| RocksError::Cancelled)?;
                        wait.recv_timeout(Duration::from_secs(10))
                            .map_err(|_| RocksError::Cancelled)?;
                        let after = transaction
                            .current()?
                            .ok_or(RocksError::GenerationUnavailable)?
                            .generation_id;
                        let snapshot = transaction.graph_snapshot()?;
                        let graph = snapshot.graph(budget)?;
                        let links = graph.neighbors(
                            &symbol(1),
                            GraphDirection::Outgoing,
                            None,
                            10,
                            budget,
                        )?;
                        let edge = graph
                            .edge(links.links.first().ok_or(RocksError::Transition)?, budget)?;
                        let value = transaction.get(
                            StateNamespace::Metadata,
                            b"concurrent-write",
                            budget,
                        )?;
                        assert_batch_snapshot(transaction, budget)?;
                        Ok((before, after, edge.provenance, value))
                    },
                )
                .await
        });
    }
    for entered in started {
        tokio::time::timeout(Duration::from_secs(3), entered).await??;
    }
    let publication = tokio::time::timeout(Duration::from_secs(3), publish_update(&store)).await;
    for gate in gates {
        let _ = gate.send(());
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await {
        results.push(result??);
    }
    publication??;
    assert_eq!(results.len(), 2);
    for (before, after, provenance, value) in results {
        assert_eq!(before, generation(1));
        assert_eq!(after, generation(1));
        assert_eq!(provenance, "before");
        assert_eq!(value, None);
    }
    let current = store
        .state(None, budget()?, 0, 0, &request()?, |transaction, _| {
            transaction.current()
        })
        .await?
        .ok_or("current missing")?;
    assert_eq!(current.generation_id, generation(2));
    runtime.shutdown(&request()?).await?;
    Ok(())
}

async fn publish_update(store: &AsyncProjectStore) -> TestResult {
    store
        .publish_graph(input(2, facts(2, vec![edge(1, 2, "after")])?)?, &request()?)
        .await?;
    store
        .state(
            Some(generation(2)),
            budget()?,
            4096,
            1,
            &request()?,
            |transaction, _| {
                transaction.put(StateNamespace::Metadata, b"concurrent-write", vec![1])
            },
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_multiget_preserves_input_order_and_transaction_local_overrides() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        digest(1),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    store
        .state(
            None,
            budget()?,
            4096,
            4,
            &request()?,
            |transaction, budget| {
                transaction.put(StateNamespace::Metadata, b"one", vec![1])?;
                transaction.put(StateNamespace::Metadata, b"two", vec![2])?;
                assert_eq!(
                    transaction.get_many(
                        StateNamespace::Metadata,
                        &[
                            b"two".to_vec(),
                            b"one".to_vec(),
                            b"two".to_vec(),
                            b"none".to_vec()
                        ],
                        budget
                    )?,
                    vec![Some(vec![2]), Some(vec![1]), Some(vec![2]), None]
                );
                transaction.delete(StateNamespace::Metadata, b"one")?;
                assert_eq!(
                    transaction.get_many(
                        StateNamespace::Metadata,
                        &[b"one".to_vec(), b"two".to_vec()],
                        budget
                    )?,
                    vec![None, Some(vec![2])]
                );
                assert!(
                    transaction
                        .get_many(StateNamespace::Metadata, &[], budget)
                        .is_err()
                );
                assert!(
                    transaction
                        .get_many(
                            StateNamespace::Metadata,
                            &vec![b"two".to_vec(); 129],
                            budget
                        )
                        .is_err()
                );
                Ok(())
            },
        )
        .await?;
    store
        .state(None, budget()?, 0, 0, &request()?, |transaction, budget| {
            assert_eq!(
                transaction.get_many(
                    StateNamespace::Metadata,
                    &[b"one".to_vec(), b"two".to_vec()],
                    budget
                )?,
                vec![None, Some(vec![2])]
            );
            let mut tiny = ReadBudget::new(10, 1, Duration::from_secs(5))?;
            assert!(
                transaction
                    .get_many(StateNamespace::Metadata, &[b"two".to_vec()], &mut tiny)
                    .is_err()
            );
            Ok(())
        })
        .await?;
    runtime.shutdown(&request()?).await?;
    Ok(())
}

fn assert_batch_snapshot(
    transaction: &cartograph_db::rocks::StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<(), RocksError> {
    assert_eq!(
        transaction.get_many(
            StateNamespace::Metadata,
            &[
                b"concurrent-write".to_vec(),
                b"absent".to_vec(),
                b"concurrent-write".to_vec()
            ],
            budget
        )?,
        vec![None, None, None]
    );
    Ok(())
}
