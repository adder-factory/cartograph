use std::time::Duration;

use cartograph_config::DatabaseSchema;
use cartograph_domain::{ContentDigest, GenerationId, ProjectId};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use sqlx_postgres::PgConnection;

use crate::{
    CartographDatabase, CurrentGenerationLookup, PrepareGenerationProgress, StorageError,
    generation::ensure_prepare_active,
};

const RELATION_PREFIX: &str = "search_g_";
const BM25_INDEX_SUFFIX: &str = "_bm25";
const RELATION_FORMAT_VERSION: i16 = 1;
const MAXIMUM_PROJECT_SEARCH_RELATIONS: i64 = 256;
const MAXIMUM_STARTUP_REPAIRS: i64 = 64;
const SEARCH_RELATION_LOCK_NAMESPACE: &str = "cartograph-v2-generation-search-relation";
const COMPACT_UUID_LENGTH: usize = 32;
const UUID_GROUP_BOUNDARIES: [usize; 6] = [0, 8, 12, 16, 20, COMPACT_UUID_LENGTH];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GenerationSearchRelation {
    table_name: String,
    index_name: String,
}

impl GenerationSearchRelation {
    pub(crate) fn from_generation(generation_id: &GenerationId) -> Result<Self, StorageError> {
        let compact = generation_id.as_str().replace('-', "");
        if compact.len() != 32 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StorageError::CorruptStoredValue {
                field: "generation_id",
            });
        }
        let table_name = format!("{RELATION_PREFIX}{compact}");
        let index_name = format!("{table_name}{BM25_INDEX_SUFFIX}");
        Ok(Self {
            table_name,
            index_name,
        })
    }

    pub(crate) fn qualified_table(&self, schema: &DatabaseSchema) -> String {
        format!(
            "{}.\"{}\"",
            crate::database::quoted_schema(schema),
            self.table_name
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GenerationSearchBuild<'a> {
    pub(crate) schema: &'a DatabaseSchema,
    pub(crate) project_id: &'a ProjectId,
    pub(crate) generation_id: &'a GenerationId,
    pub(crate) content_digest: &'a ContentDigest,
    /// The enclosing supervised prepare, whose cancellation stops the build
    /// between its statements; `None` outside a supervised prepare.
    pub(crate) progress: Option<&'a PrepareGenerationProgress>,
}

pub(crate) async fn rebuild_generation_search_relation(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
) -> Result<GenerationSearchRelation, StorageError> {
    let relation = GenerationSearchRelation::from_generation(input.generation_id)?;
    // A supervised prepare's cancellation is honored before every statement,
    // so a stop request waits for at most the one statement in flight.
    ensure_prepare_active(input.progress)?;
    acquire_generation_search_relation_lock(connection, input.schema, input.generation_id).await?;
    ensure_prepare_active(input.progress)?;
    enforce_relation_bound(connection, input, &relation).await?;
    ensure_prepare_active(input.progress)?;
    drop_relation(connection, input.schema, &relation).await?;
    ensure_prepare_active(input.progress)?;
    delete_catalog_record(connection, input).await?;
    ensure_prepare_active(input.progress)?;
    create_relation(connection, input, &relation).await?;
    ensure_prepare_active(input.progress)?;
    create_paradedb_index(connection, input.schema, &relation).await?;
    ensure_prepare_active(input.progress)?;
    let document_count = verify_relation(connection, input, &relation).await?;
    ensure_prepare_active(input.progress)?;
    record_verified_relation(connection, input, document_count).await?;
    Ok(relation)
}

pub(crate) async fn require_generation_search_relation(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    generation: CurrentGenerationLookup<'_>,
) -> Result<GenerationSearchRelation, StorageError> {
    let project_id = generation.project_id();
    let generation_id = generation.expected_generation_id();
    let relation = GenerationSearchRelation::from_generation(generation_id)?;
    let sql = format!(
        r#"SELECT relations.document_count,
                  relations.content_digest = generations.content_digest AS digest_matches,
                  relations.relation_format_version,
                  tables.relkind = 'r' AND tables.relpersistence = 'p' AS table_ready,
                  indexes.indisvalid AND indexes.indisready
                      AND methods.amname IN ('paradedb', 'bm25')
                      AND indexes.indrelid = tables.oid AS index_ready
            FROM {}."generation_search_relations" AS relations
            INNER JOIN {}."index_generations" AS generations
              ON generations.project_id = relations.project_id
             AND generations.generation_id = relations.generation_id
            LEFT JOIN pg_catalog.pg_namespace AS namespaces
              ON namespaces.nspname = $3
            LEFT JOIN pg_catalog.pg_class AS tables
              ON tables.relnamespace = namespaces.oid AND tables.relname = $4
            LEFT JOIN pg_catalog.pg_class AS index_relations
              ON index_relations.relnamespace = namespaces.oid
             AND index_relations.relname = $5
            LEFT JOIN pg_catalog.pg_index AS indexes
              ON indexes.indexrelid = index_relations.oid
            LEFT JOIN pg_catalog.pg_am AS methods
              ON methods.oid = index_relations.relam
            WHERE relations.project_id = CAST($1 AS uuid)
              AND relations.generation_id = CAST($2 AS uuid)"#,
        crate::database::quoted_schema(schema),
        crate::database::quoted_schema(schema)
    );
    let row = query(AssertSqlSafe(sql))
        .bind(project_id.as_str())
        .bind(generation_id.as_str())
        .bind(schema.as_str())
        .bind(&relation.table_name)
        .bind(&relation.index_name)
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("search-relation-catalog"))?;
    let Some(row) = row else {
        return Err(StorageError::SearchRelationUnavailable);
    };
    let document_count = row
        .try_get::<i64, _>("document_count")
        .map_err(|_| corrupt("search_relation_document_count"))?;
    let digest_matches = row
        .try_get::<Option<bool>, _>("digest_matches")
        .map_err(|_| corrupt("search_relation_content_digest"))?
        .unwrap_or(false);
    let format_version = row
        .try_get::<i16, _>("relation_format_version")
        .map_err(|_| corrupt("search_relation_format_version"))?;
    let table_ready = row
        .try_get::<Option<bool>, _>("table_ready")
        .map_err(|_| corrupt("search_relation_table"))?
        .unwrap_or(false);
    let index_ready = row
        .try_get::<Option<bool>, _>("index_ready")
        .map_err(|_| corrupt("search_relation_index"))?
        .unwrap_or(false);
    if document_count < 0
        || !digest_matches
        || format_version != RELATION_FORMAT_VERSION
        || !table_ready
        || !index_ready
    {
        return Err(StorageError::SearchRelationUnavailable);
    }
    Ok(relation)
}

