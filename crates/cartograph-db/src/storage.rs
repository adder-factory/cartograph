use std::time::Duration;

use cartograph_domain::ProjectId;
use serde::Serialize;
use sqlx_core::{error::Error as SqlxError, query::query, row::Row, sql_str::AssertSqlSafe};

use crate::{
    CartographDatabase, GenerationStorageSummary, StorageError,
    database::{quoted_schema, set_local_statement_timeout},
};

const MAXIMUM_STORAGE_ROWS: u16 = 128;
const DEFAULT_PARSE_CACHE_ROWS: u64 = 20_000;
const DEFAULT_PARSE_CACHE_PAYLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_PARSE_CACHE_CONTRACTS: u64 = 2;
const PARSE_CACHE_AMPLIFICATION_MINIMUM_BYTES: u64 = 64 * 1024 * 1024;
const PARSE_CACHE_AMPLIFICATION_ALLOWANCE_BYTES: u64 = 64 * 1024 * 1024;
const PARSE_CACHE_AMPLIFICATION_MULTIPLIER: u64 = 4;
const STALE_READY_AGE: Duration = Duration::from_hours(24);

/// One bounded relation-level storage and autovacuum signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStorageUsage {
    /// Relation for this record.
    pub relation: String,
    /// Number of bytes used by the heap.
    pub heap_bytes: u64,
    /// Number of bytes used by the index.
    pub index_bytes: u64,
    /// Number of bytes used by the toast.
    pub toast_bytes: u64,
    /// Number of bytes used by the total.
    pub total_bytes: u64,
    /// Estimated live rows; absent when tracking or an observation is unavailable.
    pub estimated_live_rows: Option<u64>,
    /// Estimated dead rows; absent when tracking or an observation is unavailable.
    pub estimated_dead_rows: Option<u64>,
    /// Optional last autovacuum, when available.
    pub last_autovacuum: Option<String>,
    /// Number of autovacuum entries.
    pub autovacuum_count: Option<u64>,
    /// Most recent manual VACUUM, if retained by PostgreSQL statistics.
    pub last_vacuum: Option<String>,
    /// Most recent manual ANALYZE, if retained by PostgreSQL statistics.
    pub last_analyze: Option<String>,
    /// Most recent automatic ANALYZE, if retained by PostgreSQL statistics.
    pub last_autoanalyze: Option<String>,
}

/// Observation context for cumulative estimates, distinct from allocated bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatisticsObservation {
    /// Server time when the statistics context was read.
    pub observed_at: String,
    /// Database-wide reset time; individual relation resets can occur separately.
    pub database_stats_reset: Option<String>,
    /// Statistics snapshot time, absent when PostgreSQL uses per-object caching.
    pub snapshot_at: Option<String>,
    /// Whether this server currently collects table row statistics.
    pub track_counts: bool,
}

/// Independently bounded offsets into the table and index inventories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUsageOffsets {
    /// Number of table rows preceding this page.
    pub tables: u32,
    /// Number of index rows preceding this page.
    pub indexes: u32,
}

/// Bounds and independent offsets for one consistent storage inventory page.
#[derive(Clone, Copy, Debug)]
pub struct StorageUsagePage {
    /// Maximum rows returned from each inventory.
    pub limit: u16,
    /// Positions within the table and index inventories.
    pub offsets: StorageUsageOffsets,
    /// Maximum duration of each storage statement.
    pub statement_timeout: Duration,
}

#[derive(Clone, Copy)]
struct RelationPage {
    limit: u16,
    offset: u32,
}

/// One bounded index allocation and catalog-health signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStorageUsage {
    /// Index for this record.
    pub index: String,
    /// Table for this record.
    pub table: String,
    /// Access method for this record.
    pub access_method: String,
    /// Allocated byte size.
    pub bytes: u64,
    /// Whether the catalog entry is valid.
    pub valid: bool,
    /// Whether the catalog entry is ready for queries.
    pub ready: bool,
}

/// Logical and allocated parse-cache pressure for one project and schema.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseCacheStorageUsage {
    /// Number of rows.
    pub rows: u64,
    /// Number of contracts.
    pub contracts: u64,
    /// Number of bytes used by the logical payload.
    pub logical_payload_bytes: u64,
    /// Number of bytes used by the stored payload.
    pub stored_payload_bytes: u64,
    /// Number of bytes used by the schema stored payload.
    pub schema_stored_payload_bytes: u64,
    /// Number of bytes used by the physical relation.
    pub physical_relation_bytes: u64,
    /// Number of bytes used by the physical overhead.
    pub physical_overhead_bytes: u64,
}

/// Exact duplicate-content evidence. Mutation remains disabled until facts can
/// be shared without weakening immutable generation identity or freshness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationDeduplicationAssessment {
    /// Number of duplicate content groups.
    pub duplicate_content_groups: u64,
    /// Number of redundant generations.
    pub redundant_generations: u64,
    /// Number of bytes used by the estimated redundant source.
    pub estimated_redundant_source_bytes: u64,
    /// Whether the current schema can safely deduplicate immutable generation data.
    pub mutation_supported: bool,
    /// Required migration for this record.
    pub required_migration: &'static str,
}

