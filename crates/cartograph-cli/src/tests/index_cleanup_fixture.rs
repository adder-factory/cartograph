//! Live fixture for an index attempt whose own staging cleanup also fails.
//!
//! Schema maintenance held by another session refuses the attempt's lease
//! after it reserved a generation, so the attempt fails with `lease_busy`;
//! a trigger then rejects that generation's staging-to-failed transition, so
//! its bounded cleanup fails as well.

use cartograph_config::DatabaseSettings;
use sqlx_core::{query::query, sql_str::AssertSqlSafe};
use tokio::{sync::oneshot, task::JoinHandle};

/// Advisory-lock namespace of PostgreSQL schema maintenance.
const SCHEMA_MAINTENANCE_NAMESPACE: &str = "cartograph-v2-schema-maintenance";

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

/// Schema maintenance held by a separate session until released; while it is
/// held, every new project lease is refused.
pub(crate) struct HeldSchemaMaintenance {
    release: oneshot::Sender<()>,
    holder: JoinHandle<()>,
}

impl HeldSchemaMaintenance {
    /// Take the schema maintenance lock of `schema` in its own transaction.
    pub(crate) async fn hold(settings: &DatabaseSettings, schema: &str) -> Self {
        let pool = cartograph_db::connect(settings)
            .await
            .unwrap_or_else(|error| panic!("maintenance lock connection failed: {error}"));
        let key = [SCHEMA_MAINTENANCE_NAMESPACE, schema].join(":");
        let (locked_sender, locked) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let holder = tokio::spawn(async move {
            let mut transaction = pool
                .begin()
                .await
                .unwrap_or_else(|error| panic!("maintenance transaction failed: {error}"));
            query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(key)
                .execute(&mut *transaction)
                .await
                .unwrap_or_else(|error| panic!("maintenance lock failed: {error}"));
            let _locked = locked_sender.send(());
            let _released = released.await;
            transaction
                .rollback()
                .await
                .unwrap_or_else(|error| panic!("maintenance release failed: {error}"));
            pool.close().await;
        });
        locked
            .await
            .unwrap_or_else(|error| panic!("maintenance holder stopped: {error}"));
        Self { release, holder }
    }

    /// Release the lock and wait for its session to end.
    pub(crate) async fn release(self) {
        let _released = self.release.send(());
        self.holder
            .await
            .unwrap_or_else(|error| panic!("maintenance holder failed: {error}"));
    }
}
