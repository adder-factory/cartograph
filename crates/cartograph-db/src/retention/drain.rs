use super::{
    CandidateWork, CartographDatabase, GenerationRetentionError, GenerationRetentionReport,
    GenerationRetentionRequest, PostRetentionMaintenance, PostRetentionMaintenancePlan,
    RemovedGenerationCounts, RetentionContext, cleanup_transaction, database_error,
    post_retention_maintenance_plan, validate_fence_shape,
};
use sqlx_core::{acquire::Acquire, query::query, row::Row, sql_str::AssertSqlSafe};
use std::{future::Future, time::Duration};

const ROWS_PER_TRANSACTION: u64 = 10_000;
const GENERATIONS_PER_TRANSACTION: u32 = 32;
const MAXIMUM_TRANSACTIONS: u64 = 512;
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(10);

// Children precede every FK parent. The catalog verifies this order under the
// same relation locks that exclude FK-changing DDL before any row is deleted.
const DELETE_ORDER: [&str; 27] = [
    "native_generation_spill_rows",
    "native_generation_spill_files",
    "native_generation_spill_symbols",
    "native_generation_spill_edges",
    "native_generation_spill_references",
    "native_generation_spill_numerical_sites",
    "native_generation_spill_documents",
    "native_generation_spill_batches",
    "native_generation_spills",
    "document_embeddings",
    "search_documents",
    "edges",
    "references",
    "numerical_sites",
    "symbol_coverage",
    "symbol_similarity_edges",
    "symbol_similarity_builds",
    "symbol_issues",
    "issue_history_refreshes",
    "summary_priority_queue",
    "structural_findings",
    "structural_finding_runs",
    "symbols",
    "files",
    "project_operation_leases",
    "generation_search_relations",
    "index_generations",
];

pub(super) async fn cleanup<Observe, Observed>(
    database: &CartographDatabase,
    request: GenerationRetentionRequest<'_>,
    observe: Observe,
) -> Result<GenerationRetentionReport, GenerationRetentionError>
where
    Observe: FnOnce() -> Observed,
    Observed: Future<Output = ()>,
{
    validate_fence_shape(request.fence)?;
    if request.statement_timeout.is_zero() {
        return Err(GenerationRetentionError::InvalidPolicy);
    }
    let started = tokio::time::Instant::now();
    let deadline = started
        .checked_add(request.statement_timeout)
        .ok_or(GenerationRetentionError::InvalidPolicy)?;
    let mut total: Option<GenerationRetentionReport> = None;
    let mut observer = Some(observe);
    for _ in 0..MAXIMUM_TRANSACTIONS {
        let remaining = request.statement_timeout.saturating_sub(started.elapsed());
        let Some(policy) = remaining_policy(request, total, remaining) else {
            break;
        };
        let context = RetentionContext {
            database,
            policy,
            fence: request.fence,
            quoted_schema: crate::database::quoted_schema(&database.schema),
        };
        let batch_deadline = deadline.min(tokio::time::Instant::now() + TRANSACTION_TIMEOUT);
        let result = one_transaction(&context, batch_deadline, || async {
            if let Some(observe) = observer.take() {
                observe().await;
            }
        })
        .await;
        match result {
            Ok(report) => {
                let progress =
                    report.cascade_rows_removed != 0 || report.search_relations_removed != 0;
                total = Some(merge(total, report));
                if !progress {
                    break;
                }
            }
            Err(error) => {
                let Some(report) = total.as_mut() else {
                    return Err(error);
                };
                report.deferred_reason = Some(error_reason(&error));
                break;
            }
        }
    }
    let mut report = total.ok_or(GenerationRetentionError::InvalidPolicy)?;
    if report.deferred_reason.is_none() && work_budget_reached(&report, request, started.elapsed())
    {
        report.deferred_reason = Some("work_budget_reached");
    }
    let maintenance = post_retention_maintenance_plan(
        report.cascade_rows_removed,
        request.post_retention_maintenance,
    );
    report.maintenance = finish_maintenance(database, maintenance, deadline).await;
    Ok(report)
}

fn work_budget_reached(
    report: &GenerationRetentionReport,
    request: GenerationRetentionRequest<'_>,
    elapsed: Duration,
) -> bool {
    report.retiring_remaining > 0
        || report.cascade_rows_removed >= request.policy.maximum_cascade_rows
        || report.removed() >= u64::from(request.policy.maximum_deletions)
        || elapsed >= request.statement_timeout
        || report.batches_committed >= MAXIMUM_TRANSACTIONS
}

