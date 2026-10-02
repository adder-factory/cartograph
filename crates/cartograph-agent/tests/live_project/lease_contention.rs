//! Lease contention and failed-cleanup classification for live index attempts.
//!
//! A competing writer must surface as the retryable `IndexLeaseBusy` without
//! reserving or abandoning a generation, and a cleanup that fails after a
//! primary failure must never replace that primary failure.

use tokio::{sync::oneshot, task::JoinHandle};

use super::*;

/// The competing writer's lease outlives every contention probe in a test.
const HOLDER_LEASE_DURATION: Duration = Duration::from_mins(2);
/// Poll interval of the project-lock waiter observer. The agent's bounded
/// staging preflight waits seconds for a held lock, so a waiting attempt stays
/// visible across many polls.
const LOCK_WAIT_POLL: Duration = Duration::from_millis(20);
/// Whether any session is waiting for (not holding) one advisory lock key.
/// A bigint advisory key is split into `classid` (high) and `objid` (low).
const PROJECT_LOCK_WAITER_SQL: &str = "SELECT EXISTS (
        SELECT 1 FROM pg_locks
        WHERE locktype = 'advisory'
          AND NOT granted
          AND objsubid = 1
          AND ((classid::bigint << 32) | objid::bigint) = hashtextextended($1, 0)
    )";
/// Bound on the post-failure lookup of whether a generation is still current.
const VISIBILITY_TIMEOUT: Duration = Duration::from_secs(5);
const ORIGINAL_SOURCE: &str = "export function contendedSource(): number { return 1; }\n";
const CHANGED_SOURCE: &str = "export function contendedSource(): number { return 2; }\n";
const RECOVERED_SOURCE: &str = "export function contendedSource(): number { return 3; }\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn competing_live_writer_reports_lease_busy_without_reserving_a_generation() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = project.path().join("service.ts");
    write_source(&source, ORIGINAL_SOURCE);
    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("contention runtime failed: {error}"));
        assert_eq!(visibility(&runtime).await, Some(false));
        let options = IndexOptions::default().with_history_refresh(false);
        let first = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("contention initial index failed: {error}"));
        assert_eq!(visibility(&runtime).await, Some(true));
        let contention = Contention {
            runtime: &runtime,
            settings: &settings,
            schema: &schema,
            project_id: &first.project_id,
        };
        let writer = CompetingWriter::start(&contention, &first.source_revision).await;

        // A live lease alone, with the project lock free: only the lease
        // pre-check keeps the attempt from reserving a generation that lease
        // acquisition would then reject and abandon.
        write_source(&source, CHANGED_SOURCE);
        contention
            .assert_rejected_before_reservation(&options)
            .await;

        // The writer's long prepare transaction also holds the project lock:
        // the lease pre-check answers without waiting for that lock.
        let prepare = contention.hold_project_lock().await;
        contention
            .assert_rejected_before_reservation(&options)
            .await;

        // An unchanged checkout needs no lease, so contention cannot fail it.
        write_source(&source, ORIGINAL_SOURCE);
        let unchanged = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("unchanged index failed under contention: {error}"));
        assert!(!unchanged.published);
        assert_eq!(unchanged.generation_id, first.generation_id);

        // A project lock still held after the lease is gone is the same
        // contention, answered by the bounded lock wait, not a staging
        // cleanup failure.
        let writer_generation = writer.release_lease(&contention).await;
        write_source(&source, CHANGED_SOURCE);
        let locked = runtime
            .index_with_failure_detail(options.clone())
            .await
            .map(|report| report.generation_id);
        let Err(failure) = locked else {
            panic!("a held project lock did not reject the attempt: {locked:?}");
        };
        assert_eq!(failure.error(), &ProjectError::IndexLeaseBusy);
        assert!(!failure.cleanup_failed());
        contention.assert_nothing_reserved().await;
        assert_eq!(visibility(&runtime).await, Some(true));

        prepare.release().await;
        write_source(&source, RECOVERED_SOURCE);
        let recovered = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("index after contention failed: {error}"));
        assert!(recovered.published);
        let abandoned = runtime
            .database()
            .generation_state(&first.project_id, &writer_generation)
            .await
            .unwrap_or_else(|error| panic!("abandoned staging state failed: {error}"));
        assert!(
            matches!(abandoned, None | Some(GenerationState::Failed)),
            "the released writer's staging generation stayed nonterminal: {abandoned:?}"
        );
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

