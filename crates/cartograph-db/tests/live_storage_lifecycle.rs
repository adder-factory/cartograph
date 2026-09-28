//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;
#[path = "live_storage_lifecycle/retention_bounds.rs"]
mod retention_bounds;
#[path = "live_storage_lifecycle/retention_progress.rs"]
mod retention_progress;
#[path = "live_storage_lifecycle/statistics.rs"]
mod statistics;

use std::{env, process, time::Duration};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CartographDatabase, GenerationContents, GenerationFacts, GenerationRetentionPolicy,
    GenerationRetentionRequest, GenerationValidationLimits, HeapCompactionPolicy,
    HeapCompactionPolicyInput, LeaseError, LeaseOwner, LeaseRequest, LeaseTarget,
    NativeParseCacheKey, NativeParseCacheKeyInput, NativeParseCacheRetentionPolicy,
    NativeParseCacheRetentionPolicyInput, NativeParseCacheRetentionRequest, NewGeneration,
    NewProject, PostRetentionMaintenance, StorageCompactionPolicy, StorageCompactionPolicyInput,
    latest_schema_version, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, GenerationId, NormalizedPath, ProjectId, ProjectOperation, SourceLanguage,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const LEASE_DURATION: Duration = Duration::from_mins(1);
const STATEMENT_TIMEOUT: Duration = Duration::from_mins(1);
const CASCADE_CHILD_ROWS: u64 = 1_024;
const CASCADE_FIXTURE_ROWS: u64 = CASCADE_CHILD_ROWS + 6;
const COMPACTION_LOCK_NAMESPACE: &str = "cartograph-v2-online-compaction";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn storage_lifecycle_is_bounded_observable_and_online() {
    let database_url = env::var(TEST_DATABASE_URL_ENV)
        .unwrap_or_else(|_| panic!("{TEST_DATABASE_URL_ENV} must be set"));
    let schema = format!("cg_storage_lifecycle_{}", process::id());
    let _schema_guard = TestSchemaGuard::new(&database_url, schema.clone())
        .unwrap_or_else(|error| panic!("storage schema guard failed: {error}"));
    let settings = DatabaseSettings::parse(&database_url, Some("8"), Some("60000"))
        .and_then(|settings| settings.with_schema(&schema))
        .unwrap_or_else(|error| panic!("storage settings failed: {error}"));
    let pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("storage connection failed: {error}"));
    let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
    let migration = database
        .migrate()
        .await
        .unwrap_or_else(|error| panic!("storage migration failed: {error}"));
    assert_eq!(migration.current_version, latest_schema_version());
    query("CREATE EXTENSION IF NOT EXISTS pgstattuple")
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("pgstattuple fixture failed: {error}"));
    assert_virtual_payload_accounting_and_autovacuum(&pool, &schema).await;

    let project = database
        .register_project(NewProject::new(
            "storage/lifecycle",
            digest(b"storage-project"),
        ))
        .await
        .unwrap_or_else(|error| panic!("storage project registration failed: {error}"));
    let cascade_generation =
        create_generation_cascade_fixture(&database, &pool, &schema, &project).await;
    statistics::assert_statistics_and_inventory(&database, &pool, &schema, &project).await;
    let first_ready = prepare_empty_generation(&database, &project, "storage-ready-one").await;
    let second_ready = prepare_empty_generation(&database, &project, "storage-ready-two").await;
    let migration_lease = assert_generation_retention_fences(
        &database,
        &pool,
        &schema,
        &project,
        &cascade_generation,
    )
    .await;
    assert_parse_cache_and_ready_retention(
        &database,
        &project,
        &migration_lease,
        &first_ready,
        &second_ready,
    )
    .await;
    assert_heap_storage_compaction(&database, &pool, &schema, &project).await;
    assert_online_storage_compaction(&database, &database_url, &pool, &schema).await;

    retention_progress::assert_retention_progress(&database, &pool, &schema).await;
    retention_progress::assert_search_budget_progress(&database, &pool, &schema).await;
    retention_progress::assert_keyset_drain_across_transactions(&database, &pool, &schema).await;
    retention_progress::assert_slow_parent_delete_is_deferred(&database, &pool, &schema).await;
    retention_bounds::assert_pool_deadline(&database, &pool, &schema).await;
    retention_bounds::assert_ddl_budget_progress(&database, &pool, &schema).await;
    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn assert_heap_storage_compaction(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
) {
    let insert = format!(
        r#"INSERT INTO "{schema}"."native_parse_cache" (
                project_id, extractor_contract_digest, path_digest, normalized_path,
                language, content_hash, source_bytes, payload, payload_digest
            )
            SELECT CAST($1 AS uuid),
                   lpad(to_hex(value), 64, '0'),
                   lpad(to_hex(value + 1000), 64, '0'),
                   'heap-fixture/' || value || '.rs',
                   'rust',
                   lpad(to_hex(value + 2000), 64, '0'),
                   octet_length(payload),
                   payload,
                   lpad(to_hex(value + 3000), 64, '0')
            FROM generate_series(1, 24) AS value
            CROSS JOIN LATERAL (
                SELECT decode(string_agg(md5(value::text || ':' || chunk), ''), 'hex') AS payload
                FROM generate_series(1, 8192) AS chunk
            ) AS generated"#
    );
    query(AssertSqlSafe(insert))
        .bind(project.as_str())
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("heap bloat fixture insert failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"DELETE FROM "{schema}"."native_parse_cache"
            WHERE normalized_path LIKE 'heap-fixture/%'"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("heap bloat fixture delete failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"VACUUM "{schema}"."native_parse_cache""#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("heap bloat fixture vacuum failed: {error}"));

    let policy = HeapCompactionPolicy::new(HeapCompactionPolicyInput {
        maximum_relations: 8,
        maximum_candidate_bytes: 64 * 1024 * 1024,
        minimum_reclaimable_bytes: 1,
        statement_timeout: Duration::from_mins(1),
    })
    .unwrap_or_else(|error| panic!("heap compaction policy failed: {error}"));
    let plan = database
        .heap_compaction_plan(policy)
        .await
        .unwrap_or_else(|error| panic!("heap compaction plan failed: {error}"));
    let cache = plan
        .candidates
        .iter()
        .find(|candidate| candidate.table == "native_parse_cache")
        .unwrap_or_else(|| panic!("native parse cache heap candidate was missing"));
    assert!(cache.estimated_reclaimable_bytes > 0);
    assert!(plan.requires_access_exclusive);

    let active_lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "heap-active-operation"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("heap active lease fixture failed: {error}"));
    assert_eq!(
        database.compact_heap_storage(policy, u64::MAX).await,
        Err(cartograph_db::StorageCompactionError::LiveOperations)
    );
    database
        .release_lease(&active_lease)
        .await
        .unwrap_or_else(|error| panic!("heap active lease release failed: {error}"));

    let lease_database = database.clone();
    let lease_project = project.clone();
    let report = database
        .compact_heap_storage_with_observer(policy, u64::MAX, move || async move {
            let attempted = lease_database
                .acquire_lease(LeaseRequest::new(
                    LeaseTarget::new(lease_project, ProjectOperation::Migration, None),
                    LeaseOwner::new(process::id(), "heap-maintenance-gate"),
                    LEASE_DURATION,
                ))
                .await;
            assert!(matches!(attempted, Err(LeaseError::Busy)));
        })
        .await
        .unwrap_or_else(|error| panic!("heap compaction apply failed: {error}"));
    let cache = report
        .compacted
        .iter()
        .find(|result| result.table == "native_parse_cache")
        .unwrap_or_else(|| panic!("native parse cache heap result was missing"));
    assert!(cache.bytes_after <= cache.bytes_before);
    assert!(cache.reclaimed_bytes > 0);
}