pub(crate) async fn drop_generation_search_relation(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    generation_id: &GenerationId,
) -> Result<(), StorageError> {
    let relation = GenerationSearchRelation::from_generation(generation_id)?;
    acquire_generation_search_relation_lock(connection, schema, generation_id).await?;
    drop_relation(connection, schema, &relation).await
}

async fn acquire_generation_search_relation_lock(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    generation_id: &GenerationId,
) -> Result<(), StorageError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{SEARCH_RELATION_LOCK_NAMESPACE}:{}:{}",
            schema.as_str(),
            generation_id.as_str()
        ))
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-lock"))?;
    Ok(())
}

async fn enforce_relation_bound(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
    relation: &GenerationSearchRelation,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"SELECT count(*)::bigint AS relation_count,
                  EXISTS (
                      SELECT 1 FROM {}."generation_search_relations"
                      WHERE project_id = CAST($1 AS uuid)
                        AND generation_id = CAST($2 AS uuid)
                  ) AS replacing
            FROM {}."generation_search_relations"
            WHERE project_id = CAST($1 AS uuid)"#,
        crate::database::quoted_schema(input.schema),
        crate::database::quoted_schema(input.schema)
    );
    let row = query(AssertSqlSafe(sql))
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .fetch_one(connection)
        .await
        .map_err(|_| database_error("search-relation-bound"))?;
    let count = row
        .try_get::<i64, _>("relation_count")
        .map_err(|_| corrupt("search_relation_count"))?;
    let replacing = row
        .try_get::<bool, _>("replacing")
        .map_err(|_| corrupt("search_relation_replacing"))?;
    if count >= MAXIMUM_PROJECT_SEARCH_RELATIONS && !replacing {
        return Err(StorageError::SearchRelationLimitReached);
    }
    if relation.table_name.len() > 63 || relation.index_name.len() > 63 {
        return Err(corrupt("search_relation_name"));
    }
    Ok(())
}

