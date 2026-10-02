use std::time::Duration;

use cartograph_domain::{GenerationId, LeaseId, ProjectId, ProjectOperation};
use serde::Serialize;
use sqlx_core::{query::query, row::Row};
use sqlx_postgres::{PgConnection, PgRow};
use thiserror::Error;

use crate::{CartographDatabase, StorageError, database::audited_query};

const LEASE_LOCK_NAMESPACE: &str = "cartograph-v2-operation";
const SCHEMA_MAINTENANCE_LOCK_NAMESPACE: &str = "cartograph-v2-schema-maintenance";
const MIN_LEASE_DURATION: Duration = Duration::from_secs(1);
const MAX_LEASE_DURATION: Duration = Duration::from_mins(5);
const MAX_PROCESS_START_BYTES: usize = 256;
const STATUS_OWNER_PID_COLUMN: usize = 0;
const STATUS_OWNER_PROCESS_START_COLUMN: usize = 1;
const STATUS_GENERATION_ID_COLUMN: usize = 2;
const STATUS_ACQUIRED_AT_COLUMN: usize = 3;
const STATUS_HEARTBEAT_AT_COLUMN: usize = 4;
const STATUS_EXPIRES_AT_COLUMN: usize = 5;
const STATUS_EXPIRED_COLUMN: usize = 6;
const UUID_TEXT_LENGTH: usize = 36;
const UUID_RANDOM_BYTES: usize = 16;
const UUID_VERSION_BYTE: usize = 6;
const UUID_VARIANT_BYTE: usize = 8;
const UUID_VERSION_CLEAR_MASK: u8 = 0x0f;
const UUID_VERSION_FOUR: u8 = 0x40;
const UUID_VARIANT_CLEAR_MASK: u8 = 0x3f;
const UUID_VARIANT_RFC_4122: u8 = 0x80;
const UUID_BYTE_HYPHEN_OFFSETS: [usize; 4] = [4, 6, 8, 10];
const UPPER_NIBBLE_SHIFT: u8 = 4;
const NIBBLE_MASK: u8 = 0x0f;
const HEX_DIGITS: &[u8] = b"0123456789abcdef";

/// Stable process identity recorded with a project-operation lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseOwner {
    pid: u32,
    process_start: String,
}

impl LeaseOwner {
    /// Build owner metadata. It is validated at the database boundary.
    #[must_use]
    pub fn new(pid: u32, process_start: impl Into<String>) -> Self {
        Self {
            pid,
            process_start: process_start.into(),
        }
    }

    /// Operating-system process identifier.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Boot/session-qualified process start marker supplied by the runtime.
    #[must_use]
    pub fn process_start(&self) -> &str {
        &self.process_start
    }
}

/// One project-local mutating operation protected by a durable lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseTarget {
    project_id: ProjectId,
    operation: ProjectOperation,
    generation_id: Option<GenerationId>,
}

impl LeaseTarget {
    /// Bind an operation to a project and, when relevant, one generation.
    #[must_use]
    pub const fn new(
        project_id: ProjectId,
        operation: ProjectOperation,
        generation_id: Option<GenerationId>,
    ) -> Self {
        Self {
            project_id,
            operation,
            generation_id,
        }
    }

    /// Project whose mutation is serialized.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Stable operation category.
    #[must_use]
    pub const fn operation(&self) -> ProjectOperation {
        self.operation
    }

    /// Optional generation associated with the mutation.
    #[must_use]
    pub const fn generation_id(&self) -> Option<&GenerationId> {
        self.generation_id.as_ref()
    }
}

/// Validated-at-write request to acquire or take over an expired lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRequest {
    target: LeaseTarget,
    owner: LeaseOwner,
    duration: Duration,
}

impl LeaseRequest {
    /// Build a lease request. Bounds are enforced immediately before mutation.
    #[must_use]
    pub const fn new(target: LeaseTarget, owner: LeaseOwner, duration: Duration) -> Self {
        Self {
            target,
            owner,
            duration,
        }
    }
}

/// Single-use, opaque authority to attempt one reconcilable acquisition.
///
/// The exact token is created inside `cartograph-db`, cannot be inspected or
/// replaced by callers, and the value is consumed by acquisition. This keeps a
/// diagnostic lease status from becoming mutation authority.
pub struct LeaseAcquisitionAttempt {
    request: LeaseRequest,
    lease_id: LeaseId,
}

/// Opaque proof used only to reconcile the matching ambiguous acquisition.
///
/// This type deliberately has no token or owner accessors and cannot be turned
/// back into an acquisition attempt.
#[derive(Clone)]
pub struct LeaseAcquisitionProbe {
    request: LeaseRequest,
    lease_id: LeaseId,
}

