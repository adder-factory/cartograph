//! Reproducible live benchmark for deterministic index-worker scaling.

#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;
#[path = "index_scaling/fixture.rs"]
mod fixture;

use std::{
    env, process,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CanonicalGenerationFacts, CartographDatabase, CurrentGeneration, CurrentGenerationLookup,
    EdgeInput, FileInput, GenerationContents, GenerationFacts, GenerationValidationLimits,
    LeaseOwner, LeaseTarget, MANAGED_DATABASE_IMAGE, NewGeneration, NewProject,
    PrepareGenerationMetrics, ReadyGeneration, ReferenceInput, SearchDocumentInput, SearchQuery,
    StagedGeneration, SymbolInput, probe_capabilities, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus,
    GenerationDigestVersion, GenerationId, ProjectId, ProjectOperation, SymbolExecutionFlags,
    SymbolExportFlags, SymbolId,
};
use cartograph_indexer::{
    IndexerSupervisor, PipelineFailure, PipelineStage, StageCapacity, StageDeadlinePolicy,
    StageEnvelope, StageExecution, StageFold, StageItemBudget, StageItemFailure, StageItemMeta,
    StageMetrics, StageMetricsSnapshot, StageOutput, StageRunConfig, StageSequence, StageWorkItem,
    StageWorkload, SupervisorContext, SupervisorRequest, SupervisorState, SupervisorStatus,
};
use serde::Serialize;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use fixture::{
    BENCHMARK_CONFIG, BenchmarkConfig, EXPECTED_BM25_DOCUMENT_ID, FixtureError, FixtureInput,
    FrozenFixture, RowCounts, SamplePlan, ValidationBudget, WorkloadSize, committed_row_counts,
    maximum_stage_reserved_bytes, qualified_name,
};

const DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";

const REDUCE_STAGE_ITEMS: usize = 1;
const REDUCE_STAGE_WORKERS: usize = 1;
const REDUCE_STAGE_QUEUE_ITEMS: usize = 0;
const REDUCE_STAGE_SEQUENCE: u64 = 0;
const REDUCE_STAGE_KEY: u8 = 0;
const REDUCE_STAGE_PROGRESS_BYTES: u64 = 0;
const MEDIAN_PERCENTILE: usize = 50;
const TAIL_PERCENTILE: usize = 95;
// V21 changes only this fixture's digest domain, not its source, facts, or ranking.
const EXPECTED_LOGICAL_DIGEST: &str =
    "0a7c4b156f643cd8216e41cea16b9b49976539eb4cf8e133e985989e6f63ff07";

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Error)]
enum BenchmarkError {
    #[error("{DATABASE_URL_ENV} must name an explicit PostgreSQL 18 + ParadeDB database")]
    MissingDatabase,
    #[error("index scaling benchmark failed during {operation}")]
    Operation { operation: &'static str },
    #[error("index scaling benchmark supervisor failed: {failure}")]
    Supervisor { failure: String },
    #[error("index scaling benchmark invariant failed: {name}")]
    Invariant { name: &'static str },
    #[error("index scaling benchmark logical digest changed to {actual}")]
    LogicalDigestChanged { actual: String },
    #[error("index scaling benchmark fixture fingerprint changed to {actual}")]
    FixtureFingerprintChanged { actual: String },
    #[error(
        "index scaling benchmark failed during {primary}; cleanup also failed during {cleanup}"
    )]
    Combined {
        primary: &'static str,
        cleanup: &'static str,
    },
}

impl From<FixtureError> for BenchmarkError {
    fn from(error: FixtureError) -> Self {
        match error {
            FixtureError::Invariant(name) => Self::Invariant { name },
            FixtureError::FingerprintChanged(actual) => Self::FixtureFingerprintChanged { actual },
        }
    }
}

type BenchmarkResult<T> = Result<T, BenchmarkError>;

struct FactBundle {
    file: FileInput,
    symbol: SymbolInput,
    edge: Option<EdgeInput>,
    reference: Option<ReferenceInput>,
    document: SearchDocumentInput,
}

struct DatabaseFixture {
    database: CartographDatabase,
    pool: sqlx_postgres::PgPool,
    schema: String,
    project: ProjectId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InvariantFingerprint {
    logical_digest: String,
    logical_digest_version: i16,
    rows: RowCounts,
    bm25_document_ids: Vec<String>,
}

struct SampleObservation {
    sample: usize,
    fingerprint: InvariantFingerprint,
    stage_nanos: u64,
    copy_nanos: u64,
    end_to_end_nanos: u64,
    peak_items: usize,
    peak_reserved_bytes: u64,
}

#[derive(Clone, Copy)]
struct SampleCoordinates {
    workers: u16,
    sample: usize,
}

struct CleanSampleRequest<'a> {
    database_url: &'a str,
    fixture: &'a FrozenFixture,
    coordinates: SampleCoordinates,
}

#[derive(Clone, Copy)]
struct ExecutionRequest<'a> {
    database: &'a DatabaseFixture,
    fixture: &'a FrozenFixture,
    coordinates: SampleCoordinates,
}

