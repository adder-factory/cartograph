use std::{future::Future, time::Duration};

mod drain;
mod telemetry;
pub use telemetry::GenerationRetentionAttempt;

use cartograph_domain::{GenerationId, ProjectOperation};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use crate::{CartographDatabase, LeaseFence, leases::project_lock_key};

const MAXIMUM_DELETE_BATCH: u32 = 10_000;
const MAXIMUM_CASCADE_ROW_BUDGET: u64 = 100_000_000;
const MAXIMUM_SEARCH_RELATION_BYTE_BUDGET: u64 = 64 * 1_024 * 1_024 * 1_024;
const MAXIMUM_DDL_RELATIONS: u32 = 64;
const DEFAULT_CASCADE_ROW_BUDGET: u64 = 5_000_000;
const DEFAULT_SEARCH_RELATION_BYTE_BUDGET: u64 = 8 * 1_024 * 1_024 * 1_024;
const DEFAULT_STALE_STAGING_AGE: Duration = Duration::from_mins(10);
const MAXIMUM_STALE_STAGING_AGE: Duration = Duration::from_hours(24);
const DEFAULT_STALE_READY_AGE: Duration = Duration::from_hours(24);
const MAXIMUM_STALE_READY_AGE: Duration = Duration::from_hours(720);
const POST_RETENTION_VACUUM_ROW_THRESHOLD: u64 = 100_000;
const RETENTION_ROW_TABLES: [&str; 27] = [
    "index_generations",
    "native_generation_spills",
    "native_generation_spill_batches",
    "native_generation_spill_rows",
    "native_generation_spill_files",
    "native_generation_spill_symbols",
    "native_generation_spill_edges",
    "native_generation_spill_references",
    "native_generation_spill_numerical_sites",
    "native_generation_spill_documents",
    "files",
    "symbols",
    "edges",
    "references",
    "numerical_sites",
    "search_documents",
    "project_operation_leases",
    "document_embeddings",
    "generation_search_relations",
    "symbol_coverage",
    "symbol_similarity_edges",
    "symbol_similarity_builds",
    "symbol_issues",
    "issue_history_refreshes",
    "summary_priority_queue",
    "structural_findings",
    "structural_finding_runs",
];
const RETENTION_LOCK_NAMESPACE: &str = "cartograph-v2-generation-retention";
const PUBLICATION_LOCK_NAMESPACE: &str = "cartograph-v2-publish";
const INVALID_DELETE_BATCH: u32 = 0;

/// Bounded policy for deleting only terminal historical generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationRetentionPolicy {
    recent_superseded: u32,
    maximum_deletions: u32,
    stale_staging_age: Duration,
    stale_ready_age: Duration,
    maximum_cascade_rows: u64,
    maximum_search_relation_bytes: u64,
    maximum_ddl_relations: u32,
}

/// Exact lease fence and deadline for a bounded sequence of retention transactions.
#[derive(Clone, Copy)]
pub struct GenerationRetentionRequest<'a> {
    policy: GenerationRetentionPolicy,
    fence: &'a LeaseFence,
    statement_timeout: Duration,
    post_retention_maintenance: PostRetentionMaintenancePolicy,
}

/// Post-commit maintenance policy for one bounded generation cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostRetentionMaintenancePolicy {
    /// Run thresholded table-scoped vacuuming before returning to the caller.
    #[default]
    Immediate,
    /// Leave dead-row reclamation to the schema's aggressive autovacuum policy.
    DelegateToAutovacuum,
}

impl<'a> GenerationRetentionRequest<'a> {
    /// Bind one retention policy to an exact project-wide migration lease.
    #[must_use]
    pub const fn new(
        policy: GenerationRetentionPolicy,
        fence: &'a LeaseFence,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            policy,
            fence,
            statement_timeout,
            post_retention_maintenance: PostRetentionMaintenancePolicy::Immediate,
        }
    }

    /// Select whether a large committed cascade runs synchronous table
    /// maintenance or delegates reclamation to PostgreSQL autovacuum.
    #[must_use]
    pub const fn with_post_retention_maintenance(
        mut self,
        policy: PostRetentionMaintenancePolicy,
    ) -> Self {
        self.post_retention_maintenance = policy;
        self
    }
}

