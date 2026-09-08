use super::*;
use cartograph_domain::GenerationState;

const DOCUMENTS: i64 = 21_003;

pub(super) async fn assert_search_budget_progress(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let project = database
        .register_project(NewProject::new(
            "storage/search-budget",
            digest(b"search-budget"),
        ))
        .await
        .unwrap_or_else(|error| panic!("search budget project failed: {error}"));
    let expensive = prepare_empty_generation(database, &project, "search-budget").await;
    query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."index_generations" SET state = 'failed'
            WHERE project_id = $1::uuid AND generation_id = $2::uuid"#
    )))
    .bind(project.as_str())
    .bind(expensive.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("search budget terminalization failed: {error}"));
    let cheap = create_generation_cascade_fixture(database, pool, schema, &project).await;
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "search-budget-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("search budget lease failed: {error}"));
    let small_budget = GenerationRetentionPolicy::new(0, 1)
        .and_then(|policy| policy.with_maximum_search_relation_bytes(1))
        .unwrap_or_else(|error| panic!("search byte policy failed: {error}"));
    let first = database
        .cleanup_generations(GenerationRetentionRequest::new(
            small_budget,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("search budget pass failed: {error}"));
    assert_eq!(first.failed_removed, 1);
    assert_eq!(first.search_relations_removed, 0);
    assert_eq!(
        count_rows(pool, schema, "index_generations", &cheap).await,
        0
    );
    assert_eq!(
        count_rows(pool, schema, "index_generations", &expensive).await,
        1
    );
    let blocked = database
        .cleanup_generations(GenerationRetentionRequest::new(
            small_budget,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("search budget diagnostic failed: {error}"));
    assert_eq!(blocked.cascade_rows_removed, 0);
    assert_eq!(blocked.retiring_remaining, 0);
    assert_eq!(blocked.deferred_reason, Some("search_relation_byte_budget"));
    let larger_budget = small_budget
        .with_maximum_search_relation_bytes(64 * 1024 * 1024)
        .unwrap_or_else(|error| panic!("search budget override failed: {error}"));
    let resumed = database
        .cleanup_generations(GenerationRetentionRequest::new(
            larger_budget,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("search budget resume failed: {error}"));
    assert_eq!(resumed.failed_removed, 1);
    assert_eq!(resumed.search_relations_removed, 1);
    assert!(resumed.search_relation_bytes_removed > 1);
    assert!(resumed.search_relation_bytes_removed <= 64 * 1024 * 1024);
    assert_eq!(
        count_rows(pool, schema, "index_generations", &expensive).await,
        0
    );
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("search budget lease release failed: {error}"));
}

pub(super) async fn assert_retention_progress(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let project = database
        .register_project(NewProject::new("storage/progress", digest(b"progress")))
        .await
        .unwrap_or_else(|error| panic!("progress project failed: {error}"));
    let generation = create_generation_cascade_fixture(database, pool, schema, &project).await;
    insert_embedded_documents(pool, schema, &project, &generation).await;
    install_delete_failure(pool, schema).await;
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "progress-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("progress lease failed: {error}"));
    let policy = GenerationRetentionPolicy::new(0, 1)
        .unwrap_or_else(|error| panic!("progress policy failed: {error}"));
    let report = database
        .cleanup_generations(GenerationRetentionRequest::new(
            policy,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("committed progress was lost: {error}"));
    assert_eq!(report.removed(), 0);
    assert_eq!(report.embeddings_removed, 20_000);
    assert_eq!(report.cascade_rows_removed, 20_000);
    assert_eq!(report.batches_committed, 2);
    assert_eq!(report.retiring_remaining, 1);
    assert_eq!(report.deferred_reason, Some("drain-generation-rows"));
    assert_eq!(
        count_rows(pool, schema, "document_embeddings", &generation).await,
        DOCUMENTS - 20_000
    );
    assert_eq!(
        count_rows(pool, schema, "search_documents", &generation).await,
        DOCUMENTS
    );
    assert_eq!(
        database
            .generation_state(&project, &generation)
            .await
            .unwrap_or_else(|error| panic!("retirement state failed: {error}")),
        Some(GenerationState::Retiring)
    );
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("progress release failed: {error}"));
    finish_retirement(
        database,
        pool,
        schema,
        &project,
        &generation,
        report.cascade_rows_removed,
    )
    .await;
}

async fn finish_retirement(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    already_removed: u64,
) {
    let blocked = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(
                project.clone(),
                ProjectOperation::Index,
                Some(generation.clone()),
            ),
            LeaseOwner::new(process::id(), "retired-writer"),
            LEASE_DURATION,
        ))
        .await;
    assert!(
        matches!(blocked, Err(LeaseError::Busy)),
        "a retired generation admitted a writer"
    );
    query(AssertSqlSafe(format!(
        r#"DROP TRIGGER fail_retirement_document ON "{schema}"."search_documents""#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("failure removal failed: {error}"));
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "resume-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("resume lease failed: {error}"));
    let budget = GenerationRetentionPolicy::new(0, 1)
        .and_then(|policy| policy.with_maximum_cascade_rows(3_000))
        .unwrap_or_else(|error| panic!("resume budget failed: {error}"));
    let mut removed = already_removed;
    let mut completed = false;
    for _ in 0..10 {
        let next = database
            .cleanup_generations(GenerationRetentionRequest::new(
                budget,
                &lease.fence(),
                STATEMENT_TIMEOUT,
            ))
            .await
            .unwrap_or_else(|error| panic!("retention resume failed: {error}"));
        assert!(next.cascade_rows_removed > 0 && next.cascade_rows_removed <= 3_000);
        removed += next.cascade_rows_removed;
        if next.removed() == 1 {
            assert_eq!(next.failed_removed, 1);
            assert_eq!(next.retiring_remaining, 0);
            completed = true;
            break;
        }
    }
    assert!(completed, "bounded retries did not converge");
    assert_eq!(removed, 42_006 + CASCADE_FIXTURE_ROWS);
    assert_eq!(
        count_rows(pool, schema, "index_generations", generation).await,
        0
    );
    assert_eq!(
        count_rows(pool, schema, "document_embeddings", generation).await,
        0
    );
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("resume release failed: {error}"));
}

async fn insert_embedded_documents(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
) {
    query(AssertSqlSafe(format!(r#"INSERT INTO "{schema}"."search_documents" (
            project_id, generation_id, document_id, file_id, symbol_id,
            path, language, document_kind, qualified_name
        ) SELECT f.project_id, f.generation_id, gen_random_uuid(), f.file_id, NULL,
            f.normalized_path, 'rust', 'documentation', 'embedded_fixture'
        FROM "{schema}"."files" f JOIN "{schema}"."symbols" s USING (project_id, generation_id, file_id)
        CROSS JOIN generate_series(1, $3::bigint)
        WHERE f.project_id = $1::uuid AND f.generation_id = $2::uuid"#)))
        .bind(project.as_str()).bind(generation.as_str()).bind(DOCUMENTS)
        .execute(pool).await.unwrap_or_else(|error| panic!("document fixture failed: {error}"));
    let model = "00000000-0000-4000-8000-000000000099";
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."embedding_models"
        (model_id, fingerprint, provider, model_name, dimension, normalization)
        VALUES ($1::uuid, repeat('9', 64), 'fixture', 'retention', 3, 'none')"#
    )))
    .bind(model)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("model fixture failed: {error}"));
    query(AssertSqlSafe(format!(r#"INSERT INTO "{schema}"."document_embeddings"
        (project_id, generation_id, document_id, model_id, source_digest, embedding)
        SELECT project_id, generation_id, document_id, $3::uuid, repeat('8', 64), '[1,2,3]'::vector
        FROM "{schema}"."search_documents" WHERE project_id = $1::uuid AND generation_id = $2::uuid"#)))
        .bind(project.as_str()).bind(generation.as_str()).bind(model)
        .execute(pool).await.unwrap_or_else(|error| panic!("embedding fixture failed: {error}"));
}

async fn install_delete_failure(pool: &sqlx_postgres::PgPool, schema: &str) {
    query(AssertSqlSafe(format!(r#"CREATE FUNCTION "{schema}".fail_retirement_document()
        RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected retention failure'; END $$"#)))
        .execute(pool).await.unwrap_or_else(|error| panic!("failure function failed: {error}"));
    query(AssertSqlSafe(format!(r#"CREATE TRIGGER fail_retirement_document BEFORE DELETE
        ON "{schema}"."search_documents" FOR EACH ROW EXECUTE FUNCTION "{schema}".fail_retirement_document()"#)))
        .execute(pool).await.unwrap_or_else(|error| panic!("failure trigger failed: {error}"));
}

async fn count_rows(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    table: &str,
    generation: &GenerationId,
) -> i64 {
    query(AssertSqlSafe(format!(
        r#"SELECT count(*)::bigint FROM "{schema}"."{table}" WHERE generation_id = $1::uuid"#
    )))
    .bind(generation.as_str())
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get(0))
    .unwrap_or_else(|error| panic!("progress count failed: {error}"))
}
