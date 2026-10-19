use cartograph_db::rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy};

use super::*;

#[tokio::test(flavor = "current_thread")]
async fn canonical_publication_uses_storage_workers_and_returns_owned_evidence() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_path_buf(),
        digest(1),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let first_input = input(1, facts(2, vec![edge(1, 2, "native")])?)?;
    let publication = store.publish_graph(first_input, &request()?).await?;
    let cancelled = request()?;
    cancelled.cancel();
    let result = store
        .publish_graph(input(2, facts(2, vec![])?)?, &cancelled)
        .await;
    let failure = result.err().ok_or("cancelled graph input was admitted")?;
    assert_eq!(failure.primary, RocksError::Cancelled);
    assert_eq!(failure.cleanup_failure, None);
    store
        .read(
            Some(publication.revision.generation_id),
            ReadBudget::new(1_000, 1_048_576, Duration::from_secs(30))?,
            &request()?,
            |snapshot, budget| {
                let graph = snapshot.graph(budget)?;
                assert_eq!(graph.info().edges, 1);
                assert_eq!(
                    graph
                        .neighbors(&symbol(1), GraphDirection::Outgoing, None, 10, budget)?
                        .links
                        .len(),
                    1
                );
                Ok(())
            },
        )
        .await?;
    runtime.shutdown(&request()?).await?;
    Ok(())
}