fn remaining_policy(
    request: GenerationRetentionRequest<'_>,
    total: Option<GenerationRetentionReport>,
    remaining: Duration,
) -> Option<super::GenerationRetentionPolicy> {
    if remaining.is_zero() {
        return None;
    }
    let mut policy = request.policy;
    if let Some(total) = total {
        policy.maximum_deletions = policy
            .maximum_deletions
            .checked_sub(u32::try_from(total.removed()).ok()?)?;
        policy.maximum_cascade_rows = policy
            .maximum_cascade_rows
            .checked_sub(total.cascade_rows_removed)?;
        policy.maximum_search_relation_bytes = policy
            .maximum_search_relation_bytes
            .checked_sub(total.search_relation_bytes_removed)?;
        policy.maximum_ddl_relations = policy
            .maximum_ddl_relations
            .checked_sub(u32::try_from(total.search_relations_removed).ok()?)?;
    }
    if policy.maximum_deletions == 0 || policy.maximum_cascade_rows == 0 {
        return None;
    }
    policy.maximum_cascade_rows = policy.maximum_cascade_rows.min(ROWS_PER_TRANSACTION);
    policy.maximum_deletions = policy.maximum_deletions.min(GENERATIONS_PER_TRANSACTION);
    Some(policy)
}

async fn one_transaction<Observe, Observed>(
    context: &RetentionContext<'_>,
    deadline: tokio::time::Instant,
    observe: Observe,
) -> Result<GenerationRetentionReport, GenerationRetentionError>
where
    Observe: FnOnce() -> Observed,
    Observed: Future<Output = ()>,
{
    let mut connection = tokio::time::timeout_at(deadline, context.database.pool.acquire())
        .await
        .map_err(|_| database_error("acquire-deadline"))?
        .map_err(|_| database_error("acquire"))?;
    let result = tokio::time::timeout_at(deadline, async {
        let mut transaction = connection
            .begin()
            .await
            .map_err(|_| database_error("begin"))?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        crate::database::set_local_statement_timeout(&mut transaction, remaining)
            .await
            .map_err(|()| GenerationRetentionError::InvalidPolicy)?;
        match cleanup_transaction(&mut transaction, context, observe).await {
            Ok(report) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error("commit"))?;
                Ok(report)
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| database_error("rollback"))?;
                Err(error)
            }
        }
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        // A timed-out transaction must not be reused by the independent cache
        // pass. Dropping the detached raw connection closes its socket; no
        // unbounded rollback is awaited after the invocation deadline.
        drop(connection.detach());
        Err(database_error("batch-deadline"))
    }
}

fn merge(
    total: Option<GenerationRetentionReport>,
    mut next: GenerationRetentionReport,
) -> GenerationRetentionReport {
    if let Some(total) = total {
        next.staging_removed += total.staging_removed;
        next.ready_removed += total.ready_removed;
        next.superseded_removed += total.superseded_removed;
        next.failed_removed += total.failed_removed;
        next.embeddings_removed += total.embeddings_removed;
        next.cascade_rows_removed += total.cascade_rows_removed;
        next.search_relations_removed += total.search_relations_removed;
        next.search_relation_bytes_removed += total.search_relation_bytes_removed;
        next.batches_committed += total.batches_committed;
    }
    next
}

pub(super) const fn error_reason(error: &GenerationRetentionError) -> &'static str {
    match error {
        GenerationRetentionError::LeaseFenceLost => "lease_fence_lost_after_progress",
        GenerationRetentionError::DatabaseOperation { operation } => operation,
        GenerationRetentionError::InvalidPolicy => "invalid_policy",
        GenerationRetentionError::ProjectNotFound => "project_not_found",
    }
}

async fn finish_maintenance(
    database: &CartographDatabase,
    plan: PostRetentionMaintenancePlan,
    deadline: tokio::time::Instant,
) -> PostRetentionMaintenance {
    match plan {
        PostRetentionMaintenancePlan::NotNeeded => PostRetentionMaintenance::NotNeeded,
        PostRetentionMaintenancePlan::DelegateToAutovacuum => PostRetentionMaintenance::Delegated {
            mechanism: "autovacuum",
        },
        PostRetentionMaintenancePlan::VacuumTables => match tokio::time::timeout_at(
            deadline,
            database.vacuum_retention_tables(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ),
        )
        .await
        {
            Ok(Ok(tables_attempted)) => PostRetentionMaintenance::Completed { tables_attempted },
            Ok(Err(())) | Err(_) => PostRetentionMaintenance::Deferred {
                reason: "table_maintenance_unavailable",
            },
        },
    }
}

#[derive(Default)]
pub(super) struct Progress {
    pub removed: RemovedGenerationCounts,
    pub rows: u64,
    pub relations: u64,
    pub bytes: u64,
}

pub(super) async fn delete_rows(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    candidates: &[CandidateWork],
) -> Result<Progress, GenerationRetentionError> {
    let mut progress = Progress::default();
    for work in candidates {
        if progress.rows >= context.policy.maximum_cascade_rows {
            break;
        }
        claim_generation(connection, context, work).await?;
        if work.search_relation_present {
            crate::search_relation::drop_generation_search_relation(
                connection,
                &context.database.schema,
                &work.candidate.generation_id,
            )
            .await
            .map_err(|_| database_error("drop-generation-search-relation"))?;
            progress.relations += 1;
            progress.bytes += work.search_relation_bytes;
        }
        progress.drain_generation(connection, context, work).await?;
    }
    Ok(progress)
}