struct CompletedSample {
    current: CurrentGeneration,
    generation_id: GenerationId,
    target: LeaseTarget,
    supervisor: IndexerSupervisor,
    stage_metrics: StageMetrics,
    prepare_metrics: PrepareGenerationMetrics,
    stage_nanos: u64,
    end_to_end_nanos: u64,
    window: usize,
    maximum_reserved_bytes: u64,
}

struct PipelineWork {
    config: BenchmarkConfig,
    staged: StagedGeneration,
    inputs: Vec<StageEnvelope<String, FixtureInput>>,
    worker_count: usize,
    queue_items: usize,
    stage_deadline: tokio::time::Instant,
    stage_metrics: StageMetrics,
    prepare_metrics: PrepareGenerationMetrics,
    stage_elapsed: Arc<AtomicU64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    Normal,
    RejectProject,
}

#[derive(Serialize)]
struct BenchmarkReport {
    environment: EnvironmentReport,
    fixture: FixtureReport,
    invariant: InvariantReport,
    workers: Vec<WorkerReport>,
}

#[derive(Serialize)]
struct EnvironmentReport {
    architecture: &'static str,
    logical_cpus: usize,
    rust_toolchain: &'static str,
    postgres_version_num: i32,
    postgres_version: String,
    pg_search_version: String,
    pgvector_version: String,
    database_image: &'static str,
}

#[derive(Serialize)]
struct FixtureReport {
    name: &'static str,
    items: usize,
    source_bytes: u64,
    source_digest: String,
    fixture_fingerprint: String,
    source_repetitions: usize,
    hash_rounds: usize,
    warmup_samples: usize,
    measured_samples: usize,
    expected_rows: RowCounts,
    bm25_query: &'static str,
}

#[derive(Serialize)]
struct InvariantReport {
    logical_digest: String,
    logical_digest_version: i16,
    row_counts: RowCounts,
    bm25_document_ids: Vec<String>,
    identical_at_workers: Vec<u16>,
    supervisor_requires_zero_active_tasks_before_publication: bool,
    exact_lease_release_verified_each_sample: bool,
    setup_failure_cleanup_verified: bool,
}

#[derive(Serialize)]
struct WorkerReport {
    workers: u16,
    queue_items: usize,
    bounded_window_items: usize,
    maximum_reserved_bytes: u64,
    stage_p50_ms: f64,
    stage_p95_ms: f64,
    copy_p50_ms: f64,
    copy_p95_ms: f64,
    end_to_end_p50_ms: f64,
    end_to_end_p95_ms: f64,
    stage_p50_items_per_second: f64,
    end_to_end_p50_items_per_second: f64,
    observed_peak_items: usize,
    observed_peak_reserved_bytes: u64,
    samples: Vec<SampleReport>,
}

#[derive(Serialize)]
struct SampleReport {
    sample: usize,
    stage_ms: f64,
    copy_ms: f64,
    end_to_end_ms: f64,
    peak_items: usize,
    peak_reserved_bytes: u64,
}

#[tokio::main]
async fn main() -> BenchmarkResult<()> {
    let database_url = env::var(DATABASE_URL_ENV).map_err(|_| BenchmarkError::MissingDatabase)?;
    let fixture = FrozenFixture::build(&BENCHMARK_CONFIG)?;
    let environment = inspect_environment(&database_url).await?;
    verify_setup_failure_cleanup(&database_url).await?;
    let report = run_matrix(&database_url, &fixture, environment).await?;
    let output = serde_json::to_string_pretty(&report)
        .map_err(|_| operation("serialize-benchmark-report"))?;
    println!("{output}");
    Ok(())
}

async fn run_matrix(
    database_url: &str,
    fixture: &FrozenFixture,
    environment: EnvironmentReport,
) -> BenchmarkResult<BenchmarkReport> {
    let config = &fixture.config;
    let baseline = committed_invariant_fingerprint();
    let mut worker_reports = Vec::with_capacity(config.workers.len());
    for workers in config.workers {
        for sample in 0..config.workload[WorkloadSize::WarmupSamples as usize] {
            let observation = run_clean_sample(CleanSampleRequest {
                database_url,
                fixture,
                coordinates: SampleCoordinates { workers, sample },
            })
            .await?;
            validate_fingerprint(&observation.fingerprint)?;
        }
        let mut observations =
            Vec::with_capacity(config.workload[WorkloadSize::MeasuredSamples as usize]);
        for sample in 0..config.workload[WorkloadSize::MeasuredSamples as usize] {
            let observation = run_clean_sample(CleanSampleRequest {
                database_url,
                fixture,
                coordinates: SampleCoordinates { workers, sample },
            })
            .await?;
            validate_fingerprint(&observation.fingerprint)?;
            observations.push(observation);
        }
        worker_reports.push(summarize_workers(workers, fixture, observations)?);
    }
    Ok(BenchmarkReport {
        environment,
        fixture: FixtureReport {
            name: config.fixture_name,
            items: config.workload[WorkloadSize::Items as usize],
            source_bytes: fixture.source_bytes,
            source_digest: fixture.source_digest.clone(),
            fixture_fingerprint: fixture.fixture_fingerprint.clone(),
            source_repetitions: config.workload[WorkloadSize::SourceRepetitions as usize],
            hash_rounds: config.workload[WorkloadSize::HashRounds as usize],
            warmup_samples: config.workload[WorkloadSize::WarmupSamples as usize],
            measured_samples: config.workload[WorkloadSize::MeasuredSamples as usize],
            expected_rows: fixture.expected_rows.clone(),
            bm25_query: config.bm25_query,
        },
        invariant: InvariantReport {
            logical_digest: baseline.logical_digest,
            logical_digest_version: baseline.logical_digest_version,
            row_counts: baseline.rows,
            bm25_document_ids: baseline.bm25_document_ids,
            identical_at_workers: config.workers.to_vec(),
            supervisor_requires_zero_active_tasks_before_publication: true,
            exact_lease_release_verified_each_sample: true,
            setup_failure_cleanup_verified: true,
        },
        workers: worker_reports,
    })
}