impl GenerationRetentionPolicy {
    /// Preserve the newest `recent_superseded` histories and bound one cleanup batch.
    /// # Errors
    ///
    /// Returns an error if `maximum_deletions` is zero or exceeds the cleanup
    /// batch maximum.
    pub const fn new(
        recent_superseded: u32,
        maximum_deletions: u32,
    ) -> Result<Self, GenerationRetentionError> {
        if maximum_deletions == INVALID_DELETE_BATCH || maximum_deletions > MAXIMUM_DELETE_BATCH {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        Ok(Self {
            recent_superseded,
            maximum_deletions,
            stale_staging_age: DEFAULT_STALE_STAGING_AGE,
            stale_ready_age: DEFAULT_STALE_READY_AGE,
            maximum_cascade_rows: DEFAULT_CASCADE_ROW_BUDGET,
            maximum_search_relation_bytes: DEFAULT_SEARCH_RELATION_BYTE_BUDGET,
            maximum_ddl_relations: if maximum_deletions < MAXIMUM_DDL_RELATIONS {
                maximum_deletions
            } else {
                MAXIMUM_DDL_RELATIONS
            },
        })
    }

    /// Override how old an unleased ready generation must be before it can be
    /// reconciled. Current pointers and incomplete imports remain protected.
    /// # Errors
    ///
    /// Returns an error if `stale_ready_age` is zero or exceeds the maximum
    /// eligible age window.
    pub fn with_stale_ready_age(
        mut self,
        stale_ready_age: Duration,
    ) -> Result<Self, GenerationRetentionError> {
        if stale_ready_age.is_zero() || stale_ready_age > MAXIMUM_STALE_READY_AGE {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        self.stale_ready_age = stale_ready_age;
        Ok(self)
    }

    /// Override how old an unleased staging generation must be before collection.
    /// # Errors
    ///
    /// Returns an error if `stale_staging_age` is zero or exceeds the maximum
    /// eligible age window.
    pub fn with_stale_staging_age(
        mut self,
        stale_staging_age: Duration,
    ) -> Result<Self, GenerationRetentionError> {
        if stale_staging_age.is_zero() || stale_staging_age > MAXIMUM_STALE_STAGING_AGE {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        self.stale_staging_age = stale_staging_age;
        Ok(self)
    }

    /// Override only the canonical/cascade-row work cap while retaining the
    /// default physical-relation and DDL bounds.
    /// # Errors
    ///
    /// Returns an error if `maximum_cascade_rows` is zero or exceeds the hard
    /// cleanup maximum.
    pub const fn with_maximum_cascade_rows(
        mut self,
        maximum_cascade_rows: u64,
    ) -> Result<Self, GenerationRetentionError> {
        if !valid_positive_limit(maximum_cascade_rows, MAXIMUM_CASCADE_ROW_BUDGET) {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        self.maximum_cascade_rows = maximum_cascade_rows;
        Ok(self)
    }

    /// Admit an explicitly audited physical search-relation byte budget.
    /// # Errors
    /// Returns an error for zero or a budget above the 64 GiB hard maximum.
    pub const fn with_maximum_search_relation_bytes(
        mut self,
        maximum_search_relation_bytes: u64,
    ) -> Result<Self, GenerationRetentionError> {
        if !valid_positive_limit(
            maximum_search_relation_bytes,
            MAXIMUM_SEARCH_RELATION_BYTE_BUDGET,
        ) {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        self.maximum_search_relation_bytes = maximum_search_relation_bytes;
        Ok(self)
    }

    /// Override exact row, physical relation-byte, and DDL-count work caps.
    /// # Errors
    ///
    /// Returns an error if any cascade-row, search-relation-byte, or DDL count
    /// cap is zero or exceeds its cleanup hard maximum.
    pub const fn with_work_limits(
        mut self,
        maximum_cascade_rows: u64,
        maximum_search_relation_bytes: u64,
        maximum_ddl_relations: u32,
    ) -> Result<Self, GenerationRetentionError> {
        if !valid_positive_limit(maximum_cascade_rows, MAXIMUM_CASCADE_ROW_BUDGET)
            || !valid_positive_limit(
                maximum_search_relation_bytes,
                MAXIMUM_SEARCH_RELATION_BYTE_BUDGET,
            )
            || maximum_ddl_relations == 0
            || maximum_ddl_relations > MAXIMUM_DDL_RELATIONS
        {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        self.maximum_cascade_rows = maximum_cascade_rows;
        self.maximum_search_relation_bytes = maximum_search_relation_bytes;
        self.maximum_ddl_relations = maximum_ddl_relations;
        Ok(self)
    }

    /// Number of most-recent superseded generations that can never be selected.
    #[must_use]
    pub const fn recent_superseded(self) -> u32 {
        self.recent_superseded
    }

    /// Maximum terminal generations deleted by one transaction.
    #[must_use]
    pub const fn maximum_deletions(self) -> u32 {
        self.maximum_deletions
    }

    /// Minimum database-clock age for collecting an unleased staging generation.
    #[must_use]
    pub const fn stale_staging_age(self) -> Duration {
        self.stale_staging_age
    }

    /// Minimum database-clock age for collecting an unleased ready generation.
    #[must_use]
    pub const fn stale_ready_age(self) -> Duration {
        self.stale_ready_age
    }

    /// Maximum canonical/cascade rows admitted into one cleanup transaction.
    #[must_use]
    pub const fn maximum_cascade_rows(self) -> u64 {
        self.maximum_cascade_rows
    }

    /// Maximum physical bytes of generation search relations dropped per call.
    #[must_use]
    pub const fn maximum_search_relation_bytes(self) -> u64 {
        self.maximum_search_relation_bytes
    }

    /// Maximum dynamic relation drops issued per cleanup transaction.
    #[must_use]
    pub const fn maximum_ddl_relations(self) -> u32 {
        self.maximum_ddl_relations
    }
}

const fn valid_positive_limit(value: u64, maximum: u64) -> bool {
    value > 0 && value <= maximum
}

/// Exact bounded cleanup result after cascades committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationRetentionReport {
    /// Stale unleased staging generations deleted in this batch.
    pub staging_removed: u64,
    /// Old unleased ready generations reconciled in this batch.
    pub ready_removed: u64,
    /// Superseded generations deleted in this batch.
    pub superseded_removed: u64,
    /// Failed generations deleted in this batch.
    pub failed_removed: u64,
    /// Model-scoped vectors removed by generation cascades in this batch.
    pub embeddings_removed: u64,
    /// Current generations still present; valid project state is zero or one.
    pub current_preserved: u64,
    /// Superseded generations retained after this batch.
    pub superseded_preserved: u64,
    /// Failed generations still waiting for a later bounded batch.
    pub failed_remaining: u64,
    /// Staging generations still present, including recent or leased work.
    pub staging_remaining: u64,
    /// Ready generations still protected or waiting for a later bounded batch.
    pub ready_remaining: u64,
    /// Generations with committed partial cleanup still awaiting a later batch.
    pub retiring_remaining: u64,
    /// Number of independently committed, bounded row batches.
    pub batches_committed: u64,
    /// Stable reason more eligible work was left for a later invocation.
    pub deferred_reason: Option<&'static str>,
    /// Canonical and cascading rows admitted under the exact row-work cap.
    pub cascade_rows_removed: u64,
    /// Physical generation search relations removed in the bounded DDL batch.
    pub search_relations_removed: u64,
    /// Physical table and BM25 index bytes admitted under the byte-work cap.
    pub search_relation_bytes_removed: u64,
    /// Thresholded, table-scoped post-commit vacuum outcome.
    pub maintenance: PostRetentionMaintenance,
}

impl GenerationRetentionReport {
    /// Total generations deleted by this invocation.
    #[must_use]
    pub const fn removed(self) -> u64 {
        self.staging_removed + self.ready_removed + self.superseded_removed + self.failed_removed
    }
}

/// Table-scoped post-retention maintenance. Routine cleanup never performs
/// `VACUUM FULL` or an unqualified database-wide `ANALYZE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PostRetentionMaintenance {
    #[default]
    /// Represents the not needed post retention maintenance.
    NotNeeded,
    /// Represents the completed post retention maintenance.
    Completed {
        /// Number of PostgreSQL tables considered for post-retention analysis.
        tables_attempted: u64,
    },
    /// PostgreSQL's configured background maintenance owns reclamation.
    Delegated {
        /// Stable background mechanism responsible for maintenance.
        mechanism: &'static str,
    },
    /// Represents the deferred post retention maintenance.
    Deferred {
        /// Stable reason the bounded maintenance step could not run.
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PostRetentionMaintenancePlan {
    NotNeeded,
    VacuumTables,
    DelegateToAutovacuum,
}

const fn post_retention_maintenance_plan(
    cascade_rows_removed: u64,
    policy: PostRetentionMaintenancePolicy,
) -> PostRetentionMaintenancePlan {
    if cascade_rows_removed < POST_RETENTION_VACUUM_ROW_THRESHOLD {
        return PostRetentionMaintenancePlan::NotNeeded;
    }
    match policy {
        PostRetentionMaintenancePolicy::Immediate => PostRetentionMaintenancePlan::VacuumTables,
        PostRetentionMaintenancePolicy::DelegateToAutovacuum => {
            PostRetentionMaintenancePlan::DelegateToAutovacuum
        }
    }
}

/// Credential-safe generation-retention failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GenerationRetentionError {
    /// Batch size or statement deadline was zero or outside its hard bound.
    #[error("Cartograph generation retention policy is invalid")]
    InvalidPolicy,
    /// The supplied token is not the current exact project migration lease.
    #[error("Cartograph generation retention lost its exact migration lease fence")]
    LeaseFenceLost,
    /// The lease project does not exist in the destination schema.
    #[error("Cartograph generation retention project does not exist")]
    ProjectNotFound,
    /// A PostgreSQL operation failed without rendering connection or query detail.
    #[error("Cartograph PostgreSQL retention operation failed during {operation}")]
    DatabaseOperation {
        /// Stable operation label.
        operation: &'static str,
    },
}

impl CartographDatabase {
    /// Delete a bounded batch of stale unleased staging/ready, failed, and old
    /// superseded generations.
    ///
    /// The current generation, recent or leased staging/ready work, incomplete
    /// imports, and the configured newest superseded histories are never
    /// candidates. The caller must hold an exact, unexpired project
    /// `migration` lease that is not generation-bound.
    /// # Errors
    ///
    /// Returns an error if the migration lease/deadline is invalid, protected
    /// generations become candidates, work caps are exceeded, or cleanup fails.
    pub async fn cleanup_generations(
        &self,
        request: GenerationRetentionRequest<'_>,
    ) -> Result<GenerationRetentionReport, GenerationRetentionError> {
        self.cleanup_generations_with_observer(request, || async {})
            .await
    }

    /// Execute generation cleanup with one observer point after every cascade
    /// relation is locked and the catalog is verified. Live concurrency tests
    /// use this to prove that FK-changing DDL cannot cross the accounting fence.
    #[doc(hidden)]
    pub async fn cleanup_generations_with_observer<Observe, Observed>(
        &self,
        request: GenerationRetentionRequest<'_>,
        observe_catalog: Observe,
    ) -> Result<GenerationRetentionReport, GenerationRetentionError>
    where
        Observe: FnOnce() -> Observed,
        Observed: Future<Output = ()>,
    {
        drain::cleanup(self, request, observe_catalog).await
    }

    async fn vacuum_retention_tables(&self, statement_timeout: Duration) -> Result<u64, ()> {
        let timeout_millis = i64::try_from(statement_timeout.as_millis()).map_err(|_| ())?;
        if timeout_millis == 0 {
            return Err(());
        }
        let mut connection = self.pool.acquire().await.map_err(|_| ())?;
        connection.close_on_drop();
        let prior_timeout = query("SELECT current_setting('statement_timeout')")
            .fetch_one(&mut *connection)
            .await
            .ok()
            .and_then(|row| row.try_get::<String, _>(0).ok())
            .ok_or(())?;
        query("SELECT set_config('statement_timeout', $1, false)")
            .bind(format!("{timeout_millis}ms"))
            .execute(&mut *connection)
            .await
            .map_err(|_| ())?;
        let schema = crate::database::quoted_schema(&self.schema);
        let mut attempted = 0_u64;
        let mut result = Ok(());
        for table in RETENTION_ROW_TABLES {
            let statement = format!(
                r#"VACUUM (
                        ANALYZE,
                        SKIP_LOCKED,
                        INDEX_CLEANUP ON,
                        TRUNCATE OFF,
                        SKIP_DATABASE_STATS
                    ) {schema}."{table}""#
            );
            if query(AssertSqlSafe(statement))
                .execute(&mut *connection)
                .await
                .is_err()
            {
                result = Err(());
                break;
            }
            attempted = attempted.checked_add(1).ok_or(())?;
        }
        let restored = query("SELECT set_config('statement_timeout', $1, false)")
            .bind(prior_timeout)
            .execute(&mut *connection)
            .await
            .map(|_| ())
            .map_err(|_| ());
        result.and(restored).map(|()| attempted)
    }
}

struct RetentionContext<'a> {
    database: &'a CartographDatabase,
    policy: GenerationRetentionPolicy,
    fence: &'a LeaseFence,
    quoted_schema: String,
    /// Monotonic deadline of the enclosing drain transaction, when bounded.
    batch_deadline: Option<tokio::time::Instant>,
}

#[derive(Default)]
struct RemovedGenerationCounts {
    staging: u64,
    ready: u64,
    superseded: u64,
    failed: u64,
    embeddings: u64,
}

struct PreservedGenerationCounts {
    current: u64,
    staging: u64,
    ready: u64,
    superseded: u64,
    failed: u64,
    retiring: u64,
}

struct RetentionCandidate {
    generation_id: GenerationId,
}

struct CandidateWork {
    candidate: RetentionCandidate,
    search_relation_bytes: u64,
    search_relation_present: bool,
}

#[derive(Default)]
struct BoundedCandidateWork {
    candidates: Vec<CandidateWork>,
    search_relation_bytes: u64,
    search_relations: u64,
}

async fn cleanup_transaction<Observe, Observed>(
    connection: &mut sqlx_postgres::PgConnection,
    scope: drain::DrainScope<'_, '_>,
    observe_catalog: Observe,
) -> Result<GenerationRetentionReport, GenerationRetentionError>
where
    Observe: FnOnce() -> Observed,
    Observed: Future<Output = ()>,
{
    let context = scope.context;
    acquire_retention_locks(connection, context).await?;
    require_live_fence(connection, context).await?;
    lock_project(connection, context).await?;
    lock_retention_relations(connection, context).await?;
    verify_retention_cascade_catalog(connection, context).await?;
    observe_catalog().await;
    let candidates = load_terminal_candidates(connection, context, true).await?;
    let relation_budget_blocked = candidates.is_empty()
        && !load_terminal_candidates(connection, context, false)
            .await?
            .is_empty();
    let bounded = bound_candidate_work(connection, context, candidates).await?;
    let progress = drain::delete_rows(connection, scope, &bounded.candidates).await?;
    // Revalidate the complete FK graph while its DDL fence is still held.
    verify_retention_cascade_catalog(connection, context).await?;
    let removed = progress.removed;
    let preserved = load_preserved_counts(connection, context).await?;
    // Row locks prevent takeover, but they do not freeze database-clock expiry.
    require_live_fence(connection, context).await?;
    Ok(GenerationRetentionReport {
        staging_removed: removed.staging,
        ready_removed: removed.ready,
        superseded_removed: removed.superseded,
        failed_removed: removed.failed,
        embeddings_removed: removed.embeddings,
        current_preserved: preserved.current,
        superseded_preserved: preserved.superseded,
        failed_remaining: preserved.failed,
        staging_remaining: preserved.staging,
        ready_remaining: preserved.ready,
        retiring_remaining: preserved.retiring,
        batches_committed: 1,
        deferred_reason: relation_budget_blocked.then_some(
            if context.policy.maximum_ddl_relations == 0 {
                "search_relation_ddl_budget"
            } else {
                "search_relation_byte_budget"
            },
        ),
        cascade_rows_removed: progress.rows,
        search_relations_removed: progress.relations,
        search_relation_bytes_removed: progress.bytes,
        maintenance: PostRetentionMaintenance::NotNeeded,
    })
}

async fn load_terminal_candidates(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    within_relation_budget: bool,
) -> Result<Vec<RetentionCandidate>, GenerationRetentionError> {
    let stale_staging_millis = i64::try_from(context.policy.stale_staging_age.as_millis())
        .map_err(|_| GenerationRetentionError::InvalidPolicy)?;
    let stale_ready_millis = i64::try_from(context.policy.stale_ready_age.as_millis())
        .map_err(|_| GenerationRetentionError::InvalidPolicy)?;
    let sql = format!(
        r#"WITH ranked AS (
                SELECT generation_id, generation_sequence, state, started_at, ready_at,
                    row_number() OVER (PARTITION BY state ORDER BY generation_sequence DESC) AS state_rank
                FROM {schema}."index_generations" WHERE project_id = $1::uuid
            ) SELECT generation_id::text FROM ranked
            WHERE (state IN ('failed', 'retiring')
                OR (state = 'superseded' AND state_rank > $2)
                OR (state = 'ready' AND COALESCE(ready_at, started_at)
                    <= clock_timestamp() - $4 * interval '1 millisecond')
                OR (state = 'staging' AND started_at
                    <= clock_timestamp() - $3 * interval '1 millisecond'))
              AND generation_id IS DISTINCT FROM (
                  SELECT current_generation_id FROM {schema}."projects" WHERE project_id = $1::uuid
              )
              AND NOT EXISTS (
                  SELECT 1 FROM {schema}."project_operation_leases" AS active
                  WHERE active.project_id = $1::uuid AND active.generation_id = ranked.generation_id
                    AND active.expires_at > clock_timestamp()
              )
              AND NOT EXISTS (
                  SELECT 1 FROM {schema}."v1_import_runs" AS recovery
                  WHERE recovery.project_id = $1::uuid AND recovery.generation_id = ranked.generation_id
                    AND recovery.checkpoint <> 'complete'
              )
              AND (COALESCE(pg_total_relation_size(to_regclass(format(
                  '%I.%I', $6::text, 'search_g_' || replace(generation_id::text, '-', '')
              ))), 0) <= $7 AND ($9 > 0 OR to_regclass(format(
                  '%I.%I', $6::text, 'search_g_' || replace(generation_id::text, '-', '')
              )) IS NULL)) = $8
            ORDER BY (state = 'retiring') DESC, generation_sequence ASC LIMIT $5"#,
        schema = context.quoted_schema,
    );
    query(AssertSqlSafe(sql))
        .bind(context.fence.target().project_id().as_str())
        .bind(i64::from(context.policy.recent_superseded))
        .bind(stale_staging_millis)
        .bind(stale_ready_millis)
        .bind(i64::from(if within_relation_budget {
            context.policy.maximum_deletions
        } else {
            1
        }))
        .bind(context.database.schema.as_str())
        .bind(
            i64::try_from(context.policy.maximum_search_relation_bytes)
                .map_err(|_| GenerationRetentionError::InvalidPolicy)?,
        )
        .bind(within_relation_budget)
        .bind(i64::from(context.policy.maximum_ddl_relations))
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("load-terminal-generations"))?
        .iter()
        .map(|row| {
            let value = row
                .try_get::<String, _>(0)
                .map_err(|_| database_error("decode-terminal-generation"))?;
            GenerationId::parse(&value)
                .map(|generation_id| RetentionCandidate { generation_id })
                .map_err(|_| database_error("decode-terminal-generation"))
        })
        .collect()
}

async fn bound_candidate_work(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    candidates: Vec<RetentionCandidate>,
) -> Result<BoundedCandidateWork, GenerationRetentionError> {
    let candidate_work = load_candidate_work(connection, context, candidates).await?;
    select_bounded_candidate_work(context.policy, candidate_work)
}

fn select_bounded_candidate_work(
    policy: GenerationRetentionPolicy,
    candidates: impl IntoIterator<Item = CandidateWork>,
) -> Result<BoundedCandidateWork, GenerationRetentionError> {
    let mut bounded = BoundedCandidateWork::default();
    let maximum_deletions = usize::try_from(policy.maximum_deletions)
        .map_err(|_| database_error("candidate-count-budget"))?;
    for work in candidates {
        if bounded.candidates.len() >= maximum_deletions {
            break;
        }
        if work.search_relation_present
            && bounded.search_relations >= u64::from(policy.maximum_ddl_relations)
        {
            continue;
        }
        let search_relation_bytes = bounded
            .search_relation_bytes
            .checked_add(work.search_relation_bytes)
            .ok_or_else(|| database_error("candidate-byte-budget"))?;
        if search_relation_bytes > policy.maximum_search_relation_bytes {
            continue;
        }
        bounded.search_relation_bytes = search_relation_bytes;
        if work.search_relation_present {
            bounded.search_relations += 1;
        }
        bounded.candidates.push(work);
    }
    Ok(bounded)
}

async fn load_candidate_work(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    candidates: Vec<RetentionCandidate>,
) -> Result<Vec<CandidateWork>, GenerationRetentionError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let generation_ids = candidates
        .iter()
        .map(|candidate| candidate.generation_id.as_str().to_owned())
        .collect::<Vec<_>>();
    // Only catalog metadata is measured. Fact rows are admitted by DELETE LIMIT,
    // never by counting an entire retained generation before enforcing a cap.
    let sql = r"SELECT listed.generation_id::text AS generation_id,
                COALESCE(pg_total_relation_size(to_regclass(format(
                    '%I.%I', $3::text,
                    'search_g_' || replace(listed.generation_id::text, '-', '')
                ))), 0)::bigint AS search_relation_bytes,
                to_regclass(format('%I.%I', $3::text,
                    'search_g_' || replace(listed.generation_id::text, '-', '')
                )) IS NOT NULL AS search_relation_present
            FROM unnest(CAST($2 AS uuid[])) WITH ORDINALITY
                AS listed(generation_id, ordinal)
            WHERE $1::uuid IS NOT NULL ORDER BY listed.ordinal";
    query(AssertSqlSafe(sql))
        .bind(context.fence.target().project_id().as_str())
        .bind(generation_ids)
        .bind(context.database.schema.as_str())
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("load-candidate-work"))?
        .iter()
        .map(|row| {
            let generation_id = row
                .try_get::<String, _>("generation_id")
                .map_err(|_| database_error("decode-candidate-work"))?;
            let generation_id = GenerationId::parse(&generation_id)
                .map_err(|_| database_error("decode-candidate-work"))?;
            let search_relation_bytes = read_named_count(row, "search_relation_bytes")?;
            let search_relation_present = row
                .try_get::<bool, _>("search_relation_present")
                .map_err(|_| database_error("decode-candidate-work"))?;
            Ok(CandidateWork {
                candidate: RetentionCandidate { generation_id },
                search_relation_bytes,
                search_relation_present,
            })
        })
        .collect()
}