async fn assert_generation_retention_fences(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    cascade_generation: &GenerationId,
) -> cartograph_db::ProjectLease {
    let retention_lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "storage-lifecycle-maintenance"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("storage migration lease failed: {error}"));
    let migration_fence = retention_lease.fence();
    assert_cascade_row_limit_is_exact(database, pool, schema, &migration_fence, cascade_generation)
        .await;
    assert_cross_schema_cascade_is_detected_and_ddl_is_fenced(
        database,
        pool,
        schema,
        project,
        &migration_fence,
    )
    .await;
    database
        .release_lease(&retention_lease)
        .await
        .unwrap_or_else(|error| panic!("initial storage migration lease release failed: {error}"));
    let failed_duplicate =
        prepare_empty_generation(database, project, "storage-failed-duplicate").await;
    mark_generation_failed(pool, schema, project, &failed_duplicate).await;
    let migration_lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "storage-lifecycle-maintenance-resumed"),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("resumed storage migration lease failed: {error}"));
    age_ready_generations(pool, schema, project).await;
    migration_lease
}

async fn assert_parse_cache_and_ready_retention(
    database: &CartographDatabase,
    project: &ProjectId,
    migration_lease: &cartograph_db::ProjectLease,
    first_ready: &GenerationId,
    second_ready: &GenerationId,
) {
    let protected_contract = digest(b"current-extractor-contract");
    let prior_contract = digest(b"prior-extractor-contract");
    let obsolete_contract = digest(b"obsolete-extractor-contract");
    for (contract, path, payload) in [
        (
            &obsolete_contract,
            "src/obsolete.rs",
            b"obsolete".as_slice(),
        ),
        (&prior_contract, "src/prior.rs", b"prior".as_slice()),
        (&protected_contract, "src/current.rs", b"current".as_slice()),
    ] {
        database
            .store_native_parse_cache(&cache_key(project, contract, path), payload)
            .await
            .unwrap_or_else(|error| panic!("parse cache fixture failed: {error}"));
    }

    let before = database
        .storage_usage(project, 64, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("storage usage failed: {error}"));
    assert_eq!(before.parse_cache.rows, 3);
    assert_eq!(before.parse_cache.contracts, 3);
    assert_eq!(before.stale_ready_generations, 2);
    assert_eq!(before.deduplication.duplicate_content_groups, 1);
    assert_eq!(before.deduplication.redundant_generations, 1);
    assert!(!before.deduplication.mutation_supported);
    assert!(before.schema_bytes > 0);
    assert!(before.index_bytes > 0);
    let totals = database
        .storage_totals(STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("compact storage totals failed: {error}"));
    // The database-wide allocation includes every schema and can change between
    // these independent read-only transactions as PostgreSQL background work
    // extends another relation. Each snapshot must still contain the exact
    // Cartograph schema allocation compared below.
    assert!(before.database_bytes >= before.schema_bytes);
    assert!(totals.database_bytes >= totals.schema_bytes);
    assert_eq!(totals.schema_bytes, before.schema_bytes);
    assert_eq!(totals.heap_bytes, before.heap_bytes);
    assert_eq!(totals.index_bytes, before.index_bytes);
    assert_eq!(totals.btree_index_bytes, before.btree_index_bytes);
    assert_eq!(totals.toast_bytes, before.toast_bytes);

    let cache_report = database
        .cleanup_native_parse_cache(NativeParseCacheRetentionRequest {
            project_id: project,
            protected_contract_digest: &protected_contract,
            policy: NativeParseCacheRetentionPolicy::new(NativeParseCacheRetentionPolicyInput {
                capacity: cartograph_db::NativeParseCacheRetentionCapacity {
                    contracts: 2,
                    rows: 2,
                    payload_bytes: 1024,
                },
                deletion_batch: 16,
            })
            .unwrap_or_else(|error| panic!("cache policy failed: {error}")),
            fence: &migration_lease.fence(),
            statement_timeout: STATEMENT_TIMEOUT,
        })
        .await
        .unwrap_or_else(|error| panic!("cache cleanup failed: {error}"));
    assert_eq!(cache_report.rows_removed, 1);
    assert_eq!(cache_report.after.rows, 2);
    assert_eq!(cache_report.after.contracts, 2);
    assert!(!cache_report.pressure.over_contract_budget);
    assert!(
        database
            .load_native_parse_cache(&cache_key(project, &protected_contract, "src/current.rs"))
            .await
            .unwrap_or_else(|error| panic!("protected cache lookup failed: {error}"))
            .is_some()
    );

    let retention = database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(0, 8)
                .and_then(|policy| policy.with_stale_ready_age(Duration::from_secs(1)))
                .unwrap_or_else(|error| panic!("retention policy failed: {error}")),
            &migration_lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("ready reconciliation failed: {error}"));
    assert_eq!(retention.ready_removed, 2);
    assert_eq!(retention.ready_remaining, 0);
    assert_eq!(retention.current_preserved, 0);
    assert_eq!(retention.maintenance, PostRetentionMaintenance::NotNeeded);
    database
        .release_lease(migration_lease)
        .await
        .unwrap_or_else(|error| panic!("storage migration lease release failed: {error}"));
    assert_ne!(first_ready, second_ready);
}

