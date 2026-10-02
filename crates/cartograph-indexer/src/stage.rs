use std::{
    collections::BTreeMap,
    future::Future,
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    sync::{Semaphore, watch},
    task::{AbortHandle, JoinSet},
    time::{Instant, sleep_until, timeout_at},
};

use crate::{
    PipelineFailure, PipelineStage, ProgressError, ScopedTaskError,
    progress::SharedProgress,
    task_scope::{TaskReservationGuard, TaskScope},
};

const MAX_STAGE_CLEANUP_GRACE: Duration = Duration::from_mins(1);

/// Stable contiguous position assigned before parallel stage work begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StageSequence(u64);

impl StageSequence {
    /// Construct a stage-local sequence. A run must begin at zero and remain contiguous.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Numeric position used only for deterministic stage ordering.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Explicit memory and latency contract carried by one stage item.
#[derive(Clone, Copy, Debug)]
pub struct StageItemBudget {
    reserved_bytes: u64,
    progress_bytes: u64,
    deadline: Instant,
}

impl StageItemBudget {
    /// Bind the maximum retained bytes, accounted progress bytes, and absolute deadline.
    #[must_use]
    pub const fn new(reserved_bytes: u64, progress_bytes: u64, deadline: Instant) -> Self {
        Self {
            reserved_bytes,
            progress_bytes,
            deadline,
        }
    }

    /// Memory held from queue admission through ordered reduction.
    #[must_use]
    pub const fn reserved_bytes(self) -> u64 {
        self.reserved_bytes
    }

    /// Source/output bytes added to observable progress after reduction.
    #[must_use]
    pub const fn progress_bytes(self) -> u64 {
        self.progress_bytes
    }

    /// Absolute item deadline, including time waiting behind the concurrency gate.
    #[must_use]
    pub const fn deadline(self) -> Instant {
        self.deadline
    }
}

/// Stable identity and budget metadata preserved across a parallel stage.
#[derive(Clone, Debug)]
pub struct StageItemMeta<Key> {
    sequence: StageSequence,
    key: Key,
    budget: StageItemBudget,
}

impl<Key> StageItemMeta<Key> {
    /// Attach a branded/stable key to its deterministic sequence and budget.
    #[must_use]
    pub const fn new(sequence: StageSequence, key: Key, budget: StageItemBudget) -> Self {
        Self {
            sequence,
            key,
            budget,
        }
    }

    /// Stage-local deterministic sequence.
    #[must_use]
    pub const fn sequence(&self) -> StageSequence {
        self.sequence
    }

    /// Stable caller-owned identity, such as a normalized path or branded file ID.
    #[must_use]
    pub const fn key(&self) -> &Key {
        &self.key
    }

    /// Explicit memory/progress/deadline contract.
    #[must_use]
    pub const fn budget(&self) -> StageItemBudget {
        self.budget
    }
}

/// Input envelope admitted to one bounded parallel stage.
pub struct StageEnvelope<Key, Payload> {
    meta: StageItemMeta<Key>,
    payload: Payload,
}

impl<Key, Payload> StageEnvelope<Key, Payload> {
    /// Pair stable metadata with the stage input payload.
    #[must_use]
    pub const fn new(meta: StageItemMeta<Key>, payload: Payload) -> Self {
        Self { meta, payload }
    }

    #[cfg(test)]
    pub(crate) const fn test_meta(&self) -> &StageItemMeta<Key> {
        &self.meta
    }

    #[cfg(test)]
    pub(crate) const fn test_payload(&self) -> &Payload {
        &self.payload
    }
}

/// Worker-facing item. Metadata cannot be rewritten by the worker.
pub struct StageWorkItem<Key, Payload> {
    sequence: StageSequence,
    key: Key,
    payload: Payload,
    cancellation: StageCancellation,
}

impl<Key, Payload> StageWorkItem<Key, Payload> {
    /// Deterministic position assigned before parallel execution.
    #[must_use]
    pub const fn sequence(&self) -> StageSequence {
        self.sequence
    }

    /// Stable identity for diagnostics and extraction decisions.
    #[must_use]
    pub const fn key(&self) -> &Key {
        &self.key
    }

    /// Cooperative parent/stage/deadline probe for CPU work that cannot yield.
    #[must_use]
    pub fn cancellation(&self) -> StageCancellation {
        self.cancellation.clone()
    }

    /// Consume the work item without exposing mutable metadata.
    #[must_use]
    pub fn into_parts(self) -> (StageSequence, Key, Payload) {
        (self.sequence, self.key, self.payload)
    }
}

/// Cloneable cooperative cancellation probe carried into each stage worker.
#[derive(Clone)]
pub struct StageCancellation {
    parent: watch::Receiver<bool>,
    stage: watch::Receiver<bool>,
    initially_cancelled: bool,
    deadline: Instant,
}

impl StageCancellation {
    fn new(parent: watch::Receiver<bool>, stage: watch::Receiver<bool>, deadline: Instant) -> Self {
        let initially_cancelled = *parent.borrow() || *stage.borrow();
        Self {
            parent,
            stage,
            initially_cancelled,
            deadline,
        }
    }

    /// True after parent cancellation, sibling/stage failure, or the item deadline.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.initially_cancelled
            || monotonic_watch_cancelled(&self.parent)
            || monotonic_watch_cancelled(&self.stage)
            || self.deadline_elapsed()
    }

    fn deadline_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

fn monotonic_watch_cancelled(receiver: &watch::Receiver<bool>) -> bool {
    // Stage/supervisor channels only transition from false to true. Tokio's
    // version probe is an atomic load; the read-lock fallback is needed only
    // after every sender has closed so the retained terminal value stays exact.
    match receiver.has_changed() {
        Ok(changed) => changed,
        Err(_) => *receiver.borrow(),
    }
}

/// Successful worker output delivered to the reducer in sequence order.
pub struct StageOutput<Key, Payload> {
    meta: StageItemMeta<Key>,
    payload: Payload,
}

impl<Key, Payload> StageOutput<Key, Payload> {
    /// Stable metadata preserved from the input envelope.
    #[must_use]
    pub const fn meta(&self) -> &StageItemMeta<Key> {
        &self.meta
    }

    /// Consume the ordered output for deterministic reduction.
    #[must_use]
    pub fn into_parts(self) -> (StageItemMeta<Key>, Payload) {
        (self.meta, self.payload)
    }
}

/// Explicit worker and queued-item limits for one stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageCapacity {
    workers: usize,
    queued_items: usize,
}

impl StageCapacity {
    /// Set the active worker count and additional bounded queue slots.
    #[must_use]
    pub const fn new(workers: usize, queued_items: usize) -> Self {
        Self {
            workers,
            queued_items,
        }
    }

    /// Maximum number of actively executing worker futures.
    #[must_use]
    pub const fn workers(self) -> usize {
        self.workers
    }

    /// Additional admitted items allowed to wait behind active workers.
    #[must_use]
    pub const fn queued_items(self) -> usize {
        self.queued_items
    }
}

/// Run and post-abort time bounds for one ordered parallel stage.
#[derive(Clone, Copy, Debug)]
pub struct StageDeadlinePolicy {
    deadline: Instant,
    cleanup_grace: Duration,
}

impl StageDeadlinePolicy {
    /// Pair the absolute run deadline with a nonzero post-abort cleanup grace.
    #[must_use]
    pub const fn new(deadline: Instant, cleanup_grace: Duration) -> Self {
        Self {
            deadline,
            cleanup_grace,
        }
    }

    /// Absolute whole-stage deadline.
    #[must_use]
    pub const fn deadline(self) -> Instant {
        self.deadline
    }

    /// Separate post-abort worker-reaping grace.
    #[must_use]
    pub const fn cleanup_grace(self) -> Duration {
        self.cleanup_grace
    }
}

/// Immutable execution policy for one ordered parallel stage.
#[derive(Clone, Copy, Debug)]
pub struct StageRunConfig {
    stage: PipelineStage,
    capacity: StageCapacity,
    deadlines: StageDeadlinePolicy,
}

impl StageRunConfig {
    /// Bind hard capacity/deadline policy plus a separate post-abort cleanup grace.
    #[must_use]
    pub const fn new(
        stage: PipelineStage,
        capacity: StageCapacity,
        deadlines: StageDeadlinePolicy,
    ) -> Self {
        Self {
            stage,
            capacity,
            deadlines,
        }
    }
}

/// Streaming input iterator and parallel transform function.
pub struct StageWorkload<Inputs, Work> {
    inputs: Inputs,
    work: Work,
}