/// Opaque proof that this process acquired the current database lease token.
#[derive(Debug)]
pub struct ProjectLease {
    target: LeaseTarget,
    lease_id: LeaseId,
    duration: Duration,
    expires_at: String,
}

impl ProjectLease {
    /// Protected project operation.
    #[must_use]
    pub const fn target(&self) -> &LeaseTarget {
        &self.target
    }

    /// Unique ownership token changed on every stale-owner takeover.
    #[must_use]
    pub const fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    /// Database-rendered expiry timestamp from the latest acquire/heartbeat.
    #[must_use]
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    /// Clone the immutable exact-token fence used by generation transactions.
    #[must_use]
    pub fn fence(&self) -> LeaseFence {
        LeaseFence {
            target: self.target.clone(),
            lease_id: self.lease_id.clone(),
        }
    }
}

/// Cloneable exact-token proof checked inside every generation mutation transaction.
///
/// A fence does not prove that ownership is still live by itself. PostgreSQL checks
/// the token, generation binding, and database-clock expiry under a row lock before
/// committing a fenced mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseFence {
    target: LeaseTarget,
    lease_id: LeaseId,
}

impl LeaseFence {
    /// Protected project operation and generation binding.
    #[must_use]
    pub const fn target(&self) -> &LeaseTarget {
        &self.target
    }

    /// Exact lease token changed by every takeover.
    #[must_use]
    pub const fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }
}

/// Observable lease metadata suitable for diagnostics and agent status output.
///
/// Exact lease tokens are intentionally excluded. Diagnostic metadata must not
/// be convertible into a [`ProjectLease`] or [`LeaseFence`].
///
/// ```compile_fail
/// fn diagnostic_data_is_not_authority(status: &cartograph_db::LeaseStatus) {
///     let _stolen_token = status.lease_id;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LeaseStatus {
    project_id: ProjectId,
    operation: ProjectOperation,
    owner_pid: u32,
    owner_process_start: String,
    generation_id: Option<GenerationId>,
    acquired_at: String,
    heartbeat_at: String,
    expires_at: String,
    expired: bool,
}

impl LeaseStatus {
    /// Protected project.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Protected operation category.
    #[must_use]
    pub const fn operation(&self) -> ProjectOperation {
        self.operation
    }

    /// Owner process identifier.
    #[must_use]
    pub const fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    /// Boot/session-qualified process start marker.
    #[must_use]
    pub fn owner_process_start(&self) -> &str {
        &self.owner_process_start
    }

    /// Optional generation associated with the mutation.
    #[must_use]
    pub const fn generation_id(&self) -> Option<&GenerationId> {
        self.generation_id.as_ref()
    }

    /// Database acquisition timestamp.
    #[must_use]
    pub fn acquired_at(&self) -> &str {
        &self.acquired_at
    }

    /// Database timestamp of the most recent heartbeat.
    #[must_use]
    pub fn heartbeat_at(&self) -> &str {
        &self.heartbeat_at
    }

    /// Database expiry timestamp.
    #[must_use]
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    /// Whether PostgreSQL's clock considers the row eligible for takeover.
    #[must_use]
    pub const fn expired(&self) -> bool {
        self.expired
    }
}

/// Lease failures with credential-safe and query-safe public messages.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LeaseError {
    /// A caller supplied invalid owner or duration metadata.
    #[error("invalid {field} in Cartograph lease request")]
    InvalidInput {
        /// Stable field name; input contents are intentionally omitted.
        field: &'static str,
    },
    /// Another non-expired owner currently holds the project operation.
    #[error("Cartograph project operation is already leased")]
    Busy,
    /// The token expired, was taken over, or was already released.
    #[error("Cartograph project operation lease is no longer owned by this token")]
    Lost,
    /// The operating system could not create a fresh exact lease token.
    #[error("Cartograph could not generate a fresh project operation lease identity")]
    IdentityUnavailable,
    /// A PostgreSQL operation failed without exposing driver or query text.
    #[error("Cartograph PostgreSQL lease operation failed during {operation}")]
    DatabaseOperation {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A durable lease row violated a branded-ID or metadata contract.
    #[error("Cartograph PostgreSQL lease data violates the {field} domain contract")]
    CorruptStoredValue {
        /// Stable field name; stored contents are intentionally omitted.
        field: &'static str,
    },
}

struct AcquiredLease {
    lease_id: LeaseId,
    expires_at: String,
}

struct AcquireTransactionInput<'a> {
    schema: &'a cartograph_config::DatabaseSchema,
    request: &'a LeaseRequest,
    lease_id: &'a LeaseId,
    duration_millis: i64,
}

