use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::process::Command as ProcessCommand;

use cartograph_db::{
    CurrentGeneration, CurrentGenerationLookup, ExactTextLookup, GenerationContents,
    PrepareGenerationMetrics, SearchQuery, StagedGeneration,
};
use cartograph_domain::{GenerationDigestVersion, GenerationId};
use cartograph_indexer::NativePipelineReport;
use serde::{Deserialize, Serialize};
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

use super::*;

const CHILD_WORKERS_ENV: &str = "CARTOGRAPH_NATIVE_CORPUS_CHILD_WORKERS";
const CHILD_SCHEMA_PREFIX_ENV: &str = "CARTOGRAPH_NATIVE_CORPUS_CHILD_SCHEMA_PREFIX";
const CHILD_HANG_ENV: &str = "CARTOGRAPH_NATIVE_CORPUS_CHILD_HANG";
const CHILD_REPORT_PREFIX: &str = "CARTOGRAPH_NATIVE_CORPUS_CHILD_REPORT=";
const MATRIX_REPORT_PREFIX: &str = "CARTOGRAPH_NATIVE_CORPUS_MATRIX_REPORT=";
const CORPUS_NAME: &str = "cartograph-v1-real-typescript-v1";
const CORPUS_FINGERPRINT_DOMAIN: &[u8] = b"cartograph-v2-native-real-corpus-v1";
const EXPECTED_CORPUS_FINGERPRINT: &str =
    "ab91088c482ed36d31759382283342654ce6958be4e601429b8181da531c5fc1";
// V16 changes the digest domain; corpus bytes, facts, ranking, and memory bounds stay fixed.
const EXPECTED_LOGICAL_DIGEST: &str =
    "14064b4d8c27542e84299a23fd4d345126eb290c9d85dd5a446b836d6ac748f6";
