use std::{future::Future, time::Duration};

use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use crate::{CartographDatabase, database::quoted_schema, leases::schema_maintenance_lock_key};

const MAXIMUM_COMPACTION_INDEXES: u16 = 64;
const MAXIMUM_COMPACTION_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MINIMUM_INDEX_BYTES: u64 = 1024 * 1024;
const MAXIMUM_STATEMENT_TIMEOUT: Duration = Duration::from_hours(24);
const HEADROOM_MULTIPLIER: u64 = 2;
const HEADROOM_ALLOWANCE_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_INVALID_ARTIFACTS: usize = 64;
const COMPACTION_LOCK_NAMESPACE: &str = "cartograph-v2-online-compaction";
const AUTOMATIC_MAXIMUM_INDEXES: u16 = 32;
const AUTOMATIC_MAXIMUM_CANDIDATE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const AUTOMATIC_MINIMUM_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const AUTOMATIC_STATEMENT_TIMEOUT: Duration = Duration::from_mins(15);
const MAXIMUM_HEAP_COMPACTION_RELATIONS: u16 = 32;
const AUTOMATIC_HEAP_COMPACTION_RELATIONS: u16 = 8;
const AUTOMATIC_MINIMUM_RECLAIMABLE_BYTES: u64 = 64 * 1024 * 1024;
const HEAP_HEADROOM_ALLOWANCE_BYTES: u64 = 64 * 1024 * 1024;
/// PostgreSQL's B-tree build fill factor for leaf pages when an index sets none.
const BTREE_DEFAULT_FILLFACTOR: f64 = 90.0;
const HEAP_COMPACTION_TABLES: &[&str] = &[
    "agent_artifacts",
    "document_embeddings",
    "edges",
    "file_cochanges",
    "file_history",
    "files",
    "index_generations",
    "mcp_sessions",
    "mcp_tool_calls",
    "native_generation_spill_batches",
    "native_generation_spill_documents",
    "native_generation_spill_edges",
    "native_generation_spill_files",
    "native_generation_spill_numerical_sites",
    "native_generation_spill_references",
    "native_generation_spill_rows",
    "native_generation_spill_symbols",
    "native_generation_spills",
    "native_parse_cache",
    "numerical_sites",
    "references",
    "search_documents",
    "structural_findings",
    "structural_finding_runs",
    "symbol_similarity_edges",
    "symbols",
];

/// One exact B-tree candidate selected for an online rebuild.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCompactionCandidate {
    /// Index for this record.
    pub index: String,
    /// Table for this record.
    pub table: String,
    /// Allocated byte size.
    pub bytes: u64,
    /// Bytes a rebuild is expected to return: leaf pages above the density a
    /// rebuild packs to, plus empty and deleted pages. `None` when
    /// `pgstattuple` is unavailable and selection fell back to size alone.
    pub estimated_reclaimable_bytes: Option<u64>,
}

/// Invalid concurrent-reindex artifact that requires operator review. Cartograph
/// never drops these automatically because `_ccold` and `_ccnew` have different
/// PostgreSQL recovery semantics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidIndexArtifact {
    /// Index for this record.
    pub index: String,
    /// Table for this record.
    pub table: String,
    /// Allocated byte size.
    pub bytes: u64,
    /// Whether the catalog entry is valid.
    pub valid: bool,
    /// Whether the catalog entry is ready for queries.
    pub ready: bool,
}

/// Hard work and deadline bounds for one resumable online compaction call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageCompactionPolicy {
    maximum_indexes: u16,
    maximum_candidate_bytes: u64,
    minimum_index_bytes: u64,
    minimum_reclaimable_bytes: u64,
    statement_timeout: Duration,
}

/// Caller-selected online-compaction bounds validated as one typed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageCompactionPolicyInput {
    /// Maximum number of indexes permitted by this request.
    pub maximum_indexes: u16,
    /// Number of bytes used by the maximum candidate.
    pub maximum_candidate_bytes: u64,
    /// Number of bytes used by the minimum index.
    pub minimum_index_bytes: u64,
    /// Minimum estimated reclaimable bytes for one measured index.
    pub minimum_reclaimable_bytes: u64,
    /// Statement timeout for this record.
    pub statement_timeout: Duration,
}

impl StorageCompactionPolicy {
    /// Creates a validated storage compaction policy.
    ///
    /// # Errors
    ///
    /// Returns an error if index-count, candidate-byte, minimum-size,
    /// minimum-reclaim, or statement-timeout bounds are zero, inconsistent, or
    /// above their maxima.
    pub fn new(input: StorageCompactionPolicyInput) -> Result<Self, StorageCompactionError> {
        if input.maximum_indexes == 0 || input.maximum_indexes > MAXIMUM_COMPACTION_INDEXES {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.maximum_candidate_bytes == 0
            || input.maximum_candidate_bytes > MAXIMUM_COMPACTION_BYTES
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.minimum_index_bytes < MINIMUM_INDEX_BYTES
            || input.minimum_index_bytes > input.maximum_candidate_bytes
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.minimum_reclaimable_bytes == 0
            || input.minimum_reclaimable_bytes > input.maximum_candidate_bytes
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.statement_timeout.is_zero() || input.statement_timeout > MAXIMUM_STATEMENT_TIMEOUT
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        Ok(Self {
            maximum_indexes: input.maximum_indexes,
            maximum_candidate_bytes: input.maximum_candidate_bytes,
            minimum_index_bytes: input.minimum_index_bytes,
            minimum_reclaimable_bytes: input.minimum_reclaimable_bytes,
            statement_timeout: input.statement_timeout,
        })
    }

    #[must_use]
    /// Returns the conservative policy used by automatic dry-run planning.
    pub const fn automatic_plan() -> Self {
        Self {
            maximum_indexes: AUTOMATIC_MAXIMUM_INDEXES,
            maximum_candidate_bytes: AUTOMATIC_MAXIMUM_CANDIDATE_BYTES,
            minimum_index_bytes: AUTOMATIC_MINIMUM_INDEX_BYTES,
            minimum_reclaimable_bytes: AUTOMATIC_MINIMUM_RECLAIMABLE_BYTES,
            statement_timeout: AUTOMATIC_STATEMENT_TIMEOUT,
        }
    }
}

impl Default for StorageCompactionPolicy {
    fn default() -> Self {
        Self::automatic_plan()
    }
}

/// Dry-run plan with conservative headroom and invalid-artifact evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCompactionPlan {
    /// Bounded candidates included in this result.
    pub candidates: Vec<StorageCompactionCandidate>,
    /// Bounded invalid artifacts included in this result.
    pub invalid_artifacts: Vec<InvalidIndexArtifact>,
    /// Total invalid concurrent-index artifacts observed before paging.
    pub invalid_artifact_total: u64,
    /// Whether additional invalid concurrent-index artifacts were omitted.
    pub invalid_artifacts_truncated: bool,
    /// Number of bytes used by the candidate.
    pub candidate_bytes: u64,
    /// Aggregate estimated reclaimable bytes, when measured.
    pub estimated_reclaimable_bytes: Option<u64>,
    /// Whether leaf density was measured with `pgstattuple`. Without it,
    /// candidates are selected by size alone and may already be compact.
    pub reclaim_measured: bool,
    /// Number of bytes used by the required headroom.
    pub required_headroom_bytes: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

/// One measured Cartograph heap relation eligible for an exclusive rewrite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapCompactionCandidate {
    /// Cartograph-owned table name within the configured schema.
    pub table: String,
    /// Allocated main-fork heap bytes.
    pub heap_bytes: u64,
    /// Allocated table, TOAST, and index bytes.
    pub total_bytes: u64,
    /// Exact dead-tuple bytes reported by `pgstattuple_approx`.
    pub dead_tuple_bytes: u64,
    /// Approximate reusable free bytes within the heap.
    pub free_bytes: u64,
    /// Estimated heap and TOAST bytes after a rewrite packs the live tuples.
    pub estimated_rewritten_bytes: u64,
    /// Heap bytes a rewrite is expected to return: current pages beyond the
    /// packed estimate, capped by the measured dead and free bytes.
    pub estimated_reclaimable_bytes: u64,
}

