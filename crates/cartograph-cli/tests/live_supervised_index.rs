//! Live coverage for cooperative cancellation of `cartograph index` and for
//! the supervised index child that `upgrade --apply` runs.
//!
//! A PostgreSQL `SHARE` lock on the fact table holds the real child inside
//! its generation COPY while it owns the project lease, so the stop request
//! (or a checkout edit) deterministically lands mid-build. A held project
//! advisory lock stands in for another writer's prepare transaction.

mod dependency_ownership;

use std::{
    panic::{AssertUnwindSafe, resume_unwind},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{CartographDatabase, LeaseOwner, LeaseRequest, LeaseTarget, NewGeneration};
use cartograph_domain::{ProjectId, ProjectOperation};
use futures_util::FutureExt as _;
use serde_json::Value;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use tokio::{
    io::AsyncReadExt as _,
    process::{Child, ChildStdin, Command},
    sync::oneshot,
    task::JoinHandle,
};

const DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
/// Longer than the indexer supervisor's 10-second cancellation grace, so the
/// prepare transaction is still blocked when the supervisor asks it to stop.
const LOCK_HOLD_AFTER_STOP: Duration = Duration::from_secs(15);
/// The stopped child must exit, with its lease released, this soon after its
/// blocked statement can run again.
const PROMPT_RELEASE: Duration = Duration::from_secs(30);
/// Bound for the child to reach a statement that waits on a held lock.
const REACH_LOCK_TIMEOUT: Duration = Duration::from_mins(2);
/// Bound for the supervised child to report that it is waiting for the
/// other writer.
const OBSERVE_WAIT_TIMEOUT: Duration = Duration::from_mins(1);
/// Bound for a supervised child to finish once the project is free.
const FINISH_TIMEOUT: Duration = Duration::from_mins(2);
/// Interval between database observations.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Interval between observations of a lock wait. A competing writer must
/// appear within the child's 5-second staging-cleanup lock bound, so the
/// wait is detected quickly.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Statement bound for a child that the test holds inside its reservation
/// while it sets up a competing writer, well above that setup's duration.
const RESERVATION_HOLD_QUERY_TIMEOUT_MS: &str = "60000";
/// Lease length of a stand-in competing writer; it outlives each scenario.
const COMPETING_LEASE_DURATION: Duration = Duration::from_mins(2);
/// Whether any session waits for a lock that backend `$1` holds.
const BLOCKED_BY_SQL: &str = "SELECT EXISTS (
        SELECT 1 FROM pg_catalog.pg_stat_activity
        WHERE $1 = ANY (pg_catalog.pg_blocking_pids(pid))
    )";
/// The backend serving the current connection.
const BACKEND_PID_SQL: &str = "SELECT pg_catalog.pg_backend_pid()";

/// One schema-isolated project checkout bound to the live test database.
struct LiveProject {
    database_url: String,
    schema: String,
    settings: DatabaseSettings,
    directory: tempfile::TempDir,
}

impl LiveProject {
    fn new(label: &str) -> Self {
        let database_url = std::env::var(DATABASE_URL_ENV)
            .unwrap_or_else(|_| panic!("live supervised index database is not configured"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let schema = format!("cg_cli_{label}_{}_{nanos}", std::process::id());
        let settings = DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
            .and_then(|settings| settings.with_schema(&schema))
            .unwrap_or_else(|error| panic!("live settings failed: {error}"));
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        write_source(directory.path(), 1);
        Self {
            database_url,
            schema,
            settings,
            directory,
        }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cartograph"));
        command
            .arg("--no-color")
            .args(arguments)
            .current_dir(self.directory.path())
            .env("CARTOGRAPH_DATABASE_URL", &self.database_url)
            .env("CARTOGRAPH_DATABASE_SCHEMA", &self.schema)
            .env("CARTOGRAPH_DATABASE_MAX_CONNECTIONS", "8")
            .env("CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS", "10000")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true);
        command
    }

    fn path(&self) -> String {
        self.directory.path().to_string_lossy().into_owned()
    }

    /// Publish the first generation directly and return its project identity.
    async fn index_once(&self) -> (ProjectId, String) {
        let output = self
            .command(&["index", &self.path(), "--format", "json"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap_or_else(|error| panic!("initial index did not start: {error}"));
        assert!(
            output.status.success(),
            "initial index failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("initial index report was not JSON: {error}"));
        let project_id = report["project_id"]
            .as_str()
            .and_then(|value| ProjectId::parse(value).ok())
            .unwrap_or_else(|| panic!("initial index omitted its project: {report}"));
        let generation = report["generation_id"]
            .as_str()
            .unwrap_or_else(|| panic!("initial index omitted its generation: {report}"))
            .to_owned();
        (project_id, generation)
    }

    async fn database(&self) -> CartographDatabase {
        let pool = cartograph_db::connect(&self.settings)
            .await
            .unwrap_or_else(|error| panic!("live connection failed: {error}"));
        CartographDatabase::new(pool, self.settings.schema().clone())
    }

    async fn drop_schema(&self) {
        let pool = cartograph_db::connect(&self.settings)
            .await
            .unwrap_or_else(|error| panic!("cleanup connection failed: {error}"));
        query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE",
            self.schema
        )))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("cleanup failed: {error}"));
        pool.close().await;
    }
}

