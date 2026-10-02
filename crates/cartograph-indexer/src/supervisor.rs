use std::{
    fmt,
    future::Future,
    ops::Deref,
    sync::{Arc, Mutex},
    time::Duration,
};

use cartograph_db::{
    CartographDatabase, CurrentGeneration, LeaseAcquisitionAttempt, LeaseAcquisitionProbe,
    LeaseError, LeaseFence, LeaseOwner, LeaseRequest, LeaseTarget, ObservedGeneration,
    ObservedLease, OperationReconciliation, ProjectLease, ReadyGeneration, StorageError,
    TerminalGenerationMutation,
};
use cartograph_domain::{ContentDigest, GenerationId, NormalizedPath, ProjectId};
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{Instant, sleep_until, timeout_at},
};

use crate::{
    CancellationReason, InvalidSupervisorConfig, PipelineStage, ProgressError, SupervisorConfig,
    SupervisorContext, SupervisorStatus,
    prepare_scope::{PrepareReap, PrepareScope},
    progress::{SharedProgress, SupervisorContextParts},
    task_scope::{ReapReport, TaskScope},
};

mod lease_keeper;
mod root_work;

use lease_keeper::{
    HEARTBEAT_OPERATION, HeartbeatRequest, KeeperExit, LeaseKeeper, LeaseRenewal, LeaseVerdict,
    RenewalWindow, run_bounded_heartbeat,
};
use root_work::{SupervisedWork, WorkCompletion};

/// Lease acquisition boundary for one supervised project operation.
#[derive(Clone, Debug)]
pub struct SupervisorRequest {
    target: LeaseTarget,
    owner: LeaseOwner,
    lease_duration: Duration,
}

impl SupervisorRequest {
    /// Bind a project/generation operation to an observable process owner.
    #[must_use]
    pub const fn new(target: LeaseTarget, owner: LeaseOwner, lease_duration: Duration) -> Self {
        Self {
            target,
            owner,
            lease_duration,
        }
    }
}

/// Credential-safe failure returned by a supervised pipeline future.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("Cartograph indexing failed during the {stage} stage")]
pub struct PipelineFailure {
    stage: PipelineStage,
    detail: PipelineFailureDetail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PipelineFailureDetail {
    None,
    Reason(PipelineFailureReason),
    File(PipelineFileFailure),
}

/// One bounded project-relative input and its allowlisted failure reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineFileFailure {
    path: NormalizedPath,
    reason: PipelineFailureReason,
}

impl PipelineFileFailure {
    /// Bind one allowlisted failure reason to a validated project-relative path.
    #[must_use]
    pub const fn new(path: NormalizedPath, reason: PipelineFailureReason) -> Self {
        Self { path, reason }
    }

    /// Exact normalized path relative to the configured project root.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Stable credential-safe reason for this input failure.
    #[must_use]
    pub const fn reason(&self) -> PipelineFailureReason {
        self.reason
    }
}

impl fmt::Display for PipelineFileFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug-format only the already-normalized relative string so control
        // characters cannot inject another terminal/log line.
        write!(
            formatter,
            "{} at project-relative path {:?}",
            self.reason,
            self.path.as_str()
        )
    }
}

/// Stable credential-safe detail for a pipeline failure whose cause is actionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineFailureReason {
    /// A bounded item or whole-stage execution horizon elapsed.
    DeadlineExceeded,
    /// The supervisor observed no durable stage progress inside its watchdog horizon.
    ProgressStalled,
    /// The configured in-memory generation ceiling rejected bounded work.
    GenerationCapacityExceeded,
    /// One manifest input could not be reopened under the bounded source contract.
    SourceReadFailed,
    /// One source changed after its exact manifest entry was read.
    SourceChangedDuringParse,
    /// Discovery admitted a language without a production extractor.
    ExtractionUnsupportedLanguage,
    /// A reusable extractor received a snapshot for another language.
    ExtractionLanguageMismatch,
    /// A statically linked grammar was unavailable or ABI-incompatible.
    ExtractionGrammarUnavailable,
    /// The parser stopped before returning a syntax tree.
    ExtractionParserStopped,
    /// Cooperative cancellation interrupted one extraction input.
    ExtractionCancelled,
    /// The parser returned a source span outside the durable contract.
    ExtractionInvalidSpan,
    /// The configured AST-depth policy was outside the supported range.
    ExtractionInvalidNestingLimit,
    /// One source syntax tree exceeded the extractor's defensive nesting ceiling.
    ExtractionNestingLimitExceeded,
    /// One source file exceeded its bounded modeled extraction output allowance.
    ExtractionOutputLimitExceeded,
    /// One file-local parse operation failed without a more specific safe classification.
    FileProcessingFailed,
    /// Canonical reduction rejected an extracted reference name above the storage bound.
    ReferenceNameTooLong,
    /// Canonical reduction rejected one bounded field, named by its stable
    /// storage field identifier so the failure is actionable without bisecting
    /// the corpus by hand.
    CanonicalFieldRejected(&'static str),
}

#[derive(Clone, Copy)]
struct PipelineFailureText {
    code: &'static str,
    description: &'static str,
}

impl PipelineFailureText {
    const fn new(code: &'static str, description: &'static str) -> Self {
        Self { code, description }
    }
}

const FILE_PROCESSING_FAILURE_TEXT: PipelineFailureText = PipelineFailureText::new(
    "file_processing_failed",
    "file-local parse processing failed without a more specific safe classification",
);

const fn pipeline_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::DeadlineExceeded
        | PipelineFailureReason::ProgressStalled
        | PipelineFailureReason::GenerationCapacityExceeded
        | PipelineFailureReason::SourceReadFailed
        | PipelineFailureReason::SourceChangedDuringParse => pipeline_control_failure_text(reason),
        PipelineFailureReason::ExtractionUnsupportedLanguage
        | PipelineFailureReason::ExtractionLanguageMismatch
        | PipelineFailureReason::ExtractionGrammarUnavailable
        | PipelineFailureReason::ExtractionParserStopped
        | PipelineFailureReason::ExtractionCancelled
        | PipelineFailureReason::ExtractionInvalidSpan
        | PipelineFailureReason::ExtractionInvalidNestingLimit
        | PipelineFailureReason::ExtractionNestingLimitExceeded
        | PipelineFailureReason::ExtractionOutputLimitExceeded
        | PipelineFailureReason::FileProcessingFailed => pipeline_extraction_failure_text(reason),
        PipelineFailureReason::ReferenceNameTooLong
        | PipelineFailureReason::CanonicalFieldRejected(_) => {
            pipeline_reduction_failure_text(reason)
        }
    }
}

const fn pipeline_control_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::DeadlineExceeded => PipelineFailureText::new(
            "deadline_exceeded",
            "the bounded item or stage deadline elapsed",
        ),
        PipelineFailureReason::ProgressStalled => PipelineFailureText::new(
            "progress_stalled",
            "the stage made no durable progress inside its watchdog horizon",
        ),
        PipelineFailureReason::GenerationCapacityExceeded => PipelineFailureText::new(
            "generation_capacity_exceeded",
            "the configured generation capacity rejected this work",
        ),
        PipelineFailureReason::SourceReadFailed => PipelineFailureText::new(
            "source_read_failed",
            "the source file could not be reopened under the bounded source contract",
        ),
        PipelineFailureReason::SourceChangedDuringParse => PipelineFailureText::new(
            "source_changed_during_parse",
            "the source file changed after its manifest entry was read",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

const fn pipeline_extraction_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::ExtractionUnsupportedLanguage => PipelineFailureText::new(
            "extraction_unsupported_language",
            "no production extractor was available for the admitted source language",
        ),
        PipelineFailureReason::ExtractionLanguageMismatch => PipelineFailureText::new(
            "extraction_language_mismatch",
            "the selected extractor did not match the source language",
        ),
        PipelineFailureReason::ExtractionGrammarUnavailable => PipelineFailureText::new(
            "extraction_grammar_unavailable",
            "the statically linked parser grammar was unavailable",
        ),
        PipelineFailureReason::ExtractionParserStopped => PipelineFailureText::new(
            "extraction_parser_stopped",
            "the parser stopped before producing a syntax tree",
        ),
        PipelineFailureReason::ExtractionCancelled => PipelineFailureText::new(
            "extraction_cancelled",
            "cooperative cancellation interrupted source extraction",
        ),
        PipelineFailureReason::ExtractionInvalidSpan => PipelineFailureText::new(
            "extraction_invalid_span",
            "the parser produced a source span outside the durable contract",
        ),
        PipelineFailureReason::ExtractionInvalidNestingLimit => PipelineFailureText::new(
            "extraction_invalid_nesting_limit",
            "the AST-depth policy was outside the supported range",
        ),
        PipelineFailureReason::ExtractionNestingLimitExceeded => PipelineFailureText::new(
            "extraction_nesting_limit_exceeded",
            "the source syntax tree exceeded the configured nesting limit",
        ),
        PipelineFailureReason::ExtractionOutputLimitExceeded => PipelineFailureText::new(
            "extraction_output_limit_exceeded",
            "the extracted file facts exceeded the configured output limit",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

const fn pipeline_reduction_failure_text(reason: PipelineFailureReason) -> PipelineFailureText {
    match reason {
        PipelineFailureReason::ReferenceNameTooLong => PipelineFailureText::new(
            "reference_name_too_long",
            "an extracted reference name exceeded the canonical storage bound",
        ),
        PipelineFailureReason::CanonicalFieldRejected(_) => PipelineFailureText::new(
            "canonical_field_rejected",
            "an extracted field was rejected by the canonical storage contract",
        ),
        _ => FILE_PROCESSING_FAILURE_TEXT,
    }
}

impl PipelineFailureReason {
    /// Stable machine-readable reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        pipeline_failure_text(self).code
    }

    /// Fixed user-facing explanation that never contains source, paths, or driver text.
    #[must_use]
    pub const fn description(self) -> &'static str {
        pipeline_failure_text(self).description
    }

    /// Stable storage field identifier for a rejected bounded field.
    #[must_use]
    pub const fn rejected_field(self) -> Option<&'static str> {
        match self {
            Self::CanonicalFieldRejected(field) => Some(field),
            _ => None,
        }
    }
}