/// Hard work and deadline bounds for one heap-compaction request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapCompactionPolicy {
    maximum_relations: u16,
    maximum_candidate_bytes: u64,
    minimum_reclaimable_bytes: u64,
    statement_timeout: Duration,
}

/// Caller-selected heap-compaction bounds validated as one typed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapCompactionPolicyInput {
    /// Maximum relations considered in one request.
    pub maximum_relations: u16,
    /// Maximum aggregate relation bytes admitted into the request.
    pub maximum_candidate_bytes: u64,
    /// Minimum estimated reclaimable bytes for one relation.
    pub minimum_reclaimable_bytes: u64,
    /// Hard PostgreSQL deadline for each relation rewrite.
    pub statement_timeout: Duration,
}

impl HeapCompactionPolicy {
    /// Creates a validated heap-compaction policy.
    ///
    /// # Errors
    ///
    /// Returns an error when any count, byte, or deadline bound is zero,
    /// inconsistent, or exceeds the hard maintenance ceiling.
    pub fn new(input: HeapCompactionPolicyInput) -> Result<Self, StorageCompactionError> {
        if input.maximum_relations == 0
            || input.maximum_relations > MAXIMUM_HEAP_COMPACTION_RELATIONS
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.maximum_candidate_bytes == 0
            || input.maximum_candidate_bytes > MAXIMUM_COMPACTION_BYTES
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.minimum_reclaimable_bytes == 0
            || input.minimum_reclaimable_bytes > input.maximum_candidate_bytes
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        if input.statement_timeout.is_zero() || input.statement_timeout > MAXIMUM_STATEMENT_TIMEOUT
        {
            return Err(StorageCompactionError::InvalidPolicy);
        }
        Ok(Self {
            maximum_relations: input.maximum_relations,
            maximum_candidate_bytes: input.maximum_candidate_bytes,
            minimum_reclaimable_bytes: input.minimum_reclaimable_bytes,
            statement_timeout: input.statement_timeout,
        })
    }

    /// Conservative default for a read-only heap-reclaim plan.
    #[must_use]
    pub const fn automatic_plan() -> Self {
        Self {
            maximum_relations: AUTOMATIC_HEAP_COMPACTION_RELATIONS,
            maximum_candidate_bytes: AUTOMATIC_MAXIMUM_CANDIDATE_BYTES,
            minimum_reclaimable_bytes: AUTOMATIC_MINIMUM_RECLAIMABLE_BYTES,
            statement_timeout: AUTOMATIC_STATEMENT_TIMEOUT,
        }
    }
}

impl Default for HeapCompactionPolicy {
    fn default() -> Self {
        Self::automatic_plan()
    }
}

/// Read-only heap-reclaim inventory and exclusive-operation warning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapCompactionPlan {
    /// Bounded measured candidates in descending reclaimable-byte order.
    pub candidates: Vec<HeapCompactionCandidate>,
    /// Aggregate allocated bytes admitted into the plan.
    pub candidate_bytes: u64,
    /// Aggregate estimated reclaimable heap bytes.
    pub estimated_reclaimable_bytes: u64,
    /// Conservative free-space requirement for the largest one-table rewrite.
    pub required_headroom_bytes: u64,
    /// Whether eligible candidates were omitted by count or byte bounds.
    pub truncated: bool,
    /// `VACUUM FULL` takes an `ACCESS EXCLUSIVE` lock on each selected table.
    pub requires_access_exclusive: bool,
}

/// Exact size outcome for one completed heap rewrite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapCompactionResult {
    /// Rewritten Cartograph table.
    pub table: String,
    /// Total relation bytes before the rewrite.
    pub bytes_before: u64,
    /// Total relation bytes after the rewrite.
    pub bytes_after: u64,
    /// Bytes returned by this rewrite, saturating at zero.
    pub reclaimed_bytes: u64,
}

/// Applied one-relation-at-a-time heap-compaction report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapCompactionReport {
    /// Exact dry-run plan used for the apply call.
    pub plan: HeapCompactionPlan,
    /// Completed relation rewrites before any bounded stop.
    pub compacted: Vec<HeapCompactionResult>,
    /// Relation at which the operation stopped, when applicable.
    pub stopped_at: Option<String>,
    /// Stable bounded reason for a partial stop.
    pub stop_reason: Option<StorageCompactionStopReason>,
}

/// Why an applied online plan stopped before completing every candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageCompactionStopReason {
    /// Represents the reindex failed storage compaction stop reason.
    ReindexFailed,
    /// `VACUUM FULL` failed or exceeded its statement deadline.
    VacuumFullFailed,
    /// Represents the timeout restore failed storage compaction stop reason.
    TimeoutRestoreFailed,
    /// Represents the advisory unlock failed storage compaction stop reason.
    AdvisoryUnlockFailed,
}

/// Dry-run or applied online B-tree compaction report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCompactionReport {
    /// Plan for this record.
    pub plan: StorageCompactionPlan,
    /// Whether the operation reports changes without applying them.
    pub dry_run: bool,
    /// Bounded reindexed included in this result.
    pub reindexed: Vec<StorageCompactionCandidate>,
    /// Number of bytes before.
    pub bytes_before: u64,
    /// Number of bytes after.
    pub bytes_after: u64,
    /// Optional stopped at, when available.
    pub stopped_at: Option<String>,
    /// Optional stop reason, when available.
    pub stop_reason: Option<StorageCompactionStopReason>,
}