fn write_source(root: &Path, revision: u32) {
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("source fixture directory failed: {error}"));
    std::fs::write(
        root.join("src/lib.rs"),
        format!(
            "pub fn leaf(value: i32) -> i32 {{ value + {revision} }}\npub fn root(value: i32) -> i32 {{ leaf(value) }}\n"
        ),
    )
    .unwrap_or_else(|error| panic!("source fixture write failed: {error}"));
}

/// A running child with both output pipes drained in the background. Its
/// stdin is held separately: `Child::wait` would otherwise close it, which a
/// supervised index treats as a stop request.
struct RunningChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<()>,
    stderr_seen: Arc<Mutex<Vec<u8>>>,
}

impl RunningChild {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("index child did not start: {error}"));
        let stdin = child.stdin.take();
        let stdout = drain(child.stdout.take());
        let stderr_seen = Arc::new(Mutex::new(Vec::new()));
        let stderr = drain_into(child.stderr.take(), stderr_seen.clone());
        Self {
            child,
            stdin,
            stdout,
            stderr,
            stderr_seen,
        }
    }

    /// Everything the child has written to stderr so far.
    fn stderr_text(&self) -> String {
        self.stderr_seen
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    async fn finish(mut self, bound: Duration) -> FinishedChild {
        let status = tokio::time::timeout(bound, self.child.wait())
            .await
            .unwrap_or_else(|_| panic!("index child did not exit within {bound:?}"))
            .unwrap_or_else(|error| panic!("index child wait failed: {error}"));
        drop(self.stdin.take());
        let stderr = self.stderr_seen.clone();
        let stdout = self.stdout.await.unwrap_or_default();
        let _ = self.stderr.await;
        let stderr = stderr
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        FinishedChild {
            succeeded: status.success(),
            stdout: String::from_utf8_lossy_owned(stdout),
            stderr,
        }
    }
}

/// How a child exited and everything it wrote.
struct FinishedChild {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

fn drain(pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes).await;
        }
        bytes
    })
}

/// Append a pipe to `sink` as it arrives so a test can observe it mid-run.
fn drain_into(
    pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    sink: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut buffer = [0_u8; 4_096];
        while let Ok(read) = pipe.read(&mut buffer).await {
            if read == 0 {
                return;
            }
            if let Ok(mut seen) = sink.lock() {
                seen.extend_from_slice(&buffer[..read]);
            }
        }
    })
}

/// The `error` object of the last column-zero JSON document on stderr.
fn final_failure(stderr: &str) -> Option<Value> {
    let start = stderr.rfind("\n{").map_or(0, |position| position + 1);
    let mut failure: Value = serde_json::from_str(&stderr[start..]).ok()?;
    failure.get_mut("error").map(Value::take)
}

/// The primary `error.code` and the secondary `error.cleanup_failure.code`
/// of the final failure on stderr.
fn failure_codes(stderr: &str) -> (Option<String>, Option<String>) {
    let Some(failure) = final_failure(stderr) else {
        return (None, None);
    };
    let code = |value: &Value| value["code"].as_str().map(str::to_owned);
    (
        code(&failure),
        failure.get("cleanup_failure").and_then(code),
    )
}

fn progress_lines(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| value.get("progress").cloned())
        .collect()
}

/// How the scenario asks the running index to stop.
#[derive(Clone, Copy)]
enum StopRequest {
    /// The supervising parent closes the child's stdin.
    CloseSupervisorStdin,
    /// An operator or service manager sends SIGTERM to a direct index.
    #[cfg(unix)]
    Terminate,
}