const EXPECTED_BM25_DOCUMENT_IDS: [&str; 5] = [
    "5471dbfc-3ba3-87dd-8861-1ce1dd51ed32",
    "78f1eb97-24b2-8a80-ad44-6dd679456592",
    "a5008cb5-184d-819e-a6bf-54c4f6207da0",
    "b7c9628b-c28f-80cd-9bf6-3cfbce849c70",
    "fc1a31af-afc0-827c-b3bc-acb5e56736d1",
];
const EXPECTED_TAGS_BM25_DOCUMENT_IDS: [&str; 6] = [
    "07ce3e1c-8912-83cb-8464-999aeef53935",
    "f758d534-485c-8f11-82b9-b7a06d614ac1",
    "d5f96fc2-4c1a-8d2a-955f-a8ecb5806548",
    "6e845eea-7263-8ff8-a07a-95f7e2ad66da",
    "94c5cdbf-1dbd-8a90-b83c-b55c79a0cd2c",
    "72da0535-5a0c-830b-a9ec-ab9948bceb6e",
];
const EXPECTED_FILES: i64 = 34;
const EXPECTED_SYMBOLS: i64 = 6_336;
const EXPECTED_EDGES: i64 = 8_945;
const EXPECTED_REFERENCES: i64 = 16_237;
const EXPECTED_NUMERICAL_SITES: i64 = 0;
const EXPECTED_DOCUMENTS: i64 = 6_336;
const EXPECTED_EDGE_KINDS: [&str; 10] = [
    "calls",
    "contains",
    "exports",
    "extends",
    "field_access",
    "implements",
    "instantiates",
    "references",
    "returns",
    "type_of",
];
const EXPECTED_SOURCE_BYTES: u64 = 1_052_564;
const EXPECTED_RESOLVED_REFERENCES: u64 = 2_825;
const EXPECTED_UNRESOLVED_REFERENCES: u64 = 13_424;
const EXPECTED_MODELED_GENERATION_BYTES: u64 = 19_759_486;
const EXPECTED_RESOLVE_HIGH_WATER_BYTES: u64 = 110_953_985;
const EXPECTED_VALIDATION_HIGH_WATER_BYTES: u64 = 141_573_768;
const CORPUS_QUERY: &str = "detectSecretsHandling";
const TAGS_CORPUS_QUERY: &str = "tagscanary";
const LIVE_SECRET_SENTINEL: &str = "sk_live_secret";
const SOURCE_REVISION: &str = "native-real-corpus-v1";
const WARMUP_SAMPLES: usize = 1;
const MEASURED_SAMPLES: usize = 3;
const WORKER_MATRIX: [u16; 5] = [1, 2, 4, 8, 16];
const MAX_FILES: usize = 64;
const MAX_PATH_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GENERATION_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SUPERVISOR_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SUPERVISOR_TASKS: usize = 64;
const ITEM_TIMEOUT: Duration = Duration::from_secs(30);
const STAGE_TIMEOUT: Duration = Duration::from_secs(45);
const OPERATION_TIMEOUT: Duration = Duration::from_mins(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(45);
const CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const COPY_TIMEOUT: Duration = Duration::from_secs(30);
const LEASE_DURATION: Duration = Duration::from_secs(90);
const CHILD_TIMEOUT: Duration = Duration::from_mins(3);
const CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const RSS_SAMPLE_INTERVAL: Duration = Duration::from_millis(25);
const MEDIAN_PERCENTILE: usize = 50;
const TAIL_PERCENTILE: usize = 95;

struct CorpusSource {
    path: &'static str,
    source: &'static str,
}

const CORPUS: &[CorpusSource] = &[
    CorpusSource {
        path: "src/types.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/types.ts"),
    },
    CorpusSource {
        path: "src/config.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/config.ts"),
    },
    CorpusSource {
        path: "src/index.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/index.ts"),
    },
    CorpusSource {
        path: "src/cartograph-llm-service.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/cartograph-llm-service.ts"),
    },
    CorpusSource {
        path: "src/context/candidate-search.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/context/candidate-search.ts"),
    },
    CorpusSource {
        path: "src/context/subgraph.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/context/subgraph.ts"),
    },
    CorpusSource {
        path: "src/db/queries-search.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/db/queries-search.ts"),
    },
    CorpusSource {
        path: "src/db/postgres-worker.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/db/postgres-worker.ts"),
    },
    CorpusSource {
        path: "src/extraction/tree-sitter.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/extraction/tree-sitter.ts"),
    },
    CorpusSource {
        path: "src/extraction/tree-sitter-decls.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/extraction/tree-sitter-decls.ts"),
    },
    CorpusSource {
        path: "src/extraction/ts-extract-calls.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/extraction/ts-extract-calls.ts"),
    },
    CorpusSource {
        path: "src/extraction/extraction-phases.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/extraction/extraction-phases.ts"),
    },
    CorpusSource {
        path: "src/resolution/import-resolver.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/resolution/import-resolver.ts"),
    },
    CorpusSource {
        path: "src/resolution/name-matcher.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/resolution/name-matcher.ts"),
    },
    CorpusSource {
        path: "src/mcp/tools/_search.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/mcp/tools/_search.ts"),
    },
    CorpusSource {
        path: "src/mcp/tools/context.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/mcp/tools/context.ts"),
    },
    CorpusSource {
        path: "src/mcp/tools/explore.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/mcp/tools/explore.ts"),
    },
    CorpusSource {
        path: "src/installer/llm-setup-plan.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/installer/llm-setup-plan.ts"),
    },
    CorpusSource {
        path: "src/llm/secrets-detector.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/llm/secrets-detector.ts"),
    },
    CorpusSource {
        path: "src/biomarkers/engine.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/biomarkers/engine.ts"),
    },
    CorpusSource {
        path: "src/mcp/tools/admin.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/mcp/tools/admin.ts"),
    },
    CorpusSource {
        path: "src/db/queries-summaries.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/db/queries-summaries.ts"),
    },
    CorpusSource {
        path: "src/features/backend/runtime.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/features/backend/runtime.ts"),
    },
    CorpusSource {
        path: "src/index-hooks/types.ts",
        source: include_str!("../fixtures/native_corpus_v1/src/index-hooks/types.ts"),
    },
    CorpusSource {
        path: "fixtures/greeter.ts",
        source: include_str!("../../../cartograph-extract/tests/fixtures/v1_1_33/greeter.ts"),
    },
    CorpusSource {
        path: "fixtures/worker.js",
        source: include_str!("../../../cartograph-extract/tests/fixtures/v1_1_33/worker.js"),
    },
    CorpusSource {
        path: "fixtures/view.tsx",
        source: include_str!("../../../cartograph-extract/tests/fixtures/v1_1_33/view.tsx"),
    },
    CorpusSource {
        path: "fixtures/card.jsx",
        source: include_str!("../../../cartograph-extract/tests/fixtures/v1_1_33/card.jsx"),
    },
    CorpusSource {
        path: "tags/elixir.ex",
        source: "defmodule T2Elixir do\n  def tagscanary, do: :sk_live_secret\nend\n",
    },
    CorpusSource {
        path: "tags/haskell.hs",
        source: "tagscanary x = x\n",
    },
    CorpusSource {
        path: "tags/julia.jl",
        source: "function tagscanary(x)\n  x\nend\n",
    },
    CorpusSource {
        path: "tags/ocaml.ml",
        source: "let tagscanary x = x\n",
    },
    CorpusSource {
        path: "tags/ocaml.mli",
        source: "val tagscanary : int -> int\n",
    },
    CorpusSource {
        path: "tags/verilog.sv",
        source: "module T2Verilog;\nfunction int tagscanary; endfunction\nendmodule\n",
    },
];

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RowCounts {
    files: i64,
    symbols: i64,
    edges: i64,
    references: i64,
    numerical_sites: i64,
    documents: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct NativeReport {
    discovered_files: u64,
    source_bytes: u64,
    symbols: u64,
    numerical_sites: u64,
    resolved_references: u64,
    unresolved_references: u64,
    diagnostics: u64,
    modeled_generation_bytes: u64,
    resolve_high_water_bytes: u64,
    validation_high_water_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct CorpusInvariant {
    corpus_fingerprint: String,
    logical_digest: String,
    logical_digest_version: i16,
    rows: RowCounts,
    edge_kinds: Vec<String>,
    bm25_document_ids: Vec<String>,
    tags_bm25_document_ids: Vec<String>,
    native: NativeReport,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct CopyTableNanos {
    files: u64,
    symbols: u64,
    edges: u64,
    references: u64,
    numerical_sites: u64,
    documents: u64,
}

impl CopyTableNanos {
    fn from_snapshot(snapshot: cartograph_db::PrepareGenerationMetricsSnapshot) -> Self {
        Self {
            files: duration_nanos(snapshot.files_copy_duration()),
            symbols: duration_nanos(snapshot.symbols_copy_duration()),
            edges: duration_nanos(snapshot.edges_copy_duration()),
            references: duration_nanos(snapshot.references_copy_duration()),
            numerical_sites: duration_nanos(snapshot.numerical_sites_copy_duration()),
            documents: duration_nanos(snapshot.documents_copy_duration()),
        }
    }

    const fn complete(self) -> bool {
        self.files != 0
            && self.symbols != 0
            && self.edges != 0
            && self.references != 0
            && self.numerical_sites != 0
            && self.documents != 0
    }

    const fn total(self) -> u64 {
        self.files
            .saturating_add(self.symbols)
            .saturating_add(self.edges)
            .saturating_add(self.references)
            .saturating_add(self.numerical_sites)
            .saturating_add(self.documents)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SampleReport {
    sample: usize,
    native_nanos: u64,
    supervised_pipeline_nanos: u64,
    copy_nanos: u64,
    copy_tables_nanos: CopyTableNanos,
    relation_validation_nanos: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkerReport {
    workers: u16,
    invariant: CorpusInvariant,
    baseline_rss_bytes: u64,
    peak_rss_bytes: u64,
    peak_rss_delta_bytes: u64,
    rss_successful_samples: u64,
    native_p50_nanos: u64,
    native_p95_nanos: u64,
    supervised_pipeline_p50_nanos: u64,
    supervised_pipeline_p95_nanos: u64,
    copy_p50_nanos: u64,
    copy_p95_nanos: u64,
    copy_tables_p50_nanos: CopyTableNanos,
    copy_tables_p95_nanos: CopyTableNanos,
    relation_validation_p50_nanos: u64,
    relation_validation_p95_nanos: u64,
    samples: Vec<SampleReport>,
}

#[derive(Serialize)]
struct MatrixReport {
    architecture: &'static str,
    operating_system: &'static str,
    logical_cpus: usize,
    rust_toolchain: &'static str,
    database_image: &'static str,
    corpus_name: &'static str,
    corpus_files: usize,
    corpus_source_bytes: u64,
    corpus_fingerprint: &'static str,
    bm25_query: &'static str,
    tags_bm25_query: &'static str,
    rss_sample_interval_millis: u64,
    warmup_samples: usize,
    measured_samples: usize,
    workers: Vec<WorkerReport>,
}

struct SampleObservation {
    invariant: CorpusInvariant,
    native_nanos: u64,
    supervised_pipeline_nanos: u64,
    copy_nanos: u64,
    copy_tables_nanos: CopyTableNanos,
    relation_validation_nanos: u64,
}

struct NativeExecutionRequest<'a> {
    supervisor: &'a IndexerSupervisor,
    lease_target: LeaseTarget,
    source_root: SourceRoot,
    staged: StagedGeneration,
    workers: u16,
}

struct CompletedSample {
    current: CurrentGeneration,
    native: NativePipelineReport,
    native_nanos: u64,
    supervised_pipeline_nanos: u64,
    copy_nanos: u64,
    copy_tables_nanos: CopyTableNanos,
    relation_validation_nanos: u64,
}

struct SampleInspection<'a> {
    fixture: &'a DatabaseFixture,
    generation_id: &'a GenerationId,
    lease_target: &'a LeaseTarget,
    supervisor: &'a IndexerSupervisor,
    completed: CompletedSample,
}

struct Bm25Inspection<'a> {
    fixture: &'a DatabaseFixture,
    current: &'a GenerationId,
    query_text: &'static str,
    limit: u16,
    operation: &'static str,
}

struct WorkerSummaryInput {
    workers: u16,
    observations: Vec<SampleObservation>,
    baseline_rss_bytes: u64,
    peak_rss_bytes: u64,
    rss_successful_samples: u64,
}

#[derive(Debug)]
struct WorkerChildOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct WorkerChildRequest<'a> {
    executable: &'a Path,
    workers: u16,
    schema_prefix: &'a str,
    timeout: Duration,
    hang: bool,
}

struct RssSampler {
    baseline: u64,
    peak: Arc<AtomicU64>,
    successful_samples: Arc<AtomicU64>,
    failed_samples: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

type CorpusResult<T> = Result<T, String>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn frozen_native_corpus_is_worker_deterministic_and_bounded() {
    let result = match env::var(CHILD_WORKERS_ENV) {
        Ok(workers) => run_child(&workers).await,
        Err(_) => run_parent().await,
    };
    if let Err(error) = result {
        panic!("native real-corpus matrix failed: {error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn timed_out_child_is_killed_reaped_and_schema_cleaned() {
    if let Err(failure) = prove_timed_out_child_cleanup().await {
        panic!("timed-out child cleanup regression failed: {failure}");
    }
}

async fn prove_timed_out_child_cleanup() -> CorpusResult<()> {
    let executable = env::current_exe().map_err(|_| error("current-test-executable"))?;
    let schema_prefix = format!("cartograph_native_timeout_{}", process::id());
    let seeded_schema = child_schema_name(&schema_prefix, "warmup", 0);
    create_schema_for_cleanup_test(&seeded_schema).await?;
    let result = run_worker_child_with_options(WorkerChildRequest {
        executable: &executable,
        workers: 1,
        schema_prefix: &schema_prefix,
        timeout: Duration::from_millis(250),
        hang: true,
    })
    .await;
    let Err(failure) = result else {
        return Err(error("deliberately-hung-child-did-not-time-out"));
    };
    if !failure.contains("was killed and reaped") {
        return Err(error("timed-out-child-was-not-killed-and-reaped"));
    }
    if !failure.contains("parent removed all child schemas") {
        return Err(error("timed-out-child-schema-cleanup-was-not-reported"));
    }
    if schema_exists(&seeded_schema).await? {
        return Err(error("timed-out-child-schema-still-exists"));
    }
    Ok(())
}

async fn run_parent() -> CorpusResult<()> {
    let executable = env::current_exe().map_err(|_| error("current-test-executable"))?;
    let mut reports = Vec::with_capacity(WORKER_MATRIX.len());
    for workers in WORKER_MATRIX {
        let schema_prefix = child_schema_prefix(workers);
        let output = run_worker_child(&executable, workers, &schema_prefix).await?;
        if !output.status.success() {
            let failure = format!(
                "worker child {workers} failed\n{}\n{}",
                tail(&output.stdout),
                tail(&output.stderr)
            );
            let cleanup = cleanup_child_schemas(&schema_prefix).await;
            return Err(with_cleanup_result(&failure, &cleanup));
        }
        let report = output
            .stdout
            .lines()
            .find_map(|line| {
                line.find(CHILD_REPORT_PREFIX)
                    .map(|start| &line[start + CHILD_REPORT_PREFIX.len()..])
            })
            .ok_or_else(|| format!("child report missing from output\n{}", tail(&output.stdout)))?;
        let report =
            serde_json::from_str::<WorkerReport>(report).map_err(|_| error("child-report-json"))?;
        if report.workers != workers {
            return Err(error("child-worker-mismatch"));
        }
        reports.push(report);
    }
    let expected = reports
        .first()
        .map(|report| &report.invariant)
        .ok_or_else(|| error("empty-worker-matrix"))?;
    if reports.iter().any(|report| &report.invariant != expected) {
        return Err(error("cross-worker-invariant-drift"));
    }
    let report = MatrixReport {
        architecture: env::consts::ARCH,
        operating_system: env::consts::OS,
        logical_cpus: thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        rust_toolchain: env!("CARGO_PKG_RUST_VERSION"),
        database_image: cartograph_db::MANAGED_DATABASE_IMAGE,
        corpus_name: CORPUS_NAME,
        corpus_files: CORPUS.len(),
        corpus_source_bytes: EXPECTED_SOURCE_BYTES,
        corpus_fingerprint: EXPECTED_CORPUS_FINGERPRINT,
        bm25_query: CORPUS_QUERY,
        tags_bm25_query: TAGS_CORPUS_QUERY,
        rss_sample_interval_millis: u64::try_from(RSS_SAMPLE_INTERVAL.as_millis())
            .map_err(|_| error("rss-sample-interval"))?,
        warmup_samples: WARMUP_SAMPLES,
        measured_samples: MEASURED_SAMPLES,
        workers: reports,
    };
    let json = serde_json::to_string(&report).map_err(|_| error("matrix-report-json"))?;
    println!("{MATRIX_REPORT_PREFIX}{json}");
    Ok(())
}

async fn run_worker_child(
    executable: &Path,
    workers: u16,
    schema_prefix: &str,
) -> CorpusResult<WorkerChildOutput> {
    run_worker_child_with_options(WorkerChildRequest {
        executable,
        workers,
        schema_prefix,
        timeout: CHILD_TIMEOUT,
        hang: false,
    })
    .await
}

async fn run_worker_child_with_options(
    request: WorkerChildRequest<'_>,
) -> CorpusResult<WorkerChildOutput> {
    let WorkerChildRequest {
        executable,
        workers,
        schema_prefix,
        timeout,
        hang,
    } = request;
    let stdout_sink = tempfile::tempfile().map_err(|_| error("child-stdout-capture"))?;
    let stderr_sink = tempfile::tempfile().map_err(|_| error("child-stderr-capture"))?;
    let stdout_reader = stdout_sink
        .try_clone()
        .map_err(|_| error("child-stdout-clone"))?;
    let stderr_reader = stderr_sink
        .try_clone()
        .map_err(|_| error("child-stderr-clone"))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args([
            "--ignored",
            "--exact",
            "native_corpus::frozen_native_corpus_is_worker_deterministic_and_bounded",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_WORKERS_ENV, workers.to_string())
        .env(CHILD_SCHEMA_PREFIX_ENV, schema_prefix)
        .stdout(Stdio::from(stdout_sink))
        .stderr(Stdio::from(stderr_sink))
        .kill_on_drop(true);
    if hang {
        command.env(CHILD_HANG_ENV, "1");
    }
    let mut child = command.spawn().map_err(|_| error("spawn-worker-child"))?;
    let status = if let Ok(result) = tokio::time::timeout(timeout, child.wait()).await {
        result.map_err(|_| error("wait-worker-child"))?
    } else {
        let kill_result = child.start_kill();
        child
            .wait()
            .await
            .map_err(|_| error("reap-timed-out-worker-child"))?;
        let disposition = if kill_result.is_ok() {
            "was killed and reaped"
        } else {
            "exited during the kill request and was reaped"
        };
        let timeout_failure = format!(
            "worker child {workers} exceeded {} milliseconds, {disposition}",
            timeout.as_millis()
        );
        let cleanup = cleanup_child_schemas(schema_prefix).await;
        return Err(with_cleanup_result(&timeout_failure, &cleanup));
    };
    Ok(WorkerChildOutput {
        status,
        stdout: read_child_capture(stdout_reader, "child-stdout-read")?,
        stderr: read_child_capture(stderr_reader, "child-stderr-read")?,
    })
}

fn read_child_capture(mut capture: fs::File, failure: &'static str) -> CorpusResult<String> {
    capture
        .seek(SeekFrom::Start(0))
        .map_err(|_| error(failure))?;
    let mut output = String::new();
    capture
        .read_to_string(&mut output)
        .map_err(|_| error(failure))?;
    Ok(output)
}

fn child_schema_prefix(workers: u16) -> String {
    format!("cartograph_native_corpus_{}_w{workers}", process::id())
}

fn child_schema_name(prefix: &str, phase: &str, sample: usize) -> String {
    format!("{prefix}_{phase}_{sample}")
}

fn child_schema_names(prefix: &str) -> Vec<String> {
    let mut schemas = Vec::with_capacity(WARMUP_SAMPLES + MEASURED_SAMPLES);
    schemas.extend((0..WARMUP_SAMPLES).map(|sample| child_schema_name(prefix, "warmup", sample)));
    schemas.extend((0..MEASURED_SAMPLES).map(|sample| child_schema_name(prefix, "sample", sample)));
    schemas
}

fn validate_schema_prefix(prefix: &str) -> CorpusResult<()> {
    if prefix.is_empty()
        || prefix.len() > 48
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(error("child-schema-prefix-invalid"));
    }
    Ok(())
}

async fn open_cleanup_pool() -> CorpusResult<sqlx_postgres::PgPool> {
    let database_url =
        env::var(TEST_DATABASE_URL_ENV).map_err(|_| error("child-schema-cleanup-database-url"))?;
    let settings = DatabaseSettings::parse(&database_url, Some("2"), Some("10000"))
        .map_err(|_| error("child-schema-cleanup-settings"))?;
    cartograph_db::connect(&settings)
        .await
        .map_err(|_| error("child-schema-cleanup-connect"))
}

async fn create_schema_for_cleanup_test(schema: &str) -> CorpusResult<()> {
    let pool = open_cleanup_pool().await?;
    let statement = format!("CREATE SCHEMA \"{schema}\"");
    let result = query(AssertSqlSafe(statement))
        .execute(&pool)
        .await
        .map(|_| ())
        .map_err(|_| error("child-schema-cleanup-seed"));
    pool.close().await;
    result
}

async fn schema_exists(schema: &str) -> CorpusResult<bool> {
    let pool = open_cleanup_pool().await?;
    let statement = AssertSqlSafe(
        "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1) AS present".to_owned(),
    );
    let result = query(statement)
        .bind(schema)
        .fetch_one(&pool)
        .await
        .map_err(|_| error("child-schema-cleanup-probe"))
        .and_then(|row| {
            row.try_get("present")
                .map_err(|_| error("child-schema-cleanup-probe-decode"))
        });
    pool.close().await;
    result
}

async fn cleanup_child_schemas(prefix: &str) -> CorpusResult<()> {
    validate_schema_prefix(prefix)?;
    let pool = open_cleanup_pool().await?;
    let cleanup = async {
        let mut failed_schemas = Vec::new();
        for schema in child_schema_names(prefix) {
            let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
            if query(AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .is_err()
            {
                failed_schemas.push(schema);
            }
        }
        if failed_schemas.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "could not remove {} child schema(s)",
                failed_schemas.len()
            ))
        }
    };
    let result = match tokio::time::timeout(CHILD_CLEANUP_TIMEOUT, cleanup).await {
        Ok(result) => result,
        Err(_) => Err(error("child-schema-cleanup-timeout")),
    };
    pool.close().await;
    result
}

fn with_cleanup_result(primary: &str, cleanup: &CorpusResult<()>) -> String {
    match cleanup {
        Ok(()) => format!("{primary}; parent removed all child schemas"),
        Err(cleanup_failure) => {
            format!("{primary}; parent child-schema cleanup also failed: {cleanup_failure}")
        }
    }
}

async fn run_child(workers: &str) -> CorpusResult<()> {
    let workers = workers
        .parse::<u16>()
        .map_err(|_| error("child-workers-parse"))?;
    if !WORKER_MATRIX.contains(&workers) {
        return Err(error("child-workers-unsupported"));
    }
    let schema_prefix =
        env::var(CHILD_SCHEMA_PREFIX_ENV).map_err(|_| error("child-schema-prefix-missing"))?;
    validate_schema_prefix(&schema_prefix)?;
    if env::var_os(CHILD_HANG_ENV).is_some() {
        std::future::pending::<()>().await;
    }
    let corpus_fingerprint = corpus_fingerprint();
    if corpus_fingerprint != EXPECTED_CORPUS_FINGERPRINT {
        return Err(format!(
            "corpus fingerprint changed to {corpus_fingerprint}"
        ));
    }
    let directory = materialize_corpus()?;
    let sampler = RssSampler::start()?;
    for sample in 0..WARMUP_SAMPLES {
        let schema = child_schema_name(&schema_prefix, "warmup", sample);
        let observation = run_sample(directory.path(), workers, &schema).await?;
        validate_invariant(&observation.invariant)?;
    }
    let mut observations = Vec::with_capacity(MEASURED_SAMPLES);
    for sample in 0..MEASURED_SAMPLES {
        let schema = child_schema_name(&schema_prefix, "sample", sample);
        let observation = run_sample(directory.path(), workers, &schema).await?;
        validate_invariant(&observation.invariant)?;
        observations.push(observation);
    }
    let (baseline_rss_bytes, peak_rss_bytes, rss_successful_samples) = sampler.finish()?;
    let report = summarize_worker(WorkerSummaryInput {
        workers,
        observations,
        baseline_rss_bytes,
        peak_rss_bytes,
        rss_successful_samples,
    })?;
    let json = serde_json::to_string(&report).map_err(|_| error("worker-report-json"))?;
    println!("{CHILD_REPORT_PREFIX}{json}");
    Ok(())
}

async fn run_sample(root: &Path, workers: u16, schema: &str) -> CorpusResult<SampleObservation> {
    let fixture = open_fixture_with_schema(schema).await;
    let observation = run_open_sample(&fixture, root, workers).await;
    fixture.close().await;
    observation
}

async fn run_open_sample(
    fixture: &DatabaseFixture,
    root: &Path,
    workers: u16,
) -> CorpusResult<SampleObservation> {
    let source_root = SourceRoot::open(root).map_err(|_| error("source-root"))?;
    let staged = fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            SOURCE_REVISION,
            workers,
        ))
        .await
        .map_err(|_| error("begin-generation"))?;
    let generation_id = staged.generation_id().clone();
    let lease_target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), supervisor_config());
    let completed = execute_native_pipeline(NativeExecutionRequest {
        supervisor: &supervisor,
        lease_target: lease_target.clone(),
        source_root,
        staged,
        workers,
    })
    .await?;
    inspect_sample(SampleInspection {
        fixture,
        generation_id: &generation_id,
        lease_target: &lease_target,
        supervisor: &supervisor,
        completed,
    })
    .await
}