/// Bounded online-compaction failure without query, path, or credential text.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum StorageCompactionError {
    #[error("Cartograph storage compaction policy is invalid")]
    /// The supplied policy contains inconsistent or out-of-range limits.
    InvalidPolicy,
    #[error("Cartograph storage compaction requires verified free-space headroom")]
    /// Free-space headroom could not be measured safely.
    HeadroomUnavailable,
    #[error("Cartograph storage compaction has insufficient free-space headroom")]
    /// Verified free space is below the online rebuild requirement.
    InsufficientHeadroom,
    #[error("another Cartograph online compaction is active")]
    /// A conflicting maintenance operation currently owns the resource.
    Busy,
    #[error("Cartograph heap compaction requires the pgstattuple extension")]
    /// Reclaimable heap pages cannot be measured without `pgstattuple`.
    HeapMeasurementUnavailable,
    #[error("Cartograph heap compaction is blocked by active project operations")]
    /// A live operation lease protects relations from exclusive maintenance.
    LiveOperations,
    #[error("Cartograph compaction measurement exceeded its deadline; raise --timeout-seconds")]
    /// Measuring index or heap reclaim did not finish within the policy deadline.
    MeasurementTimedOut,
    #[error(
        "Cartograph compaction measurement is not permitted; grant pg_stat_scan_tables to the database role"
    )]
    /// The database role cannot execute the `pgstattuple` measurement functions.
    MeasurementNotPermitted,
    #[error("Cartograph PostgreSQL compaction failed during {operation}")]
    /// PostgreSQL could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed PostgreSQL phase.
        operation: &'static str,
    },
    #[error("Cartograph PostgreSQL compaction found corrupt catalog {field}")]
    /// PostgreSQL catalog rows violate the expected index contract.
    CorruptCatalog {
        /// Catalog field whose stored value violated the compaction contract.
        field: &'static str,
    },
}

impl CartographDatabase {
    /// Build a bounded, read-only B-tree compaction plan.
    /// # Errors
    ///
    /// Returns an error if the policy is invalid or PostgreSQL cannot inspect
    /// the bounded set of eligible valid B-tree indexes.
    pub async fn storage_compaction_plan(
        &self,
        policy: StorageCompactionPolicy,
    ) -> Result<StorageCompactionPlan, StorageCompactionError> {
        validate_policy(policy)?;
        load_plan(self, policy).await
    }

    /// Apply one dry-run plan one index at a time with `REINDEX INDEX
    /// CONCURRENTLY`. The caller must provide observed free bytes; external
    /// deployments cannot infer filesystem headroom through PostgreSQL.
    /// # Errors
    ///
    /// Returns an error if policy or headroom checks fail, an index identity
    /// changes, or a concurrent reindex cannot complete safely.
    pub async fn compact_storage_online(
        &self,
        policy: StorageCompactionPolicy,
        available_headroom_bytes: u64,
    ) -> Result<StorageCompactionReport, StorageCompactionError> {
        self.compact_storage_online_with_observer(policy, available_headroom_bytes, || async {})
            .await
    }

    /// Execute online compaction with a bounded observer point after session
    /// state is installed. Live fault tests use this to prove cancellation
    /// cleanup without relying on PostgreSQL lock timing.
    #[doc(hidden)]
    pub async fn compact_storage_online_with_observer<Observe, Observed>(
        &self,
        policy: StorageCompactionPolicy,
        available_headroom_bytes: u64,
        observe_session: Observe,
    ) -> Result<StorageCompactionReport, StorageCompactionError>
    where
        Observe: FnOnce() -> Observed,
        Observed: Future<Output = ()>,
    {
        validate_policy(policy)?;
        let plan = load_plan(self, policy).await?;
        require_headroom(available_headroom_bytes, plan.required_headroom_bytes)?;
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|_| database_error("acquire"))?;
        connection.close_on_drop();
        let session =
            begin_compaction_session(&mut connection, self.schema.as_str(), policy).await?;
        observe_session().await;
        let schema = quoted_schema(&self.schema);
        let mut execution = execute_reindex_plan(&mut connection, &schema, &plan.candidates).await;
        let session_clean =
            finish_compaction_session(&mut connection, session, &mut execution).await;
        let connection_closed = connection.close().await.is_ok();
        if !session_clean && !connection_closed && execution.stop_reason.is_none() {
            execution.stop_reason = Some(StorageCompactionStopReason::AdvisoryUnlockFailed);
        }
        let bytes_before = sum_candidate_bytes(&execution.reindexed)?;
        let bytes_after = load_candidate_bytes(self, &execution.reindexed).await?;
        Ok(StorageCompactionReport {
            plan,
            dry_run: false,
            reindexed: execution.reindexed,
            bytes_before,
            bytes_after,
            stopped_at: execution.stopped_at,
            stop_reason: execution.stop_reason,
        })
    }

    /// Build a bounded, read-only inventory of free-but-allocated heap pages.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy is invalid, `pgstattuple` is not
    /// installed, or a Cartograph-owned relation cannot be measured safely.
    pub async fn heap_compaction_plan(
        &self,
        policy: HeapCompactionPolicy,
    ) -> Result<HeapCompactionPlan, StorageCompactionError> {
        validate_heap_policy(policy)?;
        load_heap_plan(self, policy).await
    }

    /// Rewrite eligible heaps one table at a time with `VACUUM FULL`. This is
    /// deliberately separate from online B-tree compaction: every candidate
    /// takes an `ACCESS EXCLUSIVE` table lock and requires quiesced project
    /// operations plus verified filesystem headroom.
    ///
    /// # Errors
    ///
    /// Returns an error when measurement, policy, headroom, compaction locking,
    /// or the active-operation preflight fails.
    pub async fn compact_heap_storage(
        &self,
        policy: HeapCompactionPolicy,
        available_headroom_bytes: u64,
    ) -> Result<HeapCompactionReport, StorageCompactionError> {
        self.compact_heap_storage_with_observer(policy, available_headroom_bytes, || async {})
            .await
    }

    /// Execute heap compaction with one observer point after the maintenance
    /// gate and active-operation check. Live tests use this to prove that new
    /// project leases cannot enter the rewrite window.
    #[doc(hidden)]
    pub async fn compact_heap_storage_with_observer<Observe, Observed>(
        &self,
        policy: HeapCompactionPolicy,
        available_headroom_bytes: u64,
        observe_gate: Observe,
    ) -> Result<HeapCompactionReport, StorageCompactionError>
    where
        Observe: FnOnce() -> Observed,
        Observed: Future<Output = ()>,
    {
        validate_heap_policy(policy)?;
        let plan = load_heap_plan(self, policy).await?;
        require_headroom(available_headroom_bytes, plan.required_headroom_bytes)?;
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|_| database_error("heap-acquire"))?;
        connection.close_on_drop();
        let session =
            begin_heap_compaction_session(&mut connection, &self.schema, policy.statement_timeout)
                .await?;
        if live_project_operations(&mut connection, self.schema.as_str()).await? > 0 {
            let mut cleanup = OnlineCompactionExecution {
                reindexed: Vec::new(),
                stopped_at: None,
                stop_reason: None,
            };
            let _ = finish_compaction_session(&mut connection, session, &mut cleanup).await;
            let _ = connection.close().await;
            return Err(StorageCompactionError::LiveOperations);
        }
        observe_gate().await;
        let schema = quoted_schema(&self.schema);
        let mut compacted = Vec::new();
        let mut stopped_at = None;
        let mut stop_reason = None;
        for candidate in &plan.candidates {
            if !vacuum_full_candidate(&mut connection, &schema, candidate).await {
                stopped_at = Some(candidate.table.clone());
                stop_reason = Some(StorageCompactionStopReason::VacuumFullFailed);
                break;
            }
            let bytes_after =
                relation_total_bytes(&mut connection, self.schema.as_str(), &candidate.table)
                    .await?;
            compacted.push(HeapCompactionResult {
                table: candidate.table.clone(),
                bytes_before: candidate.total_bytes,
                bytes_after,
                reclaimed_bytes: candidate.total_bytes.saturating_sub(bytes_after),
            });
        }
        let mut cleanup = OnlineCompactionExecution {
            reindexed: Vec::new(),
            stopped_at: None,
            stop_reason,
        };
        let session_clean = finish_compaction_session(&mut connection, session, &mut cleanup).await;
        if !session_clean && cleanup.stop_reason.is_none() {
            cleanup.stop_reason = Some(StorageCompactionStopReason::AdvisoryUnlockFailed);
        }
        let _ = connection.close().await;
        Ok(HeapCompactionReport {
            plan,
            compacted,
            stopped_at,
            stop_reason: cleanup.stop_reason,
        })
    }
}

