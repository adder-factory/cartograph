use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use cartograph_domain::{
    ContentDigest, GenerationDigestVersion, GenerationId, GenerationState, ProjectId,
};
use sqlx_core::{query::query, row::Row};
use sqlx_postgres::{PgConnection, PgRow};
use thiserror::Error;

use crate::{
    CartographDatabase, CurrentGenerationLookup, LeaseFence, StorageError,
    database::audited_query,
    ingest::{
        CanonicalGenerationFacts, CopyGenerationAttempt, CopyGenerationContext,
        CopyGenerationRequest, CopyTableDurations, copy_generation_facts,
    },
    leases::project_lock_key,
    search_relation::{
        GenerationSearchBuild, rebuild_generation_search_relation,
        require_generation_search_relation,
    },
    spill::{NativeGenerationSpillDigestReport, canonical_fact_counts},
};

const MAX_ROOT_IDENTITY_BYTES: usize = 4_096;
const MAX_SOURCE_REVISION_BYTES: usize = 1_024;
const MAX_WORKERS: u16 = 256;
const MAX_RUN_EXCLUDES: usize = 4_096;
const MAX_RUN_EXCLUDE_BYTES: usize = 4_096;
const MAX_RUN_EXCLUDES_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const RECONCILE_STATE_COLUMN: usize = 0;
const RECONCILE_SEQUENCE_COLUMN: usize = 1;
const RECONCILE_DIGEST_COLUMN: usize = 2;
const RECONCILE_DIGEST_VERSION_COLUMN: usize = 3;
const RECONCILE_CURRENT_COLUMN: usize = 4;
const RECONCILE_LEASE_ID_COLUMN: usize = 5;
const RECONCILE_LEASE_UNEXPIRED_COLUMN: usize = 6;
const RECONCILE_LEASE_GENERATION_COLUMN: usize = 7;
const GENERATION_LOCK_NAMESPACE: &str = "cartograph-v2-generation";
const COPIED_RELATION_PLANNER_COLUMNS: [(&str, &str); 6] = [
    (
        "files",
        "project_id, generation_id, file_id, normalized_path",
    ),
    (
        "symbols",
        "project_id, generation_id, symbol_id, file_id, symbol_kind, structural_digest",
    ),
    (
        "edges",
        "project_id, generation_id, source_symbol_id, target_symbol_id, edge_kind",
    ),
    (
        "references",
        "project_id, generation_id, owner_symbol_id, target_symbol_id, reference_kind",
    ),
    (
        "numerical_sites",
        "project_id, generation_id, owner_symbol_id, file_id, hazard, precision",
    ),
    (
        "search_documents",
        "project_id, generation_id, symbol_id, document_kind, id",
    ),
];

/// Validated-at-write project registration input.
pub struct NewProject {
    root_identity: String,
    repository_fingerprint: ContentDigest,
}

impl NewProject {
    /// Build a project request. Validation occurs at the database boundary so
    /// every caller receives the same stable error contract.
    #[must_use]
    pub fn new(root_identity: impl Into<String>, repository_fingerprint: ContentDigest) -> Self {
        Self {
            root_identity: root_identity.into(),
            repository_fingerprint,
        }
    }
}

/// Validated-at-write immutable generation request.
pub struct NewGeneration {
    project_id: ProjectId,
    source_revision: String,
    worker_count: u16,
    run_excludes: Vec<String>,
}

impl NewGeneration {
    /// Build a generation request tied to one branded project identity.
    #[must_use]
    pub fn new(
        project_id: ProjectId,
        source_revision: impl Into<String>,
        worker_count: u16,
    ) -> Self {
        Self {
            project_id,
            source_revision: source_revision.into(),
            worker_count,
            run_excludes: Vec::new(),
        }
    }

    /// Persist the run-scoped exclusion policy used to build this generation.
    /// Validation remains at the database boundary so every caller shares one
    /// count, item-size, aggregate-size, and NUL contract.
    #[must_use]
    pub fn with_run_excludes(mut self, run_excludes: Vec<String>) -> Self {
        self.run_excludes = run_excludes;
        self
    }
}

/// Exact project/generation identity used to rehydrate restart-safe type state.
#[derive(Clone, Copy, Debug)]
pub struct GenerationRecoveryRequest<'a> {
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
}

impl<'a> GenerationRecoveryRequest<'a> {
    /// Bind one immutable generation to its owning project.
    #[must_use]
    pub const fn new(project_id: &'a ProjectId, generation_id: &'a GenerationId) -> Self {
        Self {
            project_id,
            generation_id,
        }
    }
}

/// Opaque token proving a durable generation exists in `staging` state.
#[derive(Debug)]
pub struct StagedGeneration {
    project_id: ProjectId,
    generation_id: GenerationId,
    sequence: i64,
}

impl StagedGeneration {
    /// Owning project.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic project-local sequence used to prevent stale publication.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }
}

/// Opaque token proving all required contents committed and the durable state is `ready`.
#[derive(Debug)]
pub struct ReadyGeneration {
    project_id: ProjectId,
    generation_id: GenerationId,
    sequence: i64,
    content_digest: ContentDigest,
    digest_version: GenerationDigestVersion,
}

impl ReadyGeneration {
    /// Owning project.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic project-local sequence used to prevent stale publication.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Deterministic digest of the complete reduced logical fact set.
    #[must_use]
    pub const fn content_digest(&self) -> &ContentDigest {
        &self.content_digest
    }

    /// Persisted contract used to interpret the logical digest.
    #[must_use]
    pub const fn digest_version(&self) -> GenerationDigestVersion {
        self.digest_version
    }
}

/// Opaque token proving the atomic publication transaction committed.
#[derive(Debug)]
pub struct CurrentGeneration {
    project_id: ProjectId,
    generation_id: GenerationId,
    sequence: i64,
    content_digest: ContentDigest,
    digest_version: GenerationDigestVersion,
}

/// Durable generation state observed while reconciling a timed-out mutation.
#[derive(Debug)]
pub enum ObservedGeneration {
    /// The exact generation remains writable under a valid fence.
    Staged(StagedGeneration),
    /// COPY committed and the exact generation remains publishable.
    Ready(ReadyGeneration),
    /// Publication committed and the project pointer names this generation.
    Current(CurrentGeneration),
    /// Cleanup committed and the generation is terminally failed.
    Failed,
    /// A newer generation superseded this generation.
    Superseded,
}

/// Exact lease disposition observed in the same reconciliation query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedLease {
    /// The same exact token is current and unexpired according to PostgreSQL.
    Owned,
    /// No operation-lease row currently exists.
    Released,
    /// The row expired or belongs to a different takeover token.
    Lost,
}

/// Single-query durable snapshot used after a client-side timeout or commit error.
#[derive(Debug)]
pub struct OperationReconciliation {
    generation: ObservedGeneration,
    lease: ObservedLease,
}

impl OperationReconciliation {
    /// Rehydrated exact generation state.
    #[must_use]
    pub const fn generation(&self) -> &ObservedGeneration {
        &self.generation
    }

    /// Consume the snapshot and recover the generation type-state token.
    #[must_use]
    pub fn into_generation(self) -> ObservedGeneration {
        self.generation
    }

    /// Exact-token lease disposition from the same database statement.
    #[must_use]
    pub const fn lease(&self) -> ObservedLease {
        self.lease
    }
}

/// Exact prepare/COPY fence paired with its PostgreSQL-side statement deadline.
///
/// Prepare authority is not terminal authority:
///
/// ```compile_fail
/// async fn work_cannot_publish(
///     database: &cartograph_db::CartographDatabase,
///     ready: cartograph_db::ReadyGeneration,
///     prepare: cartograph_db::PrepareGenerationMutation<'_>,
/// ) {
///     let _ = database.publish_generation_bounded(ready, prepare).await;
/// }
/// ```
///
/// ```compile_fail
/// async fn work_cannot_cleanup(
///     database: &cartograph_db::CartographDatabase,
///     prepare: cartograph_db::PrepareGenerationMutation<'_>,
/// ) {
///     let _ = database
///         .fail_generation_and_release_bounded(prepare)
///         .await;
/// }
/// ```
pub struct PrepareGenerationMutation<'a>(LeaseMutationOptions<'a>);

impl<'a> PrepareGenerationMutation<'a> {
    /// Bind an exact lease fence to one prepare/COPY statement deadline.
    #[must_use]
    pub const fn new(fence: &'a LeaseFence, statement_timeout: Duration) -> Self {
        Self(LeaseMutationOptions::bounded(fence, statement_timeout))
    }
}

/// Exact terminal-transition fence paired with a short database deadline.
pub struct TerminalGenerationMutation<'a>(LeaseMutationOptions<'a>);

impl<'a> TerminalGenerationMutation<'a> {
    /// Bind an exact lease fence to one publish or cleanup deadline.
    #[must_use]
    pub const fn new(fence: &'a LeaseFence, statement_timeout: Duration) -> Self {
        Self(LeaseMutationOptions::bounded(fence, statement_timeout))
    }
}

impl CurrentGeneration {
    /// Owning project.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic project-local publication sequence.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Deterministic digest of the published logical fact set.
    #[must_use]
    pub const fn content_digest(&self) -> &ContentDigest {
        &self.content_digest
    }

    /// Persisted contract used to interpret the logical digest.
    #[must_use]
    pub const fn digest_version(&self) -> GenerationDigestVersion {
        self.digest_version
    }
}

/// Opaque token proving a staging or ready generation was deliberately failed.
#[derive(Debug)]
pub struct FailedGeneration {
    project_id: ProjectId,
    generation_id: GenerationId,
    sequence: i64,
}

impl FailedGeneration {
    /// Owning project.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic project-local sequence.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }
}

/// Generation states that may be resumed or deliberately failed after a
/// transient error or process restart.
#[derive(Debug)]
pub enum RecoverableGeneration {
    /// Contents may still be staged and the generation may become ready.
    Staged(StagedGeneration),
    /// Contents committed and the generation may still be published.
    Ready(ReadyGeneration),
}

/// Failed prepare operation that returns ownership of the staging token.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct PrepareGenerationError {
    generation: StagedGeneration,
    #[source]
    error: StorageError,
}

impl PrepareGenerationError {
    /// Inspect the credential-safe storage error without consuming recovery state.
    #[must_use]
    pub const fn error(&self) -> &StorageError {
        &self.error
    }

    /// Recover the staging token and underlying error for retry or failure marking.
    #[must_use]
    pub fn into_parts(self) -> (StagedGeneration, StorageError) {
        (self.generation, self.error)
    }
}

/// Failed publication that returns ownership of the ready token.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct PublishGenerationError {
    generation: Box<ReadyGeneration>,
    #[source]
    error: StorageError,
}

