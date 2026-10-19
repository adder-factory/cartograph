use super::*;
use cartograph_db::{
    NativeParseCacheEntry, NativeParseCacheError, NativeParseCacheKey, NativeParseCacheKeyInput,
    NativeParseCacheWrite,
    rocks::{ReadBudget, RocksError, StateNamespace},
};
use cartograph_domain::{NormalizedPath, SourceLanguage};

fn cache_key(database: &NativeDatabase, path: &str) -> Result<NativeParseCacheKey, Box<dyn Error>> {
    Ok(NativeParseCacheKey::new(NativeParseCacheKeyInput {
        project_id: database.project_id().clone(),
        extractor_contract_digest: ContentDigest::from_bytes([1; 32]),
        path: NormalizedPath::parse(path)?,
        language: SourceLanguage::Rust,
        content_hash: ContentDigest::from_bytes([2; 32]),
        source_bytes: 8,
    }))
}

async fn open() -> Result<(tempfile::TempDir, StorageRuntime, NativeDatabase), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let identity = ContentDigest::from_bytes([42; 32]);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &StorageRequest::new(Duration::from_secs(30))?,
    )
    .await?;
    let database = NativeDatabase::new(store, &identity);
    Ok((root, runtime, database))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunked_cache_preserves_immutable_identity_and_atomic_batch_visibility()
-> Result<(), Box<dyn Error>> {
    let (_root, runtime, database) = open().await?;
    let key = cache_key(&database, "src/first.rs")?;
    let payload = vec![37; 9 * 1_048_576 + 11];
    assert_eq!(
        database.store_native_parse_cache(&key, &payload).await?,
        NativeParseCacheWrite::Inserted
    );
    assert_eq!(
        database.store_native_parse_cache(&key, &payload).await?,
        NativeParseCacheWrite::AlreadyPresent
    );
    let loaded = database
        .load_native_parse_cache(&key)
        .await?
        .ok_or("missing chunked payload")?;
    assert_eq!(loaded.payload(), payload);
    let next = cache_key(&database, "src/next.rs")?;
    let conflict = database
        .store_native_parse_cache_batch(vec![
            NativeParseCacheEntry::new(next.clone(), vec![1; 32]),
            NativeParseCacheEntry::new(key.clone(), vec![2; 32]),
        ])
        .await;
    assert_eq!(conflict, Err(NativeParseCacheError::PayloadConflict));
    assert!(database.load_native_parse_cache(&next).await?.is_none());
    assert_eq!(
        database
            .load_native_parse_cache(&key)
            .await?
            .ok_or("old payload disappeared")?
            .payload(),
        payload
    );
    assert!(database.evict_native_parse_cache(&key).await?);
    assert!(!database.evict_native_parse_cache(&key).await?);
    assert!(database.load_native_parse_cache(&key).await?.is_none());
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_cancelled_state_transactions_publish_no_partial_values()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        ContentDigest::from_bytes([3; 32]),
        StorePolicy::default(),
        &request,
    )
    .await?;
    let budget = || ReadBudget::new(100, 1_048_576, Duration::from_secs(30));
    let failed: Result<(), RocksError> = store
        .state(None, budget()?, 4096, 2, &request, |transaction, _| {
            transaction.put(StateNamespace::Metadata, b"first", vec![1])?;
            Err(RocksError::ReplayConflict)
        })
        .await;
    assert_eq!(failed, Err(RocksError::ReplayConflict));
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let worker = control.clone();
    let cancelled = store
        .state(None, budget()?, 4096, 2, &control, move |transaction, _| {
            transaction.put(StateNamespace::Metadata, b"second", vec![2])?;
            worker.cancel();
            Ok(())
        })
        .await;
    assert_eq!(cancelled, Err(RocksError::Cancelled));
    let missing = store
        .state(None, budget()?, 0, 0, &request, |transaction, budget| {
            Ok(transaction
                .get(StateNamespace::Metadata, b"first", budget)?
                .is_none()
                && transaction
                    .get(StateNamespace::Metadata, b"second", budget)?
                    .is_none())
        })
        .await?;
    assert!(missing);
    runtime.shutdown(&request).await?;
    Ok(())
}