struct OnlineCompactionSession {
    prior_timeout: String,
    lock_names: Vec<String>,
}

struct OnlineCompactionExecution {
    reindexed: Vec<StorageCompactionCandidate>,
    stopped_at: Option<String>,
    stop_reason: Option<StorageCompactionStopReason>,
}

struct InvalidIndexArtifactInventory {
    artifacts: Vec<InvalidIndexArtifact>,
    total: u64,
    truncated: bool,
}

fn require_headroom(available: u64, required: u64) -> Result<(), StorageCompactionError> {
    if available == 0 {
        return Err(StorageCompactionError::HeadroomUnavailable);
    }
    if available < required {
        return Err(StorageCompactionError::InsufficientHeadroom);
    }
    Ok(())
}

async fn begin_compaction_session(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    policy: StorageCompactionPolicy,
) -> Result<OnlineCompactionSession, StorageCompactionError> {
    begin_compaction_session_with_timeout(connection, schema, policy.statement_timeout).await
}

async fn begin_compaction_session_with_timeout(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    statement_timeout: Duration,
) -> Result<OnlineCompactionSession, StorageCompactionError> {
    let prior_timeout = query("SELECT current_setting('statement_timeout')")
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("read-timeout"))?
        .try_get::<String, _>(0)
        .map_err(|_| corrupt("statement_timeout"))?;
    let timeout_millis = i64::try_from(statement_timeout.as_millis())
        .map_err(|_| StorageCompactionError::InvalidPolicy)?;
    let lock_name = format!("{COMPACTION_LOCK_NAMESPACE}:{schema}");
    let lock = query("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(&lock_name)
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("advisory-lock"))?
        .try_get::<bool, _>(0)
        .map_err(|_| corrupt("advisory_lock"))?;
    if !lock {
        return Err(StorageCompactionError::Busy);
    }
    if set_compaction_timeout(connection, timeout_millis)
        .await
        .is_err()
    {
        let _ = unlock_compaction(connection, &lock_name).await;
        return Err(database_error("set-timeout"));
    }
    Ok(OnlineCompactionSession {
        prior_timeout,
        lock_names: vec![lock_name],
    })
}

