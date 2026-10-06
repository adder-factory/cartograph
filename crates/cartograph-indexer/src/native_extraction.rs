use std::time::Duration;

use cartograph_extract::{
    ExtractedFile, NativeExtractor, SourceSnapshot, native_extraction_reservation,
};
use thiserror::Error;
use tokio::{
    runtime::{Handle, RuntimeFlavor},
    task::block_in_place,
    time::Instant,
};

use crate::{
    PipelineStage, StageCapacity, StageDeadlinePolicy, StageEnvelope, StageExecution, StageFold,
    StageItemBudget, StageItemFailure, StageItemMeta, StageMetrics, StageOutput, StageRunConfig,
    StageRunError, StageRunner, StageSequence, StageWorkItem, StageWorkload,
};

/// Hard concurrency and deadline policy for the first native parse/extract stage.
#[derive(Clone, Copy, Debug)]
pub struct NativeExtractionStageConfig {
    capacity: StageCapacity,
    item_timeout: Duration,
    deadlines: StageDeadlinePolicy,
}

impl NativeExtractionStageConfig {
    /// Validate native parse capacity and independent item/stage cleanup bounds.
    /// # Errors
    ///
    /// Returns an error if worker/queue capacity overflows, item timeout is
    /// zero, the stage deadline has elapsed, or cleanup bounds are inconsistent.
    pub fn new(
        capacity: StageCapacity,
        item_timeout: Duration,
        deadlines: StageDeadlinePolicy,
    ) -> Result<Self, NativeExtractionConfigError> {
        if capacity.workers() == 0 {
            return Err(NativeExtractionConfigError::Invalid { field: "workers" });
        }
        if capacity
            .workers()
            .checked_add(capacity.queued_items())
            .is_none()
        {
            return Err(NativeExtractionConfigError::Invalid {
                field: "queued_items",
            });
        }
        if item_timeout.is_zero() {
            return Err(NativeExtractionConfigError::Invalid {
                field: "item_timeout",
            });
        }
        if deadlines.deadline() <= Instant::now() {
            return Err(NativeExtractionConfigError::Invalid {
                field: "stage_deadline",
            });
        }
        if deadlines.cleanup_grace().is_zero() {
            return Err(NativeExtractionConfigError::Invalid {
                field: "cleanup_grace",
            });
        }
        Ok(Self {
            capacity,
            item_timeout,
            deadlines,
        })
    }
}

/// Complete lazy input and observability request for one native extraction stage.
pub struct NativeExtractionRequest<Inputs, Observer> {
    config: NativeExtractionStageConfig,
    snapshots: Inputs,
    metrics: StageMetrics,
    observer: Observer,
}

impl<Inputs, Observer> NativeExtractionRequest<Inputs, Observer> {
    /// Bind validated policy, lazy snapshots, metrics, and a trusted bounded observer.
    pub fn new(config: NativeExtractionStageConfig, snapshots: Inputs, observer: Observer) -> Self {
        Self {
            config,
            snapshots,
            metrics: StageMetrics::new(),
            observer,
        }
    }

    /// Attach an observable in-flight reservation metric collector for this run.
    #[must_use]
    pub fn with_metrics(mut self, metrics: StageMetrics) -> Self {
        self.metrics = metrics;
        self
    }
}

/// Temporary synchronous observation hook for one ordered native extraction output.
///
/// This trusted contract is for fixed-size digest/validation state only. It does not transfer
/// ownership and is not the production resolve/COPY handoff: implementations can still copy public
/// fields, so callers must not retain corpus-proportional state. Owned output plus transferable
/// reservations/backpressure belongs in the later supervised persistence pipeline.
pub trait NativeExtractionObserver: Send {
    /// Consume one in-order borrowed output while its parse reservation remains held.
    /// # Errors
    ///
    /// Returns an error when the observer rejects the in-order extracted file.
    fn observe(&mut self, file: &ExtractedFile) -> Result<(), NativeExtractionObserverError>;
}

