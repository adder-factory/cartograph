//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;

use std::{
    env, process,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CartographDatabase, GenerationRecoveryRequest, LeaseError, LeaseOwner, LeaseRequest,
    LeaseTarget, MigrationError, NewProject, RecoverableGeneration,
};
use cartograph_domain::{
    ContentDigest, GenerationDigestVersion, GenerationId, ProjectId, ProjectOperation,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const PROJECT_FINGERPRINT: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OWNER_ONE_START: &str = "boot-a:process-start-100";
const OWNER_TWO_START: &str = "boot-a:process-start-200";
const LEASE_DURATION: Duration = Duration::from_secs(30);
const LEASE_DURATION_MILLIS: i64 = 30_000;
const V1_PROJECT_ID: &str = "11111111-1111-4111-8111-111111111111";
const V1_GENERATION_ID: &str = "22222222-2222-4222-8222-222222222222";
const V1_FILE_ID: &str = "33333333-3333-4333-8333-333333333333";
const V1_SYMBOL_ID: &str = "44444444-4444-4444-8444-444444444444";
const V1_HISTORICAL_DIGEST: &str =
    "5555555555555555555555555555555555555555555555555555555555555555";
const INITIAL_MIGRATION_VERSION: i64 = 1;
const OPERATION_LEASES_MIGRATION_VERSION: i64 = 2;
const COMPLETE_EDGE_KINDS_MIGRATION_VERSION: i64 = 3;
const REFERENCE_EVIDENCE_MIGRATION_VERSION: i64 = 4;
const DIGEST_VERSION_MIGRATION_VERSION: i64 = 5;
const BULK_RELATION_VALIDATION_MIGRATION_VERSION: i64 = 6;
const V1_IMPORT_RETENTION_MIGRATION_VERSION: i64 = 7;
const SEMANTIC_STORAGE_MIGRATION_VERSION: i64 = 8;
const REFERENCE_MULTIPLICITY_MIGRATION_VERSION: i64 = 9;
const EXACT_LOOKUP_MIGRATION_VERSION: i64 = 10;
const GENERATION_SEARCH_RELATIONS_MIGRATION_VERSION: i64 = 11;
const TYPED_SYMBOL_SEMANTICS_MIGRATION_VERSION: i64 = 12;
const AGENT_EVIDENCE_MIGRATION_VERSION: i64 = 13;
const AGENT_SESSION_MIGRATION_VERSION: i64 = 14;
const STRUCTURAL_BRIDGE_MIGRATION_VERSION: i64 = 15;
const MATERIALIZED_SIMILARITY_MIGRATION_VERSION: i64 = 16;
const NATIVE_PARSE_CACHE_MIGRATION_VERSION: i64 = 17;
const SYMBOL_ISSUE_HISTORY_MIGRATION_VERSION: i64 = 18;
const SYMBOL_PAGERANK_MIGRATION_VERSION: i64 = 19;
const SUMMARY_PRIORITY_QUEUE_MIGRATION_VERSION: i64 = 20;
const DETERMINISTIC_COCHANGE_ORDER_MIGRATION_VERSION: i64 = 21;
const NATIVE_INDEX_DIGEST_V5_MIGRATION_VERSION: i64 = 22;
const STORAGE_LIFECYCLE_HARDENING_MIGRATION_VERSION: i64 = 23;
const RUST_WORKSPACE_RESOLUTION_DIGEST_V6_MIGRATION_VERSION: i64 = 24;
const DIRECTORY_IMPORT_SIMPLE_NAME_MIGRATION_VERSION: i64 = 25;
const NUMERICAL_EVIDENCE_DIGEST_V7_MIGRATION_VERSION: i64 = 26;
const STRUCTURAL_DIAGNOSTICS_DIGEST_V8_MIGRATION_VERSION: i64 = 27;
const BIOMARKER_PRECISION_DIGEST_V9_MIGRATION_VERSION: i64 = 28;
const DETECTOR_PRECISION_DIGEST_V10_MIGRATION_VERSION: i64 = 29;
const RUST_CLOSURE_CALL_TARGET_DIGEST_V11_MIGRATION_VERSION: i64 = 30;
const GO_CALL_TARGET_DIGEST_V12_MIGRATION_VERSION: i64 = 31;
const NATIVE_GENERATION_SPILL_MIGRATION_VERSION: i64 = 32;
const SPILL_CENTRALITY_LOOKUP_MIGRATION_VERSION: i64 = 33;
const SPILL_PARSE_CACHE_REFERENCE_MIGRATION_VERSION: i64 = 34;
const SEARCH_DOCUMENT_CANONICAL_METADATA_MIGRATION_VERSION: i64 = 35;
const JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_MIGRATION_VERSION: i64 = 36;
const STRUCTURAL_FINDING_CACHE_MIGRATION_VERSION: i64 = 37;
const SHADER_AND_DISPATCH_DIGEST_V14_MIGRATION_VERSION: i64 = 38;
const GENERATION_SOURCE_ADMISSION_MIGRATION_VERSION: i64 = 39;
const ADA_VHDL_NUMERICAL_DIGEST_V15_MIGRATION_VERSION: i64 = 40;
const RUST_SELF_RECEIVER_DIGEST_V16_MIGRATION_VERSION: i64 = 41;
const RESUMABLE_GENERATION_RETENTION_MIGRATION_VERSION: i64 = 42;
const LATEST_MIGRATION_VERSION: i64 = RESUMABLE_GENERATION_RETENTION_MIGRATION_VERSION;
const LATER_MIGRATION_COUNT: u64 = 41;
const EXPECTED_MIGRATIONS: [i64; 42] = [
    INITIAL_MIGRATION_VERSION,
    OPERATION_LEASES_MIGRATION_VERSION,
    COMPLETE_EDGE_KINDS_MIGRATION_VERSION,
    REFERENCE_EVIDENCE_MIGRATION_VERSION,
    DIGEST_VERSION_MIGRATION_VERSION,
    BULK_RELATION_VALIDATION_MIGRATION_VERSION,
    V1_IMPORT_RETENTION_MIGRATION_VERSION,
    SEMANTIC_STORAGE_MIGRATION_VERSION,
    REFERENCE_MULTIPLICITY_MIGRATION_VERSION,
    EXACT_LOOKUP_MIGRATION_VERSION,
    GENERATION_SEARCH_RELATIONS_MIGRATION_VERSION,
    TYPED_SYMBOL_SEMANTICS_MIGRATION_VERSION,
    AGENT_EVIDENCE_MIGRATION_VERSION,
    AGENT_SESSION_MIGRATION_VERSION,
    STRUCTURAL_BRIDGE_MIGRATION_VERSION,
    MATERIALIZED_SIMILARITY_MIGRATION_VERSION,
    NATIVE_PARSE_CACHE_MIGRATION_VERSION,
    SYMBOL_ISSUE_HISTORY_MIGRATION_VERSION,
    SYMBOL_PAGERANK_MIGRATION_VERSION,
    SUMMARY_PRIORITY_QUEUE_MIGRATION_VERSION,
    DETERMINISTIC_COCHANGE_ORDER_MIGRATION_VERSION,
    NATIVE_INDEX_DIGEST_V5_MIGRATION_VERSION,
    STORAGE_LIFECYCLE_HARDENING_MIGRATION_VERSION,
    RUST_WORKSPACE_RESOLUTION_DIGEST_V6_MIGRATION_VERSION,
    DIRECTORY_IMPORT_SIMPLE_NAME_MIGRATION_VERSION,
    NUMERICAL_EVIDENCE_DIGEST_V7_MIGRATION_VERSION,
    STRUCTURAL_DIAGNOSTICS_DIGEST_V8_MIGRATION_VERSION,
    BIOMARKER_PRECISION_DIGEST_V9_MIGRATION_VERSION,
    DETECTOR_PRECISION_DIGEST_V10_MIGRATION_VERSION,
    RUST_CLOSURE_CALL_TARGET_DIGEST_V11_MIGRATION_VERSION,
    GO_CALL_TARGET_DIGEST_V12_MIGRATION_VERSION,
    NATIVE_GENERATION_SPILL_MIGRATION_VERSION,
    SPILL_CENTRALITY_LOOKUP_MIGRATION_VERSION,
    SPILL_PARSE_CACHE_REFERENCE_MIGRATION_VERSION,
    SEARCH_DOCUMENT_CANONICAL_METADATA_MIGRATION_VERSION,
    JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_MIGRATION_VERSION,
    STRUCTURAL_FINDING_CACHE_MIGRATION_VERSION,
    SHADER_AND_DISPATCH_DIGEST_V14_MIGRATION_VERSION,
    GENERATION_SOURCE_ADMISSION_MIGRATION_VERSION,
    ADA_VHDL_NUMERICAL_DIGEST_V15_MIGRATION_VERSION,
    RUST_SELF_RECEIVER_DIGEST_V16_MIGRATION_VERSION,
    RESUMABLE_GENERATION_RETENTION_MIGRATION_VERSION,
];
const EXPECTED_V1_UPGRADE_MIGRATIONS: [i64; 41] = [
    OPERATION_LEASES_MIGRATION_VERSION,
    COMPLETE_EDGE_KINDS_MIGRATION_VERSION,
    REFERENCE_EVIDENCE_MIGRATION_VERSION,
    DIGEST_VERSION_MIGRATION_VERSION,
    BULK_RELATION_VALIDATION_MIGRATION_VERSION,
    V1_IMPORT_RETENTION_MIGRATION_VERSION,
    SEMANTIC_STORAGE_MIGRATION_VERSION,
    REFERENCE_MULTIPLICITY_MIGRATION_VERSION,
    EXACT_LOOKUP_MIGRATION_VERSION,
    GENERATION_SEARCH_RELATIONS_MIGRATION_VERSION,
    TYPED_SYMBOL_SEMANTICS_MIGRATION_VERSION,
    AGENT_EVIDENCE_MIGRATION_VERSION,
    AGENT_SESSION_MIGRATION_VERSION,
    STRUCTURAL_BRIDGE_MIGRATION_VERSION,
    MATERIALIZED_SIMILARITY_MIGRATION_VERSION,
    NATIVE_PARSE_CACHE_MIGRATION_VERSION,
    SYMBOL_ISSUE_HISTORY_MIGRATION_VERSION,
    SYMBOL_PAGERANK_MIGRATION_VERSION,
    SUMMARY_PRIORITY_QUEUE_MIGRATION_VERSION,
    DETERMINISTIC_COCHANGE_ORDER_MIGRATION_VERSION,
    NATIVE_INDEX_DIGEST_V5_MIGRATION_VERSION,
    STORAGE_LIFECYCLE_HARDENING_MIGRATION_VERSION,
    RUST_WORKSPACE_RESOLUTION_DIGEST_V6_MIGRATION_VERSION,
    DIRECTORY_IMPORT_SIMPLE_NAME_MIGRATION_VERSION,
    NUMERICAL_EVIDENCE_DIGEST_V7_MIGRATION_VERSION,
    STRUCTURAL_DIAGNOSTICS_DIGEST_V8_MIGRATION_VERSION,
    BIOMARKER_PRECISION_DIGEST_V9_MIGRATION_VERSION,
    DETECTOR_PRECISION_DIGEST_V10_MIGRATION_VERSION,
    RUST_CLOSURE_CALL_TARGET_DIGEST_V11_MIGRATION_VERSION,
    GO_CALL_TARGET_DIGEST_V12_MIGRATION_VERSION,
    NATIVE_GENERATION_SPILL_MIGRATION_VERSION,
    SPILL_CENTRALITY_LOOKUP_MIGRATION_VERSION,
    SPILL_PARSE_CACHE_REFERENCE_MIGRATION_VERSION,
    SEARCH_DOCUMENT_CANONICAL_METADATA_MIGRATION_VERSION,
    JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_MIGRATION_VERSION,
    STRUCTURAL_FINDING_CACHE_MIGRATION_VERSION,
    SHADER_AND_DISPATCH_DIGEST_V14_MIGRATION_VERSION,
    GENERATION_SOURCE_ADMISSION_MIGRATION_VERSION,
    ADA_VHDL_NUMERICAL_DIGEST_V15_MIGRATION_VERSION,
    RUST_SELF_RECEIVER_DIGEST_V16_MIGRATION_VERSION,
    RESUMABLE_GENERATION_RETENTION_MIGRATION_VERSION,
];

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn leases_are_exclusive_observable_recoverable_and_database_clock_driven() {
    let (database, pool, schema) = open_isolated_database().await;
    let report = match database.migrate().await {
        Ok(report) => report,
        Err(error) => panic!("lease schema migration failed: {error}"),
    };
    assert_eq!(report.applied_versions, EXPECTED_MIGRATIONS);
    assert_v1_to_latest_upgrade(&database, &pool, &schema).await;
    let project = register_project(&database).await;
    let fixture = LeaseFixture {
        database: &database,
        pool: &pool,
        schema: &schema,
        project: &project,
    };

    assert_stale_takeover(&fixture).await;
    assert_concurrent_exclusion(&database, &project).await;
    assert_ledger_gap_is_refused(&database, &pool, &schema).await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

struct LeaseFixture<'a> {
    database: &'a CartographDatabase,
    pool: &'a sqlx_postgres::PgPool,
    schema: &'a str,
    project: &'a ProjectId,
}

async fn assert_stale_takeover(fixture: &LeaseFixture<'_>) {
    let target = LeaseTarget::new(fixture.project.clone(), ProjectOperation::Index, None);
    let mut first = acquire(
        fixture.database,
        target.clone(),
        owner(101, OWNER_ONE_START),
    )
    .await;
    let blocked = fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target.clone(),
            owner(202, OWNER_TWO_START),
            LEASE_DURATION,
        ))
        .await;
    assert!(matches!(blocked, Err(LeaseError::Busy)));
    let cross_operation = fixture
        .database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(fixture.project.clone(), ProjectOperation::Hook, None),
            owner(303, "cross-operation"),
            LEASE_DURATION,
        ))
        .await;
    assert!(matches!(cross_operation, Err(LeaseError::Busy)));
    let status = match fixture.database.lease_status(&target).await {
        Ok(Some(status)) => status,
        Ok(None) => panic!("active lease was not observable"),
        Err(error) => panic!("lease status failed: {error}"),
    };
    assert_eq!(status.owner_pid(), 101);
    assert_eq!(status.owner_process_start(), OWNER_ONE_START);
    if let Err(error) = fixture.database.heartbeat_lease(&mut first).await {
        panic!("active owner could not heartbeat: {error}");
    }
    assert_heartbeat_renewed(fixture, &target, status.expires_at()).await;
    let future_heartbeat = shift_lease_timestamps_ahead(fixture, &target).await;
    if let Err(error) = fixture
        .database
        .heartbeat_lease_bounded(&mut first, Duration::from_secs(5))
        .await
    {
        panic!("backward database clock invalidated an active lease heartbeat: {error}");
    }
    assert_backward_clock_heartbeat_is_monotonic(fixture, &target, &future_heartbeat).await;

    expire_lease(fixture.pool, fixture.schema, &target).await;
    let expired = match fixture.database.lease_status(&target).await {
        Ok(Some(status)) => status,
        Ok(None) => panic!("expired lease disappeared before takeover"),
        Err(error) => panic!("expired lease status failed: {error}"),
    };
    assert!(expired.expired());
    assert!(matches!(
        fixture.database.heartbeat_lease(&mut first).await,
        Err(LeaseError::Lost)
    ));
    let second = acquire(
        fixture.database,
        target.clone(),
        owner(202, OWNER_TWO_START),
    )
    .await;
    assert_ne!(first.lease_id(), second.lease_id());
    assert!(matches!(
        fixture.database.heartbeat_lease(&mut first).await,
        Err(LeaseError::Lost)
    ));
    assert!(matches!(
        fixture.database.release_lease(&first).await,
        Err(LeaseError::Lost)
    ));
    if let Err(error) = fixture.database.release_lease(&second).await {
        panic!("takeover owner could not release: {error}");
    }
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));
}

