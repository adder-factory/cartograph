use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn cache_retention_survives_generation_failure_and_protects_the_exact_policy_contract() {
    let (schema, settings, project) = live_project_fixture("4");
    write_incremental_fixture(project.path());
    let runtime = ProjectRuntime::connect(project.path(), &settings)
        .await
        .unwrap_or_else(|error| panic!("retention runtime failed: {error}"));
    let options = IndexOptions::default().with_history_refresh(false);
    let first = initial_incremental_index(&runtime, options.clone()).await;
    assert_eq!(
        first.parse_cache_contract_digest,
        parse_cache_policy_digest(&native_extractor_contract_digest())
    );
    let pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("retention test pool failed: {error}"));
    seed_old_contracts(&settings, &schema, &first.project_id).await;
    query(AssertSqlSafe(format!(
        r#"CREATE TABLE "{schema}".unknown_retention_child (
        project_id uuid, generation_id uuid,
        FOREIGN KEY (project_id, generation_id) REFERENCES "{schema}".index_generations
            (project_id, generation_id) ON DELETE CASCADE)"#
    )))
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("catalog fault failed: {error}"));
    let failed_maintenance = runtime
        .index(options.clone())
        .await
        .unwrap_or_else(|error| panic!("no-op index failed: {error}"));
    assert!(!failed_maintenance.published);
    assert_eq!(failed_maintenance.generation_id, first.generation_id);
    let GenerationRetentionStatus::CacheOnly {
        parse_cache,
        lease_released,
        ..
    } = failed_maintenance.retention
    else {
        panic!(
            "cache cleanup was coupled to generation failure: {:?}",
            failed_maintenance.retention
        );
    };
    assert!(lease_released);
    assert_eq!(parse_cache.rows_removed, 32);
    let usage = runtime
        .database()
        .storage_usage(&first.project_id, 10, Duration::from_secs(10))
        .await
        .unwrap_or_else(|error| panic!("maintenance storage failed: {error}"));
    assert_eq!(usage.parse_cache.rows, 35);
    let attempt = usage
        .retention_maintenance
        .unwrap_or_else(|| panic!("maintenance outcome was not persisted"));
    assert_eq!(attempt.consecutive_failures, 1);
    assert_eq!(
        attempt.outcome["generationFailure"],
        "cascade-catalog-mismatch"
    );
    let protected = query(AssertSqlSafe(format!(
        r#"SELECT count(*)::bigint FROM "{schema}".native_parse_cache
        WHERE project_id = $1::uuid AND extractor_contract_digest = $2"#
    )))
    .bind(first.project_id.as_str())
    .bind(first.parse_cache_contract_digest.as_str())
    .fetch_one(&pool)
    .await
    .and_then(|row| row.try_get::<i64, _>(0))
    .unwrap_or_else(|error| panic!("protected contract count failed: {error}"));
    assert_eq!(protected, 3);
    query(AssertSqlSafe(format!(
        r#"DROP TABLE "{schema}".unknown_retention_child"#
    )))
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("catalog fault cleanup failed: {error}"));
    let recovered = runtime
        .index(options)
        .await
        .unwrap_or_else(|error| panic!("maintenance recovery failed: {error}"));
    assert_matches!(
        recovered.retention,
        GenerationRetentionStatus::Completed { .. }
    );
    let usage = runtime
        .database()
        .storage_usage(&first.project_id, 10, Duration::from_secs(10))
        .await
        .unwrap_or_else(|error| panic!("recovered storage failed: {error}"));
    assert_eq!(
        usage
            .retention_maintenance
            .map(|attempt| attempt.consecutive_failures),
        Some(0)
    );
    runtime.close().await;
    pool.close().await;
    drop_schema(&settings, &schema).await;
}