async fn inspect_environment(database_url: &str) -> BenchmarkResult<EnvironmentReport> {
    let settings = DatabaseSettings::parse(database_url, Some("4"), Some("30000"))
        .map_err(|_| operation("environment-database-settings"))?;
    let pool = cartograph_db::connect(&settings)
        .await
        .map_err(|_| operation("environment-database-connect"))?;
    let capabilities = probe_capabilities(&pool)
        .await
        .map_err(|_| operation("environment-capability-probe"))?;
    pool.close().await;
    require(capabilities.ready, "benchmark-database-capabilities")?;
    Ok(EnvironmentReport {
        architecture: std::env::consts::ARCH,
        logical_cpus: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        rust_toolchain: env!("CARGO_PKG_RUST_VERSION"),
        postgres_version_num: capabilities.postgres_version_num,
        postgres_version: capabilities.postgres_version,
        pg_search_version: capabilities
            .pg_search_version
            .ok_or_else(|| invariant("pg-search-version"))?,
        pgvector_version: capabilities
            .pgvector_version
            .ok_or_else(|| invariant("pgvector-version"))?,
        database_image: MANAGED_DATABASE_IMAGE,
    })
}

async fn verify_setup_failure_cleanup(database_url: &str) -> BenchmarkResult<()> {
    let schema = next_schema();
    let setup =
        DatabaseFixture::open_schema(database_url, schema.clone(), SetupMode::RejectProject).await;
    let expected_failure = match setup {
        Err(BenchmarkError::Operation {
            operation: "register-project",
        }) => true,
        Err(_) => false,
        Ok(fixture) => {
            fixture.close().await?;
            false
        }
    };
    let settings = DatabaseSettings::parse(database_url, Some("2"), Some("30000"))
        .map_err(|_| operation("cleanup-probe-settings"))?;
    let pool = cartograph_db::connect(&settings)
        .await
        .map_err(|_| operation("cleanup-probe-connect"))?;
    let exists =
        query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
            .bind(&schema)
            .fetch_one(&pool)
            .await
            .map_err(|_| operation("cleanup-probe-query"));
    pool.close().await;
    require(expected_failure, "injected-setup-failure")?;
    require(!exists?, "setup-failure-schema-cleanup")
}