impl<Inputs, Work> StageWorkload<Inputs, Work> {
    /// Build a lazily consumed workload; iterator `next` must be bounded and nonblocking.
    ///
    /// Blocking I/O and expensive CPU work belong in the abortable worker future. The
    /// iterator is never advanced beyond the configured admission window.
    #[must_use]
    pub const fn new(inputs: Inputs, work: Work) -> Self {
        Self { inputs, work }
    }
}

/// Initial accumulator and deterministic in-order reducer.
pub struct StageFold<Accumulator, Reduce> {
    accumulator: Accumulator,
    reduce: Reduce,
}

impl<Accumulator, Reduce> StageFold<Accumulator, Reduce> {
    /// Build a bounded nonblocking reducer that receives outputs in exact sequence order.
    ///
    /// Large canonical sorts/merges should be modelled as their own supervised stage rather
    /// than blocking this per-item callback.
    #[must_use]
    pub const fn new(accumulator: Accumulator, reduce: Reduce) -> Self {
        Self {
            accumulator,
            reduce,
        }
    }
}

/// Complete typed request for one bounded stage run.
pub struct StageExecution<Inputs, Work, Accumulator, Reduce> {
    config: StageRunConfig,
    workload: StageWorkload<Inputs, Work>,
    fold: StageFold<Accumulator, Reduce>,
    metrics: Option<StageMetrics>,
}

impl<Inputs, Work, Accumulator, Reduce> StageExecution<Inputs, Work, Accumulator, Reduce> {
    /// Combine the execution policy, lazy workload, and deterministic reducer.
    #[must_use]
    pub const fn new(
        config: StageRunConfig,
        workload: StageWorkload<Inputs, Work>,
        fold: StageFold<Accumulator, Reduce>,
    ) -> Self {
        Self {
            config,
            workload,
            fold,
            metrics: None,
        }
    }

    /// Attach an observable bounded-metric sink for this run.
    #[must_use]
    pub fn with_metrics(mut self, metrics: StageMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

/// Cloneable observer for exact stage admission and reservation high-water marks.
#[derive(Clone, Default)]
pub struct StageMetrics {
    state: Arc<Mutex<StageMetricsSnapshot>>,
}

/// Point-in-time exact stage metrics. A successful run has zero current usage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageMetricsSnapshot {
    admitted_items: u64,
    completed_items: u64,
    current_items: usize,
    current_reserved_bytes: u64,
    peak_items: usize,
    peak_reserved_bytes: u64,
}

impl StageMetrics {
    /// Create an empty metric sink that can be retained after execution moves.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read an internally consistent metric snapshot.
    /// # Errors
    ///
    /// Returns an error if another thread poisoned the metrics mutex.
    pub fn snapshot(&self) -> Result<StageMetricsSnapshot, StageMetricsError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StageMetricsError::Unavailable)?;
        Ok(*state)
    }

    fn admit(&self, reserved_bytes: u64) -> Result<StageMetricReservation, StageMetricsError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StageMetricsError::Unavailable)?;
        state.admit(reserved_bytes)?;
        drop(state);
        Ok(StageMetricReservation {
            metrics: Some(self.clone()),
            reserved_bytes,
        })
    }

    fn release(&self, reserved_bytes: u64, completed: bool) -> Result<(), StageMetricsError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StageMetricsError::Unavailable)?;
        state.release(reserved_bytes, completed)
    }
}

impl StageMetricsSnapshot {
    fn admit(&mut self, reserved_bytes: u64) -> Result<(), StageMetricsError> {
        let admitted_items = self
            .admitted_items
            .checked_add(1)
            .ok_or(StageMetricsError::Overflow)?;
        let current_items = self
            .current_items
            .checked_add(1)
            .ok_or(StageMetricsError::Overflow)?;
        let current_reserved_bytes = self
            .current_reserved_bytes
            .checked_add(reserved_bytes)
            .ok_or(StageMetricsError::Overflow)?;
        self.admitted_items = admitted_items;
        self.current_items = current_items;
        self.current_reserved_bytes = current_reserved_bytes;
        self.peak_items = self.peak_items.max(current_items);
        self.peak_reserved_bytes = self.peak_reserved_bytes.max(current_reserved_bytes);
        Ok(())
    }

    fn release(&mut self, reserved_bytes: u64, completed: bool) -> Result<(), StageMetricsError> {
        let completed_items = if completed {
            self.completed_items
                .checked_add(1)
                .ok_or(StageMetricsError::Overflow)?
        } else {
            self.completed_items
        };
        let current_items = self
            .current_items
            .checked_sub(1)
            .ok_or(StageMetricsError::Underflow)?;
        let current_reserved_bytes = self
            .current_reserved_bytes
            .checked_sub(reserved_bytes)
            .ok_or(StageMetricsError::Underflow)?;
        self.completed_items = completed_items;
        self.current_items = current_items;
        self.current_reserved_bytes = current_reserved_bytes;
        Ok(())
    }
}

struct StageMetricReservation {
    metrics: Option<StageMetrics>,
    reserved_bytes: u64,
}

impl StageMetricReservation {
    fn complete(mut self) -> Result<(), StageMetricsError> {
        self.release(true)
    }

    fn release(&mut self, completed: bool) -> Result<(), StageMetricsError> {
        let Some(metrics) = self.metrics.take() else {
            return Ok(());
        };
        metrics.release(self.reserved_bytes, completed)
    }
}

impl Drop for StageMetricReservation {
    fn drop(&mut self) {
        let _ = self.release(false);
    }
}

impl StageMetricsSnapshot {
    /// Total envelopes admitted during the run.
    #[must_use]
    pub const fn admitted_items(self) -> u64 {
        self.admitted_items
    }

    /// Outputs reduced and acknowledged through progress.
    #[must_use]
    pub const fn completed_items(self) -> u64 {
        self.completed_items
    }

    /// Currently retained task/output reservations.
    #[must_use]
    pub const fn current_items(self) -> usize {
        self.current_items
    }

    /// Currently retained bytes.
    #[must_use]
    pub const fn current_reserved_bytes(self) -> u64 {
        self.current_reserved_bytes
    }

    /// Maximum simultaneously retained envelopes.
    #[must_use]
    pub const fn peak_items(self) -> usize {
        self.peak_items
    }

    /// Maximum simultaneously retained declared bytes.
    #[must_use]
    pub const fn peak_reserved_bytes(self) -> u64 {
        self.peak_reserved_bytes
    }
}

/// Stage metric counters could not be updated or read safely.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum StageMetricsError {
    /// Metric state was poisoned by an unexpected panic.
    #[error("Cartograph stage metrics are unavailable")]
    Unavailable,
    /// A bounded metric counter exceeded its representation.
    #[error("Cartograph stage metrics overflowed")]
    Overflow,
    /// Internal reservation accounting was inconsistent.
    #[error("Cartograph stage metrics underflowed")]
    Underflow,
}

/// Credential-safe fatal item failure returned by a stage worker or reducer.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("Cartograph stage item failed")]
pub struct StageItemFailure;

/// Stable failure provenance for one item after admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageFailureKind {
    /// The worker returned a fatal item failure.
    Worker,
    /// Queue wait plus worker execution exceeded an absolute deadline.
    Deadline,
    /// The structured worker task panicked or ended unexpectedly.
    UnexpectedTaskExit,
}