async fn verify_retention_cascade_catalog(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    let rows = query(
        r"WITH RECURSIVE cascade_relations(oid, nspname, relname) AS (
                SELECT relations.oid, namespaces.nspname, relations.relname
                FROM pg_catalog.pg_class AS relations
                INNER JOIN pg_catalog.pg_namespace AS namespaces
                    ON namespaces.oid = relations.relnamespace
                WHERE namespaces.nspname = $1
                  AND relations.relname = 'index_generations'
                UNION
                SELECT children.oid, child_namespaces.nspname, children.relname
                FROM cascade_relations AS parents
                INNER JOIN pg_catalog.pg_constraint AS constraints
                    ON constraints.confrelid = parents.oid
                   AND constraints.contype = 'f'
                   AND constraints.confdeltype = 'c'
                INNER JOIN pg_catalog.pg_class AS children
                    ON children.oid = constraints.conrelid
                INNER JOIN pg_catalog.pg_namespace AS child_namespaces
                    ON child_namespaces.oid = children.relnamespace
            )
            SELECT nspname, relname
            FROM cascade_relations
            WHERE nspname <> $1 OR relname <> 'index_generations'
            ORDER BY nspname, relname",
    )
    .bind(context.database.schema.as_str())
    .fetch_all(&mut *connection)
    .await
    .map_err(|_| database_error("verify-cascade-catalog"))?;
    let actual = rows
        .iter()
        .map(|row| {
            let namespace = row
                .try_get::<String, _>(0)
                .map_err(|_| database_error("decode-cascade-catalog"))?;
            let relation = row
                .try_get::<String, _>(1)
                .map_err(|_| database_error("decode-cascade-catalog"))?;
            Ok((namespace, relation))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = RETENTION_ROW_TABLES[1..]
        .iter()
        .map(|table| {
            (
                context.database.schema.as_str().to_owned(),
                (*table).to_owned(),
            )
        })
        .collect::<Vec<_>>();
    expected.sort_unstable();
    if actual == expected {
        drain::verify_delete_order(connection, context).await
    } else {
        Err(database_error("cascade-catalog-mismatch"))
    }
}

async fn acquire_retention_locks(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    // Match the first lock taken by append-only migrations so a rolling binary
    // cannot validate one schema shape while another binary changes it.
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(crate::migrations::migration_lock_key(
            &context.database.schema,
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("schema-migration-lock"))?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(project_lock_key(
            &context.database.schema,
            context.fence.target().project_id(),
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("operation-lock"))?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{PUBLICATION_LOCK_NAMESPACE}:{}:{}",
            context.database.schema.as_str(),
            context.fence.target().project_id()
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("publication-lock"))?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{RETENTION_LOCK_NAMESPACE}:{}:{}",
            context.database.schema.as_str(),
            context.fence.target().project_id()
        ))
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("retention-lock"))?;
    Ok(())
}