async fn seed_old_contracts(
    settings: &DatabaseSettings,
    schema: &str,
    project: &cartograph_domain::ProjectId,
) {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("cache seed pool failed: {error}"));
    query(AssertSqlSafe(format!(r#"INSERT INTO "{schema}".native_parse_cache (
        project_id, extractor_contract_digest, path_digest, normalized_path, language,
        content_hash, source_bytes, payload, payload_digest, last_used_at
    ) SELECT $1::uuid, repeat(contract, 64), md5(value::text) || md5(value::text),
        'old/' || value || '.rs', 'rust', repeat('c', 64), 1, decode('01', 'hex'), repeat('d', 64),
        clock_timestamp() + CASE WHEN contract = 'a' THEN interval '1 day' ELSE interval '2 days' END
    FROM generate_series(1,32) AS value CROSS JOIN (VALUES ('a'), ('b')) AS contracts(contract)"#)))
        .bind(project.as_str()).execute(&pool).await.unwrap_or_else(|error| panic!("cache backlog failed: {error}"));
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn automatic_index_drains_failed_generations_before_reserving_another() {
    let (schema, settings, project) = live_project_fixture("4");
    let source = write_incremental_fixture(project.path());
    let runtime = ProjectRuntime::connect(project.path(), &settings)
        .await
        .unwrap_or_else(|error| panic!("backlog runtime failed: {error}"));
    let first = initial_incremental_index(
        &runtime,
        IndexOptions::default().with_history_refresh(false),
    )
    .await;
    let pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("backlog test pool failed: {error}"));
    // One more than an automatic cleanup may delete, plus one over the limit.
    stage_generations(&runtime, &first.project_id, 34).await;
    let fail_staged = || async {
        query(AssertSqlSafe(format!(
            r#"UPDATE "{schema}".index_generations SET state = 'failed'
            WHERE project_id = $1::uuid AND state = 'staging'"#
        )))
        .bind(first.project_id.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("backlog terminalization failed: {error}"));
    };
    fail_staged().await;
    std::fs::write(
        source.join("service.ts"),
        "export function calculateTotal(value: number): number { return value + 2; }\n",
    )
    .unwrap_or_else(|error| panic!("backlog edit failed: {error}"));
    let reserved = || async {
        query(AssertSqlSafe(format!(
            r#"SELECT count(*)::bigint FROM "{schema}".index_generations
            WHERE project_id = $1::uuid AND state IN ('staging', 'ready')"#
        )))
        .bind(first.project_id.as_str())
        .fetch_one(&pool)
        .await
        .and_then(|row| row.try_get::<i64, _>(0))
        .unwrap_or_else(|error| panic!("reserved generation count failed: {error}"))
    };

    // Cleanup progresses but cannot finish in one bounded attempt, so the
    // automatic attempt defers instead of reserving another generation.
    let deferred = runtime.index(IndexOptions::automatic()).await;
    assert_matches!(
        deferred,
        Err(ProjectError::IndexRetentionBacklog),
        "automatic indexing reserved a generation over a draining backlog: {deferred:?}"
    );
    assert_eq!(reserved().await, 0);
    let backlog = runtime
        .database()
        .terminal_generation_backlog(&first.project_id)
        .await
        .unwrap_or_else(|error| panic!("backlog count failed: {error}"));
    assert_eq!(backlog, 2);

    let recovered = runtime
        .index(IndexOptions::automatic())
        .await
        .unwrap_or_else(|error| panic!("drained automatic index failed: {error}"));
    assert!(recovered.published);
    assert_eq!(
        runtime
            .database()
            .terminal_generation_backlog(&first.project_id)
            .await
            .unwrap_or_else(|error| panic!("drained backlog count failed: {error}")),
        0
    );

    // A backlog that cannot drain (another operation holds the project) must
    // not freeze automatic indexing behind a retryable deferral.
    stage_generations(&runtime, &first.project_id, 3).await;
    fail_staged().await;
    std::fs::write(
        source.join("service.ts"),
        "export function calculateTotal(value: number): number { return value + 3; }\n",
    )
    .unwrap_or_else(|error| panic!("second backlog edit failed: {error}"));
    let competitor = runtime
        .database()
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(first.project_id.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "backlog-competitor"),
            Duration::from_mins(1),
        ))
        .await
        .unwrap_or_else(|error| panic!("backlog competitor lease failed: {error}"));
    let blocked = runtime.index(IndexOptions::automatic()).await;
    assert!(
        !matches!(blocked, Err(ProjectError::IndexRetentionBacklog)),
        "a backlog that made no cleanup progress deferred automatic indexing"
    );
    runtime
        .database()
        .release_lease(&competitor)
        .await
        .unwrap_or_else(|error| panic!("backlog competitor release failed: {error}"));
    pool.close().await;
    runtime.close().await;
    drop_schema(&settings, &schema).await;
}

async fn stage_generations(
    runtime: &ProjectRuntime,
    project_id: &cartograph_domain::ProjectId,
    count: usize,
) {
    for index in 0..count {
        runtime
            .database()
            .begin_generation(NewGeneration::new(
                project_id.clone(),
                format!("failed-{index}"),
                1,
            ))
            .await
            .unwrap_or_else(|error| panic!("backlog generation failed: {error}"));
    }
}