async fn create_relation(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
    relation: &GenerationSearchRelation,
) -> Result<(), StorageError> {
    let source = format!(
        r#"CREATE TABLE {} AS
            SELECT id, project_id, generation_id, document_id, file_id, symbol_id,
                   path, language, document_kind, qualified_name, code,
                   natural_text, metadata
            FROM {}."search_documents"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        relation.qualified_table(input.schema),
        crate::database::quoted_schema(input.schema)
    );
    query(AssertSqlSafe(source))
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-create"))?;
    Ok(())
}

async fn create_paradedb_index(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    relation: &GenerationSearchRelation,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"CREATE INDEX "{}"
            ON {}
            USING paradedb (
                id,
                project_id,
                generation_id,
                document_id,
                file_id,
                symbol_id,
                path,
                language,
                document_kind,
                (qualified_name::pdb.source_code),
                (code::pdb.source_code),
                natural_text,
                metadata
            )
            WITH (key_field = 'id')"#,
        relation.index_name,
        relation.qualified_table(schema)
    );
    query(AssertSqlSafe(sql))
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-index-create"))?;
    Ok(())
}

async fn verify_relation(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
    relation: &GenerationSearchRelation,
) -> Result<i64, StorageError> {
    let counts = format!(
        r#"SELECT
                (SELECT count(*)::bigint
                 FROM {}."search_documents"
                 WHERE project_id = CAST($1 AS uuid)
                   AND generation_id = CAST($2 AS uuid)) AS source_count,
                count(*)::bigint AS relation_count,
                count(DISTINCT id)::bigint AS distinct_ids,
                count(*) FILTER (
                    WHERE project_id <> CAST($1 AS uuid)
                       OR generation_id <> CAST($2 AS uuid)
                )::bigint AS mismatched_rows
            FROM {}"#,
        crate::database::quoted_schema(input.schema),
        relation.qualified_table(input.schema)
    );
    let row = query(AssertSqlSafe(counts))
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("search-relation-count-verify"))?;
    let source_count = read_count(&row, "source_count")?;
    let relation_count = read_count(&row, "relation_count")?;
    let distinct_ids = read_count(&row, "distinct_ids")?;
    let mismatched_rows = read_count(&row, "mismatched_rows")?;
    if source_count != relation_count || relation_count != distinct_ids || mismatched_rows != 0 {
        return Err(StorageError::SearchRelationUnavailable);
    }
    ensure_prepare_active(input.progress)?;
    let catalog = query(
        r"SELECT indexes.indisvalid, indexes.indisready, methods.amname,
                  indexes.indrelid = tables.oid AS correct_table
            FROM pg_catalog.pg_namespace AS namespaces
            INNER JOIN pg_catalog.pg_class AS tables
              ON tables.relnamespace = namespaces.oid AND tables.relname = $2
            INNER JOIN pg_catalog.pg_class AS index_relations
              ON index_relations.relnamespace = namespaces.oid
             AND index_relations.relname = $3
            INNER JOIN pg_catalog.pg_index AS indexes
              ON indexes.indexrelid = index_relations.oid
            INNER JOIN pg_catalog.pg_am AS methods
              ON methods.oid = index_relations.relam
            WHERE namespaces.nspname = $1",
    )
    .bind(input.schema.as_str())
    .bind(&relation.table_name)
    .bind(&relation.index_name)
    .fetch_optional(connection)
    .await
    .map_err(|_| database_error("search-relation-index-verify"))?;
    let healthy = catalog.is_some_and(|row| {
        row.try_get::<bool, _>("indisvalid").unwrap_or(false)
            && row.try_get::<bool, _>("indisready").unwrap_or(false)
            && row
                .try_get::<String, _>("amname")
                .is_ok_and(|method| matches!(method.as_str(), "paradedb" | "bm25"))
            && row.try_get::<bool, _>("correct_table").unwrap_or(false)
    });
    if !healthy {
        return Err(StorageError::SearchRelationUnavailable);
    }
    Ok(relation_count)
}