impl PublishGenerationError {
    /// Inspect the credential-safe storage error without consuming recovery state.
    #[must_use]
    pub const fn error(&self) -> &StorageError {
        &self.error
    }

    /// Recover the ready token and underlying error for retry or failure marking.
    #[must_use]
    pub fn into_parts(self) -> (ReadyGeneration, StorageError) {
        (*self.generation, self.error)
    }
}

/// Failed failure-marking operation that preserves the recoverable token.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct FailGenerationError {
    generation: Box<RecoverableGeneration>,
    #[source]
    error: StorageError,
}

impl FailGenerationError {
    /// Inspect the credential-safe storage error without consuming recovery state.
    #[must_use]
    pub const fn error(&self) -> &StorageError {
        &self.error
    }

    /// Recover the state token and underlying error for another checked attempt.
    #[must_use]
    pub fn into_parts(self) -> (RecoverableGeneration, StorageError) {
        (*self.generation, self.error)
    }
}

/// Complete storage-validated output required before a generation can become ready.
pub struct GenerationContents {
    generation: StagedGeneration,
    facts: CanonicalGenerationFacts,
    metrics: Option<PrepareGenerationMetrics>,
    progress: Option<PrepareGenerationProgress>,
}

/// Complete database-reduced output required before a spilled generation can become ready.
pub struct SpilledGenerationContents {
    generation: StagedGeneration,
    digest: NativeGenerationSpillDigestReport,
    metrics: Option<PrepareGenerationMetrics>,
    progress: Option<PrepareGenerationProgress>,
}

struct PrepareTransactionInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    generation: &'a StagedGeneration,
    facts: CanonicalGenerationFacts,
    fence: &'a LeaseFence,
    metrics: Option<&'a PrepareGenerationMetrics>,
    progress: Option<&'a PrepareGenerationProgress>,
}

struct SpilledPrepareTransactionInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    generation: &'a StagedGeneration,
    digest: &'a NativeGenerationSpillDigestReport,
    fence: &'a LeaseFence,
    metrics: Option<&'a PrepareGenerationMetrics>,
    progress: Option<&'a PrepareGenerationProgress>,
}

struct CopiedRelationCheck<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
}

struct CopyAndValidateInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    generation: &'a StagedGeneration,
    facts: CanonicalGenerationFacts,
    metrics: Option<&'a PrepareGenerationMetrics>,
    progress: Option<&'a PrepareGenerationProgress>,
}

/// Cloneable observer for the exact PostgreSQL COPY-stream duration.
#[derive(Default)]
struct PrepareGenerationMetricsInner {
    copy: AtomicU64,
    files_copy: AtomicU64,
    symbols_copy: AtomicU64,
    edges_copy: AtomicU64,
    references_copy: AtomicU64,
    numerical_sites_copy: AtomicU64,
    documents_copy: AtomicU64,
    relation_validation: AtomicU64,
}

/// Shareable observer for one generation's COPY and relation-validation durations.
#[derive(Clone, Default)]
pub struct PrepareGenerationMetrics {
    inner: Arc<PrepareGenerationMetricsInner>,
}

/// Cloneable monotonic evidence that a durable prepare operation completed bounded work.
#[derive(Clone, Default)]
pub struct PrepareGenerationProgress {
    sequence: Arc<AtomicU64>,
}

/// Point-in-time prepare metrics retained after the generation contents move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrepareGenerationMetricsSnapshot {
    copy: Duration,
    files_copy: Duration,
    symbols_copy: Duration,
    edges_copy: Duration,
    references_copy: Duration,
    numerical_sites_copy: Duration,
    documents_copy: Duration,
    relation_validation: Duration,
}

impl PrepareGenerationMetrics {
    /// Create an empty observer for one generation preparation attempt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the most recently observed six-table PostgreSQL COPY duration.
    #[must_use]
    pub fn snapshot(&self) -> PrepareGenerationMetricsSnapshot {
        PrepareGenerationMetricsSnapshot {
            copy: observed_duration(&self.inner.copy),
            files_copy: observed_duration(&self.inner.files_copy),
            symbols_copy: observed_duration(&self.inner.symbols_copy),
            edges_copy: observed_duration(&self.inner.edges_copy),
            references_copy: observed_duration(&self.inner.references_copy),
            numerical_sites_copy: observed_duration(&self.inner.numerical_sites_copy),
            documents_copy: observed_duration(&self.inner.documents_copy),
            relation_validation: observed_duration(&self.inner.relation_validation),
        }
    }

    fn record_copy(&self, duration: Duration, tables: CopyTableDurations) {
        store_duration(&self.inner.copy, duration);
        store_duration(&self.inner.files_copy, tables.files);
        store_duration(&self.inner.symbols_copy, tables.symbols);
        store_duration(&self.inner.edges_copy, tables.edges);
        store_duration(&self.inner.references_copy, tables.references);
        store_duration(&self.inner.numerical_sites_copy, tables.numerical_sites);
        store_duration(&self.inner.documents_copy, tables.documents);
        store_duration(&self.inner.relation_validation, Duration::ZERO);
    }

    fn record_relation_validation(&self, duration: Duration) {
        store_duration(&self.inner.relation_validation, duration);
    }
}

impl PrepareGenerationProgress {
    /// Create an empty progress observer for one supervised prepare operation.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the monotonic number of completed COPY statements and prepare phases.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }

    pub(crate) fn advance(&self) {
        let _ = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            });
    }
}

fn observed_duration(value: &AtomicU64) -> Duration {
    Duration::from_nanos(value.load(Ordering::Relaxed))
}

fn store_duration(target: &AtomicU64, duration: Duration) {
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    target.store(nanos, Ordering::Relaxed);
}

impl PrepareGenerationMetricsSnapshot {
    /// Wall-clock duration spent streaming all six immutable generation fact relations.
    #[must_use]
    pub const fn copy_duration(self) -> Duration {
        self.copy
    }

    /// Wall-clock duration of the files COPY statement.
    #[must_use]
    pub const fn files_copy_duration(self) -> Duration {
        self.files_copy
    }

    /// Wall-clock duration of the symbols COPY statement.
    #[must_use]
    pub const fn symbols_copy_duration(self) -> Duration {
        self.symbols_copy
    }

    /// Wall-clock duration of the edges COPY statement.
    #[must_use]
    pub const fn edges_copy_duration(self) -> Duration {
        self.edges_copy
    }

    /// Wall-clock duration of the references COPY statement.
    #[must_use]
    pub const fn references_copy_duration(self) -> Duration {
        self.references_copy
    }

    /// Wall-clock duration of the numerical-sites COPY statement.
    #[must_use]
    pub const fn numerical_sites_copy_duration(self) -> Duration {
        self.numerical_sites_copy
    }

    /// Wall-clock duration of the search-documents COPY statement, including BM25 maintenance.
    #[must_use]
    pub const fn documents_copy_duration(self) -> Duration {
        self.documents_copy
    }

    /// Wall-clock duration of the set-based PostgreSQL relation-integrity check.
    #[must_use]
    pub const fn relation_validation_duration(self) -> Duration {
        self.relation_validation
    }
}

struct GenerationStateRequirement<'a> {
    quoted_schema: &'a str,
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
    sequence: i64,
    required: GenerationState,
}

struct PublishTransactionInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    generation: &'a ReadyGeneration,
    fence: &'a LeaseFence,
}

struct FailTransactionInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    fence: &'a LeaseFence,
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
    sequence: i64,
    expected_state: GenerationState,
}

struct LeaseMutationOptions<'a> {
    fence: &'a LeaseFence,
    statement_timeout: Option<Duration>,
}

impl<'a> LeaseMutationOptions<'a> {
    const fn unbounded(fence: &'a LeaseFence) -> Self {
        Self {
            fence,
            statement_timeout: None,
        }
    }

    const fn bounded(fence: &'a LeaseFence, statement_timeout: Duration) -> Self {
        Self {
            fence,
            statement_timeout: Some(statement_timeout),
        }
    }
}

#[derive(Clone, Copy)]
enum FenceCheck {
    Observe,
    HeartbeatCompatibleLock,
    Lock,
}

impl FenceCheck {
    const fn row_lock(self) -> &'static str {
        match self {
            Self::Observe => "",
            Self::HeartbeatCompatibleLock => " FOR KEY SHARE",
            Self::Lock => " FOR UPDATE",
        }
    }

    const fn operation(self) -> &'static str {
        match self {
            Self::Observe => "check-generation-fence",
            Self::HeartbeatCompatibleLock => "protect-generation-fence",
            Self::Lock => "lock-generation-fence",
        }
    }
}

struct FenceValidation<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    fence: &'a LeaseFence,
    check: FenceCheck,
}

impl<'a> FenceValidation<'a> {
    const fn new(
        schema: &'a cartograph_config::DatabaseSchema,
        fence: &'a LeaseFence,
        check: FenceCheck,
    ) -> Self {
        Self {
            schema,
            fence,
            check,
        }
    }
}

struct ReadyTransition<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    generation: &'a StagedGeneration,
    fence: &'a LeaseFence,
    content_digest: &'a ContentDigest,
    digest_version: GenerationDigestVersion,
    /// Exact canonical counts recorded so snapshots need not count fact tables.
    counts: crate::NativeGenerationSpillFactCounts,
}

impl GenerationContents {
    /// Consume a staging token and attach a prevalidated canonical payload.
    #[must_use]
    pub const fn new(generation: StagedGeneration, facts: CanonicalGenerationFacts) -> Self {
        Self {
            generation,
            facts,
            metrics: None,
            progress: None,
        }
    }

    /// Attach an observer without changing validation or persistence behavior.
    #[must_use]
    pub fn with_metrics(mut self, metrics: PrepareGenerationMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Attach monotonic supervised progress without changing persistence behavior.
    #[must_use]
    pub fn with_progress(mut self, progress: PrepareGenerationProgress) -> Self {
        self.progress = Some(progress);
        self
    }
}

impl SpilledGenerationContents {
    /// Consume a staging token and attach its exact streamed digest capability.
    #[must_use]
    pub const fn new(
        generation: StagedGeneration,
        digest: NativeGenerationSpillDigestReport,
    ) -> Self {
        Self {
            generation,
            digest,
            metrics: None,
            progress: None,
        }
    }