/// A native extraction observer rejected an ordered output without exposing project data.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("native extraction observer rejected an output")]
pub struct NativeExtractionObserverError;

/// Fixed-size accounting for a completed streaming native extraction stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeExtractionReport {
    files: u64,
    source_bytes: u64,
    modeled_output_bytes: u64,
    maximum_file_output_bytes: u64,
}

impl NativeExtractionReport {
    /// Number of files synchronously handed to the bounded observer.
    #[must_use]
    pub const fn files(self) -> u64 {
        self.files
    }

    /// Sum of exact source bytes processed.
    #[must_use]
    pub const fn source_bytes(self) -> u64 {
        self.source_bytes
    }

    /// Sum of per-file modeled output bytes; this is throughput, not retained memory.
    #[must_use]
    pub const fn modeled_output_bytes(self) -> u64 {
        self.modeled_output_bytes
    }

    /// Largest modeled `ExtractedFile` observed while its reservation was held.
    #[must_use]
    pub const fn maximum_file_output_bytes(self) -> u64 {
        self.maximum_file_output_bytes
    }

    fn observe(&mut self, file: &ExtractedFile) -> Result<(), NativeExtractionObserverError> {
        let output_bytes = file.modeled_retained_bytes();
        self.files = self
            .files
            .checked_add(1)
            .ok_or(NativeExtractionObserverError)?;
        self.source_bytes = self
            .source_bytes
            .checked_add(file.byte_size)
            .ok_or(NativeExtractionObserverError)?;
        self.modeled_output_bytes = self
            .modeled_output_bytes
            .checked_add(output_bytes)
            .ok_or(NativeExtractionObserverError)?;
        self.maximum_file_output_bytes = self.maximum_file_output_bytes.max(output_bytes);
        Ok(())
    }
}

/// Completed native stage accounting plus the bounded observer's final state.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeExtractionResult<Observer> {
    report: NativeExtractionReport,
    observer: Observer,
}

impl<Observer> NativeExtractionResult<Observer> {
    /// Fixed-size stage throughput accounting.
    #[must_use]
    pub const fn report(&self) -> NativeExtractionReport {
        self.report
    }

    /// Split the report from caller-defined bounded observer state.
    #[must_use]
    pub fn into_parts(self) -> (NativeExtractionReport, Observer) {
        (self.report, self.observer)
    }
}

/// Stable native extraction configuration failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NativeExtractionConfigError {
    /// One named bound was zero, expired, or overflowed.
    #[error("invalid {field} in native extraction configuration")]
    Invalid {
        /// Stable field name without project data.
        field: &'static str,
    },
}

/// Native extraction could not be planned or completed under supervision.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativeExtractionStageError {
    /// CPU parsing requires Tokio's multi-thread runtime so async control work can progress.
    #[error("native extraction requires a multi-thread Tokio runtime")]
    Runtime,
    /// The shared stage runner rejected, cancelled, or failed an item.
    #[error(transparent)]
    Stage(#[from] StageRunError),
}

