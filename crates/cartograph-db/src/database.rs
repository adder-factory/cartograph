use std::time::Duration;

use cartograph_config::DatabaseSchema;
use cartograph_domain::{GenerationId, ProjectId};
use sqlx_core::{error::Error as SqlxError, query::query, row::Row, sql_str::AssertSqlSafe};
use sqlx_postgres::{PgArguments, PgConnection, PgPool, PgRow, Postgres};
use thiserror::Error;

/// PostgreSQL-backed v2 data plane bound to one validated Cartograph schema.
#[derive(Clone)]
pub struct CartographDatabase {
    pub(crate) pool: PgPool,
    pub(crate) schema: DatabaseSchema,
}

impl CartographDatabase {
    /// Bind an established PostgreSQL pool to one validated schema.
    #[must_use]
    pub const fn new(pool: PgPool, schema: DatabaseSchema) -> Self {
        Self { pool, schema }
    }

    /// Configured schema identity.
    #[must_use]
    pub const fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }

    /// Re-probe the live PostgreSQL 18, `ParadeDB`, pgvector, and tokenizer contract.
    /// # Errors
    ///
    /// Returns an error if any required PostgreSQL, `ParadeDB`, pgvector,
    /// preload, BM25, or tokenizer probe fails.
    pub async fn capability_report(&self) -> Result<crate::CapabilityReport, crate::DatabaseError> {
        crate::probe_capabilities(&self.pool).await
    }

    /// Close every pooled PostgreSQL connection owned by this service.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// Expected persistence failures with credential-safe public messages.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StorageError {
    /// A caller supplied an invalid bounded field.
    #[error("invalid {field} in Cartograph storage request")]
    InvalidInput {
        /// Stable field name; input contents are intentionally omitted.
        field: &'static str,
    },
    /// A database operation failed. Driver text is omitted because it may
    /// contain query data or deployment details.
    #[error("Cartograph PostgreSQL operation failed during {operation}")]
    DatabaseOperation {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A bounded statement exceeded its transaction-local PostgreSQL statement timeout.
    #[error("Cartograph PostgreSQL operation timed out during {operation}")]
    StatementTimeout {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A type-state transition disagreed with durable database state.
    #[error("generation cannot transition from {actual} to {requested}")]
    InvalidGenerationTransition {
        /// State currently stored in PostgreSQL.
        actual: String,
        /// State required by this operation.
        requested: &'static str,
    },
    /// A database row contains an identity or enum value this binary cannot parse.
    #[error("Cartograph PostgreSQL data violates the {field} domain contract")]
    CorruptStoredValue {
        /// Stable field name; raw stored contents are intentionally omitted.
        field: &'static str,
    },
    /// A generation token no longer has a corresponding durable row.
    #[error("generation does not exist in the configured Cartograph schema")]
    GenerationNotFound,
    /// A newer generation is already visible and cannot be replaced by an
    /// older ready token.
    #[error(
        "generation sequence {candidate_sequence} is not newer than current sequence {current_sequence}"
    )]
    StaleGeneration {
        /// Sequence carried by the ready token.
        candidate_sequence: i64,
        /// Sequence already published for the project.
        current_sequence: i64,
    },
    /// A generation mutation did not hold the current unexpired exact lease token.
    #[error("Cartograph generation mutation lost its exact PostgreSQL lease fence")]
    LeaseFenceLost,
    /// A read stage's expected publication is no longer the project pointer.
    #[error("Cartograph current generation changed during bounded retrieval")]
    CurrentGenerationChanged,
    /// A generation has no verified immutable BM25 relation.
    #[error("Cartograph generation search relation is unavailable")]
    SearchRelationUnavailable,
    /// A project reached the hard bound for retained generation search relations.
    #[error("Cartograph project search relation limit is reached")]
    SearchRelationLimitReached,
    /// A generation spill exhausted its explicit row or logical-byte quota.
    #[error("Cartograph native generation spill exceeded its {resource} limit")]
    GenerationSpillLimitReached {
        /// Stable quota name; source and database details are intentionally omitted.
        resource: &'static str,
    },
    /// A repeated spill batch disagreed with the already durable deterministic batch.
    #[error("Cartograph native generation spill batch conflicts with durable state")]
    GenerationSpillConflict,
    /// Cooperative cancellation stopped spill reduction or digest traversal.
    #[error("Cartograph native generation spill was cancelled")]
    GenerationSpillCancelled,
}

pub(crate) fn quoted_schema(schema: &DatabaseSchema) -> String {
    // DatabaseSchema accepts ASCII identifier characters only and enforces the
    // PostgreSQL 63-byte limit. Quoting still prevents keyword/case ambiguity.
    format!("\"{}\"", schema.as_str())
}

pub(crate) async fn set_local_statement_timeout(
    connection: &mut PgConnection,
    timeout: Duration,
) -> Result<(), ()> {
    set_local_timeout(connection, "statement_timeout", timeout).await
}

/// Bound every heavyweight-lock wait (table, row, and advisory locks) for the
/// rest of the current transaction; a longer wait fails with SQLSTATE `55P03`.
pub(crate) async fn set_local_lock_timeout(
    connection: &mut PgConnection,
    timeout: Duration,
) -> Result<(), ()> {
    set_local_timeout(connection, "lock_timeout", timeout).await
}