    /// Attach an observer for relation validation and final preparation.
    #[must_use]
    pub fn with_metrics(mut self, metrics: PrepareGenerationMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Attach monotonic supervised progress without changing persistence behavior.
    #[must_use]
    pub fn with_progress(mut self, progress: PrepareGenerationProgress) -> Self {
        self.progress = Some(progress);
        self
    }
}

impl CartographDatabase {
    /// Create or refresh a stable project row and return its branded identity.
    /// # Errors
    ///
    /// Returns an error if the root identity is empty/oversized, PostgreSQL
    /// cannot upsert the project, or the returned project UUID is malformed.
    pub async fn register_project(&self, input: NewProject) -> Result<ProjectId, StorageError> {
        validate_bounded_text(
            &input.root_identity,
            "root_identity",
            MAX_ROOT_IDENTITY_BYTES,
        )?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"INSERT INTO {schema}."projects" (root_identity, repository_fingerprint)
                VALUES ($1, $2)
                ON CONFLICT (root_identity) DO UPDATE
                SET repository_fingerprint = EXCLUDED.repository_fingerprint,
                    updated_at = clock_timestamp()
                RETURNING project_id::text"#
        );
        let row = audited_query(sql)
            .bind(input.root_identity)
            .bind(input.repository_fingerprint.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| database_error("register-project"))?;
        parse_project_id(&row, 0)
    }

