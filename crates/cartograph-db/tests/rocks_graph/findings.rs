//! Detector results follow source and auxiliary revisions without stale caches.

use super::*;
use cartograph_db::{
    NativeDatabase, SearchDocumentInput, StructuralFindingQuery, StructuralFindingRefresh,
    StructuralFindingSeverity,
    rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy},
};
use cartograph_domain::{DocumentId, DocumentKind};
use serde_json::json;

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    store: AsyncProjectStore,
    database: NativeDatabase,
}
impl Fixture {
    async fn new(facts: GenerationFacts) -> TestResult<Self> {
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
            .publish_graph(input(1, canonical(facts)?)?, &request()?)
            .await?;
        let database = NativeDatabase::new(store.clone(), &digest(1));
        Ok(Self {
            _root: root,
            runtime,
            store,
            database,
        })
    }
}
fn searchable(nodes: u8) -> GenerationFacts {
    let mut facts = raw_facts(u64::from(nodes), Vec::new());
    facts.files[0].byte_size = 1000;
    for (index, symbol) in facts.symbols.iter_mut().enumerate() {
        symbol.end_line = 20;
        symbol.end_byte = 1000;
        symbol.structural_digest = digest(u8::try_from(index + 1).unwrap_or(0));
    }
    facts.documents = (1..=nodes)
        .map(|id| SearchDocumentInput {
            document_id: DocumentId::from_uuid_v8([id; 16]),
            file_id: Some(facts.files[0].file_id.clone()),
            symbol_id: Some(symbol(u64::from(id))),
            path: "src/lib.rs".into(),
            language: "rust".into(),
            kind: DocumentKind::Symbol,
            qualified_name: format!("function_{id}"),
            code: format!("fn function_{id}() {{}}"),
            natural_text: String::new(),
            metadata: json!({}),
        })
        .collect();
    facts
}
fn detector_facts() -> GenerationFacts {
    let mut facts = searchable(4);
    facts.symbols[0].end_line = 250;
    facts.symbols[1].structural_digest = digest(2);
    facts.symbols[2].structural_digest = digest(2);
    facts.documents[0].metadata = json!({"health":{"code_lines":200,"cyclomatic":25,"max_nesting":7,"max_conditional_operands":8,"parameter_count":7}});
    facts.documents[3].kind = DocumentKind::Test;
    facts.documents[3].metadata =
        json!({"health":{"code_lines":900,"cyclomatic":90,"empty_catches":90}});
    facts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_findings_exclude_test_documents_and_invalidate_on_publication() -> TestResult {
    let fixture = Fixture::new(detector_facts()).await?;
    let database = &fixture.database;
    let project = database.project_id();
    assert!(
        database
            .cached_current_structural_finding_stats(project)
            .await?
            .is_none()
    );
    let stats = database
        .current_structural_finding_stats_bounded(project, Duration::from_secs(30))
        .await?;
    assert_eq!(stats.analyzed_symbols(), 3);
    assert_eq!(stats.total_findings(), 7);
    assert_eq!(stats.error_findings(), 6);
    assert!(
        database
            .cached_current_structural_finding_stats(project)
            .await?
            .is_none()
    );
    assert_eq!(
        database
            .refresh_current_structural_findings(project, Duration::from_secs(30))
            .await?,
        StructuralFindingRefresh::Recomputed
    );
    assert_eq!(
        database
            .refresh_current_structural_findings(project, Duration::from_secs(30))
            .await?,
        StructuralFindingRefresh::Current
    );
    let query =
        StructuralFindingQuery::new(2)?.with_minimum_severity(StructuralFindingSeverity::Info);
    assert_eq!(
        database
            .query_current_structural_findings(project, &query)
            .await?
            .len(),
        2
    );
    assert_eq!(
        database
            .count_current_structural_findings(project, &query)
            .await?,
        7
    );
    let clones = database
        .query_current_structural_findings(project, &query.with_finding(Some("duplicate_code"))?)
        .await?;
    assert_eq!(clones.len(), 1);
    assert_eq!(clones[0].symbol_id(), symbol(2).as_str());
    let detail = serde_json::to_value(&clones[0])?;
    assert_eq!(detail["detail"]["classSize"], 2);
    assert_eq!(
        detail["detail"]["members"][0]["symbolId"],
        symbol(3).as_str()
    );
    let mut changed = detector_facts();
    changed.symbols[0].end_line = 2;
    changed.symbols[0].structural_digest = digest(9);
    changed.documents[0].metadata = json!({});
    changed.documents[0].code = "fn edited() {}".into();
    fixture
        .store
        .publish_graph(input(2, canonical(changed)?)?, &request()?)
        .await?;
    assert!(
        database
            .cached_current_structural_finding_stats(project)
            .await?
            .is_none()
    );
    assert_eq!(database.current_structural_findings(project, 10).await?, []);
    database
        .refresh_current_structural_findings(project, Duration::from_secs(30))
        .await?;
    assert_eq!(
        database
            .current_structural_finding_stats(project)
            .await?
            .total_findings(),
        1
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_clone_cache_follows_vector_epochs_and_canonicalizes_components() -> TestResult {
    use cartograph_db::{
        EmbeddingBatchUpsertInput, EmbeddingBatchUpsertRequest, EmbeddingUpsertRow,
        PendingEmbeddingPageInput, PendingEmbeddingPageRequest, SimilarityMaterializationPolicy,
    };
    let fixture = Fixture::new(searchable(2)).await?;
    let database = &fixture.database;
    let project = database.project_id();
    let (model, _, source) = super::features::initialize_vectors(database).await?;
    let policy = SimilarityMaterializationPolicy::new(2, 0.0, Duration::from_secs(30))?;
    let built = database
        .rebuild_current_similarity_edges(project, policy)
        .await?;
    assert_eq!(
        (
            built.models,
            built.source_symbols,
            built.model_symbol_pairs,
            built.edges_written
        ),
        (1, 2, 2, 2)
    );
    database
        .refresh_current_structural_findings(project, Duration::from_secs(30))
        .await?;
    assert_eq!(
        database
            .current_structural_finding_stats(project)
            .await?
            .total_findings(),
        0
    );
    // Replace complete vectors with parallel directions; source digests remain exact.
    // pending is empty for already embedded documents, so use the saved model source digests.
    let pending = PendingEmbeddingPageRequest::new(PendingEmbeddingPageInput {
        project_id: project.clone(),
        model: model.clone(),
        maximum_documents: 2,
        maximum_source_bytes: 1_048_576,
        statement_timeout: Duration::from_secs(30),
    })?;
    assert_eq!(
        database
            .pending_current_embedding_documents(pending)
            .await?
            .documents(),
        []
    );
    database
        .upsert_current_document_embeddings(EmbeddingBatchUpsertRequest::new(
            EmbeddingBatchUpsertInput {
                project_id: project.clone(),
                generation_id: generation(1),
                model,
                rows: vec![EmbeddingUpsertRow::new(
                    DocumentId::from_uuid_v8([2; 16]),
                    source,
                    vec![1.0, 0.0],
                )?],
                statement_timeout: Duration::from_secs(30),
            },
        )?)
        .await?;
    assert!(
        database
            .cached_current_structural_finding_stats(project)
            .await?
            .is_none()
    );
    database
        .refresh_current_structural_findings(project, Duration::from_secs(30))
        .await?;
    assert_eq!(
        database
            .current_structural_finding_stats(project)
            .await?
            .total_findings(),
        0
    );
    database
        .rebuild_current_similarity_edges(project, policy)
        .await?;
    assert!(
        database
            .cached_current_structural_finding_stats(project)
            .await?
            .is_none()
    );
    database
        .refresh_current_structural_findings(project, Duration::from_secs(30))
        .await?;
    let clones = database.current_structural_findings(project, 10).await?;
    assert_eq!(clones.len(), 1);
    assert_eq!(clones[0].symbol_id(), symbol(1).as_str());
    let detail = serde_json::to_value(&clones[0])?;
    assert_eq!(detail["detail"]["cloneType"], "semantic");
    assert_eq!(detail["detail"]["classSize"], 2);
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}