async fn assert_online_storage_compaction(
    database: &CartographDatabase,
    database_url: &str,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    create_compaction_fixture(pool, schema).await;
    create_invalid_artifact_fixture(pool, schema).await;
    let compaction_policy = StorageCompactionPolicy::new(StorageCompactionPolicyInput {
        maximum_indexes: 4,
        maximum_candidate_bytes: 128 * 1024 * 1024,
        minimum_index_bytes: 1024 * 1024,
        statement_timeout: STATEMENT_TIMEOUT,
    })
    .unwrap_or_else(|error| panic!("compaction policy failed: {error}"));
    let plan = database
        .storage_compaction_plan(compaction_policy)
        .await
        .unwrap_or_else(|error| panic!("compaction plan failed: {error}"));
    assert!(
        plan.candidates
            .iter()
            .any(|candidate| candidate.index == "storage_compaction_fixture_idx")
    );
    assert!(plan.required_headroom_bytes > plan.candidate_bytes);
    assert_eq!(plan.invalid_artifact_total, 65);
    assert_eq!(plan.invalid_artifacts.len(), 64);
    assert!(plan.invalid_artifacts_truncated);
    let compacted = database
        .compact_storage_online(compaction_policy, u64::MAX)
        .await
        .unwrap_or_else(|error| panic!("online compaction failed: {error}"));
    assert!(
        compacted
            .reindexed
            .iter()
            .any(|candidate| candidate.index == "storage_compaction_fixture_idx")
    );
    assert!(compacted.stop_reason.is_none());
    assert_compaction_cancellation_closes_session(database_url, pool, schema).await;
}

