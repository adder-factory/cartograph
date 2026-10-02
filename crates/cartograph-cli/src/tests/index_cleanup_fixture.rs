//! Live fixture for an index attempt whose own staging cleanup also fails.
//!
//! Schema maintenance held by another session refuses the attempt's lease
//! after it reserved a generation, so the attempt fails with `lease_busy`;
//! a trigger then rejects that generation's staging-to-failed transition, so
//! its bounded cleanup fails as well. A project row held by another session
//! pauses the attempt inside that reservation, so a test can act at a known
//! point before the attempt fails.

use std::time::{Duration, Instant};

use cartograph_config::DatabaseSettings;
use cartograph_domain::ProjectId;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use tokio::{sync::oneshot, task::JoinHandle};

/// Advisory-lock namespace of PostgreSQL schema maintenance.
const SCHEMA_MAINTENANCE_NAMESPACE: &str = "cartograph-v2-schema-maintenance";
/// Whether any session waits for a lock that backend `$1` holds.
const BLOCKED_BY_SQL: &str = "SELECT EXISTS (
        SELECT 1 FROM pg_catalog.pg_stat_activity
        WHERE $1 = ANY (pg_catalog.pg_blocking_pids(pid))
    )";
/// Bound for an index to reach its reservation behind a held project row.
const REACH_RESERVATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval between observations of that wait.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Make every staging-to-failed transition in `schema` fail.
pub(crate) async fn reject_staging_failure(settings: &DatabaseSettings, schema: &str) {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("staging rejection connection failed: {error}"));
    for statement in [
        format!(
            r#"CREATE FUNCTION "{schema}"."reject_staging_failure"()
                RETURNS trigger
                LANGUAGE plpgsql
                AS $reject$
                BEGIN
                    RAISE EXCEPTION 'staging cleanup rejected by test fixture';
                END
                $reject$"#
        ),
        format!(
            r#"CREATE TRIGGER reject_staging_failure
                BEFORE UPDATE OF state ON "{schema}"."index_generations"
                FOR EACH ROW
                WHEN (OLD.state = 'staging' AND NEW.state = 'failed')
                EXECUTE FUNCTION "{schema}"."reject_staging_failure"()"#
        ),
    ] {
        query(AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("staging rejection fixture failed: {error}"));
    }
    pool.close().await;
}

/// One lock taken in a separate session's transaction and held until released.
struct HeldLock {
    /// Backend that holds the lock; a session waiting on it lists it as blocker.
    backend: i32,
    release: oneshot::Sender<()>,
    holder: JoinHandle<()>,
}

impl HeldLock {
    /// Run the locking `statement` with its one text `argument` in its own
    /// transaction; returns once the lock is held.
    async fn hold(settings: &DatabaseSettings, statement: String, argument: String) -> Self {
        let pool = cartograph_db::connect(settings)
            .await
            .unwrap_or_else(|error| panic!("lock connection failed: {error}"));
        let (locked_sender, locked) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let holder = tokio::spawn(async move {
            let mut transaction = pool
                .begin()
                .await
                .unwrap_or_else(|error| panic!("lock transaction failed: {error}"));
            let backend = query("SELECT pg_catalog.pg_backend_pid()")
                .fetch_one(&mut *transaction)
                .await
                .and_then(|row| row.try_get::<i32, _>(0))
                .unwrap_or_else(|error| panic!("lock backend lookup failed: {error}"));
            query(AssertSqlSafe(statement))
                .bind(argument)
                .execute(&mut *transaction)
                .await
                .unwrap_or_else(|error| panic!("lock statement failed: {error}"));
            let _locked = locked_sender.send(backend);
            let _released = released.await;
            transaction
                .rollback()
                .await
                .unwrap_or_else(|error| panic!("lock release failed: {error}"));
            pool.close().await;
        });
        let backend = locked
            .await
            .unwrap_or_else(|error| panic!("lock holder stopped: {error}"));
        Self {
            backend,
            release,
            holder,
        }
    }

    /// Release the lock and wait for its session to end.
    async fn release(self) {
        let _released = self.release.send(());
        self.holder
            .await
            .unwrap_or_else(|error| panic!("lock holder failed: {error}"));
    }
}

/// Schema maintenance held by a separate session until released; while it is
/// held, every new project lease is refused.
pub(crate) struct HeldSchemaMaintenance(HeldLock);

impl HeldSchemaMaintenance {
    /// Take the schema maintenance lock of `schema` in its own transaction.
    pub(crate) async fn hold(settings: &DatabaseSettings, schema: &str) -> Self {
        let key = [SCHEMA_MAINTENANCE_NAMESPACE, schema].join(":");
        Self(
            HeldLock::hold(
                settings,
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))".to_owned(),
                key,
            )
            .await,
        )
    }

    /// Release the lock and wait for its session to end.
    pub(crate) async fn release(self) {
        self.0.release().await;
    }
}

/// A project row held by a separate session until released: an index of that
/// project pauses inside its generation reservation, after everything it did
/// before reserving and before it acquires its lease.
pub(crate) struct HeldProjectRow {
    lock: HeldLock,
    settings: DatabaseSettings,
}

impl HeldProjectRow {
    /// Lock the row of `project_id` in `schema` in its own transaction.
    pub(crate) async fn hold(
        settings: &DatabaseSettings,
        schema: &str,
        project_id: &ProjectId,
    ) -> Self {
        let statement = format!(
            r#"SELECT 1 FROM "{schema}"."projects"
                WHERE project_id = CAST($1 AS uuid) FOR NO KEY UPDATE"#
        );
        Self {
            lock: HeldLock::hold(settings, statement, project_id.as_str().to_owned()).await,
            settings: settings.clone(),
        }
    }

    /// Wait until a session is blocked behind the held row. An index first
    /// writes that row when it registers the project and takes the next
    /// generation sequence, so the blocked session is an index inside its
    /// reservation; a caller that asserts the reserved generation's cleanup
    /// outcome confirms that it reserved one.
    pub(crate) async fn wait_until_reserving(&self) {
        let pool = cartograph_db::connect(&self.settings)
            .await
            .unwrap_or_else(|error| panic!("lock inspection connection failed: {error}"));
        let started = Instant::now();
        loop {
            let blocked = query(BLOCKED_BY_SQL)
                .bind(self.lock.backend)
                .fetch_one(&pool)
                .await
                .and_then(|row| row.try_get::<bool, _>(0))
                .unwrap_or_else(|error| panic!("lock inspection failed: {error}"));
            if blocked {
                pool.close().await;
                return;
            }
            assert!(
                started.elapsed() < REACH_RESERVATION_TIMEOUT,
                "the index never paused inside its reservation"
            );
            tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        }
    }

    /// Release the row and wait for its session to end.
    pub(crate) async fn release(self) {
        self.lock.release().await;
    }
}