/// Bounded stage setup, execution, or deterministic reduction failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StageRunError {
    /// Capacity/deadline/stage policy was invalid before useful work began.
    #[error("invalid {field} in Cartograph {stage:?} stage configuration")]
    InvalidConfig {
        /// Stage whose configuration was invalid.
        stage: PipelineStage,
        /// Stable invalid field name.
        field: &'static str,
    },
    /// Input sequences must begin at zero and remain contiguous.
    #[error("Cartograph stage input sequence is not contiguous")]
    InputSequence {
        /// Stage whose input contract failed.
        stage: PipelineStage,
        /// Next required sequence.
        expected: StageSequence,
        /// Sequence actually supplied.
        actual: StageSequence,
    },
    /// Supervisor-owned task/byte admission rejected the item.
    #[error("Cartograph stage admission failed during {stage:?}")]
    Admission {
        /// Stage being admitted.
        stage: PipelineStage,
        /// Structured scope failure with no payload data.
        source: ScopedTaskError,
    },
    /// One admitted item failed with stable provenance.
    #[error("Cartograph stage item {sequence:?} failed during {stage:?}")]
    Item {
        /// Failed stage.
        stage: PipelineStage,
        /// Stable sequence, without exposing a project path.
        sequence: StageSequence,
        /// Timeout, worker error, or unexpected task exit.
        kind: StageFailureKind,
    },
    /// The whole-stage deadline elapsed outside a shorter item deadline.
    #[error("Cartograph stage deadline elapsed during {stage:?}")]
    StageDeadline {
        /// Stage whose whole-run budget elapsed.
        stage: PipelineStage,
    },
    /// The deterministic reducer rejected one ordered output.
    #[error("Cartograph stage reducer rejected item {sequence:?} during {stage:?}")]
    Reduce {
        /// Failed stage.
        stage: PipelineStage,
        /// Stable sequence, without exposing a project path.
        sequence: StageSequence,
    },
    /// Progress reporting violated the supervisor lifecycle contract.
    #[error("Cartograph stage progress failed during {stage:?}")]
    Progress {
        /// Stage being reported.
        stage: PipelineStage,
        /// Monotonic progress failure.
        source: ProgressError,
    },
    /// Observable bounded accounting could not be updated safely.
    #[error("Cartograph stage metrics failed during {stage:?}")]
    Metrics {
        /// Stage whose accounting failed.
        stage: PipelineStage,
        /// Stable metric-accounting failure.
        source: StageMetricsError,
    },
    /// Parent supervision requested cancellation.
    #[error("Cartograph stage was cancelled during {stage:?}")]
    Cancelled {
        /// Stage interrupted by the parent.
        stage: PipelineStage,
    },
    /// A local join task ended unexpectedly.
    #[error("Cartograph stage join failed during {stage:?}")]
    Join {
        /// Stage whose retained join failed.
        stage: PipelineStage,
    },
    /// Aborted local join tasks did not finish inside the separate cleanup grace.
    #[error("Cartograph stage cleanup did not reap all work during {stage:?}")]
    Reap {
        /// Stage whose cleanup bound elapsed.
        stage: PipelineStage,
    },
}

impl StageRunError {
    /// Exact stage retained by every failure that began stage execution.
    #[must_use]
    pub const fn stage(&self) -> PipelineStage {
        match self {
            Self::InvalidConfig { stage, .. }
            | Self::InputSequence { stage, .. }
            | Self::Admission { stage, .. }
            | Self::Item { stage, .. }
            | Self::StageDeadline { stage }
            | Self::Reduce { stage, .. }
            | Self::Progress { stage, .. }
            | Self::Metrics { stage, .. }
            | Self::Cancelled { stage }
            | Self::Join { stage }
            | Self::Reap { stage } => *stage,
        }
    }
}

/// Supervisor-bound entry point for bounded ordered stage execution.
#[derive(Clone)]
pub struct StageRunner {
    progress: SharedProgress,
    cancellation: watch::Receiver<bool>,
    tasks: TaskScope,
}

#[cfg(test)]
pub(crate) async fn test_stage_runner(
    max_tasks: usize,
    max_bytes: u64,
) -> (StageRunner, TaskScope, watch::Sender<bool>) {
    let progress = SharedProgress::new();
    assert!(progress.reserve().await);
    progress.mark_active().await;
    let tasks = TaskScope::new(max_tasks, max_bytes);
    let (cancellation, receiver) = watch::channel(false);
    (
        StageRunner::new(progress, receiver, tasks.clone()),
        tasks,
        cancellation,
    )
}

#[derive(Clone, Copy)]
struct ValidatedStage {
    stage: PipelineStage,
    workers: usize,
    window: usize,
    deadline: Instant,
    cleanup_grace: Duration,
}

enum StageTaskOutcome<Output> {
    Ready(Output),
    Failed(StageFailureKind),
    StageDeadline,
}

struct StageLifecycle {
    tasks: TaskScope,
    cancellation: watch::Receiver<bool>,
    resolved: bool,
}

impl StageLifecycle {
    fn new(tasks: TaskScope, cancellation: watch::Receiver<bool>) -> Self {
        Self {
            tasks,
            cancellation,
            resolved: false,
        }
    }

    fn resolve(&mut self) {
        self.resolved = true;
    }
}

impl Drop for StageLifecycle {
    fn drop(&mut self) {
        if !self.resolved && !*self.cancellation.borrow() {
            let _ = self.tasks.record_stage_failure();
        }
    }
}

struct StageJoin<Key, Output> {
    meta: StageItemMeta<Key>,
    result: Result<(StageTaskOutcome<Output>, StageReservation), ScopedTaskError>,
}

enum StageEvent<Key, Output> {
    Cancelled,
    Deadline,
    Joined(Result<StageJoin<Key, Output>, tokio::task::JoinError>),
    Closed,
}

struct RetainedStageOutput<Key, Output> {
    output: StageOutput<Key, Output>,
    reservation: StageReservation,
}

struct StageReservation {
    task: Option<TaskReservationGuard>,
    metric: Option<StageMetricReservation>,
    cancellation: watch::Receiver<bool>,
}

impl StageReservation {
    fn new(
        task: TaskReservationGuard,
        metric: Option<StageMetricReservation>,
        cancellation: watch::Receiver<bool>,
    ) -> Self {
        Self {
            task: Some(task),
            metric,
            cancellation,
        }
    }

    fn acknowledge(mut self) -> Result<(), StageMetricsError> {
        if let Some(task) = self.task.take() {
            task.acknowledge();
        }
        match self.metric.take() {
            Some(metric) => metric.complete(),
            None => Ok(()),
        }
    }

    fn cancel(mut self) {
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        self.metric.take();
    }
}

impl Drop for StageReservation {
    fn drop(&mut self) {
        if *self.cancellation.borrow()
            && let Some(task) = self.task.take()
        {
            task.cancel();
        }
        self.metric.take();
    }
}

struct StageInputQueue<Inputs, Key, Input> {
    inputs: Inputs,
    pending: Option<StageEnvelope<Key, Input>>,
    expected: Option<u64>,
    exhausted: bool,
}

struct StageSchedule<Key, Output> {
    joins: JoinSet<StageJoin<Key, Output>>,
    aborts: BTreeMap<StageSequence, AbortHandle>,
    ready: BTreeMap<StageSequence, RetainedStageOutput<Key, Output>>,
    cancellation: watch::Sender<bool>,
    lifecycle: StageLifecycle,
}

impl<Key, Output> Drop for StageSchedule<Key, Output> {
    fn drop(&mut self) {
        self.cancel_workers();
        for abort in self.aborts.values() {
            abort.abort();
        }
    }
}

impl<Key, Output> StageSchedule<Key, Output> {
    fn cancel_workers(&self) {
        let _ = self.cancellation.send(true);
    }
}

struct StageOrderedFold<Accumulator, Reduce> {
    accumulator: Accumulator,
    reduce: Reduce,
    next_output: Option<u64>,
}

struct StageWorkers<Work, WorkFuture> {
    work: Arc<Work>,
    gate: Arc<Semaphore>,
    future: PhantomData<fn() -> WorkFuture>,
}

struct StageControl<'a> {
    runner: &'a StageRunner,
    cancellation: watch::Receiver<bool>,
}

struct StageDriver<'a, Key, Input, Output, Inputs, Work, WorkFuture, Accumulator, Reduce> {
    control: StageControl<'a>,
    policy: ValidatedStage,
    input: StageInputQueue<Inputs, Key, Input>,
    schedule: StageSchedule<Key, Output>,
    fold: StageOrderedFold<Accumulator, Reduce>,
    workers: StageWorkers<Work, WorkFuture>,
    metrics: StageMetrics,
}

impl StageRunner {
    pub(crate) const fn new(
        progress: SharedProgress,
        cancellation: watch::Receiver<bool>,
        tasks: TaskScope,
    ) -> Self {
        Self {
            progress,
            cancellation,
            tasks,
        }
    }

    pub(crate) async fn advance_progress(
        &self,
        items: u64,
        bytes: u64,
    ) -> Result<(), ProgressError> {
        self.progress.advance(items, bytes).await
    }