async fn execute_native_pipeline(
    request: NativeExecutionRequest<'_>,
) -> CorpusResult<CompletedSample> {
    let NativeExecutionRequest {
        supervisor,
        lease_target,
        source_root,
        staged,
        workers,
    } = request;
    let (report_sender, report_receiver) = oneshot::channel();
    let prepare_metrics = PrepareGenerationMetrics::new();
    let observed_prepare_metrics = prepare_metrics.clone();
    let supervised_pipeline_started = Instant::now();
    let current = supervisor
        .run(
            request_with_duration(lease_target.clone(), LEASE_DURATION),
            move |context| async move {
                let native_started = Instant::now();
                let native = build_native_generation(
                    &context.stages(),
                    source_root,
                    pipeline_config(workers),
                )
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                let native_nanos = duration_nanos(native_started.elapsed());
                let report = native.report();
                let (facts, _) = native.into_parts();
                report_sender
                    .send((report, native_nanos))
                    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                context
                    .prepare_generation(
                        GenerationContents::new(staged, facts).with_metrics(prepare_metrics),
                    )
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await;
    let supervised_pipeline_nanos = duration_nanos(supervised_pipeline_started.elapsed());
    let current = current
        .map_err(|failure| format!("native corpus supervised pipeline failed: {failure}"))?;
    let (native, native_nanos) = report_receiver
        .await
        .map_err(|_| error("native-report-missing"))?;
    let metrics = observed_prepare_metrics.snapshot();
    let copy_nanos = duration_nanos(metrics.copy_duration());
    let copy_tables_nanos = CopyTableNanos::from_snapshot(metrics);
    let relation_validation_nanos = duration_nanos(metrics.relation_validation_duration());
    if copy_nanos == 0
        || !copy_tables_nanos.complete()
        || copy_tables_nanos.total() > copy_nanos
        || relation_validation_nanos == 0
    {
        return Err(error("copy-duration-missing"));
    }
    Ok(CompletedSample {
        current,
        native,
        native_nanos,
        supervised_pipeline_nanos,
        copy_nanos,
        copy_tables_nanos,
        relation_validation_nanos,
    })
}

async fn inspect_sample(request: SampleInspection<'_>) -> CorpusResult<SampleObservation> {
    let SampleInspection {
        fixture,
        generation_id,
        lease_target,
        supervisor,
        completed,
    } = request;
    if supervisor.status().await.state() != SupervisorState::Completed {
        return Err(error("supervisor-not-completed"));
    }
    if fixture
        .database
        .lease_status(lease_target)
        .await
        .map_err(|_| error("lease-status"))?
        .is_some()
    {
        return Err(error("lease-not-released"));
    }
    let rows = row_counts(fixture, generation_id).await?;
    let edge_kinds = edge_kinds(fixture, generation_id).await?;
    let bm25_document_ids = read_bm25_document_ids(Bm25Inspection {
        fixture,
        current: completed.current.generation_id(),
        query_text: CORPUS_QUERY,
        limit: 5,
        operation: "bm25-query",
    })
    .await?;
    let tags_bm25_document_ids = read_bm25_document_ids(Bm25Inspection {
        fixture,
        current: completed.current.generation_id(),
        query_text: TAGS_CORPUS_QUERY,
        limit: 6,
        operation: "tags-bm25-query",
    })
    .await?;
    assert_tags_exact_lookup(fixture, completed.current.generation_id()).await?;
    assert_secret_sentinel_absent(fixture, generation_id).await?;
    Ok(SampleObservation {
        invariant: CorpusInvariant {
            corpus_fingerprint: corpus_fingerprint(),
            logical_digest: completed.current.content_digest().as_str().to_owned(),
            logical_digest_version: completed.current.digest_version().database_value(),
            rows,
            edge_kinds,
            bm25_document_ids,
            tags_bm25_document_ids,
            native: native_report(&completed.native),
        },
        native_nanos: completed.native_nanos,
        supervised_pipeline_nanos: completed.supervised_pipeline_nanos,
        copy_nanos: completed.copy_nanos,
        copy_tables_nanos: completed.copy_tables_nanos,
        relation_validation_nanos: completed.relation_validation_nanos,
    })
}

async fn assert_tags_exact_lookup(
    fixture: &DatabaseFixture,
    current: &GenerationId,
) -> CorpusResult<()> {
    let simple = fixture
        .database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&fixture.project, current),
            TAGS_CORPUS_QUERY,
            16,
        ))
        .await
        .map_err(|_| error("tags-simple-name-lookup"))?;
    if simple.len() != EXPECTED_TAGS_BM25_DOCUMENT_IDS.len()
        || simple
            .iter()
            .any(|symbol| !symbol.qualified_name().ends_with(TAGS_CORPUS_QUERY))
    {
        return Err(error("tags-simple-name-results"));
    }
    let full = fixture
        .database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&fixture.project, current),
            "T2Elixir.tagscanary",
            16,
        ))
        .await
        .map_err(|_| error("tags-full-name-lookup"))?;
    if full.len() != 1 || full[0].qualified_name() != "T2Elixir.tagscanary" {
        return Err(error("tags-full-name-results"));
    }
    let substring = fixture
        .database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&fixture.project, current),
            "scanary",
            16,
        ))
        .await
        .map_err(|_| error("tags-substring-name-lookup"))?;
    if substring.is_empty() {
        Ok(())
    } else {
        Err(error("tags-substring-false-positive"))
    }
}

