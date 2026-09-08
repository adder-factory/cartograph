use std::time::Duration;

use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId, ProjectId};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use sqlx_postgres::PgConnection;
use thiserror::Error;

use crate::{
    CartographDatabase, StorageError,
    database::{read_stored_string, stored_value_error},
};

const MAX_ROOT_IDENTITY_BYTES: usize = 4_096;
const MAX_RUN_EXCLUDE_PATTERNS: u16 = 4_096;
const MAX_RUN_EXCLUDE_BYTES: usize = 4_096;
const MAX_RUN_EXCLUDES_TOTAL_BYTES: usize = 4 * 1_024 * 1_024;
const PROJECT_ID_COLUMN: usize = 0;
const GENERATION_ID_COLUMN: usize = 1;
const GENERATION_SEQUENCE_COLUMN: usize = 2;
const SOURCE_REVISION_COLUMN: usize = 3;
const CONTENT_DIGEST_COLUMN: usize = 4;
const DIGEST_VERSION_COLUMN: usize = 5;
const FILE_COUNT_COLUMN: usize = 6;
const SYMBOL_COUNT_COLUMN: usize = 7;
const EDGE_COUNT_COLUMN: usize = 8;
const REFERENCE_COUNT_COLUMN: usize = 9;
const NUMERICAL_SITE_COUNT_COLUMN: usize = 10;
const DOCUMENT_COUNT_COLUMN: usize = 11;
const SOURCE_ADMISSION_COLUMN: usize = 12;
const PROJECT_PURGE_LOCK_NAMESPACE: &str = "cartograph-v2-project-purge";
const PUBLICATION_LOCK_NAMESPACE: &str = "cartograph-v2-publish";
const DIRECT_PROJECT_TABLES: &[&str] = &[
    "index_generations",
    "files",
    "symbols",
    "edges",
    "references",
    "numerical_sites",
    "search_documents",
    "project_operation_leases",
    "v1_import_runs",
    "document_embeddings",
    "generation_search_relations",
    "coverage_sources",
    "symbol_coverage",
    "file_history",
    "file_cochanges",
    "agent_artifacts",
    "mcp_sessions",
    "mcp_tool_calls",
    "mcp_macros",
    "symbol_similarity_edges",
    "symbol_similarity_builds",
    "structural_findings",
    "structural_finding_runs",
];

/// Exact bounded deletion report for one PostgreSQL-backed project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPurgeReport {
    /// Number of generations removed.
    pub generations_removed: u64,
    /// Number of search relations removed.
    pub search_relations_removed: u64,
    /// Number of cascade rows removed.
    pub cascade_rows_removed: u64,
}

/// Hard bounds and exact project identity for one destructive purge transaction.
#[derive(Clone, Copy, Debug)]
pub struct ProjectPurgeRequest<'a> {
    /// Stable project ID for this record.
    pub project_id: &'a ProjectId,
    /// Maximum number of generations permitted by this request.
    pub maximum_generations: u16,
    /// Maximum number of cascade rows permitted by this request.
    pub maximum_cascade_rows: u64,
    /// Statement timeout for this record.
    pub statement_timeout: Duration,
}

/// Safe, actionable reasons a project purge did not run.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectPurgeError {
    #[error("invalid Cartograph project purge bounds")]
    /// Supplied bounds are zero, inconsistent, or exceed the hard ceiling.
    InvalidBounds,
    #[error("Cartograph project does not exist")]
    /// No durable project matches the supplied identity.
    ProjectNotFound,
    #[error("Cartograph project has {count} unexpired operation leases")]
    /// Unexpired project leases still protect the requested data.
    LiveLeases {
        /// Number of unexpired leases that still protect the project.
        count: u64,
    },
    #[error("Cartograph project purge exceeds its generation bound")]
    /// Generation-scoped facts exceed a declared validation ceiling.
    GenerationBoundExceeded,
    #[error("Cartograph project purge exceeds its row bound")]
    /// The result exceeded its declared row ceiling.
    RowBoundExceeded,
    #[error("Cartograph project purge found corrupt stored {field}")]
    /// A stored row violates its durable typed contract.
    CorruptStoredValue {
        /// Stored field whose value violated the purge contract.
        field: &'static str,
    },
    #[error("Cartograph PostgreSQL project purge failed during {operation}")]
    /// PostgreSQL could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed PostgreSQL phase.
        operation: &'static str,
    },
}