    /// Run a lazily-fed bounded worker window and reduce outputs in input order.
    ///
    /// Dropping this future after it starts poisons the supervisor task scope and aborts
    /// retained workers, so an incomplete stage cannot be ignored before publication.
    /// # Errors
    ///
    /// Returns an error if capacity/config admission fails, cancellation or a
    /// deadline wins, a worker/reducer fails, order is invalid, or cleanup fails.
    pub async fn execute<Key, Input, Output, Inputs, Work, WorkFuture, Accumulator, Reduce>(
        &self,
        execution: StageExecution<Inputs, Work, Accumulator, Reduce>,
    ) -> Result<Accumulator, StageRunError>
    where
        Key: Clone + Send + 'static,
        Input: Send + 'static,
        Output: Send + 'static,
        Inputs: IntoIterator<Item = StageEnvelope<Key, Input>>,
        Inputs::IntoIter: Send,
        Work: Fn(StageWorkItem<Key, Input>) -> WorkFuture + Send + Sync + 'static,
        WorkFuture: Future<Output = Result<Output, StageItemFailure>> + Send + 'static,
        Accumulator: Send,
        Reduce: FnMut(&mut Accumulator, StageOutput<Key, Output>) -> Result<(), StageItemFailure>
            + Send,
    {
        let StageExecution {
            config,
            workload,
            fold,
            metrics,
        } = execution;
        let max_tasks = match self.tasks.max_tasks() {
            Ok(max_tasks) => max_tasks,
            Err(source) => {
                return self.fail(StageRunError::Admission {
                    stage: config.stage,
                    source,
                });
            }
        };
        let policy = match validate_stage(config, max_tasks) {
            Ok(policy) => policy,
            Err(error) => return self.fail(error),
        };
        if *self.cancellation.borrow() {
            return Err(StageRunError::Cancelled {
                stage: policy.stage,
            });
        }
        let mut lifecycle = StageLifecycle::new(self.tasks.clone(), self.cancellation.clone());
        if let Err(source) = self.progress.begin_stage(policy.stage).await {
            if *self.cancellation.borrow() {
                lifecycle.resolve();
                return Err(StageRunError::Cancelled {
                    stage: policy.stage,
                });
            }
            let result = self.fail(StageRunError::Progress {
                stage: policy.stage,
                source,
            });
            lifecycle.resolve();
            return result;
        }
        let StageWorkload { inputs, work } = workload;
        let StageFold {
            accumulator,
            reduce,
        } = fold;
        let (stage_cancellation, _) = watch::channel(false);
        StageDriver {
            control: StageControl {
                runner: self,
                cancellation: self.cancellation.clone(),
            },
            policy,
            input: StageInputQueue {
                inputs: inputs.into_iter(),
                pending: None,
                expected: Some(0),
                exhausted: false,
            },
            schedule: StageSchedule {
                joins: JoinSet::new(),
                aborts: BTreeMap::new(),
                ready: BTreeMap::new(),
                cancellation: stage_cancellation,
                lifecycle,
            },
            fold: StageOrderedFold {
                accumulator,
                reduce,
                next_output: Some(0),
            },
            workers: StageWorkers {
                work: Arc::new(work),
                gate: Arc::new(Semaphore::new(policy.workers)),
                future: PhantomData,
            },
            metrics: metrics.unwrap_or_default(),
        }
        .run()
        .await
    }

    fn fail<T>(&self, error: StageRunError) -> Result<T, StageRunError> {
        let _ = self.tasks.record_stage_failure();
        Err(error)
    }
}

impl<Key, Input, Output, Inputs, Work, WorkFuture, Accumulator, Reduce>
    StageDriver<'_, Key, Input, Output, Inputs, Work, WorkFuture, Accumulator, Reduce>