/// Install one transaction-local millisecond timeout. Zero is rejected because
/// PostgreSQL reads it as "no timeout".
async fn set_local_timeout(
    connection: &mut PgConnection,
    setting: &'static str,
    timeout: Duration,
) -> Result<(), ()> {
    let millis = i64::try_from(timeout.as_millis()).map_err(|_| ())?;
    if millis == 0 {
        return Err(());
    }
    query("SELECT set_config($1, $2, true)")
        .bind(setting)
        .bind(format!("{millis}ms"))
        .execute(connection)
        .await
        .map(|_| ())
        .map_err(|_| ())
}

pub(crate) struct ProjectReadRequest<'project> {
    pub(crate) statement: String,
    pub(crate) project_id: &'project ProjectId,
    pub(crate) operation: &'static str,
    pub(crate) statement_timeout: Duration,
}

pub(crate) struct RowReadRequest {
    pub(crate) statement: String,
    pub(crate) operation: &'static str,
    pub(crate) statement_timeout: Duration,
    pub(crate) serial_planning: bool,
}

impl RowReadRequest {
    pub(crate) const fn new(
        statement: String,
        operation: &'static str,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            statement,
            operation,
            statement_timeout,
            serial_planning: false,
        }
    }

    pub(crate) const fn with_serial_planning(mut self) -> Self {
        self.serial_planning = true;
        self
    }
}

pub(crate) type PgQuery<'query> = sqlx_core::query::Query<'query, Postgres, PgArguments>;

pub(crate) fn audited_query(
    sql: String,
) -> sqlx_core::query::Query<'static, Postgres, PgArguments> {
    // Dynamic content is limited to a conservatively validated DatabaseSchema
    // and is always double-quoted before insertion into the audited SQL.
    query(AssertSqlSafe(sql))
}

pub(crate) const fn validate_bounded_limit(limit: u16, maximum: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > maximum {
        Err(StorageError::InvalidInput { field: "limit" })
    } else {
        Ok(())
    }
}

pub(crate) fn read_stored_bool(
    row: &PgRow,
    index: usize,
    field: &'static str,
) -> Result<bool, StorageError> {
    row.try_get::<bool, _>(index)
        .map_err(|_| stored_value_error(field))
}

pub(crate) fn validate_json_object(
    value: &serde_json::Value,
    maximum_bytes: usize,
    field: &'static str,
) -> Result<(), StorageError> {
    let encoded = serde_json::to_vec(value).map_err(|_| StorageError::InvalidInput { field })?;
    if value.is_object() && encoded.len() <= maximum_bytes {
        Ok(())
    } else {
        Err(StorageError::InvalidInput { field })
    }
}

pub(crate) async fn read_project_rows<'query, Bind>(
    database: &CartographDatabase,
    request: ProjectReadRequest<'_>,
    bind: Bind,
) -> Result<Vec<PgRow>, StorageError>
where
    Bind: FnOnce(PgQuery<'query>) -> PgQuery<'query>,
{
    let ProjectReadRequest {
        statement,
        project_id,
        operation,
        statement_timeout,
    } = request;
    read_rows(
        database,
        RowReadRequest::new(statement, operation, statement_timeout),
        |statement| bind(statement.bind(project_id.as_str())),
    )
    .await
}

pub(crate) async fn read_rows<'query, Bind>(
    database: &CartographDatabase,
    request: RowReadRequest,
    bind: Bind,
) -> Result<Vec<PgRow>, StorageError>
where
    Bind: FnOnce(PgQuery<'query>) -> PgQuery<'query>,
{
    let RowReadRequest {
        statement,
        operation,
        statement_timeout,
        serial_planning,
    } = request;
    let database_error = || StorageError::DatabaseOperation { operation };
    let mut transaction = database.pool.begin().await.map_err(|_| database_error())?;
    query("SET TRANSACTION READ ONLY")
        .execute(&mut *transaction)
        .await
        .map_err(|_| database_error())?;
    set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .map_err(|()| database_error())?;
    if serial_planning {
        query("SELECT set_config('max_parallel_workers_per_gather', '0', true)")
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error())?;
    }
    let rows = bind(query(AssertSqlSafe(statement)))
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| classify_statement_error(&error, operation))?;
    transaction.commit().await.map_err(|_| database_error())?;
    Ok(rows)
}

/// Classify a failed bounded statement: a transaction-local statement-timeout
/// cancellation (SQLSTATE 57014) stays distinguishable from every other driver
/// failure so callers can treat lock or I/O contention as retryable.
pub(crate) fn classify_statement_error(error: &SqlxError, operation: &'static str) -> StorageError {
    if matches!(
        error,
        SqlxError::Database(database) if database.code().as_deref() == Some("57014")
    ) {
        StorageError::StatementTimeout { operation }
    } else {
        StorageError::DatabaseOperation { operation }
    }
}

pub(crate) fn read_stored_string(
    row: &PgRow,
    index: usize,
    field: &'static str,
) -> Result<String, StorageError> {
    row.try_get::<String, _>(index)
        .map_err(|_| stored_value_error(field))
}

pub(crate) fn parse_stored_generation_id(
    row: &PgRow,
    index: usize,
) -> Result<GenerationId, StorageError> {
    let raw = read_stored_string(row, index, "generation_id")?;
    GenerationId::parse(&raw).map_err(|_| stored_value_error("generation_id"))
}

pub(crate) const fn stored_value_error(field: &'static str) -> StorageError {
    StorageError::CorruptStoredValue { field }
}
