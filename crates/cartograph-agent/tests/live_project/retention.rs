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
    assert!(matches!(
        recovered.retention,
        GenerationRetentionStatus::Completed { .. }
    ));
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