impl fmt::Display for PipelineFailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())?;
        // Naming the exact rejected field turns a whole-corpus bisection into a
        // single run. The identifier is a fixed storage field name, never source
        // text or a checkout path.
        match self.rejected_field() {
            Some(field) => write!(formatter, "({field})"),
            None => Ok(()),
        }
    }
}

impl PipelineFailure {
    /// Identify the stable stage without accepting arbitrary error text.
    #[must_use]
    pub const fn new(stage: PipelineStage) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::None,
        }
    }

    /// Attach one allowlisted actionable reason without retaining source or driver text.
    #[must_use]
    pub const fn with_reason(stage: PipelineStage, reason: PipelineFailureReason) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::Reason(reason),
        }
    }

    /// Attach one validated project-relative input and allowlisted reason.
    #[must_use]
    pub const fn with_file_failure(stage: PipelineStage, failure: PipelineFileFailure) -> Self {
        Self {
            stage,
            detail: PipelineFailureDetail::File(failure),
        }
    }

    /// Stable failed stage recorded by the supervisor and structured workers.
    #[must_use]
    pub const fn stage(&self) -> PipelineStage {
        self.stage
    }

    /// Optional allowlisted actionable reason.
    #[must_use]
    pub const fn reason(&self) -> Option<PipelineFailureReason> {
        match &self.detail {
            PipelineFailureDetail::None => None,
            PipelineFailureDetail::Reason(reason) => Some(*reason),
            PipelineFailureDetail::File(failure) => Some(failure.reason()),
        }
    }

    /// Optional exact project-relative input that caused the stage failure.
    #[must_use]
    pub const fn file_failure(&self) -> Option<&PipelineFileFailure> {
        match &self.detail {
            PipelineFailureDetail::File(failure) => Some(failure),
            PipelineFailureDetail::None | PipelineFailureDetail::Reason(_) => None,
        }
    }
}

/// One-shot supervisor bound to a PostgreSQL data plane and deadline policy.
#[derive(Clone)]
pub struct IndexerSupervisor {
    database: CartographDatabase,
    config: SupervisorConfig,
    progress: SharedProgress,
    cancellation: watch::Sender<bool>,
    lifecycle: Arc<Mutex<LifecycleGate>>,
}

struct RunCoordinator<'a> {
    supervisor: &'a IndexerSupervisor,
}

impl Deref for RunCoordinator<'_> {
    type Target = IndexerSupervisor;

    fn deref(&self) -> &Self::Target {
        self.supervisor
    }
}

struct DurableCoordinator<'a> {
    supervisor: &'a IndexerSupervisor,
}

impl Deref for DurableCoordinator<'_> {
    type Target = IndexerSupervisor;

    fn deref(&self) -> &Self::Target {
        self.supervisor
    }
}

impl IndexerSupervisor {
    /// Create a queued one-shot supervisor. Configuration is validated before acquisition.
    #[must_use]
    pub fn new(database: CartographDatabase, config: SupervisorConfig) -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            database,
            config,
            progress: SharedProgress::new(),
            cancellation,
            lifecycle: Arc::new(Mutex::new(LifecycleGate::new())),
        }
    }

    /// Return a point-in-time observable status packet.
    pub async fn status(&self) -> SupervisorStatus {
        self.progress.status().await
    }

    /// Request cancellation exactly until durable publication finalization starts.
    ///
    /// A `true` result is linearized against the publication gate and guarantees
    /// that this run will not subsequently start publication.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let accepted = self
            .lifecycle
            .lock()
            .is_ok_and(|mut lifecycle| lifecycle.request_external_cancellation());
        if accepted {
            self.cancellation.send_replace(true);
        }
        accepted
    }

    /// Acquire the lease, monitor work, enforce deadlines, and clean up exactly once.
    /// # Errors
    ///
    /// Returns an error if request/lease acquisition fails, work stalls or
    /// times out, a child fails, publication loses its fence, or cleanup fails.
    pub async fn run<Work, WorkFuture>(
        &self,
        request: SupervisorRequest,
        work: Work,
    ) -> Result<CurrentGeneration, SupervisorError>
    where
        Work: FnOnce(SupervisorContext) -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = Result<ReadyGeneration, PipelineFailure>> + Send + 'static,
    {
        let runtime = tokio::runtime::Handle::current();
        let runner = self.clone();
        let run = runtime.spawn(async move {
            RunCoordinator {
                supervisor: &runner,
            }
            .run_inner(request, work)
            .await
        });
        SupervisorRunGuard::new(self.clone(), runtime, run)
            .join()
            .await
    }
}