where
    Key: Clone + Send + 'static,
    Input: Send + 'static,
    Output: Send + 'static,
    Inputs: Iterator<Item = StageEnvelope<Key, Input>>,
    Work: Fn(StageWorkItem<Key, Input>) -> WorkFuture + Send + Sync + 'static,
    WorkFuture: Future<Output = Result<Output, StageItemFailure>> + Send + 'static,
    Accumulator: Send,
    Reduce:
        FnMut(&mut Accumulator, StageOutput<Key, Output>) -> Result<(), StageItemFailure> + Send,
{
    async fn run(mut self) -> Result<Accumulator, StageRunError> {
        loop {
            if let Err(error) = self.prepare_iteration() {
                return self.stop(error).await;
            }
            if stage_is_complete(&self.input, &self.schedule) {
                if Instant::now() >= self.policy.deadline {
                    let stage = self.policy.stage;
                    return self.stop(StageRunError::StageDeadline { stage }).await;
                }
                self.schedule.lifecycle.resolve();
                return Ok(self.fold.accumulator);
            }
            let joined = match wait_for_stage_event(
                &mut self.control.cancellation,
                &mut self.schedule.joins,
                self.policy.deadline,
            )
            .await
            {
                StageEvent::Cancelled => {
                    let stage = self.policy.stage;
                    return self.stop(StageRunError::Cancelled { stage }).await;
                }
                StageEvent::Deadline => {
                    let stage = self.policy.stage;
                    return self.stop(StageRunError::StageDeadline { stage }).await;
                }
                StageEvent::Joined(joined) => joined,
                StageEvent::Closed => {
                    let stage = self.policy.stage;
                    return self.stop(StageRunError::Join { stage }).await;
                }
            };
            if let Err(error) = self.accept_join(joined) {
                return self.stop(error).await;
            }
            if let Err(error) = self.reduce_ready().await {
                return self.stop(error).await;
            }
        }
    }

    fn prepare_iteration(&mut self) -> Result<(), StageRunError> {
        if *self.control.cancellation.borrow_and_update() {
            return Err(StageRunError::Cancelled {
                stage: self.policy.stage,
            });
        }
        if Instant::now() >= self.policy.deadline {
            return Err(StageRunError::StageDeadline {
                stage: self.policy.stage,
            });
        }
        self.fill_window()
    }

    fn fill_window(&mut self) -> Result<(), StageRunError> {
        while self.schedule.joins.len() + self.schedule.ready.len() < self.policy.window
            && !self.input.exhausted
        {
            if Instant::now() >= self.policy.deadline {
                return Err(StageRunError::StageDeadline {
                    stage: self.policy.stage,
                });
            }
            if self.input.pending.is_none() {
                let envelope = self.input.inputs.next();
                if Instant::now() >= self.policy.deadline {
                    return Err(StageRunError::StageDeadline {
                        stage: self.policy.stage,
                    });
                }
                let Some(envelope) = envelope else {
                    self.input.exhausted = true;
                    break;
                };
                let expected = self.input.expected.ok_or(StageRunError::InvalidConfig {
                    stage: self.policy.stage,
                    field: "input_sequence",
                })?;
                let actual = envelope.meta.sequence();
                if actual != StageSequence::new(expected) {
                    return Err(StageRunError::InputSequence {
                        stage: self.policy.stage,
                        expected: StageSequence::new(expected),
                        actual,
                    });
                }
                self.input.expected = expected.checked_add(1);
                self.input.pending = Some(envelope);
            }
            let reserved_bytes = self
                .input
                .pending
                .as_ref()
                .map(|envelope| envelope.meta.budget().reserved_bytes())
                .ok_or(StageRunError::Join {
                    stage: self.policy.stage,
                })?;
            match self.control.runner.tasks.probe_admission(reserved_bytes) {
                Ok(()) => {}
                Err(
                    ScopedTaskError::TaskCapacityExceeded | ScopedTaskError::ByteCapacityExceeded,
                ) if !self.schedule.joins.is_empty() => break,
                Err(source) => {
                    return Err(StageRunError::Admission {
                        stage: self.policy.stage,
                        source,
                    });
                }
            }
            let envelope = self.input.pending.take().ok_or(StageRunError::Join {
                stage: self.policy.stage,
            })?;
            self.spawn_envelope(envelope)?;
        }
        Ok(())
    }

    fn spawn_envelope(&mut self, envelope: StageEnvelope<Key, Input>) -> Result<(), StageRunError> {
        let StageEnvelope { meta, payload } = envelope;
        let sequence = meta.sequence();
        let budget = meta.budget();
        let deadline = budget.deadline().min(self.policy.deadline);
        let stage_deadline_wins = self.policy.deadline <= budget.deadline();
        let item = StageWorkItem {
            sequence,
            key: meta.key().clone(),
            payload,
            cancellation: StageCancellation::new(
                self.control.cancellation.clone(),
                self.schedule.cancellation.subscribe(),
                deadline,
            ),
        };
        let work_cancellation = item.cancellation();
        let gate = self.workers.gate.clone();
        let work = self.workers.work.clone();
        let task = self
            .control
            .runner
            .tasks
            .spawn(budget.reserved_bytes(), async move {
                let result = timeout_at(deadline, async move {
                    let permit = gate.acquire_owned().await.map_err(|_| StageItemFailure)?;
                    let output = work(item).await;
                    drop(permit);
                    output
                })
                .await;
                let outcome = match result {
                    Ok(Ok(output)) => StageTaskOutcome::Ready(output),
                    Ok(Err(_)) if work_cancellation.deadline_elapsed() && stage_deadline_wins => {
                        StageTaskOutcome::StageDeadline
                    }
                    Ok(Err(_)) if work_cancellation.deadline_elapsed() => {
                        StageTaskOutcome::Failed(StageFailureKind::Deadline)
                    }
                    Ok(Err(_)) => StageTaskOutcome::Failed(StageFailureKind::Worker),
                    Err(_) if stage_deadline_wins => StageTaskOutcome::StageDeadline,
                    Err(_) => StageTaskOutcome::Failed(StageFailureKind::Deadline),
                };
                Ok::<_, PipelineFailure>(outcome)
            })
            .map_err(|source| StageRunError::Admission {
                stage: self.policy.stage,
                source,
            })?;
        let metric = match self.metrics.admit(budget.reserved_bytes()) {
            Ok(metric) => metric,
            Err(source) => {
                let abort = task.abort_handle();
                self.schedule.aborts.insert(sequence, abort.clone());
                let cancellation = self.control.cancellation.clone();
                self.schedule.joins.spawn(async move {
                    StageJoin {
                        meta,
                        result: task.join_retaining_reservation().await.map(
                            |(outcome, reservation)| {
                                (
                                    outcome,
                                    StageReservation::new(reservation, None, cancellation),
                                )
                            },
                        ),
                    }
                });
                abort.abort();
                return Err(StageRunError::Metrics {
                    stage: self.policy.stage,
                    source,
                });
            }
        };
        self.schedule.aborts.insert(sequence, task.abort_handle());
        let cancellation = self.control.cancellation.clone();
        self.schedule.joins.spawn(async move {
            StageJoin {
                meta,
                result: task
                    .join_retaining_reservation()
                    .await
                    .map(|(outcome, reservation)| {
                        (
                            outcome,
                            StageReservation::new(reservation, Some(metric), cancellation),
                        )
                    }),
            }
        });
        Ok(())
    }

    fn accept_join(
        &mut self,
        joined: Result<StageJoin<Key, Output>, tokio::task::JoinError>,
    ) -> Result<(), StageRunError> {
        let joined = joined.map_err(|_| StageRunError::Join {
            stage: self.policy.stage,
        })?;
        let sequence = joined.meta.sequence();
        self.schedule.aborts.remove(&sequence);
        match joined.result {
            Ok((StageTaskOutcome::Ready(payload), reservation)) => {
                self.schedule.ready.insert(
                    sequence,
                    RetainedStageOutput {
                        output: StageOutput {
                            meta: joined.meta,
                            payload,
                        },
                        reservation,
                    },
                );
                Ok(())
            }
            Ok((StageTaskOutcome::Failed(kind), _)) => Err(StageRunError::Item {
                stage: self.policy.stage,
                sequence,
                kind,
            }),
            Ok((StageTaskOutcome::StageDeadline, _)) => Err(StageRunError::StageDeadline {
                stage: self.policy.stage,
            }),
            Err(ScopedTaskError::Cancelled) if *self.control.cancellation.borrow() => {
                Err(StageRunError::Cancelled {
                    stage: self.policy.stage,
                })
            }
            Err(_) => Err(StageRunError::Item {
                stage: self.policy.stage,
                sequence,
                kind: StageFailureKind::UnexpectedTaskExit,
            }),
        }
    }

    async fn reduce_ready(&mut self) -> Result<(), StageRunError> {
        while let Some(sequence) = self.fold.next_output.map(StageSequence::new) {
            if Instant::now() >= self.policy.deadline {
                return Err(StageRunError::StageDeadline {
                    stage: self.policy.stage,
                });
            }
            let Some(retained) = self.schedule.ready.remove(&sequence) else {
                break;
            };
            let progress_bytes = retained.output.meta.budget().progress_bytes();
            (self.fold.reduce)(&mut self.fold.accumulator, retained.output).map_err(|_| {
                StageRunError::Reduce {
                    stage: self.policy.stage,
                    sequence,
                }
            })?;
            if *self.control.cancellation.borrow() {
                retained.reservation.cancel();
                return Err(StageRunError::Cancelled {
                    stage: self.policy.stage,
                });
            }
            if Instant::now() >= self.policy.deadline {
                return Err(StageRunError::StageDeadline {
                    stage: self.policy.stage,
                });
            }
            match timeout_at(
                self.policy.deadline,
                self.control.runner.progress.advance(1, progress_bytes),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(source)) => {
                    if *self.control.cancellation.borrow() {
                        retained.reservation.cancel();
                        return Err(StageRunError::Cancelled {
                            stage: self.policy.stage,
                        });
                    }
                    return Err(StageRunError::Progress {
                        stage: self.policy.stage,
                        source,
                    });
                }
                Err(_) if *self.control.cancellation.borrow() => {
                    retained.reservation.cancel();
                    return Err(StageRunError::Cancelled {
                        stage: self.policy.stage,
                    });
                }
                Err(_) => {
                    return Err(StageRunError::StageDeadline {
                        stage: self.policy.stage,
                    });
                }
            }
            retained
                .reservation
                .acknowledge()
                .map_err(|source| StageRunError::Metrics {
                    stage: self.policy.stage,
                    source,
                })?;
            self.fold.next_output = sequence.value().checked_add(1);
        }
        Ok(())
    }

    async fn stop(mut self, error: StageRunError) -> Result<Accumulator, StageRunError> {
        let cancelled = matches!(error, StageRunError::Cancelled { .. });
        self.schedule.cancel_workers();
        cancel_ready_outputs(&mut self.schedule.ready);
        let all_joined = abort_and_reap(
            &self.schedule.aborts,
            &mut self.schedule.joins,
            cleanup_deadline(self.policy.cleanup_grace),
        )
        .await;
        let result = if !all_joined {
            self.control.runner.fail(StageRunError::Reap {
                stage: self.policy.stage,
            })
        } else if cancelled {
            Err(error)
        } else {
            self.control.runner.fail(error)
        };
        self.schedule.lifecycle.resolve();
        result
    }
}

fn validate_stage(
    config: StageRunConfig,
    max_tasks: usize,
) -> Result<ValidatedStage, StageRunError> {
    if config.stage == PipelineStage::Publish {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "stage",
        });
    }
    if config.capacity.workers == 0 {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "workers",
        });
    }
    let window = config
        .capacity
        .workers
        .checked_add(config.capacity.queued_items)
        .ok_or(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "queued_items",
        })?;
    if window > max_tasks {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "capacity",
        });
    }
    if config.capacity.workers > Semaphore::MAX_PERMITS {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "workers",
        });
    }
    if Instant::now() >= config.deadlines.deadline {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "deadline",
        });
    }
    if config.deadlines.cleanup_grace.is_zero()
        || config.deadlines.cleanup_grace > MAX_STAGE_CLEANUP_GRACE
    {
        return Err(StageRunError::InvalidConfig {
            stage: config.stage,
            field: "cleanup_grace",
        });
    }
    Ok(ValidatedStage {
        stage: config.stage,
        workers: config.capacity.workers,
        window,
        deadline: config.deadlines.deadline,
        cleanup_grace: config.deadlines.cleanup_grace,
    })
}

fn cleanup_deadline(cleanup_grace: Duration) -> Instant {
    Instant::now()
        .checked_add(cleanup_grace)
        .unwrap_or_else(Instant::now)
}

fn stage_is_complete<Inputs, Key, Input, Output>(
    input: &StageInputQueue<Inputs, Key, Input>,
    schedule: &StageSchedule<Key, Output>,
) -> bool {
    input.exhausted
        && input.pending.is_none()
        && schedule.joins.is_empty()
        && schedule.ready.is_empty()
}

