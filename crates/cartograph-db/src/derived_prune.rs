use std::time::Duration;

use cartograph_domain::ProjectOperation;
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use crate::{CartographDatabase, LeaseFence, database::set_local_statement_timeout};

const MAXIMUM_DELETE_BATCH: u32 = 10_000;
const PUBLICATION_LOCK_NAMESPACE: &str = "cartograph-v2-publish";
const RETENTION_LOCK_NAMESPACE: &str = "cartograph-v2-generation-retention";

/// Bounded age policy for content-derived rows no longer referenced by the
/// current immutable generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivedStorePrunePolicy {
    maximum_age: Duration,
    maximum_deletions_per_store: u32,
}

impl DerivedStorePrunePolicy {
    /// Keep orphaned rows newer than `maximum_age` and cap each physical store.
    /// # Errors
    ///
    /// Returns an error if `maximum_deletions_per_store` is zero or exceeds
    /// the per-store deletion-batch maximum.
    pub const fn new(
        maximum_age: Duration,
        maximum_deletions_per_store: u32,
    ) -> Result<Self, DerivedStorePruneError> {
        if maximum_deletions_per_store == 0 || maximum_deletions_per_store > MAXIMUM_DELETE_BATCH {
            return Err(DerivedStorePruneError::InvalidPolicy);
        }
        Ok(Self {
            maximum_age,
            maximum_deletions_per_store,
        })
    }

    #[must_use]
    /// Returns the maximum age.
    pub const fn maximum_age(self) -> Duration {
        self.maximum_age
    }

    #[must_use]
    /// Returns the maximum deletions per store.
    pub const fn maximum_deletions_per_store(self) -> u32 {
        self.maximum_deletions_per_store
    }
}

/// Exact lease fence and statement deadline for one cold-store prune.
pub struct DerivedStorePruneRequest<'a> {
    policy: DerivedStorePrunePolicy,
    fence: &'a LeaseFence,
    statement_timeout: Duration,
}

impl<'a> DerivedStorePruneRequest<'a> {
    #[must_use]
    /// Creates a validated derived store prune request.
    pub const fn new(
        policy: DerivedStorePrunePolicy,
        fence: &'a LeaseFence,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            policy,
            fence,
            statement_timeout,
        }
    }
}

/// Exact committed deletion counts for v1-compatible cold-orphan cleanup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedStorePruneReport {
    /// Cutoff for this record.
    pub cutoff: String,
    /// Maximum number of age millis permitted by this request.
    pub maximum_age_millis: u64,
    /// Maximum number of deletions per store permitted by this request.
    pub maximum_deletions_per_store: u32,
    /// Number of summaries pruned.
    pub summaries_pruned: u64,
    /// Number of roles pruned.
    pub roles_pruned: u64,
    /// Number of embeddings pruned.
    pub embeddings_pruned: u64,
    /// Whether additional derived artifact candidates were omitted.
    pub artifacts_truncated: bool,
    /// Whether additional historical embedding candidates were omitted.
    pub embeddings_truncated: bool,
}

impl DerivedStorePruneReport {
    #[must_use]
    /// Returns the total pruned.
    pub const fn total_pruned(&self) -> u64 {
        self.summaries_pruned + self.roles_pruned + self.embeddings_pruned
    }
}

/// Credential-safe cold-store cleanup failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DerivedStorePruneError {
    #[error("Cartograph derived-store prune policy is invalid")]
    /// The supplied policy contains inconsistent or out-of-range limits.
    InvalidPolicy,
    #[error("Cartograph derived-store prune lost its exact migration lease fence")]
    /// The operation lost its exact project lease fence.
    LeaseFenceLost,
    #[error("Cartograph derived-store prune project does not exist")]
    /// No durable project matches the supplied identity.
    ProjectNotFound,
    #[error("Cartograph PostgreSQL derived-store prune failed during {operation}")]
    /// PostgreSQL could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed PostgreSQL phase.
        operation: &'static str,
    },
}