async fn read_bm25_document_ids(request: Bm25Inspection<'_>) -> CorpusResult<Vec<String>> {
    let hits = request
        .fixture
        .database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(&request.fixture.project, request.current),
            request.query_text,
            request.limit,
        ))
        .await
        .map_err(|_| error(request.operation))?;
    if hits
        .iter()
        .any(|hit| hit.generation_id() != request.current)
    {
        return Err(error("bm25-noncurrent-generation"));
    }
    Ok(hits
        .iter()
        .map(|hit| hit.document_id().as_str().to_owned())
        .collect())
}

async fn assert_secret_sentinel_absent(
    fixture: &DatabaseFixture,
    generation: &GenerationId,
) -> CorpusResult<()> {
    let statement = format!(
        r#"WITH persisted(value) AS (
                SELECT concat_ws(' ', normalized_path, language, parse_status)
                FROM "{schema}"."files"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT concat_ws(' ', symbol_kind, qualified_name, signature)
                FROM "{schema}"."symbols"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT concat_ws(' ', edge_kind, provenance)
                FROM "{schema}"."edges"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT concat_ws(
                    ' ', reference_kind, reference_name, resolution_provenance, span_precision
                )
                FROM "{schema}"."references"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
                UNION ALL
                SELECT concat_ws(
                    ' ', path, language, document_kind, qualified_name,
                    code, natural_text, metadata::text
                )
                FROM "{schema}"."search_documents"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
            )
            SELECT count(*)::bigint AS matches
            FROM persisted
            WHERE position($3 in value) > 0"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation.as_str())
        .bind(LIVE_SECRET_SENTINEL)
        .fetch_one(&fixture.pool)
        .await
        .map_err(|_| error("secret-persistence-query"))?;
    let matches = row
        .try_get::<i64, _>("matches")
        .map_err(|_| error("secret-persistence-decode"))?;
    if matches != 0 {
        return Err(error("secret-sentinel-persisted"));
    }
    Ok(())
}

