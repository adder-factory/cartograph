//! Typed workflows exercise publication, vector source fences, and artifact identity.

use super::*;
use cartograph_db::{
    AgentArtifactKind, AgentArtifactQuery, EmbeddingBatchUpsertInput, EmbeddingBatchUpsertRequest,
    EmbeddingModelRegistration, EmbeddingModelRegistrationInput, EmbeddingModelSelector,
    EmbeddingNormalization, EmbeddingUpsertRow, NativeDatabase, PendingEmbeddingPageInput,
    PendingEmbeddingPageRequest, SearchDocumentInput, SemanticReadinessRequest,
    SemanticReadinessState, SemanticStorageError, StructuralSymbolSummarySaveInput,
    SymbolSummarySaveInput, VectorSearchInput, VectorSearchRequest,
    rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy},
};
use cartograph_domain::{DocumentId, DocumentKind, ModelId};

fn document(value: u8) -> DocumentId {
    DocumentId::from_uuid_v8([value; 16])
}

fn searchable(changed: bool) -> TestResult<CanonicalGenerationFacts> {
    let mut facts = raw_facts(2, vec![edge(1, 2, "native")]);
    for symbol in &mut facts.symbols {
        symbol.end_line = 6;
    }
    if changed {
        facts.symbols[0].structural_digest = digest(4);
        facts.files[0].content_hash = digest(5);
    }
    facts.documents = (1_u8..=2)
        .map(|number| SearchDocumentInput {
            document_id: document(number),
            file_id: Some(facts.files[0].file_id.clone()),
            symbol_id: Some(symbol(u64::from(number))),
            path: "src/lib.rs".into(),
            language: "rust".into(),
            kind: DocumentKind::Symbol,
            qualified_name: format!("function_{number}"),
            code: if changed && number == 1 {
                "fn first() { updated() }".into()
            } else {
                format!("fn function_{number}() {{}}")
            },
            natural_text: String::new(),
            metadata: serde_json::json!({}),
        })
        .collect();
    canonical(facts)
}

fn pending(
    database: &NativeDatabase,
    model: &EmbeddingModelSelector,
) -> TestResult<PendingEmbeddingPageRequest> {
    Ok(PendingEmbeddingPageRequest::new(
        PendingEmbeddingPageInput {
            project_id: database.project_id().clone(),
            model: model.clone(),
            maximum_documents: 2,
            maximum_source_bytes: 1_048_576,
            statement_timeout: Duration::from_secs(30),
        },
    )?)
}

fn metadata_heavy_search_facts() -> TestResult<CanonicalGenerationFacts> {
    let mut facts = raw_facts(1_792, Vec::new());
    let padding = "x".repeat(60 * 1024);
    facts.documents = facts
        .symbols
        .iter()
        .zip(1_u64..)
        .map(|(symbol, number)| {
            let mut bytes = [0; 16];
            bytes[8..].copy_from_slice(&number.to_be_bytes());
            SearchDocumentInput {
                document_id: DocumentId::from_uuid_v8(bytes),
                file_id: Some(symbol.file_id.clone()),
                symbol_id: Some(symbol.symbol_id.clone()),
                path: "src/lib.rs".into(),
                language: "rust".into(),
                kind: DocumentKind::Symbol,
                qualified_name: symbol.qualified_name.clone(),
                code: match number {
                    1 => "needle",
                    2 => "needle needle",
                    _ => "other",
                }
                .into(),
                natural_text: String::new(),
                metadata: serde_json::json!({ "padding": padding }),
            }
        })
        .collect();
    Ok(validate_generation_facts(
        facts,
        GenerationValidationLimits::new(128 * 1_048_576, 512 * 1_048_576)?,
        || false,
    )?
    .0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bm25_metadata_heavy_corpus_fits_the_unchanged_physical_read_allowance() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy {
        pending_bytes: 512 * 1_048_576,
        ..WorkerPolicy::default()
    })?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_path_buf(),
        digest(1),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let published = store
        .publish_graph(input(1, metadata_heavy_search_facts()?)?, &request()?)
        .await?;
    let database = NativeDatabase::new(store, &digest(1));
    let query = |limit| {
        cartograph_db::SearchQuery::new(
            cartograph_db::CurrentGenerationLookup::new(
                database.project_id(),
                &published.revision.generation_id,
            ),
            "needle",
            limit,
        )
    };
    let hits = database.search_current_code(query(2)).await?;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].symbol_id(), Some(&symbol(2)));
    assert_eq!(hits[1].symbol_id(), Some(&symbol(1)));
    // Independent BM25: N=1792, df=2, weighted lengths 13/14, average 13+1/1792.
    assert!((hits[0].score() - 8.849_766_530_680_785).abs() < 1e-10);
    assert!((hits[1].score() - 6.575_470_205_742_833).abs() < 1e-10);
    let first = database.search_current_code(query(1)).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].score(), hits[0].score());
    runtime.shutdown(&request()?).await?;
    Ok(())
}