/// Stable storage warnings intended for CLI/MCP automation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageWarning {
    /// Represents the parse cache contract budget exceeded storage warning.
    ParseCacheContractBudgetExceeded,
    /// Represents the parse cache row budget exceeded storage warning.
    ParseCacheRowBudgetExceeded,
    /// Represents the parse cache payload budget exceeded storage warning.
    ParseCachePayloadBudgetExceeded,
    /// Represents the parse cache physical amplification storage warning.
    ParseCachePhysicalAmplification,
    /// Represents the stale ready generation storage warning.
    StaleReadyGeneration,
    /// Represents the invalid concurrent index artifact storage warning.
    InvalidConcurrentIndexArtifact,
    /// Represents the dead tuple pressure storage warning.
    DeadTuplePressure,
    /// Represents the duplicate generation content storage warning.
    DuplicateGenerationContent,
    /// Represents the relation list truncated storage warning.
    RelationListTruncated,
    /// Represents the index list truncated storage warning.
    IndexListTruncated,
    /// Allocated tables lack a usable cumulative row-estimate observation.
    UnobservedTableStatistics,
    /// A zero-byte spill heap still owns substantial index allocation; inspect the bounded compaction plan.
    EmptySpillIndexAllocation,
}

/// Last bounded maintenance attempt, retained in one project row across restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationRetentionSnapshot {
    /// Database time of the last recorded attempt.
    pub attempted_at: String,
    /// Consecutive attempts with an unavailable phase or a failed row batch.
    pub consecutive_failures: u64,
    /// Bounded structured generation/cache outcomes; never query or credential text.
    pub outcome: serde_json::Value,
}

/// Database, schema, relation, cache, generation, and maintenance-pressure
/// evidence. Byte counts are allocated bytes unless explicitly named logical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsageReport {
    /// Observation/reset context; tuple counters remain estimates.
    pub statistics: StorageStatisticsObservation,
    /// Complete table inventory count, including rows outside this page.
    pub table_count: u64,
    /// Complete index inventory count, including rows outside this page.
    pub index_count: u64,
    /// Number of preceding table rows omitted by the requested page.
    pub table_offset: u32,
    /// Number of preceding index rows omitted by the requested page.
    pub index_offset: u32,
    /// Number of bytes used by the database.
    pub database_bytes: u64,
    /// Allocated forks attributed to non-shared relations across every database schema.
    pub database_catalog_bytes: u64,
    /// Database allocation not attributed to those catalog forks at observation time.
    /// This includes auxiliary/transient files and is not proof of orphaned data.
    pub unattributed_database_bytes: u64,
    /// Number of bytes used by the schema.
    pub schema_bytes: u64,
    /// Number of bytes used by the heap.
    pub heap_bytes: u64,
    /// Number of bytes used by the index.
    pub index_bytes: u64,
    /// Number of bytes used by the btree index.
    pub btree_index_bytes: u64,
    /// Number of bytes used by the toast.
    pub toast_bytes: u64,
    /// Generation storage for this record.
    pub generation_storage: GenerationStorageSummary,
    /// Parse cache for this record.
    pub parse_cache: ParseCacheStorageUsage,
    /// Number of stale ready generations.
    pub stale_ready_generations: u64,
    /// Last independently recorded generation/cache maintenance outcome.
    pub retention_maintenance: Option<GenerationRetentionSnapshot>,
    /// Bounded tables included in this result.
    pub tables: Vec<TableStorageUsage>,
    /// Bounded indexes included in this result.
    pub indexes: Vec<IndexStorageUsage>,
    /// Whether additional table-storage rows were omitted.
    pub tables_truncated: bool,
    /// Whether additional index-storage rows were omitted.
    pub indexes_truncated: bool,
    /// Deduplication for this record.
    pub deduplication: GenerationDeduplicationAssessment,
    /// Bounded warnings included in this result.
    pub warnings: Vec<StorageWarning>,
}

/// Compact allocated database/schema totals suitable for routine status output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageTotalsReport {
    /// Allocated bytes for the current PostgreSQL database, including other schemas.
    pub database_bytes: u64,
    /// Allocated forks attributed to non-shared relations across every database schema.
    pub database_catalog_bytes: u64,
    /// Database allocation not attributed to those catalog forks at observation time.
    /// This includes auxiliary/transient files and is not proof of orphaned data.
    pub unattributed_database_bytes: u64,
    /// Allocated bytes for Cartograph tables, indexes, and TOAST in the configured schema.
    pub schema_bytes: u64,
    /// Heap bytes in the configured schema.
    pub heap_bytes: u64,
    /// All index bytes in the configured schema.
    pub index_bytes: u64,
    /// B-tree index bytes in the configured schema.
    pub btree_index_bytes: u64,
    /// TOAST and auxiliary relation bytes in the configured schema.
    pub toast_bytes: u64,
}