    /// Start an immutable generation in staging state.
    /// # Errors
    ///
    /// Returns an error if source revision or worker count is invalid, the
    /// project does not exist, or the reserved staging row cannot be created/decoded.
    pub async fn begin_generation(
        &self,
        input: NewGeneration,
    ) -> Result<StagedGeneration, StorageError> {
        validate_bounded_text(
            &input.source_revision,
            "source_revision",
            MAX_SOURCE_REVISION_BYTES,
        )?;
        if !(1..=MAX_WORKERS).contains(&input.worker_count) {
            return Err(StorageError::InvalidInput {
                field: "worker_count",
            });
        }
        validate_run_excludes(&input.run_excludes)?;
        let worker_count =
            i16::try_from(input.worker_count).map_err(|_| StorageError::InvalidInput {
                field: "worker_count",
            })?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"WITH reserved AS (
                    UPDATE {schema}."projects"
                    SET next_generation_sequence = next_generation_sequence + 1,
                        updated_at = clock_timestamp()
                    WHERE project_id = CAST($1 AS uuid)
                    RETURNING next_generation_sequence - 1 AS generation_sequence
                )
                INSERT INTO {schema}."index_generations" (
                    project_id, generation_sequence, source_revision, worker_count, run_excludes
                )
                SELECT CAST($1 AS uuid), generation_sequence, $2, $3, $4 FROM reserved
                RETURNING generation_id::text, generation_sequence"#
        );
        let row = audited_query(sql)
            .bind(input.project_id.as_str())
            .bind(input.source_revision)
            .bind(worker_count)
            .bind(input.run_excludes)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| database_error("begin-generation"))?;
        let generation_id = parse_generation_id(&row, 0)?;
        let sequence = parse_generation_sequence(&row, 1)?;
        Ok(StagedGeneration {
            project_id: input.project_id,
            generation_id,
            sequence,
        })
    }

    /// Terminalize an exact staging generation only when no live lease owns it.
    ///
    /// This closes the pre-supervisor failure window where durable staging was
    /// created before a runtime could acquire the project write lease. The
    /// project advisory lock serializes the check with lease acquisition, while
    /// the exact generation predicate keeps current, ready, and terminal state
    /// immutable. The boolean is true only when this call changed the row.
    /// # Errors
    ///
    /// Returns an error if the deadline is invalid, the advisory lock fails,
    /// or PostgreSQL cannot prove and terminalize the exact unleased staging row.
    pub async fn fail_unleased_staging_generation_bounded(
        &self,
        request: GenerationRecoveryRequest<'_>,
        statement_timeout: Duration,
    ) -> Result<bool, StorageError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("fail-unleased-staging-begin"))?;
        if crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(StorageError::InvalidInput {
                    field: "statement_timeout",
                }),
                Err(_) => Err(database_error("fail-unleased-staging-rollback")),
            };
        }
        let lock = query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(project_lock_key(&self.schema, request.project_id))
            .execute(&mut *transaction)
            .await;
        if lock.is_err() {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("fail-unleased-staging-lock")),
                Err(_) => Err(database_error("fail-unleased-staging-rollback")),
            };
        }
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"UPDATE {schema}."index_generations" AS generations
                SET state = 'failed'
                WHERE generations.project_id = CAST($1 AS uuid)
                  AND generations.generation_id = CAST($2 AS uuid)
                  AND generations.state = 'staging'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM {schema}."project_operation_leases" AS leases
                      WHERE leases.project_id = generations.project_id
                        AND leases.generation_id = generations.generation_id
                        AND leases.expires_at > clock_timestamp()
                  )"#
        );
        let updated = audited_query(sql)
            .bind(request.project_id.as_str())
            .bind(request.generation_id.as_str())
            .execute(&mut *transaction)
            .await;
        let updated = match updated {
            Ok(updated) => updated.rows_affected() == 1,
            Err(_) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(database_error("fail-unleased-staging")),
                    Err(_) => Err(database_error("fail-unleased-staging-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("fail-unleased-staging-commit"))?;
        Ok(updated)
    }

    /// Terminalize every abandoned staging generation for one project while
    /// preserving any generation owned by a live lease.
    ///
    /// This is the bounded retry preflight used by index/sync after an attached
    /// client or worker disappeared between staging and publication.
    /// # Errors
    ///
    /// Returns an error if the timeout is invalid, the project lock cannot be
    /// acquired, or the exact guarded update cannot commit.
    pub async fn fail_abandoned_staging_generations_bounded(
        &self,
        project_id: &ProjectId,
        statement_timeout: Duration,
    ) -> Result<u64, StorageError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("fail-abandoned-staging-begin"))?;
        if crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(StorageError::InvalidInput {
                    field: "statement_timeout",
                }),
                Err(_) => Err(database_error("fail-abandoned-staging-rollback")),
            };
        }
        let lock = query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(project_lock_key(&self.schema, project_id))
            .execute(&mut *transaction)
            .await;
        if lock.is_err() {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("fail-abandoned-staging-lock")),
                Err(_) => Err(database_error("fail-abandoned-staging-rollback")),
            };
        }
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"UPDATE {schema}."index_generations" AS generations
                SET state = 'failed'
                WHERE generations.project_id = CAST($1 AS uuid)
                  AND generations.state = 'staging'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM {schema}."project_operation_leases" AS leases
                      WHERE leases.project_id = generations.project_id
                        AND leases.generation_id = generations.generation_id
                        AND leases.expires_at > clock_timestamp()
                  )"#
        );
        let updated = audited_query(sql)
            .bind(project_id.as_str())
            .execute(&mut *transaction)
            .await;
        let updated = match updated {
            Ok(updated) => updated.rows_affected(),
            Err(_) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(database_error("fail-abandoned-staging")),
                    Err(_) => Err(database_error("fail-abandoned-staging-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("fail-abandoned-staging-commit"))?;
        Ok(updated)
    }

    /// Deterministically reduce and atomically COPY all fact tables before
    /// moving the consumed generation token to `ready`.
    /// # Errors
    ///
    /// Returns an error with the staging token if the lease fence is stale,
    /// facts/metrics fail validation, COPY fails, or ready publication cannot commit.
    pub async fn prepare_generation(
        &self,
        contents: GenerationContents,
        fence: &LeaseFence,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        self.prepare_generation_inner(contents, LeaseMutationOptions::unbounded(fence))
            .await
    }

    /// Validate, COPY, and mark ready under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error with the staging token if the deadline/fence is
    /// invalid or bounded validation, COPY, derived-index setup, or commit fails.
    pub async fn prepare_generation_bounded(
        &self,
        contents: GenerationContents,
        mutation: PrepareGenerationMutation<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        self.prepare_generation_inner(contents, mutation.0).await
    }

    async fn prepare_generation_inner(
        &self,
        contents: GenerationContents,
        mutation: LeaseMutationOptions<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        let fence = mutation.fence;
        let GenerationContents {
            generation,
            facts,
            metrics,
            progress,
        } = contents;
        if !fence_matches_generation(fence, generation.project_id(), generation.generation_id()) {
            return Err(PrepareGenerationError {
                generation,
                error: StorageError::LeaseFenceLost,
            });
        }
        let content_digest = facts.digest.clone();
        let digest_version = facts.digest_version;
        let Ok(mut transaction) = self.pool.begin().await else {
            return Err(PrepareGenerationError {
                generation,
                error: database_error("prepare-begin"),
            });
        };
        if let Some(statement_timeout) = mutation.statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            let error = match transaction.rollback().await {
                Ok(()) => database_error("prepare-statement-timeout"),
                Err(_) => database_error("prepare-rollback"),
            };
            return Err(PrepareGenerationError { generation, error });
        }
        let result = prepare_transaction(
            &mut transaction,
            PrepareTransactionInput {
                schema: &self.schema,
                generation: &generation,
                facts,
                fence,
                metrics: metrics.as_ref(),
                progress: progress.as_ref(),
            },
        )
        .await;
        if let Err(error) = result {
            let error = match transaction.rollback().await {
                Ok(()) => error,
                Err(_) => database_error("prepare-rollback"),
            };
            return Err(PrepareGenerationError { generation, error });
        }
        if transaction.commit().await.is_err() {
            return Err(PrepareGenerationError {
                generation,
                error: database_error("prepare-commit"),
            });
        }
        Ok(ReadyGeneration {
            project_id: generation.project_id,
            generation_id: generation.generation_id,
            sequence: generation.sequence,
            content_digest,
            digest_version,
        })
    }

    /// Validate already canonicalized spill rows and move the exact generation to ready.
    /// # Errors
    ///
    /// Returns an error with the staging token when the digest capability belongs to
    /// another generation, the lease/state changed, relation validation fails, or the
    /// final bounded transaction cannot commit.
    pub async fn prepare_spilled_generation(
        &self,
        contents: SpilledGenerationContents,
        fence: &LeaseFence,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        self.prepare_spilled_generation_inner(contents, LeaseMutationOptions::unbounded(fence))
            .await
    }

    /// Finalize an already canonicalized spill under a PostgreSQL statement deadline.
    /// # Errors
    ///
    /// Returns an error with the staging token when the deadline/fence is invalid,
    /// canonical rows or their digest disagree, or ready publication cannot commit.
    pub async fn prepare_spilled_generation_bounded(
        &self,
        contents: SpilledGenerationContents,
        mutation: PrepareGenerationMutation<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        self.prepare_spilled_generation_inner(contents, mutation.0)
            .await
    }

    async fn prepare_spilled_generation_inner(
        &self,
        contents: SpilledGenerationContents,
        mutation: LeaseMutationOptions<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        let fence = mutation.fence;
        let SpilledGenerationContents {
            generation,
            digest,
            metrics,
            progress,
        } = contents;
        if !fence_matches_generation(fence, generation.project_id(), generation.generation_id())
            || !digest.matches(generation.project_id(), generation.generation_id())
        {
            return Err(PrepareGenerationError {
                generation,
                error: StorageError::LeaseFenceLost,
            });
        }
        let content_digest = digest.digest().clone();
        let digest_version = GenerationDigestVersion::CURRENT;
        let Ok(mut transaction) = self.pool.begin().await else {
            return Err(PrepareGenerationError {
                generation,
                error: database_error("prepare-spilled-begin"),
            });
        };
        if let Some(statement_timeout) = mutation.statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            let error = match transaction.rollback().await {
                Ok(()) => database_error("prepare-spilled-statement-timeout"),
                Err(_) => database_error("prepare-spilled-rollback"),
            };
            return Err(PrepareGenerationError { generation, error });
        }
        let result = prepare_spilled_transaction(
            &mut transaction,
            SpilledPrepareTransactionInput {
                schema: &self.schema,
                generation: &generation,
                digest: &digest,
                fence,
                metrics: metrics.as_ref(),
                progress: progress.as_ref(),
            },
        )
        .await;
        if let Err(error) = result {
            let error = match transaction.rollback().await {
                Ok(()) => error,
                Err(_) => database_error("prepare-spilled-rollback"),
            };
            return Err(PrepareGenerationError { generation, error });
        }
        if transaction.commit().await.is_err() {
            return Err(PrepareGenerationError {
                generation,
                error: database_error("prepare-spilled-commit"),
            });
        }
        Ok(ReadyGeneration {
            project_id: generation.project_id,
            generation_id: generation.generation_id,
            sequence: generation.sequence,
            content_digest,
            digest_version,
        })
    }

    /// Atomically supersede the previous current generation and publish the
    /// consumed ready token under a project-scoped advisory lock.
    /// # Errors
    ///
    /// Returns an error with the ready token if the lease is stale, state is no
    /// longer ready, another writer publishes first, or the transaction fails.
    pub async fn publish_generation(
        &self,
        generation: ReadyGeneration,
        fence: &LeaseFence,
    ) -> Result<CurrentGeneration, PublishGenerationError> {
        self.publish_generation_inner(generation, LeaseMutationOptions::unbounded(fence))
            .await
    }

    /// Publish under a PostgreSQL-side statement deadline so a dropped client
    /// future cannot leave a lock-waiting backend statement behind.
    /// # Errors
    ///
    /// Returns an error with the ready token if the deadline/fence is invalid,
    /// publication loses its state race, or the atomic pointer update cannot commit.
    pub async fn publish_generation_bounded(
        &self,
        generation: ReadyGeneration,
        mutation: TerminalGenerationMutation<'_>,
    ) -> Result<CurrentGeneration, PublishGenerationError> {
        self.publish_generation_inner(generation, mutation.0).await
    }

    async fn publish_generation_inner(
        &self,
        generation: ReadyGeneration,
        mutation: LeaseMutationOptions<'_>,
    ) -> Result<CurrentGeneration, PublishGenerationError> {
        let Ok(mut transaction) = self.pool.begin().await else {
            return Err(PublishGenerationError {
                generation: Box::new(generation),
                error: database_error("publish-begin"),
            });
        };
        if let Some(statement_timeout) = mutation.statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            let error = match transaction.rollback().await {
                Ok(()) => database_error("publish-statement-timeout"),
                Err(_) => database_error("publish-rollback"),
            };
            return Err(PublishGenerationError {
                generation: Box::new(generation),
                error,
            });
        }
        let result = publish_transaction(
            &mut transaction,
            PublishTransactionInput {
                schema: &self.schema,
                generation: &generation,
                fence: mutation.fence,
            },
        )
        .await;
        if let Err(error) = result {
            let error = match transaction.rollback().await {
                Ok(()) => error,
                Err(_) => database_error("publish-rollback"),
            };
            return Err(PublishGenerationError {
                generation: Box::new(generation),
                error,
            });
        }
        if transaction.commit().await.is_err() {
            return Err(PublishGenerationError {
                generation: Box::new(generation),
                error: database_error("publish-commit"),
            });
        }
        Ok(CurrentGeneration {
            project_id: generation.project_id,
            generation_id: generation.generation_id,
            sequence: generation.sequence,
            content_digest: generation.content_digest,
            digest_version: generation.digest_version,
        })
    }

    /// Rehydrate a staging or ready type-state token after a process restart or
    /// an ambiguous connection failure. Published and terminal states are not
    /// recoverable through this mutation surface.
    /// # Errors
    ///
    /// Returns an error if the generation identity cannot be read/decoded or
    /// durable state violates the recoverable staging-or-ready contract.
    pub async fn recover_generation(
        &self,
        request: GenerationRecoveryRequest<'_>,
    ) -> Result<Option<RecoverableGeneration>, StorageError> {
        self.recover_generation_inner(request, None).await
    }

    /// Rehydrate one generation under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline is invalid, the generation row cannot
    /// be read, or its state/identity is not recoverable.
    pub async fn recover_generation_bounded(
        &self,
        request: GenerationRecoveryRequest<'_>,
        statement_timeout: Duration,
    ) -> Result<Option<RecoverableGeneration>, StorageError> {
        self.recover_generation_inner(request, Some(statement_timeout))
            .await
    }

    async fn recover_generation_inner(
        &self,
        request: GenerationRecoveryRequest<'_>,
        statement_timeout: Option<Duration>,
    ) -> Result<Option<RecoverableGeneration>, StorageError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("recover-generation-begin"))?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("recover-generation-statement-timeout")),
                Err(_) => Err(database_error("recover-generation-rollback")),
            };
        }
        let recovered = self
            .recover_generation_connection(&mut transaction, request)
            .await;
        match recovered {
            Ok(recovered) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error("recover-generation-commit"))?;
                Ok(recovered)
            }
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(_) => Err(database_error("recover-generation-rollback")),
            },
        }
    }

    async fn recover_generation_connection(
        &self,
        connection: &mut PgConnection,
        request: GenerationRecoveryRequest<'_>,
    ) -> Result<Option<RecoverableGeneration>, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT state, generation_sequence, content_digest, content_digest_version
                FROM {schema}."index_generations"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#
        );
        let row = audited_query(sql)
            .bind(request.project_id.as_str())
            .bind(request.generation_id.as_str())
            .fetch_optional(connection)
            .await
            .map_err(|_| database_error("recover-generation"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let state = parse_generation_state(&row, 0)?;
        let sequence = parse_generation_sequence(&row, 1)?;
        let content_digest =
            row.try_get::<Option<String>, _>(2)
                .map_err(|_| StorageError::CorruptStoredValue {
                    field: "content_digest",
                })?;
        let digest_version = row
            .try_get::<Option<i16>, _>(3)
            .map_err(|_| corrupt("content_digest_version"))?
            .map(|value| {
                GenerationDigestVersion::from_database_value(value)
                    .map_err(|_| corrupt("content_digest_version"))
            })
            .transpose()?;
        Ok(match state {
            GenerationState::Staging if content_digest.is_none() && digest_version.is_none() => {
                Some(RecoverableGeneration::Staged(StagedGeneration {
                    project_id: request.project_id.clone(),
                    generation_id: request.generation_id.clone(),
                    sequence,
                }))
            }
            GenerationState::Ready => {
                let content_digest = content_digest
                    .ok_or(StorageError::CorruptStoredValue {
                        field: "content_digest",
                    })
                    .and_then(|digest| {
                        ContentDigest::parse(&digest).map_err(|_| {
                            StorageError::CorruptStoredValue {
                                field: "content_digest",
                            }
                        })
                    })?;
                let digest_version =
                    digest_version.ok_or_else(|| corrupt("content_digest_version"))?;
                Some(RecoverableGeneration::Ready(ReadyGeneration {
                    project_id: request.project_id.clone(),
                    generation_id: request.generation_id.clone(),
                    sequence,
                    content_digest,
                    digest_version,
                }))
            }
            GenerationState::Staging => {
                return Err(StorageError::CorruptStoredValue {
                    field: "content_digest_version",
                });
            }
            GenerationState::Current | GenerationState::Superseded
                if content_digest.is_none() || digest_version.is_none() =>
            {
                return Err(corrupt("content_digest_version"));
            }
            GenerationState::Failed if content_digest.is_some() != digest_version.is_some() => {
                return Err(corrupt("content_digest_version"));
            }
            GenerationState::Current
            | GenerationState::Superseded
            | GenerationState::Failed
            | GenerationState::Retiring => None,
        })
    }

    /// Mark a recovered staging or ready generation terminally failed without
    /// affecting the project's visible current generation.
    /// # Errors
    ///
    /// Returns an error with the recoverable token if its lease fence is stale,
    /// state changed, or the terminal transition cannot commit.
    pub async fn fail_generation(
        &self,
        generation: RecoverableGeneration,
        fence: &LeaseFence,
    ) -> Result<FailedGeneration, FailGenerationError> {
        let (project_id, generation_id, sequence, expected_state) = recoverable_parts(&generation);
        if !fence_matches_generation(fence, project_id, generation_id) {
            return Err(FailGenerationError {
                generation: Box::new(generation),
                error: StorageError::LeaseFenceLost,
            });
        }
        let Ok(mut transaction) = self.pool.begin().await else {
            return Err(FailGenerationError {
                generation: Box::new(generation),
                error: database_error("fail-begin"),
            });
        };
        let result = fail_transaction(
            &mut transaction,
            FailTransactionInput {
                schema: &self.schema,
                fence,
                project_id,
                generation_id,
                sequence,
                expected_state,
            },
        )
        .await;
        if let Err(error) = result {
            let error = match transaction.rollback().await {
                Ok(()) => error,
                Err(_) => database_error("fail-rollback"),
            };
            return Err(FailGenerationError {
                generation: Box::new(generation),
                error,
            });
        }
        if transaction.commit().await.is_err() {
            return Err(FailGenerationError {
                generation: Box::new(generation),
                error: database_error("fail-commit"),
            });
        }
        let (project_id, generation_id, sequence, _) = into_recoverable_parts(generation);
        Ok(FailedGeneration {
            project_id,
            generation_id,
            sequence,
        })
    }

    /// Atomically fail a staging/ready generation and release its exact lease.
    ///
    /// This operation is idempotent for an already-failed generation that still
    /// carries the same live token, which permits bounded timeout reconciliation.
    /// # Errors
    ///
    /// Returns an error if the exact generation lease is stale/malformed or
    /// the idempotent fail-and-release transaction cannot commit.
    pub async fn fail_generation_and_release(
        &self,
        fence: &LeaseFence,
    ) -> Result<(), StorageError> {
        self.fail_generation_and_release_inner(LeaseMutationOptions::unbounded(fence))
            .await
    }

    /// Fail and release under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline or exact generation lease is invalid,
    /// or the bounded fail-and-release transaction cannot commit.
    pub async fn fail_generation_and_release_bounded(
        &self,
        mutation: TerminalGenerationMutation<'_>,
    ) -> Result<(), StorageError> {
        self.fail_generation_and_release_inner(mutation.0).await
    }

    async fn fail_generation_and_release_inner(
        &self,
        mutation: LeaseMutationOptions<'_>,
    ) -> Result<(), StorageError> {
        let fence = mutation.fence;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("cleanup-begin"))?;
        if let Some(statement_timeout) = mutation.statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("cleanup-statement-timeout")),
                Err(_) => Err(database_error("cleanup-rollback")),
            };
        }
        let result = cleanup_transaction(&mut transaction, &self.schema, fence).await;
        if let Err(error) = result {
            return match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(_) => Err(database_error("cleanup-rollback")),
            };
        }
        transaction
            .commit()
            .await
            .map_err(|_| database_error("cleanup-commit"))
    }

    /// Reconcile generation and exact-token lease state in one PostgreSQL statement.
    /// # Errors
    ///
    /// Returns an error if the fence lacks a generation identity or PostgreSQL
    /// cannot atomically read/decode its generation and exact-token lease state.
    pub async fn reconcile_operation(
        &self,
        fence: &LeaseFence,
    ) -> Result<OperationReconciliation, StorageError> {
        self.reconcile_operation_inner(fence, None).await
    }

    /// Reconcile generation and lease state under a PostgreSQL statement deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline/fence shape is invalid or bounded
    /// reconciliation cannot read, decode, and commit exact durable state.
    pub async fn reconcile_operation_bounded(
        &self,
        fence: &LeaseFence,
        statement_timeout: Duration,
    ) -> Result<OperationReconciliation, StorageError> {
        self.reconcile_operation_inner(fence, Some(statement_timeout))
            .await
    }

    async fn reconcile_operation_inner(
        &self,
        fence: &LeaseFence,
        statement_timeout: Option<Duration>,
    ) -> Result<OperationReconciliation, StorageError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("reconcile-operation-begin"))?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("reconcile-operation-statement-timeout")),
                Err(_) => Err(database_error("reconcile-operation-rollback")),
            };
        }
        let result = reconcile_operation_query(&mut transaction, &self.schema, fence).await;
        let reconciliation = match result {
            Ok(reconciliation) => reconciliation,
            Err(error) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(error),
                    Err(_) => Err(database_error("reconcile-operation-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("reconcile-operation-commit"))?;
        Ok(reconciliation)
    }

    /// Read the durable state for diagnostics and invariant tests.
    /// # Errors
    ///
    /// Returns an error if PostgreSQL cannot read the addressed generation or
    /// its stored state is not a recognized generation state.
    pub async fn generation_state(
        &self,
        project_id: &ProjectId,
        generation_id: &GenerationId,
    ) -> Result<Option<GenerationState>, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT state FROM {schema}."index_generations"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#
        );
        let row = audited_query(sql)
            .bind(project_id.as_str())
            .bind(generation_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("generation-state"))?;
        row.map(|row| parse_generation_state(&row, 0)).transpose()
    }
}