async fn claim_generation(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    work: &CandidateWork,
) -> Result<(), GenerationRetentionError> {
    query(AssertSqlSafe(format!(r#"UPDATE {}."index_generations"
        SET retention_original_state = state, retention_started_at = clock_timestamp(), state = 'retiring'
        WHERE project_id = $1::uuid AND generation_id = $2::uuid
          AND state IN ('staging', 'ready', 'failed', 'superseded')"#, context.quoted_schema)))
        .bind(context.fence.target().project_id().as_str()).bind(work.candidate.generation_id.as_str())
        .execute(connection).await.map_err(|_| database_error("claim-retirement"))?;
    Ok(())
}

impl Progress {
    async fn drain_generation(
        &mut self,
        connection: &mut sqlx_postgres::PgConnection,
        context: &RetentionContext<'_>,
        work: &CandidateWork,
    ) -> Result<(), GenerationRetentionError> {
        for table in DELETE_ORDER {
            let remaining = context.policy.maximum_cascade_rows - self.rows;
            if remaining == 0 {
                break;
            }
            // ctid is consumed within this statement only. TidScan avoids a join plan
            // that scans the entire large target heap to delete a small admitted set.
            let returning = if table == "index_generations" {
                "retention_original_state"
            } else {
                "NULL::text"
            };
            let sql = format!(
                r#"WITH deleted AS (
            DELETE FROM {schema}."{table}"
            WHERE project_id = $1::uuid AND generation_id = $2::uuid
              AND ctid = ANY(ARRAY(SELECT ctid FROM {schema}."{table}"
                  WHERE project_id = $1::uuid AND generation_id = $2::uuid LIMIT $3))
            RETURNING {returning} AS original_state
        ) SELECT count(*)::bigint AS removed,
              min(original_state) AS original_state FROM deleted"#,
                schema = context.quoted_schema
            );
            let row = query(AssertSqlSafe(sql))
                .bind(context.fence.target().project_id().as_str())
                .bind(work.candidate.generation_id.as_str())
                .bind(
                    i64::try_from(remaining)
                        .map_err(|_| GenerationRetentionError::InvalidPolicy)?,
                )
                .fetch_one(&mut *connection)
                .await
                .map_err(|_| database_error("drain-generation-rows"))?;
            let removed = super::read_named_count(&row, "removed")?;
            self.rows += removed;
            if table == "document_embeddings" {
                self.removed.embeddings += removed;
            }
            if table == "index_generations" && removed > 0 {
                let state = row
                    .try_get::<String, _>("original_state")
                    .map_err(|_| database_error("decode-retirement-state"))?;
                match state.as_str() {
                    "staging" => self.removed.staging += removed,
                    "ready" => self.removed.ready += removed,
                    "superseded" => self.removed.superseded += removed,
                    "failed" => self.removed.failed += removed,
                    _ => return Err(database_error("invalid-retirement-state")),
                }
            }
        }
        Ok(())
    }
}

pub(super) async fn verify_delete_order(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
) -> Result<(), GenerationRetentionError> {
    let valid = query(
        r"WITH ordered AS (
            SELECT c.oid, t.ordinal FROM unnest($2::text[]) WITH ORDINALITY AS t(name, ordinal)
            JOIN pg_class c ON c.relname = t.name
            JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = $1
        ) SELECT NOT EXISTS (
            SELECT 1 FROM pg_constraint fk
            JOIN ordered child ON child.oid = fk.conrelid
            JOIN ordered parent ON parent.oid = fk.confrelid
            WHERE fk.contype = 'f' AND (
                child.ordinal >= parent.ordinal
                OR NOT EXISTS (
                    SELECT 1 FROM unnest(fk.conkey, fk.confkey) AS keys(child_key, parent_key)
                    JOIN pg_attribute ca ON ca.attrelid = child.oid AND ca.attnum = keys.child_key
                    JOIN pg_attribute pa ON pa.attrelid = parent.oid AND pa.attnum = keys.parent_key
                    WHERE ca.attname = 'project_id' AND pa.attname = 'project_id'
                ) OR NOT EXISTS (
                    SELECT 1 FROM unnest(fk.conkey, fk.confkey) AS keys(child_key, parent_key)
                    JOIN pg_attribute ca ON ca.attrelid = child.oid AND ca.attnum = keys.child_key
                    JOIN pg_attribute pa ON pa.attrelid = parent.oid AND pa.attnum = keys.parent_key
                    WHERE ca.attname = 'generation_id' AND pa.attname = 'generation_id'
                )
            )
        )",
    )
    .bind(context.database.schema.as_str())
    .bind(DELETE_ORDER.to_vec())
    .fetch_one(connection)
    .await
    .map_err(|_| database_error("verify-delete-order"))?
    .try_get::<bool, _>(0)
    .map_err(|_| database_error("decode-delete-order"))?;
    if valid {
        Ok(())
    } else {
        Err(database_error("cascade-catalog-mismatch"))
    }
}
