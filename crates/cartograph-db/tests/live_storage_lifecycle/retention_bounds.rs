use super::*;

pub(super) async fn assert_pool_deadline(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let project = database
        .register_project(NewProject::new("storage/deadline", digest(b"deadline")))
        .await
        .unwrap_or_else(|error| panic!("deadline project failed: {error}"));
    let generation = create_generation_cascade_fixture(database, pool, schema, &project).await;
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "deadline-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("deadline lease failed: {error}"));
    let policy = GenerationRetentionPolicy::new(0, 1)
        .unwrap_or_else(|error| panic!("deadline policy failed: {error}"));
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(
            pool.acquire()
                .await
                .unwrap_or_else(|error| panic!("pool fixture failed: {error}")),
        );
    }
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        database.cleanup_generations(GenerationRetentionRequest::new(
            policy,
            &lease.fence(),
            Duration::from_millis(50),
        )),
    )
    .await
    .unwrap_or_else(|_| panic!("pool acquisition exceeded the retention deadline"));
    assert_eq!(
        result,
        Err(cartograph_db::GenerationRetentionError::DatabaseOperation {
            operation: "acquire-deadline",
        })
    );
    drop(held);
    let interrupted = database
        .cleanup_generations_with_observer(
            GenerationRetentionRequest::new(policy, &lease.fence(), Duration::from_millis(100)),
            || async { tokio::time::sleep(Duration::from_secs(10)).await },
        )
        .await;
    assert_eq!(
        interrupted,
        Err(cartograph_db::GenerationRetentionError::DatabaseOperation {
            operation: "batch-deadline",
        })
    );
    let resumed = database
        .cleanup_generations(GenerationRetentionRequest::new(
            policy,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("deadline retry failed: {error}"));
    assert_eq!(resumed.failed_removed, 1);
    assert_eq!(resumed.cascade_rows_removed, CASCADE_FIXTURE_ROWS);
    let remaining = query(AssertSqlSafe(format!(
        r#"SELECT count(*)::bigint FROM "{schema}"."index_generations" WHERE generation_id = $1::uuid"#
    ))).bind(generation.as_str()).fetch_one(pool).await
        .and_then(|row| row.try_get::<i64, _>(0))
        .unwrap_or_else(|error| panic!("deadline retry verification failed: {error}"));
    assert_eq!(remaining, 0);
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("deadline lease release failed: {error}"));
}

pub(super) async fn assert_ddl_budget_progress(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    // With a two-generation budget the next page has one slot; with three,
    // cleanup can also inspect and report the remaining DDL-blocked candidate.
    for maximum_generations in [2, 3] {
        assert_ddl_budget_case(database, pool, schema, maximum_generations).await;
    }
}

async fn assert_ddl_budget_case(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    maximum_generations: u32,
) {
    let project = database
        .register_project(NewProject::new(
            format!("storage/ddl-budget-{maximum_generations}"),
            digest(&maximum_generations.to_be_bytes()),
        ))
        .await
        .unwrap_or_else(|error| panic!("DDL budget project failed: {error}"));
    let first = prepare_empty_generation(database, &project, "ddl-first").await;
    let second = prepare_empty_generation(database, &project, "ddl-second").await;
    let third = create_generation_cascade_fixture(database, pool, schema, &project).await;
    query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."index_generations"
        SET state = 'failed' WHERE project_id = $1::uuid"#
    )))
    .bind(project.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("DDL terminalization failed: {error}"));
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "ddl-budget-retention"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("DDL budget lease failed: {error}"));
    let policy = GenerationRetentionPolicy::new(0, maximum_generations)
        .and_then(|policy| policy.with_work_limits(10_000, 64 * 1024 * 1024, 1))
        .unwrap_or_else(|error| panic!("DDL budget policy failed: {error}"));
    let report = database
        .cleanup_generations(GenerationRetentionRequest::new(
            policy,
            &lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("DDL budget pass failed: {error}"));
    assert_eq!(report.failed_removed, 2);
    assert_eq!(report.search_relations_removed, 1);
    assert_eq!(report.failed_remaining, 1);
    assert_eq!(
        report.deferred_reason,
        Some(if maximum_generations == 2 {
            "work_budget_reached"
        } else {
            "search_relation_ddl_budget"
        })
    );
    for (generation, expected) in [
        (&first, None),
        (&second, Some(cartograph_domain::GenerationState::Failed)),
        (&third, None),
    ] {
        assert_eq!(
            database
                .generation_state(&project, generation)
                .await
                .unwrap_or_else(|error| panic!("DDL budget state failed: {error}")),
            expected
        );
    }
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("DDL budget lease release failed: {error}"));
}
