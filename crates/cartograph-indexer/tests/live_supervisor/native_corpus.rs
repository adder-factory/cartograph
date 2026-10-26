//! Frozen real-source extraction, native persistence, privacy, and worker parity.

use cartograph_db::{
    CurrentGenerationLookup, NativeDatabase, SearchQuery,
    rocks::{
        AsyncProjectStore, Collection, GraphBase, GraphPolicy, GraphRevision, ReadBudget,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
};
use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};
use cartograph_extract::{DiscoveryLimits, SourceLimits, SourceRoot};
use cartograph_indexer::{
    NativeGenerationBuild, NativePipelineConfig, NativePipelineDeadlines, NativePipelineLimits,
    NativePipelineParallelism, NativeRetainedLimits, StageCapacity, build_native_rocks_graph_owned,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    time::{Duration, Instant},
};

type CorpusResult<T = ()> = Result<T, String>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "long native RocksDB corpus matrix; invoked explicitly by the native live gate"]
async fn frozen_native_corpus_is_worker_deterministic_and_bounded() {
    let source = materialize_corpus().unwrap_or_else(|error| panic!("{error}"));
    let mut observations = Vec::new();
    for workers in WORKER_MATRIX {
        for sample in 0..WARMUP_SAMPLES + MEASURED_SAMPLES {
            let observation = run_sample(source.path(), workers)
                .await
                .unwrap_or_else(|error| panic!("workers={workers}: {error}"));
            if sample >= WARMUP_SAMPLES {
                observations.push(observation);
            }
        }
    }
    println!(
        "NATIVE_CORPUS_MATRIX {}",
        serde_json::json!({"corpus":CORPUS_NAME,
        "timingsAreGates":false, "observations":observations})
    );
}

async fn run_sample(root: &std::path::Path, workers: u16) -> CorpusResult<serde_json::Value> {
    let directory = tempfile::tempdir().map_err(|_| error("store-tempdir"))?;
    let request = StorageRequest::new(OPERATION_TIMEOUT).map_err(|_| error("request"))?;
    let (runtime, store, database) = open_store(directory.path(), &request).await?;
    let started = Instant::now();
    let outcome = async {
        let (input, native) = build_native_rocks_graph_owned(
            NativeGenerationBuild::new(
                SourceRoot::open(root).map_err(|_| error("source"))?,
                pipeline_config(workers),
            ),
            GraphRevision {
                generation_id: GenerationId::from_uuid_v8([1; 16]),
                source_revision: ContentDigest::from_bytes(
                    *blake3::hash(SOURCE_REVISION.as_bytes()).as_bytes(),
                ),
                base: GraphBase::Empty,
            },
            GraphPolicy::default(),
            &request,
        )
        .await
        .map_err(|failure| format!("native pipeline: {failure}"))?;
        let extraction = started.elapsed();
        let published = store
            .publish_graph(input, &request)
            .await
            .map_err(|failure| format!("publish: {failure}"))?;
        let generation = published.revision.generation_id.clone();
        let (rows, kinds) = store
            .read(
                Some(generation.clone()),
                ReadBudget::new(200_000, 128 * 1_048_576, Duration::from_secs(30))
                    .map_err(|_| error("budget"))?,
                &request,
                inspect_graph,
            )
            .await
            .map_err(|failure| format!("native inspection: {failure}"))?;
        let mut results = Vec::new();
        for (query, limit) in [(CORPUS_QUERY, 5), (TAGS_CORPUS_QUERY, 6)] {
            let hits = database
                .search_current_code(SearchQuery::new(
                    CurrentGenerationLookup::new(database.project_id(), &generation),
                    query,
                    limit,
                ))
                .await
                .map_err(|failure| format!("BM25: {failure}"))?;
            results.push(
                hits.into_iter()
                    .map(|hit| hit.document_id().to_string())
                    .collect::<Vec<_>>(),
            );
        }
        let invariant = CorpusInvariant {
            corpus_fingerprint: corpus_fingerprint(),
            logical_digest: published
                .revision
                .content_digest
                .as_ref()
                .ok_or_else(|| error("missing-digest"))?
                .to_string(),
            logical_digest_version: published
                .revision
                .digest_version
                .ok_or_else(|| error("missing-digest-version"))?
                .database_value(),
            rows,
            edge_kinds: kinds,
            bm25_document_ids: results.remove(0),
            tags_bm25_document_ids: results.remove(0),
            native: native_report(&native),
        };
        validate_invariant(&invariant)?;
        Ok(
            serde_json::json!({"workers":workers,"extractionMillis":extraction.as_secs_f64()*1000.0,
            "elapsedMillis":started.elapsed().as_secs_f64()*1000.0,"invariant":invariant,
            "usage":store.storage_usage(&request).await.map_err(|_|error("usage"))?}),
        )
    }
    .await;
    let cleanup = runtime
        .shutdown(
            &StorageRequest::new(Duration::from_secs(30)).map_err(|_| error("cleanup-request"))?,
        )
        .await;
    match (outcome, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(failure)) => Err(failure.to_string()),
        (Err(error), Err(failure)) => Err(format!("{error}; cleanup: {failure}")),
    }
}