/// One indexed project observed while another writer contends for it.
struct Contention<'a> {
    runtime: &'a ProjectRuntime,
    settings: &'a DatabaseSettings,
    schema: &'a str,
    project_id: &'a cartograph_domain::ProjectId,
}

impl Contention<'_> {
    /// A changed checkout under a live competing lease is the retryable
    /// `IndexLeaseBusy`, answered by the live-lease pre-check without waiting
    /// for the project lock, with no generation reserved or abandoned.
    async fn assert_rejected_before_reservation(&self, options: &IndexOptions) {
        let observer = ProjectLockWaitObserver::start(self).await;
        let attempt = self
            .runtime
            .index_with_failure_detail(options.clone())
            .await
            .map(|report| report.generation_id);
        let waited = observer.finish().await;
        let Err(failure) = attempt else {
            panic!("a live competing lease did not reject the attempt: {attempt:?}");
        };
        assert_eq!(failure.error(), &ProjectError::IndexLeaseBusy);
        assert!(!failure.cleanup_failed());
        assert!(
            !waited,
            "contention was answered by the bounded project-lock wait instead of the live-lease pre-check"
        );
        self.assert_nothing_reserved().await;
    }

    /// The advisory key of this project's operation lock.
    fn project_lock_key(&self) -> String {
        format!(
            "cartograph-v2-operation:{}:{}",
            self.schema, self.project_id
        )
    }

    /// Hold the project operation lock in an open transaction, as a competing
    /// writer's long prepare transaction does for its whole COPY.
    async fn hold_project_lock(&self) -> HeldLock {
        hold_advisory_lock(self.settings, self.project_lock_key()).await
    }

    /// Only the published generation and the competing writer's own staging
    /// generation exist: the contended attempt reserved and failed nothing.
    async fn assert_nothing_reserved(&self) {
        assert_eq!(
            generation_counts(self.settings, self.schema, self.project_id).await,
            GenerationCounts {
                staging: 1,
                failed: 0,
                total: 2,
            },
            "a contended attempt reserved or abandoned a generation"
        );
    }
}

/// Another process's index: its staging generation and the live index lease
/// that owns it. Its prepare transaction's project lock is held separately.
struct CompetingWriter {
    generation_id: cartograph_domain::GenerationId,
    lease: cartograph_db::ProjectLease,
}

impl CompetingWriter {
    async fn start(contention: &Contention<'_>, revision: &ContentDigest) -> Self {
        let database = contention.runtime.database();
        let staged = database
            .begin_generation(NewGeneration::new(
                contention.project_id.clone(),
                revision.as_str(),
                1,
            ))
            .await
            .unwrap_or_else(|error| panic!("competing staging fixture failed: {error}"));
        let lease = database
            .acquire_lease(LeaseRequest::new(
                LeaseTarget::new(
                    contention.project_id.clone(),
                    ProjectOperation::Index,
                    Some(staged.generation_id().clone()),
                ),
                LeaseOwner::new(process::id(), "competing-auto-sync-index"),
                HOLDER_LEASE_DURATION,
            ))
            .await
            .unwrap_or_else(|error| panic!("competing index lease failed: {error}"));
        Self {
            generation_id: staged.generation_id().clone(),
            lease,
        }
    }