impl CartographDatabase {
    /// Delete cold, current-generation-unreferenced summary/role artifacts and
    /// vectors belonging only to terminal historical generations.
    ///
    /// Notes, sessions, active/current vectors, structural generations, and
    /// model indexes are never candidates. The caller must hold the exact
    /// project-wide migration lease used by generation retention.
    /// # Errors
    ///
    /// Returns an error if the timeout or lease fence is invalid/stale, a
    /// protected row becomes eligible, or a bounded derived-store delete fails.
    pub async fn prune_cold_derived_store(
        &self,
        request: DerivedStorePruneRequest<'_>,
    ) -> Result<DerivedStorePruneReport, DerivedStorePruneError> {
        validate_request(&request)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("begin"))?;
        set_local_statement_timeout(&mut transaction, request.statement_timeout)
            .await
            .map_err(|()| DerivedStorePruneError::InvalidPolicy)?;
        let schema = crate::database::quoted_schema(&self.schema);
        acquire_locks(&mut transaction, self, request.fence).await?;
        require_live_fence(&mut transaction, &schema, request.fence).await?;
        lock_project(&mut transaction, &schema, request.fence).await?;
        let age_seconds = request.policy.maximum_age.as_secs_f64();
        let cutoff =
            query("SELECT (clock_timestamp() - make_interval(secs => $1::double precision))::text")
                .bind(age_seconds)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| database_error("cutoff"))?
                .try_get::<String, _>(0)
                .map_err(|_| database_error("decode-cutoff"))?;
        let probe_limit = request
            .policy
            .maximum_deletions_per_store
            .checked_add(1)
            .ok_or(DerivedStorePruneError::InvalidPolicy)?;
        let batch = PruneBatchContext {
            schema: &schema,
            fence: request.fence,
            cutoff: &cutoff,
            maximum: request.policy.maximum_deletions_per_store,
            probe_limit,
        };
        let artifact_row = prune_artifacts(&mut transaction, &batch).await?;
        let embedding_row = prune_embeddings(&mut transaction, &batch).await?;
        require_live_fence(&mut transaction, &schema, request.fence).await?;
        let report = DerivedStorePruneReport {
            cutoff,
            maximum_age_millis: u64::try_from(request.policy.maximum_age.as_millis())
                .unwrap_or(u64::MAX),
            maximum_deletions_per_store: request.policy.maximum_deletions_per_store,
            summaries_pruned: nonnegative_count(&artifact_row, "summaries_pruned")?,
            roles_pruned: nonnegative_count(&artifact_row, "roles_pruned")?,
            embeddings_pruned: nonnegative_count(&embedding_row, "embeddings_pruned")?,
            artifacts_truncated: artifact_row
                .try_get("truncated")
                .map_err(|_| database_error("decode-artifact-truncation"))?,
            embeddings_truncated: embedding_row
                .try_get("truncated")
                .map_err(|_| database_error("decode-embedding-truncation"))?,
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("commit"))?;
        Ok(report)
    }
}

fn validate_request(request: &DerivedStorePruneRequest<'_>) -> Result<(), DerivedStorePruneError> {
    if request.statement_timeout.is_zero() || !request.policy.maximum_age.as_secs_f64().is_finite()
    {
        return Err(DerivedStorePruneError::InvalidPolicy);
    }
    if request.fence.target().operation() != ProjectOperation::Migration
        || request.fence.target().generation_id().is_some()
    {
        return Err(DerivedStorePruneError::LeaseFenceLost);
    }
    Ok(())
}

async fn acquire_locks(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    fence: &LeaseFence,
) -> Result<(), DerivedStorePruneError> {
    for key in [
        crate::leases::project_lock_key(&database.schema, fence.target().project_id()),
        format!(
            "{PUBLICATION_LOCK_NAMESPACE}:{}:{}",
            database.schema.as_str(),
            fence.target().project_id()
        ),
        format!(
            "{RETENTION_LOCK_NAMESPACE}:{}:{}",
            database.schema.as_str(),
            fence.target().project_id()
        ),
    ] {
        query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut *connection)
            .await
            .map_err(|_| database_error("lock"))?;
    }
    Ok(())
}

async fn require_live_fence(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    fence: &LeaseFence,
) -> Result<(), DerivedStorePruneError> {
    let sql = format!(
        r#"SELECT 1 FROM {schema}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND operation = 'migration'
              AND lease_id = CAST($2 AS uuid)
              AND generation_id IS NULL
              AND expires_at > clock_timestamp()
            FOR UPDATE"#
    );
    let live = query(AssertSqlSafe(sql))
        .bind(fence.target().project_id().as_str())
        .bind(fence.lease_id().as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("fence"))?
        .is_some();
    live.ok_or(DerivedStorePruneError::LeaseFenceLost)
}

async fn lock_project(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &str,
    fence: &LeaseFence,
) -> Result<(), DerivedStorePruneError> {
    let sql = format!(
        r#"SELECT 1 FROM {schema}."projects"
            WHERE project_id = CAST($1 AS uuid) FOR UPDATE"#
    );
    if query(AssertSqlSafe(sql))
        .bind(fence.target().project_id().as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("project-lock"))?
        .is_some()
    {
        Ok(())
    } else {
        Err(DerivedStorePruneError::ProjectNotFound)
    }
}

struct PruneBatchContext<'value> {
    schema: &'value str,
    fence: &'value LeaseFence,
    cutoff: &'value str,
    maximum: u32,
    probe_limit: u32,
}

