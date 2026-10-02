use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cartograph_db::{
    GenerationContents, NativeGenerationSpill, NativeGenerationSpillPolicy, ReadyGeneration,
    SpilledGenerationContents, StagedGeneration, StorageError,
};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    sync::{RwLock, watch},
    time::Instant,
};

use crate::{
    ScopedTask, ScopedTaskError, SupervisedPrepareError, prepare_scope::PrepareScope,
    task_scope::TaskScope,
};

/// Ordered stages in the v2 indexing and publication pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    /// Discover normalized project paths.
    Discover,
    /// Read and hash bounded source bytes.
    Read,
    /// Parse and extract file-local structural facts.
    Parse,
    /// Resolve cross-file symbols and references.
    Resolve,
    /// Reconcile an optional persistent SCIP per-file overlay.
    Overlay,
    /// Canonically reduce unordered worker facts.
    Reduce,
    /// COPY the validated logical generation.
    Copy,
    /// Build relational/derived graph state.
    RelationalMerge,
    /// Maintain the `ParadeDB` BM25 index.
    Bm25,
    /// Generate and persist model-scoped vectors.
    Vector,
    /// Atomically publish the completed generation.
    Publish,
}

impl PipelineStage {
    /// Stable credential-safe stage name used by CLI and MCP diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Read => "read",
            Self::Parse => "parse",
            Self::Resolve => "resolve",
            Self::Overlay => "overlay",
            Self::Reduce => "reduce",
            Self::Copy => "copy",
            Self::RelationalMerge => "relational_merge",
            Self::Bm25 => "bm25",
            Self::Vector => "vector",
            Self::Publish => "publish",
        }
    }
}

impl std::fmt::Display for PipelineStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Exact monotonic wall time spent in one supervisor stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStageTiming {
    stage: PipelineStage,
    elapsed_millis: u64,
}

impl PipelineStageTiming {
    /// Timed native pipeline/publication stage.
    #[must_use]
    pub const fn stage(self) -> PipelineStage {
        self.stage
    }

    /// Saturating monotonic duration for this stage.
    #[must_use]
    pub const fn elapsed_millis(self) -> u64 {
        self.elapsed_millis
    }
}

/// Observable lifecycle for one one-shot supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorState {
    /// Constructed but not yet holding a lease.
    Queued,
    /// Lease-owned work is running and making bounded progress.
    Active,
    /// Cancellation was signalled; the work is inside its grace period or,
    /// past it, still finishing a synchronous section before cleanup.
    Cancelling,
    /// Progress stalled or cooperative cancellation exceeded its grace period.
    Wedged,
    /// Work published successfully and released its exact lease token.
    Completed,
    /// Acquisition, work, durable cleanup, or release failed.
    Failed,
    /// Work cancelled cooperatively and owned cleanup completed.
    Cancelled,
}

/// Stable reason the supervisor asked work to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    /// A local caller explicitly requested cancellation.
    Requested,
    /// The whole-operation deadline elapsed.
    OperationDeadline,
    /// No stage or counter progress arrived before its deadline.
    ProgressStalled,
    /// PostgreSQL rejected the exact lease token after takeover/expiry.
    LeaseLost,
    /// A heartbeat request failed or exceeded its database deadline.
    LeaseHeartbeatFailed,
}

/// Immutable status packet suitable for future CLI/MCP diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SupervisorStatus {
    state: SupervisorState,
    stage: Option<PipelineStage>,
    completed_items: u64,
    completed_bytes: u64,
    heartbeat_count: u64,
    progress_idle_millis: u64,
    cancellation_reason: Option<CancellationReason>,
    grace_exceeded: bool,
    stage_timings: Vec<PipelineStageTiming>,
    total_elapsed_millis: u64,
}

impl SupervisorStatus {
    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> SupervisorState {
        self.state
    }

    /// Current or most recently entered pipeline stage.
    #[must_use]
    pub const fn stage(&self) -> Option<PipelineStage> {
        self.stage
    }