/// Parse/extract lazily supplied snapshots under the shared bounded ordered stage runner.
///
/// The iterator must not retain all source payloads outside the admitted window. Native parsing
/// runs inside `block_in_place`, so Tokio moves unrelated async control work off that worker while
/// the tree-sitter progress callback observes parent, sibling, and deadline cancellation.
/// # Errors
///
/// Returns an error if no multi-thread Tokio runtime is active, parsing is
/// cancelled/times out, an extractor fails, or the ordered observer rejects output.
pub async fn run_native_extraction_stage<Inputs, Observer>(
    runner: &StageRunner,
    request: NativeExtractionRequest<Inputs, Observer>,
) -> Result<NativeExtractionResult<Observer>, NativeExtractionStageError>
where
    Inputs: IntoIterator<Item = SourceSnapshot>,
    Inputs::IntoIter: Send,
    Observer: NativeExtractionObserver,
{
    let NativeExtractionRequest {
        config,
        snapshots,
        metrics,
        observer,
    } = request;
    let runtime = Handle::try_current().map_err(|_| NativeExtractionStageError::Runtime)?;
    if !matches!(runtime.runtime_flavor(), RuntimeFlavor::MultiThread) {
        return Err(NativeExtractionStageError::Runtime);
    }
    let envelopes = NativeEnvelopeIterator {
        snapshots: snapshots.into_iter(),
        sequence: 0,
        item_timeout: config.item_timeout,
        stage_deadline: config.deadlines.deadline(),
    };
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Parse, config.capacity, config.deadlines),
        StageWorkload::new(
            envelopes,
            |item: StageWorkItem<cartograph_domain::FileId, SourceSnapshot>| async move {
                let cancellation = item.cancellation();
                let (_, _, snapshot) = item.into_parts();
                block_in_place(move || {
                    let mut extractor =
                        NativeExtractor::new(snapshot.language()).map_err(|_| StageItemFailure)?;
                    extractor
                        .extract_with_cancellation(&snapshot, || cancellation.is_cancelled())
                        .map_err(|_| StageItemFailure)
                })
            },
        ),
        StageFold::new(
            NativeExtractionFold {
                report: NativeExtractionReport::default(),
                observer,
            },
            |fold: &mut NativeExtractionFold<Observer>,
             output: StageOutput<cartograph_domain::FileId, ExtractedFile>| {
                let (_, file) = output.into_parts();
                fold.report.observe(&file).map_err(|_| StageItemFailure)?;
                fold.observer.observe(&file).map_err(|_| StageItemFailure)?;
                Ok(())
            },
        ),
    )
    .with_metrics(metrics);
    let fold = runner.execute(execution).await?;
    Ok(NativeExtractionResult {
        report: fold.report,
        observer: fold.observer,
    })
}

struct NativeExtractionFold<Observer> {
    report: NativeExtractionReport,
    observer: Observer,
}

struct NativeEnvelopeIterator<Inputs> {
    snapshots: Inputs,
    sequence: u64,
    item_timeout: Duration,
    stage_deadline: Instant,
}

impl<Inputs> Iterator for NativeEnvelopeIterator<Inputs>
where
    Inputs: Iterator<Item = SourceSnapshot>,
{
    type Item = StageEnvelope<cartograph_domain::FileId, SourceSnapshot>;

    fn next(&mut self) -> Option<Self::Item> {
        let snapshot = self.snapshots.next()?;
        let reserved_bytes =
            native_extraction_reservation(snapshot.byte_size()).unwrap_or(u64::MAX);
        let sequence = StageSequence::new(self.sequence);
        self.sequence = self.sequence.saturating_add(1);
        let key = snapshot.file_id().clone();
        let item_deadline =
            planned_item_deadline(Instant::now(), self.item_timeout, self.stage_deadline);
        let budget = StageItemBudget::new(reserved_bytes, snapshot.byte_size(), item_deadline);
        Some(StageEnvelope::new(
            StageItemMeta::new(sequence, key, budget),
            snapshot,
        ))
    }
}