/// One mid-COPY stop scenario.
#[derive(Clone, Copy)]
struct StopScenario {
    stop: StopRequest,
    /// Another writer reserves its own staging generation while the index
    /// is stopping; it is not the stopped index's to clean up.
    foreign_staging: bool,
}

/// One lock statement held in an open transaction by a background task until
/// released: a `SHARE` lock on the fact table stalls a generation COPY, and a
/// lock on the project row stalls a generation reservation.
struct HeldLock {
    /// Backend that holds the lock; sessions waiting on it list it as blocker.
    backend: i32,
    release: oneshot::Sender<()>,
    holder: JoinHandle<()>,
}

impl HeldLock {
    async fn hold(project: &LiveProject, statement: String) -> Self {
        let pool = cartograph_db::connect(&project.settings)
            .await
            .unwrap_or_else(|error| panic!("lock connection failed: {error}"));
        let (locked, held) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let holder = tokio::spawn(async move {
            let mut transaction = pool
                .begin()
                .await
                .unwrap_or_else(|error| panic!("lock transaction failed: {error}"));
            let backend = query(BACKEND_PID_SQL)
                .fetch_one(&mut *transaction)
                .await
                .and_then(|row| row.try_get::<i32, _>(0))
                .unwrap_or_else(|error| panic!("lock backend lookup failed: {error}"));
            query(AssertSqlSafe(statement))
                .execute(&mut *transaction)
                .await
                .unwrap_or_else(|error| panic!("lock statement failed: {error}"));
            let _ = locked.send(backend);
            let _ = released.await;
            transaction
                .commit()
                .await
                .unwrap_or_else(|error| panic!("unlock failed: {error}"));
            pool.close().await;
        });
        let backend = held
            .await
            .unwrap_or_else(|_| panic!("the lock was never taken"));
        Self {
            backend,
            release,
            holder,
        }
    }

    /// Stall every generation COPY into the project's fact table.
    async fn fact_table(project: &LiveProject) -> Self {
        let statement = format!("LOCK TABLE \"{}\".\"files\" IN SHARE MODE", project.schema);
        Self::hold(project, statement).await
    }

    async fn release(self) {
        let _ = self.release.send(());
        self.holder
            .await
            .unwrap_or_else(|error| panic!("lock holder failed: {error}"));
    }
}

/// Poll until some session waits for a lock that `holder` holds.
async fn wait_until_blocked_by(project: &LiveProject, holder: i32) {
    let pool = cartograph_db::connect(&project.settings)
        .await
        .unwrap_or_else(|error| panic!("lock inspection connection failed: {error}"));
    let started = Instant::now();
    loop {
        let blocked = query(BLOCKED_BY_SQL)
            .bind(holder)
            .fetch_one(&pool)
            .await
            .and_then(|row| row.try_get::<bool, _>(0))
            .unwrap_or_else(|error| panic!("lock inspection failed: {error}"));
        if blocked {
            pool.close().await;
            return;
        }
        assert!(
            started.elapsed() < REACH_LOCK_TIMEOUT,
            "the index never waited on the held lock"
        );
        tokio::time::sleep(LOCK_POLL_INTERVAL).await;
    }
}