impl CartographDatabase {
    /// Remove only database-clock-expired lease rows for one project.
    /// Live ownership tokens are never selected by this maintenance path.
    /// # Errors
    ///
    /// Returns an error if PostgreSQL cannot delete only rows whose expiry is
    /// at or before the database clock.
    pub async fn remove_expired_leases(&self, project_id: &ProjectId) -> Result<u64, LeaseError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"DELETE FROM {schema}."project_operation_leases"
                WHERE project_id = CAST($1 AS uuid)
                  AND expires_at <= clock_timestamp()"#
        );
        audited_query(sql)
            .bind(project_id.as_str())
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(|_| database_error("remove-expired"))
    }

    /// Create opaque single-use acquisition and reconciliation capabilities.
    ///
    /// The generated token is deliberately inaccessible to the caller. The
    /// attempt is consumed by [`Self::acquire_reconcilable_lease`], while the
    /// separate probe can only observe whether that exact attempt committed.
    /// # Errors
    ///
    /// Returns an error if target/owner/duration fields are invalid or a
    /// cryptographically opaque lease identifier cannot be generated.
    pub fn prepare_lease_acquisition(
        request: LeaseRequest,
    ) -> Result<(LeaseAcquisitionAttempt, LeaseAcquisitionProbe), LeaseError> {
        validate_request(&request)?;
        let lease_id = random_lease_id()?;
        let probe = LeaseAcquisitionProbe {
            request: request.clone(),
            lease_id: lease_id.clone(),
        };
        Ok((LeaseAcquisitionAttempt { request, lease_id }, probe))
    }

    /// Acquire the project's only live write lease, replacing expired project rows atomically.
    /// # Errors
    ///
    /// Returns an error if the request is invalid, a live lease conflicts, or
    /// the atomic expired-row takeover and lease decode fails.
    pub async fn acquire_lease(&self, request: LeaseRequest) -> Result<ProjectLease, LeaseError> {
        let (attempt, _) = Self::prepare_lease_acquisition(request)?;
        self.acquire_reconcilable_lease(attempt).await
    }

    /// Acquire an operation lease under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if request/deadline validation fails, a live lease
    /// conflicts, or the bounded atomic acquisition cannot commit.
    pub async fn acquire_lease_bounded(
        &self,
        request: LeaseRequest,
        statement_timeout: Duration,
    ) -> Result<ProjectLease, LeaseError> {
        let (attempt, _) = Self::prepare_lease_acquisition(request)?;
        self.acquire_reconcilable_lease_bounded(attempt, statement_timeout)
            .await
    }

    /// Consume one opaque attempt and acquire or take over its target lease.
    /// # Errors
    ///
    /// Returns an error if the single-use attempt is invalid, another live
    /// owner holds the target, or its exact-token acquisition fails.
    pub async fn acquire_reconcilable_lease(
        &self,
        attempt: LeaseAcquisitionAttempt,
    ) -> Result<ProjectLease, LeaseError> {
        self.acquire_attempt(&attempt, None).await
    }

    /// Consume one opaque attempt under a PostgreSQL-side statement deadline.
    ///
    /// A statement-timeout rollback may be retried once inside this same
    /// single-use capability. The exact token never returns to the caller and
    /// cannot be reused for a later takeover.
    /// # Errors
    ///
    /// Returns an error if the deadline/attempt is invalid, a live owner
    /// conflicts, or both bounded exact-token acquisition attempts fail.
    pub async fn acquire_reconcilable_lease_bounded(
        &self,
        attempt: LeaseAcquisitionAttempt,
        statement_timeout: Duration,
    ) -> Result<ProjectLease, LeaseError> {
        let first = self
            .acquire_attempt(&attempt, Some(statement_timeout))
            .await;
        if matches!(
            &first,
            Err(LeaseError::DatabaseOperation {
                operation: "acquire"
            })
        ) {
            self.acquire_attempt(&attempt, Some(statement_timeout))
                .await
        } else {
            first
        }
    }

    async fn acquire_attempt(
        &self,
        attempt: &LeaseAcquisitionAttempt,
        statement_timeout: Option<Duration>,
    ) -> Result<ProjectLease, LeaseError> {
        let duration_millis = validate_request(&attempt.request)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("acquire-begin"))?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("acquire-statement-timeout")),
                Err(_) => Err(database_error("acquire-rollback")),
            };
        }
        let acquired = acquire_transaction(
            &mut transaction,
            AcquireTransactionInput {
                schema: &self.schema,
                request: &attempt.request,
                lease_id: &attempt.lease_id,
                duration_millis,
            },
        )
        .await;
        let acquired = match acquired {
            Ok(acquired) => acquired,
            Err(error) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(error),
                    Err(_) => Err(database_error("acquire-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("acquire-commit"))?;
        Ok(ProjectLease {
            target: attempt.request.target.clone(),
            lease_id: acquired.lease_id,
            duration: attempt.request.duration,
            expires_at: acquired.expires_at,
        })
    }

    /// Extend a lease only when its exact token is still current and unexpired.
    /// # Errors
    ///
    /// Returns an error if lease duration is invalid, the exact token is lost
    /// or expired, PostgreSQL fails, or the new expiry cannot be decoded.
    pub async fn heartbeat_lease(&self, lease: &mut ProjectLease) -> Result<(), LeaseError> {
        let duration_millis = duration_millis(lease.duration)?;
        let schema = crate::database::quoted_schema(&self.schema);
        let row = audited_query(heartbeat_sql(&schema))
            .bind(lease.target.project_id().as_str())
            .bind(lease.target.operation().as_str())
            .bind(lease.lease_id.as_str())
            .bind(duration_millis)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("heartbeat"))?
            .ok_or(LeaseError::Lost)?;
        lease.expires_at = read_nonempty_string(&row, 0, "expires_at")?;
        Ok(())
    }

    /// Extend a lease inside an explicitly rolled-back transaction with a
    /// PostgreSQL-side deadline shorter than the supervising client deadline.
    /// # Errors
    ///
    /// Returns an error if duration/deadline setup fails, the exact token is
    /// lost or expired, or the bounded heartbeat transaction cannot commit.
    pub async fn heartbeat_lease_bounded(
        &self,
        lease: &mut ProjectLease,
        statement_timeout: Duration,
    ) -> Result<(), LeaseError> {
        let duration_millis = duration_millis(lease.duration)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("heartbeat-begin"))?;
        if crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("heartbeat-statement-timeout")),
                Err(_) => Err(database_error("heartbeat-rollback")),
            };
        }
        // A heartbeat is transactional liveness evidence, not mutation authority after a
        // database restart: every prepare and publish still checks the exact live fence. Keeping
        // this commit asynchronous avoids a local WAL fsync stall consuming the whole heartbeat
        // request bound; a crash can only lose the newest extension and therefore fails closed.
        if query("SET LOCAL synchronous_commit = off")
            .execute(&mut *transaction)
            .await
            .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("heartbeat-commit-mode")),
                Err(_) => Err(database_error("heartbeat-rollback")),
            };
        }
        let schema = crate::database::quoted_schema(&self.schema);
        let expires_at = audited_query(heartbeat_sql(&schema))
            .bind(lease.target.project_id().as_str())
            .bind(lease.target.operation().as_str())
            .bind(lease.lease_id.as_str())
            .bind(duration_millis)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("heartbeat"))
            .and_then(|row| row.ok_or(LeaseError::Lost))
            .and_then(|row| read_nonempty_string(&row, 0, "expires_at"));
        let expires_at = match expires_at {
            Ok(expires_at) => expires_at,
            Err(error) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(error),
                    Err(_) => Err(database_error("heartbeat-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("heartbeat-commit"))?;
        lease.expires_at = expires_at;
        Ok(())
    }

    /// Release a lease only when its exact token is still current and unexpired.
    /// # Errors
    ///
    /// Returns an error if the exact token is no longer live or PostgreSQL
    /// cannot atomically delete and commit that lease row.
    pub async fn release_lease(&self, lease: &ProjectLease) -> Result<(), LeaseError> {
        self.release_lease_inner(lease, None).await
    }

    /// Release an exact lease token under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if deadline setup fails, the token is lost/expired, or
    /// the bounded exact-token delete cannot commit.
    pub async fn release_lease_bounded(
        &self,
        lease: &ProjectLease,
        statement_timeout: Duration,
    ) -> Result<(), LeaseError> {
        self.release_lease_inner(lease, Some(statement_timeout))
            .await
    }

    async fn release_lease_inner(
        &self,
        lease: &ProjectLease,
        statement_timeout: Option<Duration>,
    ) -> Result<(), LeaseError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("release-begin"))?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("release-statement-timeout")),
                Err(_) => Err(database_error("release-rollback")),
            };
        }
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"DELETE FROM {schema}."project_operation_leases"
                WHERE project_id = CAST($1 AS uuid)
                  AND operation = $2
                  AND lease_id = CAST($3 AS uuid)
                  AND expires_at > clock_timestamp()"#
        );
        let result = audited_query(sql)
            .bind(lease.target.project_id().as_str())
            .bind(lease.target.operation().as_str())
            .bind(lease.lease_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("release"));
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(error),
                    Err(_) => Err(database_error("release-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("release-commit"))?;
        if result.rows_affected() != 1 {
            return Err(LeaseError::Lost);
        }
        Ok(())
    }

    /// Read current or expired owner metadata without mutating lease state.
    /// # Errors
    ///
    /// Returns an error if owner/timestamp/generation fields cannot be queried
    /// or decoded from the target lease row.
    pub async fn lease_status(
        &self,
        target: &LeaseTarget,
    ) -> Result<Option<LeaseStatus>, LeaseError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"WITH lease_clock AS (SELECT clock_timestamp() AS now)
                SELECT
                    leases.owner_pid,
                    leases.owner_process_start,
                    leases.generation_id::text,
                    leases.acquired_at::text,
                    leases.heartbeat_at::text,
                    leases.expires_at::text,
                    leases.expires_at <= lease_clock.now
                FROM {schema}."project_operation_leases" AS leases
                CROSS JOIN lease_clock
                WHERE leases.project_id = CAST($1 AS uuid)
                  AND leases.operation = $2"#
        );
        let row = audited_query(sql)
            .bind(target.project_id().as_str())
            .bind(target.operation().as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("status"))?;
        row.map(|row| decode_status(&row, target)).transpose()
    }

    /// Report whether any operation currently holds an unexpired lease on the project.
    ///
    /// Lease acquisition admits one live lease per project across every
    /// operation, so a `true` answer means a new acquisition would be `Busy`.
    /// Index admission reads it before reserving a generation; acquisition
    /// remains the authority, so a lease taken after this read is still rejected.
    /// The read runs under the transaction-local `statement_timeout`, so the
    /// bounded admission preflight that calls it stays bounded.
    /// # Errors
    ///
    /// Returns an error if the timeout is invalid, or PostgreSQL cannot evaluate
    /// the database-clock check within `statement_timeout`.
    pub async fn has_live_lease(
        &self,
        project_id: &ProjectId,
        statement_timeout: Duration,
    ) -> Result<bool, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT EXISTS (
                    SELECT 1 FROM {schema}."project_operation_leases"
                    WHERE project_id = CAST($1 AS uuid)
                      AND expires_at > clock_timestamp()
                )"#
        );
        let rows = crate::database::read_project_rows(
            self,
            crate::database::ProjectReadRequest {
                statement,
                project_id,
                operation: "live-lease",
                statement_timeout,
            },
            |statement| statement,
        )
        .await?;
        rows.first().map_or(
            Err(crate::database::stored_value_error("live_lease")),
            |row| crate::database::read_stored_bool(row, 0, "live_lease"),
        )
    }

    /// Recover only the exact opaque acquisition attempt after an ambiguous response.
    /// # Errors
    ///
    /// Returns an error if PostgreSQL cannot read or decode the exact opaque
    /// acquisition token's committed lease state.
    pub async fn reconcile_acquisition(
        &self,
        probe: &LeaseAcquisitionProbe,
    ) -> Result<Option<ProjectLease>, LeaseError> {
        self.reconcile_acquisition_inner(probe, None).await
    }

    /// Reconcile an opaque attempt under a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if deadline setup fails or the exact opaque attempt's
    /// bounded reconciliation cannot be queried, decoded, or committed.
    pub async fn reconcile_acquisition_bounded(
        &self,
        probe: &LeaseAcquisitionProbe,
        statement_timeout: Duration,
    ) -> Result<Option<ProjectLease>, LeaseError> {
        self.reconcile_acquisition_inner(probe, Some(statement_timeout))
            .await
    }

    async fn reconcile_acquisition_inner(
        &self,
        probe: &LeaseAcquisitionProbe,
        statement_timeout: Option<Duration>,
    ) -> Result<Option<ProjectLease>, LeaseError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("reconcile-acquisition-begin"))?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            return match transaction.rollback().await {
                Ok(()) => Err(database_error("reconcile-acquisition-statement-timeout")),
                Err(_) => Err(database_error("reconcile-acquisition-rollback")),
            };
        }
        let result = reconcile_acquisition_query(&mut transaction, &self.schema, probe).await;
        let lease = match result {
            Ok(lease) => lease,
            Err(error) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(error),
                    Err(_) => Err(database_error("reconcile-acquisition-rollback")),
                };
            }
        };
        transaction
            .commit()
            .await
            .map_err(|_| database_error("reconcile-acquisition-commit"))?;
        Ok(lease)
    }
}