    /// Monotonic completed-item counter.
    #[must_use]
    pub const fn completed_items(&self) -> u64 {
        self.completed_items
    }

    /// Monotonic completed-byte counter.
    #[must_use]
    pub const fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    /// Successful database-side ownership renewals.
    #[must_use]
    pub const fn heartbeat_count(&self) -> u64 {
        self.heartbeat_count
    }

    /// Milliseconds since the latest stage/counter progress update.
    #[must_use]
    pub const fn progress_idle_millis(&self) -> u64 {
        self.progress_idle_millis
    }

    /// Cancellation cause when cancellation has started or completed.
    #[must_use]
    pub const fn cancellation_reason(&self) -> Option<CancellationReason> {
        self.cancellation_reason
    }

    /// Whether work ignored cancellation beyond the configured grace period.
    #[must_use]
    pub const fn grace_exceeded(&self) -> bool {
        self.grace_exceeded
    }

    /// Completed stage durations in exact pipeline order.
    #[must_use]
    pub fn stage_timings(&self) -> &[PipelineStageTiming] {
        &self.stage_timings
    }

    /// Monotonic wall time since this supervisor became active.
    #[must_use]
    pub const fn total_elapsed_millis(&self) -> u64 {
        self.total_elapsed_millis
    }
}

struct ProgressRecord {
    status: SupervisorStatus,
    last_progress: Instant,
    started: bool,
    operation_started: Option<Instant>,
    stage_started: Option<Instant>,
}

/// Supervisor progress shared by pipeline work, the monitor, and status readers.
///
/// The record sits behind Tokio's fair `RwLock`: a pending write acquisition
/// already owns permits, and later readers queue behind it. A task must never
/// await this lock while one of its own suspended futures (for example pipeline
/// work polled inline in a `select!`) holds a pending acquisition, or the two
/// deadlock and every status reader blocks behind them. The supervisor therefore
/// drives pipeline work on its own task.
#[derive(Clone)]
pub(crate) struct SharedProgress {
    record: Arc<RwLock<ProgressRecord>>,
    /// Successful lease renewals, kept outside the record lock so the lease
    /// keeper never waits on (or is woken by) pipeline progress updates.
    heartbeats: Arc<AtomicU64>,
}