async fn reconcile_operation_query(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    fence: &LeaseFence,
) -> Result<OperationReconciliation, StorageError> {
    let generation_id = fence
        .target()
        .generation_id()
        .ok_or(StorageError::InvalidInput {
            field: "generation_id",
        })?;
    let schema = crate::database::quoted_schema(schema);
    let sql = format!(
        r#"WITH database_clock AS (SELECT clock_timestamp() AS now)
                SELECT generations.state,
                       generations.generation_sequence,
                       generations.content_digest,
                       generations.content_digest_version,
                       projects.current_generation_id::text,
                       leases.lease_id::text,
                       leases.expires_at > database_clock.now,
                       leases.generation_id::text
                FROM {schema}."index_generations" AS generations
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = generations.project_id
                CROSS JOIN database_clock
                LEFT JOIN {schema}."project_operation_leases" AS leases
                  ON leases.project_id = generations.project_id
                 AND leases.operation = $3
                WHERE generations.project_id = CAST($1 AS uuid)
                  AND generations.generation_id = CAST($2 AS uuid)"#
    );
    let row = audited_query(sql)
        .bind(fence.target().project_id().as_str())
        .bind(generation_id.as_str())
        .bind(fence.target().operation().as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("reconcile-operation"))?
        .ok_or(StorageError::GenerationNotFound)?;
    decode_reconciliation(&row, fence)
}

async fn cleanup_transaction(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    fence: &LeaseFence,
) -> Result<(), StorageError> {
    let generation_id = fence
        .target()
        .generation_id()
        .ok_or(StorageError::InvalidInput {
            field: "generation_id",
        })?;
    lock_generation_fence(
        connection,
        FenceValidation::new(schema, fence, FenceCheck::Lock),
    )
    .await?;
    let quoted_schema = crate::database::quoted_schema(schema);
    let state_sql = format!(
        r#"SELECT state FROM {quoted_schema}."index_generations"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
            FOR UPDATE"#
    );
    let row = audited_query(state_sql)
        .bind(fence.target().project_id().as_str())
        .bind(generation_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| database_error("cleanup-generation-state"))?
        .ok_or(StorageError::GenerationNotFound)?;
    let state = parse_generation_state(&row, 0)?;
    match state {
        GenerationState::Staging | GenerationState::Ready => {
            let update_sql = format!(
                r#"UPDATE {quoted_schema}."index_generations"
                    SET state = 'failed'
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND state IN ('staging', 'ready')"#
            );
            let updated = audited_query(update_sql)
                .bind(fence.target().project_id().as_str())
                .bind(generation_id.as_str())
                .execute(&mut *connection)
                .await
                .map_err(|_| database_error("cleanup-fail-generation"))?;
            if updated.rows_affected() != 1 {
                return Err(StorageError::InvalidGenerationTransition {
                    actual: "changed concurrently".to_owned(),
                    requested: GenerationState::Failed.as_str(),
                });
            }
        }
        GenerationState::Failed => {}
        GenerationState::Current | GenerationState::Superseded | GenerationState::Retiring => {
            return Err(StorageError::InvalidGenerationTransition {
                actual: state.as_str().to_owned(),
                requested: GenerationState::Failed.as_str(),
            });
        }
    }
    delete_generation_spill_if_present(
        connection,
        GenerationSpillDelete {
            quoted_schema: &quoted_schema,
            project_id: fence.target().project_id(),
            generation_id,
        },
    )
    .await?;
    delete_generation_fence(connection, &quoted_schema, fence).await
}

async fn fail_transaction(
    connection: &mut PgConnection,
    input: FailTransactionInput<'_>,
) -> Result<(), StorageError> {
    lock_generation_fence(
        connection,
        FenceValidation::new(input.schema, input.fence, FenceCheck::Lock),
    )
    .await?;
    let quoted_schema = crate::database::quoted_schema(input.schema);
    let sql = format!(
        r#"UPDATE {quoted_schema}."index_generations"
            SET state = 'failed'
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND generation_sequence = $3
              AND state = $4"#
    );
    let result = audited_query(sql)
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .bind(input.sequence)
        .bind(input.expected_state.as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("fail-generation"))?;
    if result.rows_affected() == 1 {
        delete_generation_spill_if_present(
            connection,
            GenerationSpillDelete {
                quoted_schema: &quoted_schema,
                project_id: input.project_id,
                generation_id: input.generation_id,
            },
        )
        .await
    } else {
        Err(StorageError::InvalidGenerationTransition {
            actual: "changed concurrently".to_owned(),
            requested: GenerationState::Failed.as_str(),
        })
    }
}

struct GenerationSpillDelete<'input> {
    quoted_schema: &'input str,
    project_id: &'input ProjectId,
    generation_id: &'input GenerationId,
}

async fn delete_generation_spill_if_present(
    connection: &mut PgConnection,
    input: GenerationSpillDelete<'_>,
) -> Result<(), StorageError> {
    let statement = format!(
        r#"DELETE FROM {}."native_generation_spills"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        input.quoted_schema
    );
    audited_query(statement)
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .execute(connection)
        .await
        .map(|_| ())
        .map_err(|_| database_error("cleanup-generation-spill"))
}