async fn readiness(
    database: &NativeDatabase,
    model: &EmbeddingModelSelector,
) -> TestResult<cartograph_db::SemanticReadinessReport> {
    Ok(database
        .semantic_readiness(SemanticReadinessRequest::new(
            database.project_id().clone(),
            model.clone(),
            Duration::from_secs(30),
        )?)
        .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vectors_reuse_unchanged_documents_and_reject_changed_source_and_summaries() -> TestResult {
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
    store
        .publish_graph(input(1, searchable(false)?)?, &request()?)
        .await?;
    let database = NativeDatabase::new(store.clone(), &digest(1));
    let project = database.project_id().clone();
    let (model, original, _) = initialize_vectors(&database).await?;
    let saved = database
        .save_symbol_summary(
            SymbolSummarySaveInput::new(&project, &symbol(1), &digest(2))
                .with_summary("Calls the worker using current input.")
                .with_model("fixture-summary"),
        )
        .await?;
    assert_eq!(readiness(&database, &model).await?.embedded(), 1);
    assert_eq!(
        readiness(&database, &model).await?.state(),
        SemanticReadinessState::CoverageIncomplete
    );
    assert!(
        database
            .save_structural_symbol_summary(
                StructuralSymbolSummarySaveInput::new(&project, &symbol(1), &digest(2))
                    .with_summary("Thin structural fallback.")
            )
            .await?
            .is_none()
    );
    let stale = EmbeddingBatchUpsertRequest::new(EmbeddingBatchUpsertInput {
        project_id: project.clone(),
        generation_id: generation(1),
        model: model.clone(),
        rows: vec![EmbeddingUpsertRow::new(
            document(1),
            original,
            vec![1.0, 0.0],
        )?],
        statement_timeout: Duration::from_secs(30),
    })?;
    assert_eq!(
        database.upsert_current_document_embeddings(stale).await,
        Err(SemanticStorageError::SourceDigestChanged)
    );
    store
        .publish_graph(input(2, searchable(false)?)?, &request()?)
        .await?;
    let summaries = database
        .list_agent_artifacts(
            &project,
            AgentArtifactQuery::new(10)?
                .with_kind(AgentArtifactKind::Summary)
                .current_generation_only(),
        )
        .await?;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].artifact_id(), saved.artifact_id());
    let pending = database
        .pending_current_embedding_documents(pending(&database, &model)?)
        .await?;
    assert_eq!(pending.documents().len(), 1);
    assert_eq!(pending.documents()[0].document_id(), &document(1));
    store
        .publish_graph(input(3, searchable(true)?)?, &request()?)
        .await?;
    assert_eq!(
        database
            .list_agent_artifacts(
                &project,
                AgentArtifactQuery::new(10)?
                    .with_kind(AgentArtifactKind::Summary)
                    .current_generation_only()
            )
            .await?,
        [] as [cartograph_db::AgentArtifactRecord; 0]
    );
    assert_eq!(readiness(&database, &model).await?.embedded(), 1);
    runtime.shutdown(&request()?).await?;
    Ok(())
}