async fn reconcile_acquisition_query(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    probe: &LeaseAcquisitionProbe,
) -> Result<Option<ProjectLease>, LeaseError> {
    let schema = crate::database::quoted_schema(schema);
    let sql = format!(
        r#"SELECT expires_at::text
                FROM {schema}."project_operation_leases"
                WHERE project_id = CAST($1 AS uuid)
                  AND operation = $2
                  AND lease_id = CAST($3 AS uuid)
                  AND owner_pid = $4
                  AND owner_process_start = $5
                  AND generation_id IS NOT DISTINCT FROM CAST($6 AS uuid)
                  AND expires_at > clock_timestamp()"#
    );
    let row = audited_query(sql)
        .bind(probe.request.target.project_id().as_str())
        .bind(probe.request.target.operation().as_str())
        .bind(probe.lease_id.as_str())
        .bind(i64::from(probe.request.owner.pid))
        .bind(&probe.request.owner.process_start)
        .bind(
            probe
                .request
                .target
                .generation_id()
                .map(GenerationId::as_str),
        )
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("reconcile-acquisition"))?;
    row.map(|row| {
        Ok(ProjectLease {
            target: probe.request.target.clone(),
            lease_id: probe.lease_id.clone(),
            duration: probe.request.duration,
            expires_at: read_nonempty_string(&row, 0, "expires_at")?,
        })
    })
    .transpose()
}