impl CartographDatabase {
    /// Read only compact database/schema totals under a bounded transaction.
    /// # Errors
    ///
    /// Returns an error when the timeout is zero or PostgreSQL cannot return
    /// allocated relation sizes within the caller's deadline.
    pub async fn storage_totals(
        &self,
        statement_timeout: Duration,
    ) -> Result<StorageTotalsReport, StorageError> {
        if statement_timeout.is_zero() {
            return Err(StorageError::InvalidInput {
                field: "storage_totals",
            });
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("storage-totals-begin"))?;
        query("SET TRANSACTION READ ONLY")
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("storage-totals-snapshot"))?;
        set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .map_err(|()| StorageError::InvalidInput {
                field: "storage_totals",
            })?;
        let totals = load_storage_totals(&mut transaction, self).await?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("storage-totals-commit"))?;
        Ok(totals)
    }

    /// Inspect bounded schema storage under one repeatable transaction.
    /// # Errors
    ///
    /// Returns an error if the row limit or timeout is invalid, the project is
    /// unavailable, or repeatable-read storage statistics cannot be queried or decoded.
    pub async fn storage_usage(
        &self,
        project_id: &ProjectId,
        limit: u16,
        statement_timeout: Duration,
    ) -> Result<StorageUsageReport, StorageError> {
        self.storage_usage_page(
            project_id,
            StorageUsagePage {
                limit,
                offsets: StorageUsageOffsets::default(),
                statement_timeout,
            },
        )
        .await
    }

    /// Read a bounded page of allocated storage and cumulative statistics.
    ///
    /// Pages retain largest-first ordering. Concurrent allocation or DDL can
    /// reorder later pages; compare observation times and inventory counts.
    /// # Errors
    ///
    /// Returns an error for invalid limits, offsets above 100,000, unavailable
    /// projects, or database/decoding failures within the statement deadline.
    pub async fn storage_usage_page(
        &self,
        project_id: &ProjectId,
        page: StorageUsagePage,
    ) -> Result<StorageUsageReport, StorageError> {
        let StorageUsagePage {
            offsets,
            statement_timeout,
            ..
        } = page;
        page.validate()?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("storage-usage-begin"))?;
        query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("storage-usage-snapshot"))?;
        set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .map_err(|()| StorageError::InvalidInput {
                field: "storage_usage",
            })?;
        let totals = load_storage_totals(&mut transaction, self).await?;
        let retention_maintenance =
            load_retention_snapshot(&mut transaction, self, project_id).await?;
        let generation_storage =
            load_generation_storage(&mut transaction, self, project_id).await?;
        let parse_cache = load_parse_cache_storage(&mut transaction, self, project_id).await?;
        let stale_ready_generations =
            load_stale_ready_generations(&mut transaction, self, project_id).await?;
        let StorageInventoryPage {
            statistics,
            table_count,
            index_count,
            tables,
            indexes,
            tables_truncated,
            indexes_truncated,
        } = load_storage_inventory(&mut transaction, self, page).await?;
        let deduplication = load_deduplication(&mut transaction, self, project_id).await?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("storage-usage-commit"))?;
        let warnings = storage_warnings(StorageWarningInput {
            parse_cache,
            stale_ready_generations,
            tables: &tables,
            indexes: &indexes,
            tables_truncated,
            indexes_truncated,
            deduplication,
        });
        Ok(StorageUsageReport {
            statistics,
            table_count,
            index_count,
            table_offset: offsets.tables,
            index_offset: offsets.indexes,
            database_bytes: totals.database_bytes,
            database_catalog_bytes: totals.database_catalog_bytes,
            unattributed_database_bytes: totals.unattributed_database_bytes,
            schema_bytes: totals.schema_bytes,
            heap_bytes: totals.heap_bytes,
            index_bytes: totals.index_bytes,
            btree_index_bytes: totals.btree_index_bytes,
            toast_bytes: totals.toast_bytes,
            generation_storage,
            parse_cache,
            stale_ready_generations,
            retention_maintenance,
            tables,
            indexes,
            tables_truncated,
            indexes_truncated,
            deduplication,
            warnings,
        })
    }
}

impl StorageUsagePage {
    fn validate(self) -> Result<(), StorageError> {
        if self.limit == 0
            || self.limit > MAXIMUM_STORAGE_ROWS
            || self.statement_timeout.is_zero()
            || self.offsets.tables > 100_000
            || self.offsets.indexes > 100_000
        {
            return Err(StorageError::InvalidInput {
                field: "storage_usage",
            });
        }
        Ok(())
    }
}

struct StorageInventoryPage {
    statistics: StorageStatisticsObservation,
    table_count: u64,
    index_count: u64,
    tables: Vec<TableStorageUsage>,
    indexes: Vec<IndexStorageUsage>,
    tables_truncated: bool,
    indexes_truncated: bool,
}

async fn load_storage_inventory(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    page: StorageUsagePage,
) -> Result<StorageInventoryPage, StorageError> {
    let (statistics, table_count, index_count) =
        load_statistics_observation(&mut *connection, database).await?;
    let tables = load_table_storage(
        &mut *connection,
        database,
        RelationPage {
            limit: page.limit,
            offset: page.offsets.tables,
        },
    )
    .await?;
    let indexes = load_index_storage(
        connection,
        database,
        RelationPage {
            limit: page.limit,
            offset: page.offsets.indexes,
        },
    )
    .await?;
    let tables_truncated = table_count
        > u64::from(page.offsets.tables)
            + u64::try_from(tables.len()).map_err(|_| corrupt("table_count"))?;
    let indexes_truncated = index_count
        > u64::from(page.offsets.indexes)
            + u64::try_from(indexes.len()).map_err(|_| corrupt("index_count"))?;
    Ok(StorageInventoryPage {
        statistics,
        table_count,
        index_count,
        tables,
        indexes,
        tables_truncated,
        indexes_truncated,
    })
}