async fn run_clean_sample(request: CleanSampleRequest<'_>) -> BenchmarkResult<SampleObservation> {
    let database = DatabaseFixture::open(request.database_url).await?;
    let result = execute_sample(ExecutionRequest {
        database: &database,
        fixture: request.fixture,
        coordinates: request.coordinates,
    })
    .await;
    let cleanup = database.close().await;
    match (result, cleanup) {
        (Ok(observation), Ok(())) => Ok(observation),
        (Err(primary), Err(cleanup)) => Err(combine(&primary, &cleanup)),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn execute_sample(request: ExecutionRequest<'_>) -> BenchmarkResult<SampleObservation> {
    let plan = SamplePlan::build(request.fixture, request.coordinates.workers)?;
    let completed = run_supervised_pipeline(request, plan).await?;
    verify_completed_sample(request, completed).await
}

async fn run_supervised_pipeline(
    request: ExecutionRequest<'_>,
    plan: SamplePlan,
) -> BenchmarkResult<CompletedSample> {
    let SamplePlan {
        worker_count,
        queue_items,
        window,
        maximum_reserved_bytes,
        stage_deadline,
        inputs,
    } = plan;
    let database = request.database;
    let config = request.fixture.config;
    let workers = request.coordinates.workers;
    let stage_metrics = StageMetrics::new();
    let prepare_metrics = PrepareGenerationMetrics::new();
    let stage_elapsed = Arc::new(AtomicU64::new(0));
    let total_started = Instant::now();
    let staged = database
        .database
        .begin_generation(NewGeneration::new(
            database.project.clone(),
            config.source_revision,
            workers,
        ))
        .await
        .map_err(|_| operation("begin-generation"))?;
    let generation_id = staged.generation_id().clone();
    let target = LeaseTarget::new(
        database.project.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    let supervisor = IndexerSupervisor::new(
        database.database.clone(),
        config.supervisor_config(window, maximum_reserved_bytes),
    );
    let request = SupervisorRequest::new(
        target.clone(),
        LeaseOwner::new(
            process::id(),
            format!("scaling-{workers}-{}", request.coordinates.sample),
        ),
        config.deadlines.lease,
    );
    let work = PipelineWork {
        config,
        staged,
        inputs,
        worker_count,
        queue_items,
        stage_deadline,
        stage_metrics: stage_metrics.clone(),
        prepare_metrics: prepare_metrics.clone(),
        stage_elapsed: stage_elapsed.clone(),
    };
    let current = supervisor
        .run(request, move |context| run_pipeline_work(context, work))
        .await
        .map_err(|failure| BenchmarkError::Supervisor {
            failure: failure.to_string(),
        })?;
    Ok(CompletedSample {
        current,
        generation_id,
        target,
        supervisor,
        stage_metrics,
        prepare_metrics,
        stage_nanos: stage_elapsed.load(Ordering::Relaxed),
        end_to_end_nanos: duration_nanos(total_started.elapsed()),
        window,
        maximum_reserved_bytes,
    })
}

async fn run_pipeline_work(
    context: SupervisorContext,
    work: PipelineWork,
) -> Result<ReadyGeneration, PipelineFailure> {
    let stage_started = Instant::now();
    let facts =
        context
            .stages()
            .execute(
                StageExecution::new(
                    StageRunConfig::new(
                        PipelineStage::Parse,
                        StageCapacity::new(work.worker_count, work.queue_items),
                        StageDeadlinePolicy::new(
                            work.stage_deadline,
                            work.config.deadlines.cleanup_grace,
                        ),
                    ),
                    StageWorkload::new(
                        work.inputs,
                        |item: StageWorkItem<String, FixtureInput>| async move {
                            build_fact_bundle(item)
                        },
                    ),
                    StageFold::new(GenerationFacts::default(), reduce_fact_bundle),
                )
                .with_metrics(work.stage_metrics.clone()),
            )
            .await
            .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
    let validation_limits = GenerationValidationLimits::new(
        work.config.validation_bytes[ValidationBudget::Output as usize],
        work.config.validation_bytes[ValidationBudget::Working as usize],
    )
    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
    let facts = run_supervised_reduce(
        &context,
        ReduceStageRequest {
            config: work.config,
            facts,
            stage_deadline: work.stage_deadline,
            validation_limits,
            metrics: work.stage_metrics,
        },
    )
    .await?;
    work.stage_elapsed
        .store(duration_nanos(stage_started.elapsed()), Ordering::Relaxed);
    context
        .progress()
        .begin_stage(PipelineStage::Copy)
        .await
        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
    context
        .prepare_generation(
            GenerationContents::new(work.staged, facts).with_metrics(work.prepare_metrics),
        )
        .await
        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
}

struct ReduceStageRequest {
    config: BenchmarkConfig,
    facts: GenerationFacts,
    stage_deadline: tokio::time::Instant,
    validation_limits: GenerationValidationLimits,
    metrics: StageMetrics,
}

/// The single reduce work item: all generation facts under the per-item
/// deadline, capped by the stage deadline, and the validation working budget.
fn reduce_stage_input(
    facts: GenerationFacts,
    stage_deadline: tokio::time::Instant,
    config: &BenchmarkConfig,
) -> StageEnvelope<u8, GenerationFacts> {
    let item_deadline = (tokio::time::Instant::now() + config.deadlines.item).min(stage_deadline);
    StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(REDUCE_STAGE_SEQUENCE),
            REDUCE_STAGE_KEY,
            StageItemBudget::new(
                config.validation_bytes[ValidationBudget::Working as usize],
                REDUCE_STAGE_PROGRESS_BYTES,
                item_deadline,
            ),
        ),
        facts,
    )
}

/// One-worker, unqueued reduce stage bounded by the stage deadline and cleanup grace.
fn reduce_stage_config(
    stage_deadline: tokio::time::Instant,
    config: &BenchmarkConfig,
) -> StageRunConfig {
    StageRunConfig::new(
        PipelineStage::Reduce,
        StageCapacity::new(REDUCE_STAGE_WORKERS, REDUCE_STAGE_QUEUE_ITEMS),
        StageDeadlinePolicy::new(stage_deadline, config.deadlines.cleanup_grace),
    )
}

async fn run_supervised_reduce(
    context: &SupervisorContext,
    request: ReduceStageRequest,
) -> Result<CanonicalGenerationFacts, PipelineFailure> {
    let inputs = [reduce_stage_input(
        request.facts,
        request.stage_deadline,
        &request.config,
    )];
    let validation_limits = request.validation_limits;
    context
        .stages()
        .execute(
            StageExecution::new(
                reduce_stage_config(request.stage_deadline, &request.config),
                StageWorkload::new(
                    inputs,
                    move |item: StageWorkItem<u8, GenerationFacts>| async move {
                        let cancellation = item.cancellation();
                        let (_, _, facts) = item.into_parts();
                        tokio::task::block_in_place(move || {
                            validate_generation_facts(facts, validation_limits, || {
                                cancellation.is_cancelled()
                            })
                            .map(|(facts, _)| facts)
                            .map_err(|_| StageItemFailure)
                        })
                    },
                ),
                StageFold::new(
                    None,
                    |reduced: &mut Option<CanonicalGenerationFacts>,
                     output: StageOutput<u8, CanonicalGenerationFacts>| {
                        let (_, facts) = output.into_parts();
                        *reduced = Some(facts);
                        Ok(())
                    },
                ),
            )
            .with_metrics(request.metrics),
        )
        .await
        .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?
        .ok_or_else(|| PipelineFailure::new(PipelineStage::Reduce))
}

async fn verify_completed_sample(
    request: ExecutionRequest<'_>,
    completed: CompletedSample,
) -> BenchmarkResult<SampleObservation> {
    let status = completed.supervisor.status().await;
    validate_supervisor_status(&status, request.fixture)?;
    let stage_snapshot = completed
        .stage_metrics
        .snapshot()
        .map_err(|_| operation("stage-metrics-snapshot"))?;
    validate_stage_snapshot(
        stage_snapshot,
        StageSnapshotBounds {
            expected_items: request.fixture.inputs.len() + REDUCE_STAGE_ITEMS,
            window: completed.window,
            maximum_reserved_bytes: completed.maximum_reserved_bytes,
        },
    )?;
    let copy_nanos = duration_nanos(completed.prepare_metrics.snapshot().copy_duration());
    require(copy_nanos > 0, "copy-duration-observed")?;
    require(completed.stage_nanos > 0, "stage-duration-observed")?;
    require(
        request
            .database
            .database
            .lease_status(&completed.target)
            .await
            .map_err(|_| operation("lease-status"))?
            .is_none(),
        "exact-lease-released",
    )?;
    let rows = request
        .database
        .row_counts(&request.database.project, &completed.generation_id)
        .await?;
    require(
        rows == request.fixture.expected_rows,
        "published-row-counts",
    )?;
    let bm25_document_ids =
        bm25_fingerprint(request.database, request.fixture, &completed.current).await?;
    Ok(SampleObservation {
        sample: request.coordinates.sample,
        fingerprint: InvariantFingerprint {
            logical_digest: completed.current.content_digest().as_str().to_owned(),
            logical_digest_version: completed.current.digest_version().database_value(),
            rows,
            bm25_document_ids,
        },
        stage_nanos: completed.stage_nanos,
        copy_nanos,
        end_to_end_nanos: completed.end_to_end_nanos,
        peak_items: stage_snapshot.peak_items(),
        peak_reserved_bytes: stage_snapshot.peak_reserved_bytes(),
    })
}

fn validate_supervisor_status(
    status: &SupervisorStatus,
    fixture: &FrozenFixture,
) -> BenchmarkResult<()> {
    require(
        status.state() == SupervisorState::Completed,
        "supervisor-completed",
    )?;
    require(
        status.completed_items()
            == u64::try_from(fixture.inputs.len() + REDUCE_STAGE_ITEMS)
                .map_err(|_| invariant("completed-items"))?,
        "supervisor-completed-items",
    )?;
    require(
        status.completed_bytes() == fixture.source_bytes,
        "supervisor-completed-bytes",
    )
}

#[derive(Clone, Copy)]
struct StageSnapshotBounds {
    expected_items: usize,
    window: usize,
    maximum_reserved_bytes: u64,
}

fn validate_stage_snapshot(
    stage_snapshot: StageMetricsSnapshot,
    bounds: StageSnapshotBounds,
) -> BenchmarkResult<()> {
    require(
        stage_snapshot.admitted_items()
            == u64::try_from(bounds.expected_items).map_err(|_| invariant("admitted-items"))?,
        "all-items-admitted",
    )?;
    require(
        stage_snapshot.completed_items() == stage_snapshot.admitted_items(),
        "all-items-reduced",
    )?;
    require(stage_snapshot.current_items() == 0, "zero-current-items")?;
    require(
        stage_snapshot.current_reserved_bytes() == 0,
        "zero-current-reserved-bytes",
    )?;
    require(
        stage_snapshot.peak_items() <= bounds.window,
        "peak-items-within-window",
    )?;
    require(
        stage_snapshot.peak_reserved_bytes() <= bounds.maximum_reserved_bytes,
        "peak-bytes-within-cap",
    )
}

async fn bm25_fingerprint(
    database: &DatabaseFixture,
    fixture: &FrozenFixture,
    current: &CurrentGeneration,
) -> BenchmarkResult<Vec<String>> {
    let hits = database
        .database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(&database.project, current.generation_id()),
            fixture.config.bm25_query,
            5,
        ))
        .await
        .map_err(|_| operation("bm25-query"))?;
    let bm25_document_ids = hits
        .iter()
        .map(|hit| hit.document_id().as_str().to_owned())
        .collect::<Vec<_>>();
    require(
        bm25_document_ids.first().map(String::as_str) == Some(fixture.needle_document_id.as_str()),
        "bm25-first-hit",
    )?;
    require(
        hits.iter()
            .all(|hit| hit.generation_id() == current.generation_id()),
        "bm25-current-generation-only",
    )?;
    Ok(bm25_document_ids)
}