/// Exact row counts for the atomically published generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GenerationCounts {
    /// Source files in the generation.
    pub files: i64,
    /// Extracted symbols in the generation.
    pub symbols: i64,
    /// Resolved graph edges in the generation.
    pub edges: i64,
    /// Resolved and unresolved source references in the generation.
    pub references: i64,
    /// Privacy-safe static numerical evidence sites in the generation.
    pub numerical_sites: i64,
    /// File and symbol documents available to `ParadeDB`.
    pub documents: i64,
}

/// Project-wide generation retention counts and a source/BM25 lower bound.
/// Shared fact-table heaps, B-trees, embeddings, and reusable dead space are
/// intentionally reported by the fuller storage-usage surface instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GenerationStorageSummary {
    /// Generations staged but not yet prepared.
    pub staging: u64,
    /// Prepared generations awaiting publication or recovery.
    pub ready: u64,
    /// Visible generations; valid project state is zero or one.
    pub current: u64,
    /// Historical generations replaced by a newer publication.
    pub superseded: u64,
    /// Terminal failed generations awaiting bounded cleanup.
    pub failed: u64,
    /// Retired generations with committed partial cleanup awaiting completion.
    pub retiring: u64,
    /// Age since reservation of the oldest failed/superseded/retiring generation; zero when none.
    pub oldest_terminal_age_seconds: u64,
    /// Sum of source byte sizes represented by all retained generations.
    pub source_bytes: u64,
    /// Exact physical bytes occupied by generation-scoped search tables and indexes.
    pub search_relation_bytes: u64,
    /// Conservative lower-bound estimate: source bytes plus search-relation bytes.
    pub estimated_retained_bytes: u64,
}

impl GenerationStorageSummary {
    /// Total durable generation rows retained for the project.
    #[must_use]
    pub const fn generations(self) -> u64 {
        self.staging + self.ready + self.current + self.superseded + self.failed + self.retiring
    }
}

/// Durable metadata for one project's visible generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectCurrentGeneration {
    /// Immutable generation identity.
    pub generation_id: GenerationId,
    /// Monotonic project-local publication sequence.
    pub sequence: i64,
    /// Complete source-manifest digest captured before indexing.
    pub source_revision: String,
    /// Deterministic logical fact digest.
    pub content_digest: ContentDigest,
    /// Version used to interpret the logical digest.
    pub digest_version: GenerationDigestVersion,
    /// Exact persisted relation counts.
    pub counts: GenerationCounts,
    /// Privacy-preserving description of the generation's source admission policy.
    pub source_admission: GenerationSourceAdmission,
}

/// Run-scoped source admission metadata. Serialization and debug output reveal
/// only the count; the exact globs remain available solely to trusted runtime
/// freshness and reconciliation code.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GenerationSourceAdmission {
    #[serde(skip)]
    run_excludes: Vec<String>,
    run_exclude_patterns: u16,
}

impl std::fmt::Debug for GenerationSourceAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationSourceAdmission")
            .field("run_exclude_patterns", &self.run_exclude_patterns)
            .finish_non_exhaustive()
    }
}

impl GenerationSourceAdmission {
    /// Exact run-scoped excludes for trusted source-policy reconciliation.
    #[must_use]
    pub fn run_excludes(&self) -> &[String] {
        &self.run_excludes
    }

    /// Number of persisted run-scoped exclude patterns.
    #[must_use]
    pub const fn run_exclude_patterns(&self) -> u16 {
        self.run_exclude_patterns
    }
}