async fn acquire_transaction(
    connection: &mut PgConnection,
    input: AcquireTransactionInput<'_>,
) -> Result<AcquiredLease, LeaseError> {
    let maintenance_lock =
        query("SELECT pg_try_advisory_xact_lock_shared(hashtextextended($1, 0))")
            .bind(schema_maintenance_lock_key(input.schema))
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| database_error("maintenance-gate"))?
            .try_get::<bool, _>(0)
            .map_err(|_| corrupt("maintenance_gate"))?;
    if !maintenance_lock {
        return Err(LeaseError::Busy);
    }
    let lock_row = query("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(project_lock_key(
            input.schema,
            input.request.target.project_id(),
        ))
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| database_error("advisory-lock"))?;
    let acquired_lock = lock_row
        .try_get::<bool, _>(0)
        .map_err(|_| corrupt("advisory_lock"))?;
    if !acquired_lock {
        return Err(LeaseError::Busy);
    }
    let schema = crate::database::quoted_schema(input.schema);
    let delete_expired = format!(
        r#"DELETE FROM {schema}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND expires_at <= clock_timestamp()"#
    );
    audited_query(delete_expired)
        .bind(input.request.target.project_id().as_str())
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("acquire"))?;
    let sql = format!(
        r#"WITH lease_clock AS (SELECT clock_timestamp() AS now)
            INSERT INTO {schema}."project_operation_leases" (
                project_id, operation, lease_id, owner_pid, owner_process_start,
                generation_id, acquired_at, heartbeat_at, expires_at
            )
            SELECT
                CAST($1 AS uuid), $2, CAST($7 AS uuid),
                $3, $4, CAST($5 AS uuid),
                lease_clock.now, lease_clock.now,
                lease_clock.now + $6 * interval '1 millisecond'
            FROM lease_clock
            WHERE NOT EXISTS (
                SELECT 1 FROM {schema}."project_operation_leases"
                WHERE project_id = CAST($1 AS uuid)
            )
            AND NOT EXISTS (
                SELECT 1 FROM {schema}."index_generations"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($5 AS uuid)
                  AND state = 'retiring'
            )
            ON CONFLICT (project_id, operation) DO NOTHING
            RETURNING lease_id::text, expires_at::text"#
    );
    let row = audited_query(sql)
        .bind(input.request.target.project_id().as_str())
        .bind(input.request.target.operation().as_str())
        .bind(i64::from(input.request.owner.pid))
        .bind(&input.request.owner.process_start)
        .bind(
            input
                .request
                .target
                .generation_id()
                .map(GenerationId::as_str),
        )
        .bind(input.duration_millis)
        .bind(input.lease_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("acquire"))?
        .ok_or(LeaseError::Busy)?;
    let raw_id = read_nonempty_string(&row, 0, "lease_id")?;
    let lease_id = LeaseId::parse(&raw_id).map_err(|_| corrupt("lease_id"))?;
    let expires_at = read_nonempty_string(&row, 1, "expires_at")?;
    Ok(AcquiredLease {
        lease_id,
        expires_at,
    })
}