/// A stand-in for another writer inside its prepare transaction: one
/// database session that holds the project lock and, after
/// [`Self::take_lease`], a live index lease taken through that same session.
/// Advisory locks are re-entrant per session, so its own acquisition is not
/// refused, and the lease can appear after the child passed its writer wait.
struct CompetingWriter {
    /// Backend of the writer's only connection.
    backend: i32,
    take_lease: Option<oneshot::Sender<()>>,
    leased: Option<oneshot::Receiver<()>>,
    finish: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl CompetingWriter {
    /// Take the project lock; returns once it is held.
    async fn start(project: &LiveProject, project_id: &ProjectId) -> Self {
        let settings = project
            .settings
            .clone()
            .with_max_connections(1)
            .unwrap_or_else(|error| panic!("single-connection settings failed: {error}"));
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("competing writer connection failed: {error}"));
        let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
        let project_lock = format!("cartograph-v2-operation:{}:{project_id}", project.schema);
        let target = index_lease(project_id);
        let (locked, held) = oneshot::channel();
        let (take_lease, lease_requested) = oneshot::channel::<()>();
        let (leased_sender, leased) = oneshot::channel();
        let (finish, finished) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let backend_pid = || async {
                query(BACKEND_PID_SQL)
                    .fetch_one(&pool)
                    .await
                    .and_then(|row| row.try_get::<i32, _>(0))
                    .unwrap_or_else(|error| panic!("writer backend lookup failed: {error}"))
            };
            let backend = backend_pid().await;
            query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
                .bind(&project_lock)
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("project lock failed: {error}"));
            let _ = locked.send(backend);
            let lease = match lease_requested.await {
                Ok(()) => Some(
                    database
                        .acquire_lease(LeaseRequest::new(
                            target,
                            LeaseOwner::new(std::process::id(), "live-supervised-competing-writer"),
                            COMPETING_LEASE_DURATION,
                        ))
                        .await
                        .unwrap_or_else(|error| panic!("competing writer lease failed: {error}")),
                ),
                Err(_) => None,
            };
            // The lease came from the very session that holds the lock.
            assert_eq!(backend_pid().await, backend);
            let _ = leased_sender.send(());
            let _ = finished.await;
            if let Some(lease) = lease {
                database
                    .release_lease(&lease)
                    .await
                    .unwrap_or_else(|error| panic!("competing writer release failed: {error}"));
            }
            query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
                .bind(&project_lock)
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("project unlock failed: {error}"));
            pool.close().await;
        });
        let backend = held
            .await
            .unwrap_or_else(|_| panic!("the competing writer never took the project lock"));
        Self {
            backend,
            take_lease: Some(take_lease),
            leased: Some(leased),
            finish,
            task,
        }
    }

    /// Acquire the writer's index lease; returns once it is live.
    async fn take_lease(&mut self) {
        if let Some(request) = self.take_lease.take() {
            let _ = request.send(());
        }
        if let Some(leased) = self.leased.take() {
            leased
                .await
                .unwrap_or_else(|_| panic!("the competing writer never took its lease"));
        }
    }

    /// Release the lease, if one was taken, then the project lock. A writer
    /// that never took its lease is told so first; otherwise its task would
    /// keep waiting for that request instead of finishing.
    async fn finish(self) {
        let Self {
            take_lease,
            finish,
            task,
            ..
        } = self;
        drop(take_lease);
        let _ = finish.send(());
        task.await
            .unwrap_or_else(|error| panic!("competing writer failed: {error}"));
    }
}

/// Run one single-row inspection query bound to `parameter`.
async fn inspect(project: &LiveProject, statement: String, parameter: &str) -> (String, i64) {
    let pool = cartograph_db::connect(&project.settings)
        .await
        .unwrap_or_else(|error| panic!("inspection connection failed: {error}"));
    let row = query(AssertSqlSafe(statement))
        .bind(parameter)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("inspection failed: {error}"));
    pool.close().await;
    (
        row.try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("inspection text decode failed: {error}")),
        row.try_get::<i64, _>(1)
            .unwrap_or_else(|error| panic!("inspection count decode failed: {error}")),
    )
}

/// The generation's state and how many file rows it retains.
async fn generation_state(project: &LiveProject, generation: &str) -> (String, i64) {
    let schema = &project.schema;
    inspect(
        project,
        format!(
            r#"SELECT generations.state,
                    (SELECT count(*) FROM "{schema}"."files" AS files
                      WHERE files.generation_id = generations.generation_id)
                FROM "{schema}"."index_generations" AS generations
                WHERE generations.generation_id = CAST($1 AS uuid)"#
        ),
        generation,
    )
    .await
}

/// The project's current generation and its failed-generation count.
async fn project_generations(project: &LiveProject, project_id: &ProjectId) -> (String, i64) {
    let schema = &project.schema;
    inspect(
        project,
        format!(
            r#"SELECT projects.current_generation_id::text,
                    (SELECT count(*) FROM "{schema}"."index_generations" AS generations
                      WHERE generations.project_id = projects.project_id
                        AND generations.state = 'failed')
                FROM "{schema}"."projects" AS projects
                WHERE projects.project_id = CAST($1 AS uuid)"#
        ),
        project_id.as_str(),
    )
    .await
}

/// The project's current generation and its next reservation sequence, which
/// grows by one with every generation ever reserved, even after retention.
async fn reservation_sequence(project: &LiveProject, project_id: &ProjectId) -> (String, i64) {
    let schema = &project.schema;
    inspect(
        project,
        format!(
            r#"SELECT projects.current_generation_id::text,
                    projects.next_generation_sequence::bigint
                FROM "{schema}"."projects" AS projects
                WHERE projects.project_id = CAST($1 AS uuid)"#
        ),
        project_id.as_str(),
    )
    .await
}