fn cancel_ready_outputs<Key, Output>(
    ready: &mut BTreeMap<StageSequence, RetainedStageOutput<Key, Output>>,
) {
    for retained in std::mem::take(ready).into_values() {
        retained.reservation.cancel();
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow_and_update() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_for_stage_event<Key: 'static, Output: 'static>(
    cancellation: &mut watch::Receiver<bool>,
    joins: &mut JoinSet<StageJoin<Key, Output>>,
    deadline: Instant,
) -> StageEvent<Key, Output> {
    tokio::select! {
        biased;
        () = wait_for_cancellation(cancellation) => StageEvent::Cancelled,
        () = sleep_until(deadline) => StageEvent::Deadline,
        joined = joins.join_next() => match joined {
            Some(joined) => StageEvent::Joined(joined),
            None => StageEvent::Closed,
        },
    }
}

async fn abort_and_reap<Key: 'static, Output: 'static>(
    aborts: &BTreeMap<StageSequence, AbortHandle>,
    joins: &mut JoinSet<StageJoin<Key, Output>>,
    deadline: Instant,
) -> bool {
    for abort in aborts.values() {
        abort.abort();
    }
    timeout_at(deadline, async {
        while let Some(joined) = joins.join_next().await {
            if let Ok(StageJoin {
                result: Ok((_, reservation)),
                ..
            }) = joined
            {
                reservation.cancel();
            }
        }
    })
    .await
    .is_ok()
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::{
        future::pending,
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::time::{sleep, timeout};

    use super::test_stage_runner as runner;
    use super::*;

    const TEST_WORKERS: usize = 2;
    const TEST_QUEUE: usize = 3;
    const TEST_ITEMS: usize = 10;
    const TEST_SCOPE_BYTES: u64 = 128;
    const ITEM_BYTES: u64 = 4;
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const ITEM_TIMEOUT: Duration = Duration::from_secs(1);
    const DEADLINE_TIMEOUT: Duration = Duration::from_millis(30);
    const CLEANUP_GRACE: Duration = Duration::from_millis(250);

    fn meta(sequence: usize, deadline: Instant) -> StageItemMeta<usize> {
        let key = sequence;
        let sequence = u64::try_from(sequence).unwrap_or(u64::MAX);
        StageItemMeta::new(
            StageSequence::new(sequence),
            key,
            StageItemBudget::new(ITEM_BYTES, ITEM_BYTES, deadline),
        )
    }

    fn config(workers: usize, queued_items: usize, deadline: Instant) -> StageRunConfig {
        StageRunConfig::new(
            PipelineStage::Parse,
            StageCapacity::new(workers, queued_items),
            StageDeadlinePolicy::new(deadline, CLEANUP_GRACE),
        )
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        let completed = timeout(TEST_TIMEOUT, async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(completed.is_ok(), "stage test condition timed out");
    }

    #[test]
    fn cancellation_probe_uses_monotonic_watch_versions_without_changing_closed_state() {
        let (parent, parent_receiver) = watch::channel(false);
        let (stage, stage_receiver) = watch::channel(false);
        let cancellation = StageCancellation::new(
            parent_receiver,
            stage_receiver,
            Instant::now() + TEST_TIMEOUT,
        );
        assert!(!cancellation.is_cancelled());
        parent.send_replace(true);
        assert!(cancellation.is_cancelled());

        let (already_parent, already_parent_receiver) = watch::channel(true);
        let (open_stage, open_stage_receiver) = watch::channel(false);
        let already_cancelled = StageCancellation::new(
            already_parent_receiver,
            open_stage_receiver,
            Instant::now() + TEST_TIMEOUT,
        );
        assert!(already_cancelled.is_cancelled());
        drop(already_parent);
        drop(open_stage);

        let (closed_parent, closed_parent_receiver) = watch::channel(false);
        let (closed_stage, closed_stage_receiver) = watch::channel(false);
        let closed_but_not_cancelled = StageCancellation::new(
            closed_parent_receiver,
            closed_stage_receiver,
            Instant::now() + TEST_TIMEOUT,
        );
        drop(closed_parent);
        drop(closed_stage);
        assert!(!closed_but_not_cancelled.is_cancelled());

        drop(parent);
        drop(stage);
    }

    #[tokio::test]
    async fn parallel_completion_is_reduced_in_exact_input_sequence() {
        let (runner, tasks, cancellation) = runner(TEST_ITEMS, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let inputs =
            (0..TEST_ITEMS).map(|sequence| StageEnvelope::new(meta(sequence, deadline), sequence));
        let execution = StageExecution::new(
            config(TEST_WORKERS, TEST_QUEUE, deadline),
            StageWorkload::new(
                inputs,
                move |item: StageWorkItem<usize, usize>| async move {
                    let (sequence, _, value) = item.into_parts();
                    let position = usize::try_from(sequence.value()).unwrap_or(usize::MAX);
                    let reverse = TEST_ITEMS.saturating_sub(position);
                    let reverse_millis = u64::try_from(reverse).unwrap_or(u64::MAX);
                    sleep(Duration::from_millis(reverse_millis)).await;
                    Ok::<_, StageItemFailure>(value * 2)
                },
            ),
            StageFold::new(
                Vec::new(),
                |ordered: &mut Vec<(u64, usize, usize)>, output: StageOutput<usize, usize>| {
                    let (meta, value) = output.into_parts();
                    ordered.push((meta.sequence().value(), *meta.key(), value));
                    Ok(())
                },
            ),
        );
        let ordered = runner.execute(execution).await;
        let ordered = match ordered {
            Ok(ordered) => ordered,
            Err(error) => panic!("ordered parallel stage failed: {error}"),
        };
        assert_eq!(ordered.len(), TEST_ITEMS);
        assert!(ordered.iter().enumerate().all(|(expected, item)| {
            item.0 == u64::try_from(expected).unwrap_or(u64::MAX)
                && item.1 == expected
                && item.2 == expected * 2
        }));
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test]
    async fn worker_window_never_consumes_beyond_active_plus_queue_capacity() {
        let window = TEST_WORKERS + TEST_QUEUE;
        let (runner, tasks, cancellation) = runner(window, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let consumed = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (release, release_receiver) = watch::channel(false);
        let source_count = consumed.clone();
        let inputs = (0..TEST_ITEMS)
            .inspect(move |_| {
                source_count.fetch_add(1, Ordering::AcqRel);
            })
            .map(move |sequence| StageEnvelope::new(meta(sequence, deadline), sequence));
        let worker_maximum = maximum.clone();
        let execution = StageExecution::new(
            config(TEST_WORKERS, TEST_QUEUE, deadline),
            StageWorkload::new(inputs, move |item: StageWorkItem<usize, usize>| {
                let active = active.clone();
                let maximum = worker_maximum.clone();
                let mut release_receiver = release_receiver.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum.fetch_max(current, Ordering::AcqRel);
                    while !*release_receiver.borrow_and_update() {
                        if release_receiver.changed().await.is_err() {
                            return Err(StageItemFailure);
                        }
                    }
                    active.fetch_sub(1, Ordering::AcqRel);
                    Ok::<_, StageItemFailure>(item.into_parts().2)
                }
            }),
            StageFold::new(0_usize, |count: &mut usize, _| {
                *count += 1;
                Ok(())
            }),
        );
        let handle = tokio::spawn(async move { runner.execute(execution).await });
        wait_until(|| maximum.load(Ordering::Acquire) == TEST_WORKERS).await;
        assert_eq!(maximum.load(Ordering::Acquire), TEST_WORKERS);
        assert_eq!(consumed.load(Ordering::Acquire), window);
        release.send_replace(true);
        let result = handle.await;
        assert_matches!(result, Ok(Ok(TEST_ITEMS)));
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
    }

    #[tokio::test]
    async fn byte_budget_backpressures_without_consuming_the_remaining_iterator() {
        let window = TEST_WORKERS + TEST_QUEUE;
        let (runner, tasks, cancellation) = runner(window, ITEM_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let consumed = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let source_count = consumed.clone();
        let inputs = (0..TEST_ITEMS)
            .inspect(move |_| {
                source_count.fetch_add(1, Ordering::AcqRel);
            })
            .map(move |sequence| StageEnvelope::new(meta(sequence, deadline), sequence));
        let worker_maximum = maximum.clone();
        let execution = StageExecution::new(
            config(TEST_WORKERS, TEST_QUEUE, deadline),
            StageWorkload::new(inputs, move |item: StageWorkItem<usize, usize>| {
                let active = active.clone();
                let maximum = worker_maximum.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum.fetch_max(current, Ordering::AcqRel);
                    sleep(Duration::from_millis(5)).await;
                    active.fetch_sub(1, Ordering::AcqRel);
                    Ok::<_, StageItemFailure>(item.into_parts().2)
                }
            }),
            StageFold::new(0_usize, |count: &mut usize, _| {
                *count += 1;
                Ok(())
            }),
        );
        let result = runner.execute(execution).await;
        assert_eq!(result, Ok(TEST_ITEMS));
        assert_eq!(maximum.load(Ordering::Acquire), 1);
        assert_eq!(consumed.load(Ordering::Acquire), TEST_ITEMS);
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test]
    async fn cancellation_returns_before_noncooperative_work_and_scope_reaps_it() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_dropped = dropped.clone();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let inputs = [StageEnvelope::new(meta(0, deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, move |_| {
                let worker_dropped = worker_dropped.clone();
                let worker_started = worker_started.clone();
                async move {
                    struct DropFlag(Arc<AtomicBool>);
                    impl Drop for DropFlag {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _flag = DropFlag(worker_dropped);
                    worker_started.store(true, Ordering::Release);
                    pending::<Result<(), StageItemFailure>>().await
                }
            }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        let handle = tokio::spawn(async move { runner.execute(execution).await });
        wait_until(|| started.load(Ordering::Acquire)).await;
        assert!(started.load(Ordering::Acquire));
        cancellation.send_replace(true);
        assert_matches!(handle.await, Ok(Err(StageRunError::Cancelled { .. })));
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancellation_before_stage_entry_consumes_nothing_and_is_not_failure() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let consumed = Arc::new(AtomicUsize::new(0));
        let source_count = consumed.clone();
        let inputs = [StageEnvelope::new(meta(0, deadline), ())]
            .into_iter()
            .inspect(move |_| {
                source_count.fetch_add(1, Ordering::AcqRel);
            });
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        cancellation.send_replace(true);
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Cancelled { .. })
        );
        assert_eq!(consumed.load(Ordering::Acquire), 0);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test]
    async fn item_deadline_is_fatal_and_visible_to_the_supervisor_scope() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let stage_deadline = Instant::now() + TEST_TIMEOUT;
        let item_deadline = Instant::now() + DEADLINE_TIMEOUT;
        let inputs = [StageEnvelope::new(meta(0, item_deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, stage_deadline),
            StageWorkload::new(inputs, |_| pending::<Result<(), StageItemFailure>>()),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Item {
                sequence,
                kind: StageFailureKind::Deadline,
                ..
            }) if sequence == StageSequence::new(0)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(stage_deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test]
    async fn empty_stage_completes_without_synthetic_progress() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let inputs = std::iter::empty::<StageEnvelope<usize, ()>>();
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
            StageFold::new(7_usize, |_: &mut usize, _| Ok(())),
        );
        assert_eq!(runner.execute(execution).await, Ok(7));
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test]
    async fn whole_stage_deadline_covers_input_iteration_and_ordered_reduction() {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum SlowPoint {
            InputItem,
            InputExhaustion,
            Reducer,
        }

        for slow_point in [
            SlowPoint::InputItem,
            SlowPoint::InputExhaustion,
            SlowPoint::Reducer,
        ] {
            let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
            let stage_deadline = Instant::now() + DEADLINE_TIMEOUT;
            let item_deadline = Instant::now() + ITEM_TIMEOUT;
            let mut first = true;
            let inputs = std::iter::from_fn(move || {
                if !first {
                    return None;
                }
                first = false;
                if slow_point != SlowPoint::Reducer {
                    std::thread::sleep(DEADLINE_TIMEOUT * 2);
                }
                if slow_point == SlowPoint::InputExhaustion {
                    None
                } else {
                    Some(StageEnvelope::new(meta(0, item_deadline), ()))
                }
            });
            let execution = StageExecution::new(
                config(1, 0, stage_deadline),
                StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
                StageFold::new((), move |(): &mut (), _| {
                    if slow_point == SlowPoint::Reducer {
                        std::thread::sleep(DEADLINE_TIMEOUT * 2);
                    }
                    Ok(())
                }),
            );
            assert_matches!(
                runner.execute(execution).await,
                Err(StageRunError::StageDeadline {
                    stage: PipelineStage::Parse
                })
            );
            drop(cancellation);
            let report = tasks
                .close_abort_and_reap(Instant::now() + CLEANUP_GRACE)
                .await;
            assert!(report.all_joined);
            assert!(report.worker_failed);
        }
    }

    #[tokio::test]
    async fn stage_deadline_reaps_cooperative_worker_before_returning() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let stage_deadline = Instant::now() + DEADLINE_TIMEOUT;
        let item_deadline = Instant::now() + ITEM_TIMEOUT;
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_dropped = dropped.clone();
        let inputs = [StageEnvelope::new(meta(0, item_deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, stage_deadline),
            StageWorkload::new(inputs, move |_| {
                let worker_dropped = worker_dropped.clone();
                async move {
                    struct DropFlag(Arc<AtomicBool>);
                    impl Drop for DropFlag {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _drop_flag = DropFlag(worker_dropped);
                    pending::<Result<(), StageItemFailure>>().await
                }
            }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::StageDeadline { .. })
        );
        assert!(dropped.load(Ordering::Acquire));
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + CLEANUP_GRACE)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_delivery_poisoning_distinguishes_parent_cancellation() {
        for parent_cancelled in [false, true] {
            let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
            let progress = runner.progress.clone();
            let deadline = Instant::now() + TEST_TIMEOUT;
            let inputs = [StageEnvelope::new(meta(0, deadline), ())];
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let reducer_barrier = barrier.clone();
            let (entered, entered_receiver) = tokio::sync::oneshot::channel();
            let mut entered = Some(entered);
            let execution = StageExecution::new(
                config(1, 0, deadline),
                StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
                StageFold::new((), move |(): &mut (), _| {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    reducer_barrier.wait();
                    Ok(())
                }),
            );
            let handle = tokio::spawn(async move { runner.execute(execution).await });
            assert!(entered_receiver.await.is_ok());
            let (lock_entered, lock_entered_receiver) = tokio::sync::oneshot::channel();
            let (release_lock, release_lock_receiver) = watch::channel(false);
            let lock_holder = tokio::spawn(crate::progress::hold_progress_write_for_test(
                progress,
                lock_entered,
                release_lock_receiver,
            ));
            assert!(lock_entered_receiver.await.is_ok());
            if parent_cancelled {
                cancellation.send_replace(true);
            }
            handle.abort();
            barrier.wait();
            let joined = handle.await;
            let stopped = match joined {
                Err(error) => error.is_cancelled(),
                Ok(Err(StageRunError::Cancelled { .. })) => parent_cancelled,
                Ok(_) => false,
            };
            assert!(stopped);
            release_lock.send_replace(true);
            assert!(lock_holder.await.is_ok());
            drop(cancellation);
            let report = tasks.close_abort_and_reap(deadline).await;
            assert!(report.all_joined);
            assert_eq!(report.worker_failed, !parent_cancelled);
            assert_eq!(report.unobserved_results, !parent_cancelled);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn cleanup_timeout_is_distinct_when_uncooperative_work_cannot_reap() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let stage_deadline = Instant::now() + DEADLINE_TIMEOUT;
        let item_deadline = Instant::now() + ITEM_TIMEOUT;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let inputs = [StageEnvelope::new(meta(0, item_deadline), ())];
        let execution = StageExecution::new(
            StageRunConfig::new(
                PipelineStage::Parse,
                StageCapacity::new(1, 0),
                StageDeadlinePolicy::new(stage_deadline, DEADLINE_TIMEOUT),
            ),
            StageWorkload::new(inputs, move |_| {
                let worker_barrier = worker_barrier.clone();
                let worker_started = worker_started.clone();
                async move {
                    worker_started.store(true, Ordering::Release);
                    worker_barrier.wait();
                    Ok::<_, StageItemFailure>(())
                }
            }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        let handle = tokio::spawn(async move { runner.execute(execution).await });
        wait_until(|| started.load(Ordering::Acquire)).await;
        let result = handle.await;
        barrier.wait();
        assert_matches!(
            result,
            Ok(Err(StageRunError::Reap {
                stage: PipelineStage::Parse
            }))
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + CLEANUP_GRACE)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test]
    async fn noncontiguous_input_fails_closed_before_later_work_is_admitted() {
        let (runner, tasks, _) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + ITEM_TIMEOUT;
        let inputs = [StageEnvelope::new(meta(1, deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::InputSequence {
                expected,
                actual,
                ..
            }) if expected == StageSequence::new(0) && actual == StageSequence::new(1)
        );
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test]
    async fn completed_out_of_order_output_retains_capacity_until_reduction() {
        let window = TEST_WORKERS + TEST_QUEUE;
        let (runner, tasks, cancellation) = runner(window, ITEM_BYTES * 2).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let started = Arc::new(AtomicUsize::new(0));
        let first_release = Arc::new(AtomicBool::new(false));
        let inputs = (0..TEST_ITEMS)
            .map(move |sequence| StageEnvelope::new(meta(sequence, deadline), sequence));
        let worker_started = started.clone();
        let worker_release = first_release.clone();
        let metrics = StageMetrics::new();
        let execution = StageExecution::new(
            config(TEST_WORKERS, TEST_QUEUE, deadline),
            StageWorkload::new(inputs, move |item: StageWorkItem<usize, usize>| {
                let worker_started = worker_started.clone();
                let worker_release = worker_release.clone();
                async move {
                    let (sequence, _, payload) = item.into_parts();
                    worker_started.fetch_add(1, Ordering::AcqRel);
                    if sequence == StageSequence::new(0) {
                        while !worker_release.load(Ordering::Acquire) {
                            tokio::task::yield_now().await;
                        }
                    }
                    Ok::<_, StageItemFailure>(payload)
                }
            }),
            StageFold::new(0_usize, |count: &mut usize, _| {
                *count += 1;
                Ok(())
            }),
        )
        .with_metrics(metrics.clone());
        let handle = tokio::spawn(async move { runner.execute(execution).await });
        wait_until(|| started.load(Ordering::Acquire) == TEST_WORKERS).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started.load(Ordering::Acquire), TEST_WORKERS);
        first_release.store(true, Ordering::Release);
        assert_matches!(handle.await, Ok(Ok(TEST_ITEMS)));
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
        let Ok(snapshot) = metrics.snapshot() else {
            panic!("stage metrics became unavailable");
        };
        assert_eq!(snapshot.admitted_items(), TEST_ITEMS as u64);
        assert_eq!(snapshot.completed_items(), TEST_ITEMS as u64);
        assert_eq!(snapshot.current_items(), 0);
        assert_eq!(snapshot.current_reserved_bytes(), 0);
        assert_eq!(snapshot.peak_items(), 2);
        assert_eq!(snapshot.peak_reserved_bytes(), ITEM_BYTES * 2);
    }

    #[tokio::test]
    async fn worker_failure_has_stable_provenance_and_blocks_publication() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let inputs = [StageEnvelope::new(meta(0, deadline), ())];
        let metrics = StageMetrics::new();
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async { Err::<(), _>(StageItemFailure) }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        )
        .with_metrics(metrics.clone());
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Item {
                sequence,
                kind: StageFailureKind::Worker,
                ..
            }) if sequence == StageSequence::new(0)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
        let Ok(snapshot) = metrics.snapshot() else {
            panic!("failed stage metrics became unavailable");
        };
        assert_eq!(snapshot.admitted_items(), 1);
        assert_eq!(snapshot.completed_items(), 0);
        assert_eq!(snapshot.current_items(), 0);
        assert_eq!(snapshot.current_reserved_bytes(), 0);
    }

    #[tokio::test]
    async fn unavailable_metrics_abort_and_reap_the_already_registered_task() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let metrics = StageMetrics::new();
        let poison_target = metrics.clone();
        let poison = std::thread::spawn(move || {
            let Ok(_guard) = poison_target.state.lock() else {
                return;
            };
            panic!("intentional metric-lock poison");
        });
        assert!(poison.join().is_err());
        let inputs = [StageEnvelope::new(meta(0, deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async { Ok::<_, StageItemFailure>(()) }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        )
        .with_metrics(metrics);
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Metrics {
                source: StageMetricsError::Unavailable,
                ..
            })
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert_eq!(report.active_tasks, 0);
        assert!(report.all_joined);
        assert!(report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test]
    async fn reducer_failure_aborts_and_reaps_remaining_work() {
        let (runner, tasks, cancellation) = runner(2, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let second_started = Arc::new(AtomicBool::new(false));
        let second_dropped = Arc::new(AtomicBool::new(false));
        let inputs = (0..2).map(move |sequence| StageEnvelope::new(meta(sequence, deadline), ()));
        let worker_started = second_started.clone();
        let worker_dropped = second_dropped.clone();
        let execution = StageExecution::new(
            config(2, 0, deadline),
            StageWorkload::new(inputs, move |item: StageWorkItem<usize, ()>| {
                let worker_started = worker_started.clone();
                let worker_dropped = worker_dropped.clone();
                async move {
                    if item.sequence() == StageSequence::new(0) {
                        while !worker_started.load(Ordering::Acquire) {
                            tokio::task::yield_now().await;
                        }
                        Ok::<_, StageItemFailure>(())
                    } else {
                        struct DropFlag(Arc<AtomicBool>);
                        impl Drop for DropFlag {
                            fn drop(&mut self) {
                                self.0.store(true, Ordering::Release);
                            }
                        }
                        let _drop_flag = DropFlag(worker_dropped);
                        worker_started.store(true, Ordering::Release);
                        pending::<Result<(), StageItemFailure>>().await
                    }
                }
            }),
            StageFold::new((), |(): &mut (), _| Err(StageItemFailure)),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Reduce { sequence, .. })
                if sequence == StageSequence::new(0)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
        assert!(second_dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn panicked_worker_is_reported_as_unexpected_task_exit() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let inputs = [StageEnvelope::new(meta(0, deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, deadline),
            StageWorkload::new(inputs, |_| async {
                panic!("intentional stage worker panic");
            }),
            StageFold::new((), |(): &mut (), _: StageOutput<usize, ()>| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Item {
                sequence,
                kind: StageFailureKind::UnexpectedTaskExit,
                ..
            }) if sequence == StageSequence::new(0)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooperative_cpu_worker_observes_its_item_deadline_without_yielding() {
        let (runner, tasks, cancellation) = runner(1, TEST_SCOPE_BYTES).await;
        let stage_deadline = Instant::now() + TEST_TIMEOUT;
        let item_deadline = Instant::now() + DEADLINE_TIMEOUT;
        let inputs = [StageEnvelope::new(meta(0, item_deadline), ())];
        let execution = StageExecution::new(
            config(1, 0, stage_deadline),
            StageWorkload::new(inputs, |item: StageWorkItem<usize, ()>| async move {
                let cancellation = item.cancellation();
                while !cancellation.is_cancelled() {
                    std::hint::spin_loop();
                }
                Err::<(), _>(StageItemFailure)
            }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Item {
                sequence,
                kind: StageFailureKind::Deadline,
                ..
            }) if sequence == StageSequence::new(0)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(stage_deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sibling_failure_signals_cooperative_cpu_work_before_abort_reaping() {
        let (runner, tasks, cancellation) = runner(2, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let workers_started = Arc::new(Barrier::new(2));
        let saw_stage_cancellation = Arc::new(AtomicBool::new(false));
        let inputs = (0..2).map(move |sequence| StageEnvelope::new(meta(sequence, deadline), ()));
        let started = workers_started.clone();
        let observed = saw_stage_cancellation.clone();
        let execution = StageExecution::new(
            config(2, 0, deadline),
            StageWorkload::new(inputs, move |item: StageWorkItem<usize, ()>| {
                let started = started.clone();
                let observed = observed.clone();
                async move {
                    let sequence = item.sequence();
                    let cancellation = item.cancellation();
                    tokio::task::block_in_place(move || {
                        started.wait();
                        if sequence == StageSequence::new(0) {
                            return Err::<(), _>(StageItemFailure);
                        }
                        while !cancellation.is_cancelled() {
                            std::hint::spin_loop();
                        }
                        observed.store(true, Ordering::Release);
                        Err(StageItemFailure)
                    })
                }
            }),
            StageFold::new((), |(): &mut (), _| Ok(())),
        );
        assert_matches!(
            runner.execute(execution).await,
            Err(StageRunError::Item {
                sequence,
                kind: StageFailureKind::Worker,
                ..
            }) if sequence == StageSequence::new(0)
        );
        assert!(saw_stage_cancellation.load(Ordering::Acquire));
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[test]
    fn invalid_stage_policy_is_rejected_before_execution() {
        let future = Instant::now() + TEST_TIMEOUT;
        let expired = Instant::now();
        assert_matches!(
            validate_stage(
                StageRunConfig::new(
                    PipelineStage::Publish,
                    StageCapacity::new(1, 0),
                    StageDeadlinePolicy::new(future, CLEANUP_GRACE),
                ),
                1,
            )
            .err(),
            Some(StageRunError::InvalidConfig { field: "stage", .. })
        );
        assert_matches!(
            validate_stage(config(0, 0, future), 1).err(),
            Some(StageRunError::InvalidConfig {
                field: "workers",
                ..
            })
        );
        assert_matches!(
            validate_stage(config(1, 1, future), 1).err(),
            Some(StageRunError::InvalidConfig {
                field: "capacity",
                ..
            })
        );
        assert_matches!(
            validate_stage(config(1, 0, expired), 1).err(),
            Some(StageRunError::InvalidConfig {
                field: "deadline",
                ..
            })
        );
        assert_matches!(
            validate_stage(
                StageRunConfig::new(
                    PipelineStage::Parse,
                    StageCapacity::new(1, 0),
                    StageDeadlinePolicy::new(future, Duration::ZERO),
                ),
                1,
            )
            .err(),
            Some(StageRunError::InvalidConfig {
                field: "cleanup_grace",
                ..
            })
        );
    }
}