async fn prepare_empty_generation(
    database: &CartographDatabase,
    project: &ProjectId,
    revision: &str,
) -> GenerationId {
    let staged = database
        .begin_generation(NewGeneration::new(project.clone(), revision, 1))
        .await
        .unwrap_or_else(|error| panic!("storage generation start failed: {error}"));
    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(
                project.clone(),
                ProjectOperation::Index,
                Some(staged.generation_id().clone()),
            ),
            LeaseOwner::new(process::id(), revision),
            LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("storage generation lease failed: {error}"));
    let limits = GenerationValidationLimits::new(1024 * 1024, 4 * 1024 * 1024)
        .unwrap_or_else(|error| panic!("storage validation limits failed: {error}"));
    let canonical = validate_generation_facts(GenerationFacts::default(), limits, || false)
        .map_or_else(
            |error| panic!("storage generation facts failed: {error}"),
            |(facts, _)| facts,
        );
    let ready = database
        .prepare_generation(GenerationContents::new(staged, canonical), &lease.fence())
        .await
        .unwrap_or_else(|error| panic!("storage generation prepare failed: {error}"));
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("storage generation lease release failed: {error}"));
    ready.generation_id().clone()
}

async fn create_generation_cascade_fixture(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
) -> GenerationId {
    let staged = database
        .begin_generation(NewGeneration::new(
            project.clone(),
            "storage-cascade-fixture",
            1,
        ))
        .await
        .unwrap_or_else(|error| panic!("cascade generation start failed: {error}"));
    let generation = staged.generation_id().clone();
    let file_id = "11111111-1111-8111-8111-111111111111";
    let symbol_id = "22222222-2222-8222-8222-222222222222";
    insert_cascade_symbol_facts(pool, schema, project, &generation, file_id, symbol_id).await;
    insert_cascade_dependents(pool, schema, project, &generation, symbol_id).await;
    query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."index_generations"
            SET state = 'failed'
            WHERE project_id = $1::uuid AND generation_id = $2::uuid"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade generation terminalization failed: {error}"));
    generation
}

async fn insert_cascade_symbol_facts(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    file_id: &str,
    symbol_id: &str,
) {
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."files" (
                project_id, generation_id, file_id, normalized_path, language,
                content_hash, byte_size, parse_status
            ) VALUES (
                $1::uuid, $2::uuid, $3::uuid, 'src/cascade.rs', 'rust',
                repeat('a', 64), 1, 'parsed'
            )"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(file_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade file fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."symbols" (
                project_id, generation_id, symbol_id, file_id, symbol_kind,
                qualified_name, signature, start_byte, end_byte, start_line,
                end_line, structural_digest
            ) VALUES (
                $1::uuid, $2::uuid, $3::uuid, $4::uuid, 'function',
                'cascade_fixture', '()', 0, 1, 1, 1, repeat('b', 64)
            )"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(symbol_id)
    .bind(file_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade symbol fixture failed: {error}"));
}

async fn insert_cascade_dependents(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    symbol_id: &str,
) {
    let coverage_source = query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."coverage_sources" (
                project_id, label, report_format, report_digest
            ) VALUES ($1::uuid, 'cascade-fixture', 'lcov', repeat('c', 64))
            RETURNING source_id::text"#
    )))
    .bind(project.as_str())
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<String, _>(0))
    .unwrap_or_else(|error| panic!("cascade coverage source failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."symbol_coverage" (
                project_id, generation_id, source_id, symbol_id,
                lines_found, lines_hit
            ) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, 1)"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(coverage_source)
    .bind(symbol_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade coverage fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."symbol_issues" (
                project_id, generation_id, symbol_id, issue_number,
                commit_sha, attribution_kind
            )
            SELECT $1::uuid, $2::uuid, $3::uuid, value,
                   repeat('d', 40), 'modified'
            FROM generate_series(1, $4::bigint) AS value"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(symbol_id)
    .bind(i64::try_from(CASCADE_CHILD_ROWS).unwrap_or(i64::MAX))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade issue fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."summary_priority_queue" (
                project_id, generation_id, symbol_id
            ) VALUES ($1::uuid, $2::uuid, $3::uuid)"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(symbol_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade summary queue fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."issue_history_refreshes" (
                project_id, generation_id, head_commit, commits_scanned,
                tagged_commits, oversized_commits_skipped,
                comparison_failures_skipped, attributions_written, truncated
            ) VALUES (
                $1::uuid, $2::uuid, repeat('e', 40), 0, 0, 0, 0, 0, false
            )"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cascade history refresh fixture failed: {error}"));
}