async fn open_store(
    directory: &std::path::Path,
    request: &StorageRequest,
) -> CorpusResult<(StorageRuntime, AsyncProjectStore, NativeDatabase)> {
    let runtime = StorageRuntime::new(WorkerPolicy {
        pending_bytes: 512 * 1_048_576,
        ..WorkerPolicy::default()
    })
    .map_err(|_| error("runtime"))?;
    let identity = ContentDigest::from_bytes([9; 32]);
    let store = AsyncProjectStore::open(
        &runtime,
        directory.to_owned(),
        identity.clone(),
        StorePolicy::default(),
        request,
    )
    .await
    .map_err(|_| error("open"))?;
    let database = NativeDatabase::new(store.clone(), &identity);
    Ok((runtime, store, database))
}

fn inspect_graph(
    snapshot: &cartograph_db::rocks::RevisionSnapshot<'_>,
    budget: &mut ReadBudget,
) -> Result<(RowCounts, Vec<String>), cartograph_db::rocks::RocksError> {
    let graph = snapshot.graph(budget)?;
    let counts = graph.info();
    let rows = RowCounts {
        files: i64::try_from(counts.files).map_err(|_| cartograph_db::rocks::RocksError::Format)?,
        symbols: i64::try_from(counts.symbols)
            .map_err(|_| cartograph_db::rocks::RocksError::Format)?,
        edges: i64::try_from(counts.edges).map_err(|_| cartograph_db::rocks::RocksError::Format)?,
        references: i64::try_from(counts.references)
            .map_err(|_| cartograph_db::rocks::RocksError::Format)?,
        numerical_sites: i64::try_from(counts.numerical_sites)
            .map_err(|_| cartograph_db::rocks::RocksError::Format)?,
        documents: i64::try_from(counts.documents)
            .map_err(|_| cartograph_db::rocks::RocksError::Format)?,
    };
    let mut kinds = BTreeSet::new();
    let mut cursor = None;
    loop {
        let page =
            snapshot.scan_prefix(Collection::Graph, b"cg1/", cursor.as_ref(), 128, budget)?;
        for (key, bytes) in page.rows {
            if bytes
                .windows(LIVE_SECRET_SENTINEL.len())
                .any(|part| part == LIVE_SECRET_SENTINEL.as_bytes())
            {
                return Err(cartograph_db::rocks::RocksError::Corrupt {
                    field: "persisted-secret",
                });
            }
            if key.as_bytes().starts_with(b"cg1/e/") {
                let kind_index = key
                    .as_bytes()
                    .get(b"cg1/e/".len() + 32)
                    .ok_or(cartograph_db::rocks::RocksError::Format)?;
                let kind = cartograph_domain::EdgeKind::ALL
                    .get(usize::from(*kind_index))
                    .ok_or(cartograph_db::rocks::RocksError::Format)?;
                kinds.insert(kind.as_str().to_owned());
            }
        }
        cursor = page.next_after;
        if cursor.is_none() {
            break;
        }
    }
    Ok((rows, kinds.into_iter().collect()))
}