impl RunCoordinator<'_> {
    async fn run_inner<Work, WorkFuture>(
        &self,
        request: SupervisorRequest,
        work: Work,
    ) -> Result<CurrentGeneration, SupervisorError>
    where
        Work: FnOnce(SupervisorContext) -> WorkFuture,
        WorkFuture: Future<Output = Result<ReadyGeneration, PipelineFailure>> + Send + 'static,
    {
        self.config.validate_for(request.lease_duration)?;
        if request.target.generation_id().is_none() {
            return Err(SupervisorError::MissingGeneration);
        }
        if !self.progress.reserve().await {
            return Err(SupervisorError::AlreadyStarted);
        }
        self.start_lifecycle()?;
        let budget = OperationBudget::new(self.config);
        let receiver = self.cancellation.subscribe();
        let _lifecycle = LifecycleFinishGuard::new(self);
        self.run_started(StartedRun {
            request,
            work,
            receiver,
            budget,
        })
        .await
    }

    async fn run_started<Work, WorkFuture>(
        &self,
        started: StartedRun<Work>,
    ) -> Result<CurrentGeneration, SupervisorError>
    where
        Work: FnOnce(SupervisorContext) -> WorkFuture,
        WorkFuture: Future<Output = Result<ReadyGeneration, PipelineFailure>> + Send + 'static,
    {
        let StartedRun {
            request,
            work,
            mut receiver,
            budget,
        } = started;
        if *receiver.borrow_and_update() {
            self.select_cancellation(CancellationReason::Requested)?;
            self.progress
                .mark_cancelled(CancellationReason::Requested, false)
                .await;
            return Err(cancelled(CancellationReason::Requested, false));
        }
        let (attempt, probe) =
            match CartographDatabase::prepare_lease_acquisition(LeaseRequest::new(
                request.target.clone(),
                request.owner,
                request.lease_duration,
            )) {
                Ok(capabilities) => capabilities,
                Err(source) => {
                    self.begin_finishing()?;
                    self.progress.mark_failed().await;
                    return Err(SupervisorError::Lease {
                        operation: "prepare-acquire",
                        source,
                    });
                }
            };
        let lease = match self
            .durable()
            .acquire_exact(
                attempt,
                AcquisitionContext {
                    probe,
                    budget,
                    receiver: &mut receiver,
                },
            )
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                return self.finish_acquisition_failure(error, &mut receiver).await;
            }
        };
        self.progress.mark_active().await;
        let fence = lease.fence();
        let operation = OwnedOperation {
            target: request.target,
            lease: Some(lease),
            fence: fence.clone(),
            budget,
        };
        self.run_owned(OwnedRun {
            operation,
            work,
            receiver,
        })
        .await
    }

    async fn finish_acquisition_failure(
        &self,
        error: SupervisorError,
        receiver: &mut watch::Receiver<bool>,
    ) -> Result<CurrentGeneration, SupervisorError> {
        if *receiver.borrow_and_update()
            && !matches!(
                &error,
                SupervisorError::AmbiguousOutcome { .. }
                    | SupervisorError::OperationBudgetExhausted
            )
        {
            self.select_cancellation(CancellationReason::Requested)?;
            self.progress
                .mark_cancelled(CancellationReason::Requested, false)
                .await;
            return Err(cancelled(CancellationReason::Requested, false));
        }
        self.begin_finishing()?;
        self.progress.mark_failed().await;
        Err(error)
    }

    async fn run_owned<Work, WorkFuture>(
        &self,
        owned: OwnedRun<Work>,
    ) -> Result<CurrentGeneration, SupervisorError>
    where
        Work: FnOnce(SupervisorContext) -> WorkFuture,
        WorkFuture: Future<Output = Result<ReadyGeneration, PipelineFailure>> + Send + 'static,
    {
        let OwnedRun {
            mut operation,
            work,
            mut receiver,
        } = owned;
        if *receiver.borrow_and_update() {
            return self
                .cancel_before_work(&mut operation, CancellationReason::Requested)
                .await;
        }
        if Instant::now() >= operation.budget.work_deadline {
            return self
                .cancel_before_work(&mut operation, CancellationReason::OperationDeadline)
                .await;
        }

        let tasks = TaskScope::new(self.config.workers.tasks, self.config.workers.bytes);
        let prepares = PrepareScope::new(
            self.database.clone(),
            operation.fence.clone(),
            self.config.deadlines.copy_timeout(),
        );
        let context = SupervisorContext::new(SupervisorContextParts {
            shared: self.progress.clone(),
            receiver: receiver.clone(),
            tasks: tasks.clone(),
            prepares: prepares.clone(),
        });
        let Some(lease) = operation.lease.take() else {
            self.progress.mark_failed().await;
            return Err(SupervisorError::LifecycleUnavailable);
        };
        // Work and lease renewal each run on their own task so that neither a
        // monitor branch nor a work future that blocks its thread can stop the
        // other from being polled.
        let monitored = MonitoredTasks {
            work: SupervisedWork::spawn(work(context)),
            keeper: LeaseKeeper::spawn(
                self.lease_renewal(
                    operation.budget,
                    RenewalWindow::active_work(self.config, operation.budget),
                ),
                lease,
            ),
        };
        let MonitoredRun {
            outcome,
            lease,
            tasks: MonitoredTasks { work, .. },
        } = WorkMonitor {
            config: self.config,
            budget: operation.budget,
            progress: self.progress.clone(),
            cancellation: &self.cancellation,
            receiver,
            lifecycle: &self.lifecycle,
            prepares: prepares.clone(),
        }
        .run(monitored)
        .await;
        operation.lease = lease;
        let monitored = self
            .reap_work(
                &mut operation,
                WorkReap {
                    outcome,
                    work,
                    tasks: &tasks,
                    prepares: &prepares,
                },
            )
            .await;
        self.finish(&mut operation, monitored).await
    }

    /// Reap the root work, then registered children and the prepare task.
    ///
    /// Abort cannot interrupt a synchronous section of the root work, which the
    /// former inline monitor waited out before it dropped the work. The root
    /// wait is therefore bounded only by the operation's reap ceiling, so
    /// cleanup is not abandoned while the work finishes such a section. When
    /// publication or owned cleanup may follow, the lease is renewed for the
    /// whole wait so that they still hold authority. As the inline future was
    /// dropped when its monitor returned, children and the prepare task are
    /// reaped only once the root is gone, but without waiting for that renewal
    /// to settle. A root task that outlives the ceiling is reported as unreaped.
    async fn reap_work(
        &self,
        operation: &mut OwnedOperation,
        reaping: WorkReap<'_>,
    ) -> MonitoredWork {
        let WorkReap {
            outcome,
            mut work,
            tasks,
            prepares,
        } = reaping;
        let budget = operation.budget;
        let ceiling = budget.reap_ceiling(self.config);
        let mut renewal = if work.has_ended() || !outcome.keeps_authority() {
            None
        } else {
            operation.lease.take().map(|lease| {
                LeaseKeeper::spawn(
                    self.lease_renewal(budget, RenewalWindow::reaping(ceiling)),
                    lease,
                )
            })
        };
        let work_reaped = work.abort_and_reap(ceiling).await;
        let children_deadline = self.children_reap_deadline(&outcome, ceiling);
        let registered = async {
            let reap = tasks.close_abort_and_reap(children_deadline).await;
            let prepare = prepares
                .close_and_reap(ceiling, budget.final_deadline)
                .await;
            (reap, prepare)
        };
        let settlement = async {
            match renewal.as_mut() {
                Some(keeper) => Some(keeper.stop(budget.durable_ceiling(self.config)).await),
                None => None,
            }
        };
        let ((reap, prepare), settled) = tokio::join!(registered, settlement);
        let outcome = match settled {
            Some(exit) => settle_reap_renewal(operation, outcome, exit),
            None => outcome,
        };
        MonitoredWork {
            outcome,
            work_reaped,
            reap,
            prepare,
        }
    }

    /// Bound for reaping registered children once the root work is gone: one
    /// cancellation grace, or one heartbeat request when lease authority is
    /// uncertain, capped at the reap ceiling.
    fn children_reap_deadline(&self, outcome: &MonitorOutcome, ceiling: Instant) -> Instant {
        OperationBudget::request_deadline(
            match outcome {
                MonitorOutcome::Cancelled(CancelledWork {
                    reason: CancellationReason::LeaseLost | CancellationReason::LeaseHeartbeatFailed,
                    ..
                }) => self.config.deadlines.heartbeat_request,
                _ => self.config.deadlines.cancellation_grace,
            },
            ceiling,
        )
    }

    /// Inputs for a lease keeper that renews within `window`.
    fn lease_renewal(&self, budget: OperationBudget, window: RenewalWindow) -> LeaseRenewal {
        LeaseRenewal {
            database: self.database.clone(),
            config: self.config,
            budget,
            progress: self.progress.clone(),
            window,
        }
    }

    async fn cancel_before_work(
        &self,
        operation: &mut OwnedOperation,
        reason: CancellationReason,
    ) -> Result<CurrentGeneration, SupervisorError> {
        let selected = self.select_cancellation(reason)?;
        self.progress.mark_cancelling(selected).await;
        self.cancellation.send_replace(true);
        self.finish_cancelled(
            operation,
            CancelledWork {
                reason: selected,
                grace_exceeded: false,
            },
        )
        .await
    }

    async fn finish(
        &self,
        operation: &mut OwnedOperation,
        monitored: MonitoredWork,
    ) -> Result<CurrentGeneration, SupervisorError> {
        let MonitoredWork {
            outcome,
            work_reaped,
            reap,
            prepare,
        } = monitored;
        if !prepare.all_joined {
            self.progress.mark_failed().await;
            return Err(SupervisorError::UnreapedDurableOperation {
                operation: "prepare-generation",
            });
        }
        if !work_reaped || !reap.all_joined {
            self.progress.mark_failed().await;
            return Err(SupervisorError::UnreapedWorkers);
        }
        if let MonitorOutcome::Failed(failure) = &outcome {
            return self
                .fail_owned(operation, pipeline_supervisor_error(failure.clone()))
                .await;
        }
        if reap.worker_failed {
            return self
                .fail_owned(operation, SupervisorError::WorkerFailed)
                .await;
        }
        if reap.unobserved_results {
            return self
                .fail_owned(operation, SupervisorError::UnobservedWorkers)
                .await;
        }
        match outcome {
            MonitorOutcome::Ready(ready) if reap.active_tasks == 0 => {
                self.finish_ready(operation, ready).await
            }
            MonitorOutcome::Ready(_) => {
                self.fail_owned(operation, SupervisorError::UnjoinedWorkers)
                    .await
            }
            MonitorOutcome::Failed(failure) => {
                self.fail_owned(operation, pipeline_supervisor_error(failure))
                    .await
            }
            MonitorOutcome::Cancelled(cancellation) => {
                self.finish_cancelled(operation, cancellation).await
            }
            MonitorOutcome::SupervisorFailed(error) => self.fail_owned(operation, error).await,
        }
    }

    async fn finish_ready(
        &self,
        operation: &mut OwnedOperation,
        ready: ReadyGeneration,
    ) -> Result<CurrentGeneration, SupervisorError> {
        if ready.project_id() != operation.target.project_id()
            || Some(ready.generation_id()) != operation.target.generation_id()
        {
            return self
                .fail_owned(operation, SupervisorError::GenerationMismatch)
                .await;
        }
        if let Err(error) = self
            .durable()
            .heartbeat_owned(operation, "publish-heartbeat")
            .await
        {
            return self.finish_authority_error(operation, error).await;
        }
        if let Err(source) = self.progress.begin_stage(PipelineStage::Publish).await {
            return self
                .fail_owned(operation, SupervisorError::Progress { source })
                .await;
        }
        match self.durable().publish_reconciled(ready, operation).await {
            Ok(current) => {
                self.progress.mark_completed().await;
                Ok(current)
            }
            Err(error) => self.finish_authority_error(operation, error).await,
        }
    }

    async fn finish_authority_error(
        &self,
        operation: &mut OwnedOperation,
        error: SupervisorError,
    ) -> Result<CurrentGeneration, SupervisorError> {
        if error.forbids_cleanup() {
            self.progress.mark_failed().await;
            Err(error)
        } else {
            self.fail_owned(operation, error).await
        }
    }

    async fn finish_cancelled(
        &self,
        operation: &mut OwnedOperation,
        cancellation: CancelledWork,
    ) -> Result<CurrentGeneration, SupervisorError> {
        if !cancellation.reason.is_authority_uncertain()
            && let Err(cleanup) = self.durable().cleanup_owned_failure(operation).await
        {
            self.progress.mark_failed().await;
            return Err(cleanup_failed(
                cancelled(cancellation.reason, cancellation.grace_exceeded),
                cleanup,
            ));
        }
        self.progress
            .mark_cancelled(cancellation.reason, cancellation.grace_exceeded)
            .await;
        Err(cancelled(cancellation.reason, cancellation.grace_exceeded))
    }

    async fn fail_owned<T>(
        &self,
        operation: &mut OwnedOperation,
        error: SupervisorError,
    ) -> Result<T, SupervisorError> {
        if error.forbids_cleanup() {
            self.progress.mark_failed().await;
            return Err(error);
        }
        let cleanup = self.durable().cleanup_owned_failure(operation).await;
        self.progress.mark_failed().await;
        match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup_failed(error, cleanup)),
        }
    }

    const fn durable(&self) -> DurableCoordinator<'_> {
        DurableCoordinator {
            supervisor: self.supervisor,
        }
    }
}

