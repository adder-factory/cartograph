use super::{
    CandidateWork, CartographDatabase, GenerationRetentionError, GenerationRetentionReport,
    GenerationRetentionRequest, PostRetentionMaintenance, PostRetentionMaintenancePlan,
    RemovedGenerationCounts, RetentionContext, cleanup_transaction, database_error,
    post_retention_maintenance_plan, validate_fence_shape,
};
use cartograph_domain::GenerationId;
use sqlx_core::{acquire::Acquire, query::query, row::Row, sql_str::AssertSqlSafe};
use std::{collections::HashMap, future::Future, time::Duration};

const ROWS_PER_TRANSACTION: u64 = 10_000;
const GENERATIONS_PER_TRANSACTION: u32 = 32;
const MAXIMUM_TRANSACTIONS: u64 = 512;
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Deleting the generation row runs every cascading foreign-key check, which
/// walks the dead child index entries the drain just produced until VACUUM
/// removes them. A bounded savepoint defers that single row instead of rolling
/// back the committed child progress with the whole transaction.
const PARENT_DELETE_TIMEOUT: Duration = Duration::from_secs(2);
/// Transaction time kept back for cascade verification, preserved counts, the
/// final fence check, and COMMIT after the parent row is attempted.
const PARENT_DELETE_RESERVE: Duration = Duration::from_millis(1_500);
/// A parent attempt shorter than this cannot finish meaningful cascade checks.
const MINIMUM_PARENT_DELETE_BUDGET: Duration = Duration::from_millis(250);

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

/// Unique B-tree key suffix after `(project_id, generation_id)` for each large
/// generation-scoped relation. A keyset batch resumes strictly after the last
/// key it deleted, so draining one generation walks its index range once.
/// Batches selected by `LIMIT` alone restart at the range's first key and must
/// step over every dead tuple left by earlier committed batches, which makes a
/// multi-million-row generation quadratic and eventually exceeds the
/// per-transaction deadline. Tables without a suffix are single-row or small
/// and use the plain bounded sweep.
fn keyset_columns(table: &str) -> &'static [&'static str] {
    match table {
        "native_generation_spill_rows" => &["relation", "batch_sequence", "row_ordinal"],
        "native_generation_spill_files"
        | "native_generation_spill_symbols"
        | "native_generation_spill_edges"
        | "native_generation_spill_references"
        | "native_generation_spill_numerical_sites"
        | "native_generation_spill_documents" => &["batch_sequence", "row_ordinal"],
        "native_generation_spill_batches" => &["relation", "batch_sequence"],
        "document_embeddings" => &["document_id", "model_id"],
        "search_documents" => &["document_id"],
        "edges" => &[
            "source_symbol_id",
            "target_symbol_id",
            "edge_kind",
            "provenance",
        ],
        "references" => &["reference_name", "file_id", "start_byte", "reference_id"],
        "numerical_sites" => &["numerical_site_id"],
        "symbol_coverage" => &["source_id", "symbol_id"],
        "symbol_similarity_edges" => &["model_id", "source_symbol_id", "target_symbol_id"],
        "symbol_issues" => &[
            "symbol_id",
            "issue_number",
            "commit_sha",
            "attribution_kind",
        ],
        "summary_priority_queue" | "symbols" => &["symbol_id"],
        "structural_findings" => &["symbol_id", "finding"],
        "files" => &["file_id"],
        _ => &[],
    }
}

/// Per-relation drain progress for one generation.
///
/// An exhausted keyset walk is complete: every keyset column is `NOT NULL`,
/// the key is unique within a generation, and terminal generations admit no
/// writers. It is deliberately not followed by an unordered sweep, because that
/// sweep would re-walk every dead tuple the walk just produced and reintroduce
/// the deadline failure. The final `index_generations` delete still cascades
/// through the verified foreign-key catalog if an unforeseen row remained.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TableDrain {
    /// Resume after this JSON-encoded key; `None` starts at the first key.
    Keyset(Option<String>),
    /// Small relation without a keyset; delete unordered bounded batches.
    Sweep,
    /// Every row of this generation in this relation was deleted.
    Done,
    /// The generation row's cascade checks exceeded their bound in this
    /// invocation; a later invocation retries after dead-tuple maintenance.
    ParentDeferred,
}