fn pipeline_config(workers: u16) -> NativePipelineConfig {
    let discovery = DiscoveryLimits::new(MAX_FILES, MAX_PATH_BYTES)
        .unwrap_or_else(|error| panic!("native corpus discovery limits were invalid: {error}"));
    let source = SourceLimits::new(MAX_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("native corpus source limits were invalid: {error}"));
    let retained = NativeRetainedLimits::new(MAX_MANIFEST_BYTES, MAX_GENERATION_BYTES)
        .unwrap_or_else(|error| panic!("native corpus retained limits were invalid: {error}"));
    let capacity = StageCapacity::new(usize::from(workers), usize::from(workers));
    let parallelism = NativePipelineParallelism::new(capacity, capacity)
        .unwrap_or_else(|error| panic!("native corpus parallelism was invalid: {error}"));
    let deadlines = NativePipelineDeadlines::new(ITEM_TIMEOUT, STAGE_TIMEOUT, CANCELLATION_GRACE)
        .unwrap_or_else(|error| panic!("native corpus deadlines were invalid: {error}"));
    NativePipelineConfig::new(
        NativePipelineLimits::new(discovery, source, retained),
        parallelism,
        deadlines,
    )
}

fn supervisor_config() -> SupervisorConfig {
    SupervisorConfig::new(OPERATION_TIMEOUT)
        .with_heartbeat_interval(HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(HEARTBEAT_TIMEOUT)
        .with_progress_timeout(PROGRESS_TIMEOUT)
        .with_cancellation_grace(CANCELLATION_GRACE)
        .with_copy_timeout(COPY_TIMEOUT)
        .with_max_worker_tasks(MAX_SUPERVISOR_TASKS)
        .with_max_worker_bytes(MAX_SUPERVISOR_BYTES)
}

async fn row_counts(
    fixture: &DatabaseFixture,
    generation: &GenerationId,
) -> CorpusResult<RowCounts> {
    let statement = format!(
        r#"SELECT
            (SELECT count(*) FROM "{schema}"."files"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS files,
            (SELECT count(*) FROM "{schema}"."symbols"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS symbols,
            (SELECT count(*) FROM "{schema}"."edges"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS edges,
            (SELECT count(*) FROM "{schema}"."references"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS references,
            (SELECT count(*) FROM "{schema}"."numerical_sites"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS numerical_sites,
            (SELECT count(*) FROM "{schema}"."search_documents"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS documents"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation.as_str())
        .fetch_one(&fixture.pool)
        .await
        .map_err(|_| error("row-count-query"))?;
    Ok(RowCounts {
        files: row
            .try_get("files")
            .map_err(|_| error("row-count-decode"))?,
        symbols: row
            .try_get("symbols")
            .map_err(|_| error("row-count-decode"))?,
        edges: row
            .try_get("edges")
            .map_err(|_| error("row-count-decode"))?,
        references: row
            .try_get("references")
            .map_err(|_| error("row-count-decode"))?,
        numerical_sites: row
            .try_get("numerical_sites")
            .map_err(|_| error("row-count-decode"))?,
        documents: row
            .try_get("documents")
            .map_err(|_| error("row-count-decode"))?,
    })
}

async fn edge_kinds(
    fixture: &DatabaseFixture,
    generation: &GenerationId,
) -> CorpusResult<Vec<String>> {
    let statement = format!(
        r#"SELECT DISTINCT edge_kind
            FROM "{}"."edges"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
            ORDER BY edge_kind"#,
        fixture.schema,
    );
    let rows = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation.as_str())
        .fetch_all(&fixture.pool)
        .await
        .map_err(|_| error("edge-kind-query"))?;
    rows.into_iter()
        .map(|row| {
            row.try_get("edge_kind")
                .map_err(|_| error("edge-kind-decode"))
        })
        .collect()
}

fn validate_invariant(invariant: &CorpusInvariant) -> CorpusResult<()> {
    if invariant.corpus_fingerprint != EXPECTED_CORPUS_FINGERPRINT {
        return Err(format!(
            "corpus fingerprint changed to {}",
            invariant.corpus_fingerprint
        ));
    }
    if invariant.logical_digest != EXPECTED_LOGICAL_DIGEST {
        return Err(format!(
            "logical digest changed to {}; rows={:?}; edge_kinds={:?}; bm25={:?}; tags_bm25={:?}; native={:?}",
            invariant.logical_digest,
            invariant.rows,
            invariant.edge_kinds,
            invariant.bm25_document_ids,
            invariant.tags_bm25_document_ids,
            invariant.native,
        ));
    }
    if invariant.logical_digest_version != GenerationDigestVersion::CURRENT.database_value() {
        return Err(error("logical-digest-version"));
    }
    if invariant.rows != expected_row_counts() {
        return Err(format!("row counts changed to {:?}", invariant.rows));
    }
    if invariant.edge_kinds != EXPECTED_EDGE_KINDS {
        return Err(format!("edge kinds changed to {:?}", invariant.edge_kinds));
    }
    if invariant.bm25_document_ids != EXPECTED_BM25_DOCUMENT_IDS {
        return Err(format!(
            "BM25 hits changed to {:?}",
            invariant.bm25_document_ids
        ));
    }
    if invariant.tags_bm25_document_ids != EXPECTED_TAGS_BM25_DOCUMENT_IDS {
        return Err(format!(
            "tags BM25 hits changed to {:?}",
            invariant.tags_bm25_document_ids
        ));
    }
    if invariant.native != expected_native_report() {
        return Err(format!("native report changed to {:?}", invariant.native));
    }
    Ok(())
}

const fn expected_row_counts() -> RowCounts {
    RowCounts {
        files: EXPECTED_FILES,
        symbols: EXPECTED_SYMBOLS,
        edges: EXPECTED_EDGES,
        references: EXPECTED_REFERENCES,
        numerical_sites: EXPECTED_NUMERICAL_SITES,
        documents: EXPECTED_DOCUMENTS,
    }
}

const fn expected_native_report() -> NativeReport {
    NativeReport {
        discovered_files: EXPECTED_FILES as u64,
        source_bytes: EXPECTED_SOURCE_BYTES,
        symbols: EXPECTED_SYMBOLS as u64,
        numerical_sites: EXPECTED_NUMERICAL_SITES as u64,
        resolved_references: EXPECTED_RESOLVED_REFERENCES,
        unresolved_references: EXPECTED_UNRESOLVED_REFERENCES,
        diagnostics: 0,
        modeled_generation_bytes: EXPECTED_MODELED_GENERATION_BYTES,
        resolve_high_water_bytes: EXPECTED_RESOLVE_HIGH_WATER_BYTES,
        validation_high_water_bytes: EXPECTED_VALIDATION_HIGH_WATER_BYTES,
    }
}

fn summarize_worker(input: WorkerSummaryInput) -> CorpusResult<WorkerReport> {
    let WorkerSummaryInput {
        workers,
        observations,
        baseline_rss_bytes,
        peak_rss_bytes,
        rss_successful_samples,
    } = input;
    let invariant = observations
        .first()
        .map(|observation| observation.invariant.clone())
        .ok_or_else(|| error("empty-observations"))?;
    if observations
        .iter()
        .any(|observation| observation.invariant != invariant)
    {
        return Err(error("same-worker-invariant-drift"));
    }
    let supervised_pipeline = observations
        .iter()
        .map(|observation| observation.supervised_pipeline_nanos)
        .collect::<Vec<_>>();
    let native = observations
        .iter()
        .map(|observation| observation.native_nanos)
        .collect::<Vec<_>>();
    let copy = observations
        .iter()
        .map(|observation| observation.copy_nanos)
        .collect::<Vec<_>>();
    let relation_validation = observations
        .iter()
        .map(|observation| observation.relation_validation_nanos)
        .collect::<Vec<_>>();
    let copy_tables_p50_nanos = copy_table_percentile(&observations, MEDIAN_PERCENTILE)?;
    let copy_tables_p95_nanos = copy_table_percentile(&observations, TAIL_PERCENTILE)?;
    Ok(WorkerReport {
        workers,
        invariant,
        baseline_rss_bytes,
        peak_rss_bytes,
        peak_rss_delta_bytes: peak_rss_bytes.saturating_sub(baseline_rss_bytes),
        rss_successful_samples,
        native_p50_nanos: percentile(&native, MEDIAN_PERCENTILE)?,
        native_p95_nanos: percentile(&native, TAIL_PERCENTILE)?,
        supervised_pipeline_p50_nanos: percentile(&supervised_pipeline, MEDIAN_PERCENTILE)?,
        supervised_pipeline_p95_nanos: percentile(&supervised_pipeline, TAIL_PERCENTILE)?,
        copy_p50_nanos: percentile(&copy, MEDIAN_PERCENTILE)?,
        copy_p95_nanos: percentile(&copy, TAIL_PERCENTILE)?,
        copy_tables_p50_nanos,
        copy_tables_p95_nanos,
        relation_validation_p50_nanos: percentile(&relation_validation, MEDIAN_PERCENTILE)?,
        relation_validation_p95_nanos: percentile(&relation_validation, TAIL_PERCENTILE)?,
        samples: observations
            .into_iter()
            .enumerate()
            .map(|(sample, observation)| SampleReport {
                sample,
                native_nanos: observation.native_nanos,
                supervised_pipeline_nanos: observation.supervised_pipeline_nanos,
                copy_nanos: observation.copy_nanos,
                copy_tables_nanos: observation.copy_tables_nanos,
                relation_validation_nanos: observation.relation_validation_nanos,
            })
            .collect(),
    })
}

fn copy_table_percentile(
    observations: &[SampleObservation],
    requested: usize,
) -> CorpusResult<CopyTableNanos> {
    let values = |select: fn(&CopyTableNanos) -> u64| {
        observations
            .iter()
            .map(|observation| select(&observation.copy_tables_nanos))
            .collect::<Vec<_>>()
    };
    Ok(CopyTableNanos {
        files: percentile(&values(|tables| tables.files), requested)?,
        symbols: percentile(&values(|tables| tables.symbols), requested)?,
        edges: percentile(&values(|tables| tables.edges), requested)?,
        references: percentile(&values(|tables| tables.references), requested)?,
        numerical_sites: percentile(&values(|tables| tables.numerical_sites), requested)?,
        documents: percentile(&values(|tables| tables.documents), requested)?,
    })
}

fn materialize_corpus() -> CorpusResult<tempfile::TempDir> {
    let directory = tempfile::tempdir().map_err(|_| error("corpus-tempdir"))?;
    fs::create_dir(directory.path().join(".git")).map_err(|_| error("corpus-git-dir"))?;
    for entry in CORPUS {
        let destination = directory.path().join(entry.path);
        let parent = destination.parent().ok_or_else(|| error("corpus-parent"))?;
        fs::create_dir_all(parent).map_err(|_| error("corpus-create-dir"))?;
        fs::write(destination, entry.source).map_err(|_| error("corpus-write"))?;
    }
    Ok(directory)
}

fn corpus_fingerprint() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CORPUS_FINGERPRINT_DOMAIN);
    hash_usize(&mut hasher, CORPUS.len());
    for entry in CORPUS {
        hash_bytes(&mut hasher, entry.path.as_bytes());
        hash_bytes(&mut hasher, entry.source.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hash_usize(hasher, value.len());
    hasher.update(value);
}

fn hash_usize(hasher: &mut blake3::Hasher, value: usize) {
    hasher.update(&u64::try_from(value).unwrap_or(u64::MAX).to_le_bytes());
}

fn native_report(report: &cartograph_indexer::NativePipelineReport) -> NativeReport {
    NativeReport {
        discovered_files: report.discovered_files(),
        source_bytes: report.source_bytes(),
        symbols: report.symbols(),
        numerical_sites: report.numerical_sites(),
        resolved_references: report.resolved_references(),
        unresolved_references: report.unresolved_references(),
        diagnostics: report.diagnostics(),
        modeled_generation_bytes: report.modeled_generation_bytes(),
        resolve_high_water_bytes: report.resolve_high_water_bytes(),
        validation_high_water_bytes: report.validation_high_water_bytes(),
    }
}

fn percentile(values: &[u64], percentile: usize) -> CorpusResult<u64> {
    if values.is_empty() {
        return Err(error("empty-percentile"));
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let rank = values
        .len()
        .checked_mul(percentile)
        .ok_or_else(|| error("percentile-overflow"))?
        .div_ceil(100)
        .saturating_sub(1)
        .min(values.len().saturating_sub(1));
    values
        .get(rank)
        .copied()
        .ok_or_else(|| error("percentile-rank"))
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl RssSampler {
    fn start() -> CorpusResult<Self> {
        let baseline = current_rss_bytes()?;
        let peak = Arc::new(AtomicU64::new(baseline));
        let successful_samples = Arc::new(AtomicU64::new(0));
        let failed_samples = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_peak = peak.clone();
        let worker_successful_samples = successful_samples.clone();
        let worker_failed_samples = failed_samples.clone();
        let worker_stop = stop.clone();
        let handle = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                record_rss_sample(
                    &worker_peak,
                    &worker_successful_samples,
                    &worker_failed_samples,
                );
                thread::sleep(RSS_SAMPLE_INTERVAL);
            }
            record_rss_sample(
                &worker_peak,
                &worker_successful_samples,
                &worker_failed_samples,
            );
        });
        Ok(Self {
            baseline,
            peak,
            successful_samples,
            failed_samples,
            stop,
            handle: Some(handle),
        })
    }

    fn finish(mut self) -> CorpusResult<(u64, u64, u64)> {
        self.stop.store(true, Ordering::Release);
        let handle = self
            .handle
            .take()
            .ok_or_else(|| error("rss-sampler-handle"))?;
        handle.join().map_err(|_| error("rss-sampler-join"))?;
        let failed_samples = self.failed_samples.load(Ordering::Acquire);
        if failed_samples != 0 {
            return Err(format!("rss sampling failed {failed_samples} time(s)"));
        }
        let successful_samples = self.successful_samples.load(Ordering::Acquire);
        if successful_samples == 0 {
            return Err(error("rss-sampler-no-successful-samples"));
        }
        Ok((
            self.baseline,
            self.peak.load(Ordering::Acquire),
            successful_samples,
        ))
    }
}

fn record_rss_sample(peak: &AtomicU64, successful: &AtomicU64, failed: &AtomicU64) {
    match current_rss_bytes() {
        Ok(current) => {
            peak.fetch_max(current, Ordering::AcqRel);
            successful.fetch_add(1, Ordering::AcqRel);
        }
        Err(_) => {
            failed.fetch_add(1, Ordering::AcqRel);
        }
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> CorpusResult<u64> {
    let status = fs::read_to_string("/proc/self/status").map_err(|_| error("rss-read"))?;
    let kibibytes = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| error("rss-decode"))?;
    kibibytes
        .checked_mul(1024)
        .ok_or_else(|| error("rss-overflow"))
}

#[cfg(target_os = "macos")]
fn current_rss_bytes() -> CorpusResult<u64> {
    let output = ProcessCommand::new("ps")
        .args(["-o", "rss=", "-p", &process::id().to_string()])
        .output()
        .map_err(|_| error("rss-command"))?;
    if !output.status.success() {
        return Err(error("rss-command-status"));
    }
    let kibibytes = String::from_utf8(output.stdout)
        .map_err(|_| error("rss-command-utf8"))?
        .trim()
        .parse::<u64>()
        .map_err(|_| error("rss-command-decode"))?;
    kibibytes
        .checked_mul(1024)
        .ok_or_else(|| error("rss-overflow"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_rss_bytes() -> CorpusResult<u64> {
    Err(error("rss-platform-unsupported"))
}

fn tail(value: &str) -> &str {
    let start = value.len().saturating_sub(4096);
    value.get(start..).unwrap_or(value)
}

fn error(operation: &'static str) -> String {
    format!("native corpus operation failed: {operation}")
}
