//! Explicitly opted-in live pilot over a frozen, native-generated candidate pool.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde::Deserialize;
use tokio::process::Command;

use super::*;

const CASES: &str = include_str!("../../tests/fixtures/jev_context/cases.json");
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct Case {
    id: String,
    task: String,
    path: String,
    symbol: String,
}

#[derive(Clone)]
struct Candidate {
    name: String,
    path: String,
    kind: Value,
    start: Value,
    end: Value,
}

async fn cli(binary: &Path, root: &Path, args: &[&str]) -> Value {
    let mut command = Command::new(binary);
    command.args(args);
    if args.first() != Some(&"status") {
        command.arg("--project-path");
    }
    let mut child = command
        .arg(root)
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|_| panic!("native evaluation CLI failed to start"));
    let stdout = child
        .stdout
        .take()
        .unwrap_or_else(|| panic!("CLI output pipe"));
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        stdout
            .take(u64::try_from(OUTPUT_LIMIT + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .await
            .unwrap_or_else(|_| panic!("CLI output failed"));
        assert!(
            bytes.len() <= OUTPUT_LIMIT,
            "CLI evaluation output exceeded its bound"
        );
        let status = child
            .wait()
            .await
            .unwrap_or_else(|_| panic!("CLI wait failed"));
        assert!(status.success(), "native evaluation CLI failed");
        serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("CLI output was not JSON"))
    })
    .await;
    if let Ok(value) = result {
        value
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        panic!("native evaluation CLI exceeded its deadline");
    }
}

fn candidates(packet: &Value) -> Vec<Candidate> {
    packet["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item["symbol_id"].is_string()
                && item["reasons"].as_array().is_some_and(|reasons| {
                    !reasons.is_empty()
                        && reasons
                            .iter()
                            .all(|reason| matches!(reason.as_str(), Some("bm25" | "semantic")))
                })
        })
        .take(MAXIMUM_JUDGED)
        .map(|item| Candidate {
            name: item["qualified_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            path: item["path"].as_str().unwrap_or_default().to_owned(),
            kind: item["document_kind"].clone(),
            start: item["start_line"].clone(),
            end: item["end_line"].clone(),
        })
        .collect()
}

fn rank(case: &Case, candidates: &[Candidate], order: &[usize]) -> Option<usize> {
    order
        .iter()
        .position(|index| {
            candidates[*index].path == case.path && candidates[*index].name.ends_with(&case.symbol)
        })
        .map(|position| position + 1)
}

async fn evaluate_case(
    ranker: &ContextRanker<'_, JevClient>,
    native: &Value,
    case: &Case,
    generation: &Value,
    native_millis: u128,
) -> Value {
    let packet = &native["evidence"]["packet"];
    assert_eq!(
        native["freshness"], "current",
        "native evidence must be fresh"
    );
    assert_eq!(
        &packet["generation"]["generation_id"], generation,
        "generation changed during evaluation"
    );
    let candidates = candidates(packet);
    if candidates.len() < 2 {
        let order = (0..candidates.len()).collect::<Vec<_>>();
        return json!({"id":case.id,"candidateCount":candidates.len(),
            "expectedPath":case.path,"expectedSymbol":case.symbol,
            "nativeRank":rank(case,&candidates,&order),"jevRank":rank(case,&candidates,&order),
            "nativeContextMs":native_millis,"nativeRerank":packet["retrieval"]["rerank"],
            "skipped":"fewer_than_two_candidates","cold":null,"warm":null});
    }
    let state = json!({"task":case.task, "generationId": generation,
        "candidates":candidates.iter().map(|c| json!({"name":c.name,"kind":c.kind,"path":c.path,"startLine":c.start,"endLine":c.end})).collect::<Vec<_>>()});
    let (scores, cold) = ranker
        .judge(&state, candidates.len())
        .await
        .unwrap_or_else(|_| panic!("evaluation was cancelled"))
        .unwrap_or_else(|error| panic!("decision provider failed: {error}"));
    let (warm_scores, warm) = ranker
        .judge(&state, candidates.len())
        .await
        .unwrap_or_else(|_| panic!("warm evaluation was cancelled"))
        .unwrap_or_else(|error| panic!("warm decision provider failed: {error}"));
    assert_eq!(
        scores, warm_scores,
        "an exact cache hit must preserve probabilities"
    );
    assert_eq!(
        cold.map(|metrics| metrics.cache),
        Some(cartograph_domain::DecisionCacheOutcome::Miss)
    );
    assert_eq!(
        warm.map(|metrics| metrics.cache),
        Some(cartograph_domain::DecisionCacheOutcome::Hit)
    );
    let before = (0..candidates.len()).collect::<Vec<_>>();
    let mut after = before.clone();
    after.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let items = |order: &[usize]| {
        order.iter().map(|index| json!({"name": candidates[*index].name,"path":candidates[*index].path,"relevance":scores[*index]})).collect::<Vec<_>>()
    };
    json!({"id":case.id,"candidateCount":candidates.len(),"expectedPath":case.path,"expectedSymbol":case.symbol,
        "nativeRank":rank(case,&candidates,&before),"jevRank":rank(case,&candidates,&after),
        "nativeContextMs":native_millis,"nativeRerank":packet["retrieval"]["rerank"],
        "cold":cold,"warm":warm,"before":items(&before),"after":items(&after)})
}