fn pipeline_supervisor_error(failure: PipelineFailure) -> SupervisorError {
    let PipelineFailure { stage, detail } = failure;
    match detail {
        PipelineFailureDetail::None => SupervisorError::Pipeline { stage },
        PipelineFailureDetail::Reason(reason) => {
            SupervisorError::PipelineWithReason { stage, reason }
        }
        PipelineFailureDetail::File(failure) => {
            SupervisorError::PipelineWithFileFailure { stage, failure }
        }
    }
}

impl DurableCoordinator<'_> {
    async fn publish_reconciled(
        &self,
        ready: ReadyGeneration,
        operation: &OwnedOperation,
    ) -> Result<CurrentGeneration, SupervisorError> {
        let expected = ExpectedGeneration::from_ready(&ready);
        let mut retries = 0_u8;
        let mut publication = self.spawn_publication(ready, operation.fence.clone());
        loop {
            let Ok(deadline) = OperationBudget::database_deadline(
                self.config,
                operation.budget.durable_ceiling(self.config),
            ) else {
                self.finish_and_reap_durable(
                    publication,
                    DurableReap::new(operation.budget, "publish-generation"),
                )
                .await?;
                return Err(ambiguous("publish-generation"));
            };
            let pending = match classify_publication_poll(
                timeout_at(deadline, &mut publication).await,
                retries == 0,
            )? {
                PublicationPoll::Complete(current) => return Ok(current),
                PublicationPoll::Reconcile(pending) => pending,
            };
            let evidence = PublicationEvidence {
                reconciliation: match self.reconcile(operation, "publish-generation").await {
                    Ok(reconciliation) => reconciliation,
                    Err(error) => {
                        self.reap_if_active(
                            PendingDurable::new(publication, pending.task_active),
                            DurableReap::new(operation.budget, "publish-generation"),
                        )
                        .await?;
                        return Err(error);
                    }
                },
                retry: pending.retry,
                retry_allowed: pending.retry_allowed,
            };
            match decide_publication(&expected, evidence) {
                PublicationDecision::Complete(current) => {
                    self.reap_if_active(
                        PendingDurable::new(publication, pending.task_active),
                        DurableReap::new(operation.budget, "publish-generation"),
                    )
                    .await?;
                    return Ok(current);
                }
                PublicationDecision::Wait => {}
                PublicationDecision::Retry(recovered) => {
                    self.reap_if_active(
                        PendingDurable::new(publication, pending.task_active),
                        DurableReap::new(operation.budget, "publish-generation"),
                    )
                    .await?;
                    retries = retries.saturating_add(1);
                    publication = self.spawn_publication(recovered, operation.fence.clone());
                }
                PublicationDecision::Fail(error) => {
                    self.reap_if_active(
                        PendingDurable::new(publication, pending.task_active),
                        DurableReap::new(operation.budget, "publish-generation"),
                    )
                    .await?;
                    return Err(error);
                }
            }
        }
    }

    fn spawn_publication(
        &self,
        ready: ReadyGeneration,
        fence: LeaseFence,
    ) -> tokio::task::JoinHandle<Result<CurrentGeneration, cartograph_db::PublishGenerationError>>
    {
        let database = self.database.clone();
        let statement_timeout = self.config.deadlines.statement_timeout();
        tokio::spawn(async move {
            database
                .publish_generation_bounded(
                    ready,
                    TerminalGenerationMutation::new(&fence, statement_timeout),
                )
                .await
        })
    }

    async fn cleanup_owned_failure(
        &self,
        operation: &mut OwnedOperation,
    ) -> Result<(), SupervisorError> {
        self.heartbeat_owned(operation, "cleanup-heartbeat").await?;
        let budget = operation.budget;
        let mut retries = 0_u8;
        let mut cleanup = self.spawn_cleanup(operation.fence.clone());
        loop {
            let Ok(deadline) = OperationBudget::database_deadline(
                self.config,
                budget.durable_ceiling(self.config),
            ) else {
                self.finish_and_reap_durable(
                    cleanup,
                    DurableReap::new(operation.budget, "cleanup-generation"),
                )
                .await?;
                return Err(ambiguous("cleanup-generation"));
            };
            let pending = match classify_cleanup_poll(
                timeout_at(deadline, &mut cleanup).await,
                retries == 0,
            )? {
                CleanupPoll::Complete => return Ok(()),
                CleanupPoll::Reconcile(pending) => pending,
            };
            let evidence = CleanupEvidence {
                reconciliation: match self.reconcile(operation, "cleanup-generation").await {
                    Ok(reconciliation) => reconciliation,
                    Err(error) => {
                        self.reap_if_active(
                            PendingDurable::new(cleanup, pending.task_active),
                            DurableReap::new(operation.budget, "cleanup-generation"),
                        )
                        .await?;
                        return Err(error);
                    }
                },
                retry_allowed: pending.retry_allowed,
                task_active: pending.task_active,
            };
            match decide_cleanup(evidence) {
                CleanupDecision::Complete => {
                    self.reap_if_active(
                        PendingDurable::new(cleanup, pending.task_active),
                        DurableReap::new(operation.budget, "cleanup-generation"),
                    )
                    .await?;
                    return Ok(());
                }
                CleanupDecision::Wait => {}
                CleanupDecision::Retry => {
                    self.reap_if_active(
                        PendingDurable::new(cleanup, pending.task_active),
                        DurableReap::new(operation.budget, "cleanup-generation"),
                    )
                    .await?;
                    retries = retries.saturating_add(1);
                    cleanup = self.spawn_cleanup(operation.fence.clone());
                }
                CleanupDecision::Fail(error) => {
                    self.reap_if_active(
                        PendingDurable::new(cleanup, pending.task_active),
                        DurableReap::new(operation.budget, "cleanup-generation"),
                    )
                    .await?;
                    return Err(error);
                }
            }
        }
    }

    fn spawn_cleanup(
        &self,
        fence: LeaseFence,
    ) -> tokio::task::JoinHandle<Result<(), StorageError>> {
        let database = self.database.clone();
        let statement_timeout = self.config.deadlines.statement_timeout();
        tokio::spawn(async move {
            database
                .fail_generation_and_release_bounded(TerminalGenerationMutation::new(
                    &fence,
                    statement_timeout,
                ))
                .await
        })
    }

    async fn finish_and_reap_durable<T>(
        &self,
        mut handle: tokio::task::JoinHandle<T>,
        context: DurableReap,
    ) -> Result<(), SupervisorError> {
        let deadline = OperationBudget::request_deadline(
            self.config.deadlines.heartbeat_request,
            context.budget.final_deadline,
        );
        if Instant::now() >= deadline {
            handle.abort();
            let _ = handle.await;
            return Err(SupervisorError::UnreapedDurableOperation {
                operation: context.operation,
            });
        }
        if timeout_at(deadline, &mut handle).await.is_ok() {
            Ok(())
        } else {
            handle.abort();
            let _ = handle.await;
            Err(SupervisorError::UnreapedDurableOperation {
                operation: context.operation,
            })
        }
    }

    async fn reap_if_active<T>(
        &self,
        pending: PendingDurable<T>,
        context: DurableReap,
    ) -> Result<(), SupervisorError> {
        if pending.active {
            self.finish_and_reap_durable(pending.handle, context).await
        } else {
            Ok(())
        }
    }

    async fn heartbeat_owned(
        &self,
        owned: &mut OwnedOperation,
        operation: &'static str,
    ) -> Result<(), SupervisorError> {
        let budget = owned.budget;
        let Some(lease) = owned.lease.take() else {
            let reconciliation = self.reconcile(owned, operation).await?;
            return if reconciliation.lease() == ObservedLease::Owned {
                Err(SupervisorError::AmbiguousOutcome { operation })
            } else {
                Err(SupervisorError::OwnershipLost { operation })
            };
        };
        let attempt = run_bounded_heartbeat(HeartbeatRequest {
            database: self.database.clone(),
            config: self.config,
            budget,
            ceiling: budget.durable_ceiling(self.config),
            operation,
            lease,
        })
        .await?;
        let late = attempt.late;
        let result = attempt.result;
        owned.lease = Some(attempt.lease);
        match (late, result) {
            (_, Ok(())) => {
                self.progress.mark_heartbeat();
                Ok(())
            }
            (_, Err(LeaseError::Lost)) => Err(SupervisorError::OwnershipLost { operation }),
            (true, Err(_)) | (false, Err(LeaseError::DatabaseOperation { .. })) => {
                let reconciliation = self.reconcile(owned, operation).await?;
                if reconciliation.lease() == ObservedLease::Owned {
                    Ok(())
                } else {
                    Err(SupervisorError::OwnershipLost { operation })
                }
            }
            (_, Err(source)) => Err(SupervisorError::Lease { operation, source }),
        }
    }

    async fn reconcile(
        &self,
        owned: &OwnedOperation,
        operation: &'static str,
    ) -> Result<OperationReconciliation, SupervisorError> {
        let budget = owned.budget;
        let deadline =
            OperationBudget::database_deadline(self.config, budget.durable_ceiling(self.config))?;
        let database = self.database.clone();
        let fence = owned.fence.clone();
        let statement_timeout = self.config.deadlines.statement_timeout();
        let mut reconciliation = tokio::spawn(async move {
            database
                .reconcile_operation_bounded(&fence, statement_timeout)
                .await
        });
        match timeout_at(deadline, &mut reconciliation).await {
            Ok(Ok(Ok(reconciliation))) => Ok(reconciliation),
            Ok(_) => Err(SupervisorError::AmbiguousOutcome { operation }),
            Err(_) => {
                self.finish_and_reap_durable(reconciliation, DurableReap::new(budget, operation))
                    .await?;
                Err(SupervisorError::AmbiguousOutcome { operation })
            }
        }
    }

    async fn acquire_exact(
        &self,
        attempt: LeaseAcquisitionAttempt,
        context: AcquisitionContext<'_>,
    ) -> Result<ProjectLease, SupervisorError> {
        let mut acquisition = self.spawn_acquisition(attempt);
        loop {
            let Ok(deadline) =
                OperationBudget::database_deadline(self.config, context.budget.work_deadline)
            else {
                self.finish_and_reap_durable(
                    acquisition,
                    DurableReap::new(context.budget, "acquire"),
                )
                .await?;
                return self
                    .resolve_acquisition(&context, ambiguous("acquire"))
                    .await;
            };
            let polled = tokio::select! {
                biased;
                changed = context.receiver.changed() => {
                    if changed.is_err() || *context.receiver.borrow_and_update() {
                        self.finish_and_reap_durable(
                            acquisition,
                            DurableReap::new(context.budget, "acquire"),
                        )
                        .await?;
                        return match self.reconcile_acquisition(&context).await? {
                            Some(lease) => Ok(lease),
                            None => Err(cancelled(CancellationReason::Requested, false)),
                        };
                    }
                    continue;
                },
                result = timeout_at(deadline, &mut acquisition) => result,
            };
            match polled {
                Ok(Ok(Ok(lease))) => return Ok(lease),
                Ok(Ok(Err(source))) if !matches!(source, LeaseError::DatabaseOperation { .. }) => {
                    return Err(SupervisorError::Lease {
                        operation: "acquire",
                        source,
                    });
                }
                Ok(Ok(Err(source))) => {
                    return self
                        .resolve_acquisition(
                            &context,
                            SupervisorError::Lease {
                                operation: "acquire",
                                source,
                            },
                        )
                        .await;
                }
                Ok(Err(_)) => {
                    return self
                        .resolve_acquisition(&context, ambiguous("acquire"))
                        .await;
                }
                Err(_) => match self.reconcile_acquisition(&context).await {
                    Ok(Some(lease)) => {
                        self.finish_and_reap_durable(
                            acquisition,
                            DurableReap::new(context.budget, "acquire"),
                        )
                        .await?;
                        return Ok(lease);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.finish_and_reap_durable(
                            acquisition,
                            DurableReap::new(context.budget, "acquire"),
                        )
                        .await?;
                        return Err(error);
                    }
                },
            }
        }
    }

    async fn resolve_acquisition(
        &self,
        context: &AcquisitionContext<'_>,
        fallback: SupervisorError,
    ) -> Result<ProjectLease, SupervisorError> {
        match self.reconcile_acquisition(context).await? {
            Some(lease) => Ok(lease),
            None => Err(fallback),
        }
    }

    fn spawn_acquisition(
        &self,
        attempt: LeaseAcquisitionAttempt,
    ) -> tokio::task::JoinHandle<Result<ProjectLease, LeaseError>> {
        let database = self.database.clone();
        let statement_timeout = self.config.deadlines.statement_timeout();
        tokio::spawn(async move {
            database
                .acquire_reconcilable_lease_bounded(attempt, statement_timeout)
                .await
        })
    }

    async fn reconcile_acquisition(
        &self,
        context: &AcquisitionContext<'_>,
    ) -> Result<Option<ProjectLease>, SupervisorError> {
        let deadline =
            OperationBudget::database_deadline(self.config, context.budget.final_deadline)?;
        let database = self.database.clone();
        let probe = context.probe.clone();
        let statement_timeout = self.config.deadlines.statement_timeout();
        let mut reconciliation = tokio::spawn(async move {
            database
                .reconcile_acquisition_bounded(&probe, statement_timeout)
                .await
        });
        match timeout_at(deadline, &mut reconciliation).await {
            Ok(Ok(Ok(lease))) => Ok(lease),
            Ok(_) => Err(ambiguous("acquire")),
            Err(_) => {
                self.finish_and_reap_durable(
                    reconciliation,
                    DurableReap::new(context.budget, "reconcile-acquire"),
                )
                .await?;
                Err(ambiguous("acquire"))
            }
        }
    }
}

