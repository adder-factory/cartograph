//! Process faults exercise native task cancellation and WAL recovery.
//! Each process and directory in this target belongs to the test.

mod dependency_ownership;

use serde_json::Value;
use std::{error::Error, fmt::Write as _, fs, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader},
    process::{Child, Command},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const FINISH: Duration = Duration::from_secs(120);
const LOG_BYTES: u64 = 2 * 1024 * 1024;
// Each scenario owns a 32,768-function recovery build. Admit one fixture at a
// time while preserving its internal workers and the separate concurrency gates.
static RECOVERY_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn command(root: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cartograph"));
    command
        .args(arguments)
        .current_dir(root)
        .env("CARTOGRAPH_DATABASE_URL", "invalid-retired-database")
        .env("DOCKER_HOST", "unix:///no-native-fault-docker.sock")
        .kill_on_drop(true);
    command
}

async fn json(root: &Path, arguments: &[&str]) -> TestResult<Value> {
    let output = tokio::time::timeout(FINISH, command(root, arguments).output())
        .await
        .map_err(|error| format!("native command {arguments:?} exceeded {FINISH:?}: {error}"))??;
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn grow_source(root: &Path) -> TestResult {
    fs::create_dir(root.join("work"))?;
    for file in 0..512 {
        let mut source = String::new();
        for symbol in 0..64 {
            writeln!(
                &mut source,
                "pub fn f_{file}_{symbol}(input:u64)->u64{{input.wrapping_add({symbol})}}"
            )?;
        }
        fs::write(root.join("work").join(format!("file_{file}.rs")), source)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Stop {
    Eof,
    Kill,
    #[cfg(unix)]
    Term,
}

async fn finish(child: &mut Child) -> TestResult<std::process::ExitStatus> {
    match tokio::time::timeout(FINISH, child.wait()).await {
        Ok(status) => Ok(status?),
        Err(error) => {
            child.kill().await?;
            let _ = child.wait().await?;
            Err(format!("supervised child exit exceeded {FINISH:?}: {error}").into())
        }
    }
}

async fn stop_during_extraction(stop: Stop) -> TestResult {
    let _fixture = RECOVERY_FIXTURE.lock().await;
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
    let first = json(root, &["index", "--format", "json"]).await?;
    let previous = first["generation_id"].clone();
    assert!(previous.is_string());
    grow_source(root)?;
    let mut child = command(
        root,
        &[
            "index",
            "--workers",
            "1",
            "--supervised",
            "--format",
            "json",
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
    let stdin = child.stdin.take().ok_or("supervised stdin missing")?;
    let stdout = child.stdout.take().ok_or("stdout missing")?;
    let mut stderr = BufReader::new(child.stderr.take().ok_or("stderr missing")?).lines();
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = stderr.next_line().await? {
            if observed.len() >= 4096 {
                return Err("progress log limit".into());
            }
            let value = serde_json::from_str::<Value>(&line).ok();
            observed.push(line);
            if value.is_some_and(|value| {
                value.pointer("/progress/stage").and_then(Value::as_str) == Some("parse")
            }) {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
        Err("index exited before entering extraction".into())
    })
    .await??;
    match stop {
        Stop::Eof => {}
        Stop::Kill => child.start_kill()?,
        #[cfg(unix)]
        Stop::Term => {
            let id = child.id().ok_or("child exited before signal")?.to_string();
            assert!(
                Command::new("kill")
                    .args(["-TERM", &id])
                    .status()
                    .await?
                    .success()
            );
        }
    }
    drop(stdin);
    let drain = async {
        let mut stdout = stdout.take(LOG_BYTES);
        let mut bytes = Vec::new();
        let read_stdout = stdout.read_to_end(&mut bytes);
        let read_stderr = async {
            while let Some(line) = stderr.next_line().await? {
                if observed.len() >= 4096 {
                    return Err(std::io::Error::other("progress log limit"));
                }
                observed.push(line);
            }
            Ok::<(), std::io::Error>(())
        };
        let (status, out, err) = tokio::join!(finish(&mut child), read_stdout, read_stderr);
        out?;
        err?;
        Ok::<_, Box<dyn Error>>((status?, bytes))
    };
    let (status, stdout) = drain.await?;
    assert!(!status.success());
    assert!(
        stdout.is_empty(),
        "cancelled build emitted a success report"
    );
    if !matches!(stop, Stop::Kill) {
        let failure_text = observed
            .iter()
            .filter(|line| !line.starts_with("{\"progress\":"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let failure: Value = serde_json::from_str(&failure_text).map_err(|error| {
            format!("cooperative child omitted failure report: {error}: {failure_text}")
        })?;
        assert_eq!(failure["error"]["code"], "request_cancelled", "{failure}");
        assert!(failure["error"]["cleanup_failure"].is_null(), "{failure}");
    }
    verify_recovery(root, &previous).await
}

async fn verify_recovery(root: &Path, previous: &Value) -> TestResult {
    let status = json(root, &["status", "--json"]).await?;
    assert_eq!(
        status["project"]["snapshot"]["current"]["generation_id"],
        *previous
    );
    assert_eq!(status["project"]["fresh"], false);
    let recovered = json(root, &["index", "--workers", "4", "--format", "json"]).await?;
    assert!(recovered["published"].as_bool().unwrap_or(false));
    let status = json(root, &["status", "--json"]).await?;
    assert_eq!(status["project"]["fresh"], true);
    assert_ne!(
        status["project"]["snapshot"]["current"]["generation_id"],
        *previous
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_eof_cancels_owned_workers_preserves_current_and_reopens() -> TestResult {
    stop_during_extraction(Stop::Eof).await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_cancels_owned_workers_preserves_current_and_reopens() -> TestResult {
    stop_during_extraction(Stop::Term).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_native_index_recovers_wal_and_preserves_committed_current() -> TestResult {
    stop_during_extraction(Stop::Kill).await
}