async fn assert_heartbeat_renewed(
    fixture: &LeaseFixture<'_>,
    target: &LeaseTarget,
    previous_expiry: &str,
) {
    let statement = format!(
        r#"SELECT (
                expires_at > CAST($3 AS timestamptz)
                AND expires_at = heartbeat_at + $4 * interval '1 millisecond'
            ) AS renewed
            FROM "{schema}"."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid) AND operation = $2"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .bind(previous_expiry)
        .bind(LEASE_DURATION_MILLIS)
        .fetch_one(fixture.pool)
        .await;
    let renewed = match row {
        Ok(row) => row.try_get::<bool, _>("renewed"),
        Err(error) => panic!("could not verify database-side lease renewal: {error}"),
    };
    assert!(matches!(renewed, Ok(true)));
}

async fn shift_lease_timestamps_ahead(fixture: &LeaseFixture<'_>, target: &LeaseTarget) -> String {
    let statement = format!(
        r#"UPDATE "{schema}"."project_operation_leases"
            SET acquired_at = expires_at + interval '1 second',
                heartbeat_at = expires_at + interval '1 second',
                expires_at = expires_at + interval '1 second'
                    + $3 * interval '1 millisecond'
            WHERE project_id = CAST($1 AS uuid) AND operation = $2
            RETURNING heartbeat_at::text"#,
        schema = fixture.schema,
    );
    match query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .bind(LEASE_DURATION_MILLIS)
        .fetch_one(fixture.pool)
        .await
    {
        Ok(row) => row
            .try_get::<String, _>("heartbeat_at")
            .unwrap_or_else(|error| panic!("could not decode shifted heartbeat: {error}")),
        Err(error) => panic!("could not create backward-clock heartbeat fixture: {error}"),
    }
}

