//! Sustained mixed source changes through the headless owner and real MCP proxies.
use super::read_only::McpProcess;
use super::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn evidence(reply: &Value) -> Result<&Value> {
    if !reply["error"].is_null() || reply["result"]["isError"] == true {
        return Err(format!("MCP operation failed: {reply}").into());
    }
    reply["result"]
        .get("structuredContent")
        .ok_or_else(|| "MCP evidence missing".into())
}

async fn reader(root: std::path::PathBuf, stop: Arc<AtomicBool>, mode: u8) -> Result<Value> {
    let mut mcp = McpProcess::open(&root, &["serve", "--mcp", "--profile", "full"])
        .await
        .map_err(|error| error.to_string())?;
    let mut samples = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let (tool, arguments) = match mode {
            0 => (
                "cartograph_find",
                json!({"query":"fixture_0_0", "by":"name", "limit":5, "allowStale":true}),
            ),
            1 => (
                "cartograph_find",
                json!({"query":"wrapping_add", "by":"bm25", "limit":5, "allowStale":true}),
            ),
            _ => (
                "cartograph_context",
                json!({"task":"fixture arithmetic", "mode":"deterministic", "workingTree":"indexed", "allowStale":true}),
            ),
        };
        let reply = mcp
            .call(tool, arguments)
            .await
            .map_err(|error| error.to_string())?;
        let result = evidence(&reply).map_err(|error| format!("reader {mode} {tool}: {error}"))?;
        assert!(
            matches!(result["freshness"].as_str(), Some("current" | "stale")),
            "missing freshness: {reply}"
        );
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    mcp.close().await.map_err(|error| error.to_string())?;
    Ok(json!({"mode":mode, "millis":statistics(&mut samples)}))
}

fn statistics(samples: &mut [f64]) -> Value {
    samples.sort_by(f64::total_cmp);
    if samples.is_empty() {
        return json!({"count":0});
    }
    let at = |percent: usize| samples[(samples.len() - 1) * percent / 100];
    json!({"count":samples.len(), "p50":at(50), "p95":at(95), "p99":at(99), "max":at(100)})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "30-minute manual mixed workload; CARTOGRAPH_MIXED_SOAK_SECONDS permits 10..7200 seconds for harness validation"]
