//! Frozen Rust workload exercised through the native worker and `RocksDB` paths.

#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;
#[path = "index_scaling/fixture.rs"]
mod fixture;

use cartograph_db::{
    CurrentGenerationLookup, EdgeInput, FileInput, GenerationFacts, GenerationValidationLimits,
    NativeDatabase, ReferenceInput, SearchDocumentInput, SearchQuery, SymbolInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus, GenerationId,
    SymbolExecutionFlags, SymbolExportFlags, SymbolId,
};
use cartograph_indexer::StageItemFailure;
use fixture::{
    BENCHMARK_CONFIG, FixtureInput, FrozenFixture, SamplePlan, ValidationBudget, WorkloadSize,
    qualified_name,
};
use std::{
    error::Error,
    time::{Duration, Instant},
};

type BenchResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
// V25 identity for the frozen fixture; its inputs and fingerprint stay fixed.
const EXPECTED_LOGICAL_DIGEST: &str =
    "8fded6e120574bf49b9a58d49079a1e3c90ad03f07b8d057ca1fb9003fdbbc3c";

#[tokio::main]
async fn main() -> BenchResult {
    let fixture = FrozenFixture::build(&BENCHMARK_CONFIG)?;
    let mut observations = Vec::new();
    for workers in fixture.config.workers {
        for sample in 0..fixture.config.workload[WorkloadSize::WarmupSamples as usize]
            + fixture.config.workload[WorkloadSize::MeasuredSamples as usize]
        {
            let observation = sample_run(&fixture, workers, sample).await?;
            if sample >= fixture.config.workload[WorkloadSize::WarmupSamples as usize] {
                observations.push(observation);
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"storage":"rocksdb", "engine": "11.8.1",
        "fixtureFingerprint": fixture.fixture_fingerprint, "sourceDigest": fixture.source_digest,
        "logicalDigest": EXPECTED_LOGICAL_DIGEST, "sourceBytes":fixture.source_bytes,
        "timingsAreGates":false, "observations":observations})
    );
    Ok(())
}

async fn sample_run(
    fixture: &FrozenFixture,
    workers: u16,
    sample: usize,
) -> BenchResult<serde_json::Value> {
    let request = StorageRequest::new(fixture.config.deadlines.operation)?;
    let plan = SamplePlan::build(fixture, workers)?;
    let plan_report = serde_json::json!({"workers":plan.worker_count, "queueItems":plan.queue_items,
        "window":plan.window, "reservedBytes":plan.maximum_reserved_bytes, "items":plan.inputs.len(),
        "stageDeadlineMillis":plan.stage_deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis()});
    drop(plan);
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy {
        threads: u8::try_from(workers.max(2))?,
        pending_jobs: 32,
        pending_bytes: 256 * 1_048_576,
    })?;
    let identity = ContentDigest::from_bytes([7; 32]);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request,
    )
    .await?;
    let database = NativeDatabase::new(store.clone(), &identity);
    let started = Instant::now();
    let outcome = async {
        let facts = parallel_facts(fixture, workers, &request).await?;
        let extraction = started.elapsed();
        let input = tokio::task::block_in_place(|| -> BenchResult<_> {
            let (facts, _) = validate_generation_facts(facts, GenerationValidationLimits::new(
                fixture.config.validation_bytes[ValidationBudget::Output as usize],
                fixture.config.validation_bytes[ValidationBudget::Working as usize])?, || request.check().is_err())?;
            if facts.digest().as_str() != EXPECTED_LOGICAL_DIGEST { return Err(format!("frozen logical digest changed: {}", facts.digest()).into()); }
            let counts = (&fixture.expected_rows, facts.files().len(), facts.symbols().len(), facts.edges().len(), facts.references().len(), facts.numerical_sites().len(), facts.documents().len());
            if counts.0.files != i64::try_from(counts.1)? || counts.0.symbols != i64::try_from(counts.2)?
                || counts.0.edges != i64::try_from(counts.3)? || counts.0.references != i64::try_from(counts.4)?
                || counts.0.numerical_sites != i64::try_from(counts.5)? || counts.0.documents != i64::try_from(counts.6)? { return Err("frozen native row counts changed".into()); }
            Ok(GraphPublicationInput::new(GraphRevision {generation_id:GenerationId::from_uuid_v8([1;16]),
                source_revision:ContentDigest::parse(&fixture.source_digest)?, base:GraphBase::Empty}, facts, GraphPolicy::default(), &request)?)
        })?;
        let published = store.publish_graph(input, &request).await?;
        let elapsed = started.elapsed();
        let hits = database.search_current_code(SearchQuery::new(CurrentGenerationLookup::new(database.project_id(), &published.revision.generation_id), fixture.config.bm25_query, 10)).await?;
        if hits.first().map(cartograph_db::SearchHit::document_id) != Some(&fixture.needle_document_id) { return Err("frozen native BM25 needle ranking changed".into()); }
        let usage = store.storage_usage(&request).await?;
        Ok(serde_json::json!({"workers":workers,"sample":sample,"plan":plan_report,
            "extractionMillis":extraction.as_secs_f64()*1000.0,"publicationMillis":elapsed.as_secs_f64()*1000.0,
            "rows":fixture.expected_rows,"usage":usage,"write":published.report}))
    }.await;
    let cleanup = runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await;
    match (outcome, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(cleanup)) => {
            Err(format!("{error}; native worker cleanup failed: {cleanup}").into())
        }
    }
}