async fn assert_backward_clock_heartbeat_is_monotonic(
    fixture: &LeaseFixture<'_>,
    target: &LeaseTarget,
    previous_heartbeat: &str,
) {
    let statement = format!(
        r#"SELECT (
                acquired_at <= heartbeat_at
                AND heartbeat_at >= CAST($3 AS timestamptz)
                AND expires_at = heartbeat_at + $4 * interval '1 millisecond'
            ) AS monotonic
            FROM "{schema}"."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid) AND operation = $2"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .bind(previous_heartbeat)
        .bind(LEASE_DURATION_MILLIS)
        .fetch_one(fixture.pool)
        .await;
    let monotonic = match row {
        Ok(row) => row.try_get::<bool, _>("monotonic"),
        Err(error) => panic!("could not verify backward-clock heartbeat clamp: {error}"),
    };
    assert!(matches!(monotonic, Ok(true)));
}

async fn assert_v1_to_latest_upgrade(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    remove_post_v12_schema(pool, schema).await;
    remove_structural_finding_cache_schema(pool, schema).await;
    remove_typed_symbol_semantics_schema(pool, schema).await;
    remove_generation_search_relations_schema(pool, schema).await;
    remove_exact_lookup_schema(pool, schema).await;
    remove_reference_multiplicity_schema(pool, schema).await;
    remove_semantic_storage_schema(pool, schema).await;
    remove_v1_import_retention_schema(pool, schema).await;
    restore_pre_v6_relation_constraints(pool, schema).await;
    let drop_leases = format!(r#"DROP TABLE "{schema}"."project_operation_leases""#);
    if let Err(error) = query(AssertSqlSafe(drop_leases)).execute(pool).await {
        panic!("could not create the v1 upgrade fixture: {error}");
    }
    let drop_digest_version = format!(
        r#"ALTER TABLE "{schema}"."index_generations"
            DROP COLUMN content_digest_version"#
    );
    if let Err(error) = query(AssertSqlSafe(drop_digest_version))
        .execute(pool)
        .await
    {
        panic!("could not remove the digest-version column: {error}");
    }
    let drop_reference_evidence = format!(
        r#"ALTER TABLE "{schema}"."references"
            DROP COLUMN owner_symbol_id,
            DROP COLUMN reference_name,
            DROP COLUMN resolution_provenance"#
    );
    if let Err(error) = query(AssertSqlSafe(drop_reference_evidence))
        .execute(pool)
        .await
    {
        panic!("could not remove later reference evidence columns: {error}");
    }
    let delete_later = format!(r#"DELETE FROM "{schema}"."schema_migrations" WHERE version >= $1"#);
    let deleted = query(AssertSqlSafe(delete_later))
        .bind(OPERATION_LEASES_MIGRATION_VERSION)
        .execute(pool)
        .await;
    assert!(matches!(deleted, Ok(result) if result.rows_affected() == LATER_MIGRATION_COUNT));
    insert_v1_reference_fixture(pool, schema).await;

    assert!(matches!(
        database.migrate().await,
        Ok(report)
            if report.applied_versions == EXPECTED_V1_UPGRADE_MIGRATIONS
                && report.current_version == LATEST_MIGRATION_VERSION
    ));
    assert_upgraded_v1_reference_fixture(database, pool, schema).await;
    assert!(matches!(
        database.migrate().await,
        Ok(report)
            if report.applied_versions.is_empty()
                && report.current_version == LATEST_MIGRATION_VERSION
    ));
}

async fn remove_post_v12_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(
            r#"ALTER TABLE "{schema}"."index_generations"
            DROP CONSTRAINT index_generations_retention_state_check,
            DROP COLUMN retention_original_state, DROP COLUMN retention_started_at,
            DROP CONSTRAINT index_generations_state_check,
            ADD CONSTRAINT index_generations_state_check
                CHECK (state IN ('staging', 'ready', 'current', 'superseded', 'failed'))"#
        ),
        format!(
            r#"ALTER TABLE "{schema}"."projects"
            DROP COLUMN retention_last_attempt_at, DROP COLUMN retention_last_outcome,
            DROP COLUMN retention_consecutive_failures"#
        ),
        format!(
            r#"ALTER TABLE "{schema}"."index_generations"
                DROP CONSTRAINT index_generations_run_excludes_check,
                DROP COLUMN run_excludes"#,
        ),
        format!(r#"ALTER TABLE "{schema}"."search_documents" DROP COLUMN metadata_json"#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_documents""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_numerical_sites""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_references""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_edges""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_symbols""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_files""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_rows""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spill_batches""#),
        format!(r#"DROP TABLE "{schema}"."native_generation_spills""#),
        format!(r#"DROP TABLE "{schema}"."numerical_sites""#),
        format!(r#"DROP TABLE "{schema}"."summary_priority_queue""#),
        format!(r#"DROP INDEX "{schema}"."symbols_pagerank_idx""#),
        format!(r#"ALTER TABLE "{schema}"."symbols" DROP COLUMN pagerank"#),
        format!(r#"DROP TABLE "{schema}"."issue_history_refreshes""#),
        format!(r#"DROP TABLE "{schema}"."symbol_issues""#),
        format!(r#"DROP TABLE "{schema}"."native_parse_cache""#),
        format!(r#"DROP TABLE "{schema}"."symbol_similarity_edges""#),
        format!(r#"DROP TABLE "{schema}"."symbol_similarity_builds""#),
        format!(r#"DROP INDEX "{schema}"."search_documents_one_symbol_document_idx""#),
        format!(r#"DROP INDEX "{schema}"."symbols_betweenness_idx""#),
        format!(r#"ALTER TABLE "{schema}"."symbols" DROP COLUMN betweenness"#),
        format!(r#"DROP TABLE "{schema}"."mcp_tool_calls""#),
        format!(r#"DROP TABLE "{schema}"."mcp_sessions""#),
        format!(r#"DROP TABLE "{schema}"."mcp_macros""#),
        format!(r#"DROP TABLE "{schema}"."agent_artifacts""#),
        format!(r#"DROP TABLE "{schema}"."file_cochanges""#),
        format!(r#"DROP TABLE "{schema}"."file_history""#),
        format!(r#"DROP TABLE "{schema}"."symbol_coverage""#),
        format!(r#"DROP TABLE "{schema}"."coverage_sources""#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove post-v12 migration fixture: {error}");
        }
    }
}

async fn remove_typed_symbol_semantics_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP INDEX "{schema}"."symbols_exported_idx""#),
        format!(
            r#"ALTER TABLE "{schema}"."symbols"
                DROP CONSTRAINT symbols_visibility_check,
                DROP COLUMN visibility,
                DROP COLUMN exported,
                DROP COLUMN default_export,
                DROP COLUMN async_symbol,
                DROP COLUMN static_member,
                DROP COLUMN declaration_only"#,
        ),
        format!(
            r#"ALTER TABLE "{schema}"."index_generations"
                DROP CONSTRAINT index_generations_digest_version_check,
                ADD CONSTRAINT index_generations_digest_version_check
                    CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3))"#,
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove typed-symbol migration fixture: {error}");
        }
    }
}

async fn remove_generation_search_relations_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP TABLE "{schema}"."generation_search_relations""#),
        format!(r#"DROP INDEX "{schema}"."index_generations_global_identity_idx""#),
        format!(
            r#"CREATE INDEX search_documents_bm25_idx
                ON "{schema}"."search_documents"
                USING bm25 (
                    id,
                    project_id,
                    generation_id,
                    document_id,
                    file_id,
                    symbol_id,
                    path,
                    language,
                    document_kind,
                    (qualified_name::pdb.source_code),
                    (code::pdb.source_code),
                    natural_text,
                    metadata
                )
                WITH (key_field = 'id')"#,
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove generation-search migration fixture: {error}");
        }
    }
}

async fn remove_exact_lookup_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP INDEX "{schema}"."symbols_simple_name_idx""#),
        format!(r#"DROP INDEX "{schema}"."references_exact_name_site_idx""#),
        format!(r#"ALTER TABLE "{schema}"."symbols" DROP COLUMN simple_name"#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove exact-lookup migration fixture: {error}");
        }
    }
}

async fn remove_reference_multiplicity_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(
            r#"ALTER TABLE "{schema}"."references"
                DROP COLUMN site_count,
                DROP COLUMN span_precision"#,
        ),
        format!(r#"ALTER TABLE "{schema}"."edges" DROP COLUMN site_count"#),
        format!(
            r#"ALTER TABLE "{schema}"."v1_import_runs"
                DROP COLUMN source_edge_sites,
                DROP COLUMN source_reference_sites"#,
        ),
        format!(
            r#"ALTER TABLE "{schema}"."index_generations"
                DROP CONSTRAINT index_generations_digest_version_check,
                ADD CONSTRAINT index_generations_digest_version_check
                    CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2))"#,
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove reference-multiplicity migration fixture: {error}");
        }
    }
}

async fn remove_semantic_storage_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP TABLE "{schema}"."document_embeddings""#),
        format!(r#"DROP FUNCTION "{schema}"."validate_document_embedding"()"#),
        format!(r#"DROP TABLE "{schema}"."embedding_models""#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove semantic-storage migration fixture: {error}");
        }
    }
}

async fn remove_structural_finding_cache_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP TABLE "{schema}"."structural_findings""#),
        format!(r#"DROP TABLE "{schema}"."structural_finding_runs""#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove structural finding cache migration fixture: {error}");
        }
    }
}

async fn remove_v1_import_retention_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(r#"DROP TABLE "{schema}"."v1_import_checkpoints""#),
        format!(r#"DROP TABLE "{schema}"."v1_import_runs""#),
        format!(r#"DROP INDEX "{schema}"."index_generations_retention_idx""#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not remove v1 import/retention migration fixture: {error}");
        }
    }
}

async fn restore_pre_v6_relation_constraints(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(
            r#"ALTER TABLE "{schema}"."edges"
                DROP CONSTRAINT edges_generation_fk,
                ADD CONSTRAINT edges_project_id_generation_id_source_symbol_id_fkey
                    FOREIGN KEY (project_id, generation_id, source_symbol_id)
                    REFERENCES "{schema}"."symbols"(project_id, generation_id, symbol_id)
                    ON DELETE CASCADE,
                ADD CONSTRAINT edges_project_id_generation_id_target_symbol_id_fkey
                    FOREIGN KEY (project_id, generation_id, target_symbol_id)
                    REFERENCES "{schema}"."symbols"(project_id, generation_id, symbol_id)
                    ON DELETE CASCADE"#,
        ),
        format!(
            r#"ALTER TABLE "{schema}"."references"
                DROP CONSTRAINT references_generation_fk,
                ADD CONSTRAINT references_project_id_generation_id_file_id_fkey
                    FOREIGN KEY (project_id, generation_id, file_id)
                    REFERENCES "{schema}"."files"(project_id, generation_id, file_id)
                    ON DELETE CASCADE,
                ADD CONSTRAINT references_project_id_generation_id_target_symbol_id_fkey
                    FOREIGN KEY (project_id, generation_id, target_symbol_id)
                    REFERENCES "{schema}"."symbols"(project_id, generation_id, symbol_id)
                    ON DELETE CASCADE,
                ADD CONSTRAINT references_owner_symbol_fk
                    FOREIGN KEY (project_id, generation_id, owner_symbol_id)
                    REFERENCES "{schema}"."symbols"(project_id, generation_id, symbol_id)
                    ON DELETE CASCADE"#,
        ),
        format!(
            r#"ALTER TABLE "{schema}"."search_documents"
                ADD CONSTRAINT search_documents_project_id_generation_id_file_id_fkey
                    FOREIGN KEY (project_id, generation_id, file_id)
                    REFERENCES "{schema}"."files"(project_id, generation_id, file_id)
                    ON DELETE CASCADE,
                ADD CONSTRAINT search_documents_project_id_generation_id_symbol_id_fkey
                    FOREIGN KEY (project_id, generation_id, symbol_id)
                    REFERENCES "{schema}"."symbols"(project_id, generation_id, symbol_id)
                    ON DELETE CASCADE"#,
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not restore the pre-v6 relation fixture: {error}");
        }
    }
}

async fn insert_v1_reference_fixture(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statements = [
        format!(
            r#"INSERT INTO "{schema}"."projects"
                (project_id, root_identity, repository_fingerprint, next_generation_sequence)
                VALUES (CAST('{V1_PROJECT_ID}' AS uuid), 'v1-reference-fixture',
                    '{PROJECT_FINGERPRINT}', 2)"#
        ),
        format!(
            r#"INSERT INTO "{schema}"."index_generations"
                (project_id, generation_id, generation_sequence, source_revision, state,
                 worker_count, content_digest, ready_at)
                VALUES (CAST('{V1_PROJECT_ID}' AS uuid), CAST('{V1_GENERATION_ID}' AS uuid), 1,
                    'v1-reference-revision', 'ready', 1, '{V1_HISTORICAL_DIGEST}',
                    clock_timestamp())"#
        ),
        format!(
            r#"INSERT INTO "{schema}"."files"
                (project_id, generation_id, file_id, normalized_path, language, content_hash,
                 byte_size, parse_status)
                VALUES (CAST('{V1_PROJECT_ID}' AS uuid), CAST('{V1_GENERATION_ID}' AS uuid),
                    CAST('{V1_FILE_ID}' AS uuid), 'src/v1.ts', 'typescript',
                    '{PROJECT_FINGERPRINT}', 10, 'parsed')"#
        ),
        format!(
            r#"INSERT INTO "{schema}"."symbols"
                (project_id, generation_id, symbol_id, file_id, symbol_kind, qualified_name,
                 signature, start_byte, end_byte, start_line, end_line, structural_digest)
                VALUES (CAST('{V1_PROJECT_ID}' AS uuid), CAST('{V1_GENERATION_ID}' AS uuid),
                    CAST('{V1_SYMBOL_ID}' AS uuid), CAST('{V1_FILE_ID}' AS uuid), 'function',
                    'legacy', 'function legacy()', 0, 10, 1, 1, '{PROJECT_FINGERPRINT}')"#
        ),
        format!(
            r#"INSERT INTO "{schema}"."references"
                (project_id, generation_id, file_id, target_symbol_id, reference_kind,
                 start_byte, end_byte, confidence)
                VALUES (CAST('{V1_PROJECT_ID}' AS uuid), CAST('{V1_GENERATION_ID}' AS uuid),
                    CAST('{V1_FILE_ID}' AS uuid), CAST('{V1_SYMBOL_ID}' AS uuid), 'calls',
                    0, 6, 1.0)"#
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not populate the v1 reference fixture: {error}");
        }
    }
}

async fn assert_upgraded_v1_reference_fixture(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let statement = format!(
        r#"SELECT generations.content_digest, generations.content_digest_version,
                   reference_rows.reference_name, reference_rows.resolution_provenance
            FROM "{schema}"."index_generations" AS generations
            JOIN "{schema}"."references" AS reference_rows
              ON reference_rows.project_id = generations.project_id
             AND reference_rows.generation_id = generations.generation_id
            WHERE generations.generation_id = CAST($1 AS uuid)"#
    );
    let row = query(AssertSqlSafe(statement))
        .bind(V1_GENERATION_ID)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect the upgraded v1 fixture: {error}"));
    assert_eq!(
        row.try_get::<String, _>("content_digest").ok().as_deref(),
        Some(V1_HISTORICAL_DIGEST)
    );
    assert!(matches!(
        row.try_get::<i16, _>("content_digest_version"),
        Ok(1)
    ));
    assert_eq!(
        row.try_get::<String, _>("reference_name").ok().as_deref(),
        Some("<legacy-unavailable>")
    );
    assert_eq!(
        row.try_get::<String, _>("resolution_provenance")
            .ok()
            .as_deref(),
        Some("legacy-unavailable")
    );

    let project = ProjectId::parse(V1_PROJECT_ID)
        .unwrap_or_else(|error| panic!("v1 project ID was invalid: {error}"));
    let generation = GenerationId::parse(V1_GENERATION_ID)
        .unwrap_or_else(|error| panic!("v1 generation ID was invalid: {error}"));
    let recovered = database
        .recover_generation(GenerationRecoveryRequest::new(&project, &generation))
        .await
        .unwrap_or_else(|error| panic!("v1 ready generation recovery failed: {error}"));
    assert!(matches!(
        recovered,
        Some(RecoverableGeneration::Ready(ready))
            if ready.content_digest().as_str() == V1_HISTORICAL_DIGEST
                && ready.digest_version() == GenerationDigestVersion::V1
    ));
}

async fn assert_ledger_gap_is_refused(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    let delete_v1 = format!(r#"DELETE FROM "{schema}"."schema_migrations" WHERE version = $1"#);
    let deleted = query(AssertSqlSafe(delete_v1))
        .bind(INITIAL_MIGRATION_VERSION)
        .execute(pool)
        .await;
    assert!(matches!(deleted, Ok(result) if result.rows_affected() == 1));
    assert!(matches!(
        database.migrate().await,
        Err(MigrationError::LedgerGap {
            missing_version: INITIAL_MIGRATION_VERSION,
            recorded_version: LATEST_MIGRATION_VERSION,
        })
    ));
}

async fn assert_concurrent_exclusion(database: &CartographDatabase, project: &ProjectId) {
    let target = LeaseTarget::new(project.clone(), ProjectOperation::Sync, None);
    let first = LeaseRequest::new(target.clone(), owner(303, OWNER_ONE_START), LEASE_DURATION);
    let second = LeaseRequest::new(target.clone(), owner(404, OWNER_TWO_START), LEASE_DURATION);
    let (first, second) = tokio::join!(
        database.acquire_lease(first),
        database.acquire_lease(second)
    );
    let winner = match (first, second) {
        (Ok(lease), Err(LeaseError::Busy)) | (Err(LeaseError::Busy), Ok(lease)) => lease,
        results => panic!("concurrent lease result was not exactly-one winner: {results:?}"),
    };
    if let Err(error) = database.release_lease(&winner).await {
        panic!("concurrent winner could not release: {error}");
    }
}

struct GuardedSchema {
    name: String,
    _cleanup: TestSchemaGuard,
}

impl std::ops::Deref for GuardedSchema {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.name
    }
}

impl std::fmt::Display for GuardedSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.name)
    }
}

async fn open_isolated_database() -> (CartographDatabase, sqlx_postgres::PgPool, GuardedSchema) {
    let database_url = env::var(TEST_DATABASE_URL_ENV).unwrap_or_else(|error| {
        panic!("{TEST_DATABASE_URL_ENV} must be set for the ignored integration test: {error}")
    });
    let schema = format!(
        "cartograph_lease_it_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let settings = DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
        .and_then(|settings| settings.with_schema(&schema));
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => panic!("lease test database settings failed validation: {error}"),
    };
    let pool = match cartograph_db::connect(&settings).await {
        Ok(pool) => pool,
        Err(error) => panic!("lease test database connection failed: {error}"),
    };
    let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
    let cleanup = TestSchemaGuard::new(database_url, schema.clone())
        .unwrap_or_else(|error| panic!("lease schema guard failed: {error}"));
    (
        database,
        pool,
        GuardedSchema {
            name: schema,
            _cleanup: cleanup,
        },
    )
}

async fn register_project(database: &CartographDatabase) -> ProjectId {
    let fingerprint = match ContentDigest::parse(PROJECT_FINGERPRINT) {
        Ok(fingerprint) => fingerprint,
        Err(error) => panic!("lease test fingerprint is invalid: {error}"),
    };
    match database
        .register_project(NewProject::new("workspace/leases", fingerprint))
        .await
    {
        Ok(project) => project,
        Err(error) => panic!("lease test project registration failed: {error}"),
    }
}

async fn acquire(
    database: &CartographDatabase,
    target: LeaseTarget,
    owner: LeaseOwner,
) -> cartograph_db::ProjectLease {
    match database
        .acquire_lease(LeaseRequest::new(target, owner, LEASE_DURATION))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("lease acquisition failed: {error}"),
    }
}

fn owner(pid: u32, process_start: &str) -> LeaseOwner {
    LeaseOwner::new(pid, process_start)
}

async fn expire_lease(pool: &sqlx_postgres::PgPool, schema: &str, target: &LeaseTarget) {
    let statement = format!(
        r#"UPDATE "{schema}"."project_operation_leases"
            SET acquired_at = clock_timestamp() - interval '3 seconds',
                heartbeat_at = clock_timestamp() - interval '2 seconds',
                expires_at = clock_timestamp() - interval '1 second'
            WHERE project_id = CAST($1 AS uuid) AND operation = $2"#
    );
    let result = query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .execute(pool)
        .await;
    assert!(matches!(result, Ok(result) if result.rows_affected() == 1));
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
        panic!("failed to drop isolated lease-test schema: {error}");
    }
}
