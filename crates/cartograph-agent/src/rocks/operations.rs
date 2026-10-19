//! Project-owned operations survive a dropped request until their work is reaped.

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use cartograph_db::rocks::{RocksError, StorageRequest};
use tokio::{
    sync::oneshot,
    task::{Id, JoinSet},
};

use super::{
    RocksIndexOptions, RocksIndexReport, RocksProjectError, RocksProjectRuntime, RocksQuery,
    RocksQueryResult, RocksStatus,
};

#[derive(Default)]
pub(super) struct OperationRegistry {
    closed: bool,
    failed: bool,
    tasks: JoinSet<()>,
    controls: BTreeMap<Id, StorageRequest>,
}

pub(super) enum OperationDrain {
    Clean,
    Failed,
}

impl OperationRegistry {
    fn reap(&mut self) {
        while let Some(joined) = self.tasks.try_join_next_with_id() {
            let id = match joined {
                Ok((id, ())) => id,
                Err(error) => {
                    self.failed = true;
                    error.id()
                }
            };
            self.controls.remove(&id);
        }
    }
}

struct CancelOnDrop {
    control: StorageRequest,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

impl RocksProjectRuntime {
    /// Advance one bounded native retention batch without publishing a graph.
    ///
    /// # Errors
    /// Returns cancellation, deadline, ownership, or maintenance failures.
    pub async fn maintain_retention(
        self: &Arc<Self>,
        request: &StorageRequest,
    ) -> Result<super::RocksMaintenanceReport, RocksProjectError> {
        self.owned_operation(request, |runtime, control| async move {
            control.check()?;
            let root = runtime.root.clone();
            let policy =
                crate::run_source_worker(move || crate::project_source_policy(&root)).await?;
            let contract = cartograph_indexer::native_parse_cache_contract_digest(
                policy.index.maximum_ast_depth,
            );
            let (graph, graph_error) = runtime.maintain(&control).await;
            let (parse_cache, parse_cache_error, auxiliary, auxiliary_error) =
                runtime.maintain_auxiliary(&contract, &control).await;
            Ok(super::RocksMaintenanceReport {
                graph,
                graph_error,
                parse_cache,
                parse_cache_error,
                auxiliary,
                auxiliary_error,
            })
        })
        .await
    }

    /// Compare live source to the complete current native revision.
    ///
    /// # Errors
    /// Returns bounded admission, source, policy, or native read failures.
    pub async fn status(
        self: &Arc<Self>,
        request: &StorageRequest,
    ) -> Result<RocksStatus, RocksProjectError> {
        self.owned_operation(request, |runtime, control| async move {
            runtime.status_inner(&control).await
        })
        .await
    }

    /// Build and atomically publish a complete native graph.
    ///
    /// Dropping the caller cancels work but leaves its task and writer admission
    /// owned by the project until actual native/source work has completed.
    ///
    /// # Errors
    /// Preserves the previous complete graph on preparation or staging failure.
    pub async fn index(
        self: &Arc<Self>,
        options: RocksIndexOptions,
        request: &StorageRequest,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        self.owned_operation(request, move |runtime, control| async move {
            let progress = options.progress.clone();
            let result = runtime.index_inner(options, &control).await;
            if let Some(progress) = progress {
                progress
                    .finish(result.is_ok(), control.check().is_err())
                    .await;
            }
            result
        })
        .await
    }

    /// Query one pinned revision with complete pre/post live-source checks.
    ///
    /// # Errors
    /// Returns stale, unsupported, unavailable, admission, or finite-budget errors.
    pub async fn query(
        self: &Arc<Self>,
        query: RocksQuery,
        allow_stale: bool,
        request: &StorageRequest,
    ) -> Result<RocksQueryResult, RocksProjectError> {
        self.owned_operation(request, move |runtime, control| async move {
            runtime.query_inner(query, allow_stale, &control).await
        })
        .await
    }

