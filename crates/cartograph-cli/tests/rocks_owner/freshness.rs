//! End-to-end source edit to exact current evidence through the actual native CLI.
use super::*;
use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

type SoakResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

async fn observe_revision(root: &Path, revision: u64) -> SoakResult {
    let name = format!("revision_{revision}");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let output = command(
                root,
                &[
                    "find",
                    &name,
                    "--by",
                    "name",
                    "--allow-stale",
                    "--format",
                    "json",
                ],
            )
            .output()
            .await?;
            assert!(
                output.status.success(),
                "freshness query failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let observed: Value = serde_json::from_slice(&output.stdout)?;
            if observed["freshness"] == "current"
                && observed["evidence"]["rows"]
                    .as_array()
                    .is_some_and(|rows| rows.len() == 1 && rows[0]["name"] == name)
            {
                return Ok::<(), Box<dyn Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}

async fn read_during_updates(root: std::path::PathBuf, stop: Arc<AtomicBool>) -> SoakResult<u64> {
    let mut reads = 0_u64;
    while !stop.load(Ordering::Acquire) {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            command(
                &root,
                &[
                    "find",
                    "fixture_0_0",
                    "--by",
                    "name",
                    "--allow-stale",
                    "--format",
                    "json",
                ],
            )
            .output(),
        )
        .await??;
        assert!(
            output.status.success(),
            "concurrent native read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evidence: Value = serde_json::from_slice(&output.stdout)?;
        assert!(matches!(
            evidence["freshness"].as_str(),
            Some("current" | "stale")
        ));
        assert_eq!(
            evidence["evidence"]["rows"]
                .as_array()
                .ok_or("concurrent rows missing")?
                .len(),
            1
        );
        assert_eq!(evidence["evidence"]["rows"][0]["name"], "fixture_0_0");
        reads += 1;
    }
    Ok(reads)
}

fn percentile(values: &[f64], percentile: usize) -> f64 {
    values[(values.len() - 1) * percentile / 100]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "two-minute actual native CLI freshness and reader concurrency gate"]
async fn two_minute_cli_edit_soak_keeps_queries_current_and_reports_storage_growth() -> SoakResult {
    let project = tempfile::tempdir()?;
    let root = project.path();
    for file in 0..100 {
        let mut source = String::new();
        for symbol in 0..8 {
            writeln!(
                &mut source,
                "pub fn fixture_{file}_{symbol}(input:u64)->u64{{input.wrapping_add({symbol})}}"
            )?;
        }
        std::fs::write(root.join(format!("fixture_{file}.rs")), source)?;
    }
    std::fs::write(root.join("probe.rs"), "pub fn revision_0() {}\n")?;
    let (mut child, stdin) = owner(root, &["serve", "--mcp", "--no-startup-sync"])
        .await
        .map_err(|error| error.to_string())?;
    cli(root, &["index", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    let initial = cli(root, &["db", "usage", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let readers = (0..3)
        .map(|_| tokio::spawn(read_during_updates(root.to_path_buf(), stop.clone())))
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut samples = Vec::new();
    let mut revision = 0_u64;
    while started.elapsed() < Duration::from_secs(120) {
        revision += 1;
        let edited = Instant::now();
        std::fs::write(
            root.join("probe.rs"),
            format!("pub fn revision_{revision}() {{}}\n"),
        )?;
        observe_revision(root, revision).await?;
        samples.push(edited.elapsed().as_secs_f64() * 1000.0);
    }
    stop.store(true, Ordering::Release);
    let mut reads = 0;
    for reader in readers {
        reads += reader.await??;
    }
    let final_usage = cli(root, &["db", "usage", "--format", "json"])
        .await
        .map_err(|error| error.to_string())?;
    let status = cli(root, &["status", "--json"])
        .await
        .map_err(|error| error.to_string())?;
    assert_eq!(status["project"]["fresh"], true);
    assert!(
        revision >= 10,
        "insufficient measured edit cycles: {revision}"
    );
    samples.sort_by(f64::total_cmp);
    println!(
        "NATIVE_CLI_FRESHNESS_SOAK {}",
        serde_json::json!({"profile":if cfg!(debug_assertions){"debug"}else{"release"},"durationSeconds":started.elapsed().as_secs_f64(),"sourceFiles":101,"initialFunctions":801,"editCycles":revision,"concurrentReaders":3,"verifiedConcurrentReads":reads,"editToCurrentQueryMillis":{"p50":percentile(&samples,50),"p95":percentile(&samples,95),"p99":percentile(&samples,99)},"initialUsage":initial,"finalUsage":final_usage,"autoSync":status["autoSync"]})
    );
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await??
            .success()
    );
    assert_eq!(
        cli(root, &["status", "--json"])
            .await
            .map_err(|error| error.to_string())?["project"]["fresh"],
        true
    );
    Ok(())
}