pub(crate) fn project_lock_key(
    schema: &cartograph_config::DatabaseSchema,
    project_id: &ProjectId,
) -> String {
    format!("{LEASE_LOCK_NAMESPACE}:{}:{}", schema.as_str(), project_id)
}

pub(crate) fn schema_maintenance_lock_key(schema: &cartograph_config::DatabaseSchema) -> String {
    format!("{SCHEMA_MAINTENANCE_LOCK_NAMESPACE}:{}", schema.as_str())
}

fn decode_status(row: &PgRow, target: &LeaseTarget) -> Result<LeaseStatus, LeaseError> {
    let raw_pid = row
        .try_get::<i64, _>(STATUS_OWNER_PID_COLUMN)
        .map_err(|_| corrupt("owner_pid"))?;
    let owner_pid = u32::try_from(raw_pid).map_err(|_| corrupt("owner_pid"))?;
    if owner_pid == 0 {
        return Err(corrupt("owner_pid"));
    }
    let owner_process_start = read_nonempty_string(
        row,
        STATUS_OWNER_PROCESS_START_COLUMN,
        "owner_process_start",
    )?;
    if owner_process_start.len() > MAX_PROCESS_START_BYTES || owner_process_start.contains('\0') {
        return Err(corrupt("owner_process_start"));
    }
    let generation_id = row
        .try_get::<Option<String>, _>(STATUS_GENERATION_ID_COLUMN)
        .map_err(|_| corrupt("generation_id"))?
        .map(|raw| GenerationId::parse(&raw).map_err(|_| corrupt("generation_id")))
        .transpose()?;
    let acquired_at = read_nonempty_string(row, STATUS_ACQUIRED_AT_COLUMN, "acquired_at")?;
    let heartbeat_at = read_nonempty_string(row, STATUS_HEARTBEAT_AT_COLUMN, "heartbeat_at")?;
    let expires_at = read_nonempty_string(row, STATUS_EXPIRES_AT_COLUMN, "expires_at")?;
    let expired = row
        .try_get::<bool, _>(STATUS_EXPIRED_COLUMN)
        .map_err(|_| corrupt("expired"))?;
    Ok(LeaseStatus {
        project_id: target.project_id().clone(),
        operation: target.operation(),
        owner_pid,
        owner_process_start,
        generation_id,
        acquired_at,
        heartbeat_at,
        expires_at,
        expired,
    })
}