async fn lock_retention_relations(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    // PostgreSQL takes SHARE ROW EXCLUSIVE on a referenced relation while
    // adding a foreign key. ROW EXCLUSIVE conflicts with that mode while still
    // permitting ordinary concurrent DML, so the complete known cascade graph
    // cannot change between verification, accounting, and DELETE.
    let relations = RETENTION_ROW_TABLES
        .iter()
        .map(|table| format!(r#"{}."{table}""#, context.quoted_schema))
        .collect::<Vec<_>>()
        .join(", ");
    query(AssertSqlSafe(format!(
        "LOCK TABLE {relations} IN ROW EXCLUSIVE MODE"
    )))
    .execute(connection)
    .await
    .map(|_| ())
    .map_err(|_| database_error("cascade-relation-lock"))
}

async fn lock_project(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    let project_sql = format!(
        r#"SELECT 1 FROM {}."projects"
            WHERE project_id = CAST($1 AS uuid) FOR UPDATE"#,
        context.quoted_schema
    );
    if query(AssertSqlSafe(project_sql))
        .bind(context.fence.target().project_id().as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| database_error("lock-project"))?
        .is_none()
    {
        Err(GenerationRetentionError::ProjectNotFound)
    } else {
        Ok(())
    }
}

async fn load_preserved_counts(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<PreservedGenerationCounts, GenerationRetentionError> {
    let remaining_sql = format!(
        r#"SELECT
            count(*) FILTER (WHERE state = 'current')::bigint AS current_preserved,
            count(*) FILTER (WHERE state = 'staging')::bigint AS staging_remaining,
            count(*) FILTER (WHERE state = 'ready')::bigint AS ready_remaining,
            count(*) FILTER (WHERE state = 'superseded')::bigint AS superseded_preserved,
            count(*) FILTER (WHERE state = 'failed')::bigint AS failed_remaining,
            count(*) FILTER (WHERE state = 'retiring')::bigint AS retiring_remaining
        FROM {}."index_generations"
        WHERE project_id = CAST($1 AS uuid)"#,
        context.quoted_schema
    );
    let remaining = query(AssertSqlSafe(remaining_sql))
        .bind(context.fence.target().project_id().as_str())
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("count-retained-generations"))?;
    Ok(PreservedGenerationCounts {
        current: read_named_count(&remaining, "current_preserved")?,
        staging: read_named_count(&remaining, "staging_remaining")?,
        ready: read_named_count(&remaining, "ready_remaining")?,
        superseded: read_named_count(&remaining, "superseded_preserved")?,
        failed: read_named_count(&remaining, "failed_remaining")?,
        retiring: read_named_count(&remaining, "retiring_remaining")?,
    })
}

fn validate_fence_shape(fence: &LeaseFence) -> Result<(), GenerationRetentionError> {
    if fence.target().operation() != ProjectOperation::Migration
        || fence.target().generation_id().is_some()
    {
        Err(GenerationRetentionError::LeaseFenceLost)
    } else {
        Ok(())
    }
}

async fn require_live_fence(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    let sql = format!(
        r#"SELECT 1 FROM {}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND operation = 'migration'
              AND lease_id = CAST($2 AS uuid)
              AND generation_id IS NULL
              AND expires_at > clock_timestamp()
            FOR UPDATE"#,
        context.quoted_schema
    );
    let exists = query(AssertSqlSafe(sql))
        .bind(context.fence.target().project_id().as_str())
        .bind(context.fence.lease_id().as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("lock-lease"))?
        .is_some();
    exists.ok_or(GenerationRetentionError::LeaseFenceLost)
}

fn read_named_count(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<u64, GenerationRetentionError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|_| database_error("decode-counts"))?;
    u64::try_from(value).map_err(|_| database_error("decode-counts"))
}

const fn database_error(operation: &'static str) -> GenerationRetentionError {
    GenerationRetentionError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::time::Duration;

    use cartograph_domain::GenerationId;

    use super::{
        CandidateWork, DEFAULT_STALE_STAGING_AGE, GenerationRetentionError,
        GenerationRetentionPolicy, INVALID_DELETE_BATCH, MAXIMUM_CASCADE_ROW_BUDGET,
        MAXIMUM_DELETE_BATCH, MAXIMUM_STALE_STAGING_AGE, POST_RETENTION_VACUUM_ROW_THRESHOLD,
        PostRetentionMaintenance, PostRetentionMaintenancePlan, PostRetentionMaintenancePolicy,
        RetentionCandidate, post_retention_maintenance_plan, select_bounded_candidate_work,
    };

    const TEST_RETAINED_SUPERSEDED: u32 = 2;
    const TEST_DELETE_BATCH: u32 = 10;
    const TEST_OVERSIZED_DELETE_BATCH: u32 = MAXIMUM_DELETE_BATCH + 1;

    #[test]
    fn retention_requires_a_nonzero_bounded_delete_batch() {
        assert_matches!(
            GenerationRetentionPolicy::new(TEST_RETAINED_SUPERSEDED, INVALID_DELETE_BATCH),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_matches!(
            GenerationRetentionPolicy::new(TEST_RETAINED_SUPERSEDED, TEST_OVERSIZED_DELETE_BATCH),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_matches!(
            GenerationRetentionPolicy::new(TEST_RETAINED_SUPERSEDED, TEST_DELETE_BATCH),
            Ok(policy)
                if policy.recent_superseded() == TEST_RETAINED_SUPERSEDED
                    && policy.maximum_deletions() == TEST_DELETE_BATCH
                    && policy.stale_staging_age() == DEFAULT_STALE_STAGING_AGE
        );
    }

    #[test]
    fn stale_staging_age_is_nonzero_and_hard_bounded() {
        let policy = GenerationRetentionPolicy::new(TEST_RETAINED_SUPERSEDED, TEST_DELETE_BATCH)
            .unwrap_or_else(|error| panic!("base retention policy failed: {error}"));
        assert_eq!(
            policy.with_stale_staging_age(Duration::ZERO),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_eq!(
            policy.with_stale_staging_age(MAXIMUM_STALE_STAGING_AGE + Duration::from_secs(1)),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_matches!(
            policy.with_stale_staging_age(Duration::from_mins(1)),
            Ok(updated) if updated.stale_staging_age() == Duration::from_mins(1)
        );
    }

    #[test]
    fn cascade_row_override_is_nonzero_and_hard_bounded() {
        let policy = GenerationRetentionPolicy::new(TEST_RETAINED_SUPERSEDED, TEST_DELETE_BATCH)
            .unwrap_or_else(|error| panic!("base retention policy failed: {error}"));
        assert_eq!(
            policy.with_maximum_cascade_rows(0),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_eq!(
            policy.with_maximum_cascade_rows(MAXIMUM_CASCADE_ROW_BUDGET + 1),
            Err(GenerationRetentionError::InvalidPolicy)
        );
        assert_matches!(
            policy.with_maximum_cascade_rows(7_700_000),
            Ok(updated) if updated.maximum_cascade_rows() == 7_700_000
        );
    }

    #[test]
    fn automatic_retention_delegates_large_cascades_to_autovacuum() {
        assert_eq!(
            post_retention_maintenance_plan(
                POST_RETENTION_VACUUM_ROW_THRESHOLD,
                PostRetentionMaintenancePolicy::DelegateToAutovacuum,
            ),
            PostRetentionMaintenancePlan::DelegateToAutovacuum
        );
        let serialized = serde_json::to_value(PostRetentionMaintenance::Delegated {
            mechanism: "autovacuum",
        })
        .unwrap_or_else(|error| panic!("delegated maintenance serialization failed: {error}"));
        assert_eq!(serialized["state"], "delegated");
        assert_eq!(serialized["mechanism"], "autovacuum");
    }

    #[test]
    fn explicit_retention_preserves_thresholded_table_maintenance() {
        assert_eq!(
            post_retention_maintenance_plan(
                POST_RETENTION_VACUUM_ROW_THRESHOLD - 1,
                PostRetentionMaintenancePolicy::Immediate,
            ),
            PostRetentionMaintenancePlan::NotNeeded
        );
        assert_eq!(
            post_retention_maintenance_plan(
                POST_RETENTION_VACUUM_ROW_THRESHOLD,
                PostRetentionMaintenancePolicy::Immediate,
            ),
            PostRetentionMaintenancePlan::VacuumTables
        );
    }

    #[test]
    fn oversized_oldest_candidate_does_not_starve_smaller_later_work() {
        let policy = GenerationRetentionPolicy::new(0, 3)
            .and_then(|policy| policy.with_work_limits(10, 100, 3))
            .unwrap_or_else(|error| panic!("retention test policy failed: {error}"));
        let oversized = candidate_work("00000000-0000-0000-0000-000000000001", 101, true);
        let smaller = candidate_work("00000000-0000-0000-0000-000000000002", 20, true);

        let selected = select_bounded_candidate_work(policy, [oversized, smaller])
            .unwrap_or_else(|error| panic!("candidate selection failed: {error}"));

        assert_eq!(selected.candidates.len(), 1);
        assert_eq!(
            selected.candidates[0].candidate.generation_id.as_str(),
            "00000000-0000-0000-0000-000000000002"
        );
        assert_eq!(selected.search_relation_bytes, 20);
        assert_eq!(selected.search_relations, 1);
    }

    #[test]
    fn relation_free_candidates_are_not_limited_by_the_ddl_budget() {
        let policy = GenerationRetentionPolicy::new(0, 100)
            .and_then(|policy| policy.with_work_limits(1_000, 1_000, 2))
            .unwrap_or_else(|error| panic!("retention test policy failed: {error}"));
        let candidates = (1_u32..=100)
            .map(|index| candidate_work(&format!("00000000-0000-0000-0000-{index:012}"), 0, false));

        let selected = select_bounded_candidate_work(policy, candidates)
            .unwrap_or_else(|error| panic!("candidate selection failed: {error}"));

        assert_eq!(selected.candidates.len(), 100);
        assert_eq!(selected.search_relation_bytes, 0);
        assert_eq!(selected.search_relations, 0);
    }

    #[test]
    fn relation_bearing_candidates_remain_limited_by_the_ddl_budget() {
        let policy = GenerationRetentionPolicy::new(0, 100)
            .and_then(|policy| policy.with_work_limits(1_000, 1_000, 2))
            .unwrap_or_else(|error| panic!("retention test policy failed: {error}"));
        let candidates = (0_u32..5)
            .map(|index| candidate_work(&format!("10000000-0000-0000-0000-{index:012}"), 1, true));

        let selected = select_bounded_candidate_work(policy, candidates)
            .unwrap_or_else(|error| panic!("candidate selection failed: {error}"));

        assert_eq!(selected.candidates.len(), 2);
        assert_eq!(selected.search_relations, 2);
    }

    fn candidate_work(
        generation_id: &str,
        relation_bytes: u64,
        search_relation_present: bool,
    ) -> CandidateWork {
        CandidateWork {
            candidate: RetentionCandidate {
                generation_id: GenerationId::parse(generation_id)
                    .unwrap_or_else(|error| panic!("test generation id failed: {error}")),
            },
            search_relation_bytes: relation_bytes,
            search_relation_present,
        }
    }
}