async fn begin_heap_compaction_session(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    statement_timeout: Duration,
) -> Result<OnlineCompactionSession, StorageCompactionError> {
    let mut session =
        begin_compaction_session_with_timeout(connection, schema.as_str(), statement_timeout)
            .await?;
    let maintenance_lock_name = schema_maintenance_lock_key(schema);
    let maintenance_lock = query("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(&maintenance_lock_name)
        .fetch_one(&mut *connection)
        .await;
    let acquired = match maintenance_lock {
        Ok(row) => row
            .try_get::<bool, _>(0)
            .map_err(|_| corrupt("maintenance_gate")),
        Err(_) => Err(database_error("maintenance-gate")),
    };
    match acquired {
        Ok(true) => {
            session.lock_names.push(maintenance_lock_name);
            Ok(session)
        }
        Ok(false) => {
            let mut cleanup = OnlineCompactionExecution {
                reindexed: Vec::new(),
                stopped_at: None,
                stop_reason: None,
            };
            let _ = finish_compaction_session(connection, session, &mut cleanup).await;
            Err(StorageCompactionError::LiveOperations)
        }
        Err(error) => {
            let mut cleanup = OnlineCompactionExecution {
                reindexed: Vec::new(),
                stopped_at: None,
                stop_reason: None,
            };
            let _ = finish_compaction_session(connection, session, &mut cleanup).await;
            Err(error)
        }
    }
}

async fn set_compaction_timeout(
    connection: &mut sqlx_postgres::PgConnection,
    timeout_millis: i64,
) -> Result<(), StorageCompactionError> {
    query("SELECT set_config('statement_timeout', $1, false)")
        .bind(format!("{timeout_millis}ms"))
        .execute(connection)
        .await
        .map(|_| ())
        .map_err(|_| database_error("set-timeout"))
}

async fn execute_reindex_plan(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    candidates: &[StorageCompactionCandidate],
) -> OnlineCompactionExecution {
    let mut execution = OnlineCompactionExecution {
        reindexed: Vec::new(),
        stopped_at: None,
        stop_reason: None,
    };
    for candidate in candidates {
        if !reindex_candidate(connection, schema, candidate).await {
            execution.stopped_at = Some(candidate.index.clone());
            execution.stop_reason = Some(StorageCompactionStopReason::ReindexFailed);
            break;
        }
        execution.reindexed.push(candidate.clone());
    }
    execution
}

async fn reindex_candidate(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    candidate: &StorageCompactionCandidate,
) -> bool {
    if !valid_identifier(&candidate.index) {
        return false;
    }
    let statement = format!(
        r#"REINDEX INDEX CONCURRENTLY {schema}."{}""#,
        candidate.index
    );
    query(AssertSqlSafe(statement))
        .execute(connection)
        .await
        .is_ok()
}

async fn finish_compaction_session(
    connection: &mut sqlx_postgres::PgConnection,
    session: OnlineCompactionSession,
    execution: &mut OnlineCompactionExecution,
) -> bool {
    let OnlineCompactionSession {
        prior_timeout,
        lock_names,
    } = session;
    let timeout_restored = query("SELECT set_config('statement_timeout', $1, false)")
        .bind(prior_timeout)
        .execute(&mut *connection)
        .await
        .is_ok();
    if !timeout_restored && execution.stop_reason.is_none() {
        execution.stop_reason = Some(StorageCompactionStopReason::TimeoutRestoreFailed);
    }
    let mut unlocked = true;
    for lock_name in lock_names.into_iter().rev() {
        if !unlock_compaction(connection, &lock_name).await {
            unlocked = false;
        }
    }
    if !unlocked && execution.stop_reason.is_none() {
        execution.stop_reason = Some(StorageCompactionStopReason::AdvisoryUnlockFailed);
    }
    timeout_restored && unlocked
}

async fn unlock_compaction(connection: &mut sqlx_postgres::PgConnection, lock_name: &str) -> bool {
    query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(lock_name)
        .fetch_one(connection)
        .await
        .ok()
        .and_then(|row| row.try_get::<bool, _>(0).ok())
        .unwrap_or(false)
}

fn sum_candidate_bytes(
    candidates: &[StorageCompactionCandidate],
) -> Result<u64, StorageCompactionError> {
    candidates.iter().try_fold(0_u64, |total, candidate| {
        total
            .checked_add(candidate.bytes)
            .ok_or_else(|| corrupt("bytes_before"))
    })
}

/// Valid, ready, non-exclusion B-trees in the Cartograph schema; `$1` is the
/// schema and `$2` the minimum allocation.
const ELIGIBLE_INDEXES: &str = r"SELECT indexes.oid AS index_oid,
               indexes.relname AS index_name,
               tables.relname AS table_name,
               pg_relation_size(indexes.oid)::bigint AS bytes,
               indexes.reloptions
        FROM pg_catalog.pg_index AS catalog
        INNER JOIN pg_catalog.pg_class AS indexes
            ON indexes.oid = catalog.indexrelid
        INNER JOIN pg_catalog.pg_class AS tables
            ON tables.oid = catalog.indrelid
        INNER JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = indexes.relnamespace
        INNER JOIN pg_catalog.pg_am AS methods
            ON methods.oid = indexes.relam
        LEFT JOIN pg_catalog.pg_constraint AS constraints
            ON constraints.conindid = indexes.oid
           AND constraints.contype = 'x'
        WHERE namespaces.nspname = $1
          AND indexes.relkind = 'i'
          AND methods.amname = 'btree'
          AND catalog.indisvalid
          AND catalog.indisready
          AND constraints.oid IS NULL
          AND pg_relation_size(indexes.oid) >= $2";

async fn load_plan(
    database: &CartographDatabase,
    policy: StorageCompactionPolicy,
) -> Result<StorageCompactionPlan, StorageCompactionError> {
    let extension_schema = optional_pgstattuple_schema(database).await?;
    let reclaim_measured = extension_schema.is_some();
    let rows = match extension_schema {
        Some(extension_schema) => {
            load_measured_index_rows(database, policy, &extension_schema).await?
        }
        None => load_sized_index_rows(database, policy).await?,
    };
    let total_candidates = rows
        .first()
        .map(|row| read_nonnegative(row, "total_candidates"))
        .transpose()?
        .unwrap_or(0);
    let mut candidates = Vec::new();
    let mut candidate_bytes = 0_u64;
    let mut estimated_reclaimable_bytes = 0_u64;
    let mut truncated = total_candidates > u64::from(policy.maximum_indexes);
    for row in rows.into_iter().take(usize::from(policy.maximum_indexes)) {
        let candidate = StorageCompactionCandidate {
            index: read_identifier(&row, "index_name")?,
            table: read_identifier(&row, "table_name")?,
            bytes: read_nonnegative(&row, "bytes")?,
            estimated_reclaimable_bytes: reclaim_measured
                .then(|| read_nonnegative(&row, "estimated_reclaimable_bytes"))
                .transpose()?,
        };
        let admitted = candidate_bytes
            .checked_add(candidate.bytes)
            .ok_or_else(|| corrupt("candidate_bytes"))?;
        if admitted > policy.maximum_candidate_bytes {
            truncated = true;
            continue;
        }
        candidate_bytes = admitted;
        estimated_reclaimable_bytes = estimated_reclaimable_bytes
            .checked_add(candidate.estimated_reclaimable_bytes.unwrap_or(0))
            .ok_or_else(|| corrupt("estimated_reclaimable_bytes"))?;
        candidates.push(candidate);
    }
    let largest = candidates
        .iter()
        .map(|candidate| candidate.bytes)
        .max()
        .unwrap_or(0);
    let required_headroom_bytes = if largest == 0 {
        0
    } else {
        largest
            .checked_mul(HEADROOM_MULTIPLIER)
            .and_then(|bytes| bytes.checked_add(HEADROOM_ALLOWANCE_BYTES))
            .ok_or_else(|| corrupt("required_headroom_bytes"))?
    };
    let invalid_artifacts = load_invalid_artifacts(database).await?;
    Ok(StorageCompactionPlan {
        candidates,
        invalid_artifacts: invalid_artifacts.artifacts,
        invalid_artifact_total: invalid_artifacts.total,
        invalid_artifacts_truncated: invalid_artifacts.truncated,
        candidate_bytes,
        estimated_reclaimable_bytes: reclaim_measured.then_some(estimated_reclaimable_bytes),
        reclaim_measured,
        required_headroom_bytes,
        truncated,
    })
}

/// Size-only selection for PostgreSQL without `pgstattuple`.
async fn load_sized_index_rows(
    database: &CartographDatabase,
    policy: StorageCompactionPolicy,
) -> Result<Vec<sqlx_postgres::PgRow>, StorageCompactionError> {
    let statement = format!(
        r"WITH eligible AS ({ELIGIBLE_INDEXES})
        SELECT index_name, table_name, bytes, count(*) OVER ()::bigint AS total_candidates
        FROM eligible
        ORDER BY bytes DESC, index_name
        LIMIT $3"
    );
    query(AssertSqlSafe(statement))
        .bind(database.schema.as_str())
        .bind(bytes_parameter(policy.minimum_index_bytes)?)
        .bind(i64::from(policy.maximum_indexes) + 1)
        .fetch_all(&database.pool)
        .await
        .map_err(|_| database_error("plan-candidates"))
}

/// Measure every index that could meet the reclaim threshold with
/// `pgstatindex`, which reads the whole index, under the policy's deadline.
/// A rebuild packs leaf pages to the index fill factor and drops empty and
/// deleted pages; internal pages are ignored, so the estimate is conservative.
async fn load_measured_index_rows(
    database: &CartographDatabase,
    policy: StorageCompactionPolicy,
    extension_schema: &str,
) -> Result<Vec<sqlx_postgres::PgRow>, StorageCompactionError> {
    let extension_schema = quote_valid_identifier(extension_schema)?;
    let statement = format!(
        r#"WITH eligible AS ({ELIGIBLE_INDEXES}),
        measured AS (
            SELECT eligible.index_name,
                   eligible.table_name,
                   eligible.bytes,
                   (CASE WHEN statistics.leaf_pages = 0
                              OR statistics.avg_leaf_density = 'NaN'::float8
                         THEN 0
                         ELSE GREATEST(statistics.leaf_pages - ceil(
                                  statistics.leaf_pages * statistics.avg_leaf_density
                                  / COALESCE((
                                        SELECT options.option_value::float8
                                        FROM pg_catalog.pg_options_to_table(eligible.reloptions)
                                            AS options
                                        WHERE options.option_name = 'fillfactor'
                                    ), $5))::bigint, 0)
                    END + statistics.empty_pages + statistics.deleted_pages)
                   * current_setting('block_size')::bigint AS estimated_reclaimable_bytes
            FROM eligible
            CROSS JOIN LATERAL {extension_schema}."pgstatindex"(eligible.index_oid::regclass)
                AS statistics
        )
        SELECT index_name, table_name, bytes, estimated_reclaimable_bytes,
               count(*) OVER ()::bigint AS total_candidates
        FROM measured
        WHERE estimated_reclaimable_bytes >= $4
        ORDER BY estimated_reclaimable_bytes DESC, index_name
        LIMIT $3"#
    );
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("plan-begin"))?;
    crate::database::set_local_statement_timeout(&mut transaction, policy.statement_timeout)
        .await
        .map_err(|()| StorageCompactionError::InvalidPolicy)?;
    // An index smaller than the reclaim threshold can never meet it, so it is
    // not read at all.
    let rows = query(AssertSqlSafe(statement))
        .bind(database.schema.as_str())
        .bind(bytes_parameter(
            policy
                .minimum_index_bytes
                .max(policy.minimum_reclaimable_bytes),
        )?)
        .bind(i64::from(policy.maximum_indexes) + 1)
        .bind(bytes_parameter(policy.minimum_reclaimable_bytes)?)
        .bind(BTREE_DEFAULT_FILLFACTOR)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| measurement_error(&error, "plan-measure-candidates"))?;
    transaction
        .rollback()
        .await
        .map_err(|_| database_error("plan-rollback"))?;
    Ok(rows)
}