async fn prepare_transaction(
    connection: &mut PgConnection,
    input: PrepareTransactionInput<'_>,
) -> Result<(), StorageError> {
    let quoted_schema = crate::database::quoted_schema(input.schema);
    let content_digest = input.facts.digest.clone();
    let digest_version = input.facts.digest_version;
    let fact_counts = input.facts.fact_counts();
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema: &quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            sequence: input.generation.sequence(),
            required: GenerationState::Staging,
        },
        FenceCheck::Observe,
    )
    .await?;
    // Establish the same operation -> generation advisory order used by
    // cleanup and publication before COPY can acquire foreign-key row locks.
    // The exact lease row remains unlocked during the bulk stream so
    // heartbeats can renew it. The non-locking check below is stable against
    // takeover because acquisition shares the operation advisory lock, and a
    // final row-locking check still runs immediately before the ready
    // transition.
    lock_generation_mutation(connection, input.schema, input.fence).await?;
    check_generation_fence(connection, input.schema, input.fence).await?;
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema: &quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            sequence: input.generation.sequence(),
            required: GenerationState::Staging,
        },
        FenceCheck::Observe,
    )
    .await?;
    copy_and_validate_generation(
        connection,
        CopyAndValidateInput {
            schema: input.schema,
            generation: input.generation,
            facts: input.facts,
            metrics: input.metrics,
            progress: input.progress,
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    rebuild_generation_search_relation(
        connection,
        GenerationSearchBuild {
            schema: input.schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            content_digest: &content_digest,
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    carry_forward_unchanged_generation_evidence(
        connection,
        GenerationEvidenceCarry {
            quoted_schema: &quoted_schema,
            generation: input.generation,
            content_digest: &content_digest,
            digest_version,
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    analyze_copied_relations(connection, input.schema).await?;
    advance_prepare_progress(input.progress);
    mark_generation_ready(
        connection,
        ReadyTransition {
            schema: input.schema,
            generation: input.generation,
            fence: input.fence,
            content_digest: &content_digest,
            digest_version,
            counts: fact_counts,
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    Ok(())
}

async fn prepare_spilled_transaction(
    connection: &mut PgConnection,
    input: SpilledPrepareTransactionInput<'_>,
) -> Result<(), StorageError> {
    let quoted_schema = crate::database::quoted_schema(input.schema);
    let validation_started = Instant::now();
    validate_spilled_prepare_authority(connection, &input, &quoted_schema).await?;
    if let Some(metrics) = input.metrics {
        metrics.record_relation_validation(validation_started.elapsed());
    }
    finalize_spilled_prepare(connection, &input, &quoted_schema).await
}

async fn validate_spilled_prepare_authority(
    connection: &mut PgConnection,
    input: &SpilledPrepareTransactionInput<'_>,
    quoted_schema: &str,
) -> Result<(), StorageError> {
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            sequence: input.generation.sequence(),
            required: GenerationState::Staging,
        },
        FenceCheck::Observe,
    )
    .await?;
    lock_generation_mutation(connection, input.schema, input.fence).await?;
    check_generation_fence(connection, input.schema, input.fence).await?;
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            sequence: input.generation.sequence(),
            required: GenerationState::Staging,
        },
        FenceCheck::Observe,
    )
    .await?;
    let spill_sql = format!(
        r#"SELECT phase
            FROM {quoted_schema}."native_generation_spills"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND generation_sequence = $3
            FOR UPDATE"#
    );
    let phase = audited_query(spill_sql)
        .bind(input.generation.project_id().as_str())
        .bind(input.generation.generation_id().as_str())
        .bind(input.generation.sequence())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-spilled-read-run"))?
        .ok_or(StorageError::GenerationNotFound)?
        .try_get::<String, _>(0)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "native_generation_spill_phase",
        })?;
    if phase != "canonicalized" {
        return Err(StorageError::InvalidGenerationTransition {
            actual: phase,
            requested: "canonicalized",
        });
    }
    let counts = canonical_fact_counts(
        connection,
        crate::spill::SpillScope {
            schema: quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
        },
    )
    .await?;
    if counts != input.digest.counts() {
        return Err(StorageError::GenerationSpillConflict);
    }
    Ok(())
}

async fn finalize_spilled_prepare(
    connection: &mut PgConnection,
    input: &SpilledPrepareTransactionInput<'_>,
    quoted_schema: &str,
) -> Result<(), StorageError> {
    advance_prepare_progress(input.progress);
    rebuild_generation_search_relation(
        connection,
        GenerationSearchBuild {
            schema: input.schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            content_digest: input.digest.digest(),
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    carry_forward_unchanged_generation_evidence(
        connection,
        GenerationEvidenceCarry {
            quoted_schema,
            generation: input.generation,
            content_digest: input.digest.digest(),
            digest_version: GenerationDigestVersion::CURRENT,
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    analyze_copied_relations(connection, input.schema).await?;
    advance_prepare_progress(input.progress);
    let delete_spill = format!(
        r#"DELETE FROM {quoted_schema}."native_generation_spills"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#
    );
    let deleted = audited_query(delete_spill)
        .bind(input.generation.project_id().as_str())
        .bind(input.generation.generation_id().as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-spilled-cleanup"))?;
    if deleted.rows_affected() != 1 {
        return Err(StorageError::GenerationSpillConflict);
    }
    mark_generation_ready(
        connection,
        ReadyTransition {
            schema: input.schema,
            generation: input.generation,
            fence: input.fence,
            content_digest: input.digest.digest(),
            digest_version: GenerationDigestVersion::CURRENT,
            counts: input.digest.counts(),
        },
    )
    .await?;
    advance_prepare_progress(input.progress);
    Ok(())
}

fn advance_prepare_progress(progress: Option<&PrepareGenerationProgress>) {
    if let Some(progress) = progress {
        progress.advance();
    }
}

async fn mark_generation_ready(
    connection: &mut PgConnection,
    input: ReadyTransition<'_>,
) -> Result<(), StorageError> {
    lock_generation_fence(
        connection,
        FenceValidation::new(
            input.schema,
            input.fence,
            FenceCheck::HeartbeatCompatibleLock,
        ),
    )
    .await?;
    let quoted_schema = crate::database::quoted_schema(input.schema);
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema: &quoted_schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
            sequence: input.generation.sequence(),
            required: GenerationState::Staging,
        },
        FenceCheck::Lock,
    )
    .await?;
    let ready_sql = format!(
        r#"UPDATE {quoted_schema}."index_generations"
            SET state = 'ready', content_digest = $3, content_digest_version = $4,
                ready_at = clock_timestamp(),
                fact_files = $5, fact_symbols = $6, fact_edges = $7,
                fact_references = $8, fact_numerical_sites = $9, fact_documents = $10,
                fact_source_bytes = COALESCE((
                    SELECT sum(files.byte_size) FROM {quoted_schema}."files" AS files
                    WHERE files.project_id = CAST($1 AS uuid)
                      AND files.generation_id = CAST($2 AS uuid)
                ), 0)::bigint
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND state = 'staging'"#
    );
    let count = |rows: u64| {
        i64::try_from(rows).map_err(|_| StorageError::CorruptStoredValue {
            field: "generation_fact_count",
        })
    };
    let result = audited_query(ready_sql)
        .bind(input.generation.project_id().as_str())
        .bind(input.generation.generation_id().as_str())
        .bind(input.content_digest.as_str())
        .bind(input.digest_version.database_value())
        .bind(count(input.counts.files)?)
        .bind(count(input.counts.symbols)?)
        .bind(count(input.counts.edges)?)
        .bind(count(input.counts.references)?)
        .bind(count(input.counts.numerical_sites)?)
        .bind(count(input.counts.documents)?)
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("mark-generation-ready"))?;
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(StorageError::InvalidGenerationTransition {
            actual: "changed concurrently".to_owned(),
            requested: GenerationState::Ready.as_str(),
        })
    }
}

async fn analyze_copied_relations(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
) -> Result<(), StorageError> {
    let quoted_schema = crate::database::quoted_schema(schema);
    for (relation, columns) in COPIED_RELATION_PLANNER_COLUMNS {
        let statement = format!(r#"ANALYZE {quoted_schema}."{relation}" ({columns})"#);
        audited_query(statement)
            .execute(&mut *connection)
            .await
            .map_err(|_| database_error("analyze-copied-relations"))?;
    }
    Ok(())
}

async fn copy_and_validate_generation(
    connection: &mut PgConnection,
    input: CopyAndValidateInput<'_>,
) -> Result<(), StorageError> {
    let copy_started = Instant::now();
    let CopyGenerationAttempt {
        durations,
        result: copy_result,
    } = copy_generation_facts(
        connection,
        CopyGenerationRequest {
            context: CopyGenerationContext {
                schema: input.schema.clone(),
                project_id: input.generation.project_id().clone(),
                generation_id: input.generation.generation_id().clone(),
            },
            prepare_progress: input.progress,
        },
        input.facts,
    )
    .await;
    if let Some(metrics) = input.metrics {
        metrics.record_copy(copy_started.elapsed(), durations);
    }
    copy_result?;

    let validation_started = Instant::now();
    let validation_result = validate_copied_relations(
        connection,
        CopiedRelationCheck {
            schema: input.schema,
            project_id: input.generation.project_id(),
            generation_id: input.generation.generation_id(),
        },
    )
    .await;
    if let Some(metrics) = input.metrics {
        metrics.record_relation_validation(validation_started.elapsed());
    }
    validation_result
}

async fn validate_copied_relations(
    connection: &mut PgConnection,
    input: CopiedRelationCheck<'_>,
) -> Result<(), StorageError> {
    let quoted_schema = crate::database::quoted_schema(input.schema);
    // Fresh COPY targets do not have useful planner statistics yet. Set
    // difference keeps this check to bounded generation scans; equivalent
    // anti-joins can degrade into a nested-loop pass per relation row.
    let sql = format!(
        r#"WITH required_symbols(symbol_id) AS (
                SELECT source_symbol_id
                FROM {quoted_schema}."edges"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT target_symbol_id
                FROM {quoted_schema}."edges"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT owner_symbol_id
                FROM {quoted_schema}."references"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND owner_symbol_id IS NOT NULL
                UNION ALL
                SELECT target_symbol_id
                FROM {quoted_schema}."references"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND target_symbol_id IS NOT NULL
                UNION ALL
                SELECT owner_symbol_id
                FROM {quoted_schema}."numerical_sites"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND owner_symbol_id IS NOT NULL
                UNION ALL
                SELECT symbol_id
                FROM {quoted_schema}."search_documents"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND symbol_id IS NOT NULL
            ), missing_symbols AS (
                SELECT symbol_id FROM required_symbols
                EXCEPT
                SELECT symbol_id
                FROM {quoted_schema}."symbols"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
            ), required_files(file_id) AS (
                SELECT file_id
                FROM {quoted_schema}."references"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT file_id
                FROM {quoted_schema}."numerical_sites"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT file_id
                FROM {quoted_schema}."search_documents"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND file_id IS NOT NULL
            ), missing_files AS (
                SELECT file_id FROM required_files
                EXCEPT
                SELECT file_id
                FROM {quoted_schema}."files"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
            )
            SELECT EXISTS (
                SELECT 1 FROM missing_symbols
                UNION ALL
                SELECT 1 FROM missing_files
            ) AS has_invalid_relations"#,
    );
    let row = audited_query(sql)
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("verify-copied-relations"))?;
    let has_invalid_relations = row
        .try_get::<bool, _>("has_invalid_relations")
        .map_err(|_| database_error("verify-copied-relations"))?;
    if has_invalid_relations {
        Err(database_error("verify-copied-relations"))
    } else {
        Ok(())
    }
}

async fn publish_transaction(
    connection: &mut PgConnection,
    input: PublishTransactionInput<'_>,
) -> Result<(), StorageError> {
    let generation = input.generation;
    let fence = input.fence;
    if !fence_matches_generation(fence, generation.project_id(), generation.generation_id()) {
        return Err(StorageError::LeaseFenceLost);
    }
    // Publication, retention, purge, and lease acquisition all serialize on
    // the project operation lock first. Taking publication before operation
    // creates an ABBA deadlock with retention's operation -> publication order.
    lock_project_mutation(connection, input.schema, generation.project_id()).await?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "cartograph-v2-publish:{}:{}",
            input.schema.as_str(),
            generation.project_id()
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("publish-lock"))?;
    let quoted_schema = crate::database::quoted_schema(input.schema);
    lock_generation_fence(
        connection,
        FenceValidation::new(input.schema, fence, FenceCheck::Lock),
    )
    .await?;
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema: &quoted_schema,
            project_id: generation.project_id(),
            generation_id: generation.generation_id(),
            sequence: generation.sequence(),
            required: GenerationState::Ready,
        },
        FenceCheck::Lock,
    )
    .await?;
    require_generation_search_relation(
        connection,
        input.schema,
        CurrentGenerationLookup::new(generation.project_id(), generation.generation_id()),
    )
    .await?;

    let current_sequence =
        lock_current_generation_sequence(connection, &quoted_schema, generation.project_id())
            .await?;
    if let Some(current_sequence) = current_sequence
        && generation.sequence() <= current_sequence
    {
        return Err(StorageError::StaleGeneration {
            candidate_sequence: generation.sequence(),
            current_sequence,
        });
    }

    let supersede_sql = format!(
        r#"UPDATE {quoted_schema}."index_generations"
            SET state = 'superseded'
            WHERE project_id = CAST($1 AS uuid) AND state = 'current'"#
    );
    audited_query(supersede_sql)
        .bind(generation.project_id().as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("supersede-generation"))?;

    let publish_sql = format!(
        r#"UPDATE {quoted_schema}."index_generations"
            SET state = 'current', published_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND state = 'ready'"#
    );
    let published = audited_query(publish_sql)
        .bind(generation.project_id().as_str())
        .bind(generation.generation_id().as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("publish-generation"))?;
    if published.rows_affected() != 1 {
        return Err(StorageError::InvalidGenerationTransition {
            actual: "changed concurrently".to_owned(),
            requested: GenerationState::Current.as_str(),
        });
    }

    let project_sql = format!(
        r#"UPDATE {quoted_schema}."projects"
            SET current_generation_id = CAST($2 AS uuid), updated_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)"#
    );
    let project = audited_query(project_sql)
        .bind(generation.project_id().as_str())
        .bind(generation.generation_id().as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("publish-project-pointer"))?;
    if project.rows_affected() != 1 {
        return Err(StorageError::GenerationNotFound);
    }
    delete_generation_fence(connection, &quoted_schema, fence).await?;
    Ok(())
}