    async fn owned_operation<T, Work, Operation>(
        self: &Arc<Self>,
        request: &StorageRequest,
        work: Work,
    ) -> Result<T, RocksProjectError>
    where
        T: Send + 'static,
        Work: FnOnce(Arc<Self>, StorageRequest) -> Operation + Send + 'static,
        Operation: Future<Output = Result<T, RocksProjectError>> + Send + 'static,
    {
        request.check()?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut registry = self.operations.lock().map_err(|_| operation_error())?;
            registry.reap();
            if registry.closed || registry.failed {
                return Err(operation_error());
            }
            if registry.tasks.len() >= 8 {
                return Err(RocksError::Limit {
                    resource: "project_operations",
                }
                .into());
            }
            let runtime = Arc::clone(self);
            let completion_owner = Arc::clone(self);
            let control = request.clone();
            let token = control.clone();
            let handle = registry.tasks.spawn(async move {
                let result = work(runtime, control).await;
                // A delivered result has no admitted work left to cancel.
                // Remove its control before waking the caller, which may reuse
                // its request while closing and reopening this project.
                if let Ok(mut registry) = completion_owner.operations.lock() {
                    registry.controls.remove(&tokio::task::id());
                }
                let _ = sender.send(result);
            });
            registry.controls.insert(handle.id(), token);
        }
        let mut cancellation = CancelOnDrop {
            control: request.clone(),
            armed: true,
        };
        let result = receiver.await.map_err(|_| operation_error())?;
        cancellation.armed = false;
        result
    }