fn index_lease(project_id: &ProjectId) -> LeaseTarget {
    LeaseTarget::new(project_id.clone(), ProjectOperation::Index, None)
}

/// Another writer's own staging generation, reserved without any lock the
/// stopping index holds.
async fn reserve_foreign_generation(
    database: &CartographDatabase,
    project_id: &ProjectId,
) -> String {
    database
        .begin_generation(NewGeneration::new(
            project_id.clone(),
            "live-supervised-foreign-writer",
            1,
        ))
        .await
        .unwrap_or_else(|error| panic!("foreign staging generation failed: {error}"))
        .generation_id()
        .as_str()
        .to_owned()
}

async fn stop_mid_copy_and_assert_prompt_release(project: &LiveProject, scenario: StopScenario) {
    let (project_id, first_generation) = project.index_once().await;
    write_source(project.directory.path(), 2);
    let database = project.database().await;
    let blocker = HeldLock::fact_table(project).await;

    let path = project.path();
    let mut arguments = vec!["index", path.as_str(), "--format", "json"];
    if matches!(scenario.stop, StopRequest::CloseSupervisorStdin) {
        arguments.push("--supervised");
    }
    let mut running = RunningChild::spawn(project.command(&arguments));
    wait_until_blocked_by(project, blocker.backend).await;
    let lease = database
        .lease_status(&index_lease(&project_id))
        .await
        .unwrap_or_else(|error| panic!("lease inspection failed: {error}"))
        .unwrap_or_else(|| panic!("the building index held no project lease"));
    assert!(!lease.expired());
    assert_eq!(Some(lease.owner_pid()), running.child.id());
    let attempted = lease
        .generation_id()
        .unwrap_or_else(|| panic!("the index lease named no generation"))
        .as_str()
        .to_owned();
    let foreign = if scenario.foreign_staging {
        Some(reserve_foreign_generation(&database, &project_id).await)
    } else {
        None
    };

    request_stop(&mut running, scenario.stop);
    tokio::time::sleep(LOCK_HOLD_AFTER_STOP).await;
    assert!(
        running
            .child
            .try_wait()
            .unwrap_or_else(|error| panic!("index child poll failed: {error}"))
            .is_none(),
        "the child must still be finishing its blocked statement"
    );
    let released = Instant::now();
    blocker.release().await;

    let FinishedChild {
        succeeded, stderr, ..
    } = running.finish(PROMPT_RELEASE).await;
    assert!(released.elapsed() < PROMPT_RELEASE);
    assert!(
        !succeeded,
        "a stopped index must not report success: {stderr}"
    );
    assert_eq!(
        failure_codes(&stderr),
        (Some("request_cancelled".to_owned()), None),
        "the stop must be reported as a confirmed cooperative cancellation: {stderr}"
    );
    let remaining = database
        .lease_status(&index_lease(&project_id))
        .await
        .unwrap_or_else(|error| panic!("post-stop lease inspection failed: {error}"));
    assert!(
        remaining.is_none_or(|lease| lease.expired()),
        "the stopped index must release its project lease itself"
    );
    database.close().await;
    // The in-flight prepare rolled back at its next statement boundary
    // instead of copying the whole generation first, and the supervisor
    // marked the staging generation failed.
    assert_eq!(
        generation_state(project, &attempted).await,
        ("failed".to_owned(), 0)
    );
    assert_eq!(
        project_generations(project, &project_id).await,
        (first_generation, 1)
    );
    if let Some(foreign) = foreign {
        // The other writer's reservation was left alone and did not add a
        // cleanup failure to the confirmed cancellation.
        assert_eq!(
            generation_state(project, &foreign).await,
            ("staging".to_owned(), 0)
        );
    }
    if matches!(scenario.stop, StopRequest::CloseSupervisorStdin) {
        assert!(
            progress_lines(&stderr)
                .iter()
                .any(|progress| progress["stage"] == "copy"),
            "the supervised child reports its building stage: {stderr}"
        );
    }
}

