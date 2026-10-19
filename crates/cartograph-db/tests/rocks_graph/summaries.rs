//! Artifact workflows reject stale roll-ups and preserve producer precedence.

use super::*;
use cartograph_db::{
    AgentArtifactKind, AgentArtifactQuery, FileSummarySaveRequest, ModuleSummarySaveRequest,
    NativeDatabase, NeighborSummarySaveInput, NeighborSummarySaveRequest,
    PendingStructuralSummaryQuery, PendingSummaryRollupQuery, RoleSweepModel, SearchDocumentInput,
    StorageError, SummaryCandidatePolicy, SummarySaveInput, SymbolRoleSaveInput,
    SymbolSummarySaveInput,
    rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy},
};
use cartograph_domain::{DocumentId, DocumentKind, ModelId, NormalizedPath};

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    store: AsyncProjectStore,
    database: NativeDatabase,
}
impl Fixture {
    async fn new() -> TestResult<Self> {
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
        let mut facts = raw_facts(3, vec![edge(1, 2, "native")]);
        facts.documents = (1_u8..=3)
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
                metadata: serde_json::json!({"name":format!("function_{id}")}),
            })
            .collect();
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
    async fn summarize(&self, number: u64, body: &str) -> TestResult {
        self.database
            .save_symbol_summary(
                SymbolSummarySaveInput::new(
                    self.database.project_id(),
                    &symbol(number),
                    &digest(2),
                )
                .with_summary(body)
                .with_model("fixture-judge"),
            )
            .await?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_role_cursor_advances_past_cached_pages_and_fences_changed_source() -> TestResult
{
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let project = db.project_id();
    db.save_symbol_role(
        SymbolRoleSaveInput::new(project, &symbol(1), "worker")
            .with_metadata(serde_json::json!({"model":"judge-a", "via":"llm"})),
    )
    .await?;
    let first = db
        .pending_structural_roles(project, "structural:v2", None, 1)
        .await?;
    assert!(
        first.symbols.is_empty(),
        "the judged role must be preserved"
    );
    let cursor = first
        .next
        .as_ref()
        .ok_or("cached first page lost its continuation")?;
    assert!(matches!(
        db.pending_structural_roles(project, "different-model", Some(cursor), 1)
            .await,
        Err(StorageError::InvalidInput {
            field: "role_cursor"
        })
    ));
    let mut next = Some(cursor.clone());
    let mut ids = Vec::new();
    while let Some(after) = next {
        let page = db
            .pending_structural_roles(project, "structural:v2", Some(&after), 1)
            .await?;
        assert_eq!(page.generation_id, first.generation_id);
        for candidate in page.symbols {
            assert!(candidate.code().is_empty() && candidate.description().is_empty());
            assert_eq!(candidate.path(), "src/lib.rs");
            ids.push(candidate.symbol_id().to_owned());
        }
        next = page.next;
    }
    assert_eq!(ids, [symbol(2).to_string(), symbol(3).to_string()]);
    for via in ["llm", "jev"] {
        let judged = db
            .save_symbol_role(
                SymbolRoleSaveInput::new(project, &symbol(2), "worker")
                    .with_metadata(serde_json::json!({"model":"judge-a", "via":via})),
            )
            .await?;
        let saved = db
            .save_symbol_role(
                SymbolRoleSaveInput::new(project, &symbol(2), "unknown")
                    .with_metadata(
                        serde_json::json!({"model":"structural:v2", "via":"structural_fallback"}),
                    )
                    .in_generation(&first.generation_id),
            )
            .await?;
        assert_eq!(saved.artifact_id(), judged.artifact_id());
        assert_eq!(saved.body(), "worker");
        assert_eq!(saved.metadata()["via"], via);
    }
    fixture
        .store
        .publish_graph(input(2, canonical(raw_facts(3, Vec::new()))?)?, &request()?)
        .await?;
    assert!(matches!(
        db.pending_structural_roles(project, "structural:v2", Some(cursor), 1)
            .await,
        Err(StorageError::CurrentGenerationChanged)
    ));
    assert_eq!(
        db.save_symbol_role(
            SymbolRoleSaveInput::new(project, &symbol(2), "worker")
                .with_metadata(
                    serde_json::json!({"model":"structural:v2", "via":"structural_fallback"})
                )
                .in_generation(&first.generation_id)
        )
        .await,
        Err(StorageError::CurrentGenerationChanged)
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollup_save_rechecks_evidence_and_detects_same_generation_summary_edits() -> TestResult {
    let fixture = Fixture::new().await?;
    let database = &fixture.database;
    let project = database.project_id();
    for number in 1..=3 {
        fixture
            .summarize(number, &format!("Judged behavior of symbol {number}."))
            .await?;
    }
    let anchor = digest(7);
    let query = || PendingSummaryRollupQuery::new(project, "fixture-judge", &anchor).page(10, 2);
    let pending = database.pending_file_summaries(query()).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].summarized_symbols(), 3);
    assert!(pending[0].items_truncated());
    assert_eq!(pending[0].items()[0].symbol_id(), symbol(1).as_str());
    let path = NormalizedPath::parse("src/lib.rs")?;
    let expected = ContentDigest::parse(pending[0].content_hash())?;
    let save = || {
        FileSummarySaveRequest::new(
            &path,
            SummarySaveInput::new(&expected, &anchor, "A current file paragraph.")
                .with_model("fixture-judge")
                .with_item_limit(2),
        )
    };
    let saved = database
        .save_file_summary(project, save()?)
        .await?
        .ok_or("file summary missing")?;
    assert_eq!(
        database.pending_file_summaries(query()).await?,
        [] as [cartograph_db::PendingFileSummary; 0]
    );
    fixture
        .summarize(1, "A changed explanation within the same generation.")
        .await?;
    assert_eq!(
        database.save_file_summary(project, save()?).await,
        Err(StorageError::CurrentGenerationChanged)
    );
    let changed = database.pending_file_summaries(query()).await?;
    assert_eq!(changed.len(), 1);
    assert_ne!(changed[0].content_hash(), expected.as_str());
    let new_expected = ContentDigest::parse(changed[0].content_hash())?;
    let updated = database
        .save_file_summary(
            project,
            FileSummarySaveRequest::new(
                &path,
                SummarySaveInput::new(&new_expected, &anchor, "A changed current file paragraph.")
                    .with_model("fixture-judge")
                    .with_item_limit(2),
            )?,
        )
        .await?
        .ok_or("updated summary missing")?;
    assert_eq!(updated.artifact_id(), saved.artifact_id());
    let modules = database.pending_module_summaries(query()).await?;
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].directory(), "src");
    assert_eq!(modules[0].summarized_symbols(), 3);
    let directory = NormalizedPath::parse("src")?;
    let module_digest = ContentDigest::parse(modules[0].content_hash())?;
    database
        .save_module_summary(
            project,
            ModuleSummarySaveRequest::new(
                &directory,
                SummarySaveInput::new(&module_digest, &anchor, "Current module paragraph.")
                    .with_model("fixture-judge")
                    .with_item_limit(2),
            )?,
        )
        .await?;
    assert_eq!(
        database.pending_module_summaries(query()).await?,
        [] as [cartograph_db::PendingModuleSummary; 0]
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_batches_are_atomic_bounded_ordered_and_preserve_judged_results() -> TestResult {
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let project = db.project_id();
    let generation = db
        .pending_structural_roles(project, "structural:v2", None, 1)
        .await?
        .generation_id;
    let metadata = || serde_json::json!({"model":"structural:v2", "via":"structural_fallback"});
    let first = symbol(2);
    let absent = symbol(999);
    assert_eq!(
        db.save_symbol_roles(
            &generation,
            [
                SymbolRoleSaveInput::new(project, &first, "worker").with_metadata(metadata()),
                SymbolRoleSaveInput::new(project, &absent, "worker").with_metadata(metadata()),
            ]
        )
        .await,
        Err(StorageError::CurrentGenerationChanged)
    );
    assert_eq!(
        db.pending_symbol_roles(project, RoleSweepModel::Structural("structural:v2"), 10)
            .await?
            .len(),
        3
    );
    let judged = db
        .save_symbol_role(
            SymbolRoleSaveInput::new(project, &symbol(1), "judged-worker")
                .with_metadata(serde_json::json!({"model":"judge", "via":"llm"})),
        )
        .await?;
    let ids = [symbol(3), symbol(1), symbol(2)];
    let saved = db
        .save_symbol_roles(
            &generation,
            ids.iter().map(|id| {
                SymbolRoleSaveInput::new(project, id, "worker").with_metadata(metadata())
            }),
        )
        .await?;
    assert_eq!(
        saved
            .iter()
            .map(cartograph_db::AgentArtifactRecord::scope_key)
            .collect::<Vec<_>>(),
        ids.iter()
            .map(cartograph_domain::SymbolId::as_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(saved[1].artifact_id(), judged.artifact_id());
    assert_eq!(saved[1].body(), "judged-worker");
    assert_eq!(
        db.pending_symbol_roles(project, RoleSweepModel::Structural("structural:v2"), 10)
            .await?,
        [] as [cartograph_db::PendingRoleSymbol; 0]
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_batch_rejects_empty_duplicates_and_oversized_inputs() -> TestResult {
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let project = db.project_id();
    let generation = db
        .pending_structural_roles(project, "structural:v2", None, 1)
        .await?
        .generation_id;
    let metadata = || serde_json::json!({"model":"structural:v2", "via":"structural_fallback"});
    let first = symbol(2);
    for inputs in [
        Vec::new(),
        vec![
            SymbolRoleSaveInput::new(project, &first, "worker").with_metadata(metadata()),
            SymbolRoleSaveInput::new(project, &first, "worker").with_metadata(metadata()),
        ],
    ] {
        assert_eq!(
            db.save_symbol_roles(&generation, inputs).await,
            Err(StorageError::InvalidInput {
                field: "role_batch"
            })
        );
    }
    let ids = (1..=17).map(symbol).collect::<Vec<_>>();
    assert_eq!(
        db.save_symbol_roles(
            &generation,
            ids.iter().map(|id| {
                SymbolRoleSaveInput::new(project, id, "worker").with_metadata(metadata())
            })
        )
        .await,
        Err(StorageError::InvalidInput {
            field: "role_batch"
        })
    );
    let body = "x".repeat(65_536);
    assert_eq!(
        db.save_symbol_roles(
            &generation,
            ids[..16]
                .iter()
                .map(|id| SymbolRoleSaveInput::new(project, id, &body).with_metadata(metadata()))
        )
        .await,
        Err(StorageError::InvalidInput {
            field: "role_batch"
        })
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_evidence_roles_and_neighbor_propagation_keep_exact_provenance() -> TestResult {
    let fixture = Fixture::new().await?;
    let database = &fixture.database;
    let project = database.project_id();
    let policy = SummaryCandidatePolicy::default();
    let pending = database
        .pending_structural_summaries(PendingStructuralSummaryQuery::new(project, 10, &policy))
        .await?;
    assert_eq!(pending.len(), 3);
    assert_eq!(pending[0].name(), "function_1");
    assert_eq!(pending[0].edges().len(), 1);
    assert_eq!(pending[0].edges()[0].target_name(), "function_2");
    database
        .save_symbol_role(
            SymbolRoleSaveInput::new(project, &symbol(1), "worker")
                .with_metadata(serde_json::json!({"model":"judge-a", "via":"llm"})),
        )
        .await?;
    assert_eq!(
        database
            .pending_symbol_roles(project, RoleSweepModel::Structural("structural:v2"), 10)
            .await?
            .len(),
        2
    );
    assert_eq!(
        database
            .pending_symbol_roles(project, RoleSweepModel::Judge("judge-b"), 10)
            .await?
            .len(),
        3
    );
    fixture.summarize(2, "Runs the shared worker.").await?;
    let model = ModelId::from_uuid_v8([1; 16]);
    let target = symbol(1);
    let neighbor = symbol(2);
    let source_digest = digest(2);
    let save = || {
        NeighborSummarySaveRequest::new(NeighborSummarySaveInput {
            symbol_id: &target,
            source_digest: &source_digest,
            neighbor_symbol_id: &neighbor,
            neighbor_summary: "Runs the shared worker.",
            summary: "Runs a related worker.",
            embedding_model_id: &model,
            similarity: 0.95,
        })
    };
    assert!(
        database
            .save_neighbor_symbol_summary(project, save()?)
            .await?
            .is_some()
    );
    assert!(
        database
            .save_neighbor_symbol_summary(project, save()?)
            .await?
            .is_none()
    );
    let sources = database
        .neighbor_summary_sources(project, &generation(1), &[symbol(1), symbol(2)])
        .await?;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].symbol_id(), symbol(2).as_str());
    let summaries = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)?.with_kind(AgentArtifactKind::Summary),
        )
        .await?;
    assert_eq!(summaries.len(), 2);
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}