async fn sustained_headless_owner_mixed_edits_and_mcp_readers() -> Result {
    let duration = std::env::var("CARTOGRAPH_MIXED_SOAK_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(1800);
    assert!((10..=7200).contains(&duration));
    let (project, owner) = fixture().await?;
    println!("MIXED_OWNER_PHASE initialized");
    let root = project.path();
    let initial = cli(root, &["db", "usage", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = tokio::task::JoinSet::new();
    for mode in 0..3 {
        readers.spawn(reader(root.to_path_buf(), stop.clone(), mode));
    }
    let mut observer = McpProcess::open(root, &["serve", "--mcp", "--profile", "full"])
        .await
        .map_err(|error| error.to_string())?;
    let measured = run(root, &mut observer, duration).await;
    println!("MIXED_OWNER_PHASE measured");
    stop.store(true, Ordering::Release);
    let (mut owner, reader_stats) = finish_readers(owner, readers).await?;
    let Measurements {
        started,
        revision,
        samples,
        periodic,
    } = measured?;
    let final_usage = cli(root, &["db", "usage", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    let status = cli(root, &["status", "--json"])
        .await
        .map_err(|error| error.to_string())?;
    assert_eq!(status["project"]["fresh"], true);
    assert_eq!(status["autoSync"]["errors"], 0);
    assert_eq!(status["autoSync"]["retentionFailures"], 0);
    let mut distributions = serde_json::Map::new();
    for (mode, mut values) in samples {
        distributions.insert(mode.into(), statistics(&mut values));
    }
    observer.close().await.map_err(|error| error.to_string())?;
    let pid = owner.id().ok_or("headless owner exited")?.to_string();
    assert!(
        tokio::process::Command::new("/bin/kill")
            .args(["-TERM", &pid])
            .status()
            .await?
            .success()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(30), owner.wait())
            .await??
            .success()
    );
    let verify = cli(root, &["index", "--force", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    assert_eq!(verify["live_source"], "matched");
    assert_eq!(
        verify["content_digest"],
        status["project"]["snapshot"]["current"]["content_digest"]
    );
    println!(
        "MIXED_OWNER_SOAK {}",
        json!({"profile":if cfg!(debug_assertions){"debug"}else{"release"}, "durationSeconds":started.elapsed().as_secs_f64(), "sourceFiles":253, "initialFunctions":4003, "editCycles":revision, "editToCurrentMcpQueryMillis":distributions, "readerQueryMillis":reader_stats, "initialUsage":initial, "finalUsage":final_usage, "autoSync":status["autoSync"], "periodic":periodic, "coldDigestVerified":true})
    );
    Ok(())
}

async fn finish_readers(
    mut owner: Child,
    mut readers: tokio::task::JoinSet<Result<Value>>,
) -> Result<(Child, Vec<Value>)> {
    let mut statistics = Vec::new();
    let mut failure = None;
    while let Some(reader) = readers.join_next().await {
        match reader? {
            Ok(stats) => statistics.push(stats),
            Err(error) => failure = Some(error),
        }
    }
    if let Some(error) = failure {
        owner.kill().await?;
        let output = owner.wait_with_output().await?;
        eprintln!(
            "owner diagnostics: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(error);
    }
    Ok((owner, statistics))
}

async fn fixture() -> Result<(tempfile::TempDir, Child)> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    for file in 0..250 {
        let mut source = String::new();
        for symbol in 0..16 {
            writeln!(
                &mut source,
                "pub fn fixture_{file}_{symbol}(input:u64)->u64{{input.wrapping_add({symbol})}}"
            )?;
        }
        std::fs::write(root.join(format!("fixture_{file}.rs")), source)?;
    }
    std::fs::write(root.join("probe.rs"), "pub fn probe_value() -> u64 { 0 }\n")?;
    std::fs::write(root.join("probe.py"), "def python_probe():\n    return 0\n")?;
    std::fs::write(
        root.join("literal.rs"),
        "pub fn stable_literal() -> u64 { 0 }\n",
    )?;
    let mut owner = command(root, &["watch"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        BufReader::new(owner.stdout.take().ok_or("watch output missing")?).read_line(&mut ready),
    )
    .await??;
    assert_eq!(serde_json::from_str::<Value>(&ready)?["owner"], "ready");
    println!("MIXED_OWNER_PHASE owner_ready");
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let status = cli(root, &["status", "--json"])
                .await
                .map_err(|error| error.to_string())?;
            if status["project"]["fresh"] == true
                && status["autoSync"]["lastSuccessUnixMillis"].is_u64()
            {
                break Ok::<_, Box<dyn Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    println!("MIXED_OWNER_PHASE initial_index_completed");
    Ok((project, owner))
}

struct Measurements {
    started: Instant,
    revision: u64,
    samples: BTreeMap<&'static str, Vec<f64>>,
    periodic: Vec<Value>,
}

async fn run(root: &Path, observer: &mut McpProcess, duration: u64) -> Result<Measurements> {
    let reply = observer
        .call("cartograph_status", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    let mut previous = evidence(&reply)?.clone();
    let started = Instant::now();
    let mut revision = 0_u64;
    let mut samples = BTreeMap::<&str, Vec<f64>>::new();
    let mut periodic = Vec::new();
    let mut next_sample = 60_u64;
    let mut renamed = false;
    while started.elapsed() < Duration::from_secs(duration) {
        revision += 1;
        let edited = Instant::now();
        let mode = edit(root, revision, &mut renamed)?;
        wait_current(observer).await?;
        samples
            .entry(mode)
            .or_default()
            .push(edited.elapsed().as_secs_f64() * 1000.0);
        verify_edit(observer, revision, renamed).await?;
        previous = completed_publication(observer, &previous).await?;
        if started.elapsed().as_secs() >= next_sample {
            let usage = cli(root, &["db", "usage", "--format", "json"])
                .await
                .map_err(|error| error.to_string())?;
            let status = cli(root, &["status", "--json"])
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(status["autoSync"]["errors"], 0);
            assert_eq!(status["autoSync"]["retentionFailures"], 0);
            let item = json!({"elapsedSeconds":started.elapsed().as_secs(), "editCycles":revision, "usage":usage, "autoSync":status["autoSync"]});
            println!("MIXED_OWNER_SAMPLE {item}");
            periodic.push(item);
            next_sample += 60;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(Measurements {
        started,
        revision,
        samples,
        periodic,
    })
}

async fn verify_edit(observer: &mut McpProcess, revision: u64, renamed: bool) -> Result {
    let (query, count, path) = match revision % 6 {
        0 => ("stable_literal".to_owned(), 1, "literal.rs"),
        1 => ("python_probe".to_owned(), 1, "probe.py"),
        2 => (format!("added_{revision}"), 1, "addition.rs"),
        3 => (format!("added_{}", revision - 1), 0, "addition.rs"),
        _ => (
            "probe_value".to_owned(),
            1,
            if renamed { "renamed.rs" } else { "probe.rs" },
        ),
    };
    let reply = observer
        .call(
            "cartograph_find",
            json!({"by":"name", "query":query, "limit":5}),
        )
        .await
        .map_err(|error| error.to_string())?;
    let packet = evidence(&reply)?;
    assert_eq!(packet["freshness"], "current");
    let rows = packet["evidence"]["rows"]
        .as_array()
        .ok_or("expected rows missing")?;
    assert_eq!(rows.len(), count, "wrong cycle evidence: {reply}");
    if let Some(row) = rows.first() {
        assert_eq!(row["path"], path);
    }
    if revision % 6 == 5 {
        let name = format!("declaration_{revision}");
        let target = observer
            .call(
                "cartograph_find",
                json!({"by":"name", "query":name, "limit":5}),
            )
            .await
            .map_err(|error| error.to_string())?;
        let target = evidence(&target)?["evidence"]["rows"][0]["id"].clone();
        assert!(target.is_string());
        let graph = observer.call("cartograph_graph", json!({"start":"probe_value", "direction":"both", "edgeKind":"calls", "depth":1, "maxNodes":10})).await.map_err(|error| error.to_string())?;
        let packet = evidence(&graph)?;
        assert_eq!(packet["freshness"], "current");
        let nodes = packet["evidence"]["graph"]["outgoing"]["nodes"]
            .as_array()
            .ok_or("call nodes missing")?;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["symbol"]["symbol_id"], target);
        assert_eq!(nodes[0]["via"]["edge_kind"], "calls");
    }
    Ok(())
}

async fn completed_publication(observer: &mut McpProcess, previous: &Value) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let reply = observer
                .call("cartograph_status", json!({}))
                .await
                .map_err(|error| error.to_string())?;
            let status = evidence(&reply)?;
            assert_eq!(status["autoSync"]["errors"], 0);
            assert_eq!(status["autoSync"]["retentionFailures"], 0);
            if status["project"]["fresh"] == true
                && status["autoSync"]["publications"]
                    .as_u64()
                    .ok_or("publication count missing")?
                    > previous["autoSync"]["publications"]
                        .as_u64()
                        .ok_or("prior publication count missing")?
            {
                for field in ["generation_id", "source_revision", "content_digest"] {
                    assert_ne!(
                        status["project"]["snapshot"]["current"][field],
                        previous["project"]["snapshot"]["current"][field],
                        "cycle retained {field}"
                    );
                }
                return Ok(status.clone());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

fn edit(root: &Path, revision: u64, renamed: &mut bool) -> Result<&'static str> {
    let mode = match revision % 6 {
        0 => {
            std::fs::write(
                root.join("literal.rs"),
                format!("pub fn stable_literal() -> u64 {{ {} }}\n", revision % 10),
            )?;
            "literal"
        }
        1 => {
            std::fs::write(
                root.join("probe.py"),
                format!("def python_probe():\n    return {}\n", revision % 10),
            )?;
            "python"
        }
        2 => {
            std::fs::write(
                root.join("addition.rs"),
                format!("pub fn added_{revision}() {{}}\n"),
            )?;
            "add_or_declare"
        }
        3 => {
            std::fs::remove_file(root.join("addition.rs"))?;
            "delete"
        }
        4 => {
            let (from, to) = if *renamed {
                ("renamed.rs", "probe.rs")
            } else {
                ("probe.rs", "renamed.rs")
            };
            std::fs::rename(root.join(from), root.join(to))?;
            *renamed = !*renamed;
            "rename"
        }
        _ => {
            std::fs::write(
                root.join(if *renamed { "renamed.rs" } else { "probe.rs" }),
                format!(
                    "pub fn declaration_{revision}() -> u64 {{ 1 }}\npub fn probe_value() -> u64 {{ declaration_{revision}() }}\n"
                ),
            )?;
            "signature_topology"
        }
    };
    Ok(mode)
}

async fn wait_current(observer: &mut McpProcess) -> Result {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let reply = observer
                .call(
                    "cartograph_find",
                    json!({"query":"probe_value", "by":"name", "limit":5, "allowStale":true}),
                )
                .await
                .map_err(|error| error.to_string())?;
            let packet = evidence(&reply)?;
            if packet["freshness"] == "current" {
                assert_eq!(
                    packet["evidence"]["rows"]
                        .as_array()
                        .ok_or("probe rows missing")?
                        .len(),
                    1
                );
                break Ok::<_, Box<dyn Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
