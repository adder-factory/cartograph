use cartograph_db::rocks::{
    GraphPolicy, GraphPublicationInput, GraphRevision, RocksError, StorageRequest,
};
use std::time::Duration;
use thiserror::Error;
use tokio::{sync::watch, time::Instant};

use crate::{
    CancellationReason, PipelineStage, SupervisorStatus, progress::SharedProgress,
    task_scope::TaskScope,
};

/// One-shot native build and publication progress, shared with its request host.
#[derive(Clone)]
pub struct NativeBuildProgress {
    shared: SharedProgress,
}

impl Default for NativeBuildProgress {
    fn default() -> Self {
        Self {
            shared: SharedProgress::new(),
        }
    }
}

impl std::fmt::Debug for NativeBuildProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeBuildProgress")
            .finish_non_exhaustive()
    }
}

impl NativeBuildProgress {
    /// Start an owner-validated sparse publication without reserving full stages.
    /// # Errors
    /// Rejects observer reuse, including an already completed full build.
    pub async fn publishing_incremental(&self) -> Result<(), RocksError> {
        if !self.shared.reserve().await {
            return Err(RocksError::Operation {
                operation: "native_stage_admission",
            });
        }
        self.shared.mark_active().await;
        self.publishing().await
    }

    /// Current counters and actual monotonic stage timings.
    pub async fn status(&self) -> SupervisorStatus {
        self.shared.status().await
    }

    /// Start the atomic native storage phase after canonical extraction.
    /// # Errors
    /// Rejects reuse of a completed observer or regressing stage order.
    pub async fn publishing(&self) -> Result<(), RocksError> {
        self.shared
            .begin_stage(PipelineStage::Publish)
            .await
            .map_err(|_| RocksError::Operation {
                operation: "native_publication_progress",
            })
    }

    /// Complete only after native work and publication have actually settled.
    pub async fn finish(&self, success: bool, cancelled: bool) {
        if cancelled {
            self.shared
                .mark_cancelled(CancellationReason::Requested, false)
                .await;
        } else if success {
            self.shared.mark_completed().await;
        } else {
            self.shared.mark_failed().await;
        }
    }
}

use crate::{
    NativeGenerationBuild, NativePipelineError, NativePipelineReport, StageRunner,
    build_native_generation_with_scip_and_cache,
};

/// Canonical native extraction or the typed `RocksDB` handoff failed.
#[derive(Debug, Error)]
pub enum NativeRocksGraphError {
    /// Existing discovery, parsing, resolution, and canonical validation failure.
    #[error(transparent)]
    Native(#[from] NativePipelineError),
    /// Bounded typed-storage admission failure, without source or paths.
    #[error(transparent)]
    Storage(#[from] RocksError),
}

/// Build complete native facts and prepare their bounded `RocksDB` publication.
///
/// This reuses the existing supervised discovery, extraction, cross-file
/// resolution, and canonical digest. The blocking handoff measures retained
/// memory before the storage owner admits it; no database I/O occurs here.
/// The caller retains its pipeline reservation until `publish_graph` consumes
/// the input and must revalidate live source according to its runtime policy.
/// Native work retains the supplied runner's cancellation and stage deadlines;
/// `request` controls the publication handoff and storage admission.
///
/// # Errors
/// Returns native pipeline, memory-policy, cancellation, or deadline failures.
pub async fn build_native_rocks_graph(
    runner: &StageRunner,
    build: NativeGenerationBuild,
    identity: GraphRevision,
    policy: GraphPolicy,
    request: &StorageRequest,
) -> Result<(GraphPublicationInput, NativePipelineReport), NativeRocksGraphError> {
    request.check()?;
    let generation = build_native_generation_with_scip_and_cache(runner, build).await?;
    request.check()?;
    let (facts, report) = generation.into_parts();
    let input = tokio::task::block_in_place(|| {
        GraphPublicationInput::new(identity, facts, policy, request)
    })?;
    Ok((input, report))
}

/// Own native stage tasks without a PostgreSQL lease or connection.
///
/// The project `RocksDB` owner supplies exclusive writer admission. This scope
/// reuses the normal ordered stages, byte admission, cancellation, and reaping.
///
/// # Errors
/// Returns build, request, or worker-cleanup errors, preserving build failure.
pub async fn build_native_rocks_graph_owned(
    build: NativeGenerationBuild,
    identity: GraphRevision,
    policy: GraphPolicy,
    request: &StorageRequest,
) -> Result<(GraphPublicationInput, NativePipelineReport), NativeRocksGraphError> {
    let progress = NativeBuildProgress::default();
    let result =
        build_native_rocks_graph_observed(build, identity, policy, request, &progress).await;
    progress
        .finish(result.is_ok(), request.check().is_err())
        .await;
    result
}

/// Build under the project's observer; the owner completes it after publication.
/// # Errors
/// Returns bounded native, storage-admission, cancellation or worker-reaping failures.
pub async fn build_native_rocks_graph_observed(
    build: NativeGenerationBuild,
    identity: GraphRevision,
    policy: GraphPolicy,
    request: &StorageRequest,
    observer: &NativeBuildProgress,
) -> Result<(GraphPublicationInput, NativePipelineReport), NativeRocksGraphError> {
    request.check()?;
    let maximum_bytes = build
        .maximum_stage_reservation_bytes()
        .map_err(|_| RocksError::Limit {
            resource: "native_stage_bytes",
        })?;
    let progress = observer.shared.clone();
    if !progress.reserve().await {
        return Err(RocksError::Operation {
            operation: "native_stage_admission",
        }
        .into());
    }
    progress.mark_active().await;
    let tasks = TaskScope::new(64, maximum_bytes);
    let (cancellation, receiver) = watch::channel(false);
    let owner = NativeGraphTasks {
        tasks,
        cancellation,
    };
    let runner = StageRunner::new(progress, receiver, owner.tasks.clone());
    let result = {
        let work = build_native_rocks_graph(&runner, build, identity, policy, request);
        tokio::pin!(work);
        let mut probe = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = probe.tick() => {
                    if request.check().is_err() {
                        owner.cancellation.send_replace(true);
                    }
                }
            }
        }
    };
    // The project retains this coordinator beyond request cancellation. Keep
    // its scope and admission until blocking native tasks actually finish;
    // shutdown may time out while retaining this unfinished operation.
    let reaped = loop {
        let reaped = owner
            .tasks
            .close_abort_and_reap_retained(Instant::now() + Duration::from_secs(10))
            .await;
        if reaped.all_joined {
            break reaped;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if result.is_ok()
        && (!reaped.all_joined
            || reaped.active_tasks > 0
            || reaped.worker_failed
            || reaped.unobserved_results)
    {
        return Err(RocksError::Operation {
            operation: "native_worker_cleanup",
        }
        .into());
    }
    result
}

struct NativeGraphTasks {
    tasks: TaskScope,
    cancellation: watch::Sender<bool>,
}

impl Drop for NativeGraphTasks {
    fn drop(&mut self) {
        self.cancellation.send_replace(true);
        self.tasks.close_and_abort();
    }
}

#[cfg(test)]
mod tests;