async fn record_verified_relation(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
    document_count: i64,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"INSERT INTO {}."generation_search_relations" (
                project_id, generation_id, document_count, content_digest,
                relation_format_version, built_at, verified_at
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), $3, $4, $5,
                clock_timestamp(), clock_timestamp()
            )
            ON CONFLICT (project_id, generation_id) DO UPDATE
            SET document_count = EXCLUDED.document_count,
                content_digest = EXCLUDED.content_digest,
                relation_format_version = EXCLUDED.relation_format_version,
                built_at = EXCLUDED.built_at,
                verified_at = EXCLUDED.verified_at"#,
        crate::database::quoted_schema(input.schema)
    );
    query(AssertSqlSafe(sql))
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .bind(document_count)
        .bind(input.content_digest.as_str())
        .bind(RELATION_FORMAT_VERSION)
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-record"))?;
    Ok(())
}

async fn delete_catalog_record(
    connection: &mut PgConnection,
    input: GenerationSearchBuild<'_>,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"DELETE FROM {}."generation_search_relations"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        crate::database::quoted_schema(input.schema)
    );
    query(AssertSqlSafe(sql))
        .bind(input.project_id.as_str())
        .bind(input.generation_id.as_str())
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-record-delete"))?;
    Ok(())
}

async fn drop_relation(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    relation: &GenerationSearchRelation,
) -> Result<(), StorageError> {
    let sql = format!("DROP TABLE IF EXISTS {}", relation.qualified_table(schema));
    query(AssertSqlSafe(sql))
        .execute(connection)
        .await
        .map_err(|_| database_error("search-relation-drop"))?;
    Ok(())
}

fn read_count(row: &sqlx_postgres::PgRow, column: &'static str) -> Result<i64, StorageError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|_| corrupt("search_relation_count"))?;
    if value < 0 {
        Err(corrupt("search_relation_count"))
    } else {
        Ok(value)
    }
}

const fn corrupt(field: &'static str) -> StorageError {
    StorageError::CorruptStoredValue { field }
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

/// Bounded startup reconciliation report for immutable search relations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SearchRelationMaintenanceReport {
    /// Missing or invalid current/ready relations rebuilt from canonical rows.
    pub rebuilt: u64,
    /// Healthy current/ready relations left unchanged.
    pub verified: u64,
    /// Trusted but unowned physical generation relations removed.
    pub orphans_removed: u64,
}