impl IndexerSupervisor {
    fn start_lifecycle(&self) -> Result<(), SupervisorError> {
        self.lifecycle
            .lock()
            .map_err(|_| SupervisorError::LifecycleUnavailable)?
            .start()
    }

    fn select_cancellation(
        &self,
        reason: CancellationReason,
    ) -> Result<CancellationReason, SupervisorError> {
        self.lifecycle
            .lock()
            .map_err(|_| SupervisorError::LifecycleUnavailable)
            .map(|mut lifecycle| lifecycle.select_cancellation(reason))
    }

    fn begin_finishing(&self) -> Result<Option<CancellationReason>, SupervisorError> {
        self.lifecycle
            .lock()
            .map_err(|_| SupervisorError::LifecycleUnavailable)
            .map(|mut lifecycle| lifecycle.begin_finishing())
    }

    fn finish_lifecycle(&self) {
        if let Ok(mut lifecycle) = self.lifecycle.lock() {
            lifecycle.finish();
        }
    }
}

struct SupervisorRunGuard {
    supervisor: IndexerSupervisor,
    runtime: tokio::runtime::Handle,
    handle: Option<tokio::task::JoinHandle<Result<CurrentGeneration, SupervisorError>>>,
    reap_timeout: Duration,
}

impl SupervisorRunGuard {
    fn new(
        supervisor: IndexerSupervisor,
        runtime: tokio::runtime::Handle,
        handle: tokio::task::JoinHandle<Result<CurrentGeneration, SupervisorError>>,
    ) -> Self {
        let reap_timeout = supervisor
            .config
            .deadlines
            .operation
            .saturating_add(supervisor.config.deadlines.heartbeat_request);
        Self {
            supervisor,
            runtime,
            handle: Some(handle),
            reap_timeout,
        }
    }

    async fn join(mut self) -> Result<CurrentGeneration, SupervisorError> {
        let result = match self.handle.as_mut() {
            Some(handle) => handle.await,
            None => return Err(SupervisorError::LifecycleUnavailable),
        };
        self.handle.take();
        match result {
            Ok(result) => result,
            Err(_) => Err(SupervisorError::LifecycleUnavailable),
        }
    }
}

impl Drop for SupervisorRunGuard {
    fn drop(&mut self) {
        let Some(mut handle) = self.handle.take() else {
            return;
        };
        let _cancellation_was_new = self.supervisor.cancel();
        let reap_timeout = self.reap_timeout;
        // The caller can no longer await cleanup, so retain one bounded reaper
        // on the spawning runtime even if this guard is dropped from another thread.
        drop(self.runtime.spawn(async move {
            let deadline = Instant::now() + reap_timeout;
            if timeout_at(deadline, &mut handle).await.is_err() {
                handle.abort();
                let _ = handle.await;
            }
        }));
    }
}

struct LifecycleFinishGuard<'a> {
    supervisor: &'a IndexerSupervisor,
}

impl<'a> LifecycleFinishGuard<'a> {
    const fn new(supervisor: &'a IndexerSupervisor) -> Self {
        Self { supervisor }
    }
}

impl Drop for LifecycleFinishGuard<'_> {
    fn drop(&mut self) {
        self.supervisor.finish_lifecycle();
    }
}

struct OwnedOperation {
    target: LeaseTarget,
    lease: Option<ProjectLease>,
    fence: LeaseFence,
    budget: OperationBudget,
}

#[derive(Clone, Copy)]
struct DurableReap {
    budget: OperationBudget,
    operation: &'static str,
}

impl DurableReap {
    const fn new(budget: OperationBudget, operation: &'static str) -> Self {
        Self { budget, operation }
    }
}

struct PendingDurable<T> {
    handle: tokio::task::JoinHandle<T>,
    active: bool,
}

impl<T> PendingDurable<T> {
    const fn new(handle: tokio::task::JoinHandle<T>, active: bool) -> Self {
        Self { handle, active }
    }
}

struct AcquisitionContext<'a> {
    probe: LeaseAcquisitionProbe,
    budget: OperationBudget,
    receiver: &'a mut watch::Receiver<bool>,
}

struct StartedRun<Work> {
    request: SupervisorRequest,
    work: Work,
    receiver: watch::Receiver<bool>,
    budget: OperationBudget,
}

struct OwnedRun<Work> {
    operation: OwnedOperation,
    work: Work,
    receiver: watch::Receiver<bool>,
}

struct MonitoredWork {
    outcome: MonitorOutcome,
    work_reaped: bool,
    reap: ReapReport,
    prepare: PrepareReap,
}

enum MonitorOutcome {
    Ready(ReadyGeneration),
    Failed(PipelineFailure),
    Cancelled(CancelledWork),
    SupervisorFailed(SupervisorError),
}

impl MonitorOutcome {
    /// Whether finishing this outcome may still act under the lease, by
    /// publishing or by owned cleanup.
    const fn keeps_authority(&self) -> bool {
        match self {
            Self::Ready(_) | Self::Failed(_) => true,
            Self::Cancelled(cancelled) => !cancelled.reason.is_authority_uncertain(),
            Self::SupervisorFailed(error) => !error.forbids_cleanup(),
        }
    }
}

#[derive(Clone, Copy)]
struct CancelledWork {
    reason: CancellationReason,
    grace_exceeded: bool,
}