async fn load_storage_totals(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
) -> Result<StorageTotalsReport, StorageError> {
    let statement = r"WITH tables AS (
            SELECT classes.oid,
                   pg_relation_size(classes.oid)::bigint AS heap_bytes,
                   pg_indexes_size(classes.oid)::bigint AS index_bytes,
                   pg_total_relation_size(classes.oid)::bigint AS total_bytes
            FROM pg_catalog.pg_class AS classes
            INNER JOIN pg_catalog.pg_namespace AS namespaces
                ON namespaces.oid = classes.relnamespace
            WHERE namespaces.nspname = $1
              AND classes.relkind IN ('r', 'p', 'm')
        ), btree AS (
            SELECT COALESCE(sum(pg_relation_size(indexes.oid)), 0)::bigint AS bytes
            FROM pg_catalog.pg_class AS indexes
            INNER JOIN pg_catalog.pg_namespace AS namespaces
                ON namespaces.oid = indexes.relnamespace
            INNER JOIN pg_catalog.pg_index AS catalog
                ON catalog.indexrelid = indexes.oid
            INNER JOIN pg_catalog.pg_am AS methods
                ON methods.oid = indexes.relam
            WHERE namespaces.nspname = $1
              AND methods.amname = 'btree'
        )
        SELECT pg_database_size(current_database())::bigint AS database_bytes,
               (SELECT COALESCE(sum(
                    COALESCE(pg_relation_size(c.oid, 'main'), 0)
                    + COALESCE(pg_relation_size(c.oid, 'fsm'), 0)
                    + COALESCE(pg_relation_size(c.oid, 'vm'), 0)
                    + COALESCE(pg_relation_size(c.oid, 'init'), 0)
                ), 0)::bigint FROM pg_catalog.pg_class c
                WHERE NOT c.relisshared AND c.relkind IN ('r', 'm', 'i', 'S', 't')) AS database_catalog_bytes,
               COALESCE(sum(tables.total_bytes), 0)::bigint AS schema_bytes,
               COALESCE(sum(tables.heap_bytes), 0)::bigint AS heap_bytes,
               COALESCE(sum(tables.index_bytes), 0)::bigint AS index_bytes,
               COALESCE(sum(GREATEST(
                   tables.total_bytes - tables.heap_bytes - tables.index_bytes, 0
               )), 0)::bigint AS toast_bytes,
               (SELECT bytes FROM btree) AS btree_index_bytes
        FROM tables";
    let row = query(statement)
        .bind(database.schema.as_str())
        .fetch_one(connection)
        .await
        .map_err(|error| storage_query_error(&error, "storage-totals"))?;
    let database_bytes = nonnegative(&row, "database_bytes")?;
    let database_catalog_bytes = nonnegative(&row, "database_catalog_bytes")?;
    Ok(StorageTotalsReport {
        database_bytes,
        database_catalog_bytes,
        unattributed_database_bytes: database_bytes.saturating_sub(database_catalog_bytes),
        schema_bytes: nonnegative(&row, "schema_bytes")?,
        heap_bytes: nonnegative(&row, "heap_bytes")?,
        index_bytes: nonnegative(&row, "index_bytes")?,
        btree_index_bytes: nonnegative(&row, "btree_index_bytes")?,
        toast_bytes: nonnegative(&row, "toast_bytes")?,
    })
}

async fn load_retention_snapshot(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<Option<GenerationRetentionSnapshot>, StorageError> {
    let schema = quoted_schema(&database.schema);
    let row = query(AssertSqlSafe(format!(
        r#"SELECT retention_last_attempt_at::text AS attempted_at,
        retention_consecutive_failures AS failures, retention_last_outcome::text AS outcome
        FROM {schema}."projects" WHERE project_id = $1::uuid
            AND retention_last_attempt_at IS NOT NULL"#
    )))
    .bind(project_id.as_str())
    .fetch_optional(connection)
    .await
    .map_err(|_| database_error("storage-retention-snapshot"))?;
    row.map(|row| {
        let encoded: String = row
            .try_get("outcome")
            .map_err(|_| corrupt("retention_outcome"))?;
        if encoded.len() > 16_384 {
            return Err(corrupt("retention_outcome"));
        }
        Ok(GenerationRetentionSnapshot {
            attempted_at: row
                .try_get("attempted_at")
                .map_err(|_| corrupt("retention_attempted_at"))?,
            consecutive_failures: nonnegative(&row, "failures")?,
            outcome: serde_json::from_str(&encoded).map_err(|_| corrupt("retention_outcome"))?,
        })
    })
    .transpose()
}

async fn load_generation_storage(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<GenerationStorageSummary, StorageError> {
    let schema = quoted_schema(&database.schema);
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
                    WHERE files.project_id = $1::uuid
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
                    WHERE relations.project_id = $1::uuid
                ), 0)::bigint AS search_relation_bytes
            FROM {schema}."index_generations"
            WHERE project_id = $1::uuid"#
    );
    let row = query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .bind(database.schema.as_str())
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("storage-generation-summary"))?;
    let source_bytes = nonnegative(&row, "source_bytes")?;
    let search_relation_bytes = nonnegative(&row, "search_relation_bytes")?;
    Ok(GenerationStorageSummary {
        staging: nonnegative(&row, "staging")?,
        ready: nonnegative(&row, "ready")?,
        current: nonnegative(&row, "current")?,
        superseded: nonnegative(&row, "superseded")?,
        failed: nonnegative(&row, "failed")?,
        retiring: nonnegative(&row, "retiring")?,
        oldest_terminal_age_seconds: nonnegative(&row, "oldest_terminal_age_seconds")?,
        source_bytes,
        search_relation_bytes,
        estimated_retained_bytes: source_bytes
            .checked_add(search_relation_bytes)
            .ok_or_else(|| corrupt("estimated_retained_bytes"))?,
    })
}

