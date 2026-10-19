//! A secondary MCP host must retain its own branch authorization ceiling.

use super::*;
use serde_json::json;
use tokio::process::ChildStdout;

pub(super) struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpProcess {
    pub(super) async fn open(root: &Path, arguments: &[&str]) -> Result<Self, Box<dyn Error>> {
        let mut child = command(root, arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("MCP stdin missing")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("MCP stdout missing")?);
        let mut process = Self {
            child,
            stdin,
            stdout,
        };
        let ready = process.request("ping", json!({})).await?;
        assert!(ready["result"].is_object(), "MCP ping failed: {ready}");
        Ok(process)
    }

    async fn request(&mut self, method: &str, mut params: Value) -> Result<Value, Box<dyn Error>> {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": cartograph_mcp::MODERN_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientInfo": {"name":"owner-read-only-test","version":"1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let request = json!({"jsonrpc":"2.0","id":"call","method":method,"params":params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(20), self.stdout.read_line(&mut line)).await??;
        let response: Value = serde_json::from_str(&line)?;
        assert_eq!(response["id"], "call");
        Ok(response)
    }

    pub(super) async fn call(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, Box<dyn Error>> {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }

    pub(super) async fn close(mut self) -> Result<(), Box<dyn Error>> {
        self.stdin.shutdown().await?;
        drop(self.stdin);
        assert!(
            tokio::time::timeout(Duration::from_secs(20), self.child.wait())
                .await??
                .success()
        );
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_native_owner_cannot_expand_a_read_only_proxy_branch_ceiling()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    std::fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
    let mut full = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    cli(root, &["index", "--format", "json"]).await?;
    let added = full
        .call(
            "cartograph_note",
            json!({"action":"add","text":"authorized durable note"}),
        )
        .await?;
    assert!(
        added["error"].is_null() && added["result"]["isError"] != true,
        "authorized owner write failed: {added}"
    );
    let note_id = added["result"]["structuredContent"]["artifact"]["id"]
        .as_u64()
        .ok_or("note id missing")?;
    let initial =
        cli(root, &["status", "--json"]).await?["project"]["snapshot"]["current"]["generation_id"]
            .clone();
    std::fs::write(root.join("lib.rs"), "pub fn changed_source() {}\n")?;

    for arguments in [
        vec!["serve", "--mcp", "--profile", "read-only", "--no-auto-sync"],
        vec![
            "serve",
            "--mcp",
            "--profile",
            "full",
            "--no-write-tools",
            "--no-auto-sync",
        ],
    ] {
        let admin_included = arguments.contains(&"--no-write-tools");
        let mut proxy = McpProcess::open(root, &arguments).await?;
        let listed = proxy
            .call("cartograph_note", json!({"action":"list"}))
            .await?;
        assert!(
            listed["error"].is_null() && listed["result"]["isError"] != true,
            "read branch was rejected: {listed}"
        );
        assert_eq!(
            listed["result"]["structuredContent"]["result"][0]["id"],
            note_id
        );
        for (name, arguments) in [
            (
                "cartograph_note",
                json!({"action":"add","text":"forbidden proxy note"}),
            ),
            ("cartograph_note", json!({"action":"delete","id":note_id})),
            ("cartograph_admin", json!({"action":"index"})),
        ] {
            let denied = proxy.call(name, arguments).await?;
            assert!(
                denied["error"].is_object() || denied["result"]["isError"] == true,
                "proxy mutation was accepted: {denied}"
            );
            assert!(
                if name == "cartograph_admin" && !admin_included {
                    denied["error"]["data"]["code"] == "unknown_tool"
                } else {
                    denied.to_string().contains("read-only write gate")
                },
                "mutation did not reach the branch gate: {denied}"
            );
        }
        proxy.close().await?;
    }
    let listed = full
        .call("cartograph_note", json!({"action":"list"}))
        .await?;
    let notes = listed["result"]["structuredContent"]["result"]
        .as_array()
        .ok_or("notes missing")?;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["id"], note_id);
    assert_eq!(notes[0]["body"], "authorized durable note");
    full.close().await?;
    let reopened = cli(root, &["status", "--json"]).await?;
    assert_eq!(reopened["project"]["fresh"], false);
    assert_eq!(
        reopened["project"]["snapshot"]["current"]["generation_id"],
        initial
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_macro_cannot_reenable_a_tool_disabled_after_owner_restart()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    std::fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
    let mut full = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    cli(root, &["index", "--format", "json"]).await?;
    let saved = full.call("cartograph_session", json!({"action":"macro_save","name":"refresh-index","steps":[{"tool":"cartograph_admin","args":{"action":"index"}}]})).await?;
    assert!(
        saved["error"].is_null() && saved["result"]["isError"] != true,
        "macro save failed: {saved}"
    );
    let initial =
        cli(root, &["status", "--json"]).await?["project"]["snapshot"]["current"]["generation_id"]
            .clone();
    full.close().await?;
    std::fs::write(root.join("lib.rs"), "pub fn changed_source() {}\n")?;
    let mut restricted = McpProcess::open(
        root,
        &[
            "serve",
            "--mcp",
            "--profile",
            "full",
            "--no-auto-sync",
            "--disable-tool",
            "cartograph_admin",
        ],
    )
    .await?;
    let replayed = restricted
        .call(
            "cartograph_session",
            json!({"action":"macro_run","name":"refresh-index"}),
        )
        .await?;
    assert!(
        replayed["error"].is_null() && replayed["result"]["isError"] != true,
        "macro run failed outside its step: {replayed}"
    );
    let report = &replayed["result"]["structuredContent"];
    assert_eq!(
        report["successfulSteps"], 0,
        "disabled tool ran: {replayed}"
    );
    assert_eq!(report["failedSteps"], 1);
    assert!(
        report["steps"][0]["output"]
            .as_str()
            .is_some_and(|text| text.contains("authorization ceiling"))
    );
    restricted.close().await?;
    let reopened = cli(root, &["status", "--json"]).await?;
    assert_eq!(reopened["project"]["fresh"], false);
    assert_eq!(
        reopened["project"]["snapshot"]["current"]["generation_id"],
        initial
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_proxy_cannot_run_a_disabled_tool_through_a_full_owners_macro()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    std::fs::write(root.join("lib.rs"), "pub fn original() {}\n")?;
    let mut full = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    cli(root, &["index", "--format", "json"]).await?;
    for (name, tool, arguments) in [
        (
            "refresh-index",
            "cartograph_admin",
            json!({"action":"index"}),
        ),
        ("read-notes", "cartograph_note", json!({"action":"list"})),
    ] {
        let saved = full
            .call(
                "cartograph_session",
                json!({"action":"macro_save","name":name,"steps":[{"tool":tool,"args":arguments}]}),
            )
            .await?;
        assert!(
            saved["error"].is_null() && saved["result"]["isError"] != true,
            "macro save failed: {saved}"
        );
    }
    let initial =
        cli(root, &["status", "--json"]).await?["project"]["snapshot"]["current"]["generation_id"]
            .clone();
    std::fs::write(root.join("lib.rs"), "pub fn changed_source() {}\n")?;
    let mut restricted = McpProcess::open(
        root,
        &[
            "serve",
            "--mcp",
            "--profile",
            "full",
            "--no-auto-sync",
            "--disable-tool",
            "cartograph_admin",
        ],
    )
    .await?;
    let permitted = restricted
        .call(
            "cartograph_session",
            json!({"action":"macro_run","name":"read-notes"}),
        )
        .await?;
    assert_eq!(
        permitted["result"]["structuredContent"]["successfulSteps"], 1,
        "permitted nested tool was rejected: {permitted}"
    );
    let denied = restricted
        .call(
            "cartograph_session",
            json!({"action":"macro_run","name":"refresh-index"}),
        )
        .await?;
    let report = &denied["result"]["structuredContent"];
    assert_eq!(
        report["successfulSteps"], 0,
        "disabled proxy tool ran: {denied}"
    );
    assert_eq!(report["failedSteps"], 1);
    assert!(
        report["steps"][0]["output"]
            .as_str()
            .is_some_and(|text| text.contains("authorization ceiling"))
    );
    let owner_permitted = full
        .call("cartograph_admin", json!({"action":"embedding-status"}))
        .await?;
    assert!(
        owner_permitted["error"].is_null() && owner_permitted["result"]["isError"] != true,
        "proxy restrictions leaked into the full owner: {owner_permitted}"
    );
    restricted.close().await?;
    full.close().await?;
    let reopened = cli(root, &["status", "--json"]).await?;
    assert_eq!(reopened["project"]["fresh"], false);
    assert_eq!(
        reopened["project"]["snapshot"]["current"]["generation_id"],
        initial
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_lived_proxy_rejects_reused_job_ids_after_owner_replacement()
-> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    let first = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    let mut proxy = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    let old = proxy
        .call("cartograph_admin", json!({"action":"embedding-status"}))
        .await?;
    assert_eq!(
        old["result"]["structuredContent"]["jobId"], 1,
        "first job missing: {old}"
    );
    first.close().await?;
    let mut second = McpProcess::open(
        root,
        &["serve", "--mcp", "--profile", "full", "--no-auto-sync"],
    )
    .await?;
    let new = second
        .call("cartograph_admin", json!({"action":"embedding-status"}))
        .await?;
    assert_eq!(
        new["result"]["structuredContent"]["jobId"], 1,
        "replacement job missing: {new}"
    );
    for action in ["status", "cancel"] {
        let rejected = proxy
            .call("cartograph_admin", json!({"action":action,"jobId":1}))
            .await?;
        assert!(
            rejected["error"].is_object() || rejected["result"]["isError"] == true,
            "old proxy addressed replacement job: {rejected}"
        );
        assert!(
            rejected
                .to_string()
                .contains("native project owner changed"),
            "owner fence did not reject the call: {rejected}"
        );
    }
    let current = second
        .call("cartograph_admin", json!({"action":"status","jobId":1}))
        .await?;
    assert_ne!(
        current["result"]["structuredContent"]["status"], "cancelled",
        "old proxy cancelled replacement job: {current}"
    );
    proxy.close().await?;
    second.close().await?;
    Ok(())
}
