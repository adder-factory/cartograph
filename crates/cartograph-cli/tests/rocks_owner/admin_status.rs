//! A separate CLI status observer must return while the retained job keeps running.

use super::*;
use cartograph_mcp::ToolResult;
use std::{os::unix::fs::PermissionsExt as _, sync::Arc};
use tokio::{io::AsyncReadExt as _, net::UnixListener, sync::Mutex};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_status_observes_running_job_once_without_waiting_or_cancelling()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir_in("/tmp")?;
    let root = project.path().canonicalize()?;
    let state = root.join(".cartograph");
    std::fs::create_dir(&state)?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    let socket = state.join("owner.sock");
    assert!(socket.as_os_str().len() < 100);
    let listener = UnixListener::bind(socket)?;
    let actions = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed = actions.clone();
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        loop {
            let (mut stream, _) = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted?,
            };
            // Owner discovery connects without sending a request.
            let Ok(length) = stream.read_u32().await else {
                continue;
            };
            assert!(length <= 64 * 1024);
            let mut bytes = vec![0; usize::try_from(length)?];
            stream.read_exact(&mut bytes).await?;
            let request: Value = serde_json::from_slice(&bytes)?;
            let action = request["arguments"]["action"]
                .as_str()
                .ok_or("action missing")?;
            observed.lock().await.push(action.to_owned());
            let status = if action == "cancel" {
                "cancelled"
            } else {
                "running"
            };
            let report = serde_json::json!({"action":"index", "jobId":7, "status":status});
            let result = ToolResult::text(report.to_string())
                .with_structured_content(report.as_object().cloned().ok_or("report missing")?);
            let reply = serde_json::to_vec(&serde_json::json!({
                "status":"ok", "owner_instance":"retained-job-fixture", "result":result,
            }))?;
            stream.write_u32(u32::try_from(reply.len())?).await?;
            stream.write_all(&reply).await?;
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    });
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        command(&root, &["admin", "status", "--job-id", "7", "--json"]).output(),
    )
    .await;
    let _stopped = stop.send(());
    serving
        .await?
        .map_err(|error| -> Box<dyn Error> { error })?;
    let output = output??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["status"], "running");
    assert_eq!(report["jobId"], 7);
    assert_eq!(*actions.lock().await, ["status"]);
    Ok(())
}