    /// Release the lease, leaving its staging generation unleased.
    async fn release_lease(self, contention: &Contention<'_>) -> cartograph_domain::GenerationId {
        contention
            .runtime
            .database()
            .release_lease(&self.lease)
            .await
            .unwrap_or_else(|error| panic!("competing lease release failed: {error}"));
        self.generation_id
    }
}

/// Watches, from its own session, whether any session waits for the project
/// operation lock while one contended attempt runs.
struct ProjectLockWaitObserver {
    stop: oneshot::Sender<()>,
    observer: JoinHandle<bool>,
}

impl ProjectLockWaitObserver {
    async fn start(contention: &Contention<'_>) -> Self {
        let pool = cartograph_db::connect(contention.settings)
            .await
            .unwrap_or_else(|error| panic!("lock observer connection failed: {error}"));
        let key = contention.project_lock_key();
        let (stop, mut stopped) = oneshot::channel::<()>();
        let observer = tokio::spawn(async move {
            let mut waited = false;
            loop {
                let row = query(PROJECT_LOCK_WAITER_SQL)
                    .bind(&key)
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|error| panic!("lock observer query failed: {error}"));
                waited |= row
                    .try_get::<bool, _>(0)
                    .unwrap_or_else(|error| panic!("lock observer row failed: {error}"));
                tokio::select! {
                    _ = &mut stopped => break,
                    () = tokio::time::sleep(LOCK_WAIT_POLL) => {}
                }
            }
            pool.close().await;
            waited
        });
        Self { stop, observer }
    }

    /// Stop observing and report whether any session waited for the lock.
    async fn finish(self) -> bool {
        let _stopped = self.stop.send(());
        self.observer
            .await
            .unwrap_or_else(|error| panic!("lock observer failed: {error}"))
    }
}

fn write_source(path: &Path, contents: &str) {
    std::fs::write(path, contents)
        .unwrap_or_else(|error| panic!("contention source write failed: {error}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn failed_cleanup_after_a_primary_failure_keeps_the_primary_code() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = project.path().join("service.ts");
    write_source(&source, ORIGINAL_SOURCE);
    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("cleanup runtime failed: {error}"));
        let options = IndexOptions::default().with_history_refresh(false);
        let first = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("cleanup initial index failed: {error}"));
        write_source(&source, CHANGED_SOURCE);
        install_staging_failure_rejection(&settings, &schema).await;
        // Schema maintenance rejects lease acquisition after the generation is
        // reserved: the primary failure is contention, and the reserved
        // generation's cleanup then fails on the rejecting trigger.
        let maintenance = hold_schema_maintenance_lock(&settings, &schema).await;

        let contended = runtime
            .index_with_failure_detail(options.clone())
            .await
            .map(|report| report.generation_id);
        let Err(failure) = contended else {
            panic!("schema maintenance did not reject the lease: {contended:?}");
        };
        assert_eq!(failure.error(), &ProjectError::IndexLeaseBusy);
        assert!(failure.cleanup_failed());
        assert_eq!(visibility(&runtime).await, Some(true));
        assert_eq!(
            generation_counts(&settings, &schema, &first.project_id).await,
            GenerationCounts {
                staging: 1,
                failed: 0,
                total: 2,
            }
        );

        maintenance.release().await;
        remove_staging_failure_rejection(&settings, &schema).await;
        let recovered = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("index after failed cleanup did not recover: {error}"));
        assert!(recovered.published);
        let counts = generation_counts(&settings, &schema, &first.project_id).await;
        assert_eq!(
            counts.staging, 0,
            "the preflight left the loser's staging row"
        );
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

async fn visibility(runtime: &ProjectRuntime) -> Option<bool> {
    runtime
        .database()
        .root_has_current_generation(runtime.root_identity(), VISIBILITY_TIMEOUT)
        .await
        .ok()
}

#[derive(Debug, PartialEq, Eq)]
struct GenerationCounts {
    staging: i64,
    failed: i64,
    total: i64,
}