impl SharedProgress {
    pub(crate) fn new() -> Self {
        Self {
            record: Arc::new(RwLock::new(ProgressRecord {
                status: SupervisorStatus {
                    state: SupervisorState::Queued,
                    stage: None,
                    completed_items: 0,
                    completed_bytes: 0,
                    heartbeat_count: 0,
                    progress_idle_millis: 0,
                    cancellation_reason: None,
                    grace_exceeded: false,
                    stage_timings: Vec::new(),
                    total_elapsed_millis: 0,
                },
                last_progress: Instant::now(),
                started: false,
                operation_started: None,
                stage_started: None,
            })),
            heartbeats: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn reserve(&self) -> bool {
        let mut record = self.record.write().await;
        if record.started {
            false
        } else {
            record.started = true;
            true
        }
    }

    pub(crate) async fn status(&self) -> SupervisorStatus {
        let record = self.record.read().await;
        let mut status = record.status.clone();
        status.heartbeat_count = self.heartbeats.load(Ordering::Acquire);
        status.progress_idle_millis = millis(record.last_progress.elapsed());
        if let Some(started) = record.operation_started {
            status.total_elapsed_millis = millis(started.elapsed());
        }
        if let (Some(stage), Some(started)) = (status.stage, record.stage_started) {
            push_or_extend_stage_timing(&mut status.stage_timings, stage, started.elapsed());
        }
        status
    }

    pub(crate) async fn mark_active(&self) {
        let mut record = self.record.write().await;
        let now = Instant::now();
        record.status.state = SupervisorState::Active;
        record.status.stage_timings.clear();
        record.status.total_elapsed_millis = 0;
        record.operation_started = Some(now);
        record.stage_started = None;
        record.last_progress = now;
    }

    pub(crate) fn mark_heartbeat(&self) {
        let _ = self
            .heartbeats
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_add(1))
            });
    }

    pub(crate) async fn mark_cancelling(&self, reason: CancellationReason) {
        let mut record = self.record.write().await;
        record.status.state = if reason == CancellationReason::ProgressStalled {
            SupervisorState::Wedged
        } else {
            SupervisorState::Cancelling
        };
        record.status.cancellation_reason = Some(reason);
    }

    pub(crate) async fn mark_completed(&self) {
        let mut record = self.record.write().await;
        finalize_timing(&mut record);
        record.status.state = SupervisorState::Completed;
        record.status.cancellation_reason = None;
        record.status.grace_exceeded = false;
    }

    pub(crate) async fn mark_failed(&self) {
        let mut record = self.record.write().await;
        finalize_timing(&mut record);
        record.status.state = SupervisorState::Failed;
    }

    pub(crate) async fn mark_cancelled(&self, reason: CancellationReason, grace_exceeded: bool) {
        let mut record = self.record.write().await;
        finalize_timing(&mut record);
        record.status.state = if reason == CancellationReason::ProgressStalled || grace_exceeded {
            SupervisorState::Wedged
        } else {
            SupervisorState::Cancelled
        };
        record.status.cancellation_reason = Some(reason);
        record.status.grace_exceeded = grace_exceeded;
    }

    pub(crate) async fn last_progress(&self) -> Instant {
        self.record.read().await.last_progress
    }

    pub(crate) async fn begin_stage(&self, stage: PipelineStage) -> Result<(), ProgressError> {
        let mut record = self.record.write().await;
        if record.status.state != SupervisorState::Active {
            return Err(ProgressError::NotActive);
        }
        if record.status.stage.is_some_and(|current| stage <= current) {
            return Err(ProgressError::StageRegression);
        }
        finalize_current_stage(&mut record);
        record.status.stage = Some(stage);
        let now = Instant::now();
        record.stage_started = Some(now);
        record.last_progress = now;
        Ok(())
    }

    pub(crate) async fn advance(&self, items: u64, bytes: u64) -> Result<(), ProgressError> {
        if items == 0 && bytes == 0 {
            return Err(ProgressError::EmptyIncrement);
        }
        let mut record = self.record.write().await;
        if record.status.state != SupervisorState::Active {
            return Err(ProgressError::NotActive);
        }
        record.status.completed_items = record
            .status
            .completed_items
            .checked_add(items)
            .ok_or(ProgressError::CounterOverflow)?;
        record.status.completed_bytes = record
            .status
            .completed_bytes
            .checked_add(bytes)
            .ok_or(ProgressError::CounterOverflow)?;
        record.last_progress = Instant::now();
        Ok(())
    }
}

fn finalize_current_stage(record: &mut ProgressRecord) {
    if let (Some(stage), Some(started)) = (record.status.stage, record.stage_started.take()) {
        push_or_extend_stage_timing(&mut record.status.stage_timings, stage, started.elapsed());
    }
}

fn finalize_timing(record: &mut ProgressRecord) {
    finalize_current_stage(record);
    if let Some(started) = record.operation_started.take() {
        record.status.total_elapsed_millis = millis(started.elapsed());
    }
}

fn push_or_extend_stage_timing(
    timings: &mut Vec<PipelineStageTiming>,
    stage: PipelineStage,
    elapsed: Duration,
) {
    let elapsed_millis = millis(elapsed);
    if let Some(existing) = timings.last_mut().filter(|timing| timing.stage == stage) {
        existing.elapsed_millis = existing.elapsed_millis.saturating_add(elapsed_millis);
    } else {
        timings.push(PipelineStageTiming {
            stage,
            elapsed_millis,
        });
    }
}

/// Controlled progress reporting surface passed to pipeline work.
#[derive(Clone)]
pub struct ProgressReporter {
    shared: SharedProgress,
}