pub(super) async fn initialize_vectors(
    database: &NativeDatabase,
) -> TestResult<(EmbeddingModelSelector, ContentDigest, ContentDigest)> {
    let registration = EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: ModelId::from_uuid_v8([1; 16]),
        fingerprint: digest(8),
        provider: "fixture".into(),
        model_name: "two-dimensional".into(),
        dimension: 2,
        normalization: EmbeddingNormalization::None,
    })?;
    let model = database
        .register_embedding_model(registration, Duration::from_secs(30))
        .await?
        .selector()
        .clone();
    assert_eq!(
        database
            .embedding_model_index_status(&model, Duration::from_secs(30))
            .await?
            .algorithm(),
        "exact-cosine"
    );
    let first = database
        .pending_current_embedding_documents(pending(database, &model)?)
        .await?;
    assert_eq!(first.documents().len(), 2);
    let original = first
        .documents()
        .iter()
        .find(|row| row.document_id() == &document(1))
        .ok_or("first document missing")?
        .source_digest()
        .clone();
    let rows = first
        .documents()
        .iter()
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
    let second = first
        .documents()
        .iter()
        .find(|row| row.document_id() == &document(2))
        .ok_or("second document missing")?
        .source_digest()
        .clone();
    database
        .upsert_current_document_embeddings(EmbeddingBatchUpsertRequest::new(
            EmbeddingBatchUpsertInput {
                project_id: database.project_id().clone(),
                generation_id: generation(1),
                model: model.clone(),
                rows,
                statement_timeout: Duration::from_secs(30),
            },
        )?)
        .await?;
    assert!(readiness(database, &model).await?.ready());
    verify_exact_ranking(database, &model).await?;
    Ok((model, original, second))
}

async fn verify_exact_ranking(
    database: &NativeDatabase,
    model: &EmbeddingModelSelector,
) -> TestResult {
    let hits = database
        .vector_top_k(VectorSearchRequest::new(VectorSearchInput {
            project_id: database.project_id().clone(),
            expected_generation_id: generation(1),
            model: model.clone(),
            vector: vec![1.0, 0.0],
            limit: 2,
            statement_timeout: Duration::from_secs(30),
        })?)
        .await?;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].document_id(), &document(1));
    assert!(hits[0].distance().abs() < 1e-12);
    assert_eq!(hits[1].document_id(), &document(2));
    assert!((hits[1].distance() - 1.0).abs() < 1e-12);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_semantic_cache_observes_same_generation_vector_and_summary_writes() -> TestResult {
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
    store
        .publish_graph(input(1, searchable(false)?)?, &request()?)
        .await?;
    let database = NativeDatabase::new(store.clone(), &digest(1));
    let (model, first, second) = initialize_vectors(&database).await?;
    verify_exact_ranking(&database, &model).await?;
    verify_exact_ranking(&database, &model).await?;
    database
        .upsert_current_document_embeddings(EmbeddingBatchUpsertRequest::new(
            EmbeddingBatchUpsertInput {
                project_id: database.project_id().clone(),
                generation_id: generation(1),
                model: model.clone(),
                rows: vec![
                    EmbeddingUpsertRow::new(document(1), first, vec![0.0, 1.0])?,
                    EmbeddingUpsertRow::new(document(2), second, vec![1.0, 0.0])?,
                ],
                statement_timeout: Duration::from_secs(30),
            },
        )?)
        .await?;
    let query = |generation| {
        VectorSearchRequest::new(VectorSearchInput {
            project_id: database.project_id().clone(),
            expected_generation_id: generation,
            model: model.clone(),
            vector: vec![1.0, 0.0],
            limit: 2,
            statement_timeout: Duration::from_secs(30),
        })
    };
    for _ in 0..2 {
        let hits = database.vector_top_k(query(generation(1))?).await?;
        assert_eq!(hits[0].document_id(), &document(2));
        assert!(hits[0].distance().abs() < 1e-12);
        assert_eq!(hits[1].document_id(), &document(1));
        assert!(readiness(&database, &model).await?.ready());
    }
    database
        .save_symbol_summary(
            SymbolSummarySaveInput::new(database.project_id(), &symbol(2), &digest(2))
                .with_summary("New source-fenced explanation.")
                .with_model("fixture"),
        )
        .await?;
    assert_eq!(
        readiness(&database, &model).await?.state(),
        SemanticReadinessState::CoverageIncomplete
    );
    assert!(database.vector_top_k(query(generation(1))?).await.is_err());
    store
        .publish_graph(input(2, searchable(true)?)?, &request()?)
        .await?;
    assert_eq!(readiness(&database, &model).await?.embedded(), 0);
    assert!(database.vector_top_k(query(generation(2))?).await.is_err());
    runtime.shutdown(&request()?).await?;
    Ok(())
}