fn random_lease_id() -> Result<LeaseId, LeaseError> {
    let mut bytes = [0_u8; UUID_RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| LeaseError::IdentityUnavailable)?;
    bytes[UUID_VERSION_BYTE] =
        (bytes[UUID_VERSION_BYTE] & UUID_VERSION_CLEAR_MASK) | UUID_VERSION_FOUR;
    bytes[UUID_VARIANT_BYTE] =
        (bytes[UUID_VARIANT_BYTE] & UUID_VARIANT_CLEAR_MASK) | UUID_VARIANT_RFC_4122;
    let mut encoded = String::with_capacity(UUID_TEXT_LENGTH);
    for (offset, byte) in bytes.into_iter().enumerate() {
        if UUID_BYTE_HYPHEN_OFFSETS.contains(&offset) {
            encoded.push('-');
        }
        encoded.push(char::from(
            HEX_DIGITS[usize::from(byte >> UPPER_NIBBLE_SHIFT)],
        ));
        encoded.push(char::from(HEX_DIGITS[usize::from(byte & NIBBLE_MASK)]));
    }
    LeaseId::parse(&encoded).map_err(|_| LeaseError::IdentityUnavailable)
}

fn heartbeat_sql(quoted_schema: &str) -> String {
    // `clock_timestamp()` is wall time and may step backward. Preserve the durable timestamp
    // order so a live exact-token heartbeat cannot violate its own database constraint.
    format!(
        r#"WITH lease_clock AS (SELECT clock_timestamp() AS now)
            UPDATE {quoted_schema}."project_operation_leases" AS leases
            SET heartbeat_at = GREATEST(
                    lease_clock.now,
                    leases.acquired_at,
                    leases.heartbeat_at
                ),
                expires_at = GREATEST(
                    lease_clock.now,
                    leases.acquired_at,
                    leases.heartbeat_at
                ) + $4 * interval '1 millisecond'
            FROM lease_clock
            WHERE leases.project_id = CAST($1 AS uuid)
              AND leases.operation = $2
              AND leases.lease_id = CAST($3 AS uuid)
              AND leases.expires_at > lease_clock.now
            RETURNING leases.expires_at::text"#
    )
}

fn validate_request(request: &LeaseRequest) -> Result<i64, LeaseError> {
    if request.owner.pid == 0 {
        return Err(LeaseError::InvalidInput { field: "owner_pid" });
    }
    if request.owner.process_start.trim().is_empty()
        || request.owner.process_start.len() > MAX_PROCESS_START_BYTES
        || request.owner.process_start.contains('\0')
    {
        return Err(LeaseError::InvalidInput {
            field: "owner_process_start",
        });
    }
    duration_millis(request.duration)
}

fn duration_millis(duration: Duration) -> Result<i64, LeaseError> {
    if !(MIN_LEASE_DURATION..=MAX_LEASE_DURATION).contains(&duration) {
        return Err(LeaseError::InvalidInput {
            field: "lease_duration",
        });
    }
    i64::try_from(duration.as_millis()).map_err(|_| LeaseError::InvalidInput {
        field: "lease_duration",
    })
}

fn read_nonempty_string(
    row: &PgRow,
    index: usize,
    field: &'static str,
) -> Result<String, LeaseError> {
    let value = row
        .try_get::<String, _>(index)
        .map_err(|_| corrupt(field))?;
    if value.is_empty() || value.contains('\0') {
        Err(corrupt(field))
    } else {
        Ok(value)
    }
}