/// The monitor outcome and everything still to reap under it.
struct WorkReap<'a> {
    outcome: MonitorOutcome,
    work: SupervisedWork,
    tasks: &'a TaskScope,
    prepares: &'a PrepareScope,
}

/// Return the lease token recovered from reap-time renewal and fold its
/// verdict into the outcome.
///
/// A heartbeat that lost or failed to renew the token leaves the outcome to
/// the cleanup heartbeat, which re-verifies ownership before any mutation;
/// renewal that cannot vouch for the token ends the run.
fn settle_reap_renewal(
    operation: &mut OwnedOperation,
    outcome: MonitorOutcome,
    exit: KeeperExit,
) -> MonitorOutcome {
    let KeeperExit { lease, verdict } = exit;
    operation.lease = lease;
    match verdict {
        Some(LeaseVerdict::Failed(error)) => MonitorOutcome::SupervisorFailed(error),
        Some(LeaseVerdict::Cancel(_)) | None => outcome,
    }
}

struct WorkMonitor<'a> {
    config: SupervisorConfig,
    budget: OperationBudget,
    progress: SharedProgress,
    cancellation: &'a watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
    lifecycle: &'a Arc<Mutex<LifecycleGate>>,
    prepares: PrepareScope,
}

/// The pipeline work and lease-renewal tasks one monitor supervises.
struct MonitoredTasks {
    work: SupervisedWork,
    keeper: LeaseKeeper,
}

/// Monitor outcome plus the exact lease token and the tasks still to reap.
struct MonitoredRun {
    outcome: MonitorOutcome,
    lease: Option<ProjectLease>,
    tasks: MonitoredTasks,
}

/// First terminal event the monitor observed.
enum MonitorEvent {
    Cancel(CancellationReason),
    WorkCompleted(WorkCompletion),
    KeeperExited(KeeperExit),
}

/// A terminal monitor event together with the lease keeper's final verdict.
struct Resolution {
    verdict: Option<LeaseVerdict>,
    event: Option<MonitorEvent>,
}

/// Watchdog state for the operation's one retained prepare/COPY task.
struct PrepareWatch {
    sequence: u64,
    observed_at: Instant,
    was_running: bool,
}

/// Progress watchdog state, shared by the monitor loop and the priority
/// re-check after an in-flight heartbeat.
struct ProgressWatchdog {
    /// When the watchdog next re-evaluates progress.
    deadline: Instant,
    /// Prepare/COPY progress last observed by the watchdog.
    prepare: PrepareWatch,
}

impl WorkMonitor<'_> {
    async fn run(mut self, mut tasks: MonitoredTasks) -> MonitoredRun {
        let mut watchdog = self.progress_watchdog().await;
        let event = if *self.receiver.borrow_and_update() {
            MonitorEvent::Cancel(CancellationReason::Requested)
        } else {
            self.monitor_events(&mut tasks, &mut watchdog).await
        };
        let (exit, event) = match event {
            MonitorEvent::KeeperExited(exit) => (exit, None),
            event => {
                // Accepting the event freezes renewal: no heartbeat can start
                // afterwards, and one already in flight is reported exactly.
                let renewing = tasks.keeper.freeze_renewal();
                let exit = tasks.keeper.stop(self.keeper_stop_deadline()).await;
                let event = if renewing {
                    self.supersede(event, &mut watchdog).await
                } else {
                    event
                };
                (exit, Some(event))
            }
        };
        let KeeperExit { lease, verdict } = exit;
        let outcome = self
            .resolve(Resolution { verdict, event }, &mut tasks.work)
            .await;
        MonitoredRun {
            outcome,
            lease,
            tasks,
        }
    }

    /// Arm the progress watchdog one progress timeout after the last progress.
    async fn progress_watchdog(&self) -> ProgressWatchdog {
        ProgressWatchdog {
            deadline: self.progress.last_progress().await + self.config.deadlines.progress,
            prepare: PrepareWatch {
                sequence: self.prepares.progress_sequence(),
                observed_at: Instant::now(),
                was_running: self.prepares.is_running(),
            },
        }
    }

    async fn monitor_events(
        &mut self,
        tasks: &mut MonitoredTasks,
        watchdog: &mut ProgressWatchdog,
    ) -> MonitorEvent {
        let operation_deadline = sleep_until(self.budget.work_deadline);
        let progress_deadline = sleep_until(watchdog.deadline);
        tokio::pin!(operation_deadline);
        tokio::pin!(progress_deadline);

        loop {
            tokio::select! {
                biased;
                changed = self.receiver.changed() => {
                    if changed.is_err() || *self.receiver.borrow_and_update() {
                        break MonitorEvent::Cancel(CancellationReason::Requested);
                    }
                },
                () = &mut operation_deadline => {
                    break MonitorEvent::Cancel(CancellationReason::OperationDeadline);
                },
                () = &mut progress_deadline => {
                    match self.next_progress_deadline(&mut watchdog.prepare).await {
                        Some(next) => {
                            watchdog.deadline = next;
                            progress_deadline.as_mut().reset(next);
                        }
                        None => break MonitorEvent::Cancel(CancellationReason::ProgressStalled),
                    }
                },
                completion = tasks.work.completion() => {
                    break MonitorEvent::WorkCompleted(completion);
                },
                exit = tasks.keeper.exited() => break MonitorEvent::KeeperExited(exit),
            }
        }
    }

    /// Re-arm the progress watchdog after it fires; `None` means work stalled.
    ///
    /// Progress is evaluated from the shared last-progress instant when the
    /// watchdog fires, so the monitor needs no wake-up per progress update.
    async fn next_progress_deadline(&self, prepare: &mut PrepareWatch) -> Option<Instant> {
        let now = Instant::now();
        if self.prepares.is_running() {
            let sequence = self.prepares.progress_sequence();
            if !prepare.was_running || sequence != prepare.sequence {
                prepare.sequence = sequence;
                prepare.observed_at = now;
            }
            prepare.was_running = true;
            let durable_deadline = prepare.observed_at + self.config.deadlines.copy_timeout();
            if durable_deadline > now {
                return Some(durable_deadline.min(now + self.config.deadlines.progress));
            }
        } else {
            prepare.was_running = false;
        }
        let next = self.progress.last_progress().await + self.config.deadlines.progress;
        (next > now).then_some(next)
    }

    /// Re-apply the monitor's branch priority after an in-flight heartbeat.
    ///
    /// The former inline monitor finished a running heartbeat before it polled
    /// any other branch, so a cancellation request, the work deadline, or a
    /// progress stall that arrived during that heartbeat outranked the event,
    /// in that order. Only used when a heartbeat was in flight as the event was
    /// accepted; otherwise the event stands, as it did when the inline monitor
    /// acted on it immediately.
    async fn supersede(
        &mut self,
        event: MonitorEvent,
        watchdog: &mut ProgressWatchdog,
    ) -> MonitorEvent {
        if *self.receiver.borrow_and_update() {
            return MonitorEvent::Cancel(CancellationReason::Requested);
        }
        let now = Instant::now();
        if now >= self.budget.work_deadline {
            return MonitorEvent::Cancel(CancellationReason::OperationDeadline);
        }
        if now >= watchdog.deadline
            && self
                .next_progress_deadline(&mut watchdog.prepare)
                .await
                .is_none()
        {
            return MonitorEvent::Cancel(CancellationReason::ProgressStalled);
        }
        event
    }

    /// Decide the outcome of one monitored run.
    ///
    /// A verdict from a heartbeat that was in flight when the event arrived is
    /// applied first, exactly as the former inline monitor always finished a
    /// running heartbeat before it observed any other event.
    async fn resolve(&self, resolution: Resolution, work: &mut SupervisedWork) -> MonitorOutcome {
        match resolution {
            Resolution {
                verdict: Some(LeaseVerdict::Cancel(reason)),
                ..
            }
            | Resolution {
                verdict: None,
                event: Some(MonitorEvent::Cancel(reason)),
            } => self.cancel_work(reason, work).await,
            Resolution {
                verdict: Some(LeaseVerdict::Failed(error)),
                ..
            } => self.fail_supervision(error),
            Resolution {
                verdict: None,
                event: Some(MonitorEvent::WorkCompleted(completion)),
            } => self.complete_work(completion),
            // A keeper only exits on its own with a verdict; fail closed otherwise.
            Resolution { verdict: None, .. } => {
                self.fail_supervision(ambiguous(HEARTBEAT_OPERATION))
            }
        }
    }

    /// Fail supervision when lease renewal can no longer vouch for authority.
    ///
    /// The cooperative signal reaches work and children that poll it in
    /// synchronous sections, which task abortion alone cannot interrupt; no
    /// cancellation reason is selected, so the failure and its cleanup ban hold.
    fn fail_supervision(&self, error: SupervisorError) -> MonitorOutcome {
        self.cancellation.send_replace(true);
        MonitorOutcome::SupervisorFailed(error)
    }

    /// Absolute bound for a keeper to finish an in-flight heartbeat.
    ///
    /// The heartbeat is already bounded by its own request and reap horizons,
    /// which the former inline monitor awaited without a further limit, and it
    /// started before the work deadline, so it ends well before this durable
    /// ceiling; a slow but healthy heartbeat is never failed early. The cap only
    /// guards a keeper that never stops.
    fn keeper_stop_deadline(&self) -> Instant {
        self.budget.durable_ceiling(self.config)
    }

    fn complete_work(&self, completion: WorkCompletion) -> MonitorOutcome {
        let transition = self.lifecycle.lock();
        let Ok(mut lifecycle) = transition else {
            return MonitorOutcome::Failed(PipelineFailure::new(PipelineStage::Reduce));
        };
        match completion {
            WorkCompletion::Finished(Ok(ready)) => match lifecycle.begin_publication() {
                Ok(()) => MonitorOutcome::Ready(ready),
                Err(reason) => cancelled_outcome(reason),
            },
            WorkCompletion::Finished(Err(failure)) => match lifecycle.begin_finishing() {
                Some(reason) => cancelled_outcome(reason),
                None => MonitorOutcome::Failed(failure),
            },
            WorkCompletion::Interrupted => match lifecycle.begin_finishing() {
                Some(reason) => cancelled_outcome(reason),
                None => MonitorOutcome::SupervisorFailed(SupervisorError::WorkerFailed),
            },
        }
    }

    async fn cancel_work(
        &self,
        reason: CancellationReason,
        work: &mut SupervisedWork,
    ) -> MonitorOutcome {
        let selected = self.lifecycle.lock().map_or(reason, |mut lifecycle| {
            lifecycle.select_cancellation(reason)
        });
        // Signal before progress stops accepting updates: work on its own task
        // that sees `NotActive` must already observe cancellation, or it would
        // record a stage failure that overrides the selected reason.
        self.cancellation.send_replace(true);
        self.progress.mark_cancelling(selected).await;
        if selected.is_authority_uncertain() {
            return cancelled_outcome(selected);
        }
        let grace_deadline = OperationBudget::request_deadline(
            self.config.deadlines.cancellation_grace,
            self.budget.reap_ceiling(self.config),
        );
        let completed = work.finished_by(grace_deadline).await;
        MonitorOutcome::Cancelled(CancelledWork {
            reason: selected,
            grace_exceeded: !completed,
        })
    }
}