#[tokio::test]
#[ignore = "explicit metadata-disclosure opt-in, current native CLI/owner, configured Jev and private output path required"]
async fn live_fixed_candidate_context_ranking_pilot() {
    assert_eq!(
        std::env::var("CARTOGRAPH_JEV_EVAL_ALLOW_METADATA")
            .ok()
            .as_deref(),
        Some("1")
    );
    let required =
        |name| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")));
    let root = required("CARTOGRAPH_JEV_EVAL_PROJECT");
    let binary = required("CARTOGRAPH_JEV_EVAL_BINARY");
    let output = required("CARTOGRAPH_JEV_EVAL_OUTPUT");
    assert!(
        !output.exists(),
        "do not replace sealed evaluation evidence"
    );
    let settings = JevSettings::try_from_project(&root)
        .unwrap_or_else(|e| panic!("settings: {e}"))
        .unwrap_or_else(|| panic!("Jev must be configured"));
    assert!(
        !settings.allows(JevFeature::Context),
        "native control requires context decisions disabled"
    );
    let client = JevClient::new(settings).unwrap_or_else(|e| panic!("client: {e}"));
    let cancellation = ProjectCancellation::new();
    let ranker = ContextRanker {
        provider: &client,
        cancellation: &cancellation,
    };
    let before = cli(&binary, &root, &["status"]).await;
    assert_eq!(before["storage"], "rocksdb");
    assert_eq!(before["project"]["fresh"], true);
    let generation = &before["project"]["snapshot"]["current"]["generation_id"];
    assert!(generation.is_string());
    let cases: Vec<Case> =
        serde_json::from_str(CASES).unwrap_or_else(|e| panic!("frozen cases: {e}"));
    let mut observations = Vec::new();
    for case in &cases {
        let started = Instant::now();
        let native = cli(&binary, &root, &["context", &case.task]).await;
        observations.push(
            evaluate_case(
                &ranker,
                &native,
                case,
                generation,
                started.elapsed().as_millis(),
            )
            .await,
        );
        println!("JEV_CONTEXT_PILOT_CASE={}", case.id);
    }
    let after = cli(&binary, &root, &["status"]).await;
    assert_eq!(after["project"]["fresh"], true);
    assert_eq!(
        &after["project"]["snapshot"]["current"]["generation_id"],
        generation
    );
    assert!(
        observations
            .iter()
            .filter(|case| case["cold"].is_object())
            .count()
            >= 12,
        "pilot must exercise the provider for at least half its cases"
    );
    let report = json!({"schemaVersion":1,"model":JEV_MODEL,"generation":generation,
        "casesFingerprint":blake3::hash(CASES.as_bytes()).to_hex().to_string(),
        "scope":"retrieval-only ordering on the same native-generated candidate pool; exact and graph anchors excluded",
        "latencyScope":"nativeContextMs is the complete native query; cold/warm elapsedMs measure the additional Jev decision only",
        "defaultPolicyChanged":false,"observations":observations});
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(output)
        .unwrap_or_else(|_| panic!("private evaluation output could not be created"));
    file.write_all(&serde_json::to_vec_pretty(&report).unwrap_or_default())
        .unwrap_or_else(|_| panic!("evaluation report write failed"));
}