/// Re-link content-addressed bulk evidence while the new generation is staged.
///
/// Symbol and document identities are stable across generations, but every
/// structural fact table is generation-fenced. Carrying rows forward inside
/// the already-bounded prepare transaction prevents a successful re-index from
/// eroding embeddings or coverage for unchanged code without making the short
/// publication critical section scale with the corpus. Exact digest or
/// rendered-document equality is required; changed source is deliberately left
/// without derived evidence so callers cannot mistake it for fresh data.
async fn carry_forward_unchanged_generation_evidence(
    connection: &mut PgConnection,
    input: GenerationEvidenceCarry<'_>,
) -> Result<(), StorageError> {
    let GenerationEvidenceCarry {
        quoted_schema,
        generation,
        content_digest,
        digest_version,
    } = input;
    let project_id = generation.project_id().as_str();
    let generation_id = generation.generation_id().as_str();

    let embeddings = include_str!("sql/generation_carry_embeddings.sql")
        .replace("{quoted_schema}", quoted_schema);
    audited_query(embeddings)
        .bind(project_id)
        .bind(generation_id)
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-carry-embeddings"))?;

    let coverage =
        include_str!("sql/generation_carry_coverage.sql").replace("{quoted_schema}", quoted_schema);
    audited_query(coverage)
        .bind(project_id)
        .bind(generation_id)
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-carry-coverage"))?;

    // A materialized Top-K is valid only when the complete logical fact set is
    // identical. Partial carry-forward would preserve stale ranks after a
    // neighbor changed; omitting the build metadata intentionally makes reads
    // fall back to authoritative live pgvector search instead.
    let similarity_edges = include_str!("sql/generation_carry_similarity_edges.sql")
        .replace("{quoted_schema}", quoted_schema);
    audited_query(similarity_edges)
        .bind(project_id)
        .bind(generation_id)
        .bind(content_digest.as_str())
        .bind(digest_version.database_value())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-carry-similarity-edges"))?;
    let similarity_builds = include_str!("sql/generation_carry_similarity_builds.sql")
        .replace("{quoted_schema}", quoted_schema);
    audited_query(similarity_builds)
        .bind(project_id)
        .bind(generation_id)
        .bind(content_digest.as_str())
        .bind(digest_version.database_value())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("prepare-carry-similarity-builds"))?;
    Ok(())
}

struct GenerationEvidenceCarry<'a> {
    quoted_schema: &'a str,
    generation: &'a StagedGeneration,
    content_digest: &'a ContentDigest,
    digest_version: GenerationDigestVersion,
}

async fn lock_generation_fence(
    connection: &mut PgConnection,
    input: FenceValidation<'_>,
) -> Result<(), StorageError> {
    lock_generation_mutation(connection, input.schema, input.fence).await?;
    require_generation_fence(connection, input).await
}

async fn check_generation_fence(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    fence: &LeaseFence,
) -> Result<(), StorageError> {
    require_generation_fence(
        connection,
        FenceValidation::new(schema, fence, FenceCheck::Observe),
    )
    .await
}

/// Exact lease fence and staging generation one fence check applies to.
#[derive(Clone, Copy)]
pub(crate) struct StagingFenceTarget<'target> {
    pub(crate) fence: &'target LeaseFence,
    pub(crate) project_id: &'target ProjectId,
    pub(crate) generation_id: &'target GenerationId,
    pub(crate) sequence: i64,
}

pub(crate) async fn lock_staging_generation_fence(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    target: StagingFenceTarget<'_>,
) -> Result<(), StorageError> {
    let StagingFenceTarget {
        fence,
        project_id,
        generation_id,
        sequence,
    } = target;
    if !fence_matches_generation(fence, project_id, generation_id) {
        return Err(StorageError::LeaseFenceLost);
    }
    lock_generation_mutation(connection, schema, fence).await?;
    check_generation_fence(connection, schema, fence).await?;
    let quoted_schema = crate::database::quoted_schema(schema);
    validate_generation_state(
        connection,
        GenerationStateRequirement {
            quoted_schema: &quoted_schema,
            project_id,
            generation_id,
            sequence,
            required: GenerationState::Staging,
        },
        FenceCheck::Observe,
    )
    .await
}

pub(crate) async fn check_staging_generation_fence(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    target: StagingFenceTarget<'_>,
) -> Result<(), StorageError> {
    let StagingFenceTarget {
        fence,
        project_id,
        generation_id,
        sequence,
    } = target;
    if !fence_matches_generation(fence, project_id, generation_id) {
        return Err(StorageError::LeaseFenceLost);
    }
    let quoted_schema = crate::database::quoted_schema(schema);
    let sql = format!(
        r#"SELECT 1
            FROM {quoted_schema}."project_operation_leases" AS leases
            INNER JOIN {quoted_schema}."index_generations" AS generations
              ON generations.project_id = leases.project_id
             AND generations.generation_id = leases.generation_id
            WHERE leases.project_id = CAST($1 AS uuid)
              AND leases.operation = $2
              AND leases.lease_id = CAST($3 AS uuid)
              AND leases.generation_id = CAST($4 AS uuid)
              AND leases.expires_at > clock_timestamp()
              AND generations.generation_sequence = $5
              AND generations.state = 'staging'"#
    );
    let live = audited_query(sql)
        .bind(project_id.as_str())
        .bind(fence.target().operation().as_str())
        .bind(fence.lease_id().as_str())
        .bind(generation_id.as_str())
        .bind(sequence)
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("check-staging-generation-fence"))?
        .is_some();
    if live {
        Ok(())
    } else {
        Err(StorageError::LeaseFenceLost)
    }
}

async fn require_generation_fence(
    connection: &mut PgConnection,
    input: FenceValidation<'_>,
) -> Result<(), StorageError> {
    let generation_id = input
        .fence
        .target()
        .generation_id()
        .ok_or(StorageError::LeaseFenceLost)?;
    let quoted_schema = crate::database::quoted_schema(input.schema);
    let row_lock = input.check.row_lock();
    let sql = format!(
        r#"SELECT 1 FROM {quoted_schema}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND operation = $2
              AND lease_id = CAST($3 AS uuid)
              AND generation_id = CAST($4 AS uuid)
              AND expires_at > clock_timestamp(){row_lock}"#
    );
    let row = audited_query(sql)
        .bind(input.fence.target().project_id().as_str())
        .bind(input.fence.target().operation().as_str())
        .bind(input.fence.lease_id().as_str())
        .bind(generation_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error(input.check.operation()))?;
    if row.is_some() {
        Ok(())
    } else {
        Err(StorageError::LeaseFenceLost)
    }
}

async fn lock_generation_mutation(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    fence: &LeaseFence,
) -> Result<(), StorageError> {
    let generation_id = fence
        .target()
        .generation_id()
        .ok_or(StorageError::LeaseFenceLost)?;
    lock_project_mutation(connection, schema, fence.target().project_id()).await?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{GENERATION_LOCK_NAMESPACE}:{}:{}:{}",
            schema.as_str(),
            fence.target().project_id(),
            generation_id
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("lock-generation-identity"))?;
    Ok(())
}

async fn lock_project_mutation(
    connection: &mut PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    project_id: &ProjectId,
) -> Result<(), StorageError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(project_lock_key(schema, project_id))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("lock-generation-operation"))?;
    Ok(())
}

async fn delete_generation_fence(
    connection: &mut PgConnection,
    quoted_schema: &str,
    fence: &LeaseFence,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"DELETE FROM {quoted_schema}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND operation = $2
              AND lease_id = CAST($3 AS uuid)"#
    );
    let deleted = audited_query(sql)
        .bind(fence.target().project_id().as_str())
        .bind(fence.target().operation().as_str())
        .bind(fence.lease_id().as_str())
        .execute(connection)
        .await
        .map_err(|_| database_error("delete-generation-fence"))?;
    if deleted.rows_affected() == 1 {
        Ok(())
    } else {
        Err(StorageError::LeaseFenceLost)
    }
}

fn fence_matches_generation(
    fence: &LeaseFence,
    project_id: &ProjectId,
    generation_id: &GenerationId,
) -> bool {
    fence.target().project_id() == project_id
        && fence.target().generation_id() == Some(generation_id)
}

fn decode_reconciliation(
    row: &PgRow,
    fence: &LeaseFence,
) -> Result<OperationReconciliation, StorageError> {
    let (generation, expected_generation_id) = decode_reconciled_generation(row, fence)?;
    let lease = decode_reconciled_lease(row, fence, &expected_generation_id)?;
    Ok(OperationReconciliation { generation, lease })
}