impl DatabaseFixture {
    async fn open(database_url: &str) -> BenchmarkResult<Self> {
        Self::open_schema(database_url, next_schema(), SetupMode::Normal).await
    }

    async fn open_schema(
        database_url: &str,
        schema: String,
        mode: SetupMode,
    ) -> BenchmarkResult<Self> {
        let settings = DatabaseSettings::parse(database_url, Some("32"), Some("30000"))
            .and_then(|settings| settings.with_schema(&schema))
            .map_err(|_| operation("database-settings"))?;
        let pool = cartograph_db::connect(&settings)
            .await
            .map_err(|_| operation("database-connect"))?;
        let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
        let setup = setup_database(&database, &schema, mode).await;
        match setup {
            Ok(project) => Ok(Self {
                database,
                pool,
                schema,
                project,
            }),
            Err(primary) => {
                drop(database);
                let cleanup = drop_benchmark_schema(&pool, &schema).await;
                pool.close().await;
                match cleanup {
                    Ok(()) => Err(primary),
                    Err(cleanup) => Err(combine(&primary, &cleanup)),
                }
            }
        }
    }

    async fn row_counts(
        &self,
        project: &ProjectId,
        generation: &GenerationId,
    ) -> BenchmarkResult<RowCounts> {
        let sql = format!(
            r#"SELECT
                (SELECT count(*) FROM "{schema}"."files"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS files,
                (SELECT count(*) FROM "{schema}"."symbols"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS symbols,
                (SELECT count(*) FROM "{schema}"."edges"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS edges,
                (SELECT count(*) FROM "{schema}"."references"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS references,
                (SELECT count(*) FROM "{schema}"."numerical_sites"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS numerical_sites,
                (SELECT count(*) FROM "{schema}"."search_documents"
                  WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS documents"#,
            schema = self.schema
        );
        let row = query(AssertSqlSafe(sql))
            .bind(project.as_str())
            .bind(generation.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| operation("row-count-query"))?;
        Ok(RowCounts {
            files: row
                .try_get("files")
                .map_err(|_| operation("decode-row-counts"))?,
            symbols: row
                .try_get("symbols")
                .map_err(|_| operation("decode-row-counts"))?,
            edges: row
                .try_get("edges")
                .map_err(|_| operation("decode-row-counts"))?,
            references: row
                .try_get("references")
                .map_err(|_| operation("decode-row-counts"))?,
            numerical_sites: row
                .try_get("numerical_sites")
                .map_err(|_| operation("decode-row-counts"))?,
            documents: row
                .try_get("documents")
                .map_err(|_| operation("decode-row-counts"))?,
        })
    }

    async fn close(self) -> BenchmarkResult<()> {
        drop(self.database);
        let cleanup = drop_benchmark_schema(&self.pool, &self.schema).await;
        self.pool.close().await;
        cleanup
    }
}

async fn setup_database(
    database: &CartographDatabase,
    schema: &str,
    mode: SetupMode,
) -> BenchmarkResult<ProjectId> {
    database
        .migrate()
        .await
        .map_err(|_| operation("database-migrate"))?;
    let root_identity = if mode == SetupMode::RejectProject {
        String::new()
    } else {
        format!("benchmark/{schema}")
    };
    database
        .register_project(NewProject::new(
            root_identity,
            ContentDigest::from_bytes([0x42; 32]),
        ))
        .await
        .map_err(|_| operation("register-project"))
}

async fn drop_benchmark_schema(pool: &sqlx_postgres::PgPool, schema: &str) -> BenchmarkResult<()> {
    let statement = format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#);
    query(AssertSqlSafe(statement))
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|_| operation("drop-benchmark-schema"))
}

fn build_fact_bundle(
    item: StageWorkItem<String, FixtureInput>,
) -> Result<FactBundle, StageItemFailure> {
    let (_, path, input) = item.into_parts();
    let index = input.index;
    let name = qualified_name(index);
    let signature = format!("function {name}(input: number): number");
    let content_hash = blake3::hash(input.source.as_bytes());
    let mut structural_hash = content_hash;
    for round in 0..input.hash_rounds {
        let mut hasher = blake3::Hasher::new();
        hasher.update(structural_hash.as_bytes());
        hasher.update(input.source.as_bytes());
        hasher.update(&round.to_le_bytes());
        structural_hash = hasher.finalize();
    }
    let file = file_id(index)?;
    let symbol = symbol_id(index)?;
    let source_bytes = u64::try_from(input.source.len()).map_err(|_| StageItemFailure)?;
    let end_line = u32::try_from(input.source_repetitions).map_err(|_| StageItemFailure)?;
    let previous_symbol = index.checked_sub(1).map(symbol_id).transpose()?;
    Ok(FactBundle {
        file: FileInput {
            file_id: file.clone(),
            normalized_path: path.clone(),
            language: "typescript".to_owned(),
            content_hash: ContentDigest::from_bytes(*content_hash.as_bytes()),
            byte_size: source_bytes,
            parse_status: FileParseStatus::Parsed,
        },
        symbol: SymbolInput {
            symbol_id: symbol.clone(),
            file_id: file.clone(),
            symbol_kind: "function".to_owned(),
            qualified_name: name.clone(),
            signature: signature.clone(),
            start_byte: 0,
            end_byte: source_bytes,
            start_line: 1,
            end_line,
            structural_digest: ContentDigest::from_bytes(*structural_hash.as_bytes()),
            visibility: None,
            export: SymbolExportFlags::named(true),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        },
        edge: previous_symbol.clone().map(|target_symbol_id| EdgeInput {
            source_symbol_id: symbol.clone(),
            target_symbol_id,
            kind: EdgeKind::Calls,
            confidence: 1.0,
            provenance: "rust-scaling-fixture".to_owned(),
            site_count: 1,
        }),
        reference: previous_symbol.map(|target_symbol_id| ReferenceInput {
            file_id: file.clone(),
            owner_symbol_id: Some(symbol.clone()),
            target_symbol_id: Some(target_symbol_id),
            reference_name: "previous_benchmark_symbol".to_owned(),
            reference_kind: "call".to_owned(),
            start_byte: 0,
            end_byte: u64::try_from(signature.len()).unwrap_or(u64::MAX),
            confidence: 1.0,
            resolution_provenance: "rust-scaling-fixture".to_owned(),
            site_count: 1,
            span_precision: cartograph_db::ReferenceSpanPrecision::Exact,
        }),
        document: SearchDocumentInput {
            document_id: document_id(index).map_err(|_| StageItemFailure)?,
            file_id: Some(file),
            symbol_id: Some(symbol),
            path,
            language: "typescript".to_owned(),
            kind: DocumentKind::Symbol,
            qualified_name: name,
            code: input.source,
            natural_text: natural_text(index),
            metadata: serde_json::json!({
                "fixture": input.fixture_name,
                "index": index,
            }),
        },
    })
}

fn reduce_fact_bundle(
    facts: &mut GenerationFacts,
    output: StageOutput<String, FactBundle>,
) -> Result<(), StageItemFailure> {
    let (_, bundle) = output.into_parts();
    facts
        .files
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    facts
        .symbols
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    facts
        .edges
        .try_reserve_exact(usize::from(bundle.edge.is_some()))
        .map_err(|_| StageItemFailure)?;
    facts
        .references
        .try_reserve_exact(usize::from(bundle.reference.is_some()))
        .map_err(|_| StageItemFailure)?;
    facts
        .documents
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    facts.files.push(bundle.file);
    facts.symbols.push(bundle.symbol);
    if let Some(edge) = bundle.edge {
        facts.edges.push(edge);
    }
    if let Some(reference) = bundle.reference {
        facts.references.push(reference);
    }
    facts.documents.push(bundle.document);
    Ok(())
}

fn summarize_workers(
    workers: u16,
    fixture: &FrozenFixture,
    observations: Vec<SampleObservation>,
) -> BenchmarkResult<WorkerReport> {
    let stage = observations
        .iter()
        .map(|sample| sample.stage_nanos)
        .collect::<Vec<_>>();
    let copy = observations
        .iter()
        .map(|sample| sample.copy_nanos)
        .collect::<Vec<_>>();
    let total = observations
        .iter()
        .map(|sample| sample.end_to_end_nanos)
        .collect::<Vec<_>>();
    let stage_p50 = percentile(&stage, MEDIAN_PERCENTILE)?;
    let total_p50 = percentile(&total, MEDIAN_PERCENTILE)?;
    let worker_count = usize::from(workers);
    let window = worker_count
        .checked_mul(2)
        .ok_or_else(|| invariant("report-window"))?;
    Ok(WorkerReport {
        workers,
        queue_items: worker_count,
        bounded_window_items: window,
        maximum_reserved_bytes: maximum_stage_reserved_bytes(fixture, window)?,
        stage_p50_ms: nanos_to_millis(stage_p50),
        stage_p95_ms: nanos_to_millis(percentile(&stage, TAIL_PERCENTILE)?),
        copy_p50_ms: nanos_to_millis(percentile(&copy, MEDIAN_PERCENTILE)?),
        copy_p95_ms: nanos_to_millis(percentile(&copy, TAIL_PERCENTILE)?),
        end_to_end_p50_ms: nanos_to_millis(total_p50),
        end_to_end_p95_ms: nanos_to_millis(percentile(&total, TAIL_PERCENTILE)?),
        stage_p50_items_per_second: throughput(stage_p50, fixture.inputs.len()),
        end_to_end_p50_items_per_second: throughput(total_p50, fixture.inputs.len()),
        observed_peak_items: observations
            .iter()
            .map(|sample| sample.peak_items)
            .max()
            .unwrap_or(0),
        observed_peak_reserved_bytes: observations
            .iter()
            .map(|sample| sample.peak_reserved_bytes)
            .max()
            .unwrap_or(0),
        samples: observations
            .into_iter()
            .map(|sample| SampleReport {
                sample: sample.sample,
                stage_ms: nanos_to_millis(sample.stage_nanos),
                copy_ms: nanos_to_millis(sample.copy_nanos),
                end_to_end_ms: nanos_to_millis(sample.end_to_end_nanos),
                peak_items: sample.peak_items,
                peak_reserved_bytes: sample.peak_reserved_bytes,
            })
            .collect(),
    })
}

fn validate_fingerprint(actual: &InvariantFingerprint) -> BenchmarkResult<()> {
    if actual.logical_digest != EXPECTED_LOGICAL_DIGEST {
        return Err(BenchmarkError::LogicalDigestChanged {
            actual: actual.logical_digest.clone(),
        });
    }
    require(
        actual.logical_digest_version == GenerationDigestVersion::CURRENT.database_value(),
        "logical-digest-version",
    )?;
    require(
        actual == &committed_invariant_fingerprint(),
        "committed-logical-output",
    )
}

fn committed_invariant_fingerprint() -> InvariantFingerprint {
    InvariantFingerprint {
        logical_digest: EXPECTED_LOGICAL_DIGEST.to_owned(),
        logical_digest_version: GenerationDigestVersion::CURRENT.database_value(),
        rows: committed_row_counts(),
        bm25_document_ids: vec![EXPECTED_BM25_DOCUMENT_ID.to_owned()],
    }
}

fn percentile(values: &[u64], percentile: usize) -> BenchmarkResult<u64> {
    require(!values.is_empty(), "nonempty-percentile-samples")?;
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    let rank = ordered
        .len()
        .checked_mul(percentile)
        .ok_or_else(|| invariant("percentile-rank"))?
        .div_ceil(100)
        .saturating_sub(1);
    ordered
        .get(rank)
        .copied()
        .ok_or_else(|| invariant("percentile-rank"))
}

fn natural_text(index: usize) -> String {
    if index == 0 {
        "unique needle cartograph benchmark evidence".to_owned()
    } else {
        format!("deterministic scaling fixture symbol {index:04}")
    }
}

fn file_id(index: usize) -> Result<FileId, StageItemFailure> {
    FileId::parse(&stable_uuid(0x1000_0000, index)).map_err(|_| StageItemFailure)
}

fn symbol_id(index: usize) -> Result<SymbolId, StageItemFailure> {
    SymbolId::parse(&stable_uuid(0x2000_0000, index)).map_err(|_| StageItemFailure)
}

fn document_id(index: usize) -> BenchmarkResult<DocumentId> {
    DocumentId::parse(&stable_uuid(0x3000_0000, index))
        .map_err(|_| invariant("fixture-document-id"))
}

fn stable_uuid(namespace: u32, index: usize) -> String {
    let value = index.saturating_add(1);
    format!("{namespace:08x}-0000-4000-8000-{value:012x}")
}

fn next_schema() -> String {
    format!(
        "cartograph_scaling_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn nanos_to_millis(nanos: u64) -> f64 {
    Duration::from_nanos(nanos).as_secs_f64() * 1_000.0
}

fn throughput(nanos: u64, item_count: usize) -> f64 {
    if nanos == 0 {
        0.0
    } else {
        let items = f64::from(u32::try_from(item_count).unwrap_or(u32::MAX));
        items / Duration::from_nanos(nanos).as_secs_f64()
    }
}

fn require(condition: bool, name: &'static str) -> BenchmarkResult<()> {
    if condition {
        Ok(())
    } else {
        Err(invariant(name))
    }
}

impl BenchmarkError {
    const fn code(&self) -> &'static str {
        match self {
            Self::MissingDatabase => "database-url",
            Self::Operation { operation } => operation,
            Self::Supervisor { .. } => "supervised-index-publication",
            Self::Invariant { name } => name,
            Self::LogicalDigestChanged { .. } => "logical-digest-changed",
            Self::FixtureFingerprintChanged { .. } => "fixture-fingerprint-changed",
            Self::Combined { .. } => "combined-failure",
        }
    }
}

fn combine(primary: &BenchmarkError, cleanup: &BenchmarkError) -> BenchmarkError {
    BenchmarkError::Combined {
        primary: primary.code(),
        cleanup: cleanup.code(),
    }
}

const fn invariant(name: &'static str) -> BenchmarkError {
    BenchmarkError::Invariant { name }
}

const fn operation(operation: &'static str) -> BenchmarkError {
    BenchmarkError::Operation { operation }
}