async fn generation_counts(
    settings: &DatabaseSettings,
    schema: &str,
    project_id: &cartograph_domain::ProjectId,
) -> GenerationCounts {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("generation count connection failed: {error}"));
    let row = query(AssertSqlSafe(format!(
        r#"SELECT
                count(*) FILTER (WHERE state = 'staging')::bigint AS staging,
                count(*) FILTER (WHERE state = 'failed')::bigint AS failed,
                count(*)::bigint AS total
            FROM "{schema}"."index_generations"
            WHERE project_id = CAST($1 AS uuid)"#
    )))
    .bind(project_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap_or_else(|error| panic!("generation count failed: {error}"));
    let read = |column: &str| {
        row.try_get::<i64, _>(column)
            .unwrap_or_else(|error| panic!("generation count column {column} failed: {error}"))
    };
    let counts = GenerationCounts {
        staging: read("staging"),
        failed: read("failed"),
        total: read("total"),
    };
    pool.close().await;
    counts
}

/// One advisory lock held by a separate session until released.
pub(super) struct HeldLock {
    release: oneshot::Sender<()>,
    holder: JoinHandle<()>,
}

impl HeldLock {
    pub(super) async fn release(self) {
        let _released = self.release.send(());
        self.holder
            .await
            .unwrap_or_else(|error| panic!("advisory lock holder failed: {error}"));
    }
}

/// Hold the schema maintenance lock exclusively, which rejects every new lease.
pub(super) async fn hold_schema_maintenance_lock(
    settings: &DatabaseSettings,
    schema: &str,
) -> HeldLock {
    hold_advisory_lock(
        settings,
        format!("cartograph-v2-schema-maintenance:{schema}"),
    )
    .await
}

async fn hold_advisory_lock(settings: &DatabaseSettings, key: String) -> HeldLock {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("advisory lock connection failed: {error}"));
    let (locked_sender, locked) = oneshot::channel();
    let (release, released) = oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let mut transaction = pool
            .begin()
            .await
            .unwrap_or_else(|error| panic!("advisory lock transaction failed: {error}"));
        query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut *transaction)
            .await
            .unwrap_or_else(|error| panic!("advisory lock failed: {error}"));
        let _locked = locked_sender.send(());
        let _released = released.await;
        transaction
            .rollback()
            .await
            .unwrap_or_else(|error| panic!("advisory lock release failed: {error}"));
        pool.close().await;
    });
    locked
        .await
        .unwrap_or_else(|error| panic!("advisory lock holder stopped: {error}"));
    HeldLock { release, holder }
}

/// Make every staging-to-failed transition fail, so cleanup cannot complete.
async fn install_staging_failure_rejection(settings: &DatabaseSettings, schema: &str) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}"."reject_staging_failure"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $reject$
            BEGIN
                RAISE EXCEPTION 'staging cleanup rejected by test fixture';
            END
            $reject$"#
    );
    let trigger = format!(
        r#"CREATE TRIGGER reject_staging_failure
            BEFORE UPDATE OF state ON "{schema}"."index_generations"
            FOR EACH ROW
            WHEN (OLD.state = 'staging' AND NEW.state = 'failed')
            EXECUTE FUNCTION "{schema}"."reject_staging_failure"()"#
    );
    execute_statements(settings, [function, trigger]).await;
}

async fn remove_staging_failure_rejection(settings: &DatabaseSettings, schema: &str) {
    execute_statements(
        settings,
        [
            format!(r#"DROP TRIGGER reject_staging_failure ON "{schema}"."index_generations""#),
            format!(r#"DROP FUNCTION "{schema}"."reject_staging_failure"()"#),
        ],
    )
    .await;
}

async fn execute_statements<const COUNT: usize>(
    settings: &DatabaseSettings,
    statements: [String; COUNT],
) {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("fixture statement connection failed: {error}"));
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("fixture statement failed: {error}"));
    }
    pool.close().await;
}
