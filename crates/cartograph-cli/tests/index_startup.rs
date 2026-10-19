//! Process-level native startup supervision. A real primary holds the project
//! lock, so the child waits before it has a cancellable index request.

mod dependency_ownership;

use cartograph_agent::RocksProjectRuntime;
use cartograph_db::rocks::StorageRequest;
use serde_json::Value;
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt as _,
    process::{Child, Command},
    task::JoinHandle,
};

const REACH_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPTED_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_OBSERVATION: Duration = Duration::from_secs(7);
const MINIMUM_STARTUP_LINES: usize = 3;
const IGNORE_SIGINT_THEN_EXEC: &str = r#"trap '' INT; exec "$0" "$@""#;

fn project() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("lib.rs"), "pub fn indexed() {}\n")?;
    Ok(root)
}

fn index_command(root: &std::path::Path, supervised: bool, sigint_ignored: bool) -> Command {
    let binary = env!("CARGO_BIN_EXE_cartograph");
    let mut command = if sigint_ignored {
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", IGNORE_SIGINT_THEN_EXEC, binary]);
        shell
    } else {
        Command::new(binary)
    };
    command.arg("index").arg(root).args(["--format", "json"]);
    if supervised {
        command.arg("--supervised");
    }
    command
        .current_dir(root)
        .env(
            "CARTOGRAPH_DATABASE_URL",
            "invalid-postgresql-must-not-be-used",
        )
        .env(
            "DOCKER_HOST",
            "unix:///cartograph-native-startup-no-docker.sock",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

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

fn text(seen: &Mutex<Vec<u8>>) -> String {
    seen.lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

async fn reached_startup(seen: &Mutex<Vec<u8>>, supervised: bool) {
    tokio::time::timeout(REACH_STARTUP_TIMEOUT, async {
        loop {
            let observed = text(seen);
            if (supervised && observed.contains("starting"))
                || (!supervised && observed.contains("native project owner is active"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("index never reached native startup: {}", text(seen)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervised_index_reports_liveness_while_its_native_owner_is_busy()
-> Result<(), Box<dyn std::error::Error>> {
    let project = project()?;
    let owner = RocksProjectRuntime::open(
        project.path(),
        &StorageRequest::new(Duration::from_secs(30))?,
    )
    .await?;
    let mut child = index_command(project.path(), true, false).spawn()?;
    let (seen, reader) = collect_stderr(&mut child);
    reached_startup(&seen, true).await;
    tokio::time::sleep(STARTUP_OBSERVATION).await;
    let stderr = text(&seen);
    let mut lines = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| line.get("progress").cloned())
        .collect::<Vec<_>>();
    assert!(
        lines.iter().all(|line| line["phase"] == "starting"),
        "{stderr}"
    );
    lines.dedup();
    assert!(lines.len() >= MINIMUM_STARTUP_LINES, "{stderr}");
    assert!(child.try_wait()?.is_none(), "{stderr}");
    // Releasing the real primary lets this same child finish. No PostgreSQL
    // service or mocked native storage participates in either transition.
    owner.shutdown().await?;
    // Child::wait closes an attached stdin. Keep the parent's cancellation
    // channel open while waiting for a successful supervised request.
    let parent_pipe = child.stdin.take();
    let status = tokio::time::timeout(INTERRUPTED_EXIT_TIMEOUT, child.wait()).await??;
    drop(parent_pipe);
    reader.await?;
    assert!(status.success(), "{}", text(&seen));
    let reopened = RocksProjectRuntime::open(
        project.path(),
        &StorageRequest::new(Duration::from_secs(30))?,
    )
    .await?;
    assert!(
        reopened
            .status(&StorageRequest::new(Duration::from_secs(30))?)
            .await?
            .fresh
    );
    reopened.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_interrupt_during_native_startup_ends_the_process_by_that_signal()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::ExitStatusExt as _;
    let project = project()?;
    let owner = RocksProjectRuntime::open(
        project.path(),
        &StorageRequest::new(Duration::from_secs(30))?,
    )
    .await?;
    for supervised in [false, true] {
        for sigint_ignored in [false, true] {
            let mut child = index_command(project.path(), supervised, sigint_ignored).spawn()?;
            let (seen, reader) = collect_stderr(&mut child);
            reached_startup(&seen, supervised).await;
            let pid = child.id().ok_or("index child has no pid")?;
            assert!(
                std::process::Command::new("kill")
                    .args(["-INT", &pid.to_string()])
                    .status()?
                    .success()
            );
            let status = tokio::time::timeout(INTERRUPTED_EXIT_TIMEOUT, child.wait()).await??;
            reader.await?;
            let stderr = text(&seen);
            assert_eq!(
                status.signal(),
                Some(signal_hook::consts::SIGINT),
                "{status:?}: {stderr}"
            );
            assert!(!stderr.contains("cooperatively"), "{stderr}");
        }
    }
    owner.shutdown().await?;
    Ok(())
}