    pub(super) async fn drain_operations(
        &self,
        request: &StorageRequest,
    ) -> Result<OperationDrain, RocksProjectError> {
        {
            let mut registry = self.operations.lock().map_err(|_| operation_error())?;
            registry.reap();
            registry.closed = true;
            for control in registry.controls.values() {
                control.cancel();
            }
        }
        loop {
            request.check()?;
            {
                let mut registry = self.operations.lock().map_err(|_| operation_error())?;
                registry.reap();
                if registry.tasks.is_empty() {
                    return Ok(if registry.failed {
                        OperationDrain::Failed
                    } else {
                        OperationDrain::Clean
                    });
                }
            }
            // Polling leaves every handle in its owner even if shutdown itself
            // is cancelled or reaches its deadline. A later shutdown retries.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

pub(super) fn operation_error() -> RocksProjectError {
    RocksError::Operation {
        operation: "project_operation_owner",
    }
    .into()
}

impl RocksProjectRuntime {
    /// Commit one explicit, bounded collection batch while preserving current and pinned snapshots.
    /// # Errors
    /// Rejects invalid retention limits, cancellation, active writers or native failures.
    pub async fn prune_generations(
        self: &Arc<Self>,
        keep: u8,
        generations: u16,
        versions: u32,
        request: &StorageRequest,
    ) -> Result<cartograph_db::rocks::PruneReport, RocksProjectError> {
        self.owned_operation(request, move |runtime, control| async move {
            let budget = cartograph_db::rocks::ReadBudget::new(
                20_000,
                64 * 1_048_576,
                Duration::from_secs(30),
            )?;
            runtime
                .store
                .prune(keep, generations, versions, budget, &control)
                .await
                .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feature_index_waits_for_the_watcher_within_the_original_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("lib.rs"), "pub fn current_target() {}\n")?;
        let runtime =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
                .await?;
        for cancelled in [false, true] {
            let writer = runtime.indexing.lock().await;
            let feature = crate::ProjectRuntime::from_native(runtime.clone());
            let cancellation = crate::ProjectCancellation::new();
            let control = cancellation.clone();
            let work = tokio::spawn(async move {
                feature
                    .index_with_cancellation(
                        crate::IndexOptions::automatic().with_history_refresh(false),
                        control,
                    )
                    .await
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !work.is_finished(),
                "feature reconciliation failed while the watcher still owned its writer"
            );
            if cancelled {
                cancellation.cancel();
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(3), work).await??,
                    Err(crate::ProjectError::RequestCancelled)
                ));
                drop(writer);
            } else {
                drop(writer);
                assert!(
                    tokio::time::timeout(Duration::from_secs(3), work)
                        .await??
                        .is_ok()
                );
            }
        }
        runtime.shutdown().await?;
        Ok(())
    }

    struct ReleaseGate(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            if let Ok(mut released) = self.0.0.lock() {
                *released = true;
                self.0.1.notify_all();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicked_operation_still_closes_storage_without_dropping_its_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let failed: Result<(), RocksProjectError> = runtime
            .owned_operation(&request, |_, _| async {
                panic!("injected project operation panic");
            })
            .await;
        assert!(failed.is_err());
        assert!(runtime.shutdown().await.is_err());
        // The original owner remains alive, but its completed failed task
        // must not prevent releasing the primary database and worker pool.
        let replacement =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
                .await?;
        replacement.shutdown().await?;
        assert!(runtime.shutdown().await.is_err());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_index_retains_writer_admission_and_shutdown_until_blocking_work_finishes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let gate = ReleaseGate(Arc::new((Mutex::new(false), Condvar::new())));
        let worker_gate = Arc::clone(&gate.0);
        let (started, started_receiver) = oneshot::channel();
        let caller_runtime = Arc::clone(&runtime);
        let caller_request = request.clone();
        let caller = tokio::spawn(async move {
            caller_runtime
                .owned_operation(&caller_request, move |runtime, _| async move {
                    let _writer = runtime
                        .indexing
                        .try_lock()
                        .map_err(|_| RocksError::WriterBusy)?;
                    tokio::task::spawn_blocking(move || {
                        let _ = started.send(());
                        let released = worker_gate.0.lock().map_err(|_| operation_error())?;
                        let (released, _) = worker_gate
                            .1
                            .wait_timeout_while(released, Duration::from_secs(10), |released| {
                                !*released
                            })
                            .map_err(|_| operation_error())?;
                        if !*released {
                            return Err(operation_error());
                        }
                        Ok(())
                    })
                    .await
                    .map_err(|_| operation_error())?
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), started_receiver).await??;
        caller.abort();
        assert!(caller.await.is_err());
        // Abort yields JoinError rather than a completed operation. The
        // request guard cancelled it, but its actual writer remains occupied.
        assert!(request.check().is_err());
        let second = runtime
            .index(
                RocksIndexOptions::default(),
                &StorageRequest::new(Duration::from_secs(5))?,
            )
            .await;
        assert!(matches!(
            second,
            Err(RocksProjectError::Storage(RocksError::WriterBusy))
        ));
        let drain = runtime
            .drain_operations(&StorageRequest::new(Duration::from_millis(25))?)
            .await;
        assert!(drain.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), runtime.shutdown())
                .await
                .is_err()
        );
        drop(gate);
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown()).await??;
        Ok(())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn facade_status_scan_remains_owned_after_watcher_caller_abort_until_shutdown_reaps_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("lib.rs"), "pub fn target() {}\n")?;
        let runtime =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
                .await?;
        let gate = ReleaseGate(Arc::new((Mutex::new(false), Condvar::new())));
        let pause = Arc::new(super::super::SourceScanPause {
            entered: tokio::sync::Notify::new(),
            gate: gate.0.clone(),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        *runtime.source_pause.lock().map_err(|_| "pause lock")? = Some(pause.clone());
        let facade = Arc::new(crate::ProjectRuntime::from_native(runtime.clone()));
        let scan = tokio::spawn(async move { facade.status().await });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified()).await?;
        scan.abort();
        assert!(scan.await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), runtime.shutdown())
                .await
                .is_err()
        );
        assert!(!pause.finished.load(std::sync::atomic::Ordering::Acquire));
        if let Ok(second) =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(5))?)
                .await
        {
            second.shutdown().await?;
            return Err("primary reopened while source worker remained admitted".into());
        }
        drop(gate);
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown()).await??;
        assert!(pause.finished.load(std::sync::atomic::Ordering::Acquire));
        let replacement =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(5))?)
                .await
                .map_err(|error| {
                    format!("primary reopen after completed source-worker shutdown: {error:?}")
                })?;
        replacement.shutdown().await?;
        Ok(())
    }
}
