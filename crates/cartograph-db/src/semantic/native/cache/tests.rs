use super::*;
use crate::{
    EmbeddingBatchUpsertInput, EmbeddingBatchUpsertRequest, EmbeddingModelRegistration,
    EmbeddingModelRegistrationInput, EmbeddingNormalization, EmbeddingUpsertRow, FileInput,
    GenerationFacts, GenerationValidationLimits, NativeDatabase, PendingEmbeddingPageInput,
    PendingEmbeddingPageRequest, SearchDocumentInput, SemanticReadinessRequest, SymbolInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentKind, FileId, FileParseStatus, ModelId, SymbolExecutionFlags,
    SymbolExportFlags, SymbolId,
};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerank_sources_preserve_order_and_reject_foreign_generations_and_missing_ids() -> TestResult
{
    let fixture = Fixture::new(false).await?;
    let project = fixture.database.project_id().clone();
    let query = |generation, documents| {
        crate::RerankDocumentsRequest::new(project.clone(), generation, documents, TIMEOUT)
    };
    let inputs = fixture
        .database
        .rerank_documents(query(generation(), vec![document(2), document(1)])?)
        .await?;
    assert_eq!(
        inputs
            .iter()
            .map(crate::RerankDocument::document_id)
            .collect::<Vec<_>>(),
        [&document(2), &document(1)]
    );
    assert!(inputs[0].text().starts_with("name: function_2\n"));
    assert!(!fixture.readiness().await?.ready());
    assert!(matches!(
        fixture
            .database
            .rerank_documents(query(
                GenerationId::from_uuid_v8([9; 16]),
                vec![document(1)]
            )?)
            .await,
        Err(crate::SemanticStorageError::CurrentGenerationChanged)
    ));
    assert!(
        fixture
            .database
            .rerank_documents(query(generation(), vec![document(255)])?)
            .await
            .is_err()
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[test]
fn packing_destroys_corpus_before_releasing_builder() -> TestResult {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ObservedCorpus<'a> {
        cache: &'a SemanticCache,
        destroyed: &'a AtomicBool,
    }
    impl Drop for ObservedCorpus<'_> {
        fn drop(&mut self) {
            assert!(self.cache.0.lock().is_ok_and(|cached| cached.building));
            self.destroyed.store(true, Ordering::SeqCst);
        }
    }

    let cache = SemanticCache::default();
    cache.0.lock().map_err(|_| "cache lock poisoned")?.building = true;
    let destroyed = AtomicBool::new(false);
    let packing = Packing {
        corpus: ObservedCorpus {
            cache: &cache,
            destroyed: &destroyed,
        },
        permit: BuildPermit(&cache),
    };
    drop(packing);
    assert!(destroyed.load(Ordering::SeqCst));
    assert!(!cache.0.lock().map_err(|_| "cache lock poisoned")?.building);
    Ok(())
}

fn digest(value: u8) -> ContentDigest {
    ContentDigest::from_bytes([value; 32])
}
fn document(value: u8) -> DocumentId {
    DocumentId::from_uuid_v8([value; 16])
}
fn generation() -> GenerationId {
    GenerationId::from_uuid_v8([1; 16])
}
fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(TIMEOUT)?)
}