fn bytes_parameter(bytes: u64) -> Result<i64, StorageCompactionError> {
    i64::try_from(bytes).map_err(|_| StorageCompactionError::InvalidPolicy)
}

async fn load_invalid_artifacts(
    database: &CartographDatabase,
) -> Result<InvalidIndexArtifactInventory, StorageCompactionError> {
    let statement = r"SELECT indexes.relname AS index_name,
               tables.relname AS table_name,
               pg_relation_size(indexes.oid)::bigint AS bytes,
               catalog.indisvalid,
               catalog.indisready,
               count(*) OVER ()::bigint AS total_artifacts
        FROM pg_catalog.pg_index AS catalog
        INNER JOIN pg_catalog.pg_class AS indexes
            ON indexes.oid = catalog.indexrelid
        INNER JOIN pg_catalog.pg_class AS tables
            ON tables.oid = catalog.indrelid
        INNER JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = indexes.relnamespace
        WHERE namespaces.nspname = $1
          AND (NOT catalog.indisvalid OR NOT catalog.indisready
               OR indexes.relname ~ '_cc(new|old)[0-9]*$')
        ORDER BY bytes DESC, indexes.relname
        LIMIT $2";
    let rows = query(statement)
        .bind(database.schema.as_str())
        .bind(
            i64::try_from(MAXIMUM_INVALID_ARTIFACTS + 1)
                .map_err(|_| corrupt("invalid_artifact_limit"))?,
        )
        .fetch_all(&database.pool)
        .await
        .map_err(|_| database_error("plan-invalid-artifacts"))?;
    let total = rows
        .first()
        .map(|row| read_nonnegative(row, "total_artifacts"))
        .transpose()?
        .unwrap_or(0);
    let artifacts = rows
        .iter()
        .take(MAXIMUM_INVALID_ARTIFACTS)
        .map(|row| {
            Ok(InvalidIndexArtifact {
                index: read_identifier(row, "index_name")?,
                table: read_identifier(row, "table_name")?,
                bytes: read_nonnegative(row, "bytes")?,
                valid: row
                    .try_get::<bool, _>("indisvalid")
                    .map_err(|_| corrupt("index_valid"))?,
                ready: row
                    .try_get::<bool, _>("indisready")
                    .map_err(|_| corrupt("index_ready"))?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let returned =
        u64::try_from(artifacts.len()).map_err(|_| corrupt("invalid_artifact_returned"))?;
    Ok(InvalidIndexArtifactInventory {
        artifacts,
        total,
        truncated: total > returned,
    })
}

async fn load_candidate_bytes(
    database: &CartographDatabase,
    candidates: &[StorageCompactionCandidate],
) -> Result<u64, StorageCompactionError> {
    let names = candidates
        .iter()
        .map(|candidate| candidate.index.clone())
        .collect::<Vec<_>>();
    let statement = r"SELECT COALESCE(sum(pg_relation_size(indexes.oid)), 0)::bigint AS bytes
        FROM pg_catalog.pg_class AS indexes
        INNER JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = indexes.relnamespace
        WHERE namespaces.nspname = $1
          AND indexes.relname = ANY($2::text[])";
    let row = query(statement)
        .bind(database.schema.as_str())
        .bind(names)
        .fetch_one(&database.pool)
        .await
        .map_err(|_| database_error("post-compaction-size"))?;
    read_nonnegative(&row, "bytes")
}

async fn load_heap_plan(
    database: &CartographDatabase,
    policy: HeapCompactionPolicy,
) -> Result<HeapCompactionPlan, StorageCompactionError> {
    let rows = load_heap_candidate_rows(database, policy).await?;
    assemble_heap_plan(rows, policy)
}

/// Measured heap and TOAST reclaim for the allowlisted tables. `$1` is the
/// schema, `$2` the allowlist, `$3` the reclaim threshold and `$4` the limit.
///
/// A rewrite packs live tuples at the table fill factor, so its size is the
/// live tuple count over the tuples that fit on one page (average tuple plus
/// line pointer and alignment, capped at PostgreSQL's per-page tuple limit).
/// TOAST always fits at least four chunks per page, which the approximate
/// average length understates. Free space inside that packed layout is not
/// reclaimable.
fn heap_plan_statement(extension_schema: &str) -> String {
    format!(
        r#"WITH parents AS (
               SELECT classes.oid AS parent_oid,
                      classes.reltoastrelid AS toast_oid,
                      classes.relname AS table_name,
                      COALESCE((
                          SELECT options.option_value::float8
                          FROM pg_catalog.pg_options_to_table(classes.reloptions) AS options
                          WHERE options.option_name = 'fillfactor'
                      ), 100) AS fillfactor
               FROM pg_catalog.pg_class AS classes
               INNER JOIN pg_catalog.pg_namespace AS namespaces
                   ON namespaces.oid = classes.relnamespace
               WHERE namespaces.nspname = $1
                 AND classes.relkind = 'r'
                 AND classes.relname = ANY($2::text[])
           ), forks AS (
               SELECT parent_oid, parent_oid AS fork_oid, fillfactor, 1 AS minimum_per_page
               FROM parents
               UNION ALL
               SELECT parent_oid, toast_oid AS fork_oid, 100 AS fillfactor, 4 AS minimum_per_page
               FROM parents WHERE toast_oid <> 0
           ), fork_measurements AS (
               SELECT forks.parent_oid,
                      pg_relation_size(forks.fork_oid)::bigint AS fork_bytes,
                      statistics.dead_tuple_len::bigint AS dead_tuple_bytes,
                      statistics.approx_free_space::bigint AS free_bytes,
                      statistics.approx_tuple_count::float8 AS tuple_count,
                      statistics.approx_tuple_len::float8 AS tuple_bytes,
                      forks.fillfactor,
                      forks.minimum_per_page,
                      current_setting('block_size')::float8 AS block_bytes
               FROM forks
               CROSS JOIN LATERAL {extension_schema}."pgstattuple_approx"(forks.fork_oid)
                   AS statistics
           ), fork_estimates AS (
               SELECT parent_oid,
                      fork_bytes,
                      dead_tuple_bytes,
                      free_bytes,
                      LEAST(fork_bytes, CASE WHEN tuple_count = 0 THEN 0
                          ELSE (ceil(tuple_count / LEAST(
                                   floor((block_bytes - 24) / 28),
                                   GREATEST(minimum_per_page, floor(
                                       (block_bytes - 24 - block_bytes * (100 - fillfactor) / 100)
                                       / (tuple_bytes / tuple_count + 8)))
                               )) * block_bytes)::bigint
                      END) AS rewritten_bytes
               FROM fork_measurements
           ), fork_statistics AS (
               SELECT parent_oid,
                      sum(fork_bytes)::bigint AS heap_bytes,
                      sum(dead_tuple_bytes)::bigint AS dead_tuple_bytes,
                      sum(free_bytes)::bigint AS free_bytes,
                      sum(rewritten_bytes)::bigint AS rewritten_bytes,
                      sum(LEAST(fork_bytes - rewritten_bytes,
                                dead_tuple_bytes + free_bytes))::bigint
                          AS estimated_reclaimable_bytes
               FROM fork_estimates
               GROUP BY parent_oid
           ), measured AS (
               SELECT parents.table_name,
                      fork_statistics.heap_bytes,
                      pg_total_relation_size(parents.parent_oid)::bigint AS total_bytes,
                      fork_statistics.dead_tuple_bytes,
                      fork_statistics.free_bytes,
                      fork_statistics.rewritten_bytes,
                      fork_statistics.estimated_reclaimable_bytes
               FROM parents
               INNER JOIN fork_statistics
                   ON fork_statistics.parent_oid = parents.parent_oid
           )
           SELECT measured.table_name,
                  measured.heap_bytes,
                  measured.total_bytes,
                  measured.dead_tuple_bytes,
                  measured.free_bytes,
                  measured.rewritten_bytes,
                  measured.estimated_reclaimable_bytes,
                  count(*) OVER ()::bigint AS total_candidates
           FROM measured
           WHERE measured.estimated_reclaimable_bytes >= $3
           ORDER BY measured.estimated_reclaimable_bytes DESC, measured.table_name
           LIMIT $4"#
    )
}

async fn load_heap_candidate_rows(
    database: &CartographDatabase,
    policy: HeapCompactionPolicy,
) -> Result<Vec<sqlx_postgres::PgRow>, StorageCompactionError> {
    let extension_schema = pgstattuple_schema(database).await?;
    let statement = heap_plan_statement(&quote_valid_identifier(&extension_schema)?);
    let allowlist = HEAP_COMPACTION_TABLES
        .iter()
        .map(|table| (*table).to_owned())
        .collect::<Vec<_>>();
    query(AssertSqlSafe(statement))
        .bind(database.schema.as_str())
        .bind(allowlist)
        .bind(
            i64::try_from(policy.minimum_reclaimable_bytes)
                .map_err(|_| StorageCompactionError::InvalidPolicy)?,
        )
        .bind(i64::from(policy.maximum_relations) + 1)
        .fetch_all(&database.pool)
        .await
        .map_err(|error| measurement_error(&error, "heap-plan"))
}

fn assemble_heap_plan(
    rows: Vec<sqlx_postgres::PgRow>,
    policy: HeapCompactionPolicy,
) -> Result<HeapCompactionPlan, StorageCompactionError> {
    let total_candidates = rows
        .first()
        .map(|row| read_nonnegative(row, "total_candidates"))
        .transpose()?
        .unwrap_or(0);
    let mut candidates = Vec::new();
    let mut candidate_bytes = 0_u64;
    let mut estimated_reclaimable_bytes = 0_u64;
    let mut truncated = total_candidates > u64::from(policy.maximum_relations);
    for row in rows.into_iter().take(usize::from(policy.maximum_relations)) {
        let candidate = HeapCompactionCandidate {
            table: read_identifier(&row, "table_name")?,
            heap_bytes: read_nonnegative(&row, "heap_bytes")?,
            total_bytes: read_nonnegative(&row, "total_bytes")?,
            dead_tuple_bytes: read_nonnegative(&row, "dead_tuple_bytes")?,
            free_bytes: read_nonnegative(&row, "free_bytes")?,
            estimated_rewritten_bytes: read_nonnegative(&row, "rewritten_bytes")?,
            estimated_reclaimable_bytes: read_nonnegative(&row, "estimated_reclaimable_bytes")?,
        };
        if !heap_table_allowed(&candidate.table) {
            return Err(corrupt("heap_table"));
        }
        let admitted = candidate_bytes
            .checked_add(candidate.total_bytes)
            .ok_or_else(|| corrupt("heap_candidate_bytes"))?;
        if admitted > policy.maximum_candidate_bytes {
            truncated = true;
            continue;
        }
        candidate_bytes = admitted;
        estimated_reclaimable_bytes = estimated_reclaimable_bytes
            .checked_add(candidate.estimated_reclaimable_bytes)
            .ok_or_else(|| corrupt("heap_reclaimable_bytes"))?;
        candidates.push(candidate);
    }
    let required_headroom_bytes = candidates
        .iter()
        .map(|candidate| candidate.total_bytes)
        .max()
        .unwrap_or(0)
        .checked_add(if candidates.is_empty() {
            0
        } else {
            HEAP_HEADROOM_ALLOWANCE_BYTES
        })
        .ok_or_else(|| corrupt("heap_required_headroom_bytes"))?;
    Ok(HeapCompactionPlan {
        candidates,
        candidate_bytes,
        estimated_reclaimable_bytes,
        required_headroom_bytes,
        truncated,
        requires_access_exclusive: true,
    })
}

async fn pgstattuple_schema(
    database: &CartographDatabase,
) -> Result<String, StorageCompactionError> {
    optional_pgstattuple_schema(database)
        .await?
        .ok_or(StorageCompactionError::HeapMeasurementUnavailable)
}

async fn optional_pgstattuple_schema(
    database: &CartographDatabase,
) -> Result<Option<String>, StorageCompactionError> {
    let row = query(
        r"SELECT namespaces.nspname
           FROM pg_catalog.pg_extension AS extensions
           INNER JOIN pg_catalog.pg_namespace AS namespaces
               ON namespaces.oid = extensions.extnamespace
           WHERE extensions.extname = 'pgstattuple'",
    )
    .fetch_optional(&database.pool)
    .await
    .map_err(|_| database_error("pgstattuple-extension"))?;
    row.map(|row| read_identifier(&row, "nspname")).transpose()
}

fn quote_valid_identifier(value: &str) -> Result<String, StorageCompactionError> {
    valid_identifier(value)
        .then(|| format!(r#""{value}""#))
        .ok_or_else(|| corrupt("identifier"))
}

fn heap_table_allowed(table: &str) -> bool {
    HEAP_COMPACTION_TABLES.contains(&table)
}

async fn live_project_operations(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
) -> Result<u64, StorageCompactionError> {
    let schema = quote_valid_identifier(schema)?;
    let statement = format!(
        r#"SELECT count(*)::bigint AS live_operations
           FROM {schema}."project_operation_leases"
           WHERE expires_at > clock_timestamp()"#
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("heap-live-operations"))?;
    read_nonnegative(&row, "live_operations")
}

async fn vacuum_full_candidate(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    candidate: &HeapCompactionCandidate,
) -> bool {
    if !valid_identifier(&candidate.table) || !heap_table_allowed(&candidate.table) {
        return false;
    }
    let statement = format!(r#"VACUUM (FULL, ANALYZE) {schema}."{}""#, candidate.table);
    query(AssertSqlSafe(statement))
        .execute(connection)
        .await
        .is_ok()
}

async fn relation_total_bytes(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    table: &str,
) -> Result<u64, StorageCompactionError> {
    if !valid_identifier(table) || !heap_table_allowed(table) {
        return Err(corrupt("heap_table"));
    }
    let row = query(
        r"SELECT pg_total_relation_size(classes.oid)::bigint AS bytes
           FROM pg_catalog.pg_class AS classes
           INNER JOIN pg_catalog.pg_namespace AS namespaces
               ON namespaces.oid = classes.relnamespace
           WHERE namespaces.nspname = $1 AND classes.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(connection)
    .await
    .map_err(|_| database_error("heap-post-compaction-size"))?;
    read_nonnegative(&row, "bytes")
}

fn validate_heap_policy(policy: HeapCompactionPolicy) -> Result<(), StorageCompactionError> {
    HeapCompactionPolicy::new(HeapCompactionPolicyInput {
        maximum_relations: policy.maximum_relations,
        maximum_candidate_bytes: policy.maximum_candidate_bytes,
        minimum_reclaimable_bytes: policy.minimum_reclaimable_bytes,
        statement_timeout: policy.statement_timeout,
    })
    .map(|_| ())
}

fn validate_policy(policy: StorageCompactionPolicy) -> Result<(), StorageCompactionError> {
    StorageCompactionPolicy::new(StorageCompactionPolicyInput {
        maximum_indexes: policy.maximum_indexes,
        maximum_candidate_bytes: policy.maximum_candidate_bytes,
        minimum_index_bytes: policy.minimum_index_bytes,
        minimum_reclaimable_bytes: policy.minimum_reclaimable_bytes,
        statement_timeout: policy.statement_timeout,
    })
    .map(|_| ())
}

fn read_identifier(
    row: &sqlx_postgres::PgRow,
    field: &'static str,
) -> Result<String, StorageCompactionError> {
    row.try_get::<String, _>(field)
        .ok()
        .filter(|value| valid_identifier(value))
        .ok_or_else(|| corrupt(field))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn read_nonnegative(
    row: &sqlx_postgres::PgRow,
    field: &'static str,
) -> Result<u64, StorageCompactionError> {
    row.try_get::<i64, _>(field)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| corrupt(field))
}

/// Separate the two measurement failures an operator can act on from other
/// database errors: the policy deadline and a missing statistics grant.
fn measurement_error(error: &sqlx_core::Error, operation: &'static str) -> StorageCompactionError {
    match error
        .as_database_error()
        .and_then(sqlx_core::error::DatabaseError::code)
        .as_deref()
    {
        Some("57014") => StorageCompactionError::MeasurementTimedOut,
        Some("42501") => StorageCompactionError::MeasurementNotPermitted,
        _ => database_error(operation),
    }
}

const fn database_error(operation: &'static str) -> StorageCompactionError {
    StorageCompactionError::DatabaseOperation { operation }
}

const fn corrupt(field: &'static str) -> StorageCompactionError {
    StorageCompactionError::CorruptCatalog { field }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_policy_and_identifiers_are_strictly_bounded() {
        assert!(
            StorageCompactionPolicy::new(StorageCompactionPolicyInput {
                maximum_indexes: 0,
                maximum_candidate_bytes: 1,
                minimum_index_bytes: 1,
                minimum_reclaimable_bytes: 1,
                statement_timeout: Duration::from_secs(1),
            })
            .is_err()
        );
        assert!(
            StorageCompactionPolicy::new(StorageCompactionPolicyInput {
                maximum_indexes: 1,
                maximum_candidate_bytes: 2,
                minimum_index_bytes: 1,
                minimum_reclaimable_bytes: 1,
                statement_timeout: Duration::from_secs(1),
            })
            .is_err()
        );
        let bounded = |minimum_reclaimable_bytes| {
            StorageCompactionPolicy::new(StorageCompactionPolicyInput {
                maximum_indexes: 1,
                maximum_candidate_bytes: 2 * MINIMUM_INDEX_BYTES,
                minimum_index_bytes: MINIMUM_INDEX_BYTES,
                minimum_reclaimable_bytes,
                statement_timeout: Duration::from_secs(1),
            })
        };
        assert!(bounded(1).is_ok());
        assert!(bounded(0).is_err());
        assert!(bounded(2 * MINIMUM_INDEX_BYTES + 1).is_err());
        assert!(valid_identifier("references_exact_name_site_idx"));
        assert!(!valid_identifier("public.references_idx"));
        assert!(!valid_identifier("unsafe\"identifier"));
        assert!(heap_table_allowed("search_documents"));
        assert!(!heap_table_allowed("operator_table"));
        assert!(
            HeapCompactionPolicy::new(HeapCompactionPolicyInput {
                maximum_relations: 8,
                maximum_candidate_bytes: 1024,
                minimum_reclaimable_bytes: 1,
                statement_timeout: Duration::from_secs(1),
            })
            .is_ok()
        );
        assert!(
            HeapCompactionPolicy::new(HeapCompactionPolicyInput {
                maximum_relations: 33,
                maximum_candidate_bytes: 1024,
                minimum_reclaimable_bytes: 1,
                statement_timeout: Duration::from_secs(1),
            })
            .is_err()
        );
    }
}