const fn database_error(operation: &'static str) -> LeaseError {
    LeaseError::DatabaseOperation { operation }
}

const fn corrupt(field: &'static str) -> LeaseError {
    LeaseError::CorruptStoredValue { field }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_OWNER_PID: u32 = 10;
    const ACCESSOR_OWNER_PID: u32 = 42;
    const VALID_DURATION_SECONDS: u64 = 30;
    const TOO_SHORT_DURATION_MILLIS: u64 = 999;
    const EXPECTED_DURATION_MILLIS: i64 = 30_000;

    fn target() -> LeaseTarget {
        let project_id = match ProjectId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") {
            Ok(project_id) => project_id,
            Err(error) => panic!("fixture project UUID is invalid: {error}"),
        };
        LeaseTarget::new(project_id, ProjectOperation::Index, None)
    }

    #[test]
    fn request_validation_rejects_ambiguous_owners_and_unbounded_durations() {
        let missing_pid = LeaseRequest::new(
            target(),
            LeaseOwner::new(0, "boot-a:100"),
            Duration::from_secs(VALID_DURATION_SECONDS),
        );
        let blank_start = LeaseRequest::new(
            target(),
            LeaseOwner::new(VALID_OWNER_PID, "   "),
            Duration::from_secs(VALID_DURATION_SECONDS),
        );
        let too_short = LeaseRequest::new(
            target(),
            LeaseOwner::new(VALID_OWNER_PID, "boot-a:100"),
            Duration::from_millis(TOO_SHORT_DURATION_MILLIS),
        );
        let too_long = LeaseRequest::new(
            target(),
            LeaseOwner::new(VALID_OWNER_PID, "boot-a:100"),
            MAX_LEASE_DURATION + Duration::from_secs(1),
        );

        assert_eq!(
            validate_request(&missing_pid),
            Err(LeaseError::InvalidInput { field: "owner_pid" })
        );
        assert_eq!(
            validate_request(&blank_start),
            Err(LeaseError::InvalidInput {
                field: "owner_process_start"
            })
        );
        assert_eq!(
            validate_request(&too_short),
            Err(LeaseError::InvalidInput {
                field: "lease_duration"
            })
        );
        assert_eq!(
            validate_request(&too_long),
            Err(LeaseError::InvalidInput {
                field: "lease_duration"
            })
        );
    }

    #[test]
    fn target_and_owner_accessors_preserve_branded_metadata() {
        let target = target();
        let owner = LeaseOwner::new(ACCESSOR_OWNER_PID, "boot-a:100");

        assert_eq!(target.operation(), ProjectOperation::Index);
        assert!(target.generation_id().is_none());
        assert_eq!(owner.pid(), ACCESSOR_OWNER_PID);
        assert_eq!(owner.process_start(), "boot-a:100");
        assert_eq!(
            duration_millis(Duration::from_secs(VALID_DURATION_SECONDS)),
            Ok(EXPECTED_DURATION_MILLIS)
        );
    }

    #[test]
    fn opaque_acquisition_tokens_are_fresh_canonical_v4_values() {
        let request = || {
            LeaseRequest::new(
                target(),
                LeaseOwner::new(VALID_OWNER_PID, "boot-a:100"),
                Duration::from_secs(VALID_DURATION_SECONDS),
            )
        };
        let first = match CartographDatabase::prepare_lease_acquisition(request()) {
            Ok((attempt, _)) => attempt.lease_id,
            Err(error) => panic!("first lease capability was not generated: {error}"),
        };
        let second = match CartographDatabase::prepare_lease_acquisition(request()) {
            Ok((attempt, _)) => attempt.lease_id,
            Err(error) => panic!("second lease capability was not generated: {error}"),
        };
        assert_ne!(first, second);
        assert_eq!(first.as_str().as_bytes().get(14), Some(&b'4'));
        assert!(matches!(
            first.as_str().as_bytes().get(19),
            Some(b'8' | b'9' | b'a' | b'b')
        ));
    }

    #[test]
    fn diagnostic_status_serialization_excludes_exact_mutation_token() {
        let status = LeaseStatus {
            project_id: target().project_id().clone(),
            operation: ProjectOperation::Index,
            owner_pid: VALID_OWNER_PID,
            owner_process_start: "boot-a:100".to_owned(),
            generation_id: None,
            acquired_at: "2026-07-22 00:00:00+00".to_owned(),
            heartbeat_at: "2026-07-22 00:00:01+00".to_owned(),
            expires_at: "2026-07-22 00:00:31+00".to_owned(),
            expired: false,
        };
        let value = match serde_json::to_value(status) {
            Ok(value) => value,
            Err(error) => panic!("diagnostic lease status did not serialize: {error}"),
        };
        assert!(value.get("lease_id").is_none());
    }
}