/// Read-only project state used by CLI, MCP, and freshness checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectSnapshot {
    /// Stable database identity for the project.
    pub project_id: ProjectId,
    /// The currently visible generation, absent before first publication.
    pub current: Option<ProjectCurrentGeneration>,
    /// Project-wide generation retention and storage pressure evidence.
    pub generation_storage: GenerationStorageSummary,
}

impl CartographDatabase {
    /// Resolve one privacy-preserving root identity without creating project state.
    /// # Errors
    ///
    /// Returns an error if `root_identity` is empty/oversized or project,
    /// generation, count, and storage-summary fields cannot be queried or decoded.
    pub async fn project_snapshot_by_root(
        &self,
        root_identity: &str,
    ) -> Result<Option<ProjectSnapshot>, StorageError> {
        validate_root_identity(root_identity)?;
        let schema = crate::database::quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT
                    projects.project_id::text,
                    generations.generation_id::text,
                    generations.generation_sequence,
                    generations.source_revision,
                    generations.content_digest,
                    generations.content_digest_version,
                    COALESCE((SELECT count(*) FROM {schema}."files" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    COALESCE((SELECT count(*) FROM {schema}."symbols" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    COALESCE((SELECT count(*) FROM {schema}."edges" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    COALESCE((SELECT count(*) FROM {schema}."references" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    COALESCE((SELECT count(*) FROM {schema}."numerical_sites" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    COALESCE((SELECT count(*) FROM {schema}."search_documents" AS rows
                        WHERE rows.project_id = projects.project_id
                          AND rows.generation_id = generations.generation_id), 0)::bigint,
                    generations.run_excludes
                FROM {schema}."projects" AS projects
                LEFT JOIN {schema}."index_generations" AS generations
                  ON generations.project_id = projects.project_id
                 AND generations.generation_id = projects.current_generation_id
                WHERE projects.root_identity = $1"#,
        );
        let row = query(AssertSqlSafe(statement))
            .bind(root_identity)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("project-status"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut snapshot = decode_snapshot(&row)?;
        snapshot.generation_storage = self
            .generation_storage_summary(&snapshot.project_id)
            .await?;
        Ok(Some(snapshot))
    }

    /// Count all retained generation states and estimate their dominant physical bytes.
    /// # Errors
    ///
    /// Returns an error if generation-state counts/physical bytes cannot be
    /// queried, are negative, or overflow the retained-byte estimate.
    pub async fn generation_storage_summary(
        &self,
        project_id: &ProjectId,
    ) -> Result<GenerationStorageSummary, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT
                    count(*) FILTER (WHERE state = 'staging')::bigint AS staging,
                    count(*) FILTER (WHERE state = 'ready')::bigint AS ready,
                    count(*) FILTER (WHERE state = 'current')::bigint AS current,
                    count(*) FILTER (WHERE state = 'superseded')::bigint AS superseded,
                    count(*) FILTER (WHERE state = 'failed')::bigint AS failed,
                    count(*) FILTER (WHERE state = 'retiring')::bigint AS retiring,
                    GREATEST(COALESCE(extract(epoch FROM clock_timestamp() -
                        min(started_at) FILTER (WHERE state IN ('failed', 'superseded', 'retiring')))::bigint, 0), 0) AS oldest_terminal_age_seconds,
                    COALESCE((
                        SELECT sum(files.byte_size)::bigint
                        FROM {schema}."files" AS files
                        WHERE files.project_id = CAST($1 AS uuid)
                    ), 0)::bigint AS source_bytes,
                    COALESCE((
                        SELECT sum(pg_total_relation_size(tables.oid))::bigint
                        FROM {schema}."generation_search_relations" AS relations
                        INNER JOIN pg_catalog.pg_namespace AS namespaces
                          ON namespaces.nspname = $2
                        INNER JOIN pg_catalog.pg_class AS tables
                          ON tables.relnamespace = namespaces.oid
                         AND tables.relname = 'search_g_'
                             || replace(relations.generation_id::text, '-', '')
                        WHERE relations.project_id = CAST($1 AS uuid)
                    ), 0)::bigint AS search_relation_bytes
                FROM {schema}."index_generations"
                WHERE project_id = CAST($1 AS uuid)"#
        );
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(self.schema.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| database_error("generation-storage-summary"))?;
        let source_bytes = read_named_nonnegative(&row, "source_bytes")?;
        let search_relation_bytes = read_named_nonnegative(&row, "search_relation_bytes")?;
        let estimated_retained_bytes = source_bytes
            .checked_add(search_relation_bytes)
            .ok_or_else(|| corrupt("estimated_retained_bytes"))?;
        Ok(GenerationStorageSummary {
            staging: read_named_nonnegative(&row, "staging")?,
            ready: read_named_nonnegative(&row, "ready")?,
            current: read_named_nonnegative(&row, "current")?,
            superseded: read_named_nonnegative(&row, "superseded")?,
            failed: read_named_nonnegative(&row, "failed")?,
            retiring: read_named_nonnegative(&row, "retiring")?,
            oldest_terminal_age_seconds: read_named_nonnegative(
                &row,
                "oldest_terminal_age_seconds",
            )?,
            source_bytes,
            search_relation_bytes,
            estimated_retained_bytes,
        })
    }

    /// Remove one project and every generation-scoped physical BM25 relation.
    ///
    /// The caller supplies hard work bounds. A live operation lease always
    /// blocks deletion; expired leases are removed by the project cascade.
    /// # Errors
    ///
    /// Returns an error if work/deadline bounds are invalid, a live lease or
    /// catalog mismatch blocks deletion, or bounded relation/project cleanup fails.
    pub async fn purge_project(
        &self,
        input: ProjectPurgeRequest<'_>,
    ) -> Result<ProjectPurgeReport, ProjectPurgeError> {
        if input.maximum_generations == 0
            || input.maximum_cascade_rows == 0
            || input.statement_timeout.is_zero()
        {
            return Err(ProjectPurgeError::InvalidBounds);
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| purge_database_error("begin"))?;
        crate::database::set_local_statement_timeout(&mut transaction, input.statement_timeout)
            .await
            .map_err(|()| ProjectPurgeError::InvalidBounds)?;
        let result = purge_project_transaction(&mut transaction, self, input).await;
        match result {
            Ok(report) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| purge_database_error("commit"))?;
                Ok(report)
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| purge_database_error("rollback"))?;
                Err(error)
            }
        }
    }
}

async fn purge_project_transaction(
    connection: &mut PgConnection,
    database: &CartographDatabase,
    input: ProjectPurgeRequest<'_>,
) -> Result<ProjectPurgeReport, ProjectPurgeError> {
    let ProjectPurgeRequest {
        project_id,
        maximum_generations,
        maximum_cascade_rows,
        statement_timeout: _,
    } = input;
    acquire_project_purge_locks(connection, database, project_id).await?;
    let schema = crate::database::quoted_schema(&database.schema);
    require_purgeable_project(connection, &schema, project_id).await?;
    let generations = locked_project_generations(
        connection,
        ProjectGenerationQuery {
            schema: &schema,
            project_id,
            maximum_generations,
        },
    )
    .await?;
    let cascade_rows = count_project_rows(connection, &schema, project_id).await?;
    if cascade_rows > maximum_cascade_rows {
        return Err(ProjectPurgeError::RowBoundExceeded);
    }
    let search_relations_removed =
        drop_project_search_relations(connection, database, &generations).await?;
    delete_project_row(connection, &schema, project_id).await?;
    Ok(ProjectPurgeReport {
        generations_removed: u64::try_from(generations.len())
            .map_err(|_| purge_corrupt("generation_count"))?,
        search_relations_removed,
        cascade_rows_removed: cascade_rows,
    })
}

async fn acquire_project_purge_locks(
    connection: &mut PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<(), ProjectPurgeError> {
    for lock in [
        crate::leases::project_lock_key(&database.schema, project_id),
        format!(
            "{PUBLICATION_LOCK_NAMESPACE}:{}:{project_id}",
            database.schema.as_str()
        ),
        format!(
            "{PROJECT_PURGE_LOCK_NAMESPACE}:{}:{project_id}",
            database.schema.as_str()
        ),
    ] {
        query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock)
            .execute(&mut *connection)
            .await
            .map_err(|_| purge_database_error("lock"))?;
    }
    Ok(())
}

async fn require_purgeable_project(
    connection: &mut PgConnection,
    schema: &str,
    project_id: &ProjectId,
) -> Result<(), ProjectPurgeError> {
    let project_sql = format!(
        r#"SELECT 1 FROM {schema}."projects"
            WHERE project_id = CAST($1 AS uuid) FOR UPDATE"#
    );
    if query(AssertSqlSafe(project_sql))
        .bind(project_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| purge_database_error("lock-project"))?
        .is_none()
    {
        return Err(ProjectPurgeError::ProjectNotFound);
    }
    let leases_sql = format!(
        r#"SELECT count(*)::bigint AS live_leases
            FROM {schema}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND expires_at > clock_timestamp()"#
    );
    let live_leases = query(AssertSqlSafe(leases_sql))
        .bind(project_id.as_str())
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| purge_database_error("live-leases"))
        .and_then(|row| purge_count(&row, "live_leases"))?;
    if live_leases > 0 {
        return Err(ProjectPurgeError::LiveLeases { count: live_leases });
    }
    Ok(())
}

struct ProjectGenerationQuery<'project> {
    schema: &'project str,
    project_id: &'project ProjectId,
    maximum_generations: u16,
}

async fn locked_project_generations(
    connection: &mut PgConnection,
    input: ProjectGenerationQuery<'_>,
) -> Result<Vec<GenerationId>, ProjectPurgeError> {
    let generations_sql = format!(
        r#"SELECT generation_id::text AS generation_id
            FROM {}."index_generations"
            WHERE project_id = CAST($1 AS uuid)
            ORDER BY generation_sequence
            LIMIT $2
            FOR UPDATE"#,
        input.schema
    );
    let rows = query(AssertSqlSafe(generations_sql))
        .bind(input.project_id.as_str())
        .bind(i64::from(input.maximum_generations) + 1)
        .fetch_all(&mut *connection)
        .await
        .map_err(|_| purge_database_error("generations"))?;
    if rows.len() > usize::from(input.maximum_generations) {
        return Err(ProjectPurgeError::GenerationBoundExceeded);
    }
    rows.iter()
        .map(|row| {
            let raw = row
                .try_get::<String, _>("generation_id")
                .map_err(|_| purge_corrupt("generation_id"))?;
            GenerationId::parse(&raw).map_err(|_| purge_corrupt("generation_id"))
        })
        .collect::<Result<Vec<_>, _>>()
}

async fn drop_project_search_relations(
    connection: &mut PgConnection,
    database: &CartographDatabase,
    generations: &[GenerationId],
) -> Result<u64, ProjectPurgeError> {
    let mut search_relations_removed = 0_u64;
    for generation_id in generations {
        if generation_search_relation_exists(connection, database, generation_id).await? {
            search_relations_removed = search_relations_removed
                .checked_add(1)
                .ok_or_else(|| purge_corrupt("search_relation_count"))?;
        }
        crate::search_relation::drop_generation_search_relation(
            connection,
            &database.schema,
            generation_id,
        )
        .await
        .map_err(|_| purge_database_error("drop-search-relation"))?;
    }
    Ok(search_relations_removed)
}

async fn delete_project_row(
    connection: &mut PgConnection,
    schema: &str,
    project_id: &ProjectId,
) -> Result<(), ProjectPurgeError> {
    let delete_sql = format!(
        r#"DELETE FROM {schema}."projects"
            WHERE project_id = CAST($1 AS uuid)"#
    );
    let deleted = query(AssertSqlSafe(delete_sql))
        .bind(project_id.as_str())
        .execute(connection)
        .await
        .map_err(|_| purge_database_error("delete-project"))?;
    if deleted.rows_affected() != 1 {
        return Err(ProjectPurgeError::ProjectNotFound);
    }
    Ok(())
}

async fn generation_search_relation_exists(
    connection: &mut PgConnection,
    database: &CartographDatabase,
    generation_id: &GenerationId,
) -> Result<bool, ProjectPurgeError> {
    let relation = crate::search_relation::GenerationSearchRelation::from_generation(generation_id)
        .map_err(|_| purge_corrupt("generation_id"))?;
    query("SELECT to_regclass($1) IS NOT NULL AS present")
        .bind(relation.qualified_table(&database.schema))
        .fetch_one(connection)
        .await
        .map_err(|_| purge_database_error("search-relation-status"))?
        .try_get::<bool, _>("present")
        .map_err(|_| purge_corrupt("search_relation_status"))
}

async fn count_project_rows(
    connection: &mut PgConnection,
    schema: &str,
    project_id: &ProjectId,
) -> Result<u64, ProjectPurgeError> {
    let mut total = 1_u64;
    for table in DIRECT_PROJECT_TABLES {
        let sql = format!(
            r#"SELECT count(*)::bigint AS rows
                FROM {schema}."{table}"
                WHERE project_id = CAST($1 AS uuid)"#
        );
        let rows = query(AssertSqlSafe(sql))
            .bind(project_id.as_str())
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| purge_database_error("count-project-rows"))
            .and_then(|row| purge_count(&row, "rows"))?;
        total = total
            .checked_add(rows)
            .ok_or_else(|| purge_corrupt("row_count"))?;
    }
    let checkpoints_sql = format!(
        r#"SELECT count(*)::bigint AS rows
            FROM {schema}."v1_import_checkpoints" AS checkpoints
            INNER JOIN {schema}."v1_import_runs" AS runs
              ON runs.import_id = checkpoints.import_id
            WHERE runs.project_id = CAST($1 AS uuid)"#
    );
    let checkpoints = query(AssertSqlSafe(checkpoints_sql))
        .bind(project_id.as_str())
        .fetch_one(connection)
        .await
        .map_err(|_| purge_database_error("count-import-checkpoints"))
        .and_then(|row| purge_count(&row, "rows"))?;
    total
        .checked_add(checkpoints)
        .ok_or_else(|| purge_corrupt("row_count"))
}

fn purge_count(row: &sqlx_postgres::PgRow, field: &'static str) -> Result<u64, ProjectPurgeError> {
    row.try_get::<i64, _>(field)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| purge_corrupt(field))
}

const fn purge_corrupt(field: &'static str) -> ProjectPurgeError {
    ProjectPurgeError::CorruptStoredValue { field }
}

const fn purge_database_error(operation: &'static str) -> ProjectPurgeError {
    ProjectPurgeError::DatabaseOperation { operation }
}

fn validate_root_identity(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > MAX_ROOT_IDENTITY_BYTES || value.contains('\0') {
        return Err(StorageError::InvalidInput {
            field: "root_identity",
        });
    }
    Ok(())
}

fn decode_snapshot(row: &sqlx_postgres::PgRow) -> Result<ProjectSnapshot, StorageError> {
    let project_id = ProjectId::parse(&read_stored_string(row, PROJECT_ID_COLUMN, "project_id")?)
        .map_err(|_| corrupt("project_id"))?;
    let generation_id = row
        .try_get::<Option<String>, _>(GENERATION_ID_COLUMN)
        .map_err(|_| corrupt("generation_id"))?;
    let current = match generation_id {
        None => None,
        Some(raw) => Some(ProjectCurrentGeneration {
            generation_id: GenerationId::parse(&raw).map_err(|_| corrupt("generation_id"))?,
            sequence: read_nonnegative(row, GENERATION_SEQUENCE_COLUMN, "generation_sequence")?,
            source_revision: read_stored_string(row, SOURCE_REVISION_COLUMN, "source_revision")?,
            content_digest: ContentDigest::parse(&read_stored_string(
                row,
                CONTENT_DIGEST_COLUMN,
                "content_digest",
            )?)
            .map_err(|_| corrupt("content_digest"))?,
            digest_version: GenerationDigestVersion::from_database_value(
                row.try_get::<i16, _>(DIGEST_VERSION_COLUMN)
                    .map_err(|_| corrupt("digest_version"))?,
            )
            .map_err(|_| corrupt("digest_version"))?,
            counts: GenerationCounts {
                files: read_nonnegative(row, FILE_COUNT_COLUMN, "files")?,
                symbols: read_nonnegative(row, SYMBOL_COUNT_COLUMN, "symbols")?,
                edges: read_nonnegative(row, EDGE_COUNT_COLUMN, "edges")?,
                references: read_nonnegative(row, REFERENCE_COUNT_COLUMN, "references")?,
                numerical_sites: read_nonnegative(
                    row,
                    NUMERICAL_SITE_COUNT_COLUMN,
                    "numerical_sites",
                )?,
                documents: read_nonnegative(row, DOCUMENT_COUNT_COLUMN, "documents")?,
            },
            source_admission: decode_source_admission(row)?,
        }),
    };
    Ok(ProjectSnapshot {
        project_id,
        current,
        generation_storage: GenerationStorageSummary::default(),
    })
}

fn decode_source_admission(
    row: &sqlx_postgres::PgRow,
) -> Result<GenerationSourceAdmission, StorageError> {
    let run_excludes = row
        .try_get::<Vec<String>, _>(SOURCE_ADMISSION_COLUMN)
        .map_err(|_| corrupt("run_excludes"))?;
    let count = u16::try_from(run_excludes.len()).map_err(|_| corrupt("run_excludes"))?;
    let total_bytes = run_excludes.iter().try_fold(0_usize, |total, pattern| {
        total
            .checked_add(pattern.len())
            .ok_or_else(|| corrupt("run_excludes"))
    })?;
    if count > MAX_RUN_EXCLUDE_PATTERNS
        || total_bytes > MAX_RUN_EXCLUDES_TOTAL_BYTES
        || run_excludes.iter().any(|pattern| {
            pattern.is_empty() || pattern.len() > MAX_RUN_EXCLUDE_BYTES || pattern.contains('\0')
        })
    {
        return Err(corrupt("run_excludes"));
    }
    Ok(GenerationSourceAdmission {
        run_excludes,
        run_exclude_patterns: count,
    })
}

fn read_named_nonnegative(
    row: &sqlx_postgres::PgRow,
    field: &'static str,
) -> Result<u64, StorageError> {
    row.try_get::<i64, _>(field)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| corrupt(field))
}

fn read_nonnegative(
    row: &sqlx_postgres::PgRow,
    index: usize,
    field: &'static str,
) -> Result<i64, StorageError> {
    let value = row.try_get::<i64, _>(index).map_err(|_| corrupt(field))?;
    if value < 0 {
        return Err(corrupt(field));
    }
    Ok(value)
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

const fn corrupt(field: &'static str) -> StorageError {
    stored_value_error(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_identity_rejects_empty_unbounded_and_nul_values() {
        assert_eq!(
            validate_root_identity(""),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
        assert_eq!(
            validate_root_identity(&"x".repeat(MAX_ROOT_IDENTITY_BYTES + 1)),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
        assert_eq!(
            validate_root_identity("project\0escape"),
            Err(StorageError::InvalidInput {
                field: "root_identity"
            })
        );
    }
}