impl ProgressReporter {
    /// Enter the next strictly ordered stage and reset the progress watchdog.
    /// # Errors
    ///
    /// Returns an error if `stage` is supervisor-owned, progress is inactive,
    /// or the requested stage repeats/regresses the current stage.
    pub async fn begin_stage(&self, stage: PipelineStage) -> Result<(), ProgressError> {
        if stage == PipelineStage::Publish {
            return Err(ProgressError::SupervisorOwnedStage);
        }
        self.shared.begin_stage(stage).await
    }

    /// Add nonzero monotonic item/byte progress and reset the watchdog.
    /// # Errors
    ///
    /// Returns an error if both increments are zero, progress is inactive, or
    /// monotonic item/byte counters overflow.
    pub async fn advance(&self, items: u64, bytes: u64) -> Result<(), ProgressError> {
        self.shared.advance(items, bytes).await
    }
}

/// Cooperative cancellation receiver inherited by every pipeline stage.
#[derive(Clone)]
pub struct CancellationSignal {
    receiver: watch::Receiver<bool>,
}

impl CancellationSignal {
    /// Whether cancellation has already been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Wait until cancellation is requested or the supervisor is dropped.
    pub async fn cancelled(&mut self) {
        while !*self.receiver.borrow_and_update() {
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Context inherited by the supervised indexing future.
#[derive(Clone)]
pub struct SupervisorContext {
    progress: ProgressReporter,
    cancellation: CancellationSignal,
    tasks: TaskScope,
    prepares: PrepareScope,
}

pub(crate) struct SupervisorContextParts {
    pub(crate) shared: SharedProgress,
    pub(crate) receiver: watch::Receiver<bool>,
    pub(crate) tasks: TaskScope,
    pub(crate) prepares: PrepareScope,
}

impl SupervisorContext {
    pub(crate) fn new(parts: SupervisorContextParts) -> Self {
        Self {
            progress: ProgressReporter {
                shared: parts.shared,
            },
            cancellation: CancellationSignal {
                receiver: parts.receiver,
            },
            tasks: parts.tasks,
            prepares: parts.prepares,
        }
    }

    /// Clone the controlled progress reporter.
    #[must_use]
    pub fn progress(&self) -> ProgressReporter {
        self.progress.clone()
    }

    /// Clone the cooperative cancellation receiver.
    #[must_use]
    pub fn cancellation(&self) -> CancellationSignal {
        self.cancellation.clone()
    }

    /// Build a bounded deterministic stage runner bound to this supervisor.
    #[must_use]
    pub fn stages(&self) -> crate::StageRunner {
        crate::StageRunner::new(
            self.progress.shared.clone(),
            self.cancellation.receiver.clone(),
            self.tasks.clone(),
        )
    }

    /// Create spill-only authority for the exact generation owned by this supervisor.
    ///
    /// The returned capability rechecks the live lease and staging state on every
    /// database transaction and cannot publish or terminalize the generation.
    /// # Errors
    ///
    /// Returns an error when the staging token does not match this operation's fence.
    pub fn generation_spill(
        &self,
        generation: &StagedGeneration,
        policy: NativeGenerationSpillPolicy,
    ) -> Result<NativeGenerationSpill, StorageError> {
        self.prepares.spill(generation, policy)
    }

    /// Run the operation's one retained, server-bounded prepare/COPY task.
    ///
    /// Publication and cleanup authority are deliberately absent from this
    /// context; only the supervisor lifecycle gate can perform terminal
    /// generation transitions.
    /// # Errors
    ///
    /// Returns an error if the lease fence is lost, canonical facts fail
    /// storage validation, or transactional COPY/ready transition fails.
    pub async fn prepare_generation(
        &self,
        contents: GenerationContents,
    ) -> Result<ReadyGeneration, SupervisedPrepareError> {
        self.prepares.prepare(contents).await
    }

    /// Run the operation's one retained finalization task for database-spilled facts.
    /// # Errors
    ///
    /// Returns an error if the lease fence is lost, canonical database facts fail
    /// relation/digest validation, or the ready transition fails.
    pub async fn prepare_spilled_generation(
        &self,
        contents: SpilledGenerationContents,
    ) -> Result<ReadyGeneration, SupervisedPrepareError> {
        self.prepares.prepare_spilled(contents).await
    }

    /// Spawn an admitted supervisor-owned worker with an explicit byte reservation.
    ///
    /// The future must return a stage-scoped [`crate::PipelineFailure`]. The
    /// supervisor records that outcome independently of this result handle, so
    /// dropping a failed child cannot permit publication.
    /// # Errors
    ///
    /// Returns an error if the task scope is poisoned/closed or the explicit
    /// byte reservation exceeds bounded task capacity.
    pub fn spawn<T, Work>(
        &self,
        reserved_bytes: u64,
        work: Work,
    ) -> Result<ScopedTask<T>, ScopedTaskError>
    where
        T: Send + 'static,
        Work: std::future::Future<Output = Result<T, crate::PipelineFailure>> + Send + 'static,
    {
        self.tasks.spawn(reserved_bytes, work)
    }
}

/// Invalid or non-monotonic pipeline progress.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgressError {
    /// Work tried to report outside the active state.
    #[error("Cartograph indexing progress is not active")]
    NotActive,
    /// Work re-entered the same or an earlier ordered stage.
    #[error("Cartograph indexing stage progress must be strictly monotonic")]
    StageRegression,
    /// A zero/zero update cannot prove useful work.
    #[error("Cartograph indexing progress increment must be nonzero")]
    EmptyIncrement,
    /// Monotonic counters exceeded their durable representation.
    #[error("Cartograph indexing progress counter overflowed")]
    CounterOverflow,
    /// Publication is an exact-token database transition owned only by the supervisor.
    #[error("Cartograph publication progress is owned by the indexer supervisor")]
    SupervisorOwnedStage,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) async fn hold_progress_write_for_test(
    shared: SharedProgress,
    entered: tokio::sync::oneshot::Sender<()>,
    mut release: watch::Receiver<bool>,
) {
    let _guard = shared.record.write().await;
    let _ = entered.send(());
    while !*release.borrow_and_update() {
        if release.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_stage_names_are_stable_and_display_safe() {
        let expected = [
            (PipelineStage::Discover, "discover"),
            (PipelineStage::Read, "read"),
            (PipelineStage::Parse, "parse"),
            (PipelineStage::Resolve, "resolve"),
            (PipelineStage::Overlay, "overlay"),
            (PipelineStage::Reduce, "reduce"),
            (PipelineStage::Copy, "copy"),
            (PipelineStage::RelationalMerge, "relational_merge"),
            (PipelineStage::Bm25, "bm25"),
            (PipelineStage::Vector, "vector"),
            (PipelineStage::Publish, "publish"),
        ];

        for (stage, name) in expected {
            assert_eq!(stage.as_str(), name);
            assert_eq!(stage.to_string(), name);
        }
    }

    #[tokio::test]
    async fn progress_is_one_shot_ordered_and_monotonic() {
        let shared = SharedProgress::new();
        assert!(shared.reserve().await);
        assert!(!shared.reserve().await);
        shared.mark_active().await;
        let reporter = ProgressReporter {
            shared: shared.clone(),
        };
        assert!(reporter.begin_stage(PipelineStage::Read).await.is_ok());
        assert_eq!(
            reporter.begin_stage(PipelineStage::Publish).await,
            Err(ProgressError::SupervisorOwnedStage)
        );
        assert_eq!(
            reporter.begin_stage(PipelineStage::Discover).await,
            Err(ProgressError::StageRegression)
        );
        assert_eq!(
            reporter.advance(0, 0).await,
            Err(ProgressError::EmptyIncrement)
        );
        assert!(reporter.advance(2, 128).await.is_ok());
        let status = shared.status().await;
        assert_eq!(status.stage(), Some(PipelineStage::Read));
        assert_eq!(status.completed_items(), 2);
        assert_eq!(status.completed_bytes(), 128);
    }
}
