//! Deadline expiry must retain the native atomic cache-header outcome.
use super::*;
use crate::{
    NativeParseCacheKeyInput,
    rocks::{AsyncProjectStore, StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy},
};
use cartograph_domain::{NormalizedPath, SourceLanguage};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_preparation_deadline_drains_native_headers_and_preserves_commit_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    for after_commit in [false, true] {
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
        let key = NativeParseCacheKey::new(NativeParseCacheKeyInput {
            project_id: database.project_id().clone(),
            extractor_contract_digest: ContentDigest::from_bytes([1; 32]),
            path: NormalizedPath::parse("lib.rs")?,
            language: SourceLanguage::Rust,
            content_hash: ContentDigest::from_bytes([2; 32]),
            source_bytes: 8,
        });
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *database
            .store
            .state_gate
            .lock()
            .map_err(|_| "gate poisoned")? = Some(crate::rocks::StateTestGate {
            write_rows: 512,
            after_commit,
            entered,
            release: released,
        });
        let writing = database.clone();
        let entry = NativeParseCacheEntry::new(key.clone(), b"complete-payload".to_vec());
        let work = tokio::spawn(async move {
            NativeDatabase::with_preparation_deadline(
                Duration::from_millis(500),
                writing.prepare_cache(vec![entry]),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let returned_early = work.is_finished();
        release.send(())?;
        let result = tokio::time::timeout(Duration::from_secs(3), work).await??;
        assert!(
            !returned_early,
            "preparation dropped an admitted native header job"
        );
        assert_eq!(
            result.is_ok(),
            after_commit,
            "actual cache commit outcome was lost: {result:?}"
        );
        let visible = database.load_native_parse_cache(&key).await?;
        assert_eq!(visible.is_some(), after_commit);
        if let Some(visible) = visible {
            assert_eq!(visible.payload(), b"complete-payload");
        }
        runtime
            .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    }
    Ok(())
}