impl CartographDatabase {
    pub(crate) async fn maintain_generation_search_relations(
        &self,
        statement_timeout: Option<Duration>,
    ) -> Result<SearchRelationMaintenanceReport, StorageError> {
        let sql = format!(
            r#"SELECT generations.project_id::text,
                      generations.generation_id::text,
                      generations.content_digest,
                      COALESCE(
                          relations.content_digest = generations.content_digest
                          AND relations.relation_format_version = 1
                          AND tables.relkind = 'r'
                          AND tables.relpersistence = 'p'
                          AND indexes.indisvalid
                          AND indexes.indisready
                          AND indexes.indrelid = tables.oid
                          AND methods.amname IN ('paradedb', 'bm25'),
                          FALSE
                      ) AS healthy
                FROM {}."index_generations" AS generations
                LEFT JOIN {}."generation_search_relations" AS relations
                  ON relations.project_id = generations.project_id
                 AND relations.generation_id = generations.generation_id
                LEFT JOIN pg_catalog.pg_namespace AS namespaces
                  ON namespaces.nspname = $1
                LEFT JOIN pg_catalog.pg_class AS tables
                  ON tables.relnamespace = namespaces.oid
                 AND tables.relname = 'search_g_'
                     || replace(generations.generation_id::text, '-', '')
                LEFT JOIN pg_catalog.pg_class AS index_relations
                  ON index_relations.relnamespace = namespaces.oid
                 AND index_relations.relname = tables.relname || '_bm25'
                LEFT JOIN pg_catalog.pg_index AS indexes
                  ON indexes.indexrelid = index_relations.oid
                LEFT JOIN pg_catalog.pg_am AS methods
                  ON methods.oid = index_relations.relam
                WHERE generations.state IN ('current', 'ready')
                  AND generations.content_digest IS NOT NULL
                ORDER BY healthy ASC,
                         CASE generations.state WHEN 'current' THEN 0 ELSE 1 END,
                         generations.generation_sequence
                LIMIT $2"#,
            crate::database::quoted_schema(&self.schema),
            crate::database::quoted_schema(&self.schema)
        );
        let rows = query(AssertSqlSafe(sql))
            .bind(self.schema.as_str())
            .bind(MAXIMUM_STARTUP_REPAIRS + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| database_error("search-relation-maintenance-scan"))?;
        let mut report = SearchRelationMaintenanceReport::default();
        {
            let mut execution = SearchRepairExecution {
                database: self,
                statement_timeout,
                report: &mut report,
            };
            for row in rows {
                let target = decode_search_repair_target(&row)?;
                execution.maintain(&target).await?;
            }
        }
        report.orphans_removed = self
            .remove_orphan_generation_search_relations(statement_timeout)
            .await?;
        Ok(report)
    }

    async fn remove_orphan_generation_search_relations(
        &self,
        statement_timeout: Option<Duration>,
    ) -> Result<u64, StorageError> {
        let sql = format!(
            r#"SELECT tables.relname
                FROM pg_catalog.pg_namespace AS namespaces
                INNER JOIN pg_catalog.pg_class AS tables
                  ON tables.relnamespace = namespaces.oid
                WHERE namespaces.nspname = $1
                  AND tables.relkind = 'r'
                  AND tables.relname ~ '^search_g_[0-9a-f]{{32}}$'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM {}."generation_search_relations" AS relations
                      WHERE tables.relname = 'search_g_'
                          || replace(relations.generation_id::text, '-', '')
                  )
                ORDER BY tables.relname
                LIMIT $2"#,
            crate::database::quoted_schema(&self.schema)
        );
        let rows = query(AssertSqlSafe(sql))
            .bind(self.schema.as_str())
            .bind(MAXIMUM_STARTUP_REPAIRS)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| database_error("search-relation-orphan-scan"))?;
        let mut removed = 0_u64;
        for row in rows {
            let name = row
                .try_get::<String, _>(0)
                .map_err(|_| corrupt("search_relation_name"))?;
            let generation =
                generation_from_table_name(&name).ok_or_else(|| corrupt("search_relation_name"))?;
            let relation = GenerationSearchRelation::from_generation(&generation)?;
            if relation.table_name != name {
                return Err(corrupt("search_relation_name"));
            }
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| database_error("search-relation-orphan-begin"))?;
            if let Some(timeout) = statement_timeout
                && crate::database::set_local_statement_timeout(&mut transaction, timeout)
                    .await
                    .is_err()
            {
                let _ = transaction.rollback().await;
                return Err(database_error("search-relation-orphan-timeout"));
            }
            drop_relation(&mut transaction, &self.schema, &relation).await?;
            transaction
                .commit()
                .await
                .map_err(|_| database_error("search-relation-orphan-commit"))?;
            removed += 1;
        }
        Ok(removed)
    }
}

struct SearchRepairTarget {
    project: ProjectId,
    generation: GenerationId,
    digest: ContentDigest,
}