/// Drain progress retained across the committed transactions of one cleanup
/// invocation. Rolled-back transactions never publish their updates.
#[derive(Clone, Debug, Default)]
pub(super) struct DrainCursors {
    tables: HashMap<(GenerationId, &'static str), TableDrain>,
}

impl DrainCursors {
    fn state(&self, generation_id: &GenerationId, table: &'static str) -> TableDrain {
        self.tables
            .get(&(generation_id.clone(), table))
            .cloned()
            .unwrap_or_else(|| {
                if keyset_columns(table).is_empty() {
                    TableDrain::Sweep
                } else {
                    TableDrain::Keyset(None)
                }
            })
    }

    fn set(&mut self, generation_id: &GenerationId, table: &'static str, state: TableDrain) {
        self.tables.insert((generation_id.clone(), table), state);
    }

    fn parent_deferred(&self) -> bool {
        self.tables
            .values()
            .any(|state| *state == TableDrain::ParentDeferred)
    }
}

/// Statement deadline for one parent-row attempt, clamped so the transaction
/// keeps time to verify and commit its child progress. `None` defers the row.
fn parent_delete_budget(remaining: Option<Duration>) -> Option<Duration> {
    let budget = remaining.map_or(PARENT_DELETE_TIMEOUT, |remaining| {
        remaining
            .saturating_sub(PARENT_DELETE_RESERVE)
            .min(PARENT_DELETE_TIMEOUT)
    });
    (budget >= MINIMUM_PARENT_DELETE_BUDGET).then_some(budget)
}

fn remaining_until(deadline: Option<tokio::time::Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
}

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
    let mut cursors = DrainCursors::default();
    let mut observer = Some(observe);
    for _ in 0..MAXIMUM_TRANSACTIONS {
        let remaining = request.statement_timeout.saturating_sub(started.elapsed());
        let Some(policy) = remaining_policy(request, total, remaining) else {
            break;
        };
        let batch_deadline = deadline.min(tokio::time::Instant::now() + TRANSACTION_TIMEOUT);
        let context = RetentionContext {
            database,
            policy,
            fence: request.fence,
            quoted_schema: crate::database::quoted_schema(&database.schema),
            batch_deadline: Some(batch_deadline),
        };
        let result = one_transaction(&context, batch_deadline, &mut cursors, || async {
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
    if report.deferred_reason.is_none() && cursors.parent_deferred() {
        report.deferred_reason = Some("parent_delete_deferred");
    }
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
    cursors: &mut DrainCursors,
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
    let mut working = cursors.clone();
    let result = tokio::time::timeout_at(deadline, async {
        let mut transaction = connection
            .begin()
            .await
            .map_err(|_| database_error("begin"))?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        crate::database::set_local_statement_timeout(&mut transaction, remaining)
            .await
            .map_err(|()| GenerationRetentionError::InvalidPolicy)?;
        match cleanup_transaction(&mut transaction, context, &mut working, observe).await {
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
        if result.is_ok() {
            *cursors = working;
        }
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
    cursors: &mut DrainCursors,
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
        progress
            .drain_generation(connection, context, work, cursors)
            .await?;
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
        cursors: &mut DrainCursors,
    ) -> Result<(), GenerationRetentionError> {
        let generation_id = &work.candidate.generation_id;
        for table in DELETE_ORDER {
            let mut state = cursors.state(generation_id, table);
            if let TableDrain::Keyset(cursor) = &state {
                let remaining = context.policy.maximum_cascade_rows - self.rows;
                if remaining == 0 {
                    break;
                }
                let batch = keyset_batch(
                    connection,
                    context,
                    work,
                    table,
                    cursor.as_deref(),
                    remaining,
                )
                .await?;
                self.record(table, batch.removed, None)?;
                state = if batch.removed < remaining {
                    TableDrain::Done
                } else {
                    TableDrain::Keyset(batch.next_cursor)
                };
                cursors.set(generation_id, table, state.clone());
            }
            if state == TableDrain::Sweep {
                let remaining = context.policy.maximum_cascade_rows - self.rows;
                if remaining == 0 {
                    break;
                }
                let Some((removed, original_state)) = (if table == "index_generations" {
                    delete_parent_bounded(connection, context, work, remaining).await?
                } else {
                    Some(sweep_batch(connection, context, work, table, remaining).await?)
                }) else {
                    cursors.set(generation_id, table, TableDrain::ParentDeferred);
                    continue;
                };
                self.record(table, removed, original_state.as_deref())?;
                if removed < remaining {
                    cursors.set(generation_id, table, TableDrain::Done);
                }
            }
        }
        Ok(())
    }

    fn record(
        &mut self,
        table: &str,
        removed: u64,
        original_state: Option<&str>,
    ) -> Result<(), GenerationRetentionError> {
        self.rows += removed;
        if table == "document_embeddings" {
            self.removed.embeddings += removed;
        }
        if table == "index_generations" && removed > 0 {
            match original_state {
                Some("staging") => self.removed.staging += removed,
                Some("ready") => self.removed.ready += removed,
                Some("superseded") => self.removed.superseded += removed,
                Some("failed") => self.removed.failed += removed,
                _ => return Err(database_error("invalid-retirement-state")),
            }
        }
        Ok(())
    }
}

struct KeysetBatch {
    removed: u64,
    next_cursor: Option<String>,
}

/// Delete the next ordered batch of one generation's rows strictly after the
/// previous batch's last key. Cursor columns are decoded through the relation's
/// own row type, so every comparison keeps the exact column type and collation
/// used by the backing B-tree.
async fn keyset_batch(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    work: &CandidateWork,
    table: &'static str,
    cursor: Option<&str>,
    limit: u64,
) -> Result<KeysetBatch, GenerationRetentionError> {
    let columns = keyset_columns(table);
    let schema = &context.quoted_schema;
    let quoted: Vec<String> = columns
        .iter()
        .map(|column| format!(r#""{column}""#))
        .collect();
    let ordered = quoted.join(", ");
    let descending = quoted
        .iter()
        .map(|column| format!("{column} DESC"))
        .collect::<Vec<_>>()
        .join(", ");
    let encoded = columns
        .iter()
        .zip(&quoted)
        .map(|(name, column)| format!("'{name}', {column}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Each cursor value is an uncorrelated scalar subquery, which PostgreSQL
    // evaluates once as an init-plan parameter. The row comparison then stays
    // an index qualification and the ordered scan starts after the cursor.
    let resume = if cursor.is_some() {
        let bounds = quoted
            .iter()
            .map(|column| {
                format!(
                    r#"(SELECT (jsonb_populate_record(NULL::{schema}."{table}", $4::jsonb)).{column})"#
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("AND ({ordered}) > ({bounds})")
    } else {
        "AND $4::jsonb IS NULL".to_owned()
    };
    let sql = format!(
        r#"WITH batch AS MATERIALIZED (
            SELECT ctid AS drain_ctid, {ordered} FROM {schema}."{table}"
            WHERE project_id = $1::uuid AND generation_id = $2::uuid {resume}
            ORDER BY {ordered} LIMIT $3
        ), deleted AS (
            DELETE FROM {schema}."{table}"
            WHERE project_id = $1::uuid AND generation_id = $2::uuid
              AND ctid = ANY(ARRAY(SELECT drain_ctid FROM batch))
            RETURNING 1
        ) SELECT (SELECT count(*) FROM deleted)::bigint AS removed,
              (SELECT jsonb_build_object({encoded})::text FROM batch
                  ORDER BY {descending} LIMIT 1) AS next_cursor"#
    );
    let row = query(AssertSqlSafe(sql))
        .bind(context.fence.target().project_id().as_str())
        .bind(work.candidate.generation_id.as_str())
        .bind(i64::try_from(limit).map_err(|_| GenerationRetentionError::InvalidPolicy)?)
        .bind(cursor)
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("drain-generation-rows"))?;
    Ok(KeysetBatch {
        removed: super::read_named_count(&row, "removed")?,
        next_cursor: row
            .try_get::<Option<String>, _>("next_cursor")
            .map_err(|_| database_error("decode-drain-cursor"))?,
    })
}

/// Delete an unordered bounded batch from a small relation without a keyset.
async fn sweep_batch(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    work: &CandidateWork,
    table: &'static str,
    limit: u64,
) -> Result<(u64, Option<String>), GenerationRetentionError> {
    sweep_statement(connection, context, work, table, limit)
        .await?
        .map_err(|_| database_error("drain-generation-rows"))
}

/// Execute one unordered bounded delete, returning the driver error separately
/// so the parent-row path can distinguish its own deadline from real failures.
async fn sweep_statement(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    work: &CandidateWork,
    table: &'static str,
    limit: u64,
) -> Result<Result<(u64, Option<String>), sqlx_core::Error>, GenerationRetentionError> {
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
    let row = match query(AssertSqlSafe(sql))
        .bind(context.fence.target().project_id().as_str())
        .bind(work.candidate.generation_id.as_str())
        .bind(i64::try_from(limit).map_err(|_| GenerationRetentionError::InvalidPolicy)?)
        .fetch_one(&mut *connection)
        .await
    {
        Ok(row) => row,
        Err(error) => return Ok(Err(error)),
    };
    let removed = super::read_named_count(&row, "removed")?;
    let original_state = if table == "index_generations" && removed > 0 {
        Some(
            row.try_get::<String, _>("original_state")
                .map_err(|_| database_error("decode-retirement-state"))?,
        )
    } else {
        None
    };
    Ok(Ok((removed, original_state)))
}

/// Delete the generation row inside a savepoint with its own short deadline,
/// clamped to the enclosing transaction's remaining time. `None` means the row
/// was deferred: either too little time remained to attempt it, or only the
/// cascade checks' statement deadline expired and the savepoint rolled back so
/// child rows deleted earlier in this transaction still commit. Every other
/// failure aborts the transaction as before.
async fn delete_parent_bounded(
    connection: &mut sqlx_postgres::PgConnection,
    context: &RetentionContext<'_>,
    work: &CandidateWork,
    limit: u64,
) -> Result<Option<(u64, Option<String>)>, GenerationRetentionError> {
    const QUERY_CANCELED: &str = "57014";
    let Some(budget) = parent_delete_budget(remaining_until(context.batch_deadline)) else {
        return Ok(None);
    };
    query("SAVEPOINT cartograph_retention_parent")
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("parent-savepoint"))?;
    crate::database::set_local_statement_timeout(connection, budget)
        .await
        .map_err(|()| GenerationRetentionError::InvalidPolicy)?;
    match sweep_statement(connection, context, work, "index_generations", limit).await? {
        Ok(result) => {
            query("RELEASE SAVEPOINT cartograph_retention_parent")
                .execute(&mut *connection)
                .await
                .map_err(|_| database_error("parent-savepoint"))?;
            // Released savepoints keep their setting; restore the time that is
            // actually left rather than the transaction's original budget.
            let remaining = remaining_until(context.batch_deadline)
                .unwrap_or(PARENT_DELETE_TIMEOUT)
                .max(Duration::from_millis(1));
            crate::database::set_local_statement_timeout(connection, remaining)
                .await
                .map_err(|()| database_error("restore-parent-deadline"))?;
            Ok(Some(result))
        }
        Err(error)
            if error
                .as_database_error()
                .and_then(sqlx_core::error::DatabaseError::code)
                .is_some_and(|code| code == QUERY_CANCELED) =>
        {
            // Rolling back to the savepoint also restores the transaction deadline.
            query("ROLLBACK TO SAVEPOINT cartograph_retention_parent")
                .execute(&mut *connection)
                .await
                .map_err(|_| database_error("parent-savepoint"))?;
            Ok(None)
        }
        Err(_) => Err(database_error("drain-generation-rows")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_budget_keeps_commit_time_and_defers_when_the_batch_is_nearly_spent() {
        assert_eq!(parent_delete_budget(None), Some(PARENT_DELETE_TIMEOUT));
        assert_eq!(
            parent_delete_budget(Some(Duration::from_secs(10))),
            Some(PARENT_DELETE_TIMEOUT)
        );
        assert_eq!(
            parent_delete_budget(Some(Duration::from_millis(2_500))),
            Some(Duration::from_millis(1_000))
        );
        assert_eq!(
            parent_delete_budget(Some(Duration::from_millis(1_749))),
            None,
            "an attempt that would leave under the commit reserve is deferred"
        );
        assert_eq!(parent_delete_budget(Some(Duration::ZERO)), None);
    }
}
