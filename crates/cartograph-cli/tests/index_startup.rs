//! `cartograph index` before its cancellable index request starts.
//!
//! A local listener accepts the child's database connection and never
//! answers, which holds the real binary inside its startup (database
//! connection and schema migration) for as long as the test needs.

mod dependency_ownership;

use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::AsyncReadExt as _,
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    task::JoinHandle,
};

/// Driver bound on the stalled connection; far longer than any assertion
/// window, so only the behavior under test can end the child earlier.
const STALLED_ACQUIRE_TIMEOUT_MS: &str = "60000";
/// Bound for the child to reach its database connection.
const REACH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for an interrupted starting child to end; far below the stalled
/// connection's own bound.
#[cfg(unix)]
const INTERRUPTED_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the test observes a supervised child stalled in its startup:
/// several of the child's 2-second progress intervals.
const STARTUP_OBSERVATION: Duration = Duration::from_secs(7);
/// Distinct progress lines a parent must see in that window to know the
/// child is alive; each one restarts its inactivity bound.
const MINIMUM_STARTUP_LINES: usize = 3;
/// Database settings a developer environment may set that would change how
/// (or whether) the child reaches its connection.
const INHERITED_DATABASE_SETTINGS: [&str; 4] = [
    "CARTOGRAPH_DATABASE_MAX_CONNECTIONS",
    "CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS",
    "CARTOGRAPH_DATABASE_REQUIRE_SSL",
    "CARTOGRAPH_DATABASE_SCHEMA",
];
/// Shell script that ignores `SIGINT` and then becomes the program in `$0`.
const IGNORE_SIGINT_THEN_EXEC: &str = r#"trap '' INT; exec "$0" "$@""#;

/// A database endpoint that accepts connections and never answers.
struct SilentDatabase {
    listener: TcpListener,
    url: String,
}

impl SilentDatabase {
    /// Listen on an ephemeral loopback port.
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("silent database did not bind: {error}"));
        let port = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("silent database has no address: {error}"))
            .port();
        Self {
            listener,
            url: format!("postgresql://cartograph:unused@127.0.0.1:{port}/cartograph"),
        }
    }

    /// Wait for the child's connection and keep it open, unanswered.
    async fn accept(&self) -> TcpStream {
        let accepted = tokio::time::timeout(REACH_CONNECT_TIMEOUT, self.listener.accept())
            .await
            .unwrap_or_else(|_| panic!("the index never connected to its database"));
        let (stream, _) = accepted
            .unwrap_or_else(|error| panic!("accepting the index connection failed: {error}"));
        stream
    }
}

/// How a test starts its `cartograph index` child.
struct Launch<'arguments> {
    /// Arguments after `index <project>`.
    extra: &'arguments [&'arguments str],
    /// Start it as a non-interactive shell starts a background job: with
    /// `SIGINT` ignored, a disposition the child inherits.
    sigint_ignored: bool,
}

/// A real `cartograph index` child pointed at `database`, with every other
/// database setting at its default.
fn index_command(
    database: &SilentDatabase,
    project: &tempfile::TempDir,
    launch: &Launch<'_>,
) -> Command {
    let binary = env!("CARGO_BIN_EXE_cartograph");
    let mut command = if launch.sigint_ignored {
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", IGNORE_SIGINT_THEN_EXEC, binary]);
        shell
    } else {
        Command::new(binary)
    };
    command
        .arg("index")
        .arg(project.path())
        .args(launch.extra)
        .current_dir(project.path())
        .env("CARTOGRAPH_DATABASE_URL", &database.url)
        .env(
            "CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS",
            STALLED_ACQUIRE_TIMEOUT_MS,
        );
    for setting in INHERITED_DATABASE_SETTINGS {
        command.env_remove(setting);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// An empty checkout; the index never gets past its database startup.
fn project() -> tempfile::TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("project tempdir failed: {error}"))
}

/// Collect the child's stderr as it arrives.
fn collect_stderr(child: &mut Child) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut pipe = child
        .stderr
        .take()
        .unwrap_or_else(|| panic!("index child has no stderr"));
    let reader = tokio::spawn(async move {
        let mut buffer = [0_u8; 4_096];
        while let Ok(read) = pipe.read(&mut buffer).await {
            if read == 0 {
                return;
            }
            if let Ok(mut seen) = sink.lock() {
                seen.extend_from_slice(&buffer[..read]);
            }
        }
    });
    (seen, reader)
}

/// Everything collected from stderr so far.
fn text(seen: &Mutex<Vec<u8>>) -> String {
    seen.lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervised_index_reports_liveness_while_its_startup_is_slow() {
    let database = SilentDatabase::bind().await;
    let project = project();
    let launch = Launch {
        extra: &["--supervised", "--format", "json"],
        sigint_ignored: false,
    };
    let mut child = index_command(&database, &project, &launch)
        .spawn()
        .unwrap_or_else(|error| panic!("index child did not start: {error}"));
    let (seen, _reader) = collect_stderr(&mut child);
    let _connection = database.accept().await;
    tokio::time::sleep(STARTUP_OBSERVATION).await;

    let stderr = text(&seen);
    let mut lines = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| line.get("progress").cloned())
        .collect::<Vec<_>>();
    assert!(
        lines.iter().all(|progress| progress["phase"] == "starting"),
        "only startup progress can precede the connection: {stderr}"
    );
    lines.dedup();
    let parent_sees_liveness = lines.len() >= MINIMUM_STARTUP_LINES;
    assert!(
        parent_sees_liveness,
        "the supervising parent must see changing progress while the child connects and migrates: {stderr}"
    );
    assert!(
        child
            .try_wait()
            .unwrap_or_else(|error| panic!("index child poll failed: {error}"))
            .is_none(),
        "the child must still be starting: {stderr}"
    );
}

/// Interrupt a child that is still connecting and return how it ended.
#[cfg(unix)]
async fn interrupt_while_starting(launch: &Launch<'_>) -> (std::process::ExitStatus, String) {
    let database = SilentDatabase::bind().await;
    let project = project();
    let mut child = index_command(&database, &project, launch)
        .spawn()
        .unwrap_or_else(|error| panic!("index child did not start: {error}"));
    let (seen, reader) = collect_stderr(&mut child);
    let _connection = database.accept().await;
    let pid = child
        .id()
        .unwrap_or_else(|| panic!("index child has no pid"));
    let sent = std::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .unwrap_or_else(|error| panic!("kill did not start: {error}"));
    assert!(sent.success());
    let status = tokio::time::timeout(INTERRUPTED_EXIT_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("the interrupt did not end the starting index"))
        .unwrap_or_else(|error| panic!("index child wait failed: {error}"));
    let _ = reader.await;
    (status, text(&seen))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_interrupt_before_the_index_request_ends_the_process_by_that_signal() {
    use std::os::unix::process::ExitStatusExt as _;

    // Nothing cancellable exists yet, so the interrupt ends the process as
    // the default disposition does, even when the child inherited SIGINT as
    // ignored, and no cooperative stop is announced that nothing would act on.
    for sigint_ignored in [false, true] {
        let (status, stderr) = interrupt_while_starting(&Launch {
            extra: &[],
            sigint_ignored,
        })
        .await;
        assert_eq!(
            status.signal(),
            Some(signal_hook::consts::SIGINT),
            "inherited SIGINT ignored: {sigint_ignored}; {status:?}: {stderr}"
        );
        assert!(!stderr.contains("cooperatively"), "{stderr}");
    }
}