fn request_stop(running: &mut RunningChild, stop: StopRequest) {
    match stop {
        StopRequest::CloseSupervisorStdin => drop(running.stdin.take()),
        #[cfg(unix)]
        StopRequest::Terminate => {
            let pid = running
                .child
                .id()
                .unwrap_or_else(|| panic!("index child has no pid"));
            let sent = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .unwrap_or_else(|error| panic!("kill did not start: {error}"));
            assert!(sent.success());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_supervised_index_stopped_through_stdin_releases_its_lease_promptly() {
    let project = LiveProject::new("stdin_stop");
    let outcome = AssertUnwindSafe(stop_mid_copy_and_assert_prompt_release(
        &project,
        StopScenario {
            stop: StopRequest::CloseSupervisorStdin,
            foreign_staging: false,
        },
    ))
    .catch_unwind()
    .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_direct_index_honors_sigterm_cooperatively_beside_another_writers_generation() {
    let project = LiveProject::new("sigterm_stop");
    let outcome = AssertUnwindSafe(stop_mid_copy_and_assert_prompt_release(
        &project,
        StopScenario {
            stop: StopRequest::Terminate,
            foreign_staging: true,
        },
    ))
    .catch_unwind()
    .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

/// Wait until the running supervised child reports a progress line that
/// satisfies `expected`, failing if it exits first.
async fn wait_for_progress(running: &mut RunningChild, expected: impl Fn(&Value) -> bool) {
    let started = Instant::now();
    while !progress_lines(&running.stderr_text()).iter().any(&expected) {
        assert!(
            started.elapsed() < OBSERVE_WAIT_TIMEOUT,
            "the supervised index never reported the expected progress: {}",
            running.stderr_text()
        );
        assert!(
            running
                .child
                .try_wait()
                .unwrap_or_else(|error| panic!("index child poll failed: {error}"))
                .is_none(),
            "the supervised index must keep waiting for the other writer: {}",
            running.stderr_text()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// A progress line showing the child waiting for another writer's renewals
/// before or after attempt `attempt`.
fn waiting_for_writer(progress: &Value, attempt: u64) -> bool {
    progress["phase"] == "waiting_for_writer"
        && progress["attempt"].as_u64() == Some(attempt)
        && progress["writer_heartbeats"].as_u64() >= Some(1)
}

/// The child exits 0 with a JSON report that published `live_source`.
fn published_report(finished: &FinishedChild, live_source: &str) -> Value {
    let FinishedChild {
        succeeded,
        stdout,
        stderr,
    } = finished;
    assert!(succeeded, "the supervised index must publish: {stderr}");
    let report: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("supervised report was not JSON: {error}: {stdout}"));
    assert_eq!(report["published"], true, "{report}");
    assert_eq!(report["live_source"], live_source, "{report}");
    report
}

fn report_generation(report: &Value) -> String {
    report["generation_id"]
        .as_str()
        .unwrap_or_else(|| panic!("supervised report omitted its generation: {report}"))
        .to_owned()
}

async fn wait_for_other_writer_then_publish(project: &LiveProject) {
    let (project_id, _first_generation) = project.index_once().await;
    write_source(project.directory.path(), 3);
    let database = project.database().await;
    // A non-index operation also blocks index acquisition on the project.
    let other_writer = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project_id.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(std::process::id(), "live-supervised-other-writer"),
            COMPETING_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("other writer lease failed: {error}"));

    let path = project.path();
    let mut running = RunningChild::spawn(project.command(&[
        "index",
        path.as_str(),
        "--preserve-current-excludes",
        "--supervised",
        "--format",
        "json",
    ]));
    // Release the other writer only after the child has observably started
    // waiting for it, however slowly the child starts.
    wait_for_progress(&mut running, |progress| waiting_for_writer(progress, 0)).await;
    database
        .release_lease(&other_writer)
        .await
        .unwrap_or_else(|error| panic!("other writer release failed: {error}"));

    let report = published_report(&running.finish(FINISH_TIMEOUT).await, "matched");
    // Waiting instead of colliding leaves no failed staging generation, and
    // the supervised run's own publication is current.
    assert_eq!(
        project_generations(project, &project_id).await,
        (report_generation(&report), 0)
    );
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_supervised_index_waits_for_another_project_writer_instead_of_colliding() {
    let project = LiveProject::new("writer_wait");
    let outcome = AssertUnwindSafe(wait_for_other_writer_then_publish(&project))
        .catch_unwind()
        .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

fn supervised_command(project: &LiveProject) -> Command {
    let path = project.path();
    project.command(&["index", path.as_str(), "--supervised", "--format", "json"])
}

fn supervised_index(project: &LiveProject) -> RunningChild {
    RunningChild::spawn(supervised_command(project))
}

async fn await_a_writer_that_started_after_the_writer_wait(project: &LiveProject) {
    let (project_id, _first_generation) = project.index_once().await;
    write_source(project.directory.path(), 4);
    let mut writer = CompetingWriter::start(project, &project_id).await;
    let mut running = supervised_index(project);
    // The child found no live lease, scanned, and now waits for the project
    // lock in its bounded staging cleanup: the writer's lease appears only
    // now, as it would for a writer that started during a long scan.
    wait_until_blocked_by(project, writer.backend).await;
    writer.take_lease().await;
    // The bounded cleanup times out behind the writer's lock, so the attempt
    // answers `lease_busy` before reserving anything. The child must wait for
    // that writer instead of failing.
    wait_for_progress(&mut running, |progress| waiting_for_writer(progress, 1)).await;
    writer.finish().await;

    // Attempt 1 collided, so this publication is the retry after the writer
    // finished. The collision came before any reservation: nothing failed.
    let report = published_report(&running.finish(FINISH_TIMEOUT).await, "matched");
    assert_eq!(
        project_generations(project, &project_id).await,
        (report_generation(&report), 0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_cleanup_blocked_by_a_writer_that_started_mid_scan_is_awaited() {
    let project = LiveProject::new("blocked_cleanup");
    let outcome = AssertUnwindSafe(await_a_writer_that_started_after_the_writer_wait(&project))
        .catch_unwind()
        .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

/// The project's staging generations: one identifier and how many exist.
async fn staging_generations(project: &LiveProject, project_id: &ProjectId) -> (String, i64) {
    let schema = &project.schema;
    inspect(
        project,
        format!(
            r#"SELECT COALESCE(min(generation_id::text), ''), count(*)
                FROM "{schema}"."index_generations"
                WHERE project_id = CAST($1 AS uuid) AND state = 'staging'"#
        ),
        project_id.as_str(),
    )
    .await
}

async fn cancel_while_waiting_after_a_collided_reservation(project: &LiveProject) {
    let (project_id, _first_generation) = project.index_once().await;
    write_source(project.directory.path(), 7);
    // Holding the project row pauses the child inside its reservation, after
    // its writer wait and staging preflight already passed.
    let row = HeldLock::hold(
        project,
        format!(
            r#"SELECT 1 FROM "{}"."projects" WHERE project_id = CAST('{project_id}' AS uuid) FOR NO KEY UPDATE"#,
            project.schema
        ),
    )
    .await;
    let mut command = supervised_command(project);
    command.env(
        "CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS",
        RESERVATION_HOLD_QUERY_TIMEOUT_MS,
    );
    let mut running = RunningChild::spawn(command);
    wait_until_blocked_by(project, row.backend).await;
    let mut writer = CompetingWriter::start(project, &project_id).await;
    writer.take_lease().await;
    row.release().await;
    // The child reserves its generation, finds the project leased, and its
    // cleanup of that reservation times out behind the writer's lock; it
    // then waits for the writer.
    wait_for_progress(&mut running, |progress| waiting_for_writer(progress, 1)).await;
    let (reserved, staging) = staging_generations(project, &project_id).await;
    assert_eq!(staging, 1, "the collided attempt left its reservation");
    request_stop(&mut running, StopRequest::CloseSupervisorStdin);
    let FinishedChild {
        succeeded, stderr, ..
    } = running.finish(PROMPT_RELEASE).await;
    writer.finish().await;
    assert!(
        !succeeded,
        "a stopped index must not report success: {stderr}"
    );
    // A cancellation during the wait still accounts for the earlier
    // attempt's reservation, which it could not fail behind the writer: the
    // cancellation stays primary and the unconfirmed cleanup is secondary.
    assert_eq!(
        failure_codes(&stderr),
        (
            Some("request_cancelled".to_owned()),
            Some("index_cleanup_failed".to_owned())
        ),
        "{stderr}"
    );
    assert_eq!(
        generation_state(project, &reserved).await,
        ("staging".to_owned(), 0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_cancelled_wait_still_accounts_for_an_earlier_attempts_reservation() {
    let project = LiveProject::new("cancelled_wait");
    let outcome = AssertUnwindSafe(cancel_while_waiting_after_a_collided_reservation(&project))
        .catch_unwind()
        .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

/// A stop that lands while the attempt waits behind another writer's project
/// lock, before it reserved anything.
#[cfg(unix)]
async fn stop_before_reserving_behind_another_writer(project: &LiveProject, stop: StopRequest) {
    let (project_id, first_generation) = project.index_once().await;
    let (_, sequence_before) = reservation_sequence(project, &project_id).await;
    write_source(project.directory.path(), 8);
    // The writer holds only the project lock, so the child passes its
    // live-lease check (and, supervised, its writer wait) and then blocks in
    // its bounded staging recovery.
    let writer = CompetingWriter::start(project, &project_id).await;
    let path = project.path();
    let mut arguments = vec!["index", path.as_str(), "--format", "json"];
    if matches!(stop, StopRequest::CloseSupervisorStdin) {
        arguments.push("--supervised");
    }
    let mut running = RunningChild::spawn(project.command(&arguments));
    wait_until_blocked_by(project, writer.backend).await;
    request_stop(&mut running, stop);
    // The lock wait runs to its five-second bound and the attempt answers
    // `lease_busy`. The stop was requested and nothing was reserved, so the
    // outcome is a confirmed cancellation, not contention to retry.
    let FinishedChild {
        succeeded, stderr, ..
    } = running.finish(PROMPT_RELEASE).await;
    writer.finish().await;
    assert!(
        !succeeded,
        "a stopped index must not report success: {stderr}"
    );
    assert_eq!(
        failure_codes(&stderr),
        (Some("request_cancelled".to_owned()), None),
        "{stderr}"
    );
    assert_eq!(
        reservation_sequence(project, &project_id).await,
        (first_generation, sequence_before)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_stop_that_meets_another_writer_before_reserving_is_a_confirmed_cancellation() {
    for (label, stop) in [
        ("lock_stop_sigterm", StopRequest::Terminate),
        ("lock_stop_stdin", StopRequest::CloseSupervisorStdin),
    ] {
        let project = LiveProject::new(label);
        let outcome = AssertUnwindSafe(stop_before_reserving_behind_another_writer(&project, stop))
            .catch_unwind()
            .await;
        project.drop_schema().await;
        if let Err(payload) = outcome {
            resume_unwind(payload);
        }
    }
}

async fn publish_once_while_the_checkout_keeps_changing(project: &LiveProject) {
    let (project_id, first_generation) = project.index_once().await;
    let (_, sequence_before) = reservation_sequence(project, &project_id).await;
    write_source(project.directory.path(), 5);
    let blocker = HeldLock::fact_table(project).await;
    let running = supervised_index(project);
    wait_until_blocked_by(project, blocker.backend).await;
    // Another session edits the checkout after the child scanned the
    // revision it is now building.
    write_source(project.directory.path(), 6);
    blocker.release().await;

    let report = published_report(
        &running.finish(FINISH_TIMEOUT).await,
        "changed_after_publication",
    );
    let generation = report_generation(&report);
    assert_ne!(generation, first_generation);
    // Exactly one new generation was reserved and it stays current: the
    // change after publication was reported instead of rebuilt.
    assert_eq!(
        reservation_sequence(project, &project_id).await,
        (generation, sequence_before + 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn a_supervised_index_publishes_once_when_the_checkout_changes_after_its_scan() {
    let project = LiveProject::new("single_publication");
    let outcome = AssertUnwindSafe(publish_once_while_the_checkout_keeps_changing(&project))
        .catch_unwind()
        .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

async fn install_stops_after_a_failed_initial_index(project: &LiveProject) {
    // A malformed project configuration fails the index after it connected,
    // where a JSON-format index reports its failure itself and exits nonzero.
    let configuration = project.directory.path().join(".cartograph");
    std::fs::create_dir_all(&configuration)
        .unwrap_or_else(|error| panic!("configuration directory failed: {error}"));
    std::fs::write(
        configuration.join("config.json"),
        r#"{"maxFileSize": "unbounded"}"#,
    )
    .unwrap_or_else(|error| panic!("configuration write failed: {error}"));
    let path = project.path();
    let output = project
        .command(&[
            "install",
            "--yes",
            "--target",
            "none",
            "--location",
            "local",
            "--project-path",
            path.as_str(),
            "--no-permissions",
            "--format",
            "json",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap_or_else(|error| panic!("install did not start: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "install must fail when its initial index failed: {stderr}"
    );
    assert!(
        stderr.contains("\"project_configuration_invalid\""),
        "the index failure is reported first: {stderr}"
    );
    assert!(
        stderr.contains("install stopped before Git hooks"),
        "install says where it stopped: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn install_fails_when_its_json_initial_index_fails() {
    let project = LiveProject::new("install_index_failure");
    let outcome = AssertUnwindSafe(install_stops_after_a_failed_initial_index(&project))
        .catch_unwind()
        .await;
    project.drop_schema().await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}