async fn assert_cascade_row_limit_is_exact(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    fence: &cartograph_db::LeaseFence,
    generation: &GenerationId,
) {
    let below_exact = GenerationRetentionPolicy::new(0, 1)
        .and_then(|policy| policy.with_work_limits(CASCADE_FIXTURE_ROWS - 1, 1024 * 1024 * 1024, 1))
        .unwrap_or_else(|error| panic!("bounded cascade policy failed: {error}"));
    let preserved = database
        .cleanup_generations(GenerationRetentionRequest::new(
            below_exact,
            fence,
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("bounded cascade preflight failed: {error}"));
    assert_eq!(preserved.removed(), 0);
    assert_eq!(preserved.cascade_rows_removed, CASCADE_FIXTURE_ROWS - 1);
    assert_eq!(preserved.retiring_remaining, 1);

    let exact = GenerationRetentionPolicy::new(0, 1)
        .and_then(|policy| policy.with_work_limits(CASCADE_FIXTURE_ROWS, 1024 * 1024 * 1024, 1))
        .unwrap_or_else(|error| panic!("exact cascade policy failed: {error}"));
    let removed = database
        .cleanup_generations(GenerationRetentionRequest::new(
            exact,
            fence,
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("exact cascade cleanup failed: {error}"));
    assert_eq!(removed.failed_removed, 1);
    assert_eq!(removed.cascade_rows_removed, 1);
    assert_eq!(removed.maintenance, PostRetentionMaintenance::NotNeeded);
    assert_eq!(removed.failed_remaining, 0);
    let remaining = query(AssertSqlSafe(format!(
        r#"SELECT count(*)::bigint FROM "{schema}"."index_generations"
            WHERE generation_id = $1::uuid"#
    )))
    .bind(generation.as_str())
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>(0))
    .unwrap_or_else(|error| panic!("cascade cleanup verification failed: {error}"));
    assert_eq!(remaining, 0);

    query(AssertSqlSafe(format!(
        r#"CREATE TABLE "{schema}"."storage_unknown_generation_child" (
                project_id uuid NOT NULL,
                generation_id uuid NOT NULL,
                FOREIGN KEY (project_id, generation_id)
                    REFERENCES "{schema}"."index_generations"(project_id, generation_id)
                    ON DELETE CASCADE
            )"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("unknown cascade fixture failed: {error}"));
    let drift = database
        .cleanup_generations(GenerationRetentionRequest::new(
            exact,
            fence,
            STATEMENT_TIMEOUT,
        ))
        .await;
    assert!(matches!(
        drift,
        Err(cartograph_db::GenerationRetentionError::DatabaseOperation {
            operation: "cascade-catalog-mismatch"
        })
    ));
    query(AssertSqlSafe(format!(
        r#"DROP TABLE "{schema}"."storage_unknown_generation_child""#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("unknown cascade fixture cleanup failed: {error}"));
}

async fn assert_cross_schema_cascade_is_detected_and_ddl_is_fenced(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    fence: &cartograph_db::LeaseFence,
) {
    let (external_schema, policy) =
        assert_cross_schema_ddl_fence(database, pool, schema, fence).await;
    assert_cross_schema_preservation(
        database,
        pool,
        schema,
        project,
        fence,
        &external_schema,
        policy,
    )
    .await;
}

async fn assert_cross_schema_ddl_fence(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    fence: &cartograph_db::LeaseFence,
) -> (String, GenerationRetentionPolicy) {
    let external_schema = format!("{schema}_external");
    query(AssertSqlSafe(format!(
        r#"CREATE SCHEMA "{external_schema}""#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("external cascade schema failed: {error}"));

    let policy = GenerationRetentionPolicy::new(0, 1)
        .and_then(|policy| policy.with_work_limits(10_000, 1024 * 1024 * 1024, 1))
        .unwrap_or_else(|error| panic!("cascade concurrency policy failed: {error}"));
    let (catalog_entered_sender, catalog_entered_receiver) = tokio::sync::oneshot::channel();
    let (catalog_hold_sender, catalog_hold_receiver) = tokio::sync::oneshot::channel::<()>();
    let cleanup_database = database.clone();
    let cleanup_fence = fence.clone();
    let cleanup = tokio::spawn(async move {
        cleanup_database
            .cleanup_generations_with_observer(
                GenerationRetentionRequest::new(policy, &cleanup_fence, STATEMENT_TIMEOUT),
                || async move {
                    let _ = catalog_entered_sender.send(());
                    let _ = catalog_hold_receiver.await;
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), catalog_entered_receiver)
        .await
        .unwrap_or_else(|_| panic!("retention catalog observer was not reached"))
        .unwrap_or_else(|_| panic!("retention catalog observer closed unexpectedly"));

    let mut ddl_connection = pool
        .acquire()
        .await
        .unwrap_or_else(|error| panic!("cascade DDL connection failed: {error}"));
    let ddl_pid = query("SELECT pg_backend_pid()")
        .fetch_one(&mut *ddl_connection)
        .await
        .and_then(|row| row.try_get::<i32, _>(0))
        .unwrap_or_else(|error| panic!("cascade DDL pid failed: {error}"));
    let ddl_external_schema = external_schema.clone();
    let ddl_schema = schema.to_owned();
    let ddl = tokio::spawn(async move {
        query(AssertSqlSafe(format!(
            r#"CREATE TABLE "{ddl_external_schema}"."storage_cross_schema_symbol_child" (
                    project_id uuid NOT NULL,
                    generation_id uuid NOT NULL,
                    symbol_id uuid NOT NULL,
                    FOREIGN KEY (project_id, generation_id, symbol_id)
                        REFERENCES "{ddl_schema}"."symbols"(project_id, generation_id, symbol_id)
                        ON DELETE CASCADE
                )"#
        )))
        .execute(&mut *ddl_connection)
        .await
    });
    wait_for_blocked_fk_lock(pool, schema, "symbols", ddl_pid).await;
    let _ = catalog_hold_sender.send(());
    let cleanup_report = tokio::time::timeout(Duration::from_secs(10), cleanup)
        .await
        .unwrap_or_else(|_| panic!("retention cleanup did not finish after DDL fence release"))
        .unwrap_or_else(|error| panic!("retention cleanup task failed: {error}"))
        .unwrap_or_else(|error| panic!("retention cleanup failed: {error}"));
    assert_eq!(cleanup_report.removed(), 0);
    tokio::time::timeout(Duration::from_secs(10), ddl)
        .await
        .unwrap_or_else(|_| panic!("cross-schema cascade DDL remained blocked"))
        .unwrap_or_else(|error| panic!("cross-schema cascade DDL task failed: {error}"))
        .unwrap_or_else(|error| panic!("cross-schema cascade DDL failed: {error}"));
    (external_schema, policy)
}

async fn assert_cross_schema_preservation(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    fence: &cartograph_db::LeaseFence,
    external_schema: &str,
    policy: GenerationRetentionPolicy,
) {
    let (generation, file_id, symbol_id) =
        create_minimal_failed_symbol_generation(database, pool, schema, project).await;
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{external_schema}"."storage_cross_schema_symbol_child" (
                project_id, generation_id, symbol_id
            ) VALUES ($1::uuid, $2::uuid, $3::uuid)"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(&symbol_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cross-schema cascade row failed: {error}"));

    let rejected = database
        .cleanup_generations(GenerationRetentionRequest::new(
            policy,
            fence,
            STATEMENT_TIMEOUT,
        ))
        .await;
    assert!(matches!(
        rejected,
        Err(cartograph_db::GenerationRetentionError::DatabaseOperation {
            operation: "cascade-catalog-mismatch"
        })
    ));
    let preserved = query(AssertSqlSafe(format!(
        r#"SELECT
                EXISTS (
                    SELECT 1 FROM "{schema}"."index_generations"
                    WHERE project_id = $1::uuid AND generation_id = $2::uuid
                ) AS generation_present,
                EXISTS (
                    SELECT 1 FROM "{schema}"."files"
                    WHERE project_id = $1::uuid AND generation_id = $2::uuid
                      AND file_id = $3::uuid
                ) AS file_present,
                EXISTS (
                    SELECT 1 FROM "{external_schema}"."storage_cross_schema_symbol_child"
                    WHERE project_id = $1::uuid AND generation_id = $2::uuid
                      AND symbol_id = $4::uuid
                ) AS child_present"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(&file_id)
    .bind(&symbol_id)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("cross-schema preservation query failed: {error}"));
    assert!(
        preserved
            .try_get::<bool, _>("generation_present")
            .unwrap_or(false)
    );
    assert!(
        preserved
            .try_get::<bool, _>("file_present")
            .unwrap_or(false)
    );
    assert!(
        preserved
            .try_get::<bool, _>("child_present")
            .unwrap_or(false)
    );

    query(AssertSqlSafe(format!(
        r#"DROP SCHEMA "{external_schema}" CASCADE"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("external cascade schema cleanup failed: {error}"));
}

async fn create_minimal_failed_symbol_generation(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
) -> (GenerationId, String, String) {
    let staged = database
        .begin_generation(NewGeneration::new(
            project.clone(),
            "storage-cross-schema-cascade",
            1,
        ))
        .await
        .unwrap_or_else(|error| panic!("cross-schema generation start failed: {error}"));
    let generation = staged.generation_id().clone();
    let file_id = "33333333-3333-8333-8333-333333333333".to_owned();
    let symbol_id = "44444444-4444-8444-8444-444444444444".to_owned();
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."files" (
                project_id, generation_id, file_id, normalized_path, language,
                content_hash, byte_size, parse_status
            ) VALUES (
                $1::uuid, $2::uuid, $3::uuid, 'src/cross_schema.rs', 'rust',
                repeat('f', 64), 1, 'parsed'
            )"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(&file_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cross-schema file fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."symbols" (
                project_id, generation_id, symbol_id, file_id, symbol_kind,
                qualified_name, signature, start_byte, end_byte, start_line,
                end_line, structural_digest
            ) VALUES (
                $1::uuid, $2::uuid, $3::uuid, $4::uuid, 'function',
                'cross_schema_fixture', '()', 0, 1, 1, 1, repeat('a', 64)
            )"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .bind(&symbol_id)
    .bind(&file_id)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("cross-schema symbol fixture failed: {error}"));
    mark_generation_failed(pool, schema, project, &generation).await;
    (generation, file_id, symbol_id)
}

async fn mark_generation_failed(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
) {
    query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."index_generations"
            SET state = 'failed'
            WHERE project_id = $1::uuid AND generation_id = $2::uuid"#
    )))
    .bind(project.as_str())
    .bind(generation.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("generation failure fixture failed: {error}"));
}

async fn wait_for_blocked_fk_lock(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    relation: &str,
    pid: i32,
) {
    for _ in 0..200 {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1
                    FROM pg_catalog.pg_locks AS locks
                    INNER JOIN pg_catalog.pg_class AS relations
                        ON relations.oid = locks.relation
                    INNER JOIN pg_catalog.pg_namespace AS namespaces
                        ON namespaces.oid = relations.relnamespace
                    WHERE locks.pid = $1
                      AND locks.mode = 'ShareRowExclusiveLock'
                      AND NOT locks.granted
                      AND namespaces.nspname = $2
                      AND relations.relname = $3
                )",
        )
        .bind(pid)
        .bind(schema)
        .bind(relation)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("blocked FK lock probe failed: {error}"));
        if row.try_get::<bool, _>(0).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("foreign-key DDL did not wait on the retention relation lock");
}

async fn age_ready_generations(pool: &sqlx_postgres::PgPool, schema: &str, project: &ProjectId) {
    query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."index_generations"
            SET ready_at = clock_timestamp() - interval '2 days'
            WHERE project_id = $1::uuid AND state = 'ready'"#
    )))
    .bind(project.as_str())
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("ready generation aging failed: {error}"));
}

async fn assert_virtual_payload_accounting_and_autovacuum(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let generated = query(
        r"SELECT attributes.attgenerated::text AS generated
            FROM pg_catalog.pg_attribute AS attributes
            INNER JOIN pg_catalog.pg_class AS classes ON classes.oid = attributes.attrelid
            INNER JOIN pg_catalog.pg_namespace AS namespaces
                ON namespaces.oid = classes.relnamespace
            WHERE namespaces.nspname = $1
              AND classes.relname = 'native_parse_cache'
              AND attributes.attname = 'payload_bytes'",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("generated column lookup failed: {error}"));
    assert_eq!(
        generated.try_get::<String, _>("generated").ok().as_deref(),
        Some("v")
    );
    let options = query(
        r"SELECT reloptions
            FROM pg_catalog.pg_class AS classes
            INNER JOIN pg_catalog.pg_namespace AS namespaces
                ON namespaces.oid = classes.relnamespace
            WHERE namespaces.nspname = $1 AND classes.relname = 'native_parse_cache'",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("autovacuum options lookup failed: {error}"))
    .try_get::<Option<Vec<String>>, _>("reloptions")
    .unwrap_or_else(|error| panic!("autovacuum options decode failed: {error}"))
    .unwrap_or_default();
    assert!(
        options
            .iter()
            .any(|option| option == "autovacuum_vacuum_scale_factor=0.01")
    );
    assert!(
        options
            .iter()
            .any(|option| option == "autovacuum_vacuum_threshold=100")
    );
}

async fn create_compaction_fixture(pool: &sqlx_postgres::PgPool, schema: &str) {
    for statement in [
        format!(
            r#"CREATE TABLE "{schema}"."storage_compaction_fixture" (
                id bigint NOT NULL, payload text NOT NULL
            )"#
        ),
        format!(
            r#"INSERT INTO "{schema}"."storage_compaction_fixture" (id, payload)
                SELECT value, repeat('x', 64) FROM generate_series(1, 200000) AS value"#
        ),
        format!(
            r#"CREATE INDEX storage_compaction_fixture_idx
                ON "{schema}"."storage_compaction_fixture" (id)"#
        ),
    ] {
        query(AssertSqlSafe(statement))
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("compaction fixture failed: {error}"));
    }
}

async fn create_invalid_artifact_fixture(pool: &sqlx_postgres::PgPool, schema: &str) {
    query(AssertSqlSafe(format!(
        r#"CREATE TABLE "{schema}"."storage_invalid_artifact_fixture" (
            id bigint NOT NULL
        )"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("invalid artifact table fixture failed: {error}"));
    for index in 0..65 {
        query(AssertSqlSafe(format!(
            r#"CREATE INDEX storage_artifact_ccnew{index}
                ON "{schema}"."storage_invalid_artifact_fixture" (id)"#
        )))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("invalid artifact index fixture failed: {error}"));
    }
}

async fn assert_compaction_cancellation_closes_session(
    database_url: &str,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let settings = DatabaseSettings::parse(database_url, Some("1"), Some("60000"))
        .and_then(|settings| settings.with_schema(schema))
        .unwrap_or_else(|error| panic!("isolated compaction settings failed: {error}"));
    let isolated_pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("isolated compaction connection failed: {error}"));
    let isolated = CartographDatabase::new(isolated_pool.clone(), settings.schema().clone());
    let policy = StorageCompactionPolicy::new(StorageCompactionPolicyInput {
        maximum_indexes: 1,
        maximum_candidate_bytes: 128 * 1024 * 1024,
        minimum_index_bytes: 1024 * 1024,
        statement_timeout: Duration::from_secs(30),
    })
    .unwrap_or_else(|error| panic!("cancellation compaction policy failed: {error}"));
    let plan = isolated
        .storage_compaction_plan(policy)
        .await
        .unwrap_or_else(|error| panic!("cancellation compaction plan failed: {error}"));
    assert_eq!(
        plan.candidates
            .first()
            .map(|candidate| candidate.index.as_str()),
        Some("storage_compaction_fixture_idx")
    );
    let default_timeout = statement_timeout(pool).await;

    let (session_entered_sender, session_entered_receiver) = tokio::sync::oneshot::channel();
    let (session_hold_sender, session_hold_receiver) = tokio::sync::oneshot::channel::<()>();
    let cancelled_database = isolated.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_database
            .compact_storage_online_with_observer(policy, u64::MAX, || async move {
                let _ = session_entered_sender.send(());
                let _ = session_hold_receiver.await;
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), session_entered_receiver)
        .await
        .unwrap_or_else(|_| panic!("compaction session observer was not reached"))
        .unwrap_or_else(|_| panic!("compaction session observer closed unexpectedly"));
    wait_for_compaction_lock_state(pool, schema, true).await;
    cancelled.abort();
    assert!(
        cancelled.await.is_err_and(|error| error.is_cancelled()),
        "compaction cancellation task was not cancelled"
    );
    drop(session_hold_sender);
    wait_for_compaction_lock_state(pool, schema, false).await;

    let resumed = tokio::time::timeout(
        STATEMENT_TIMEOUT,
        isolated.compact_storage_online(policy, u64::MAX),
    )
    .await
    .unwrap_or_else(|_| panic!("compaction did not resume after cancellation"))
    .unwrap_or_else(|error| panic!("compaction resume failed: {error}"));
    assert!(resumed.stop_reason.is_none());
    assert_eq!(statement_timeout(&isolated_pool).await, default_timeout);
    isolated.close().await;
}

async fn wait_for_compaction_lock_state(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    expected_held: bool,
) {
    let lock_name = format!("{COMPACTION_LOCK_NAMESPACE}:{schema}");
    for _ in 0..200 {
        let mut connection = pool
            .acquire()
            .await
            .unwrap_or_else(|error| panic!("compaction lock probe failed: {error}"));
        let acquired = query("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
            .bind(&lock_name)
            .fetch_one(&mut *connection)
            .await
            .and_then(|row| row.try_get::<bool, _>(0))
            .unwrap_or_else(|error| panic!("compaction lock probe query failed: {error}"));
        if acquired {
            query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
                .bind(&lock_name)
                .execute(&mut *connection)
                .await
                .unwrap_or_else(|error| panic!("compaction lock probe release failed: {error}"));
        }
        if acquired != expected_held {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("compaction advisory lock did not reach expected held={expected_held} state");
}

async fn statement_timeout(pool: &sqlx_postgres::PgPool) -> String {
    query("SELECT current_setting('statement_timeout')")
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>(0))
        .unwrap_or_else(|error| panic!("statement timeout probe failed: {error}"))
}

fn cache_key(project: &ProjectId, contract: &ContentDigest, path: &str) -> NativeParseCacheKey {
    NativeParseCacheKey::new(NativeParseCacheKeyInput {
        project_id: project.clone(),
        extractor_contract_digest: contract.clone(),
        path: NormalizedPath::parse(path)
            .unwrap_or_else(|error| panic!("cache path failed: {error}")),
        language: SourceLanguage::Rust,
        content_hash: digest(path.as_bytes()),
        source_bytes: u64::try_from(path.len()).unwrap_or(u64::MAX),
    })
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::from_bytes(*blake3::hash(bytes).as_bytes())
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    query(AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("storage schema cleanup failed: {error}"));
}
