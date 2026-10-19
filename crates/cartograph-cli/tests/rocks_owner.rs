//! Real CLI processes share one native owner while source edits are published.

mod dependency_ownership;

#[cfg(unix)]
#[path = "rocks_owner"]
mod unix {
    #[path = "admin_status.rs"]
    mod admin_status;
    #[path = "freshness.rs"]
    mod freshness;
    #[path = "mixed_soak.rs"]
    mod mixed_soak;
    #[path = "persistent.rs"]
    mod persistent;
    #[path = "read_only.rs"]
    mod read_only;

    use serde_json::Value;
    use std::{error::Error, path::Path, process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::process::{Child, ChildStdin, Command};

    fn command(root: &Path, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cartograph"));
        command
            .args(arguments)
            .current_dir(root)
            .env(
                "CARTOGRAPH_DATABASE_URL",
                "invalid-postgresql-must-not-be-used",
            )
            .env(
                "DOCKER_HOST",
                "unix:///cartograph-native-owner-no-docker.sock",
            )
            .kill_on_drop(true);
        command
    }

    async fn cli(root: &Path, arguments: &[&str]) -> Result<Value, Box<dyn Error>> {
        let output =
            tokio::time::timeout(Duration::from_secs(30), command(root, arguments).output())
                .await??;
        assert!(
            output.status.success(),
            "CLI {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    async fn owner(root: &Path, arguments: &[&str]) -> Result<(Child, ChildStdin), Box<dyn Error>> {
        let mut child = command(root, arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or("owner stdin missing")?;
        let stdout = child.stdout.take().ok_or("owner stdout missing")?;
        stdin.write_all(format!("{}\n", serde_json::json!({"jsonrpc":"2.0","id":"ready","method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":cartograph_mcp::MODERN_PROTOCOL_VERSION,"io.modelcontextprotocol/clientInfo":{"name":"owner-test","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}}})).as_bytes()).await?;
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(20),
            BufReader::new(stdout).read_line(&mut line),
        )
        .await??;
        let ready: Value = serde_json::from_str(&line)?;
        assert_eq!(ready["id"], "ready");
        assert!(ready["result"].is_object(), "owner ping failed: {ready}");
        Ok((child, stdin))
    }

    async fn verify_owned_feature_routes(root: &Path) -> Result<(), Box<dyn Error>> {
        let context = cli(root, &["context", "replacement", "--format", "json"]).await?;
        assert_eq!(context["freshness"], "current");
        let export = cli(root, &["export", "--format", "json"]).await?;
        assert!(
            export["nodes"]
                .as_array()
                .is_some_and(|nodes| !nodes.is_empty())
        );
        let embeddings = cli(root, &["embedding-status", "--format", "json"]).await?;
        assert_eq!(embeddings["state"], "not_configured");
        assert_eq!(embeddings["reason"], "embedding_configuration_unavailable");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_routes_to_mcp_owner_and_watcher_keeps_its_graph_current()
    -> Result<(), Box<dyn Error>> {
        let project = tempfile::tempdir()?;
        let root = project.path();
        std::fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
        let (mut child, stdin) = owner(root, &["serve", "--mcp"]).await?;
        let status = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = cli(root, &["status", "--json"]).await?;
                if status["project"]["fresh"] == true {
                    return Ok::<_, Box<dyn Error>>(status);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        assert_eq!(status["autoSync"]["active"], true);
        let initial = status["project"]["snapshot"]["current"]["generation_id"].clone();
        assert_eq!(
            cli(root, &["index", "--format", "json"]).await?["published"],
            false
        );
        let completed = cli(root, &["admin", "status", "--json"]).await?;
        assert_eq!(completed["report"]["enrichment"]["reason"], "base_only");
        std::fs::write(root.join("lib.rs"), "pub fn replacement() {}\n")?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = cli(root, &["status", "--json"]).await?;
                if status["project"]["fresh"] == true
                    && status["project"]["snapshot"]["current"]["generation_id"] != initial
                {
                    return Ok::<_, Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        let original = cli(
            root,
            &["find", "original", "--by", "name", "--format", "json"],
        )
        .await?;
        assert_eq!(
            original["evidence"]["rows"]
                .as_array()
                .ok_or("results missing")?
                .len(),
            0
        );
        let replacement = cli(
            root,
            &["find", "replacement", "--by", "name", "--format", "json"],
        )
        .await?;
        assert_eq!(
            replacement["evidence"]["rows"]
                .as_array()
                .ok_or("results missing")?
                .len(),
            1
        );
        assert_eq!(
            cli(root, &["doctor", "--json"]).await?["capabilitiesReady"],
            true
        );
        verify_owned_feature_routes(root).await?;
        std::fs::write(root.join("lib.rs"), "pub fn synchronized() {}\n")?;
        let sync = tokio::time::timeout(
            Duration::from_secs(30),
            command(root, &["sync-if-dirty", "--quiet"]).output(),
        )
        .await??;
        assert!(
            sync.status.success(),
            "{}",
            String::from_utf8_lossy(&sync.stderr)
        );
        assert_eq!(
            cli(root, &["status", "--json"]).await?["project"]["fresh"],
            true
        );
        let completed = cli(root, &["admin", "status", "--json"]).await?;
        assert_eq!(completed["report"]["enrichment"]["reason"], "base_only");
        drop(stdin);
        assert!(
            tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await??
                .success()
        );
        assert!(!root.join(".cartograph/owner.sock").exists());
        assert_eq!(
            cli(root, &["status", "--json"]).await?["project"]["fresh"],
            true
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn long_project_path_uses_private_short_owner_endpoint_and_reopens_cleanly()
    -> Result<(), Box<dyn Error>> {
        let project = tempfile::tempdir()?;
        let root = project
            .path()
            .join("long-project-parent-directory-".repeat(3))
            .join("long-project-directory-".repeat(3));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("lib.rs"), "pub fn long_path_target() {}\n")?;
        let (mut child, stdin) = owner(&root, &["serve", "--mcp", "--no-auto-sync"]).await?;
        assert!(!root.join(".cartograph/owner.sock").exists());
        assert_eq!(
            cli(&root, &["index", "--format", "json"]).await?["live_source"],
            "matched"
        );
        assert_eq!(
            cli(&root, &["status", "--json"]).await?["project"]["fresh"],
            true
        );
        drop(stdin);
        assert!(
            tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await??
                .success()
        );
        assert_eq!(
            cli(&root, &["index", "--format", "json"]).await?["published"],
            false
        );
        Ok(())
    }
}
