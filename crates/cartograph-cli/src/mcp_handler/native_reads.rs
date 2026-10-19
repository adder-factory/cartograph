//! Retry only read-only generation races, within the caller's original scope.

use cartograph_db::StorageError;
use std::future::Future;

pub(super) async fn retry_generation_read<T, Read, Work>(mut read: Read) -> Result<T, StorageError>
where
    Read: FnMut() -> Work,
    Work: Future<Output = Result<T, StorageError>>,
{
    // Native calls inherit the original request's cancellation and deadline.
    // A continuously publishing writer cannot create an unbounded retry loop.
    for attempt in 0..3 {
        match read().await {
            Err(StorageError::CurrentGenerationChanged) if attempt < 2 => {}
            result => return result,
        }
    }
    Err(StorageError::CurrentGenerationChanged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cartograph_agent::{RocksIndexOptions, RocksProjectRuntime};
    use cartograph_db::{CurrentGenerationLookup, QualifiedSymbolQuery, rocks::StorageRequest};
    use std::{
        error::Error,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publication_between_lookup_and_query_retries_the_actual_native_reader()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        let native = RocksProjectRuntime::open(root.path(), &request).await?;
        native.index(RocksIndexOptions::default(), &request).await?;
        let database = native.database();
        let attempts = AtomicUsize::new(0);
        let page = retry_generation_read(|| async {
            let generation = database
                .current_generation_record(database.project_id())
                .await?
                .ok_or(StorageError::GenerationNotFound)?;
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n").map_err(
                    |_| StorageError::DatabaseOperation {
                        operation: "test-source-edit",
                    },
                )?;
                native
                    .index(RocksIndexOptions::default(), &request)
                    .await
                    .map_err(|_| StorageError::DatabaseOperation {
                        operation: "test-publication",
                    })?;
            }
            database
                .qualified_current_symbols(QualifiedSymbolQuery::new(
                    CurrentGenerationLookup::new(database.project_id(), generation.generation_id()),
                    "replacement",
                    10,
                ))
                .await
        })
        .await?;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(page.total(), 1);
        native.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn repeated_publication_races_are_bounded_and_other_errors_are_not_retried() {
        let mut attempts = 0;
        let result = retry_generation_read::<(), _, _>(|| {
            attempts += 1;
            std::future::ready(Err(StorageError::CurrentGenerationChanged))
        })
        .await;
        assert_eq!(result, Err(StorageError::CurrentGenerationChanged));
        assert_eq!(attempts, 3);
        attempts = 0;
        let result = retry_generation_read::<(), _, _>(|| {
            attempts += 1;
            std::future::ready(Err(StorageError::ImmutableConflict))
        })
        .await;
        assert_eq!(result, Err(StorageError::ImmutableConflict));
        assert_eq!(attempts, 1);
    }
}