async fn load_parse_cache_storage(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<ParseCacheStorageUsage, StorageError> {
    let schema = quoted_schema(&database.schema);
    let statement = format!(
        r#"SELECT count(*) FILTER (WHERE project_id = $1::uuid)::bigint AS rows,
                  count(DISTINCT extractor_contract_digest)
                      FILTER (WHERE project_id = $1::uuid)::bigint AS contracts,
                  COALESCE(sum(payload_bytes)
                      FILTER (WHERE project_id = $1::uuid), 0)::bigint
                      AS logical_payload_bytes,
                  COALESCE(sum(pg_column_size(payload))
                      FILTER (WHERE project_id = $1::uuid), 0)::bigint
                      AS stored_payload_bytes,
                  COALESCE(sum(pg_column_size(payload)), 0)::bigint
                      AS schema_stored_payload_bytes,
                  pg_total_relation_size('{schema}."native_parse_cache"'::regclass)::bigint
                      AS physical_relation_bytes
           FROM {schema}."native_parse_cache""#
    );
    let row = query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("storage-parse-cache"))?;
    let schema_stored_payload_bytes = nonnegative(&row, "schema_stored_payload_bytes")?;
    let physical_relation_bytes = nonnegative(&row, "physical_relation_bytes")?;
    Ok(ParseCacheStorageUsage {
        rows: nonnegative(&row, "rows")?,
        contracts: nonnegative(&row, "contracts")?,
        logical_payload_bytes: nonnegative(&row, "logical_payload_bytes")?,
        stored_payload_bytes: nonnegative(&row, "stored_payload_bytes")?,
        schema_stored_payload_bytes,
        physical_relation_bytes,
        physical_overhead_bytes: physical_relation_bytes
            .saturating_sub(schema_stored_payload_bytes),
    })
}

async fn load_stale_ready_generations(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<u64, StorageError> {
    let age_millis =
        i64::try_from(STALE_READY_AGE.as_millis()).map_err(|_| StorageError::InvalidInput {
            field: "storage_usage",
        })?;
    let schema = quoted_schema(&database.schema);
    let statement = format!(
        r#"SELECT count(*)::bigint AS stale_ready
           FROM {schema}."index_generations" AS generations
           WHERE generations.project_id = $1::uuid
             AND generations.state = 'ready'
             AND COALESCE(generations.ready_at, generations.started_at)
                 <= clock_timestamp() - $2 * interval '1 millisecond'
             AND generations.generation_id IS DISTINCT FROM (
                 SELECT current_generation_id FROM {schema}."projects"
                 WHERE project_id = $1::uuid
             )
             AND NOT EXISTS (
                 SELECT 1 FROM {schema}."project_operation_leases" AS leases
                 WHERE leases.project_id = generations.project_id
                   AND leases.generation_id = generations.generation_id
                   AND leases.expires_at > clock_timestamp()
             )
             AND NOT EXISTS (
                 SELECT 1 FROM {schema}."v1_import_runs" AS imports
                 WHERE imports.project_id = generations.project_id
                   AND imports.generation_id = generations.generation_id
                   AND imports.checkpoint <> 'complete'
             )"#
    );
    let row = query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .bind(age_millis)
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("storage-stale-ready"))?;
    nonnegative(&row, "stale_ready")
}