async fn parallel_facts(
    fixture: &FrozenFixture,
    workers: u16,
    request: &StorageRequest,
) -> BenchResult<GenerationFacts> {
    let mut facts = GenerationFacts::default();
    let mut tasks = tokio::task::JoinSet::new();
    let mut primary = None;
    for chunk in fixture.inputs.chunks(usize::from(workers)) {
        request.check()?;
        for input in chunk.iter().cloned() {
            let request = request.clone();
            tasks.spawn_blocking(move || build_fact_bundle(input, &request));
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(bundle)) if primary.is_none() => {
                    append_bundle(&mut facts, bundle);
                }
                Ok(Ok(_)) => {}
                _ => {
                    request.cancel();
                    primary = Some("native benchmark CPU work failed");
                }
            }
        }
        if let Some(error) = primary {
            return Err(error.into());
        }
    }
    Ok(facts)
}

struct FactBundle {
    file: FileInput,
    symbol: SymbolInput,
    edge: Option<EdgeInput>,
    reference: Option<ReferenceInput>,
    document: SearchDocumentInput,
}

fn append_bundle(facts: &mut GenerationFacts, bundle: FactBundle) {
    facts.files.push(bundle.file);
    facts.symbols.push(bundle.symbol);
    facts.documents.push(bundle.document);
    if let Some(edge) = bundle.edge {
        facts.edges.push(edge);
    }
    if let Some(reference) = bundle.reference {
        facts.references.push(reference);
    }
}

fn build_fact_bundle(
    input: FixtureInput,
    request: &StorageRequest,
) -> Result<FactBundle, StageItemFailure> {
    let path = fixture::normalized_path(input.index);
    let index = input.index;
    let name = qualified_name(index);
    let signature = format!("function {name}(input: number): number");
    let content_hash = blake3::hash(input.source.as_bytes());
    let mut structural_hash = content_hash;
    for round in 0..input.hash_rounds {
        request.check().map_err(|_| StageItemFailure)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(structural_hash.as_bytes());
        hasher.update(input.source.as_bytes());
        hasher.update(&round.to_le_bytes());
        structural_hash = hasher.finalize();
    }
    let file = file_id(index)?;
    let symbol = symbol_id(index)?;
    let source_bytes = u64::try_from(input.source.len()).map_err(|_| StageItemFailure)?;
    let end_line = u32::try_from(input.source_repetitions).map_err(|_| StageItemFailure)?;
    let previous_symbol = index.checked_sub(1).map(symbol_id).transpose()?;
    Ok(FactBundle {
        file: FileInput {
            file_id: file.clone(),
            normalized_path: path.clone(),
            language: "typescript".to_owned(),
            content_hash: ContentDigest::from_bytes(*content_hash.as_bytes()),
            byte_size: source_bytes,
            parse_status: FileParseStatus::Parsed,
        },
        symbol: SymbolInput {
            symbol_id: symbol.clone(),
            file_id: file.clone(),
            symbol_kind: "function".to_owned(),
            qualified_name: name.clone(),
            signature: signature.clone(),
            start_byte: 0,
            end_byte: source_bytes,
            start_line: 1,
            end_line,
            structural_digest: ContentDigest::from_bytes(*structural_hash.as_bytes()),
            visibility: None,
            export: SymbolExportFlags::named(true),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        },
        edge: previous_symbol.clone().map(|target_symbol_id| EdgeInput {
            source_symbol_id: symbol.clone(),
            target_symbol_id,
            kind: EdgeKind::Calls,
            confidence: 1.0,
            provenance: "rust-scaling-fixture".to_owned(),
            site_count: 1,
        }),
        reference: previous_symbol.map(|target_symbol_id| ReferenceInput {
            file_id: file.clone(),
            owner_symbol_id: Some(symbol.clone()),
            target_symbol_id: Some(target_symbol_id),
            reference_name: "previous_benchmark_symbol".to_owned(),
            reference_kind: "call".to_owned(),
            start_byte: 0,
            end_byte: u64::try_from(signature.len()).unwrap_or(u64::MAX),
            confidence: 1.0,
            resolution_provenance: "rust-scaling-fixture".to_owned(),
            site_count: 1,
            span_precision: cartograph_db::ReferenceSpanPrecision::Exact,
        }),
        document: SearchDocumentInput {
            document_id: document_id(index).map_err(|_| StageItemFailure)?,
            file_id: Some(file),
            symbol_id: Some(symbol),
            path,
            language: "typescript".to_owned(),
            kind: DocumentKind::Symbol,
            qualified_name: name,
            code: input.source,
            natural_text: natural_text(index),
            metadata: serde_json::json!({
                "fixture": input.fixture_name,
                "index": index,
            }),
        },
    })
}

fn natural_text(index: usize) -> String {
    if index == 0 {
        "unique needle cartograph benchmark evidence".to_owned()
    } else {
        format!("deterministic scaling fixture symbol {index:04}")
    }
}

fn file_id(index: usize) -> Result<FileId, StageItemFailure> {
    FileId::parse(&stable_uuid(0x1000_0000, index)).map_err(|_| StageItemFailure)
}

fn symbol_id(index: usize) -> Result<SymbolId, StageItemFailure> {
    SymbolId::parse(&stable_uuid(0x2000_0000, index)).map_err(|_| StageItemFailure)
}

fn document_id(index: usize) -> Result<DocumentId, StageItemFailure> {
    DocumentId::parse(&stable_uuid(0x3000_0000, index)).map_err(|_| StageItemFailure)
}

fn stable_uuid(namespace: u32, index: usize) -> String {
    let value = index.saturating_add(1);
    format!("{namespace:08x}-0000-4000-8000-{value:012x}")
}
