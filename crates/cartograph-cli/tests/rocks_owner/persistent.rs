use super::*;

async fn wait_fresh(root: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = cli(root, &["status", "--json"]).await?;
            if status["project"]["fresh"] == true {
                let found = cli(root, &["find", name, "--format", "json", "--allow-stale"]).await?;
                if found["evidence"]["rows"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["name"] == name))
                {
                    return Ok::<_, Box<dyn Error>>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_owner_survives_mcp_eof_and_termination_drains_storage()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    std::fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
    let mut child = command(root, &["watch"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        BufReader::new(child.stdout.take().ok_or("watch output missing")?).read_line(&mut ready),
    )
    .await??;
    assert_eq!(serde_json::from_str::<Value>(&ready)?["owner"], "ready");
    wait_fresh(root, "original").await?;
    assert_eq!(
        cli(root, &["status", "--json"]).await?["autoSync"]["active"],
        true
    );
    let (mut proxy, input) = owner(root, &["serve", "--mcp"]).await?;
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(20), proxy.wait())
            .await??
            .success()
    );
    assert!(
        child.try_wait()?.is_none(),
        "MCP disconnect stopped the headless owner"
    );
    std::fs::write(root.join("lib.rs"), "pub fn replacement() {}\n")?;
    wait_fresh(root, "replacement").await?;
    let pid = child.id().ok_or("headless process exited")?.to_string();
    assert!(
        tokio::process::Command::new("/bin/kill")
            .args(["-TERM", &pid])
            .status()
            .await?
            .success()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await??
            .success()
    );
    assert_eq!(
        cli(root, &["status", "--json"]).await?["project"]["fresh"],
        true
    );
    Ok(())
}