const fn cancelled_outcome(reason: CancellationReason) -> MonitorOutcome {
    MonitorOutcome::Cancelled(CancelledWork {
        reason,
        grace_exceeded: false,
    })
}

#[derive(Clone, Copy)]
struct OperationBudget {
    work_deadline: Instant,
    final_deadline: Instant,
}

impl OperationBudget {
    fn new(config: SupervisorConfig) -> Self {
        let final_deadline = Instant::now() + config.deadlines.operation;
        Self {
            work_deadline: final_deadline - config.deadlines.finish_reserve(),
            final_deadline,
        }
    }

    fn request_deadline(maximum: Duration, ceiling: Instant) -> Instant {
        (Instant::now() + maximum).min(ceiling)
    }

    fn database_deadline(
        config: SupervisorConfig,
        ceiling: Instant,
    ) -> Result<Instant, SupervisorError> {
        if Instant::now() >= ceiling {
            Err(SupervisorError::OperationBudgetExhausted)
        } else {
            Ok(Self::request_deadline(
                config.deadlines.heartbeat_request,
                ceiling,
            ))
        }
    }

    fn durable_ceiling(self, config: SupervisorConfig) -> Instant {
        self.final_deadline - config.deadlines.heartbeat_request
    }

    /// Latest instant for cancellation grace and for reaping the root work,
    /// registered children, and the prepare task; it leaves the database finish
    /// reserve for owned cleanup.
    fn reap_ceiling(self, config: SupervisorConfig) -> Instant {
        self.final_deadline - config.deadlines.database_finish_reserve()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LifecyclePhase {
    Queued,
    Running,
    Publishing,
    Finishing,
    Terminal,
}

struct LifecycleGate {
    phase: LifecyclePhase,
    cancellation: Option<CancellationReason>,
}

impl LifecycleGate {
    const fn new() -> Self {
        Self {
            phase: LifecyclePhase::Queued,
            cancellation: None,
        }
    }

    fn start(&mut self) -> Result<(), SupervisorError> {
        if self.phase == LifecyclePhase::Queued {
            self.phase = LifecyclePhase::Running;
            Ok(())
        } else {
            Err(SupervisorError::AlreadyStarted)
        }
    }

    fn request_external_cancellation(&mut self) -> bool {
        if !matches!(self.phase, LifecyclePhase::Queued | LifecyclePhase::Running)
            || self.cancellation.is_some()
        {
            false
        } else {
            self.cancellation = Some(CancellationReason::Requested);
            true
        }
    }

    fn select_cancellation(&mut self, reason: CancellationReason) -> CancellationReason {
        let selected = self.cancellation.unwrap_or(reason);
        self.cancellation = Some(selected);
        self.phase = LifecyclePhase::Finishing;
        selected
    }

    fn begin_publication(&mut self) -> Result<(), CancellationReason> {
        if let Some(reason) = self.cancellation {
            self.phase = LifecyclePhase::Finishing;
            Err(reason)
        } else {
            self.phase = LifecyclePhase::Publishing;
            Ok(())
        }
    }

    fn begin_finishing(&mut self) -> Option<CancellationReason> {
        self.phase = LifecyclePhase::Finishing;
        self.cancellation
    }

    fn finish(&mut self) {
        self.phase = LifecyclePhase::Terminal;
    }
}

struct ExpectedGeneration {
    project_id: ProjectId,
    generation_id: GenerationId,
    sequence: i64,
    digest: ContentDigest,
}

struct PublicationEvidence {
    reconciliation: OperationReconciliation,
    retry: Option<ReadyGeneration>,
    retry_allowed: bool,
}

struct PendingPublication {
    retry: Option<ReadyGeneration>,
    retry_allowed: bool,
    task_active: bool,
}

enum PublicationPoll {
    Complete(CurrentGeneration),
    Reconcile(PendingPublication),
}

enum PublicationDecision {
    Complete(CurrentGeneration),
    Wait,
    Retry(ReadyGeneration),
    Fail(SupervisorError),
}

struct CleanupEvidence {
    reconciliation: OperationReconciliation,
    retry_allowed: bool,
    task_active: bool,
}

enum CleanupDecision {
    Complete,
    Wait,
    Retry,
    Fail(SupervisorError),
}

struct PendingCleanup {
    retry_allowed: bool,
    task_active: bool,
}

enum CleanupPoll {
    Complete,
    Reconcile(PendingCleanup),
}

type PublicationWait = Result<
    Result<
        Result<CurrentGeneration, cartograph_db::PublishGenerationError>,
        tokio::task::JoinError,
    >,
    tokio::time::error::Elapsed,
>;

type CleanupWait =
    Result<Result<Result<(), StorageError>, tokio::task::JoinError>, tokio::time::error::Elapsed>;

impl ExpectedGeneration {
    fn from_ready(ready: &ReadyGeneration) -> Self {
        Self {
            project_id: ready.project_id().clone(),
            generation_id: ready.generation_id().clone(),
            sequence: ready.sequence(),
            digest: ready.content_digest().clone(),
        }
    }

    fn matches_ready(&self, ready: &ReadyGeneration) -> bool {
        ready.project_id() == &self.project_id
            && ready.generation_id() == &self.generation_id
            && ready.sequence() == self.sequence
            && ready.content_digest() == &self.digest
    }

    fn matches_current(&self, current: &CurrentGeneration) -> bool {
        current.project_id() == &self.project_id
            && current.generation_id() == &self.generation_id
            && current.sequence() == self.sequence
            && current.content_digest() == &self.digest
    }
}

fn decide_publication(
    expected: &ExpectedGeneration,
    evidence: PublicationEvidence,
) -> PublicationDecision {
    let lease = evidence.reconciliation.lease();
    match evidence.reconciliation.into_generation() {
        ObservedGeneration::Current(current)
            if expected.matches_current(&current) && lease != ObservedLease::Owned =>
        {
            PublicationDecision::Complete(current)
        }
        ObservedGeneration::Ready(observed) if expected.matches_ready(&observed) => {
            if lease != ObservedLease::Owned {
                PublicationDecision::Fail(ownership_lost("publish-generation"))
            } else if let Some(retry) = evidence.retry {
                if evidence.retry_allowed && expected.matches_ready(&retry) {
                    PublicationDecision::Retry(retry)
                } else {
                    PublicationDecision::Fail(ambiguous("publish-generation"))
                }
            } else {
                PublicationDecision::Wait
            }
        }
        _ => PublicationDecision::Fail(ambiguous("publish-generation")),
    }
}

fn classify_publication_poll(
    result: PublicationWait,
    retry_allowed: bool,
) -> Result<PublicationPoll, SupervisorError> {
    match result {
        Ok(Ok(Ok(current))) => Ok(PublicationPoll::Complete(current)),
        Ok(Ok(Err(error))) if *error.error() == StorageError::LeaseFenceLost => {
            Err(ownership_lost("publish-generation"))
        }
        Ok(Ok(Err(error))) if !matches!(error.error(), StorageError::DatabaseOperation { .. }) => {
            let (_, source) = error.into_parts();
            Err(SupervisorError::Storage {
                operation: "publish-generation",
                source,
            })
        }
        Ok(Ok(Err(error))) => Ok(PublicationPoll::Reconcile(PendingPublication {
            retry: Some(error.into_parts().0),
            retry_allowed,
            task_active: false,
        })),
        Ok(Err(_)) => Err(ambiguous("publish-generation")),
        Err(_) => Ok(PublicationPoll::Reconcile(PendingPublication {
            retry: None,
            retry_allowed: false,
            task_active: true,
        })),
    }
}

fn decide_cleanup(evidence: CleanupEvidence) -> CleanupDecision {
    let lease = evidence.reconciliation.lease();
    match evidence.reconciliation.into_generation() {
        ObservedGeneration::Failed if lease != ObservedLease::Owned => CleanupDecision::Complete,
        ObservedGeneration::Staged(_)
        | ObservedGeneration::Ready(_)
        | ObservedGeneration::Failed
            if lease == ObservedLease::Owned =>
        {
            if evidence.retry_allowed {
                CleanupDecision::Retry
            } else if evidence.task_active {
                CleanupDecision::Wait
            } else {
                CleanupDecision::Fail(ambiguous("cleanup-generation"))
            }
        }
        ObservedGeneration::Staged(_) | ObservedGeneration::Ready(_)
            if lease != ObservedLease::Owned =>
        {
            CleanupDecision::Fail(ownership_lost("cleanup-generation"))
        }
        _ => CleanupDecision::Fail(ambiguous("cleanup-generation")),
    }
}

fn classify_cleanup_poll(
    result: CleanupWait,
    retry_allowed: bool,
) -> Result<CleanupPoll, SupervisorError> {
    match result {
        Ok(Ok(Ok(()))) => Ok(CleanupPoll::Complete),
        Ok(Ok(Err(StorageError::LeaseFenceLost))) => Err(ownership_lost("cleanup-generation")),
        Ok(Ok(Err(error))) if !matches!(error, StorageError::DatabaseOperation { .. }) => {
            Err(SupervisorError::Storage {
                operation: "cleanup-generation",
                source: error,
            })
        }
        Ok(Ok(Err(_))) => Ok(CleanupPoll::Reconcile(PendingCleanup {
            retry_allowed,
            task_active: false,
        })),
        Ok(Err(_)) => Err(ambiguous("cleanup-generation")),
        Err(_) => Ok(CleanupPoll::Reconcile(PendingCleanup {
            retry_allowed: false,
            task_active: true,
        })),
    }
}

impl CancellationReason {
    const fn is_authority_uncertain(self) -> bool {
        matches!(self, Self::LeaseLost | Self::LeaseHeartbeatFailed)
    }
}

/// Credential-safe supervisor failure.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// Deadline configuration is invalid before any external mutation.
    #[error(transparent)]
    InvalidConfig(#[from] InvalidSupervisorConfig),
    /// The one-shot supervisor was reused.
    #[error("Cartograph indexer supervisor has already started")]
    AlreadyStarted,
    /// Lease acquisition, renewal, or exact release failed.
    #[error("Cartograph indexer lease operation failed during {operation}")]
    Lease {
        /// Stable operation identifier.
        operation: &'static str,
        /// Credential-safe database lease error.
        #[source]
        source: LeaseError,
    },
    /// Durable generation cleanup or verification failed safely.
    #[error("Cartograph indexer storage operation failed during {operation}")]
    Storage {
        /// Stable operation identifier.
        operation: &'static str,
        /// Credential-safe storage error.
        #[source]
        source: StorageError,
    },
    /// A pipeline mutation lost its exact database lease token.
    #[error("Cartograph indexer lost exact lease ownership during {operation}")]
    OwnershipLost {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// PostgreSQL could not prove whether a timed-out mutation committed.
    #[error("Cartograph indexer could not reconcile durable outcome during {operation}")]
    AmbiguousOutcome {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// Pipeline work returned a stage-scoped failure.
    #[error("Cartograph indexing failed during the {stage} stage")]
    Pipeline {
        /// Stable pipeline stage.
        stage: PipelineStage,
    },
    /// Pipeline work returned an allowlisted stage/reason failure.
    #[error("Cartograph indexing failed during {stage}/{reason}")]
    PipelineWithReason {
        /// Stable pipeline stage.
        stage: PipelineStage,
        /// Stable actionable reason with no source or driver text.
        reason: PipelineFailureReason,
    },
    /// A pipeline input failed with one bounded project-relative diagnostic.
    #[error("Cartograph indexing failed during {stage}/{failure}")]
    PipelineWithFileFailure {
        /// Stable pipeline stage.
        stage: PipelineStage,
        /// Exact normalized relative input plus one allowlisted reason.
        failure: PipelineFileFailure,
    },
    /// A full index operation did not identify a generation to publish.
    #[error("Cartograph indexer supervision requires a generation-bound lease target")]
    MissingGeneration,
    /// Work returned a ready token for a different project or generation.
    #[error("Cartograph indexer work returned a mismatched ready generation")]
    GenerationMismatch,
    /// Controlled stage progress failed its monotonic contract.
    #[error("Cartograph indexer progress contract failed")]
    Progress {
        /// Stable progress-domain failure.
        #[source]
        source: ProgressError,
    },
    /// Supervision cancelled work and completed every safe owned cleanup step.
    #[error("Cartograph indexing was cancelled because {reason:?}")]
    Cancelled {
        /// Stable cancellation cause.
        reason: CancellationReason,
        /// Whether the work future or a registered worker exceeded its grace period.
        grace_exceeded: bool,
    },
    /// A work future returned while a registered child was still running.
    #[error("Cartograph indexing returned before all registered workers completed")]
    UnjoinedWorkers,
    /// A registered worker or the root pipeline task panicked before it was joined.
    #[error("Cartograph indexing registered worker failed")]
    WorkerFailed,
    /// A registered worker result was dropped instead of being joined.
    #[error("Cartograph indexing registered worker result was not observed")]
    UnobservedWorkers,
    /// An aborted registered worker or root pipeline task could not be joined by
    /// the absolute deadline.
    #[error("Cartograph indexing could not reap all registered workers by its deadline")]
    UnreapedWorkers,
    /// An acquisition, heartbeat, publication, or cleanup task could not be reaped.
    #[error("Cartograph indexer could not reap durable operation {operation} by its deadline")]
    UnreapedDurableOperation {
        /// Stable database operation identifier.
        operation: &'static str,
    },
    /// The absolute operation budget was exhausted before a required database request.
    #[error("Cartograph indexer exhausted its whole-operation deadline")]
    OperationBudgetExhausted,
    /// The in-process lifecycle gate became unavailable.
    #[error("Cartograph indexer lifecycle gate is unavailable")]
    LifecycleUnavailable,
    /// Owned cleanup failed after the failure that ended the run.
    ///
    /// The primary failure stays authoritative for classification and retry;
    /// the cleanup failure is secondary detail about the owned generation.
    #[error("{primary}; owned generation cleanup also failed: {cleanup}")]
    CleanupFailed {
        /// The failure or cancellation that ended the run.
        primary: Box<SupervisorError>,
        /// The owned-generation cleanup failure observed afterward.
        cleanup: Box<SupervisorError>,
    },
}

impl SupervisorError {
    const fn forbids_cleanup(&self) -> bool {
        matches!(
            self,
            Self::OwnershipLost { .. }
                | Self::AmbiguousOutcome { .. }
                | Self::OperationBudgetExhausted
                | Self::UnreapedWorkers
                | Self::UnreapedDurableOperation { .. }
        )
    }
}

const fn cancelled(reason: CancellationReason, grace_exceeded: bool) -> SupervisorError {
    SupervisorError::Cancelled {
        reason,
        grace_exceeded,
    }
}

fn cleanup_failed(primary: SupervisorError, cleanup: SupervisorError) -> SupervisorError {
    SupervisorError::CleanupFailed {
        primary: Box::new(primary),
        cleanup: Box::new(cleanup),
    }
}

const fn ambiguous(operation: &'static str) -> SupervisorError {
    SupervisorError::AmbiguousOutcome { operation }
}

const fn ownership_lost(operation: &'static str) -> SupervisorError {
    SupervisorError::OwnershipLost { operation }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_failure_preserves_one_relative_path_without_terminal_injection() {
        let path = NormalizedPath::parse("src/broken\nfile.rs")
            .unwrap_or_else(|error| panic!("fixture path was invalid: {error}"));
        let pipeline = PipelineFailure::with_file_failure(
            PipelineStage::Parse,
            PipelineFileFailure::new(path, PipelineFailureReason::ExtractionParserStopped),
        );
        assert_eq!(pipeline.stage(), PipelineStage::Parse);
        assert_eq!(
            pipeline.reason(),
            Some(PipelineFailureReason::ExtractionParserStopped)
        );

        let supervisor = pipeline_supervisor_error(pipeline);
        let SupervisorError::PipelineWithFileFailure { stage, failure } = supervisor else {
            panic!("file failure was flattened before the supervisor boundary");
        };
        assert_eq!(stage, PipelineStage::Parse);
        assert_eq!(failure.path().as_str(), "src/broken\nfile.rs");
        let rendered = failure.to_string();
        assert_eq!(rendered.lines().count(), 1);
        assert!(rendered.contains("src/broken\\nfile.rs"));
    }

    #[test]
    fn cancellation_and_publication_are_one_atomic_lifecycle_decision() {
        let mut cancelled = LifecycleGate::new();
        assert!(cancelled.request_external_cancellation());
        assert!(cancelled.start().is_ok());
        assert_eq!(
            cancelled.begin_publication(),
            Err(CancellationReason::Requested)
        );
        cancelled.finish();
        assert!(!cancelled.request_external_cancellation());

        let mut publishing = LifecycleGate::new();
        assert!(publishing.start().is_ok());
        assert!(publishing.begin_publication().is_ok());
        assert!(!publishing.request_external_cancellation());
        publishing.finish();
        assert!(!publishing.request_external_cancellation());
    }
}