const CORPUS_NAME: &str = "cartograph-v1-real-typescript-v1";

const CORPUS_FINGERPRINT_DOMAIN: &[u8] = b"cartograph-v2-native-real-corpus-v1";

const EXPECTED_CORPUS_FINGERPRINT: &str =
    "ab91088c482ed36d31759382283342654ce6958be4e601429b8181da531c5fc1";

// V26 reassigns 2,381 executable initializer references to containing owners.
// An independent edge reconstruction coalesces 28 edges (11,278 -> 11,250);
// targets, declarations, documents, source bytes and BM25 identities stay fixed.
// Independent wire encoding reproduces the old V25 digest before deriving V26.
const EXPECTED_LOGICAL_DIGEST: &str =
    "c75fca59e7a4a9addcd263d889d29e3b965c479bff35879e430acedf2f0b56a8";

// Native scoring uses k1=1.2, b=.75 and name/code/natural weights 4/1/2.
// Independently checked corpus statistics: N=6362, weighted tokens=348835,
// df(detect,detectsecretshandling,handling,secrets)=(60,11,14,21).
// corpus: length=28, tf=(4,4,4,4), score=42.05470128199791;
// score: length=42, tf=(5,5,5,5), score=41.774553035347694.
// This intentionally differs from ParadeDB's per-field fifth result. The
// source fingerprint, all extracted facts and the six tag results are fixed.
const EXPECTED_BM25_DOCUMENT_IDS: [&str; 5] = [
    "5471dbfc-3ba3-87dd-8861-1ce1dd51ed32",
    "78f1eb97-24b2-8a80-ad44-6dd679456592",
    "a5008cb5-184d-819e-a6bf-54c4f6207da0",
    "b7c9628b-c28f-80cd-9bf6-3cfbce849c70",
    "29f064c9-f931-89d3-8eb8-d05e7906749f",
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

const EXPECTED_SYMBOLS: i64 = 6_362;

const EXPECTED_EDGES: i64 = 11_250;

const EXPECTED_REFERENCES: i64 = 21_320;

const EXPECTED_NUMERICAL_SITES: i64 = 0;

const EXPECTED_DOCUMENTS: i64 = 6_362;

const EXPECTED_EDGE_KINDS: [&str; 12] = [
    "calls",
    "contains",
    "def_use",
    "exports",
    "extends",
    "field_access",
    "implements",
    "imports",
    "instantiates",
    "references",
    "returns",
    "type_of",
];

const EXPECTED_SOURCE_BYTES: u64 = 1_052_564;

const EXPECTED_RESOLVED_REFERENCES: u64 = 7_918;

const EXPECTED_UNRESOLVED_REFERENCES: u64 = 13_414;

// Derived allocation pins include spare capacity retained by geometrically
// grown canonical metadata buffers. Corpus bytes and logical facts are fixed.
const EXPECTED_MODELED_GENERATION_BYTES: u64 = 25_638_779;

const EXPECTED_RESOLVE_HIGH_WATER_BYTES: u64 = 127_405_291;

const EXPECTED_VALIDATION_HIGH_WATER_BYTES: u64 = 153_392_355;

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

const ITEM_TIMEOUT: Duration = Duration::from_secs(30);

const STAGE_TIMEOUT: Duration = Duration::from_secs(45);

const OPERATION_TIMEOUT: Duration = Duration::from_mins(2);

const CANCELLATION_GRACE: Duration = Duration::from_secs(5);

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

fn error(operation: &'static str) -> String {
    format!("native corpus operation failed: {operation}")
}