fn planned_item_deadline(
    admitted_at: Instant,
    item_timeout: Duration,
    stage_deadline: Instant,
) -> Instant {
    admitted_at
        .checked_add(item_timeout)
        .unwrap_or(stage_deadline)
        .min(stage_deadline)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::time::Duration;

    use cartograph_extract::{ExtractedFile, SourceLimits, SourceSnapshot, native_output_limit};
    use tokio::time::Instant;

    use super::{
        NativeExtractionConfigError, NativeExtractionObserver, NativeExtractionObserverError,
        NativeExtractionRequest, NativeExtractionStageConfig, NativeExtractionStageError,
        planned_item_deadline, run_native_extraction_stage,
    };
    use crate::{
        StageCapacity, StageDeadlinePolicy, StageMetrics, stage::test_stage_runner as runner,
    };

    const TYPESCRIPT_SOURCE: &str =
        include_str!("../../cartograph-extract/tests/fixtures/v1_1_33/greeter.ts");
    const JAVASCRIPT_SOURCE: &str =
        include_str!("../../cartograph-extract/tests/fixtures/v1_1_33/worker.js");
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);
    const TEST_ITEM_TIMEOUT: Duration = Duration::from_secs(1);
    const TEST_CLEANUP_GRACE: Duration = Duration::from_millis(250);
    const SECOND_ADMISSION_OFFSET: Duration = Duration::from_secs(2);
    const DEADLINE_TEST_ITEM_TIMEOUT: Duration = Duration::from_secs(3);
    const DEADLINE_TEST_STAGE_WINDOW: Duration = Duration::from_secs(10);
    const DEADLINE_TEST_LONG_TIMEOUT: Duration = Duration::from_secs(20);
    const SERIAL_WORKERS: usize = 1;
    const PARALLEL_WORKERS: usize = 4;
    const TEST_MAX_TASKS: usize = 8;
    const TEST_SCOPE_BYTES: u64 = 16 * 1024 * 1024;
    const TEST_SOURCE_LIMIT_BYTES: usize = 1024 * 1024;
    const FIXTURE_COUNT: usize = 6;
    const MANY_FIXTURE_COUNT: usize = 256;
    const EXPECTED_COMPLETED_ITEMS: u64 = 6;
    const LANGUAGE_ALTERNATION: usize = 2;
    const EMPTY_ITEMS: usize = 0;
    const EMPTY_BYTES: u64 = 0;
    const WINDOW_MULTIPLIER: usize = 2;

    #[test]
    fn item_deadlines_start_at_admission_and_never_cross_the_stage_ceiling() {
        let first_admission = Instant::now();
        let second_admission = first_admission + SECOND_ADMISSION_OFFSET;
        let stage_deadline = first_admission + DEADLINE_TEST_STAGE_WINDOW;
        let item_timeout = DEADLINE_TEST_ITEM_TIMEOUT;

        assert_eq!(
            planned_item_deadline(first_admission, item_timeout, stage_deadline),
            first_admission + item_timeout
        );
        assert_eq!(
            planned_item_deadline(second_admission, item_timeout, stage_deadline),
            second_admission + item_timeout
        );
        assert_eq!(
            planned_item_deadline(second_admission, DEADLINE_TEST_LONG_TIMEOUT, stage_deadline,),
            stage_deadline
        );
    }

    #[test]
    fn native_stage_configuration_rejects_invalid_capacity_and_deadlines() {
        let deadline = Instant::now() + TEST_TIMEOUT;
        assert_matches!(
            NativeExtractionStageConfig::new(
                StageCapacity::new(EMPTY_ITEMS, EMPTY_ITEMS),
                TEST_ITEM_TIMEOUT,
                StageDeadlinePolicy::new(deadline, TEST_CLEANUP_GRACE),
            ),
            Err(NativeExtractionConfigError::Invalid { field: "workers" })
        );
        assert_matches!(
            NativeExtractionStageConfig::new(
                StageCapacity::new(SERIAL_WORKERS, EMPTY_ITEMS),
                Duration::ZERO,
                StageDeadlinePolicy::new(deadline, TEST_CLEANUP_GRACE),
            ),
            Err(NativeExtractionConfigError::Invalid {
                field: "item_timeout"
            })
        );
        assert_matches!(
            NativeExtractionStageConfig::new(
                StageCapacity::new(SERIAL_WORKERS, EMPTY_ITEMS),
                TEST_ITEM_TIMEOUT,
                StageDeadlinePolicy::new(Instant::now(), TEST_CLEANUP_GRACE),
            ),
            Err(NativeExtractionConfigError::Invalid {
                field: "stage_deadline"
            })
        );
        assert_matches!(
            NativeExtractionStageConfig::new(
                StageCapacity::new(SERIAL_WORKERS, EMPTY_ITEMS),
                TEST_ITEM_TIMEOUT,
                StageDeadlinePolicy::new(deadline, Duration::ZERO),
            ),
            Err(NativeExtractionConfigError::Invalid {
                field: "cleanup_grace"
            })
        );
    }

    #[tokio::test]
    async fn native_stage_rejects_current_thread_runtime_before_consuming_inputs() {
        let (runner, tasks, cancellation) = runner(TEST_MAX_TASKS, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let config = match NativeExtractionStageConfig::new(
            StageCapacity::new(SERIAL_WORKERS, EMPTY_ITEMS),
            TEST_ITEM_TIMEOUT,
            StageDeadlinePolicy::new(deadline, TEST_CLEANUP_GRACE),
        ) {
            Ok(config) => config,
            Err(error) => panic!("native extraction runtime test config failed: {error}"),
        };
        let request = NativeExtractionRequest::new(
            config,
            std::iter::empty::<SourceSnapshot>(),
            DigestObserver::new(StageMetrics::new()),
        );
        assert_matches!(
            Box::pin(run_native_extraction_stage(&runner, request))
                .await
                .err(),
            Some(NativeExtractionStageError::Runtime)
        );
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn native_extraction_is_ordered_and_identical_across_worker_counts() {
        let one = Box::pin(run(SERIAL_WORKERS)).await;
        let four = Box::pin(run(PARALLEL_WORKERS)).await;
        assert_eq!(one.0, four.0);
        assert_eq!(one.1.digest, four.1.digest);
        assert_eq!(one.1.files, four.1.files);
        assert_eq!(one.0.files(), EXPECTED_COMPLETED_ITEMS);
        assert_eq!(one.1.files, EXPECTED_COMPLETED_ITEMS);
        assert!(one.1.maximum_current_reserved >= one.0.maximum_file_output_bytes());
        assert!(four.1.maximum_current_reserved >= four.0.maximum_file_output_bytes());
        assert!(one.1.maximum_current_reserved <= TEST_SCOPE_BYTES);
        assert!(four.1.maximum_current_reserved <= TEST_SCOPE_BYTES);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_file_outputs_are_consumed_while_their_reservation_is_held() {
        let (report, output, metrics) =
            Box::pin(run_count(PARALLEL_WORKERS, MANY_FIXTURE_COUNT)).await;
        let snapshot = metric_snapshot(&metrics);

        assert_eq!(
            report.files(),
            u64::try_from(MANY_FIXTURE_COUNT).unwrap_or(u64::MAX)
        );
        assert_eq!(output.files, report.files());
        assert!(report.modeled_output_bytes() > report.maximum_file_output_bytes());
        assert!(output.maximum_current_reserved >= report.maximum_file_output_bytes());
        assert_eq!(snapshot.current_reserved_bytes(), EMPTY_BYTES);
        assert!(snapshot.peak_reserved_bytes() <= TEST_SCOPE_BYTES);
    }

    async fn run(workers: usize) -> (super::NativeExtractionReport, DigestOutput) {
        let (report, output, _) = Box::pin(run_count(workers, FIXTURE_COUNT)).await;
        (report, output)
    }

    async fn run_count(
        workers: usize,
        count: usize,
    ) -> (super::NativeExtractionReport, DigestOutput, StageMetrics) {
        let (runner, tasks, cancellation) = runner(TEST_MAX_TASKS, TEST_SCOPE_BYTES).await;
        let deadline = Instant::now() + TEST_TIMEOUT;
        let config = match NativeExtractionStageConfig::new(
            StageCapacity::new(workers, workers),
            TEST_ITEM_TIMEOUT,
            StageDeadlinePolicy::new(deadline, TEST_CLEANUP_GRACE),
        ) {
            Ok(config) => config,
            Err(error) => panic!("native extraction test config failed: {error}"),
        };
        let metrics = StageMetrics::new();
        let observer = DigestObserver::new(metrics.clone());
        let request = NativeExtractionRequest::new(config, snapshots(count), observer)
            .with_metrics(metrics.clone());
        let result = Box::pin(run_native_extraction_stage(&runner, request)).await;
        let completed = match result {
            Ok(completed) => completed,
            Err(error) => panic!("native extraction stage failed: {error}"),
        };
        let snapshot = metric_snapshot(&metrics);
        assert_eq!(
            snapshot.completed_items(),
            u64::try_from(count).unwrap_or(u64::MAX)
        );
        assert_eq!(snapshot.current_items(), EMPTY_ITEMS);
        assert_eq!(snapshot.current_reserved_bytes(), EMPTY_BYTES);
        assert!(snapshot.peak_items() <= workers.saturating_mul(WINDOW_MULTIPLIER));
        assert!(snapshot.peak_reserved_bytes() <= TEST_SCOPE_BYTES);
        drop(cancellation);
        let report = tasks.close_abort_and_reap(deadline).await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        let (report, observer) = completed.into_parts();
        (report, observer.into_output(), metrics)
    }

    fn snapshots(count: usize) -> impl Iterator<Item = SourceSnapshot> {
        (0..count).map(|index| {
            let (path, source) = if index % LANGUAGE_ALTERNATION == 0 {
                (format!("src/{index}.ts"), TYPESCRIPT_SOURCE)
            } else {
                (format!("src/{index}.js"), JAVASCRIPT_SOURCE)
            };
            let limits = match SourceLimits::new(TEST_SOURCE_LIMIT_BYTES) {
                Ok(limits) => limits,
                Err(error) => panic!("native extraction test limit failed: {error}"),
            };
            match SourceSnapshot::from_bytes(&path, source.as_bytes(), limits) {
                Ok(snapshot) => snapshot,
                Err(error) => panic!("native extraction test snapshot failed: {error}"),
            }
        })
    }

    fn metric_snapshot(metrics: &StageMetrics) -> crate::StageMetricsSnapshot {
        match metrics.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("native extraction metrics failed: {error}"),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DigestOutput {
        digest: [u8; 32],
        files: u64,
        maximum_current_reserved: u64,
    }

    struct DigestObserver {
        hasher: blake3::Hasher,
        files: u64,
        metrics: StageMetrics,
        maximum_current_reserved: u64,
    }

    impl DigestObserver {
        fn new(metrics: StageMetrics) -> Self {
            Self {
                hasher: blake3::Hasher::new(),
                files: 0,
                metrics,
                maximum_current_reserved: 0,
            }
        }

        fn into_output(self) -> DigestOutput {
            DigestOutput {
                digest: *self.hasher.finalize().as_bytes(),
                files: self.files,
                maximum_current_reserved: self.maximum_current_reserved,
            }
        }
    }

    impl NativeExtractionObserver for DigestObserver {
        fn observe(&mut self, file: &ExtractedFile) -> Result<(), NativeExtractionObserverError> {
            let current = self
                .metrics
                .snapshot()
                .map_err(|_| NativeExtractionObserverError)?
                .current_reserved_bytes();
            let output_bytes = file.modeled_retained_bytes();
            if current < output_bytes
                || output_bytes > native_output_limit(file.byte_size).unwrap_or(0)
            {
                return Err(NativeExtractionObserverError);
            }
            let path_index = file
                .path
                .as_str()
                .strip_prefix("src/")
                .and_then(|path| path.split_once('.'))
                .and_then(|(index, _)| index.parse::<u64>().ok())
                .ok_or(NativeExtractionObserverError)?;
            if path_index != self.files {
                return Err(NativeExtractionObserverError);
            }
            hash_file(&mut self.hasher, file);
            self.files = self
                .files
                .checked_add(1)
                .ok_or(NativeExtractionObserverError)?;
            self.maximum_current_reserved = self.maximum_current_reserved.max(current);
            Ok(())
        }
    }

    fn hash_file(hasher: &mut blake3::Hasher, file: &ExtractedFile) {
        hash_text(hasher, file.file_id.as_str());
        hash_text(hasher, file.path.as_str());
        hash_text(hasher, file.language.as_str());
        hash_text(hasher, file.content_hash.as_str());
        hash_number(hasher, file.byte_size);
        hash_text(hasher, file.parse_status.as_str());
        hash_number(
            hasher,
            u64::try_from(file.symbols.len()).unwrap_or(u64::MAX),
        );
        for symbol in &file.symbols {
            hash_text(hasher, symbol.id.as_str());
            hash_text(hasher, symbol.kind.as_str());
            hash_text(hasher, &symbol.name);
            hash_text(hasher, &symbol.qualified_name);
            hash_span(hasher, symbol.span);
            hash_optional_text(hasher, symbol.signature.as_deref());
            hash_optional_text(hasher, symbol.docstring.as_deref());
            hash_bool(hasher, symbol.export.exported);
            hash_bool(hasher, symbol.export.default_export);
            hash_bool(hasher, symbol.execution.async_symbol);
            hash_bool(hasher, symbol.execution.static_member);
            hash_optional_text(
                hasher,
                symbol.visibility.map(cartograph_domain::Visibility::as_str),
            );
            hash_text(hasher, symbol.structural_digest.as_str());
        }
        hash_number(
            hasher,
            u64::try_from(file.containments.len()).unwrap_or(u64::MAX),
        );
        for edge in &file.containments {
            hash_text(hasher, edge.parent.as_str());
            hash_text(hasher, edge.child.as_str());
        }
        hash_number(
            hasher,
            u64::try_from(file.references.len()).unwrap_or(u64::MAX),
        );
        for reference in &file.references {
            hash_optional_text(
                hasher,
                reference
                    .owner
                    .as_ref()
                    .map(cartograph_domain::SymbolId::as_str),
            );
            hash_text(hasher, &reference.name);
            hash_text(hasher, reference.kind.as_str());
            hash_span(hasher, reference.span);
        }
        hash_number(
            hasher,
            u64::try_from(file.diagnostics.len()).unwrap_or(u64::MAX),
        );
        for diagnostic in &file.diagnostics {
            let code = match diagnostic.code {
                cartograph_extract::DiagnosticCode::SyntaxError => "syntax_error",
                cartograph_extract::DiagnosticCode::InvalidSpan => "invalid_span",
                cartograph_extract::DiagnosticCode::ParserStopped => "parser_stopped",
                cartograph_extract::DiagnosticCode::CanonicalNameTruncated => {
                    "canonical_name_truncated"
                }
                cartograph_extract::DiagnosticCode::NestingLimitExceeded => {
                    "nesting_limit_exceeded"
                }
                cartograph_extract::DiagnosticCode::OptionalFactsOmitted => {
                    "optional_facts_omitted"
                }
            };
            hash_text(hasher, code);
            match diagnostic.span {
                Some(span) => {
                    hash_bool(hasher, true);
                    hash_span(hasher, span);
                }
                None => hash_bool(hasher, false),
            }
        }
    }

    fn hash_span(hasher: &mut blake3::Hasher, span: cartograph_domain::SourceSpan) {
        hash_number(hasher, span.start_byte());
        hash_number(hasher, span.end_byte());
        hash_number(hasher, u64::from(span.start_line()));
        hash_number(hasher, u64::from(span.end_line()));
        hash_number(hasher, u64::from(span.start_column()));
        hash_number(hasher, u64::from(span.end_column()));
    }

    fn hash_optional_text(hasher: &mut blake3::Hasher, value: Option<&str>) {
        match value {
            Some(value) => {
                hash_bool(hasher, true);
                hash_text(hasher, value);
            }
            None => hash_bool(hasher, false),
        }
    }

    fn hash_bool(hasher: &mut blake3::Hasher, value: bool) {
        hasher.update(&[u8::from(value)]);
    }

    fn hash_text(hasher: &mut blake3::Hasher, value: &str) {
        hash_number(hasher, u64::try_from(value.len()).unwrap_or(u64::MAX));
        hasher.update(value.as_bytes());
    }

    fn hash_number(hasher: &mut blake3::Hasher, value: u64) {
        hasher.update(&value.to_le_bytes());
    }
}