async fn prune_artifacts(
    connection: &mut sqlx_postgres::PgConnection,
    context: &PruneBatchContext<'_>,
) -> Result<sqlx_postgres::PgRow, DerivedStorePruneError> {
    let PruneBatchContext {
        schema,
        fence,
        cutoff,
        maximum,
        probe_limit,
    } = context;
    let sql = format!(
        r#"WITH current AS (
                SELECT current_generation_id AS generation_id
                FROM {schema}."projects"
                WHERE project_id = CAST($1 AS uuid)
            ), candidates AS MATERIALIZED (
                SELECT artifacts.id, artifacts.artifact_kind,
                       row_number() OVER (ORDER BY artifacts.updated_at, artifacts.id) AS ordinal
                FROM {schema}."agent_artifacts" AS artifacts
                WHERE artifacts.project_id = CAST($1 AS uuid)
                  AND artifacts.artifact_kind IN ('summary', 'role')
                  AND artifacts.updated_at <= CAST($2 AS timestamptz)
                  AND NOT EXISTS (
                      SELECT 1
                      FROM {schema}."symbols" AS symbols
                      JOIN current ON current.generation_id = symbols.generation_id
                      WHERE symbols.project_id = artifacts.project_id
                        AND symbols.symbol_id::text = artifacts.scope_key
                        AND symbols.structural_digest = artifacts.source_digest
                  )
                ORDER BY artifacts.updated_at, artifacts.id
                LIMIT $4
            ), deleted AS (
                DELETE FROM {schema}."agent_artifacts" AS artifacts
                USING candidates
                WHERE artifacts.id = candidates.id AND candidates.ordinal <= $3
                RETURNING artifacts.artifact_kind
            )
            SELECT count(*) FILTER (WHERE artifact_kind = 'summary')::bigint AS summaries_pruned,
                   count(*) FILTER (WHERE artifact_kind = 'role')::bigint AS roles_pruned,
                   (SELECT count(*) > $3 FROM candidates) AS truncated
            FROM deleted"#
    );
    query(AssertSqlSafe(sql))
        .bind(fence.target().project_id().as_str())
        .bind(cutoff)
        .bind(i64::from(*maximum))
        .bind(i64::from(*probe_limit))
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("prune-artifacts"))
}

async fn prune_embeddings(
    connection: &mut sqlx_postgres::PgConnection,
    context: &PruneBatchContext<'_>,
) -> Result<sqlx_postgres::PgRow, DerivedStorePruneError> {
    let PruneBatchContext {
        schema,
        fence,
        cutoff,
        maximum,
        probe_limit,
    } = context;
    let sql = format!(
        r#"WITH candidates AS MATERIALIZED (
                SELECT embeddings.ctid AS row_id,
                       row_number() OVER (
                           ORDER BY embeddings.updated_at, embeddings.generation_id,
                                    embeddings.document_id, embeddings.model_id
                       ) AS ordinal
                FROM {schema}."document_embeddings" AS embeddings
                JOIN {schema}."index_generations" AS generations
                  ON generations.project_id = embeddings.project_id
                 AND generations.generation_id = embeddings.generation_id
                WHERE embeddings.project_id = CAST($1 AS uuid)
                  AND generations.state IN ('superseded', 'failed')
                  AND embeddings.updated_at <= CAST($2 AS timestamptz)
                ORDER BY embeddings.updated_at, embeddings.generation_id,
                         embeddings.document_id, embeddings.model_id
                LIMIT $4
            ), deleted AS (
                DELETE FROM {schema}."document_embeddings" AS embeddings
                USING candidates
                WHERE embeddings.ctid = candidates.row_id AND candidates.ordinal <= $3
                RETURNING 1
            )
            SELECT count(*)::bigint AS embeddings_pruned,
                   (SELECT count(*) > $3 FROM candidates) AS truncated
            FROM deleted"#
    );
    query(AssertSqlSafe(sql))
        .bind(fence.target().project_id().as_str())
        .bind(cutoff)
        .bind(i64::from(*maximum))
        .bind(i64::from(*probe_limit))
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("prune-embeddings"))
}

fn nonnegative_count(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<u64, DerivedStorePruneError> {
    row.try_get::<i64, _>(column)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| database_error("decode-count"))
}

const fn database_error(operation: &'static str) -> DerivedStorePruneError {
    DerivedStorePruneError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_accepts_zero_age_and_rejects_unbounded_batches() {
        assert!(DerivedStorePrunePolicy::new(Duration::ZERO, 1).is_ok());
        assert_eq!(
            DerivedStorePrunePolicy::new(Duration::from_secs(1), 0),
            Err(DerivedStorePruneError::InvalidPolicy)
        );
        assert_eq!(
            DerivedStorePrunePolicy::new(Duration::from_secs(1), MAXIMUM_DELETE_BATCH + 1),
            Err(DerivedStorePruneError::InvalidPolicy)
        );
    }
}