async fn load_statistics_observation(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
) -> Result<(StorageStatisticsObservation, u64, u64), StorageError> {
    let row = query(r"SELECT clock_timestamp()::text AS observed_at,
            stats.stats_reset::text AS database_stats_reset,
            pg_stat_get_snapshot_timestamp()::text AS snapshot_at,
            current_setting('track_counts') = 'on' AS track_counts,
            (SELECT count(*) FROM pg_catalog.pg_class AS classes
                JOIN pg_catalog.pg_namespace AS namespaces ON namespaces.oid = classes.relnamespace
                WHERE namespaces.nspname = $1 AND classes.relkind IN ('r', 'p', 'm'))::bigint AS table_count,
            (SELECT count(*) FROM pg_catalog.pg_index AS indexes
                JOIN pg_catalog.pg_class AS classes ON classes.oid = indexes.indexrelid
                JOIN pg_catalog.pg_namespace AS namespaces ON namespaces.oid = classes.relnamespace
                WHERE namespaces.nspname = $1)::bigint AS index_count
        FROM pg_catalog.pg_stat_database AS stats WHERE stats.datname = current_database()")
        .bind(database.schema.as_str()).fetch_one(connection).await
        .map_err(|_| database_error("storage-statistics-observation"))?;
    Ok((
        StorageStatisticsObservation {
            observed_at: row
                .try_get("observed_at")
                .map_err(|_| corrupt("observed_at"))?,
            database_stats_reset: optional_statistic_time(&row, "database_stats_reset")?,
            snapshot_at: optional_statistic_time(&row, "snapshot_at")?,
            track_counts: row
                .try_get("track_counts")
                .map_err(|_| corrupt("track_counts"))?,
        },
        nonnegative(&row, "table_count")?,
        nonnegative(&row, "index_count")?,
    ))
}

fn optional_statistic_time(
    row: &sqlx_postgres::PgRow,
    field: &'static str,
) -> Result<Option<String>, StorageError> {
    row.try_get(field).map_err(|_| corrupt(field))
}

fn optional_nonnegative(
    row: &sqlx_postgres::PgRow,
    field: &'static str,
) -> Result<Option<u64>, StorageError> {
    row.try_get::<Option<i64>, _>(field)
        .map_err(|_| corrupt(field))?
        .map(|value| u64::try_from(value).map_err(|_| corrupt(field)))
        .transpose()
}

async fn load_table_storage(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    page: RelationPage,
) -> Result<Vec<TableStorageUsage>, StorageError> {
    let statement = r"SELECT classes.relname,
               pg_relation_size(classes.oid)::bigint AS heap_bytes,
               pg_indexes_size(classes.oid)::bigint AS index_bytes,
               GREATEST(
                   pg_total_relation_size(classes.oid)
                   - pg_relation_size(classes.oid)
                   - pg_indexes_size(classes.oid), 0
               )::bigint AS toast_bytes,
               pg_total_relation_size(classes.oid)::bigint AS total_bytes,
               stats.n_live_tup::bigint AS live_rows,
               stats.n_dead_tup::bigint AS dead_rows,
               stats.last_autovacuum::text,
               stats.last_vacuum::text, stats.last_analyze::text, stats.last_autoanalyze::text,
               CASE WHEN current_setting('track_counts') = 'on' THEN stats.autovacuum_count END::bigint AS autovacuum_count,
               current_setting('track_counts') = 'on' AND COALESCE(
                   stats.n_live_tup > 0 OR stats.n_dead_tup > 0
                   OR stats.last_vacuum IS NOT NULL OR stats.last_autovacuum IS NOT NULL
                   OR stats.last_analyze IS NOT NULL OR stats.last_autoanalyze IS NOT NULL,
                   false
               ) AS estimates_observed
        FROM pg_catalog.pg_class AS classes
        INNER JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = classes.relnamespace
        LEFT JOIN pg_catalog.pg_stat_user_tables AS stats
            ON stats.relid = classes.oid
        WHERE namespaces.nspname = $1
          AND classes.relkind IN ('r', 'p', 'm')
        ORDER BY total_bytes DESC, classes.relname
        LIMIT $2 OFFSET $3";
    let rows = query(statement)
        .bind(database.schema.as_str())
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("storage-tables"))?;
    let tables = rows
        .iter()
        .map(|row| {
            let observed = row
                .try_get::<bool, _>("estimates_observed")
                .map_err(|_| corrupt("estimates_observed"))?;
            Ok(TableStorageUsage {
                relation: stored_name(row, "relname")?,
                heap_bytes: nonnegative(row, "heap_bytes")?,
                index_bytes: nonnegative(row, "index_bytes")?,
                toast_bytes: nonnegative(row, "toast_bytes")?,
                total_bytes: nonnegative(row, "total_bytes")?,
                estimated_live_rows: optional_nonnegative(row, "live_rows")?.filter(|_| observed),
                estimated_dead_rows: optional_nonnegative(row, "dead_rows")?.filter(|_| observed),
                last_autovacuum: row
                    .try_get::<Option<String>, _>("last_autovacuum")
                    .map_err(|_| corrupt("last_autovacuum"))?,
                autovacuum_count: optional_nonnegative(row, "autovacuum_count")?,
                last_vacuum: optional_statistic_time(row, "last_vacuum")?,
                last_analyze: optional_statistic_time(row, "last_analyze")?,
                last_autoanalyze: optional_statistic_time(row, "last_autoanalyze")?,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(tables)
}

async fn load_index_storage(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    page: RelationPage,
) -> Result<Vec<IndexStorageUsage>, StorageError> {
    let statement = r"SELECT indexes.relname AS index_name,
               tables.relname AS table_name,
               methods.amname AS access_method,
               pg_relation_size(indexes.oid)::bigint AS bytes,
               catalog.indisvalid,
               catalog.indisready
        FROM pg_catalog.pg_index AS catalog
        INNER JOIN pg_catalog.pg_class AS indexes
            ON indexes.oid = catalog.indexrelid
        INNER JOIN pg_catalog.pg_class AS tables
            ON tables.oid = catalog.indrelid
        INNER JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = indexes.relnamespace
        INNER JOIN pg_catalog.pg_am AS methods
            ON methods.oid = indexes.relam
        WHERE namespaces.nspname = $1
        ORDER BY bytes DESC, indexes.relname
        LIMIT $2 OFFSET $3";
    let rows = query(statement)
        .bind(database.schema.as_str())
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("storage-indexes"))?;
    let indexes = rows
        .iter()
        .map(|row| {
            Ok(IndexStorageUsage {
                index: stored_name(row, "index_name")?,
                table: stored_name(row, "table_name")?,
                access_method: stored_name(row, "access_method")?,
                bytes: nonnegative(row, "bytes")?,
                valid: row
                    .try_get::<bool, _>("indisvalid")
                    .map_err(|_| corrupt("index_valid"))?,
                ready: row
                    .try_get::<bool, _>("indisready")
                    .map_err(|_| corrupt("index_ready"))?,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(indexes)
}

async fn load_deduplication(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> Result<GenerationDeduplicationAssessment, StorageError> {
    let schema = quoted_schema(&database.schema);
    let statement = format!(
        r#"WITH duplicate_groups AS MATERIALIZED (
                SELECT content_digest_version, content_digest, count(*)::bigint AS generations
                FROM {schema}."index_generations"
                WHERE project_id = $1::uuid
                  AND content_digest IS NOT NULL
                  AND state IN ('ready', 'current', 'superseded')
                GROUP BY content_digest_version, content_digest
                HAVING count(*) > 1
            ), redundant AS MATERIALIZED (
                SELECT generations.content_digest_version, generations.content_digest,
                       generations.generation_id,
                       row_number() OVER (
                           PARTITION BY generations.content_digest_version,
                                        generations.content_digest
                           ORDER BY generations.generation_sequence DESC
                       ) AS content_rank
                FROM {schema}."index_generations" AS generations
                INNER JOIN duplicate_groups
                    USING (content_digest_version, content_digest)
                WHERE generations.project_id = $1::uuid
                  AND generations.state IN ('ready', 'current', 'superseded')
            )
            SELECT (SELECT count(*) FROM duplicate_groups)::bigint AS duplicate_groups,
                   count(DISTINCT redundant.generation_id)
                       FILTER (WHERE redundant.content_rank > 1)::bigint
                       AS redundant_generations,
                   COALESCE(sum(files.byte_size)
                       FILTER (WHERE redundant.content_rank > 1), 0)::bigint
                       AS redundant_source_bytes
            FROM redundant
            LEFT JOIN {schema}."files" AS files
              ON files.project_id = $1::uuid
             AND files.generation_id = redundant.generation_id"#
    );
    let row = query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("storage-deduplication"))?;
    Ok(GenerationDeduplicationAssessment {
        duplicate_content_groups: nonnegative(&row, "duplicate_groups")?,
        redundant_generations: nonnegative(&row, "redundant_generations")?,
        estimated_redundant_source_bytes: nonnegative(&row, "redundant_source_bytes")?,
        mutation_supported: false,
        required_migration: "normalized_content_addressed_generation_facts",
    })
}

#[derive(Clone, Copy)]
struct StorageWarningInput<'a> {
    parse_cache: ParseCacheStorageUsage,
    stale_ready_generations: u64,
    tables: &'a [TableStorageUsage],
    indexes: &'a [IndexStorageUsage],
    tables_truncated: bool,
    indexes_truncated: bool,
    deduplication: GenerationDeduplicationAssessment,
}

impl TableStorageUsage {
    fn has_empty_spill_index_allocation(&self) -> bool {
        self.relation.starts_with("native_generation_spill_")
            && self.heap_bytes == 0
            && self.index_bytes >= PARSE_CACHE_AMPLIFICATION_MINIMUM_BYTES
    }

    fn has_dead_tuple_pressure(&self) -> bool {
        self.estimated_live_rows
            .zip(self.estimated_dead_rows)
            .is_some_and(|(live, dead)| dead >= 10_000 && dead > live.saturating_add(dead) / 10)
    }

    fn has_unobserved_statistics(&self) -> bool {
        self.heap_bytes > 0 && self.estimated_live_rows.is_none()
    }
}

fn storage_warnings(input: StorageWarningInput<'_>) -> Vec<StorageWarning> {
    let mut warnings = Vec::new();
    if input
        .tables
        .iter()
        .any(TableStorageUsage::has_empty_spill_index_allocation)
    {
        warnings.push(StorageWarning::EmptySpillIndexAllocation);
    }
    if input.parse_cache.contracts > DEFAULT_PARSE_CACHE_CONTRACTS {
        warnings.push(StorageWarning::ParseCacheContractBudgetExceeded);
    }
    if input.parse_cache.rows > DEFAULT_PARSE_CACHE_ROWS {
        warnings.push(StorageWarning::ParseCacheRowBudgetExceeded);
    }
    if input.parse_cache.logical_payload_bytes > DEFAULT_PARSE_CACHE_PAYLOAD_BYTES {
        warnings.push(StorageWarning::ParseCachePayloadBudgetExceeded);
    }
    if parse_cache_physically_amplified(input.parse_cache) {
        warnings.push(StorageWarning::ParseCachePhysicalAmplification);
    }
    if input.stale_ready_generations > 0 {
        warnings.push(StorageWarning::StaleReadyGeneration);
    }
    if input
        .indexes
        .iter()
        .any(|index| !index.valid || !index.ready)
    {
        warnings.push(StorageWarning::InvalidConcurrentIndexArtifact);
    }
    if input
        .tables
        .iter()
        .any(TableStorageUsage::has_dead_tuple_pressure)
    {
        warnings.push(StorageWarning::DeadTuplePressure);
    }
    if input
        .tables
        .iter()
        .any(TableStorageUsage::has_unobserved_statistics)
    {
        warnings.push(StorageWarning::UnobservedTableStatistics);
    }
    if input.deduplication.duplicate_content_groups > 0 {
        warnings.push(StorageWarning::DuplicateGenerationContent);
    }
    if input.tables_truncated {
        warnings.push(StorageWarning::RelationListTruncated);
    }
    if input.indexes_truncated {
        warnings.push(StorageWarning::IndexListTruncated);
    }
    warnings
}

fn parse_cache_physically_amplified(cache: ParseCacheStorageUsage) -> bool {
    let healthy_upper_bound = cache
        .schema_stored_payload_bytes
        .saturating_mul(PARSE_CACHE_AMPLIFICATION_MULTIPLIER)
        .saturating_add(PARSE_CACHE_AMPLIFICATION_ALLOWANCE_BYTES);
    cache.physical_relation_bytes >= PARSE_CACHE_AMPLIFICATION_MINIMUM_BYTES
        && cache.physical_relation_bytes > healthy_upper_bound
}

fn nonnegative(row: &sqlx_postgres::PgRow, field: &'static str) -> Result<u64, StorageError> {
    row.try_get::<i64, _>(field)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| corrupt(field))
}

fn stored_name(row: &sqlx_postgres::PgRow, field: &'static str) -> Result<String, StorageError> {
    row.try_get::<String, _>(field)
        .ok()
        .filter(|value| !value.is_empty() && value.len() <= 63 && !value.contains('\0'))
        .ok_or_else(|| corrupt(field))
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

fn storage_query_error(error: &SqlxError, operation: &'static str) -> StorageError {
    if matches!(
        error,
        SqlxError::Database(database) if database.code().as_deref() == Some("57014")
    ) {
        StorageError::StatementTimeout { operation }
    } else {
        database_error(operation)
    }
}

const fn corrupt(field: &'static str) -> StorageError {
    StorageError::CorruptStoredValue { field }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_policy_is_bounded_and_specific() {
        let tables = [TableStorageUsage {
            relation: "references".to_owned(),
            heap_bytes: 1,
            index_bytes: 1,
            toast_bytes: 0,
            total_bytes: 2,
            estimated_live_rows: Some(1),
            estimated_dead_rows: Some(10_000),
            last_autovacuum: None,
            autovacuum_count: Some(0),
            last_vacuum: None,
            last_analyze: None,
            last_autoanalyze: None,
        }];
        let indexes = [IndexStorageUsage {
            index: "references_ccnew".to_owned(),
            table: "references".to_owned(),
            access_method: "btree".to_owned(),
            bytes: 1,
            valid: false,
            ready: false,
        }];
        let warnings = storage_warnings(StorageWarningInput {
            parse_cache: ParseCacheStorageUsage {
                rows: DEFAULT_PARSE_CACHE_ROWS + 1,
                contracts: DEFAULT_PARSE_CACHE_CONTRACTS + 1,
                logical_payload_bytes: DEFAULT_PARSE_CACHE_PAYLOAD_BYTES + 1,
                stored_payload_bytes: 1,
                schema_stored_payload_bytes: 1,
                physical_relation_bytes: 128 * 1024 * 1024,
                physical_overhead_bytes: 128 * 1024 * 1024 - 1,
            },
            stale_ready_generations: 1,
            tables: &tables,
            indexes: &indexes,
            tables_truncated: true,
            indexes_truncated: true,
            deduplication: GenerationDeduplicationAssessment {
                duplicate_content_groups: 1,
                ..GenerationDeduplicationAssessment::default()
            },
        });
        assert_eq!(warnings.len(), 10);
        assert!(warnings.contains(&StorageWarning::ParseCachePhysicalAmplification));
    }

    #[test]
    fn small_parse_cache_relations_do_not_report_physical_amplification() {
        assert!(!parse_cache_physically_amplified(ParseCacheStorageUsage {
            stored_payload_bytes: 1,
            schema_stored_payload_bytes: 1,
            physical_relation_bytes: PARSE_CACHE_AMPLIFICATION_MINIMUM_BYTES - 1,
            physical_overhead_bytes: PARSE_CACHE_AMPLIFICATION_MINIMUM_BYTES - 2,
            ..ParseCacheStorageUsage::default()
        }));
    }

    #[test]
    fn empty_spill_warning_requires_empty_heap_and_substantial_index_allocation() {
        for (relation, heap_bytes, index_bytes, expected) in [
            (
                "native_generation_spill_references",
                0,
                64 * 1024 * 1024,
                true,
            ),
            (
                "native_generation_spill_references",
                8192,
                64 * 1024 * 1024,
                false,
            ),
            (
                "native_generation_spill_references",
                0,
                64 * 1024 * 1024 - 1,
                false,
            ),
            ("references", 0, 64 * 1024 * 1024, false),
        ] {
            let tables = [TableStorageUsage {
                relation: relation.to_owned(),
                heap_bytes,
                index_bytes,
                toast_bytes: 0,
                total_bytes: heap_bytes + index_bytes,
                estimated_live_rows: Some(0),
                estimated_dead_rows: Some(0),
                last_autovacuum: None,
                autovacuum_count: Some(0),
                last_vacuum: None,
                last_analyze: None,
                last_autoanalyze: None,
            }];
            let warnings = storage_warnings(StorageWarningInput {
                parse_cache: ParseCacheStorageUsage::default(),
                stale_ready_generations: 0,
                tables: &tables,
                indexes: &[],
                tables_truncated: false,
                indexes_truncated: false,
                deduplication: GenerationDeduplicationAssessment::default(),
            });
            assert_eq!(
                warnings.contains(&StorageWarning::EmptySpillIndexAllocation),
                expected
            );
        }
    }
}