fn facts() -> GenerationFacts {
    let file = FileInput {
        file_id: FileId::from_uuid_v8([1; 16]),
        normalized_path: "src/lib.rs".into(),
        language: "rust".into(),
        content_hash: digest(2),
        byte_size: 1,
        parse_status: FileParseStatus::Parsed,
    };
    let symbols: Vec<_> = (1..=128_u8)
        .map(|id| SymbolInput {
            symbol_id: SymbolId::from_uuid_v8([id; 16]),
            file_id: file.file_id.clone(),
            symbol_kind: "function".into(),
            qualified_name: format!("function_{id}"),
            signature: "fn function()".into(),
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            end_line: 5,
            structural_digest: digest(2),
            visibility: None,
            export: SymbolExportFlags::default(),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        })
        .collect();
    let documents = symbols
        .iter()
        .zip(1..=128_u8)
        .map(|(symbol, id)| SearchDocumentInput {
            document_id: document(id),
            file_id: Some(file.file_id.clone()),
            symbol_id: Some(symbol.symbol_id.clone()),
            path: file.normalized_path.clone(),
            language: "rust".into(),
            kind: DocumentKind::Symbol,
            qualified_name: symbol.qualified_name.clone(),
            code: "fn function() {}".into(),
            natural_text: String::new(),
            metadata: serde_json::json!({}),
        })
        .collect();
    GenerationFacts {
        files: vec![file],
        symbols,
        documents,
        ..GenerationFacts::default()
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    store: AsyncProjectStore,
    database: NativeDatabase,
    model: EmbeddingModelSelector,
}

impl Fixture {
    async fn new(complete: bool) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let store = AsyncProjectStore::open(
            &runtime,
            root.path().to_owned(),
            digest(1),
            StorePolicy::default(),
            &request()?,
        )
        .await?;
        let (facts, _) = validate_generation_facts(
            facts(),
            GenerationValidationLimits::new(16 * 1_048_576, 64 * 1_048_576)?,
            || false,
        )?;
        store
            .publish_graph(
                GraphPublicationInput::new(
                    GraphRevision {
                        generation_id: generation(),
                        source_revision: digest(9),
                        base: GraphBase::Empty,
                    },
                    facts,
                    GraphPolicy::default(),
                    &request()?,
                )?,
                &request()?,
            )
            .await?;
        let database = NativeDatabase::new(store.clone(), &digest(1));
        let model = database
            .register_embedding_model(
                EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
                    model_id: ModelId::from_uuid_v8([1; 16]),
                    fingerprint: digest(8),
                    provider: "fixture".into(),
                    model_name: "two-dimensional".into(),
                    dimension: 2,
                    normalization: EmbeddingNormalization::None,
                })?,
                TIMEOUT,
            )
            .await?
            .selector()
            .clone();
        let fixture = Self {
            _root: root,
            runtime,
            store,
            database,
            model,
        };
        fixture.fill(complete).await?;
        Ok(fixture)
    }

    async fn fill(&self, complete: bool) -> TestResult {
        let page = self
            .database
            .pending_current_embedding_documents(PendingEmbeddingPageRequest::new(
                PendingEmbeddingPageInput {
                    project_id: self.database.project_id().clone(),
                    model: self.model.clone(),
                    maximum_documents: 128,
                    maximum_source_bytes: 1_048_576,
                    statement_timeout: TIMEOUT,
                },
            )?)
            .await?;
        let rows = page
            .documents()
            .iter()
            .filter(|row| complete || row.document_id() != &document(128))
            .map(|row| {
                EmbeddingUpsertRow::new(
                    row.document_id().clone(),
                    row.source_digest().clone(),
                    if row.document_id() == &document(1) {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.database
            .upsert_current_document_embeddings(EmbeddingBatchUpsertRequest::new(
                EmbeddingBatchUpsertInput {
                    project_id: self.database.project_id().clone(),
                    generation_id: generation(),
                    model: self.model.clone(),
                    rows,
                    statement_timeout: TIMEOUT,
                },
            )?)
            .await?;
        Ok(())
    }

    async fn readiness(&self) -> TestResult<SemanticReadinessReport> {
        Ok(self
            .database
            .semantic_readiness(SemanticReadinessRequest::new(
                self.database.project_id().clone(),
                self.model.clone(),
                TIMEOUT,
            )?)
            .await?)
    }

    async fn bounded_query(&self) -> TestResult {
        let cache = self.database.semantic_cache.clone();
        let model = self.model.clone();
        let hits = self
            .store
            .state(
                Some(generation()),
                ReadBudget::new(32, 16_384, TIMEOUT)?,
                0,
                0,
                &request()?,
                move |transaction, budget| {
                    let snapshot = transaction.graph_snapshot()?;
                    let graph = snapshot.graph(budget)?;
                    Ok(top_k(
                        &cache,
                        VectorQuery {
                            transaction,
                            graph: &graph,
                            generation: &generation(),
                            model: &model,
                            vector: &[1.0, 0.0],
                            limit: 2,
                        },
                        budget,
                    ))
                },
            )
            .await??;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].document_id(), &document(1));
        assert!(hits[0].distance().abs() < 1e-12);
        assert_eq!(hits[1].document_id(), &document(2));
        assert!((hits[1].distance() - 1.0).abs() < 1e-12);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_prepares_first_query_without_another_corpus_scan() -> TestResult {
    let fixture = Fixture::new(true).await?;
    let report = fixture.readiness().await?;
    assert!(report.ready());
    assert_eq!(report.documents(), 128);
    fixture.bounded_query().await?;
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_readiness_discards_packing_and_releases_builder() -> TestResult {
    let fixture = Fixture::new(false).await?;
    let report = fixture.readiness().await?;
    assert_eq!(
        report.state(),
        crate::SemanticReadinessState::CoverageIncomplete
    );
    assert_eq!(report.embedded(), 127);
    {
        let cached = fixture
            .database
            .semantic_cache
            .0
            .lock()
            .map_err(|_| "cache poisoned")?;
        assert!(cached.corpus.is_none());
        assert!(!cached.building);
    }
    fixture.fill(true).await?;
    assert!(fixture.readiness().await?.ready());
    fixture.bounded_query().await?;
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_readiness_discards_partial_corpus_and_releases_builder() -> TestResult {
    let fixture = Fixture::new(true).await?;
    let cache = fixture.database.semantic_cache.clone();
    let model = fixture.model.clone();
    let result = fixture
        .store
        .state(
            Some(generation()),
            ReadBudget::new(400, 1_048_576, TIMEOUT)?,
            0,
            0,
            &request()?,
            move |transaction, budget| Ok(readiness(&cache, transaction, &model, budget)),
        )
        .await?;
    assert!(result.is_err());
    {
        let cached = fixture
            .database
            .semantic_cache
            .0
            .lock()
            .map_err(|_| "cache poisoned")?;
        assert!(cached.corpus.is_none());
        assert!(cached.readiness.is_none());
        assert!(!cached.building);
    }
    assert!(fixture.readiness().await?.ready());
    fixture.bounded_query().await?;
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_readiness_cannot_publish_a_partial_corpus() -> TestResult {
    let fixture = Fixture::new(true).await?;
    let key = format!("v/{}/{}", fixture.model.model_id(), document(128)).into_bytes();
    let broken = key.clone();
    fixture
        .database
        .state(None, 1024, 1, move |tx, _| {
            tx.put(StateNamespace::Embeddings, &broken, vec![0])
        })
        .await?;
    assert!(fixture.readiness().await.is_err());
    {
        let cached = fixture
            .database
            .semantic_cache
            .0
            .lock()
            .map_err(|_| "cache poisoned")?;
        assert!(cached.corpus.is_none());
        assert!(cached.readiness.is_none());
        assert!(!cached.building);
    }
    fixture
        .database
        .state(None, 1024, 1, move |tx, _| {
            tx.delete(StateNamespace::Embeddings, &key)
        })
        .await?;
    fixture.fill(true).await?;
    assert!(fixture.readiness().await?.ready());
    fixture.bounded_query().await?;
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}