struct SearchRepairExecution<'database, 'report> {
    database: &'database CartographDatabase,
    statement_timeout: Option<Duration>,
    report: &'report mut SearchRelationMaintenanceReport,
}

fn decode_search_repair_target(
    row: &sqlx_postgres::PgRow,
) -> Result<SearchRepairTarget, StorageError> {
    Ok(SearchRepairTarget {
        project: ProjectId::parse(
            &row.try_get::<String, _>("project_id")
                .map_err(|_| corrupt("project_id"))?,
        )
        .map_err(|_| corrupt("project_id"))?,
        generation: GenerationId::parse(
            &row.try_get::<String, _>("generation_id")
                .map_err(|_| corrupt("generation_id"))?,
        )
        .map_err(|_| corrupt("generation_id"))?,
        digest: ContentDigest::parse(
            &row.try_get::<String, _>("content_digest")
                .map_err(|_| corrupt("content_digest"))?,
        )
        .map_err(|_| corrupt("content_digest"))?,
    })
}

impl SearchRepairExecution<'_, '_> {
    async fn maintain(&mut self, target: &SearchRepairTarget) -> Result<(), StorageError> {
        let mut transaction = self
            .database
            .pool
            .begin()
            .await
            .map_err(|_| database_error("search-relation-maintenance-begin"))?;
        if let Some(timeout) = self.statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, timeout)
                .await
                .is_err()
        {
            let _ = transaction.rollback().await;
            return Err(database_error("search-relation-maintenance-timeout"));
        }
        match require_generation_search_relation(
            &mut transaction,
            &self.database.schema,
            CurrentGenerationLookup::new(&target.project, &target.generation),
        )
        .await
        {
            Ok(_) => self.report.verified = self.report.verified.saturating_add(1),
            Err(StorageError::SearchRelationUnavailable) => {
                if self.report.rebuilt >= 64 {
                    transaction
                        .rollback()
                        .await
                        .map_err(|_| database_error("search-relation-maintenance-rollback"))?;
                    return Err(database_error("search-relation-maintenance-limit"));
                }
                rebuild_generation_search_relation(
                    &mut transaction,
                    GenerationSearchBuild {
                        schema: &self.database.schema,
                        project_id: &target.project,
                        generation_id: &target.generation,
                        content_digest: &target.digest,
                        progress: None,
                    },
                )
                .await?;
                self.report.rebuilt = self.report.rebuilt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
        transaction
            .commit()
            .await
            .map_err(|_| database_error("search-relation-maintenance-commit"))
    }
}

fn generation_from_table_name(name: &str) -> Option<GenerationId> {
    let compact = name.strip_prefix(RELATION_PREFIX)?;
    if compact.len() != COMPACT_UUID_LENGTH
        || !compact
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let [start, group_one, group_two, group_three, group_four, end] = UUID_GROUP_BOUNDARIES;
    let canonical = format!(
        "{}-{}-{}-{}-{}",
        &compact[start..group_one],
        &compact[group_one..group_two],
        &compact[group_two..group_three],
        &compact[group_three..group_four],
        &compact[group_four..end]
    );
    GenerationId::parse(&canonical).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_identifiers_are_generation_only_bounded_and_sql_safe() {
        let generation = GenerationId::parse("11111111-2222-4333-8444-555555555555")
            .unwrap_or_else(|error| panic!("generation fixture failed: {error}"));
        let relation = GenerationSearchRelation::from_generation(&generation)
            .unwrap_or_else(|error| panic!("relation derivation failed: {error}"));
        assert_eq!(
            relation.table_name,
            "search_g_11111111222243338444555555555555"
        );
        assert_eq!(
            relation.index_name,
            "search_g_11111111222243338444555555555555_bm25"
        );
        assert!(relation.table_name.len() <= 63);
        assert!(relation.index_name.len() <= 63);
        assert!(
            relation
                .table_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        );
        assert_eq!(
            generation_from_table_name(&relation.table_name),
            Some(generation)
        );
    }
}