fn decode_reconciled_generation(
    row: &PgRow,
    fence: &LeaseFence,
) -> Result<(ObservedGeneration, String), StorageError> {
    let project_id = fence.target().project_id().clone();
    let generation_id = fence
        .target()
        .generation_id()
        .ok_or(StorageError::LeaseFenceLost)?
        .clone();
    let expected_generation_id = generation_id.as_str().to_owned();
    let state = parse_generation_state(row, RECONCILE_STATE_COLUMN)?;
    let sequence = parse_generation_sequence(row, RECONCILE_SEQUENCE_COLUMN)?;
    let (content_digest, digest_version) = parse_reconciled_digest(row)?;
    let generation = match state {
        GenerationState::Staging => {
            if content_digest.is_some() || digest_version.is_some() {
                return Err(corrupt("content_digest_version"));
            }
            ObservedGeneration::Staged(StagedGeneration {
                project_id,
                generation_id,
                sequence,
            })
        }
        GenerationState::Ready => ObservedGeneration::Ready(ReadyGeneration {
            project_id,
            generation_id,
            sequence,
            content_digest: content_digest.ok_or_else(|| corrupt("content_digest"))?,
            digest_version: digest_version.ok_or_else(|| corrupt("content_digest_version"))?,
        }),
        GenerationState::Current => {
            let current = row
                .try_get::<Option<String>, _>(RECONCILE_CURRENT_COLUMN)
                .map_err(|_| corrupt("current_generation_id"))?;
            if current.as_deref() != Some(generation_id.as_str()) {
                return Err(corrupt("current_generation_id"));
            }
            ObservedGeneration::Current(CurrentGeneration {
                project_id,
                generation_id,
                sequence,
                content_digest: content_digest.ok_or_else(|| corrupt("content_digest"))?,
                digest_version: digest_version.ok_or_else(|| corrupt("content_digest_version"))?,
            })
        }
        GenerationState::Retiring => {
            return Err(StorageError::InvalidGenerationTransition {
                actual: "retiring".to_owned(),
                requested: "reconcile",
            });
        }
        GenerationState::Failed => {
            if content_digest.is_some() != digest_version.is_some() {
                return Err(corrupt("content_digest_version"));
            }
            ObservedGeneration::Failed
        }
        GenerationState::Superseded => {
            if content_digest.is_none() || digest_version.is_none() {
                return Err(corrupt("content_digest_version"));
            }
            ObservedGeneration::Superseded
        }
    };
    Ok((generation, expected_generation_id))
}

fn parse_reconciled_digest(
    row: &PgRow,
) -> Result<(Option<ContentDigest>, Option<GenerationDigestVersion>), StorageError> {
    let raw_digest = row
        .try_get::<Option<String>, _>(RECONCILE_DIGEST_COLUMN)
        .map_err(|_| corrupt("content_digest"))?;
    let content_digest = raw_digest
        .map(|raw| ContentDigest::parse(&raw).map_err(|_| corrupt("content_digest")))
        .transpose()?;
    let digest_version = row
        .try_get::<Option<i16>, _>(RECONCILE_DIGEST_VERSION_COLUMN)
        .map_err(|_| corrupt("content_digest_version"))?
        .map(|value| {
            GenerationDigestVersion::from_database_value(value)
                .map_err(|_| corrupt("content_digest_version"))
        })
        .transpose()?;
    Ok((content_digest, digest_version))
}

fn decode_reconciled_lease(
    row: &PgRow,
    fence: &LeaseFence,
    expected_generation_id: &str,
) -> Result<ObservedLease, StorageError> {
    let raw_lease_id = row
        .try_get::<Option<String>, _>(RECONCILE_LEASE_ID_COLUMN)
        .map_err(|_| corrupt("lease_id"))?;
    let lease = match raw_lease_id {
        None => ObservedLease::Released,
        Some(raw_lease_id) => {
            let lease_id = cartograph_domain::LeaseId::parse(&raw_lease_id)
                .map_err(|_| corrupt("lease_id"))?;
            let unexpired = row
                .try_get::<Option<bool>, _>(RECONCILE_LEASE_UNEXPIRED_COLUMN)
                .map_err(|_| corrupt("lease_expiry"))?
                .unwrap_or(false);
            let lease_generation = row
                .try_get::<Option<String>, _>(RECONCILE_LEASE_GENERATION_COLUMN)
                .map_err(|_| corrupt("lease_generation_id"))?;
            if lease_id == *fence.lease_id()
                && unexpired
                && lease_generation.as_deref() == Some(expected_generation_id)
            {
                ObservedLease::Owned
            } else {
                ObservedLease::Lost
            }
        }
    };
    Ok(lease)
}

async fn lock_current_generation_sequence(
    connection: &mut PgConnection,
    quoted_schema: &str,
    project_id: &ProjectId,
) -> Result<Option<i64>, StorageError> {
    let sql = format!(
        r#"SELECT generations.generation_sequence
            FROM {quoted_schema}."projects" AS projects
            LEFT JOIN {quoted_schema}."index_generations" AS generations
              ON generations.project_id = projects.project_id
             AND generations.generation_id = projects.current_generation_id
            WHERE projects.project_id = CAST($1 AS uuid)
            FOR UPDATE OF projects"#
    );
    let row = audited_query(sql)
        .bind(project_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("lock-current-generation"))?
        .ok_or(StorageError::GenerationNotFound)?;
    row.try_get::<Option<i64>, _>(0)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "generation_sequence",
        })
}

async fn validate_generation_state(
    connection: &mut PgConnection,
    requirement: GenerationStateRequirement<'_>,
    check: FenceCheck,
) -> Result<(), StorageError> {
    let (lock_clause, operation) = match check {
        FenceCheck::Observe => ("", "check-generation"),
        FenceCheck::HeartbeatCompatibleLock => (" FOR KEY SHARE", "protect-generation"),
        FenceCheck::Lock => (" FOR UPDATE", "lock-generation"),
    };
    let sql = format!(
        r#"SELECT state, generation_sequence FROM {schema}."index_generations"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid){lock_clause}"#,
        schema = requirement.quoted_schema,
    );
    let row = audited_query(sql)
        .bind(requirement.project_id.as_str())
        .bind(requirement.generation_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error(operation))?
        .ok_or(StorageError::GenerationNotFound)?;
    validate_generation_state_row(&row, &requirement)
}

fn validate_generation_state_row(
    row: &PgRow,
    requirement: &GenerationStateRequirement<'_>,
) -> Result<(), StorageError> {
    let actual = row
        .try_get::<String, _>(0)
        .map_err(|_| StorageError::CorruptStoredValue { field: "state" })?;
    let sequence = parse_generation_sequence(row, 1)?;
    if sequence != requirement.sequence {
        return Err(StorageError::CorruptStoredValue {
            field: "generation_sequence",
        });
    }
    if actual != requirement.required.as_str() {
        return Err(StorageError::InvalidGenerationTransition {
            actual,
            requested: requirement.required.as_str(),
        });
    }
    Ok(())
}

fn recoverable_parts(
    generation: &RecoverableGeneration,
) -> (&ProjectId, &GenerationId, i64, GenerationState) {
    match generation {
        RecoverableGeneration::Staged(generation) => (
            generation.project_id(),
            generation.generation_id(),
            generation.sequence(),
            GenerationState::Staging,
        ),
        RecoverableGeneration::Ready(generation) => (
            generation.project_id(),
            generation.generation_id(),
            generation.sequence(),
            GenerationState::Ready,
        ),
    }
}

fn into_recoverable_parts(
    generation: RecoverableGeneration,
) -> (ProjectId, GenerationId, i64, GenerationState) {
    match generation {
        RecoverableGeneration::Staged(generation) => (
            generation.project_id,
            generation.generation_id,
            generation.sequence,
            GenerationState::Staging,
        ),
        RecoverableGeneration::Ready(generation) => (
            generation.project_id,
            generation.generation_id,
            generation.sequence,
            GenerationState::Ready,
        ),
    }
}

fn validate_bounded_text(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(StorageError::InvalidInput { field });
    }
    Ok(())
}

fn validate_run_excludes(patterns: &[String]) -> Result<(), StorageError> {
    if patterns.len() > MAX_RUN_EXCLUDES {
        return Err(StorageError::InvalidInput {
            field: "run_excludes",
        });
    }
    let mut total = 0_usize;
    for pattern in patterns {
        validate_bounded_text(pattern, "run_excludes", MAX_RUN_EXCLUDE_BYTES)?;
        total = total
            .checked_add(pattern.len())
            .ok_or(StorageError::InvalidInput {
                field: "run_excludes",
            })?;
        if total > MAX_RUN_EXCLUDES_TOTAL_BYTES {
            return Err(StorageError::InvalidInput {
                field: "run_excludes",
            });
        }
    }
    Ok(())
}

fn parse_project_id(row: &sqlx_postgres::PgRow, index: usize) -> Result<ProjectId, StorageError> {
    let raw = row
        .try_get::<String, _>(index)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "project_id",
        })?;
    ProjectId::parse(&raw).map_err(|_| StorageError::CorruptStoredValue {
        field: "project_id",
    })
}

fn parse_generation_id(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<GenerationId, StorageError> {
    let raw = row
        .try_get::<String, _>(index)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "generation_id",
        })?;
    GenerationId::parse(&raw).map_err(|_| StorageError::CorruptStoredValue {
        field: "generation_id",
    })
}

fn parse_generation_sequence(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<i64, StorageError> {
    let sequence = row
        .try_get::<i64, _>(index)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "generation_sequence",
        })?;
    if sequence <= 0 {
        return Err(StorageError::CorruptStoredValue {
            field: "generation_sequence",
        });
    }
    Ok(sequence)
}

fn parse_generation_state(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<GenerationState, StorageError> {
    let raw = row
        .try_get::<String, _>(index)
        .map_err(|_| StorageError::CorruptStoredValue { field: "state" })?;
    match raw.as_str() {
        "staging" => Ok(GenerationState::Staging),
        "ready" => Ok(GenerationState::Ready),
        "current" => Ok(GenerationState::Current),
        "superseded" => Ok(GenerationState::Superseded),
        "failed" => Ok(GenerationState::Failed),
        "retiring" => Ok(GenerationState::Retiring),
        _ => Err(StorageError::CorruptStoredValue { field: "state" }),
    }
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

const fn corrupt(field: &'static str) -> StorageError {
    StorageError::CorruptStoredValue { field }
}

#[cfg(test)]
mod tests {
    use super::PrepareGenerationProgress;

    #[test]
    fn prepare_progress_is_monotonic_and_shared_across_clones() {
        let progress = PrepareGenerationProgress::new();
        let observed = progress.clone();
        assert_eq!(observed.sequence(), 0);
        progress.advance();
        progress.advance();
        assert_eq!(observed.sequence(), 2);
    }
}
