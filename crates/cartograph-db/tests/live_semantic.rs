//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;

use std::{
    env, process,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CartographDatabase, EmbeddingBatchUpsertInput, EmbeddingBatchUpsertRequest,
    EmbeddingModelRegistration, EmbeddingModelRegistrationInput, EmbeddingNormalization,
    EmbeddingUpsertRow, FileInput, GenerationContents, GenerationFacts, GenerationRetentionPolicy,
    GenerationRetentionRequest, GenerationValidationLimits, LeaseOwner, LeaseRequest, LeaseTarget,
    NewGeneration, NewProject, PendingEmbeddingPageInput, PendingEmbeddingPageRequest,
    RetireEmbeddingModelRequest, SearchDocumentInput, SemanticReadinessRequest,
    SemanticReadinessState, SemanticStorageError, SimilarSymbolsInput, SimilarSymbolsRequest,
    SimilarityMaterializationPolicy, StructuralFindingRefresh, SymbolInput, VectorSearchInput,
    VectorSearchRequest, latest_schema_version, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, FileId, FileParseStatus, GenerationId, ModelId,
    ProjectId, ProjectOperation, SymbolId, SymbolKind,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const LEASE_DURATION: Duration = Duration::from_mins(1);
const VALIDATION_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const VALIDATION_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const PROJECT_FINGERPRINT: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_PROJECT_FINGERPRINT: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MODEL_A_ID: &str = "11111111-1111-8111-8111-111111111111";
const MODEL_B_ID: &str = "22222222-2222-8222-8222-222222222222";
const MODEL_A_FINGERPRINT: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";
const MODEL_B_FINGERPRINT: &str =
    "2222222222222222222222222222222222222222222222222222222222222222";
const DOCUMENT_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const DOCUMENT_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const DOCUMENT_C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const FILE_A: &str = "10000000-0000-4000-8000-000000000001";
const FILE_B: &str = "10000000-0000-4000-8000-000000000002";
const FILE_C: &str = "10000000-0000-4000-8000-000000000003";
const SYMBOL_A: &str = "40000000-0000-4000-8000-000000000001";
const SYMBOL_B: &str = "40000000-0000-4000-8000-000000000002";
const SYMBOL_C: &str = "40000000-0000-4000-8000-000000000003";
const VECTOR_DIMENSION: u16 = 3;
const PAGE_DOCUMENTS: u16 = 2;
const PAGE_BYTES: u64 = 2 * 1024 * 1024;
const FILTERED_HNSW_TARGET_ROWS: i32 = 10_000;
const FILTERED_HNSW_DISTRACTOR_ROWS: i32 = 10_000;
const FILTERED_HNSW_LIMIT: u16 = 10;

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB/pgvector test database"]
async fn semantic_storage_is_model_scoped_ready_bounded_and_retention_safe() {
    let fixture = open_fixture().await;
    migrate(&fixture).await;
    let scenario = setup_semantic_scenario(&fixture).await;
    assert_embedding_writes_and_readiness(&fixture, &scenario).await;
    assert_vector_similarity(&fixture, &scenario).await;
    assert_semantic_clone_findings(&fixture, &scenario).await;
    assert_non_clique_semantic_clone_component(&fixture, &scenario).await;
    let stale_cursor = prepare_replacement_and_retire(&fixture, &scenario).await;
    assert_generation_cursor_refresh(&fixture, &scenario, stale_cursor).await;
    assert_semantic_retention(&fixture, &scenario).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

struct SemanticScenario {
    project: ProjectId,
    first: GenerationId,
    selector_a: cartograph_db::EmbeddingModelSelector,
    selector_b: cartograph_db::EmbeddingModelSelector,
    first_page: cartograph_db::PendingEmbeddingPage,
    second_page: cartograph_db::PendingEmbeddingPage,
}

async fn setup_semantic_scenario(fixture: &Fixture) -> SemanticScenario {
    let model_a = model_registration(ModelFixture::A);
    let model_b = model_registration(ModelFixture::B);
    let selector_a = model_a.selector();
    let selector_b = model_b.selector();

    let registered_a = register_model(&fixture.database, model_a.clone()).await;
    let repeated_a = register_model(&fixture.database, model_a).await;
    assert_eq!(registered_a, repeated_a);
    assert!(matches!(
        fixture
            .database
            .register_embedding_model(conflicting_model(), STATEMENT_TIMEOUT)
            .await,
        Err(SemanticStorageError::ModelConflict)
    ));
    assert!(matches!(
        fixture
            .database
            .register_embedding_model(conflicting_fingerprint_model(), STATEMENT_TIMEOUT)
            .await,
        Err(SemanticStorageError::ModelConflict)
    ));
    let hnsw_a = fixture
        .database
        .ensure_embedding_model_hnsw(&selector_a, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("model A HNSW creation failed: {error}"));
    assert!(hnsw_a.ready());
    assert!(hnsw_a.index_name().len() <= 63);

    let project = register_project(&fixture.database).await;
    let first = publish_generation(&fixture.database, &project, 1).await;
    assert_readiness(
        &fixture.database,
        &project,
        &selector_b,
        SemanticReadinessState::ModelMissing,
        0,
        0,
    )
    .await;
    assert_dimension_trigger_rejects(fixture, &project, &first, &selector_a).await;
    assert_readiness(
        &fixture.database,
        &project,
        &selector_a,
        SemanticReadinessState::CoverageIncomplete,
        3,
        0,
    )
    .await;

    let first_page = pending_page(&fixture.database, &project, &selector_a, None).await;
    assert_eq!(first_page.generation_id(), &first);
    assert_eq!(first_page.documents().len(), 2);
    assert_eq!(first_page.documents()[0].document_id().as_str(), DOCUMENT_A);
    assert_eq!(first_page.documents()[1].document_id().as_str(), DOCUMENT_B);
    assert!(first_page.documents()[0].text().contains("alpha_symbol"));
    let cursor = first_page
        .next_cursor()
        .cloned()
        .unwrap_or_else(|| panic!("first semantic page did not expose a cursor"));
    let second_page = pending_page(&fixture.database, &project, &selector_a, Some(cursor)).await;
    assert_eq!(second_page.documents().len(), 1);
    assert_eq!(
        second_page.documents()[0].document_id().as_str(),
        DOCUMENT_C
    );
    assert!(second_page.next_cursor().is_none());
    SemanticScenario {
        project,
        first,
        selector_a,
        selector_b,
        first_page,
        second_page,
    }
}

async fn assert_embedding_writes_and_readiness(fixture: &Fixture, scenario: &SemanticScenario) {
    let project = &scenario.project;
    let first = &scenario.first;
    let selector_a = &scenario.selector_a;
    let first_page = &scenario.first_page;
    let second_page = &scenario.second_page;
    let rollback = fixture
        .database
        .upsert_current_document_embeddings(
            EmbeddingBatchUpsertRequest::new(EmbeddingBatchUpsertInput {
                project_id: project.clone(),
                generation_id: first.clone(),
                model: selector_a.clone(),
                rows: vec![
                    embedding_row(&first_page.documents()[0], [1.0, 0.0, 0.0]),
                    EmbeddingUpsertRow::new(
                        first_page.documents()[1].document_id().clone(),
                        digest(MODEL_B_FINGERPRINT),
                        vec![0.0, 1.0, 0.0],
                    )
                    .unwrap_or_else(|error| panic!("rollback embedding row failed: {error}")),
                ],
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("rollback batch failed validation: {error}")),
        )
        .await;
    assert_eq!(rollback, Err(SemanticStorageError::SourceDigestChanged));
    assert_embedding_count(fixture, first, selector_a.model_id(), 0).await;

    let first_write = upsert_documents(
        &fixture.database,
        project,
        first,
        selector_a,
        &first_page.documents()[..2],
    )
    .await;
    assert_eq!(first_write.requested(), 2);
    assert_eq!(first_write.written(), 2);
    let repeated_write = upsert_documents(
        &fixture.database,
        project,
        first,
        selector_a,
        &first_page.documents()[..2],
    )
    .await;
    assert_eq!(repeated_write.written(), 0);
    assert_eq!(repeated_write.unchanged(), 2);
    assert_readiness(
        &fixture.database,
        project,
        selector_a,
        SemanticReadinessState::CoverageIncomplete,
        3,
        2,
    )
    .await;

    upsert_documents(
        &fixture.database,
        project,
        first,
        selector_a,
        second_page.documents(),
    )
    .await;
    assert_readiness(
        &fixture.database,
        project,
        selector_a,
        SemanticReadinessState::Ready,
        3,
        3,
    )
    .await;
}

async fn assert_vector_similarity(fixture: &Fixture, scenario: &SemanticScenario) {
    let project = &scenario.project;
    let first = &scenario.first;
    let selector_a = &scenario.selector_a;
    let hits = vector_search(
        &fixture.database,
        project,
        first,
        selector_a,
        [1.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].document_id().as_str(), DOCUMENT_A);
    assert_eq!(hits[1].document_id().as_str(), DOCUMENT_B);
    assert_eq!(hits[2].document_id().as_str(), DOCUMENT_C);
    assert!(
        hits.windows(2)
            .all(|pair| pair[0].distance() <= pair[1].distance())
    );
    let rerank_text = hits[0]
        .rerank_text()
        .unwrap_or_else(|| panic!("vector hit omitted bounded reranker text"));
    assert!(rerank_text.contains("alpha_symbol"));
    let serialized = serde_json::to_string(&hits[0])
        .unwrap_or_else(|error| panic!("vector hit serialization failed: {error}"));
    assert!(!serialized.contains("documentation for alpha_symbol"));
    let materialized = fixture
        .database
        .rebuild_current_similarity_edges(
            project,
            SimilarityMaterializationPolicy::new(2, 0.0, STATEMENT_TIMEOUT)
                .unwrap_or_else(|error| panic!("similarity policy failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("similarity materialization failed: {error}"));
    assert_eq!(materialized.models, 1);
    assert_eq!(materialized.source_symbols, 3);
    assert_eq!(materialized.model_symbol_pairs, 3);
    assert_eq!(materialized.edges_written, 6);
    let similar = fixture
        .database
        .similar_current_symbols(
            SimilarSymbolsRequest::new(SimilarSymbolsInput {
                project_id: project.clone(),
                expected_generation_id: first.clone(),
                source_symbol_id: symbol(SYMBOL_A),
                model_id: Some(selector_a.model_id().clone()),
                limit: 1,
                minimum_score: 0.0,
                same_language: false,
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("similarity request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("materialized similarity query failed: {error}"));
    assert_eq!(similar.hits().len(), 1);
    assert!(similar.truncated());
    assert_eq!(
        similar.hits()[0].symbol().symbol_id().map(SymbolId::as_str),
        Some(SYMBOL_B)
    );
    assert_eq!(
        similar.hits()[0].symbol().symbol_kind(),
        Some(SymbolKind::Function)
    );

    let language_filtered = fixture
        .database
        .similar_current_symbols(
            SimilarSymbolsRequest::new(SimilarSymbolsInput {
                project_id: project.clone(),
                expected_generation_id: first.clone(),
                source_symbol_id: symbol(SYMBOL_A),
                model_id: Some(selector_a.model_id().clone()),
                limit: 1,
                minimum_score: 0.0,
                same_language: true,
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("language-filtered request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("language-filtered similarity query failed: {error}"));
    assert_eq!(
        language_filtered.hits()[0].symbol().symbol_kind(),
        Some(SymbolKind::Function)
    );
}

async fn assert_semantic_clone_findings(fixture: &Fixture, scenario: &SemanticScenario) {
    let project = &scenario.project;
    assert_eq!(
        fixture
            .database
            .refresh_current_structural_findings(project, STATEMENT_TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("semantic clone refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed
    );
    let findings = fixture
        .database
        .current_structural_findings(project, 100)
        .await
        .unwrap_or_else(|error| panic!("semantic clone findings failed: {error}"));
    let findings = serde_json::to_value(findings)
        .unwrap_or_else(|error| panic!("semantic clone findings serialization failed: {error}"));
    let findings = findings
        .as_array()
        .unwrap_or_else(|| panic!("semantic clone findings were not an array"));
    let semantic_classes = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding["qualifiedName"].as_str(),
                Some("alpha_symbol" | "beta_symbol")
            ) && finding["finding"] == "duplicate_code"
                && finding["metricName"] == "semantic_clone_peers"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        semantic_classes.len(),
        1,
        "semantic clone class was not represented exactly once: {findings:?}"
    );
    let semantic_detail = semantic_classes[0];
    assert_eq!(semantic_detail["detail"]["cloneType"], "semantic");
    assert_eq!(semantic_detail["detail"]["classSize"], 2);
    assert_eq!(semantic_detail["detail"]["recordScope"], "clone_class");
    assert_eq!(semantic_detail["detail"]["semanticThreshold"], 0.95);
    assert_eq!(
        semantic_detail["detail"]["members"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert!(!findings.iter().any(|finding| {
        finding["qualifiedName"] == "gamma_symbol" && finding["finding"] == "duplicate_code"
    }));
}

async fn assert_non_clique_semantic_clone_component(
    fixture: &Fixture,
    scenario: &SemanticScenario,
) {
    let statement = format!(
        r#"UPDATE "{}"."symbol_similarity_edges"
            SET score = CASE
                WHEN (
                    source_symbol_id = CAST($4 AS uuid)
                    AND target_symbol_id = CAST($5 AS uuid)
                ) OR (
                    source_symbol_id = CAST($5 AS uuid)
                    AND target_symbol_id = CAST($4 AS uuid)
                ) THEN 0.90
                ELSE 0.97
            END
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND model_id = CAST($3 AS uuid)"#,
        fixture.schema
    );
    let updated = query(AssertSqlSafe(statement))
        .bind(scenario.project.as_str())
        .bind(scenario.first.as_str())
        .bind(scenario.selector_a.model_id().as_str())
        .bind(SYMBOL_A)
        .bind(SYMBOL_B)
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("semantic chain fixture failed: {error}"));
    assert_eq!(updated.rows_affected(), 6);

    assert_eq!(
        fixture
            .database
            .refresh_current_structural_findings(&scenario.project, STATEMENT_TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("semantic chain refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed
    );

    let findings = fixture
        .database
        .current_structural_findings(&scenario.project, 100)
        .await
        .unwrap_or_else(|error| panic!("semantic chain findings failed: {error}"));
    let findings = serde_json::to_value(findings)
        .unwrap_or_else(|error| panic!("semantic chain findings serialization failed: {error}"));
    let semantic_classes = findings
        .as_array()
        .unwrap_or_else(|| panic!("semantic chain findings were not an array"))
        .iter()
        .filter(|finding| {
            matches!(
                finding["qualifiedName"].as_str(),
                Some("alpha_symbol" | "beta_symbol" | "gamma_symbol")
            ) && finding["finding"] == "duplicate_code"
                && finding["metricName"] == "semantic_clone_peers"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        semantic_classes.len(),
        1,
        "non-clique semantic component was not represented exactly once: {findings:?}"
    );
    let representative = semantic_classes[0];
    assert_eq!(representative["qualifiedName"], "alpha_symbol");
    assert_eq!(representative["detail"]["classSize"], 3);
    assert_eq!(
        representative["detail"]["members"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(representative["detail"]["maximumSemanticScore"], 0.97);
}

async fn prepare_replacement_and_retire(
    fixture: &Fixture,
    scenario: &SemanticScenario,
) -> cartograph_db::EmbeddingPageCursor {
    let project = &scenario.project;
    let first = &scenario.first;
    let selector_a = &scenario.selector_a;
    let selector_b = &scenario.selector_b;
    register_model(&fixture.database, model_registration(ModelFixture::B)).await;
    let hnsw_b = fixture
        .database
        .ensure_embedding_model_hnsw(selector_b, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("model B HNSW creation failed: {error}"));
    assert!(hnsw_b.ready());
    let premature_retirement = fixture
        .database
        .retire_embedding_model(
            RetireEmbeddingModelRequest::new(
                selector_a.clone(),
                selector_b.clone(),
                STATEMENT_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("retirement request failed: {error}")),
        )
        .await;
    assert_eq!(
        premature_retirement,
        Err(SemanticStorageError::ReplacementNotReady)
    );
    let model_b_page = pending_page(&fixture.database, project, selector_b, None).await;
    upsert_documents(
        &fixture.database,
        project,
        first,
        selector_b,
        model_b_page.documents(),
    )
    .await;
    let stale_cursor = model_b_page
        .next_cursor()
        .cloned()
        .unwrap_or_else(|| panic!("model B first page did not expose a cursor"));
    let model_b_tail = if let Some(cursor) = model_b_page.next_cursor().cloned() {
        pending_page(&fixture.database, project, selector_b, Some(cursor)).await
    } else {
        panic!("model B first page unexpectedly contained every fixture document")
    };
    upsert_documents(
        &fixture.database,
        project,
        first,
        selector_b,
        model_b_tail.documents(),
    )
    .await;
    assert_readiness(
        &fixture.database,
        project,
        selector_b,
        SemanticReadinessState::Ready,
        3,
        3,
    )
    .await;
    let retired = fixture
        .database
        .retire_embedding_model(
            RetireEmbeddingModelRequest::new(
                selector_a.clone(),
                selector_b.clone(),
                STATEMENT_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("retirement request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("audited retirement failed: {error}"));
    assert!(retired.state().is_retired());
    assert_readiness(
        &fixture.database,
        project,
        selector_a,
        SemanticReadinessState::ModelRetired,
        3,
        3,
    )
    .await;
    stale_cursor
}

async fn assert_generation_cursor_refresh(
    fixture: &Fixture,
    scenario: &SemanticScenario,
    stale_cursor: cartograph_db::EmbeddingPageCursor,
) {
    let project = &scenario.project;
    let selector_b = &scenario.selector_b;
    let second = publish_generation(&fixture.database, project, 2).await;
    let stale_page = PendingEmbeddingPageRequest::new(PendingEmbeddingPageInput {
        project_id: project.clone(),
        model: selector_b.clone(),
        maximum_documents: PAGE_DOCUMENTS,
        maximum_source_bytes: PAGE_BYTES,
        statement_timeout: STATEMENT_TIMEOUT,
    })
    .and_then(|request| request.with_cursor(stale_cursor))
    .unwrap_or_else(|error| panic!("stale page request failed: {error}"));
    assert_eq!(
        fixture
            .database
            .pending_current_embedding_documents(stale_page)
            .await,
        Err(SemanticStorageError::CurrentGenerationChanged)
    );
    let current_page = pending_page(&fixture.database, project, selector_b, None).await;
    assert_eq!(current_page.generation_id(), &second);
    assert!(current_page.documents().is_empty());
    assert_readiness(
        &fixture.database,
        project,
        selector_b,
        SemanticReadinessState::Ready,
        3,
        3,
    )
    .await;
}

async fn assert_semantic_retention(fixture: &Fixture, scenario: &SemanticScenario) {
    let project = &scenario.project;
    let first = &scenario.first;
    let selector_b = &scenario.selector_b;
    let lease = fixture
        .database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "semantic-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("retention lease failed: {error}"));
    let byte_limited = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(0, 10)
                .and_then(|policy| policy.with_work_limits(100, 1, 1))
                .unwrap_or_else(|error| panic!("byte-limited retention policy failed: {error}")),
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("byte-limited retention failed: {error}"));
    assert_eq!(byte_limited.removed(), 0);
    assert_eq!(byte_limited.cascade_rows_removed, 0);
    assert_eq!(byte_limited.search_relations_removed, 0);
    assert_eq!(
        byte_limited.deferred_reason,
        Some("search_relation_byte_budget")
    );
    let row_limited = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(0, 10)
                .and_then(|policy| policy.with_work_limits(1, 8 * 1_024 * 1_024 * 1_024, 1))
                .unwrap_or_else(|error| panic!("row-limited retention policy failed: {error}")),
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("row-limited retention failed: {error}"));
    assert_eq!(row_limited.removed(), 0);
    assert_eq!(row_limited.cascade_rows_removed, 1);
    assert_eq!(row_limited.embeddings_removed, 1);
    assert_eq!(row_limited.search_relations_removed, 1);
    assert!(row_limited.search_relation_bytes_removed > 0);
    assert_eq!(row_limited.retiring_remaining, 1);
    let retention = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(0, 10)
                .unwrap_or_else(|error| panic!("retention policy failed: {error}")),
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("retention failed: {error}"));
    assert_eq!(retention.superseded_removed, 1);
    assert_eq!(retention.embeddings_removed, 5);
    assert_eq!(retention.search_relations_removed, 0);
    assert_eq!(retention.search_relation_bytes_removed, 0);
    assert!(retention.cascade_rows_removed >= 11);
    fixture
        .database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("retention lease release failed: {error}"));
    assert_embedding_count(fixture, first, selector_b.model_id(), 0).await;
    assert_readiness(
        &fixture.database,
        project,
        selector_b,
        SemanticReadinessState::Ready,
        3,
        3,
    )
    .await;
    assert!(
        fixture
            .database
            .embedding_model_hnsw_status(selector_b)
            .await
            .is_ok_and(|status| status.ready())
    );
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB/pgvector test database"]
async fn retired_embedding_maintenance_is_auditable_dry_run_first_and_model_scoped() {
    let fixture = open_fixture().await;
    migrate(&fixture).await;
    let project = setup_retired_embedding_fixture(&fixture).await;

    let before = fixture
        .database
        .embedding_storage_audit(&project, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("embedding audit failed: {error}"));
    assert_eq!(before.active_models, 1);
    assert_eq!(before.retired_models, 1);
    assert_eq!(before.current_documents, 3);
    assert_eq!(before.current_embeddings, 6);
    assert_eq!(before.historical_embeddings, 0);
    assert_eq!(before.retired_model_embeddings, 3);
    assert_eq!(before.model_indexes, 2);

    let dry_run = fixture
        .database
        .cleanup_retired_embeddings(&project, false, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("embedding cleanup dry-run failed: {error}"));
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.candidate_embeddings, 3);
    assert_eq!(dry_run.deleted_embeddings, 0);
    assert_eq!(dry_run.dropped_model_indexes, 0);
    let after_dry_run = fixture
        .database
        .embedding_storage_audit(&project, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("post-dry-run embedding audit failed: {error}"));
    assert_eq!(after_dry_run, before);

    let applied = fixture
        .database
        .cleanup_retired_embeddings(&project, true, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("embedding cleanup failed: {error}"));
    assert!(!applied.dry_run);
    assert_eq!(applied.candidate_embeddings, 3);
    assert_eq!(applied.deleted_embeddings, 3);
    assert_eq!(applied.dropped_model_indexes, 1);

    let after = fixture
        .database
        .embedding_storage_audit(&project, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("post-cleanup embedding audit failed: {error}"));
    assert_eq!(after.active_models, 1);
    assert_eq!(after.retired_models, 1);
    assert_eq!(after.current_documents, 3);
    assert_eq!(after.current_embeddings, 3);
    assert_eq!(after.historical_embeddings, 0);
    assert_eq!(after.retired_model_embeddings, 0);
    assert_eq!(after.model_indexes, 1);
    assert!(matches!(
        fixture
            .database
            .embedding_storage_audit(&project, Duration::ZERO)
            .await,
        Err(SemanticStorageError::InvalidInput {
            field: "statement_timeout"
        })
    ));
    assert!(matches!(
        fixture
            .database
            .cleanup_retired_embeddings(&project, false, Duration::ZERO)
            .await,
        Err(SemanticStorageError::InvalidInput {
            field: "statement_timeout"
        })
    ));

    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

async fn setup_retired_embedding_fixture(fixture: &Fixture) -> ProjectId {
    let model_a = model_registration(ModelFixture::A);
    let model_b = model_registration(ModelFixture::B);
    let selector_a = model_a.selector();
    let selector_b = model_b.selector();
    register_model(&fixture.database, model_a).await;
    register_model(&fixture.database, model_b).await;
    fixture
        .database
        .ensure_embedding_model_hnsw(&selector_a, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("model A HNSW creation failed: {error}"));
    fixture
        .database
        .ensure_embedding_model_hnsw(&selector_b, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("model B HNSW creation failed: {error}"));

    let project = register_project(&fixture.database).await;
    let generation = publish_generation(&fixture.database, &project, 20).await;
    embed_all_pending(&fixture.database, &project, &generation, &selector_a).await;
    embed_all_pending(&fixture.database, &project, &generation, &selector_b).await;
    fixture
        .database
        .retire_embedding_model(
            RetireEmbeddingModelRequest::new(
                selector_a.clone(),
                selector_b.clone(),
                STATEMENT_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("retirement request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("model retirement failed: {error}"));
    project
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB/pgvector test database"]
async fn filtered_hnsw_scan_is_bounded_index_backed_and_generation_isolated() {
    let fixture = open_fixture().await;
    migrate(&fixture).await;
    let selector = model_registration(ModelFixture::A).selector();
    register_model(&fixture.database, model_registration(ModelFixture::A)).await;
    let hnsw = fixture
        .database
        .ensure_embedding_model_hnsw(&selector, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("filtered HNSW creation failed: {error}"));

    let project = register_project(&fixture.database).await;
    let superseded = publish_generation(&fixture.database, &project, 10).await;
    let current = publish_generation(&fixture.database, &project, 11).await;
    seed_semantic_scale(
        &fixture,
        SemanticScaleInput::new(
            &project,
            &superseded,
            &selector,
            "superseded",
            FILTERED_HNSW_DISTRACTOR_ROWS,
        ),
    )
    .await;

    let other_project = register_project_named(
        &fixture.database,
        "semantic/other-project",
        OTHER_PROJECT_FINGERPRINT,
    )
    .await;
    let other_current = publish_generation(&fixture.database, &other_project, 12).await;
    seed_semantic_scale(
        &fixture,
        SemanticScaleInput::new(
            &other_project,
            &other_current,
            &selector,
            "other-project",
            FILTERED_HNSW_DISTRACTOR_ROWS,
        ),
    )
    .await;

    seed_semantic_scale(
        &fixture,
        SemanticScaleInput::new(
            &project,
            &current,
            &selector,
            "current",
            FILTERED_HNSW_TARGET_ROWS,
        ),
    )
    .await;
    embed_all_pending(&fixture.database, &project, &current, &selector).await;
    analyze_semantic_scale(&fixture).await;

    assert_readiness(
        &fixture.database,
        &project,
        &selector,
        SemanticReadinessState::Ready,
        u64::try_from(FILTERED_HNSW_TARGET_ROWS).unwrap_or_default() + 3,
        u64::try_from(FILTERED_HNSW_TARGET_ROWS).unwrap_or_default() + 3,
    )
    .await;
    assert_filtered_hnsw_plan(&fixture, &project, &current, &selector, hnsw.index_name()).await;
    let hits = filtered_vector_search(&fixture.database, &project, &current, &selector).await;
    assert_eq!(hits.len(), usize::from(FILTERED_HNSW_LIMIT));
    assert!(hits.iter().all(|hit| hit.generation_id() == &current));

    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

#[test]
fn semantic_request_boundaries_reject_invalid_dimensions_vectors_and_batches() {
    let model = model_registration(ModelFixture::A);
    assert!(
        EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
            model_id: model.model_id().clone(),
            fingerprint: model.fingerprint().clone(),
            provider: "openai-compatible".to_owned(),
            model_name: "fixture".to_owned(),
            dimension: 0,
            normalization: EmbeddingNormalization::None,
        })
        .is_err()
    );
    assert!(
        EmbeddingUpsertRow::new(document(DOCUMENT_A), digest(MODEL_A_FINGERPRINT), vec![]).is_err()
    );
    assert!(
        EmbeddingUpsertRow::new(
            document(DOCUMENT_A),
            digest(MODEL_A_FINGERPRINT),
            vec![f32::NAN, 1.0],
        )
        .is_err()
    );
    assert!(
        EmbeddingUpsertRow::new(
            document(DOCUMENT_A),
            digest(MODEL_A_FINGERPRINT),
            vec![0.0, 0.0, 0.0],
        )
        .is_err()
    );
}

#[derive(Clone, Copy)]
enum ModelFixture {
    A,
    B,
}

struct Fixture {
    database: CartographDatabase,
    pool: sqlx_postgres::PgPool,
    schema: String,
    _schema_guard: TestSchemaGuard,
}

async fn open_fixture() -> Fixture {
    let database_url = env::var(TEST_DATABASE_URL_ENV)
        .unwrap_or_else(|_| panic!("{TEST_DATABASE_URL_ENV} must be set"));
    let schema = format!(
        "cartograph_semantic_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let settings = DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
        .and_then(|settings| settings.with_schema(&schema))
        .unwrap_or_else(|error| panic!("semantic settings failed: {error}"));
    let pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("semantic database connection failed: {error}"));
    Fixture {
        database: CartographDatabase::new(pool.clone(), settings.schema().clone()),
        pool,
        schema,
        _schema_guard: TestSchemaGuard::new(database_url, settings.schema().as_str())
            .unwrap_or_else(|error| panic!("semantic schema guard failed: {error}")),
    }
}

async fn migrate(fixture: &Fixture) {
    let report = fixture
        .database
        .migrate()
        .await
        .unwrap_or_else(|error| panic!("semantic migration failed: {error}"));
    assert_eq!(
        report.applied_versions,
        [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43,
        ]
    );
    assert_eq!(report.current_version, latest_schema_version());
}

fn model_registration(fixture: ModelFixture) -> EmbeddingModelRegistration {
    let (id, fingerprint, model) = match fixture {
        ModelFixture::A => (MODEL_A_ID, MODEL_A_FINGERPRINT, "fixture-a"),
        ModelFixture::B => (MODEL_B_ID, MODEL_B_FINGERPRINT, "fixture-b"),
    };
    EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: ModelId::parse(id).unwrap_or_else(|error| panic!("model ID failed: {error}")),
        fingerprint: digest(fingerprint),
        provider: "openai-compatible".to_owned(),
        model_name: model.to_owned(),
        dimension: VECTOR_DIMENSION,
        normalization: EmbeddingNormalization::None,
    })
    .unwrap_or_else(|error| panic!("model registration failed: {error}"))
}

fn conflicting_model() -> EmbeddingModelRegistration {
    EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: ModelId::parse(MODEL_A_ID)
            .unwrap_or_else(|error| panic!("model ID failed: {error}")),
        fingerprint: digest(MODEL_B_FINGERPRINT),
        provider: "openai-compatible".to_owned(),
        model_name: "conflict".to_owned(),
        dimension: VECTOR_DIMENSION,
        normalization: EmbeddingNormalization::None,
    })
    .unwrap_or_else(|error| panic!("conflicting model fixture failed: {error}"))
}

fn conflicting_fingerprint_model() -> EmbeddingModelRegistration {
    EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: ModelId::parse(MODEL_B_ID)
            .unwrap_or_else(|error| panic!("model ID failed: {error}")),
        fingerprint: digest(MODEL_A_FINGERPRINT),
        provider: "openai-compatible".to_owned(),
        model_name: "fingerprint-conflict".to_owned(),
        dimension: VECTOR_DIMENSION,
        normalization: EmbeddingNormalization::None,
    })
    .unwrap_or_else(|error| panic!("fingerprint conflict fixture failed: {error}"))
}

async fn assert_dimension_trigger_rejects(
    fixture: &Fixture,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
) {
    let statement = format!(
        r#"INSERT INTO "{}"."document_embeddings" (
                project_id, generation_id, document_id, model_id, source_digest, embedding
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), CAST($4 AS uuid),
                $5, '[1,0]'::vector
            )"#,
        fixture.schema
    );
    let result = query(AssertSqlSafe(statement))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(DOCUMENT_A)
        .bind(selector.model_id().as_str())
        .bind(MODEL_A_FINGERPRINT)
        .execute(&fixture.pool)
        .await;
    assert!(result.is_err());
    assert_embedding_count(fixture, generation, selector.model_id(), 0).await;
}

async fn register_model(
    database: &CartographDatabase,
    model: EmbeddingModelRegistration,
) -> cartograph_db::RegisteredEmbeddingModel {
    database
        .register_embedding_model(model, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("model registration failed: {error}"))
}

async fn register_project(database: &CartographDatabase) -> ProjectId {
    register_project_named(database, "semantic/project", PROJECT_FINGERPRINT).await
}

async fn register_project_named(
    database: &CartographDatabase,
    root_identity: &str,
    fingerprint: &str,
) -> ProjectId {
    database
        .register_project(NewProject::new(root_identity, digest(fingerprint)))
        .await
        .unwrap_or_else(|error| panic!("semantic project failed: {error}"))
}

struct SemanticScaleInput<'a> {
    project: &'a ProjectId,
    generation: &'a GenerationId,
    selector: &'a cartograph_db::EmbeddingModelSelector,
    label: &'a str,
    rows: i32,
}

impl<'a> SemanticScaleInput<'a> {
    const fn new(
        project: &'a ProjectId,
        generation: &'a GenerationId,
        selector: &'a cartograph_db::EmbeddingModelSelector,
        label: &'a str,
        rows: i32,
    ) -> Self {
        Self {
            project,
            generation,
            selector,
            label,
            rows,
        }
    }
}

async fn seed_semantic_scale(fixture: &Fixture, input: SemanticScaleInput<'_>) {
    let documents = format!(
        r#"INSERT INTO "{}"."search_documents" (
                project_id, generation_id, document_id, path, language,
                document_kind, qualified_name, code, natural_text, metadata
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid), gen_random_uuid(),
                   'scale/' || $3 || '/' || series::text || '.rs',
                   'rust', 'symbol', $3 || '_symbol_' || series::text,
                   'fn scale_fixture() {{}}', '',
                   jsonb_build_object('semantic_scale_fixture', $3)
            FROM generate_series(1, $4) AS series"#,
        fixture.schema
    );
    query(AssertSqlSafe(documents))
        .bind(input.project.as_str())
        .bind(input.generation.as_str())
        .bind(input.label)
        .bind(input.rows)
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("semantic scale documents failed: {error}"));

    let embeddings = format!(
        r#"INSERT INTO "{}"."document_embeddings" (
                project_id, generation_id, document_id, model_id,
                source_digest, embedding
            )
            SELECT project_id, generation_id, document_id, CAST($3 AS uuid),
                   $4, '[1,0,0]'::vector
            FROM "{}"."search_documents"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND metadata ->> 'semantic_scale_fixture' = $5"#,
        fixture.schema, fixture.schema
    );
    query(AssertSqlSafe(embeddings))
        .bind(input.project.as_str())
        .bind(input.generation.as_str())
        .bind(input.selector.model_id().as_str())
        .bind(MODEL_A_FINGERPRINT)
        .bind(input.label)
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("semantic scale embeddings failed: {error}"));
}

async fn embed_all_pending(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
) {
    let mut cursor = None;
    loop {
        let page = pending_page(database, project, selector, cursor).await;
        if page.documents().is_empty() {
            break;
        }
        upsert_documents(database, project, generation, selector, page.documents()).await;
        let Some(next) = page.next_cursor().cloned() else {
            break;
        };
        cursor = Some(next);
    }
}

async fn analyze_semantic_scale(fixture: &Fixture) {
    for table in ["search_documents", "document_embeddings"] {
        let statement = format!(r#"ANALYZE "{}"."{table}""#, fixture.schema);
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("semantic scale analyze failed for {table}: {error}"));
    }
}

async fn assert_filtered_hnsw_plan(
    fixture: &Fixture,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
    expected_index: &str,
) {
    let mut transaction = fixture
        .pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("filtered HNSW explain begin failed: {error}"));
    query(
        r"SELECT set_config('hnsw.iterative_scan', 'strict_order', true),
                  set_config('hnsw.ef_search', '200', true),
                  set_config('hnsw.max_scan_tuples', '100000', true),
                  set_config('hnsw.scan_mem_multiplier', '8', true)",
    )
    .execute(&mut *transaction)
    .await
    .unwrap_or_else(|error| panic!("filtered HNSW explain settings failed: {error}"));
    let settings = query(
        r"SELECT current_setting('hnsw.iterative_scan') AS iterative_scan,
                  current_setting('hnsw.ef_search') AS ef_search,
                  current_setting('hnsw.max_scan_tuples') AS max_scan_tuples,
                  current_setting('hnsw.scan_mem_multiplier') AS scan_mem_multiplier",
    )
    .fetch_one(&mut *transaction)
    .await
    .unwrap_or_else(|error| panic!("filtered HNSW settings read failed: {error}"));
    assert_eq!(read_text(&settings, "iterative_scan"), "strict_order");
    assert_eq!(read_text(&settings, "ef_search"), "200");
    assert_eq!(read_text(&settings, "max_scan_tuples"), "100000");
    assert_eq!(read_text(&settings, "scan_mem_multiplier"), "8");

    let statement = format!(
        r#"EXPLAIN (FORMAT TEXT, COSTS OFF)
            WITH nearest AS MATERIALIZED (
                SELECT embeddings.project_id, embeddings.generation_id,
                       embeddings.document_id,
                       (embeddings.embedding::vector({})
                           <=> CAST($3 AS vector({})))::float8 AS distance
                FROM "{}"."document_embeddings" AS embeddings
                WHERE embeddings.project_id = CAST($1 AS uuid)
                  AND embeddings.generation_id = CAST($2 AS uuid)
                  AND embeddings.model_id = '{}'::uuid
                ORDER BY embeddings.embedding::vector({})
                             <=> CAST($3 AS vector({}))
                LIMIT $4
            )
            SELECT documents.document_id
            FROM nearest
            INNER JOIN "{}"."search_documents" AS documents
              ON documents.project_id = nearest.project_id
             AND documents.generation_id = nearest.generation_id
             AND documents.document_id = nearest.document_id
            ORDER BY nearest.distance, documents.id"#,
        selector.dimension(),
        selector.dimension(),
        fixture.schema,
        selector.model_id(),
        selector.dimension(),
        selector.dimension(),
        fixture.schema
    );
    let rows = query(AssertSqlSafe(statement))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind("[1,0,0]")
        .bind(i64::from(FILTERED_HNSW_LIMIT))
        .fetch_all(&mut *transaction)
        .await
        .unwrap_or_else(|error| panic!("filtered HNSW explain failed: {error}"));
    let plan = rows
        .iter()
        .map(|row| read_text(row, 0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains(expected_index), "filtered HNSW plan: {plan}");
    assert!(!plan.contains("Seq Scan on document_embeddings"), "{plan}");
    transaction
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("filtered HNSW explain rollback failed: {error}"));
}

fn read_text(
    row: &sqlx_postgres::PgRow,
    column: impl sqlx_core::column::ColumnIndex<sqlx_postgres::PgRow>,
) -> String {
    row.try_get::<String, _>(column)
        .unwrap_or_else(|error| panic!("semantic text decode failed: {error}"))
}

async fn publish_generation(
    database: &CartographDatabase,
    project: &ProjectId,
    sequence: u32,
) -> GenerationId {
    let staged = database
        .begin_generation(NewGeneration::new(
            project.clone(),
            format!("semantic-revision-{sequence}"),
            1,
        ))
        .await
        .unwrap_or_else(|error| panic!("semantic generation failed: {error}"));
    let target = LeaseTarget::new(
        project.clone(),
        ProjectOperation::Index,
        Some(staged.generation_id().clone()),
    );
    let lease = database
        .acquire_lease(LeaseRequest::new(
            target,
            LeaseOwner::new(process::id(), format!("semantic-generation-{sequence}")),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("semantic generation lease failed: {error}"));
    let facts = GenerationFacts {
        files: vec![
            semantic_file(FILE_A, "src/a.rs", MODEL_A_FINGERPRINT),
            semantic_file(FILE_B, "src/b.rs", MODEL_B_FINGERPRINT),
            semantic_file(FILE_C, "src/c.rs", PROJECT_FINGERPRINT),
        ],
        symbols: vec![
            semantic_symbol(SYMBOL_A, FILE_A, "alpha_symbol", MODEL_A_FINGERPRINT),
            semantic_symbol(SYMBOL_B, FILE_B, "beta_symbol", MODEL_B_FINGERPRINT),
            semantic_symbol(SYMBOL_C, FILE_C, "gamma_symbol", PROJECT_FINGERPRINT),
        ],
        documents: vec![
            search_document(
                DOCUMENT_A,
                FILE_A,
                SYMBOL_A,
                "src/a.rs",
                "alpha_symbol",
                "fn alpha_symbol() {}",
            ),
            search_document(
                DOCUMENT_B,
                FILE_B,
                SYMBOL_B,
                "src/b.rs",
                "beta_symbol",
                "fn beta_symbol() {}",
            ),
            search_document(
                DOCUMENT_C,
                FILE_C,
                SYMBOL_C,
                "src/c.rs",
                "gamma_symbol",
                "fn gamma_symbol() {}",
            ),
        ],
        ..GenerationFacts::default()
    };
    let (canonical, _) = validate_generation_facts(
        facts,
        GenerationValidationLimits::new(VALIDATION_OUTPUT_BYTES, VALIDATION_WORKING_BYTES)
            .unwrap_or_else(|error| panic!("semantic validation limits failed: {error}")),
        || false,
    )
    .unwrap_or_else(|error| panic!("semantic facts failed: {error}"));
    let ready = database
        .prepare_generation(GenerationContents::new(staged, canonical), &lease.fence())
        .await
        .unwrap_or_else(|error| panic!("semantic prepare failed: {error}"));
    let current = database
        .publish_generation(ready, &lease.fence())
        .await
        .unwrap_or_else(|error| panic!("semantic publication failed: {error}"));
    current.generation_id().clone()
}

fn semantic_file(id: &str, path: &str, content_hash: &str) -> FileInput {
    FileInput {
        file_id: file(id),
        normalized_path: path.to_owned(),
        language: "rust".to_owned(),
        content_hash: digest(content_hash),
        byte_size: 128,
        parse_status: FileParseStatus::Parsed,
    }
}

fn semantic_symbol(id: &str, file_id: &str, name: &str, structural_hash: &str) -> SymbolInput {
    SymbolInput {
        symbol_id: symbol(id),
        file_id: file(file_id),
        symbol_kind: "function".to_owned(),
        qualified_name: name.to_owned(),
        signature: format!("fn {name}()"),
        start_byte: 0,
        end_byte: 96,
        start_line: 1,
        end_line: 8,
        structural_digest: digest(structural_hash),
        visibility: None,
        export: cartograph_domain::SymbolExportFlags::named(true),
        execution: cartograph_domain::SymbolExecutionFlags::default(),
        declaration_only: false,
        betweenness_ppb: None,
        pagerank_ppb: None,
    }
}

fn search_document(
    id: &str,
    file_id: &str,
    symbol_id: &str,
    path: &str,
    name: &str,
    code: &str,
) -> SearchDocumentInput {
    SearchDocumentInput {
        document_id: document(id),
        file_id: Some(file(file_id)),
        symbol_id: Some(symbol(symbol_id)),
        path: path.to_owned(),
        language: "rust".to_owned(),
        kind: DocumentKind::Symbol,
        qualified_name: name.to_owned(),
        code: code.to_owned(),
        natural_text: format!("documentation for {name}"),
        metadata: serde_json::json!({"fixture": true}),
    }
}

async fn pending_page(
    database: &CartographDatabase,
    project: &ProjectId,
    selector: &cartograph_db::EmbeddingModelSelector,
    cursor: Option<cartograph_db::EmbeddingPageCursor>,
) -> cartograph_db::PendingEmbeddingPage {
    let request = PendingEmbeddingPageRequest::new(PendingEmbeddingPageInput {
        project_id: project.clone(),
        model: selector.clone(),
        maximum_documents: PAGE_DOCUMENTS,
        maximum_source_bytes: PAGE_BYTES,
        statement_timeout: STATEMENT_TIMEOUT,
    })
    .and_then(|request| match cursor {
        Some(cursor) => request.with_cursor(cursor),
        None => Ok(request),
    })
    .unwrap_or_else(|error| panic!("pending request failed: {error}"));
    database
        .pending_current_embedding_documents(request)
        .await
        .unwrap_or_else(|error| panic!("pending page failed: {error}"))
}

fn embedding_row(
    document: &cartograph_db::PendingEmbeddingDocument,
    vector: [f32; 3],
) -> EmbeddingUpsertRow {
    EmbeddingUpsertRow::new(
        document.document_id().clone(),
        document.source_digest().clone(),
        vector.to_vec(),
    )
    .unwrap_or_else(|error| panic!("embedding row failed: {error}"))
}

async fn upsert_documents(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
    documents: &[cartograph_db::PendingEmbeddingDocument],
) -> cartograph_db::EmbeddingBatchUpsertReport {
    let rows = documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let vector = match document.document_id().as_str() {
                DOCUMENT_A => [1.0, 0.0, 0.0],
                DOCUMENT_B => [0.999, 0.001, 0.0],
                _ if index % 2 == 0 => [0.0, 0.0, 1.0],
                _ => [0.0, 0.0, 1.0],
            };
            embedding_row(document, vector)
        })
        .collect();
    database
        .upsert_current_document_embeddings(
            EmbeddingBatchUpsertRequest::new(EmbeddingBatchUpsertInput {
                project_id: project.clone(),
                generation_id: generation.clone(),
                model: selector.clone(),
                rows,
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("embedding batch failed validation: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("embedding batch write failed: {error}"))
}

async fn assert_readiness(
    database: &CartographDatabase,
    project: &ProjectId,
    selector: &cartograph_db::EmbeddingModelSelector,
    expected: SemanticReadinessState,
    documents: u64,
    embedded: u64,
) {
    let report = database
        .semantic_readiness(
            SemanticReadinessRequest::new(project.clone(), selector.clone(), STATEMENT_TIMEOUT)
                .unwrap_or_else(|error| panic!("readiness request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("semantic readiness failed: {error}"));
    assert_eq!(report.state(), expected);
    assert_eq!(report.documents(), documents);
    assert_eq!(report.embedded(), embedded);
    assert_eq!(report.ready(), expected == SemanticReadinessState::Ready);
}

async fn vector_search(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
    vector: [f32; 3],
) -> Vec<cartograph_db::VectorSearchHit> {
    database
        .vector_top_k(
            VectorSearchRequest::new(VectorSearchInput {
                project_id: project.clone(),
                expected_generation_id: generation.clone(),
                model: selector.clone(),
                vector: vector.to_vec(),
                limit: 10,
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("vector request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("vector search failed: {error}"))
}

async fn filtered_vector_search(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
    selector: &cartograph_db::EmbeddingModelSelector,
) -> Vec<cartograph_db::VectorSearchHit> {
    database
        .vector_top_k(
            VectorSearchRequest::new(VectorSearchInput {
                project_id: project.clone(),
                expected_generation_id: generation.clone(),
                model: selector.clone(),
                vector: vec![1.0, 0.0, 0.0],
                limit: FILTERED_HNSW_LIMIT,
                statement_timeout: STATEMENT_TIMEOUT,
            })
            .unwrap_or_else(|error| panic!("filtered vector request failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("filtered vector search failed: {error}"))
}

async fn assert_embedding_count(
    fixture: &Fixture,
    generation: &GenerationId,
    model: &ModelId,
    expected: i64,
) {
    let statement = format!(
        r#"SELECT count(*)::bigint AS count
            FROM "{}"."document_embeddings"
            WHERE generation_id = CAST($1 AS uuid) AND model_id = CAST($2 AS uuid)"#,
        fixture.schema
    );
    let row = query(AssertSqlSafe(statement))
        .bind(generation.as_str())
        .bind(model.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("embedding count failed: {error}"));
    assert_eq!(
        row.try_get::<i64, _>("count")
            .unwrap_or_else(|error| panic!("embedding count decode failed: {error}")),
        expected
    );
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#);
    query(AssertSqlSafe(statement))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("semantic schema cleanup failed: {error}"));
}

fn document(raw: &str) -> DocumentId {
    DocumentId::parse(raw).unwrap_or_else(|error| panic!("document ID failed: {error}"))
}

fn file(raw: &str) -> FileId {
    FileId::parse(raw).unwrap_or_else(|error| panic!("file ID failed: {error}"))
}

fn symbol(raw: &str) -> SymbolId {
    SymbolId::parse(raw).unwrap_or_else(|error| panic!("symbol ID failed: {error}"))
}

fn digest(raw: &str) -> ContentDigest {
    ContentDigest::parse(raw).unwrap_or_else(|error| panic!("digest failed: {error}"))
}
