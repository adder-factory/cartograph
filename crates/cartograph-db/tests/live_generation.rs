//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;

use std::assert_matches;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, process,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    AgentArtifactContent, AgentArtifactKind, AgentArtifactQuery, AgentArtifactScope,
    AgentArtifactState, CanonicalGenerationFacts, CartographDatabase, CurrentFileLookup,
    CurrentFilesLookup, CurrentGeneration, CurrentGenerationLookup, CurrentGraphLookup,
    CurrentSourceRangeLookup, CurrentSymbolSetLookup, DerivedStorePrunePolicy,
    DerivedStorePruneRequest, EdgeInput, ExactTextLookup, FailGenerationError, FailedGeneration,
    FileInput, GenerationContents, GenerationFacts, GenerationRecoveryRequest,
    GenerationRetentionPolicy, GenerationRetentionRequest, GenerationValidationError,
    GenerationValidationLimits, GraphDirection, LeaseOwner, LeaseRequest, LeaseTarget,
    McpMacroStep, McpSessionCallsQuery, McpSessionLookup, McpToolCallData, McpToolCallInput,
    McpToolCallWrite, MigrationError, NewAgentArtifact, NewGeneration, NewMcpMacro, NewMcpSession,
    NewProject, NumericalSiteInput, NumericalSiteQuery, PrepareGenerationError, ProjectLease,
    PublishGenerationError, ReadOnlySqlRequest, ReadyGeneration, RecoverableGeneration,
    ReferenceInput, RoleSweepModel, SearchDocumentInput, SearchQuery, SourceLineRange,
    StorageError, SummaryCandidatePolicy, SymbolInput, SymbolRoleSaveInput, SymbolSummarySaveInput,
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus, GenerationId,
    GenerationState, NormalizedPath, NumericalSiteId, ProjectId, ProjectOperation, SymbolId,
    Visibility,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const REVISION_ONE: &str = "1111111111111111111111111111111111111111";
const REVISION_TWO: &str = "2222222222222222222222222222222222222222";
const REVISION_THREE: &str = "3333333333333333333333333333333333333333";
const REVISION_FOUR: &str = "4444444444444444444444444444444444444444";
const REVISION_FIVE: &str = "5555555555555555555555555555555555555555";
const DIGEST_ONE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DOCUMENT_ONE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const DOCUMENT_TWO: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const DOCUMENT_THREE: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const DOCUMENT_FOUR: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const EVIDENCE_MODEL: &str = "99999999-9999-4999-8999-999999999999";
const RETRIEVAL_FILE: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
const RETRIEVAL_TARGET: &str = "11111111-1111-4111-8111-111111111111";
const RETRIEVAL_CALLER: &str = "22222222-2222-4222-8222-222222222222";
const RETRIEVAL_NUMERICAL_SITE: &str = "33333333-3333-4333-8333-333333333333";
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
const GRAMMAR_REFRESH_DIGEST_V17_MIGRATION_VERSION: i64 = 43;
const CUDA_UNICODE_DIGEST_V18_MIGRATION_VERSION: i64 = 44;
const GENERATION_FACT_COUNTS_MIGRATION_VERSION: i64 = 45;
const RUST_MACRO_REFERENCES_DIGEST_V19_MIGRATION_VERSION: i64 = 46;
const LATEST_MIGRATION_VERSION: i64 = RUST_MACRO_REFERENCES_DIGEST_V19_MIGRATION_VERSION;
const EXPECTED_MIGRATIONS: [i64; 46] = [
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
    GRAMMAR_REFRESH_DIGEST_V17_MIGRATION_VERSION,
    CUDA_UNICODE_DIGEST_V18_MIGRATION_VERSION,
    GENERATION_FACT_COUNTS_MIGRATION_VERSION,
    RUST_MACRO_REFERENCES_DIGEST_V19_MIGRATION_VERSION,
];
const INITIAL_WORKERS: u16 = 4;
const REPLACEMENT_WORKERS: u16 = 8;
const RECOVERY_WORKERS: u16 = 2;
const SEARCH_LIMIT: u16 = 10;
const TEST_LEASE_DURATION: Duration = Duration::from_secs(30);
const LOCK_ORDER_TIMEOUT: Duration = Duration::from_secs(5);
/// Short enough to keep the held-lock probes fast, long enough to be a real wait.
const CONTENDED_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const PROJECT_ROOT: &str = "workspace/cartograph";
const FAILED_SEARCH_BUILD_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_OBSERVATION_ATTEMPTS: usize = 100;
const TEST_VALIDATION_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const TEST_VALIDATION_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const LOCK_OBSERVATION_INTERVAL: Duration = Duration::from_millis(20);
const INTERACTIVE_STALL_TIMEOUT: Duration = Duration::from_millis(75);
const HEARTBEAT_LOCK_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const RELEASE_LOCK_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const READY_TRANSITION_DELAY_SECONDS: &str = "3.0";
const EXACT_LOOKUP_SCALE_ROWS: i32 = 20_000;
const EXACT_LOOKUP_TARGET_NAME: &str = "tagscanary";
const EXACT_LOOKUP_MIGRATION_CHECKSUM: &str =
    "e9ba5a57487dd2f8d9c8e903147b2e083d19094d8c4ec289ac459b84e8b7b86a";
const GENERATION_SEARCH_RELATIONS_MIGRATION_CHECKSUM: &str =
    "a899f5923b2659a8b2be52da09b97784a954de8abd72d03a833005f5276d60ef";
const TYPED_SYMBOL_SEMANTICS_MIGRATION_CHECKSUM: &str =
    "b4539a68d90dac58c142fd0c6573965a93f70400ac94fa924c872c848db2a50c";
const AGENT_EVIDENCE_MIGRATION_CHECKSUM: &str =
    "6ddfc988c68cf7e88dc70c3291379e3afeceab667916fbbe597ef0fdd988eff8";
const AGENT_SESSION_MIGRATION_CHECKSUM: &str =
    "687d57c314fb32a6a16b6c892b6b04494384e651a575c9342ce0f0d22974168d";
const DETERMINISTIC_COCHANGE_ORDER_MIGRATION_CHECKSUM: &str =
    "5cbc965cc09530332f8c320c70aac3b083324a21f78fe6ed8edb23057d6af518";
const NATIVE_INDEX_DIGEST_V5_MIGRATION_CHECKSUM: &str =
    "ac9255910ba9dcd7babba294440758ee3bdee9ed3f142b9cd8291cc3e1128edb";
const RUST_WORKSPACE_RESOLUTION_DIGEST_V6_MIGRATION_CHECKSUM: &str =
    "aa6f62e612975ad71d5f3d44d7636f958f6b13c64bb8f0a795e150ed2105f9cd";
const DIRECTORY_IMPORT_SIMPLE_NAME_MIGRATION_CHECKSUM: &str =
    "6e3150fef9c6e7adba0f17f66864a1b217104b813725a0e99feedb07f2d88331";
const NUMERICAL_EVIDENCE_DIGEST_V7_MIGRATION_CHECKSUM: &str =
    "821c3fa10f3c60766d0a38c6dd0c747fdc273f2d10089fd6f715b347b9d47441";
const STRUCTURAL_DIAGNOSTICS_DIGEST_V8_MIGRATION_CHECKSUM: &str =
    "b0e245329c8698665484bbfcdba2fdb8dee225b56162bf9591057ba7e7af05f4";
const BIOMARKER_PRECISION_DIGEST_V9_MIGRATION_CHECKSUM: &str =
    "702c0fdafe0c2aa6bac5b967f373a310a4ad4ef4ecc0ff710dd0fb2e4c86a8b8";
const DETECTOR_PRECISION_DIGEST_V10_MIGRATION_CHECKSUM: &str =
    "84f354b54354963e1a0733e8415d87b4dec7475bdf32d6e6365e0e390eb19b22";
const RUST_CLOSURE_CALL_TARGET_DIGEST_V11_MIGRATION_CHECKSUM: &str =
    "263ca9fb0ce149525b45c77ab037dd363e4c294d880c06a584ec93904efc30ef";
const GO_CALL_TARGET_DIGEST_V12_MIGRATION_CHECKSUM: &str =
    "a078b0076c3448fe6fee0fb99d0ddf0c2073dfc64be1ca860dea74b222714b46";
const JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_MIGRATION_CHECKSUM: &str =
    "d6cd2cbb7c422e5738b5e32acea7eb200ccd90ab79a07c363f53f94e8ec9815e";
const SHADER_AND_DISPATCH_DIGEST_V14_MIGRATION_CHECKSUM: &str =
    "3cbb510a7453da463f127d7c92f4c216d5fb65748154e19ff035711b1f3a0d46";
const GENERATION_SOURCE_ADMISSION_MIGRATION_CHECKSUM: &str =
    "f6a2d4ac41b97e8d47a58eb95c2d8b739b14f6c2318c4ad89af5a43bb039141b";
const ADA_VHDL_NUMERICAL_DIGEST_V15_MIGRATION_CHECKSUM: &str =
    "a5fc95d44ff7b12038baa012552316c3ac5ba7e7a214c4aa2bcc47d7c04e14c5";

const RUST_SELF_RECEIVER_DIGEST_V16_MIGRATION_CHECKSUM: &str =
    "1bf568eee031e851410c76a5bea4a847e61060560679b7576fc9d3bf238de583";

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn unleased_staging_cleanup_is_exact_lease_safe_and_retention_collects_stale_rows() {
    let (database, pool, schema) = open_isolated_database().await;
    database
        .migrate()
        .await
        .unwrap_or_else(|error| panic!("staging cleanup fixture migration failed: {error}"));
    let project = register_project(&database).await;

    let leased_id = assert_unleased_staging_cleanup(&database, &project).await;
    assert_stale_staging_retention(&database, &pool, &schema, &project, &leased_id).await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn staging_cleanup_reports_a_held_project_lock_as_a_lock_timeout() {
    let (database, pool, schema) = open_isolated_database().await;
    database
        .migrate()
        .await
        .unwrap_or_else(|error| panic!("contended cleanup fixture migration failed: {error}"));
    assert!(
        !root_visible(&database).await,
        "an unknown root was visible"
    );
    let project = register_project(&database).await;
    assert!(
        !root_visible(&database).await,
        "an unpublished project was visible"
    );
    let abandoned = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let abandoned_id = abandoned.generation_id().clone();

    // Another writer's prepare transaction holds the project operation lock.
    let project_key = format!("cartograph-v2-operation:{schema}:{project}");
    let mut writer = pool
        .acquire()
        .await
        .unwrap_or_else(|error| panic!("contended cleanup writer connection failed: {error}"));
    query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&project_key)
        .execute(&mut *writer)
        .await
        .unwrap_or_else(|error| panic!("contended cleanup writer lock failed: {error}"));
    assert_eq!(
        database
            .fail_abandoned_staging_generations_bounded(&project, CONTENDED_LOCK_TIMEOUT)
            .await,
        Err(StorageError::StatementTimeout {
            operation: "fail-abandoned-staging-lock"
        })
    );
    assert_eq!(
        database
            .fail_unleased_staging_generation_bounded(
                GenerationRecoveryRequest::new(&project, &abandoned_id),
                CONTENDED_LOCK_TIMEOUT,
            )
            .await,
        Err(StorageError::StatementTimeout {
            operation: "fail-unleased-staging-lock"
        })
    );
    assert_state(
        &database,
        StateExpectation::new(&project, &abandoned_id, GenerationState::Staging),
    )
    .await;
    query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&project_key)
        .execute(&mut *writer)
        .await
        .unwrap_or_else(|error| panic!("contended cleanup writer unlock failed: {error}"));
    drop(writer);

    assert_live_lease_observation(&database, &project, &abandoned_id).await;
    assert_eq!(
        database
            .fail_abandoned_staging_generations_bounded(&project, CONTENDED_LOCK_TIMEOUT)
            .await,
        Ok(1)
    );
    publish_initial_generation(&database, &project).await;
    assert!(
        root_visible(&database).await,
        "a published project was not visible"
    );

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn root_visible(database: &CartographDatabase) -> bool {
    database
        .root_has_current_generation(PROJECT_ROOT, CONTENDED_LOCK_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("current generation visibility failed: {error}"))
}

/// A held lease of any operation is observed as live; a released one is not.
async fn assert_live_lease_observation(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
) {
    assert!(
        !live_lease(database, project).await,
        "no lease was reported live"
    );
    let index = acquire_generation_lease(database, project, generation).await;
    assert!(
        live_lease(database, project).await,
        "a held index lease was not live"
    );
    database
        .release_lease(&index)
        .await
        .unwrap_or_else(|error| panic!("observed index lease did not release: {error}"));
    let migration = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "live-lease-observation"),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("observed migration lease failed: {error}"));
    assert!(
        live_lease(database, project).await,
        "a held migration lease was not live"
    );
    database
        .release_lease(&migration)
        .await
        .unwrap_or_else(|error| panic!("observed migration lease did not release: {error}"));
    assert!(
        !live_lease(database, project).await,
        "a released lease was reported live"
    );
}

async fn live_lease(database: &CartographDatabase, project: &ProjectId) -> bool {
    database
        .has_live_lease(project, LOCK_ORDER_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("live lease probe failed: {error}"))
}

async fn assert_unleased_staging_cleanup(
    database: &CartographDatabase,
    project: &ProjectId,
) -> GenerationId {
    assert_single_unleased_staging_cleanup(database, project).await;
    assert_bulk_unleased_staging_cleanup_preserves_live_lease(database, project).await
}

async fn assert_single_unleased_staging_cleanup(
    database: &CartographDatabase,
    project: &ProjectId,
) {
    let abandoned = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let abandoned_id = abandoned.generation_id().clone();
    assert!(
        database
            .fail_unleased_staging_generation_bounded(
                GenerationRecoveryRequest::new(project, &abandoned_id),
                LOCK_ORDER_TIMEOUT,
            )
            .await
            .unwrap_or_else(|error| panic!("unleased staging cleanup failed: {error}"))
    );
    assert!(
        !database
            .fail_unleased_staging_generation_bounded(
                GenerationRecoveryRequest::new(project, &abandoned_id),
                LOCK_ORDER_TIMEOUT,
            )
            .await
            .unwrap_or_else(|error| panic!("idempotent staging cleanup failed: {error}"))
    );
    assert_state(
        database,
        StateExpectation::new(project, &abandoned_id, GenerationState::Failed),
    )
    .await;
}

async fn assert_bulk_unleased_staging_cleanup_preserves_live_lease(
    database: &CartographDatabase,
    project: &ProjectId,
) -> GenerationId {
    let leased = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let leased_id = leased.generation_id().clone();
    let exact_lease = acquire_generation_lease(database, project, &leased_id).await;
    assert!(
        !database
            .fail_unleased_staging_generation_bounded(
                GenerationRecoveryRequest::new(project, &leased_id),
                LOCK_ORDER_TIMEOUT,
            )
            .await
            .unwrap_or_else(|error| panic!("lease-safe staging cleanup failed: {error}"))
    );
    assert_state(
        database,
        StateExpectation::new(project, &leased_id, GenerationState::Staging),
    )
    .await;
    let first_abandoned = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let second_abandoned = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    assert_eq!(
        database
            .fail_abandoned_staging_generations_bounded(project, LOCK_ORDER_TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("bulk staging recovery failed: {error}")),
        2
    );
    for generation in [&first_abandoned, &second_abandoned] {
        assert_state(
            database,
            StateExpectation::new(project, generation.generation_id(), GenerationState::Failed),
        )
        .await;
    }
    assert_state(
        database,
        StateExpectation::new(project, &leased_id, GenerationState::Staging),
    )
    .await;
    database
        .release_lease(&exact_lease)
        .await
        .unwrap_or_else(|error| panic!("exact staging lease did not release: {error}"));
    leased_id
}

async fn assert_stale_staging_retention(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    leased_id: &GenerationId,
) {
    let recent = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_THREE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let recent_id = recent.generation_id().clone();
    let age_sql = format!(
        r#"UPDATE "{schema}"."index_generations"
            SET started_at = clock_timestamp() - interval '2 minutes'
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#
    );
    query(AssertSqlSafe(age_sql))
        .bind(project.as_str())
        .bind(leased_id.as_str())
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("stale staging fixture could not be aged: {error}"));
    let retention_lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "stale-staging-retention"),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("stale staging retention lease failed: {error}"));
    let retention = database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(2, 10)
                .and_then(|policy| policy.with_stale_staging_age(Duration::from_mins(1)))
                .unwrap_or_else(|error| panic!("stale staging policy failed: {error}")),
            &retention_lease.fence(),
            LOCK_ORDER_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("stale staging retention failed: {error}"));
    assert_eq!(retention.staging_removed, 1);
    assert_eq!(retention.staging_remaining, 1);
    assert_state(
        database,
        StateExpectation::new(project, &recent_id, GenerationState::Staging),
    )
    .await;
    database
        .release_lease(&retention_lease)
        .await
        .unwrap_or_else(|error| panic!("stale staging retention lease did not release: {error}"));
}
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn migrations_are_idempotent_and_only_published_generations_are_searchable() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    assert_deterministic_cochange_order_migration(&pool, &schema).await;
    assert_native_index_digest_migrations(&pool, &schema).await;
    let project = register_project(&database).await;
    let current_one = publish_initial_generation(&database, &project).await;
    let ready_older =
        prepare_rollback_retry(&database, &project, current_one.generation_id()).await;
    let current_two = publish_newer_generation(&database, &project, &ready_older).await;
    assert_deterministic_retrieval(&database, &project, current_two.generation_id()).await;
    assert_interactive_reads_timeout_and_pool_recovers(
        &database,
        &pool,
        &schema,
        &project,
        current_two.generation_id(),
    )
    .await;
    reject_stale_publication(&database, ready_older).await;
    assert_state(
        &database,
        StateExpectation::new(
            &project,
            current_one.generation_id(),
            GenerationState::Superseded,
        ),
    )
    .await;
    assert_state(
        &database,
        StateExpectation::new(
            &project,
            current_two.generation_id(),
            GenerationState::Current,
        ),
    )
    .await;
    assert_search(
        &database,
        SearchExpectation::empty(&project, "http response"),
    )
    .await;
    assert_search(
        &database,
        SearchExpectation::one(
            &project,
            "json payload",
            ExpectedHit {
                document_id: DOCUMENT_THREE,
                generation_id: current_two.generation_id(),
            },
        ),
    )
    .await;
    assert_restart_recovery(&database, &project).await;
    assert_validation_token_return(&database, &project).await;
    assert_ledger_tampering_is_refused(&database, &pool, &schema).await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn copied_relations_receive_planner_statistics_before_ready() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let copied_relations = [
        "files",
        "symbols",
        "edges",
        "references",
        "numerical_sites",
        "search_documents",
    ];
    disable_autoanalyze(&pool, &schema, &copied_relations).await;

    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let mut analyze_blocker = match pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("could not begin planner-statistics lock fixture: {error}"),
    };
    let lock = format!(r#"LOCK TABLE "{schema}"."files" IN SHARE UPDATE EXCLUSIVE MODE"#);
    if let Err(error) = query(AssertSqlSafe(lock))
        .execute(&mut *analyze_blocker)
        .await
    {
        panic!("could not lock planner-statistics fixture: {error}");
    }

    let prepare_database = database.clone();
    let prepare = tokio::spawn(async move {
        prepare_fenced(
            &prepare_database,
            staged,
            GenerationFacts {
                documents: vec![document(DocumentFixture {
                    id: DOCUMENT_ONE,
                    path: "src/planner_statistics.rs",
                    qualified_name: "planner_statistics",
                    code: "fn planner_statistics() {}",
                })],
                ..GenerationFacts::default()
            },
        )
        .await
    });
    wait_for_planner_statistics_lock(&pool, &schema).await;
    assert!(
        !prepare.is_finished(),
        "generation preparation skipped the contended planner-statistics relation"
    );
    if let Err(error) = analyze_blocker.rollback().await {
        panic!("could not release planner-statistics lock fixture: {error}");
    }
    let ready = match tokio::time::timeout(LOCK_ORDER_TIMEOUT, prepare).await {
        Ok(Ok(Ok(ready))) => ready,
        Ok(Ok(Err(error))) => panic!("planner-statistics prepare failed: {error}"),
        Ok(Err(error)) => panic!("planner-statistics prepare task failed: {error}"),
        Err(error) => {
            panic!("planner-statistics prepare did not resume after lock release: {error}")
        }
    };

    assert_manual_planner_statistics(&pool, &schema, copied_relations.len()).await;
    assert!(
        fail_fenced(&database, RecoverableGeneration::Ready(ready))
            .await
            .is_ok()
    );

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn disable_autoanalyze(pool: &sqlx_postgres::PgPool, schema: &str, relations: &[&str]) {
    for relation in relations {
        let statement =
            format!(r#"ALTER TABLE "{schema}"."{relation}" SET (autovacuum_enabled = false)"#);
        query(AssertSqlSafe(statement))
            .execute(pool)
            .await
            .unwrap_or_else(|error| {
                panic!("could not disable autoanalyze for {relation}: {error}")
            });
    }
}

async fn assert_manual_planner_statistics(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    expected_relations: usize,
) {
    let rows = query(
        r"SELECT relname, last_analyze IS NOT NULL, last_autoanalyze IS NULL
           FROM pg_stat_user_tables
           WHERE schemaname = $1
             AND relname IN ('files', 'symbols', 'edges', 'references', 'numerical_sites', 'search_documents')
           ORDER BY relname",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|error| panic!("could not read copied-relation statistics: {error}"));
    assert_eq!(rows.len(), expected_relations);
    for row in rows {
        let relation = row
            .try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("statistics relation was invalid: {error}"));
        assert!(
            row.try_get::<bool, _>(1)
                .unwrap_or_else(|error| panic!("manual analyze state was invalid: {error}")),
            "{relation} did not receive manual planner statistics"
        );
        assert!(
            row.try_get::<bool, _>(2)
                .unwrap_or_else(|error| panic!("autoanalyze state was invalid: {error}")),
            "{relation} was analyzed by autovacuum instead of generation preparation"
        );
    }
}

async fn wait_for_planner_statistics_lock(pool: &sqlx_postgres::PgPool, schema: &str) {
    let query_pattern = format!("%ANALYZE%{schema}%files%");
    for _ in 0..LOCK_OBSERVATION_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE pid <> pg_backend_pid()
                      AND application_name = 'cartograph-v2'
                      AND wait_event_type = 'Lock'
                      AND query LIKE $1
                ) AS waiting",
        )
        .bind(&query_pattern)
        .fetch_one(pool)
        .await;
        if matches!(
            row.and_then(|row| row.try_get::<bool, _>("waiting")),
            Ok(true)
        ) {
            return;
        }
        tokio::time::sleep(LOCK_OBSERVATION_INTERVAL).await;
    }
    panic!("generation preparation did not wait for the contended planner-statistics relation");
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn agent_sessions_trace_usage_and_macros_are_durable() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    assert_agent_session_migration(&pool, &schema).await;
    let project = register_project(&database).await;

    let (automatic_session_id, named_session_id) =
        assert_session_trace_lifecycle(&database, &project).await;
    assert_mcp_macro_lifecycle(&database, &project, &named_session_id).await;
    assert_ne!(automatic_session_id, named_session_id);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn assert_session_trace_lifecycle(
    database: &CartographDatabase,
    project: &ProjectId,
) -> (String, String) {
    let automatic = database
        .create_mcp_session(project, &NewMcpSession::automatic())
        .await
        .unwrap_or_else(|error| panic!("could not create automatic session: {error}"));
    let named_input = NewMcpSession::named("postgres-review", "Review durable query evidence")
        .unwrap_or_else(|error| panic!("could not build named session: {error}"));
    let named = database
        .create_mcp_session(project, &named_input)
        .await
        .unwrap_or_else(|error| panic!("could not create named session: {error}"));
    let first_call = McpToolCallInput::new(McpToolCallData {
        tool_name: "cartograph_status",
        arguments: serde_json::json!({"mode": "diagnostics"}),
        result_summary: "status current",
        success: true,
        duration_ms: 12,
    })
    .unwrap_or_else(|error| panic!("could not build first trace call: {error}"));
    let second_call = McpToolCallInput::new(McpToolCallData {
        tool_name: "cartograph_find",
        arguments: serde_json::json!({"query": "missing"}),
        result_summary: "not_found: symbol was not found",
        success: false,
        duration_ms: 48,
    })
    .unwrap_or_else(|error| panic!("could not build second trace call: {error}"));
    let recorded_one = database
        .record_mcp_tool_call(McpToolCallWrite {
            project_id: project,
            session_id: named.session_id(),
            input: &first_call,
        })
        .await
        .unwrap_or_else(|error| panic!("could not record first trace call: {error}"));
    let recorded_two = database
        .record_mcp_tool_call(McpToolCallWrite {
            project_id: project,
            session_id: named.session_id(),
            input: &second_call,
        })
        .await
        .unwrap_or_else(|error| panic!("could not record second trace call: {error}"));
    assert_eq!(recorded_one.step(), 1);
    assert_eq!(recorded_two.step(), 2);
    let calls = database
        .mcp_calls_for_session(McpSessionCallsQuery {
            project_id: project,
            session_id: named.session_id(),
            limit: 100,
        })
        .await
        .unwrap_or_else(|error| panic!("could not read trace calls: {error}"));
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].tool_name(), "cartograph_status");
    assert!(!calls[1].success());
    let found = database
        .find_mcp_session(McpSessionLookup {
            project_id: project,
            session_id: None,
            label: Some("postgres-review"),
            require_calls: false,
        })
        .await
        .unwrap_or_else(|error| panic!("could not find named session: {error}"))
        .unwrap_or_else(|| panic!("named session was missing"));
    assert_eq!(found.session_id(), named.session_id());
    assert_eq!(found.tool_count(), 2);
    let sessions = database
        .list_mcp_sessions(project, 10)
        .await
        .unwrap_or_else(|error| panic!("could not list sessions: {error}"));
    assert_eq!(sessions.len(), 2);
    let usage = serde_json::to_value(
        database
            .mcp_trace_usage(project)
            .await
            .unwrap_or_else(|error| panic!("could not aggregate trace usage: {error}")),
    )
    .unwrap_or_else(|error| panic!("could not serialize trace usage: {error}"));
    assert_eq!(usage["sessionCount"], 2);
    assert_eq!(usage["toolCallCount"], 2);
    assert_eq!(usage["errorCount"], 1);
    (
        automatic.session_id().to_owned(),
        named.session_id().to_owned(),
    )
}

async fn assert_mcp_macro_lifecycle(
    database: &CartographDatabase,
    project: &ProjectId,
    named_session_id: &str,
) {
    let macro_step = McpMacroStep::new(
        "cartograph_find",
        serde_json::Map::from_iter([(
            "query".to_owned(),
            serde_json::Value::String("${0}".to_owned()),
        )]),
    )
    .unwrap_or_else(|error| panic!("could not build macro step: {error}"));
    let macro_input = NewMcpMacro::new("find-symbol", vec![macro_step])
        .unwrap_or_else(|error| panic!("could not build macro: {error}"));
    let saved = database
        .save_mcp_macro(project, &macro_input)
        .await
        .unwrap_or_else(|error| panic!("could not save macro: {error}"));
    assert_eq!(saved.name(), "find-symbol");
    database
        .mark_mcp_macro_run(project, "find-symbol")
        .await
        .unwrap_or_else(|error| panic!("could not mark macro run: {error}"));
    let macros = database
        .list_mcp_macros(project, 10)
        .await
        .unwrap_or_else(|error| panic!("could not list macros: {error}"));
    assert_eq!(macros.len(), 1);
    assert_eq!(macros[0].steps().len(), 1);
    assert!(
        database
            .delete_mcp_macro(project, "find-symbol")
            .await
            .unwrap_or_else(|error| panic!("could not delete macro: {error}"))
    );
    assert!(
        database
            .delete_mcp_session(project, named_session_id)
            .await
            .unwrap_or_else(|error| panic!("could not delete session: {error}"))
    );
    assert_eq!(
        database
            .mcp_calls_for_session(McpSessionCallsQuery {
                project_id: project,
                session_id: named_session_id,
                limit: 100,
            })
            .await
            .unwrap_or_else(|error| panic!("could not verify cascade: {error}")),
        []
    );
}
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn agent_artifacts_and_summary_digest_fences_are_durable() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    assert_agent_evidence_migration(&pool, &schema).await;
    assert_agent_session_migration(&pool, &schema).await;
    let project = register_project(&database).await;
    let initial = publish_initial_generation(&database, &project).await;
    let ready_older = prepare_rollback_retry(&database, &project, initial.generation_id()).await;
    let current = publish_newer_generation(&database, &project, &ready_older).await;

    let note_artifact_id = assert_agent_note_queries(&database, &project).await;
    let summary = seed_agent_role_fixture(&database, &project, current.generation_id()).await;
    assert_project_scoped_read_only_sql(&database, &project).await;
    assert_summary_digest_fence(&database, &project, &summary).await;
    assert!(
        database
            .delete_agent_artifact(&project, &note_artifact_id)
            .await
            .unwrap_or_else(|error| panic!("could not delete durable note: {error}"))
    );

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn role_cache_is_scoped_to_the_model_that_wrote_it() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let initial = publish_initial_generation(&database, &project).await;
    let ready_older = prepare_rollback_retry(&database, &project, initial.generation_id()).await;
    publish_newer_generation(&database, &project, &ready_older).await;
    let target = parse_symbol_id(RETRIEVAL_TARGET);
    let caller = parse_symbol_id(RETRIEVAL_CALLER);
    let save = async |symbol: &SymbolId, via: &str, model: &str| {
        database
            .save_symbol_role(
                SymbolRoleSaveInput::new(&project, symbol, "business_logic")
                    .with_metadata(serde_json::json!({"via": via, "model": model})),
            )
            .await
            .unwrap_or_else(|error| panic!("could not save role fixture: {error}"));
    };
    let pending = async |sweep: RoleSweepModel<'_>| {
        database
            .pending_symbol_roles(&project, sweep, 20)
            .await
            .unwrap_or_else(|error| panic!("could not list pending roles: {error}"))
            .iter()
            .map(|symbol| symbol.symbol_id().to_owned())
            .collect::<Vec<_>>()
    };
    let structural = RoleSweepModel::Structural("structural-v2");
    let jev = RoleSweepModel::Judge("jev-roles-v1");
    let chat = RoleSweepModel::Judge("chat-model");
    let caller_pending = vec![caller.as_str().to_owned()];

    // A structural rule match is final for every sweep.
    save(&target, "rule", "structural-v1").await;
    // A fallback written by an older rule version is re-classified by rules and models.
    save(&caller, "structural_fallback", "structural-v1").await;
    assert_eq!(pending(structural).await, caller_pending);
    assert_eq!(pending(jev).await, caller_pending);

    // A model's own judgment is cached for it, re-judged by another model,
    // and never downgraded by a rules-only sweep.
    save(&caller, "jev", "jev-roles-v1").await;
    assert_eq!(pending(structural).await, [] as [String; 0]);
    assert_eq!(pending(jev).await, [] as [String; 0]);
    assert_eq!(pending(chat).await, caller_pending);

    // A current rules fallback satisfies rules but still invites a model.
    save(&caller, "structural_fallback", "structural-v2").await;
    assert_eq!(pending(structural).await, [] as [String; 0]);
    assert_eq!(pending(jev).await, caller_pending);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn assert_agent_note_queries(database: &CartographDatabase, project: &ProjectId) -> String {
    let note = NewAgentArtifact::new(
        AgentArtifactKind::Note,
        AgentArtifactScope::Project,
        AgentArtifactContent::new("project", "Review the public API boundary before editing."),
    )
    .unwrap_or_else(|error| panic!("could not build note fixture: {error}"));
    let created = database
        .create_agent_artifact(project, &note)
        .await
        .unwrap_or_else(|error| panic!("could not create durable note: {error}"));
    let notes = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)
                .unwrap_or_else(|error| panic!("could not build note query: {error}"))
                .with_kind(AgentArtifactKind::Note),
        )
        .await
        .unwrap_or_else(|error| panic!("could not list durable notes: {error}"));
    assert_eq!(notes.len(), 1);
    let question = NewAgentArtifact::new(
        AgentArtifactKind::Note,
        AgentArtifactScope::Project,
        AgentArtifactContent::new("project", "Does this boundary need an integration test?"),
    )
    .and_then(|artifact| {
        artifact.with_metadata(serde_json::json!({
            "noteKind": "question",
            "author": "integration-test"
        }))
    })
    .unwrap_or_else(|error| panic!("could not build question note: {error}"));
    database
        .create_agent_artifact(project, &question)
        .await
        .unwrap_or_else(|error| panic!("could not create question note: {error}"));
    let questions = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)
                .unwrap_or_else(|error| panic!("could not build question query: {error}"))
                .with_kind(AgentArtifactKind::Note)
                .with_note_kind("question")
                .unwrap_or_else(|error| panic!("could not filter question notes: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("could not list question notes: {error}"));
    assert_eq!(questions.len(), 1);
    created.artifact_id().to_owned()
}

struct AgentSummaryFixture {
    policy: SummaryCandidatePolicy,
    pending_count: usize,
    symbol_id: SymbolId,
    source_digest: ContentDigest,
}

async fn seed_agent_role_fixture(
    database: &CartographDatabase,
    project: &ProjectId,
    generation_id: &GenerationId,
) -> AgentSummaryFixture {
    let summary_policy = SummaryCandidatePolicy::new(1, BTreeMap::new())
        .unwrap_or_else(|error| panic!("could not build summary policy: {error}"));
    let pending = database
        .pending_symbol_summaries_with_policy(project, 20, &summary_policy)
        .await
        .unwrap_or_else(|error| panic!("could not list pending summaries: {error}"));
    let candidate = pending
        .first()
        .unwrap_or_else(|| panic!("published fixture must expose a pending symbol summary"));
    let symbol_id = SymbolId::parse(candidate.symbol_id())
        .unwrap_or_else(|error| panic!("pending summary symbol id was invalid: {error}"));
    let source_digest = ContentDigest::parse(candidate.content_hash())
        .unwrap_or_else(|error| panic!("pending summary digest was invalid: {error}"));
    let role = NewAgentArtifact::new(
        AgentArtifactKind::Role,
        AgentArtifactScope::Symbol,
        AgentArtifactContent::new(symbol_id.as_str(), "business_logic"),
    )
    .unwrap_or_else(|error| panic!("could not build role fixture: {error}"))
    .with_generation(generation_id.clone())
    .with_source_digest(source_digest.clone())
    .with_state(AgentArtifactState::Complete)
    .unwrap_or_else(|error| panic!("could not complete role fixture: {error}"));
    database
        .replace_scoped_agent_artifact(project, &role)
        .await
        .unwrap_or_else(|error| panic!("could not save role fixture: {error}"));
    database
        .replace_scoped_agent_artifact(project, &role)
        .await
        .unwrap_or_else(|error| panic!("could not replace role fixture: {error}"));
    let roles = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)
                .unwrap_or_else(|error| panic!("could not build role query: {error}"))
                .with_kind(AgentArtifactKind::Role)
                .with_body("business_logic")
                .unwrap_or_else(|error| panic!("could not filter role query: {error}"))
                .current_generation_only(),
        )
        .await
        .unwrap_or_else(|error| panic!("could not list role fixture: {error}"));
    assert_eq!(roles.len(), 1);
    assert_eq!(
        database
            .agent_role_distribution(project)
            .await
            .unwrap_or_else(|error| panic!("could not aggregate role fixture: {error}"))
            .len(),
        1
    );
    AgentSummaryFixture {
        policy: summary_policy,
        pending_count: pending.len(),
        symbol_id,
        source_digest,
    }
}

async fn assert_project_scoped_read_only_sql(database: &CartographDatabase, project: &ProjectId) {
    let sql = ReadOnlySqlRequest::new(
        "SELECT qualified_name, symbol_kind FROM symbols ORDER BY qualified_name",
        20,
        Duration::from_secs(10),
    )
    .unwrap_or_else(|error| panic!("could not build read-only SQL fixture: {error}"));
    let sql_result = database
        .execute_read_only_sql(project, &sql)
        .await
        .unwrap_or_else(|error| panic!("project-scoped SQL failed: {error}"));
    let sql_json = serde_json::to_value(sql_result)
        .unwrap_or_else(|error| panic!("SQL result did not serialize: {error}"));
    assert_eq!(sql_json["rows"].as_array().map(Vec::len), Some(2));
    let explain = ReadOnlySqlRequest::new(
        "EXPLAIN SELECT * FROM symbols WHERE symbol_kind = 'function'",
        100,
        Duration::from_secs(10),
    )
    .unwrap_or_else(|error| panic!("could not build SQL explain fixture: {error}"));
    let explain_result = database
        .execute_read_only_sql(project, &explain)
        .await
        .unwrap_or_else(|error| panic!("project-scoped SQL explain failed: {error}"));
    let explain_json = serde_json::to_value(explain_result)
        .unwrap_or_else(|error| panic!("SQL explain did not serialize: {error}"));
    assert_eq!(explain_json["explain"], true);
}

async fn assert_summary_digest_fence(
    database: &CartographDatabase,
    project: &ProjectId,
    fixture: &AgentSummaryFixture,
) {
    database
        .save_symbol_summary(
            SymbolSummarySaveInput::new(project, &fixture.symbol_id, &fixture.source_digest)
                .with_summary("Handles the live artifact fixture.")
                .with_model("integration-test"),
        )
        .await
        .unwrap_or_else(|error| panic!("could not save current summary: {error}"));
    let after = database
        .pending_symbol_summaries_with_policy(project, 20, &fixture.policy)
        .await
        .unwrap_or_else(|error| panic!("could not refresh pending summaries: {error}"));
    assert_eq!(after.len() + 1, fixture.pending_count);
    let stale_digest = digest("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    assert_matches!(
        database
            .save_symbol_summary(
                SymbolSummarySaveInput::new(project, &fixture.symbol_id, &stale_digest)
                    .with_summary("This must not overwrite current evidence.")
                    .with_model("integration-test"),
            )
            .await,
        Err(StorageError::CurrentGenerationChanged)
    );
}
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn publication_carries_only_digest_identical_derived_evidence() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let initial_staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let initial_ready = prepare_fenced(
        &database,
        initial_staged,
        derived_evidence_generation_facts(),
    )
    .await
    .unwrap_or_else(|error| panic!("initial evidence generation failed: {error}"));
    let initial = publish_fenced(&database, initial_ready)
        .await
        .unwrap_or_else(|error| panic!("initial evidence generation did not publish: {error}"));
    seed_current_derived_evidence(&database, &pool, &schema, &project, initial.generation_id())
        .await;

    let identical_staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let identical_ready = prepare_fenced(
        &database,
        identical_staged,
        derived_evidence_generation_facts(),
    )
    .await
    .unwrap_or_else(|error| panic!("identical evidence generation failed: {error}"));
    let identical = publish_fenced(&database, identical_ready)
        .await
        .unwrap_or_else(|error| panic!("identical evidence generation did not publish: {error}"));
    assert_current_derived_evidence(
        &database,
        &pool,
        &schema,
        &project,
        identical.generation_id(),
        1,
    )
    .await;

    let mut changed_facts = derived_evidence_generation_facts();
    changed_facts.symbols[0].structural_digest =
        digest("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    changed_facts.documents[0].code = "fn decode_json_payload() { changed(); }".to_owned();
    let changed_staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_THREE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let changed_ready = prepare_fenced(&database, changed_staged, changed_facts)
        .await
        .unwrap_or_else(|error| panic!("changed evidence generation failed: {error}"));
    let changed = publish_fenced(&database, changed_ready)
        .await
        .unwrap_or_else(|error| panic!("changed evidence generation did not publish: {error}"));
    assert_current_derived_evidence(
        &database,
        &pool,
        &schema,
        &project,
        changed.generation_id(),
        0,
    )
    .await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn seed_current_derived_evidence(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation_id: &GenerationId,
) {
    let symbol_id = parse_symbol_id(RETRIEVAL_TARGET);
    let document_id = parse_document_id(DOCUMENT_THREE);
    let structural_digest = digest(DIGEST_ONE);
    database
        .save_symbol_role(
            SymbolRoleSaveInput::new(project, &symbol_id, "business_logic")
                .with_metadata(serde_json::json!({"via": "integration-test"})),
        )
        .await
        .unwrap_or_else(|error| panic!("could not save role evidence: {error}"));
    database
        .save_symbol_summary(
            SymbolSummarySaveInput::new(project, &symbol_id, &structural_digest)
                .with_summary("Decodes a JSON payload.")
                .with_model("integration-test"),
        )
        .await
        .unwrap_or_else(|error| panic!("could not save summary evidence: {error}"));
    seed_generation_bound_evidence(
        pool,
        schema,
        project,
        generation_id,
        &symbol_id,
        &document_id,
    )
    .await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn cold_derived_prune_removes_only_unreferenced_artifacts_and_terminal_vectors() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let initial_staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let initial_ready = prepare_fenced(&database, initial_staged, retrieval_generation_facts())
        .await
        .unwrap_or_else(|error| panic!("initial prune generation failed: {error}"));
    let initial = publish_fenced(&database, initial_ready)
        .await
        .unwrap_or_else(|error| panic!("initial prune generation did not publish: {error}"));
    seed_current_derived_evidence(&database, &pool, &schema, &project, initial.generation_id())
        .await;
    seed_prune_survivor_note(&database, &project).await;

    let mut changed_facts = retrieval_generation_facts();
    changed_facts.symbols[0].structural_digest =
        digest("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    changed_facts.documents[0].code = "fn decode_json_payload() { changed(); }".to_owned();
    let changed_staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let changed_ready = prepare_fenced(&database, changed_staged, changed_facts)
        .await
        .unwrap_or_else(|error| panic!("changed prune generation failed: {error}"));
    let changed = publish_fenced(&database, changed_ready)
        .await
        .unwrap_or_else(|error| panic!("changed prune generation did not publish: {error}"));

    let lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "derived-prune-test"),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("could not acquire prune lease: {error}"));
    let policy = DerivedStorePrunePolicy::new(Duration::ZERO, 100)
        .unwrap_or_else(|error| panic!("could not build prune policy: {error}"));
    let report = database
        .prune_cold_derived_store(DerivedStorePruneRequest::new(
            policy,
            &lease.fence(),
            Duration::from_secs(30),
        ))
        .await
        .unwrap_or_else(|error| panic!("cold derived prune failed: {error}"));
    assert_eq!(report.summaries_pruned, 1);
    assert_eq!(report.roles_pruned, 1);
    assert_eq!(report.embeddings_pruned, 1);
    assert_eq!(report.total_pruned(), 3);
    assert!(!report.artifacts_truncated);
    assert!(!report.embeddings_truncated);
    assert!(database.release_lease(&lease).await.is_ok());

    assert_post_prune_state(
        &database,
        &pool,
        &schema,
        &project,
        initial.generation_id(),
        changed.generation_id(),
    )
    .await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn seed_prune_survivor_note(database: &CartographDatabase, project: &ProjectId) {
    let note = NewAgentArtifact::new(
        AgentArtifactKind::Note,
        AgentArtifactScope::Project,
        AgentArtifactContent::new(
            "project",
            "This operator note must survive derived-store pruning.",
        ),
    )
    .unwrap_or_else(|error| panic!("could not build prune note: {error}"));
    database
        .create_agent_artifact(project, &note)
        .await
        .unwrap_or_else(|error| panic!("could not seed prune note: {error}"));
}

async fn assert_post_prune_state(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    previous_generation: &GenerationId,
    current_generation: &GenerationId,
) {
    let notes = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)
                .unwrap_or_else(|error| panic!("could not build post-prune note query: {error}"))
                .with_kind(AgentArtifactKind::Note),
        )
        .await
        .unwrap_or_else(|error| panic!("could not read post-prune notes: {error}"));
    assert_eq!(notes.len(), 1);
    assert_matches!(
        database.current_generation_record(project).await,
        Ok(Some(current)) if current.generation_id() == current_generation
    );
    let old_vectors = query(AssertSqlSafe(format!(
        r#"SELECT count(*)::bigint FROM "{schema}"."document_embeddings"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#
    )))
    .bind(project.as_str())
    .bind(previous_generation.as_str())
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>(0))
    .unwrap_or_else(|error| panic!("could not count post-prune vectors: {error}"));
    assert_eq!(old_vectors, 0);
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn exact_name_indexes_are_live_frozen_and_used_at_scale() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    assert_exact_lookup_migration(&pool, &schema).await;
    assert_generation_search_migration(&pool, &schema).await;
    assert_typed_symbol_semantics_migration(&pool, &schema).await;
    assert_agent_evidence_migration(&pool, &schema).await;
    assert_agent_session_migration(&pool, &schema).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    seed_exact_lookup_scale(
        &pool,
        &schema,
        &project,
        staged.generation_id(),
        EXACT_LOOKUP_SCALE_ROWS,
    )
    .await;
    assert_exact_lookup_plans_use_indexes(&pool, &schema, &project, staged.generation_id()).await;

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn expected_generation_reads_survive_publication_without_mixing_snapshots() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let current = publish_initial_generation(&database, &project).await;
    seed_generation_fence_symbol(&pool, &schema, &project, current.generation_id()).await;
    let ready = prepare_rollback_retry(&database, &project, current.generation_id()).await;

    let mut blocker = pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("generation fence blocker begin failed: {error}"));
    let lock = format!(r#"LOCK TABLE "{schema}"."symbols" IN ACCESS EXCLUSIVE MODE"#);
    query(AssertSqlSafe(lock))
        .execute(&mut *blocker)
        .await
        .unwrap_or_else(|error| panic!("generation fence symbols lock failed: {error}"));

    let lookup_database = database.clone();
    let lookup_project = project.clone();
    let expected_generation = current.generation_id().clone();
    let lookup = tokio::spawn(async move {
        lookup_database
            .exact_current_symbols_by_name(ExactTextLookup::new(
                CurrentGenerationLookup::new(&lookup_project, &expected_generation),
                "fencedCanary",
                SEARCH_LIMIT,
            ))
            .await
    });
    wait_for_table_query_lock(&pool, &schema, "symbols").await;
    let replacement = publish_fenced(&database, ready)
        .await
        .unwrap_or_else(|error| panic!("generation fence publication failed: {error}"));
    blocker
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("generation fence blocker rollback failed: {error}"));

    let observed = lookup
        .await
        .unwrap_or_else(|error| panic!("generation fence lookup task failed: {error}"))
        .unwrap_or_else(|error| panic!("generation fence lookup failed: {error}"));
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].generation_id(), current.generation_id());
    assert_eq!(observed[0].qualified_name(), "fencedCanary");
    assert_eq!(
        database
            .exact_current_symbols_by_name(ExactTextLookup::new(
                CurrentGenerationLookup::new(&project, current.generation_id()),
                "fencedCanary",
                SEARCH_LIMIT,
            ))
            .await,
        Err(StorageError::CurrentGenerationChanged)
    );
    let replacement_rows = database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&project, replacement.generation_id()),
            "fencedCanary",
            SEARCH_LIMIT,
        ))
        .await
        .unwrap_or_else(|error| panic!("replacement generation lookup failed: {error}"));
    assert_eq!(replacement_rows, []);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn bm25_scores_and_order_are_generation_isolated_from_distractors() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let current = publish_bm25_isolation_generation(&database, &project).await;
    let before = bm25_signature(&database, &project, current.generation_id()).await;

    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    prepare(
        &database,
        PrepareFixture {
            staged,
            document: document(DocumentFixture {
                id: DOCUMENT_THREE,
                path: "src/ready_distractor.rs",
                qualified_name: "readyDistractor",
                code: "fn ready_distractor() { isolationtoken(); }",
            }),
        },
    )
    .await;

    let other_project = database
        .register_project(NewProject::new(
            "workspace/cartograph-other",
            digest(DIGEST_ONE),
        ))
        .await
        .unwrap_or_else(|error| panic!("other BM25 project registration failed: {error}"));
    let other_staged = begin(
        &database,
        GenerationFixture {
            project: &other_project,
            revision: REVISION_FOUR,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let other_ready = prepare(
        &database,
        PrepareFixture {
            staged: other_staged,
            document: document(DocumentFixture {
                id: DOCUMENT_FOUR,
                path: "src/other_distractor.rs",
                qualified_name: "otherDistractor",
                code: "fn other_distractor() { isolationtoken(); }",
            }),
        },
    )
    .await;
    publish_fenced(&database, other_ready)
        .await
        .unwrap_or_else(|error| panic!("other BM25 project publication failed: {error}"));

    let after = bm25_signature(&database, &project, current.generation_id()).await;
    assert_eq!(after, before);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn concurrent_bm25_reader_observes_one_complete_generation_relation() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let current = publish_initial_generation(&database, &project).await;
    let ready = prepare_rollback_retry(&database, &project, current.generation_id()).await;
    let table = search_relation_table_name(current.generation_id());
    let mut blocker = pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("BM25 relation blocker begin failed: {error}"));
    let lock = format!(r#"LOCK TABLE "{schema}"."{table}" IN ACCESS EXCLUSIVE MODE"#);
    query(AssertSqlSafe(lock))
        .execute(&mut *blocker)
        .await
        .unwrap_or_else(|error| panic!("BM25 relation lock failed: {error}"));

    let reader_database = database.clone();
    let reader_project = project.clone();
    let reader_generation = current.generation_id().clone();
    let reader = tokio::spawn(async move {
        reader_database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&reader_project, &reader_generation),
                "http response",
                SEARCH_LIMIT,
            ))
            .await
    });
    wait_for_table_query_lock(&pool, &schema, &table).await;
    let replacement = publish_fenced(&database, ready)
        .await
        .unwrap_or_else(|error| panic!("concurrent BM25 publication failed: {error}"));
    blocker
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("BM25 relation blocker rollback failed: {error}"));

    let old_hits = reader
        .await
        .unwrap_or_else(|error| panic!("BM25 reader task failed: {error}"))
        .unwrap_or_else(|error| panic!("old BM25 reader failed: {error}"));
    assert_eq!(old_hits.len(), 1);
    assert_eq!(old_hits[0].generation_id(), current.generation_id());
    assert_eq!(old_hits[0].document_id().as_str(), DOCUMENT_ONE);
    assert_eq!(
        database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&project, current.generation_id()),
                "http response",
                SEARCH_LIMIT,
            ))
            .await,
        Err(StorageError::CurrentGenerationChanged)
    );
    let new_hits = database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(&project, replacement.generation_id()),
            "json payload",
            SEARCH_LIMIT,
        ))
        .await
        .unwrap_or_else(|error| panic!("replacement BM25 reader failed: {error}"));
    assert_eq!(new_hits.len(), 1);
    assert_eq!(new_hits[0].generation_id(), replacement.generation_id());
    assert_eq!(new_hits[0].document_id().as_str(), DOCUMENT_TWO);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn startup_rebuilds_crash_lost_search_relation_and_removes_trusted_orphan() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let current = publish_bm25_isolation_generation(&database, &project).await;
    let before = bm25_signature(&database, &project, current.generation_id()).await;
    let table = search_relation_table_name(current.generation_id());
    let drop_current = format!(r#"DROP TABLE "{schema}"."{table}""#);
    query(AssertSqlSafe(drop_current))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("crash-loss relation drop failed: {error}"));
    assert_eq!(
        database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&project, current.generation_id()),
                "isolationtoken",
                SEARCH_LIMIT,
            ))
            .await,
        Err(StorageError::SearchRelationUnavailable)
    );

    let orphan_generation = parse_generation_id("99999999-9999-4999-8999-999999999999");
    let orphan_table = search_relation_table_name(&orphan_generation);
    let create_orphan = format!(r#"CREATE TABLE "{schema}"."{orphan_table}" (id bigint)"#);
    query(AssertSqlSafe(create_orphan))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("orphan relation creation failed: {error}"));
    let unrelated_table = "searchXg_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let create_unrelated = format!(r#"CREATE TABLE "{schema}"."{unrelated_table}" (id bigint)"#);
    query(AssertSqlSafe(create_unrelated))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("unrelated relation creation failed: {error}"));

    let report = database
        .migrate()
        .await
        .unwrap_or_else(|error| panic!("startup search repair failed: {error}"));
    assert_eq!(report.applied_versions, [] as [i64; 0]);
    assert_eq!(report.current_version, LATEST_MIGRATION_VERSION);
    let after = bm25_signature(&database, &project, current.generation_id()).await;
    assert_eq!(after, before);
    let orphan_exists = query("SELECT to_regclass($1) IS NOT NULL AS exists")
        .bind(format!("{schema}.{orphan_table}"))
        .fetch_one(&pool)
        .await
        .and_then(|row| row.try_get::<bool, _>("exists"))
        .unwrap_or_else(|error| panic!("orphan relation verification failed: {error}"));
    assert!(!orphan_exists);
    let unrelated_exists = query("SELECT to_regclass($1) IS NOT NULL AS exists")
        .bind(format!(r#"{schema}."{unrelated_table}""#))
        .fetch_one(&pool)
        .await
        .and_then(|row| row.try_get::<bool, _>("exists"))
        .unwrap_or_else(|error| panic!("unrelated relation verification failed: {error}"));
    assert!(unrelated_exists);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn failed_generation_search_build_never_reaches_ready_or_publication() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let current = publish_initial_generation(&database, &project).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_TWO,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let table = search_relation_table_name(&generation_id);
    let create = format!(r#"CREATE TABLE "{schema}"."{table}" (sentinel bigint)"#);
    query(AssertSqlSafe(create))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("failed-build barrier table creation failed: {error}"));
    let mut blocker = pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("failed-build blocker begin failed: {error}"));
    let lock = format!(r#"LOCK TABLE "{schema}"."{table}" IN ACCESS SHARE MODE"#);
    query(AssertSqlSafe(lock))
        .execute(&mut *blocker)
        .await
        .unwrap_or_else(|error| panic!("failed-build relation lock failed: {error}"));
    let lease = acquire_generation_lease(&database, &project, &generation_id).await;
    let fence = lease.fence();
    let result = database
        .prepare_generation_bounded(
            GenerationContents::new(
                staged,
                canonical(GenerationFacts {
                    documents: vec![document(DocumentFixture {
                        id: DOCUMENT_TWO,
                        path: "src/failed_build.rs",
                        qualified_name: "failedBuild",
                        code: "fn failed_build() {}",
                    })],
                    ..GenerationFacts::default()
                }),
            ),
            cartograph_db::PrepareGenerationMutation::new(&fence, FAILED_SEARCH_BUILD_TIMEOUT),
        )
        .await;
    let staged = match result {
        Err(error) => {
            assert_matches!(
                error.error(),
                StorageError::DatabaseOperation {
                    operation: "search-relation-drop"
                },
                "blocked search-relation build returned an unexpected error: {:?}",
                error.error()
            );
            error.into_parts().0
        }
        Ok(_) => panic!("blocked generation search build unexpectedly became ready"),
    };
    blocker
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("failed-build blocker rollback failed: {error}"));
    assert!(database.release_lease(&lease).await.is_ok());
    let drop_barrier = format!(r#"DROP TABLE "{schema}"."{table}""#);
    query(AssertSqlSafe(drop_barrier))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("failed-build barrier cleanup failed: {error}"));
    assert_state(
        &database,
        StateExpectation::new(&project, &generation_id, GenerationState::Staging),
    )
    .await;
    let current_after = database
        .current_generation_record(&project)
        .await
        .unwrap_or_else(|error| panic!("failed-build current lookup failed: {error}"))
        .unwrap_or_else(|| panic!("failed-build current generation disappeared"));
    assert_eq!(current_after.generation_id(), current.generation_id());
    assert_generation_search_rows(&pool, &schema, &project, &generation_id, 0, 0).await;
    assert!(
        fail_fenced(&database, RecoverableGeneration::Staged(staged))
            .await
            .is_ok()
    );

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn stale_fences_cannot_prepare_or_publish_after_exact_token_takeover() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let target = LeaseTarget::new(
        project.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    let stale_prepare_lease = acquire_generation_lease(&database, &project, &generation_id).await;
    expire_generation_lease(&pool, &schema, &target).await;
    let prepare_lease = acquire_generation_lease(&database, &project, &generation_id).await;
    install_copy_rejection_sentinel(&pool, &schema).await;
    let stale_prepare = database
        .prepare_generation(
            GenerationContents::new(
                staged,
                canonical(GenerationFacts {
                    documents: vec![document(DocumentFixture {
                        id: DOCUMENT_ONE,
                        path: "src/stale_copy.rs",
                        qualified_name: "stale_copy_probe",
                        code: "fn stale_copy_probe() {}",
                    })],
                    ..GenerationFacts::default()
                }),
            ),
            &stale_prepare_lease.fence(),
        )
        .await;
    remove_copy_rejection_sentinel(&pool, &schema).await;
    let staged = match stale_prepare {
        Err(error) if *error.error() == StorageError::LeaseFenceLost => error.into_parts().0,
        Err(error) => panic!("stale prepare fence returned wrong error: {error}"),
        Ok(_) => panic!("stale prepare fence committed after takeover"),
    };
    let ready = match database
        .prepare_generation(
            GenerationContents::new(staged, canonical(GenerationFacts::default())),
            &prepare_lease.fence(),
        )
        .await
    {
        Ok(ready) => ready,
        Err(error) => panic!("current prepare fence failed: {error}"),
    };
    expire_generation_lease(&pool, &schema, &target).await;
    let publish_lease = acquire_generation_lease(&database, &project, &generation_id).await;
    let ready = match database
        .publish_generation(ready, &prepare_lease.fence())
        .await
    {
        Err(error) if *error.error() == StorageError::LeaseFenceLost => error.into_parts().0,
        Err(error) => panic!("stale publish fence returned wrong error: {error}"),
        Ok(_) => panic!("stale publish fence committed after takeover"),
    };
    let current = match database
        .publish_generation(ready, &publish_lease.fence())
        .await
    {
        Ok(current) => current,
        Err(error) => panic!("current publish fence failed: {error}"),
    };
    assert_eq!(current.generation_id(), &generation_id);
    assert_matches!(database.lease_status(&target).await, Ok(None));

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn concurrent_prepare_and_cleanup_share_one_pre_copy_lock_order() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let target = LeaseTarget::new(
        project.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    let lease = acquire_generation_lease(&database, &project, &generation_id).await;
    let fence = lease.fence();
    let barrier_key = format!("cartograph-test-copy-barrier:{schema}");
    install_copy_barrier(&pool, &schema, &barrier_key).await;
    let mut barrier = match pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => panic!("could not acquire COPY barrier connection: {error}"),
    };
    if let Err(error) = query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&barrier_key)
        .execute(&mut *barrier)
        .await
    {
        panic!("could not establish COPY barrier: {error}");
    }

    let prepare_fence = fence.clone();
    let prepare_database = database.clone();
    let prepare = prepare_database.prepare_generation(
        GenerationContents::new(
            staged,
            canonical(GenerationFacts {
                documents: vec![document(DocumentFixture {
                    id: DOCUMENT_ONE,
                    path: "src/lock_order.rs",
                    qualified_name: "lock_order_probe",
                    code: "fn lock_order_probe() {}",
                })],
                ..GenerationFacts::default()
            }),
        ),
        &prepare_fence,
    );
    tokio::pin!(prepare);
    let cleanup_fence = fence.clone();
    let cleanup_database = database.clone();
    let concurrent_cleanup = async {
        wait_for_copy_barrier(&pool, &schema).await;
        let cleanup = cleanup_database.fail_generation_and_release(&cleanup_fence);
        tokio::pin!(cleanup);
        assert!(
            tokio::time::timeout(LOCK_OBSERVATION_INTERVAL, &mut cleanup)
                .await
                .is_err()
        );
        if let Err(error) = query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&barrier_key)
            .execute(&mut *barrier)
            .await
        {
            panic!("could not release COPY barrier: {error}");
        }
        cleanup.await
    };
    let joined = tokio::time::timeout(LOCK_ORDER_TIMEOUT, async {
        tokio::join!(&mut prepare, concurrent_cleanup)
    })
    .await;
    let (prepare, cleanup) = joined.unwrap_or_else(|error| {
        panic!("prepare and cleanup deadlocked across COPY foreign-key locks: {error}")
    });
    assert!(prepare.is_ok());
    assert!(cleanup.is_ok());
    assert_matches!(
        database.generation_state(&project, &generation_id).await,
        Ok(Some(GenerationState::Failed))
    );
    assert_matches!(database.lease_status(&target).await, Ok(None));

    drop(barrier);
    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn ready_transition_fence_allows_heartbeats_but_blocks_release() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let mut lease = acquire_generation_lease(&database, &project, &generation_id).await;
    install_ready_transition_delay(&pool, &schema).await;

    let prepare_database = database.clone();
    let prepare_fence = lease.fence();
    let prepare = tokio::spawn(async move {
        prepare_database
            .prepare_generation(
                GenerationContents::new(staged, canonical(GenerationFacts::default())),
                &prepare_fence,
            )
            .await
    });
    wait_for_ready_transition_delay(&pool, &schema).await;

    database
        .heartbeat_lease_bounded(&mut lease, HEARTBEAT_LOCK_PROBE_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("ready transition blocked its lease heartbeat: {error}"));
    assert!(
        database
            .release_lease_bounded(&lease, RELEASE_LOCK_PROBE_TIMEOUT)
            .await
            .is_err(),
        "ready transition did not protect its exact lease from release"
    );

    let ready = tokio::time::timeout(LOCK_ORDER_TIMEOUT, prepare)
        .await
        .unwrap_or_else(|_| panic!("delayed ready transition did not finish"))
        .unwrap_or_else(|error| panic!("ready transition task failed: {error}"))
        .unwrap_or_else(|error| panic!("ready transition failed: {error}"));
    assert_eq!(ready.generation_id(), &generation_id);
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("ready transition lease release failed: {error}"));

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn bounded_heartbeat_avoids_synchronous_commit_stalls() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let mut lease = acquire_generation_lease(&database, &project, staged.generation_id()).await;
    install_heartbeat_commit_mode_guard(&pool, &schema).await;

    database
        .heartbeat_lease_bounded(&mut lease, HEARTBEAT_LOCK_PROBE_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("bounded heartbeat used a blocking commit mode: {error}"));
    database
        .release_lease(&lease)
        .await
        .unwrap_or_else(|error| panic!("heartbeat fixture lease release failed: {error}"));

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn stale_publish_and_post_index_retention_share_one_lock_order() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let staged = begin(
        &database,
        GenerationFixture {
            project: &project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let target = LeaseTarget::new(
        project.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    let stale_lease = acquire_generation_lease(&database, &project, &generation_id).await;
    let stale_fence = stale_lease.fence();
    let ready = database
        .prepare_generation(
            GenerationContents::new(staged, canonical(GenerationFacts::default())),
            &stale_fence,
        )
        .await
        .unwrap_or_else(|error| panic!("stale publication fixture did not become ready: {error}"));
    expire_generation_lease(&pool, &schema, &target).await;
    let retention_lease = database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "post-index-lock-order"),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("post-index retention lease failed: {error}"));
    assert_publish_retention_lock_order(
        &database,
        &pool,
        &schema,
        &project,
        ready,
        stale_fence,
        &retention_lease,
    )
    .await;
    database
        .release_lease(&retention_lease)
        .await
        .unwrap_or_else(|error| panic!("post-index retention lease did not release: {error}"));

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

async fn assert_publish_retention_lock_order(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    ready: ReadyGeneration,
    stale_fence: cartograph_db::LeaseFence,
    retention_lease: &ProjectLease,
) {
    let project_key = format!("cartograph-v2-operation:{schema}:{project}");
    let publication_key = format!("cartograph-v2-publish:{schema}:{project}");
    let mut project_blocker = pool
        .acquire()
        .await
        .unwrap_or_else(|error| panic!("project lock-order blocker connection failed: {error}"));
    query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&project_key)
        .execute(&mut *project_blocker)
        .await
        .unwrap_or_else(|error| panic!("project lock-order blocker failed: {error}"));
    assert_eq!(
        advisory_waiter_count(pool, &project_key).await,
        0,
        "project lock-order fixture started with an unexpected waiter"
    );
    let publish_database = database.clone();
    let publish = tokio::spawn(async move {
        publish_database
            .publish_generation(ready, &stale_fence)
            .await
    });
    wait_for_advisory_waiters(pool, &project_key, 1, "stale publication").await;

    let publication_available = query("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(&publication_key)
        .fetch_one(&mut *project_blocker)
        .await
        .and_then(|row| row.try_get::<bool, _>(0))
        .unwrap_or_else(|error| panic!("publication lock-order probe failed: {error}"));
    assert!(
        publication_available,
        "publication acquired its advisory lock before the project operation lock"
    );

    let retention_database = database.clone();
    let retention_fence = retention_lease.fence();
    let retention = tokio::spawn(async move {
        retention_database
            .cleanup_generations(GenerationRetentionRequest::new(
                GenerationRetentionPolicy::new(2, 10)
                    .unwrap_or_else(|error| panic!("post-index retention policy failed: {error}")),
                &retention_fence,
                LOCK_ORDER_TIMEOUT,
            ))
            .await
    });
    wait_for_advisory_waiters(pool, &project_key, 2, "post-index retention").await;
    query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&project_key)
        .execute(&mut *project_blocker)
        .await
        .unwrap_or_else(|error| panic!("project lock-order blocker did not release: {error}"));
    query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&publication_key)
        .execute(&mut *project_blocker)
        .await
        .unwrap_or_else(|error| panic!("publication lock-order probe did not release: {error}"));

    let joined = tokio::time::timeout(LOCK_ORDER_TIMEOUT, async {
        tokio::join!(publish, retention)
    })
    .await
    .unwrap_or_else(|error| panic!("stale publication and retention deadlocked: {error}"));
    let publish = match joined.0 {
        Ok(publish) => publish,
        Err(error) => panic!("stale publication task failed: {error}"),
    };
    assert_matches!(
        publish,
        Err(error) if *error.error() == StorageError::LeaseFenceLost
    );
    joined
        .1
        .unwrap_or_else(|error| panic!("post-index retention task failed: {error}"))
        .unwrap_or_else(|error| panic!("post-index retention failed: {error}"));
}

async fn advisory_waiter_count(pool: &sqlx_postgres::PgPool, lock_key: &str) -> i64 {
    query(
        r"WITH lock_identity AS (
                SELECT pg_catalog.hashtextextended($1, 0) AS value
            )
            SELECT count(*)::bigint
            FROM pg_catalog.pg_locks, lock_identity
            WHERE locktype = 'advisory'
              AND database = (
                  SELECT oid FROM pg_catalog.pg_database
                  WHERE datname = current_database()
              )
              AND classid::bigint = ((lock_identity.value >> 32) & 4294967295)
              AND objid::bigint = (lock_identity.value & 4294967295)
              AND objsubid = 1
              AND NOT granted",
    )
    .bind(lock_key)
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>(0))
    .unwrap_or_else(|error| panic!("could not count advisory waiters: {error}"))
}

async fn wait_for_advisory_waiters(
    pool: &sqlx_postgres::PgPool,
    lock_key: &str,
    expected: i64,
    operation: &str,
) {
    for _ in 0..LOCK_OBSERVATION_ATTEMPTS {
        if advisory_waiter_count(pool, lock_key).await >= expected {
            return;
        }
        tokio::time::sleep(LOCK_OBSERVATION_INTERVAL).await;
    }
    let observed = advisory_waiter_count(pool, lock_key).await;
    panic!("expected {expected} {operation} advisory waiters, observed {observed}");
}

async fn install_copy_barrier(pool: &sqlx_postgres::PgPool, schema: &str, barrier_key: &str) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}"."block_search_document_copy"()
            RETURNS trigger LANGUAGE plpgsql AS $body$
            BEGIN
                PERFORM pg_advisory_xact_lock(hashtextextended('{barrier_key}', 0));
                RETURN NULL;
            END
            $body$"#
    );
    if let Err(error) = query(AssertSqlSafe(function)).execute(pool).await {
        panic!("could not install COPY barrier function: {error}");
    }
    let trigger = format!(
        r#"CREATE TRIGGER "block_search_document_copy"
            AFTER INSERT ON "{schema}"."search_documents"
            FOR EACH STATEMENT
            EXECUTE FUNCTION "{schema}"."block_search_document_copy"()"#
    );
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(pool).await {
        panic!("could not install COPY barrier trigger: {error}");
    }
}

async fn install_ready_transition_delay(pool: &sqlx_postgres::PgPool, schema: &str) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}"."delay_generation_ready"()
            RETURNS trigger LANGUAGE plpgsql AS $body$
            BEGIN
                IF OLD.state = 'staging' AND NEW.state = 'ready' THEN
                    PERFORM pg_sleep({READY_TRANSITION_DELAY_SECONDS});
                END IF;
                RETURN NEW;
            END
            $body$"#
    );
    query(AssertSqlSafe(function))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not install ready-transition delay: {error}"));
    let trigger = format!(
        r#"CREATE TRIGGER "delay_generation_ready"
            BEFORE UPDATE OF state ON "{schema}"."index_generations"
            FOR EACH ROW
            EXECUTE FUNCTION "{schema}"."delay_generation_ready"()"#
    );
    query(AssertSqlSafe(trigger))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not install ready-transition trigger: {error}"));
}

async fn install_heartbeat_commit_mode_guard(pool: &sqlx_postgres::PgPool, schema: &str) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}"."require_async_heartbeat_commit"()
            RETURNS trigger LANGUAGE plpgsql AS $body$
            BEGIN
                IF current_setting('synchronous_commit') <> 'off' THEN
                    RAISE EXCEPTION 'heartbeat commit mode was synchronous';
                END IF;
                RETURN NEW;
            END
            $body$"#
    );
    query(AssertSqlSafe(function))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not install heartbeat commit guard: {error}"));
    let trigger = format!(
        r#"CREATE TRIGGER "require_async_heartbeat_commit"
            BEFORE UPDATE ON "{schema}"."project_operation_leases"
            FOR EACH ROW
            EXECUTE FUNCTION "{schema}"."require_async_heartbeat_commit"()"#
    );
    query(AssertSqlSafe(trigger))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not install heartbeat commit trigger: {error}"));
}

async fn wait_for_ready_transition_delay(pool: &sqlx_postgres::PgPool, schema: &str) {
    let query_pattern = format!("%{schema}%index_generations%");
    for _ in 0..LOCK_OBSERVATION_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'cartograph-v2'
                      AND wait_event = 'PgSleep'
                      AND query LIKE $1
                ) AS waiting",
        )
        .bind(&query_pattern)
        .fetch_one(pool)
        .await;
        if matches!(
            row.and_then(|row| row.try_get::<bool, _>("waiting")),
            Ok(true)
        ) {
            return;
        }
        tokio::time::sleep(LOCK_OBSERVATION_INTERVAL).await;
    }
    panic!("prepare did not reach its delayed ready transition");
}

async fn install_copy_rejection_sentinel(pool: &sqlx_postgres::PgPool, schema: &str) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}"."reject_stale_search_document_copy"()
            RETURNS trigger LANGUAGE plpgsql AS $body$
            BEGIN
                RAISE EXCEPTION 'stale lease reached COPY';
            END
            $body$"#
    );
    if let Err(error) = query(AssertSqlSafe(function)).execute(pool).await {
        panic!("could not install stale-COPY sentinel function: {error}");
    }
    let trigger = format!(
        r#"CREATE TRIGGER "reject_stale_search_document_copy"
            BEFORE INSERT ON "{schema}"."search_documents"
            FOR EACH STATEMENT
            EXECUTE FUNCTION "{schema}"."reject_stale_search_document_copy"()"#
    );
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(pool).await {
        panic!("could not install stale-COPY sentinel trigger: {error}");
    }
}

async fn remove_copy_rejection_sentinel(pool: &sqlx_postgres::PgPool, schema: &str) {
    let trigger = format!(
        r#"DROP TRIGGER "reject_stale_search_document_copy"
            ON "{schema}"."search_documents""#
    );
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(pool).await {
        panic!("could not remove stale-COPY sentinel trigger: {error}");
    }
    let function = format!(r#"DROP FUNCTION "{schema}"."reject_stale_search_document_copy"()"#);
    if let Err(error) = query(AssertSqlSafe(function)).execute(pool).await {
        panic!("could not remove stale-COPY sentinel function: {error}");
    }
}

async fn wait_for_copy_barrier(pool: &sqlx_postgres::PgPool, schema: &str) {
    let query_pattern = format!("%{schema}%search_documents%");
    for _ in 0..LOCK_OBSERVATION_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'cartograph-v2'
                      AND wait_event_type = 'Lock'
                      AND query LIKE $1
                ) AS waiting",
        )
        .bind(&query_pattern)
        .fetch_one(pool)
        .await;
        if matches!(
            row.and_then(|row| row.try_get::<bool, _>("waiting")),
            Ok(true)
        ) {
            return;
        }
        tokio::time::sleep(LOCK_OBSERVATION_INTERVAL).await;
    }
    panic!("prepare COPY did not reach the deterministic advisory barrier");
}

async fn wait_for_table_query_lock(pool: &sqlx_postgres::PgPool, schema: &str, table: &str) {
    for _ in 0..LOCK_OBSERVATION_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1
                    FROM pg_catalog.pg_stat_activity AS activity
                    INNER JOIN pg_catalog.pg_locks AS locks
                      ON locks.pid = activity.pid
                    INNER JOIN pg_catalog.pg_class AS tables
                      ON tables.oid = locks.relation
                    INNER JOIN pg_catalog.pg_namespace AS namespaces
                      ON namespaces.oid = tables.relnamespace
                    WHERE activity.application_name = 'cartograph-v2'
                      AND activity.wait_event_type = 'Lock'
                      AND NOT locks.granted
                      AND namespaces.nspname = $1
                      AND tables.relname = $2
                ) AS waiting",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(pool)
        .await;
        if matches!(
            row.and_then(|row| row.try_get::<bool, _>("waiting")),
            Ok(true)
        ) {
            return;
        }
        tokio::time::sleep(LOCK_OBSERVATION_INTERVAL).await;
    }
    panic!("bounded retrieval did not reach the {table} lock barrier");
}

fn search_relation_table_name(generation: &GenerationId) -> String {
    let compact = generation.as_str().replace('-', "");
    assert_eq!(compact.len(), 32);
    assert!(compact.bytes().all(|byte| byte.is_ascii_hexdigit()));
    format!("search_g_{compact}")
}

async fn seed_generation_fence_symbol(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
) {
    let file = format!(
        r#"INSERT INTO "{schema}"."files" (
                project_id, generation_id, file_id, normalized_path, language,
                content_hash, byte_size, parse_status
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid),
                'src/fenced_canary.rs', 'rust', $4, 64, 'parsed'
            )"#
    );
    query(AssertSqlSafe(file))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(RETRIEVAL_FILE)
        .bind(DIGEST_ONE)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("generation fence file seed failed: {error}"));
    let symbol = format!(
        r#"INSERT INTO "{schema}"."symbols" (
                project_id, generation_id, symbol_id, file_id, symbol_kind,
                qualified_name, signature, start_byte, end_byte,
                start_line, end_line, structural_digest
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid),
                CAST($4 AS uuid), 'function', 'fencedCanary',
                'fn fenced_canary()', 0, 20, 1, 1, $5
            )"#
    );
    query(AssertSqlSafe(symbol))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(RETRIEVAL_TARGET)
        .bind(RETRIEVAL_FILE)
        .bind(DIGEST_ONE)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("generation fence symbol seed failed: {error}"));
}

async fn assert_generation_search_rows(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    expected_documents: i64,
    expected_catalog_rows: i64,
) {
    let sql = format!(
        r#"SELECT
                (SELECT count(*)::bigint FROM "{schema}"."search_documents"
                 WHERE project_id = CAST($1 AS uuid)
                   AND generation_id = CAST($2 AS uuid)) AS documents,
                (SELECT count(*)::bigint FROM "{schema}"."generation_search_relations"
                 WHERE project_id = CAST($1 AS uuid)
                   AND generation_id = CAST($2 AS uuid)) AS catalog_rows"#
    );
    let row = query(AssertSqlSafe(sql))
        .bind(project.as_str())
        .bind(generation.as_str())
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("generation search row verification failed: {error}"));
    assert_eq!(
        row.try_get::<i64, _>("documents")
            .unwrap_or_else(|error| panic!("document count decode failed: {error}")),
        expected_documents
    );
    assert_eq!(
        row.try_get::<i64, _>("catalog_rows")
            .unwrap_or_else(|error| panic!("catalog count decode failed: {error}")),
        expected_catalog_rows
    );
}

#[derive(Clone, Copy)]
struct DocumentFixture<'a> {
    id: &'a str,
    path: &'a str,
    qualified_name: &'a str,
    code: &'a str,
}

struct StateExpectation<'a> {
    project: &'a ProjectId,
    generation: &'a GenerationId,
    state: GenerationState,
}

struct GenerationFixture<'a> {
    project: &'a ProjectId,
    revision: &'a str,
    workers: u16,
}

struct PrepareFixture {
    staged: cartograph_db::StagedGeneration,
    document: SearchDocumentInput,
}

#[derive(Clone, Copy)]
struct ExpectedHit<'a> {
    document_id: &'a str,
    generation_id: &'a GenerationId,
}

impl<'a> StateExpectation<'a> {
    const fn new(
        project: &'a ProjectId,
        generation: &'a GenerationId,
        state: GenerationState,
    ) -> Self {
        Self {
            project,
            generation,
            state,
        }
    }
}

struct SearchExpectation<'a> {
    project: &'a ProjectId,
    query: &'a str,
    document_id: Option<&'a str>,
    generation_id: Option<&'a GenerationId>,
}

impl<'a> SearchExpectation<'a> {
    const fn empty(project: &'a ProjectId, query: &'a str) -> Self {
        Self {
            project,
            query,
            document_id: None,
            generation_id: None,
        }
    }

    const fn one(project: &'a ProjectId, query: &'a str, hit: ExpectedHit<'a>) -> Self {
        Self {
            project,
            query,
            document_id: Some(hit.document_id),
            generation_id: Some(hit.generation_id),
        }
    }
}

async fn assert_migration_ledger(database: &CartographDatabase) {
    let first = match database.migrate().await {
        Ok(report) => report,
        Err(error) => panic!("initial migration failed: {error}"),
    };
    assert_eq!(first.applied_versions, EXPECTED_MIGRATIONS);
    assert_eq!(first.current_version, LATEST_MIGRATION_VERSION);
    assert_matches!(
        database.migrate().await,
        Ok(report)
            if report.applied_versions.is_empty()
                && report.current_version == LATEST_MIGRATION_VERSION
    );
}

async fn register_project(database: &CartographDatabase) -> ProjectId {
    match database
        .register_project(NewProject::new(PROJECT_ROOT, digest(DIGEST_ONE)))
        .await
    {
        Ok(project) => project,
        Err(error) => panic!("project registration failed: {error}"),
    }
}

async fn publish_initial_generation(
    database: &CartographDatabase,
    project: &ProjectId,
) -> CurrentGeneration {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let ready = prepare(
        database,
        PrepareFixture {
            staged,
            document: document(DocumentFixture {
                id: DOCUMENT_ONE,
                path: "src/http_parser.rs",
                qualified_name: "parseHTTPResponse",
                code: "fn parse_http_response() -> Response { todo!() }",
            }),
        },
    )
    .await;
    assert_matches!(database.current_generation_record(project).await, Ok(None));
    let current = match publish_fenced(database, ready).await {
        Ok(generation) => generation,
        Err(error) => panic!("first generation did not publish: {error}"),
    };
    assert_search(
        database,
        SearchExpectation::one(
            project,
            "http response",
            ExpectedHit {
                document_id: DOCUMENT_ONE,
                generation_id: current.generation_id(),
            },
        ),
    )
    .await;
    // Field provenance is probed inside the BM25 index for the returned rows
    // only; the name and code match while the natural text does not.
    let hits = database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(project, current.generation_id()),
            "http response",
            SEARCH_LIMIT,
        ))
        .await
        .unwrap_or_else(|error| panic!("component search failed: {error}"));
    assert_eq!(
        hits[0].components(),
        [
            cartograph_db::SearchComponent::QualifiedName,
            cartograph_db::SearchComponent::Code
        ]
    );
    current
}

async fn publish_bm25_isolation_generation(
    database: &CartographDatabase,
    project: &ProjectId,
) -> CurrentGeneration {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_ONE,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let facts = GenerationFacts {
        documents: vec![
            document(DocumentFixture {
                id: DOCUMENT_ONE,
                path: "src/isolation_one.rs",
                qualified_name: "isolationOne",
                code: "fn isolation_one() { isolationtoken(); }",
            }),
            document(DocumentFixture {
                id: DOCUMENT_TWO,
                path: "src/isolation_two.rs",
                qualified_name: "isolationTwo",
                code: "fn isolation_two() { isolationtoken(); isolationtoken(); }",
            }),
        ],
        ..GenerationFacts::default()
    };
    let ready = prepare_fenced(database, staged, facts)
        .await
        .unwrap_or_else(|error| panic!("BM25 isolation generation prepare failed: {error}"));
    publish_fenced(database, ready)
        .await
        .unwrap_or_else(|error| panic!("BM25 isolation generation publication failed: {error}"))
}

async fn bm25_signature(
    database: &CartographDatabase,
    project: &ProjectId,
    generation: &GenerationId,
) -> Vec<(String, u64)> {
    database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(project, generation),
            "isolationtoken",
            SEARCH_LIMIT,
        ))
        .await
        .unwrap_or_else(|error| panic!("BM25 isolation query failed: {error}"))
        .into_iter()
        .map(|hit| (hit.document_id().as_str().to_owned(), hit.score().to_bits()))
        .collect()
}

async fn prepare_rollback_retry(
    database: &CartographDatabase,
    project: &ProjectId,
    current_generation_id: &GenerationId,
) -> ReadyGeneration {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_TWO,
            workers: REPLACEMENT_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    let duplicate = document(DocumentFixture {
        id: DOCUMENT_TWO,
        path: "src/new_parser.rs",
        qualified_name: "decodeJSONPayload",
        code: "fn decode_json_payload() {}",
    });
    let mut conflicting = duplicate.clone();
    "fn decode_json_payload() { unreachable!() }".clone_into(&mut conflicting.code);
    let invalid = validate_for_test(GenerationFacts {
        documents: vec![duplicate.clone(), conflicting],
        ..GenerationFacts::default()
    });
    assert_matches!(
        invalid.err(),
        Some(GenerationValidationError::Storage(
            StorageError::InvalidInput {
                field: "duplicate_document_id"
            }
        ))
    );
    assert_state(
        database,
        StateExpectation::new(project, &generation_id, GenerationState::Staging),
    )
    .await;
    assert_search(
        database,
        SearchExpectation::one(
            project,
            "http response",
            ExpectedHit {
                document_id: DOCUMENT_ONE,
                generation_id: current_generation_id,
            },
        ),
    )
    .await;
    prepare(
        database,
        PrepareFixture {
            staged,
            document: duplicate,
        },
    )
    .await
}

async fn publish_newer_generation(
    database: &CartographDatabase,
    project: &ProjectId,
    ready_older: &ReadyGeneration,
) -> CurrentGeneration {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_THREE,
            workers: REPLACEMENT_WORKERS,
        },
    )
    .await;
    assert!(staged.sequence() > ready_older.sequence());
    let ready = match prepare_fenced(database, staged, retrieval_generation_facts()).await {
        Ok(ready) => ready,
        Err(error) => panic!("retrieval generation did not become ready: {error}"),
    };
    match publish_fenced(database, ready).await {
        Ok(generation) => generation,
        Err(error) => panic!("newer generation did not publish: {error}"),
    }
}

fn retrieval_generation_facts() -> GenerationFacts {
    let file_id = parse_file_id(RETRIEVAL_FILE);
    let target_id = parse_symbol_id(RETRIEVAL_TARGET);
    let caller_id = parse_symbol_id(RETRIEVAL_CALLER);
    let mut search_document = document(DocumentFixture {
        id: DOCUMENT_THREE,
        path: "src/json_decoder.rs",
        qualified_name: "decoder::decodeJSONPayload",
        code: "fn decode_json_payload() {}",
    });
    search_document.file_id = Some(file_id.clone());
    search_document.symbol_id = Some(target_id.clone());
    GenerationFacts {
        files: vec![FileInput {
            file_id: file_id.clone(),
            normalized_path: "src/json_decoder.rs".to_owned(),
            language: "rust".to_owned(),
            content_hash: digest(DIGEST_ONE),
            byte_size: 256,
            parse_status: FileParseStatus::Parsed,
        }],
        numerical_sites: vec![retrieval_numerical_site(&file_id, &caller_id)],
        symbols: vec![
            SymbolInput {
                symbol_id: target_id.clone(),
                file_id: file_id.clone(),
                symbol_kind: "function".to_owned(),
                qualified_name: "decoder::decodeJSONPayload".to_owned(),
                signature: "fn decode_json_payload()".to_owned(),
                start_byte: 0,
                end_byte: 24,
                start_line: 1,
                end_line: 1,
                structural_digest: digest(DIGEST_ONE),
                visibility: Some(Visibility::Public),
                export: cartograph_domain::SymbolExportFlags::named(true),
                execution: cartograph_domain::SymbolExecutionFlags::default(),
                declaration_only: false,
                betweenness_ppb: None,
                pagerank_ppb: None,
            },
            SymbolInput {
                symbol_id: caller_id.clone(),
                file_id: file_id.clone(),
                symbol_kind: "function".to_owned(),
                qualified_name: "decoder::decode_request".to_owned(),
                signature: "fn decode_request()".to_owned(),
                start_byte: 25,
                end_byte: 80,
                start_line: 2,
                end_line: 4,
                structural_digest: digest(DIGEST_ONE),
                visibility: Some(Visibility::Private),
                export: cartograph_domain::SymbolExportFlags::default(),
                execution: cartograph_domain::SymbolExecutionFlags {
                    async_symbol: true,
                    static_member: false,
                },
                declaration_only: false,
                betweenness_ppb: None,
                pagerank_ppb: None,
            },
        ],
        edges: vec![EdgeInput {
            source_symbol_id: caller_id.clone(),
            target_symbol_id: target_id.clone(),
            kind: EdgeKind::Calls,
            confidence: 1.0,
            provenance: "live-retrieval-fixture".to_owned(),
            site_count: 1,
        }],
        references: vec![ReferenceInput {
            file_id,
            owner_symbol_id: Some(caller_id),
            target_symbol_id: Some(target_id),
            reference_name: "decodeJSONPayload".to_owned(),
            reference_kind: "calls".to_owned(),
            start_byte: 40,
            end_byte: 57,
            confidence: 1.0,
            resolution_provenance: "live-retrieval-fixture".to_owned(),
            site_count: 1,
            span_precision: cartograph_db::ReferenceSpanPrecision::Exact,
        }],
        documents: vec![search_document],
    }
}

fn retrieval_numerical_site(file_id: &FileId, owner_symbol_id: &SymbolId) -> NumericalSiteInput {
    NumericalSiteInput {
        site_id: NumericalSiteId::parse(RETRIEVAL_NUMERICAL_SITE)
            .unwrap_or_else(|error| panic!("numerical site fixture ID was invalid: {error}")),
        file_id: file_id.clone(),
        owner_symbol_id: Some(owner_symbol_id.clone()),
        start_byte: 40,
        end_byte: 57,
        start_line: 2,
        end_line: 2,
        operation: "tolerance_comparison".to_owned(),
        hazard: "absolute_only_tolerance".to_owned(),
        precision: "f64".to_owned(),
        expression_digest: digest(DIGEST_ONE),
        confidence_ppm: 900_000,
        provenance: "rust_ast_v1".to_owned(),
        evidence_level: "heuristic".to_owned(),
        unknowns: "relative_scale,input_range".to_owned(),
    }
}

fn derived_evidence_generation_facts() -> GenerationFacts {
    let mut facts = retrieval_generation_facts();
    facts.documents[0].natural_text.clear();
    facts
}

async fn seed_generation_bound_evidence(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    symbol: &SymbolId,
    document: &DocumentId,
) {
    let model = format!(
        r#"INSERT INTO "{schema}"."embedding_models" (
                model_id, fingerprint, provider, model_name, dimension, normalization
            ) VALUES (
                CAST($1 AS uuid), $2, 'integration-test', 'tiny-vector', 3, 'none'
            )"#,
    );
    query(AssertSqlSafe(model))
        .bind(EVIDENCE_MODEL)
        .bind("9999999999999999999999999999999999999999999999999999999999999999")
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed embedding model: {error}"));
    let embedding = format!(
        r#"INSERT INTO "{schema}"."document_embeddings" (
                project_id, generation_id, document_id, model_id, source_digest, embedding
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), CAST($4 AS uuid),
                $5, CAST('[1,2,3]' AS vector)
            )"#,
    );
    query(AssertSqlSafe(embedding))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(document.as_str())
        .bind(EVIDENCE_MODEL)
        .bind("8888888888888888888888888888888888888888888888888888888888888888")
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed document embedding: {error}"));
    let coverage_source = format!(
        r#"INSERT INTO "{schema}"."coverage_sources" (
                project_id, label, report_format, report_digest
            ) VALUES (CAST($1 AS uuid), 'integration-test', 'lcov', $2)
            RETURNING source_id::text"#,
    );
    let source_id = query(AssertSqlSafe(coverage_source))
        .bind(project.as_str())
        .bind("7777777777777777777777777777777777777777777777777777777777777777")
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>(0))
        .unwrap_or_else(|error| panic!("could not seed coverage source: {error}"));
    let coverage = format!(
        r#"INSERT INTO "{schema}"."symbol_coverage" (
                project_id, generation_id, source_id, symbol_id,
                lines_found, lines_hit, functions_found, functions_hit
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), CAST($4 AS uuid),
                10, 8, 1, 1
            )"#,
    );
    query(AssertSqlSafe(coverage))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(source_id)
        .bind(symbol.as_str())
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed symbol coverage: {error}"));
    let similarity_edge = format!(
        r#"INSERT INTO "{schema}"."symbol_similarity_edges" (
                project_id, generation_id, model_id, source_symbol_id,
                target_symbol_id, score, neighbor_rank
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), CAST($4 AS uuid),
                CAST($5 AS uuid), 0.8, 1
            )"#,
    );
    query(AssertSqlSafe(similarity_edge))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(EVIDENCE_MODEL)
        .bind(symbol.as_str())
        .bind(RETRIEVAL_CALLER)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed similarity edge: {error}"));
    let similarity_build = format!(
        r#"INSERT INTO "{schema}"."symbol_similarity_builds" (
                project_id, generation_id, model_id, neighbors_per_symbol,
                minimum_score, source_symbols, edges_written
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), 10, 0.3, 1, 1
            )"#,
    );
    query(AssertSqlSafe(similarity_build))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(EVIDENCE_MODEL)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed similarity build: {error}"));
}

async fn assert_current_derived_evidence(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    expected: i64,
) {
    let roles = database
        .list_agent_artifacts(
            project,
            AgentArtifactQuery::new(10)
                .unwrap_or_else(|error| panic!("could not build role query: {error}"))
                .with_kind(AgentArtifactKind::Role)
                .current_generation_only(),
        )
        .await
        .unwrap_or_else(|error| panic!("could not read current roles: {error}"));
    assert_eq!(i64::try_from(roles.len()).unwrap_or(i64::MAX), expected);
    let summary_policy = SummaryCandidatePolicy::new(1, BTreeMap::new())
        .unwrap_or_else(|error| panic!("could not build summary policy: {error}"));
    let summaries = database
        .current_summary_coverage_with_policy(project, &summary_policy)
        .await
        .unwrap_or_else(|error| panic!("could not read current summaries: {error}"));
    let summaries = serde_json::to_value(summaries)
        .unwrap_or_else(|error| panic!("could not serialize summary coverage: {error}"));
    assert_eq!(summaries["summarizedSymbols"].as_i64(), Some(expected));

    for table in [
        "document_embeddings",
        "symbol_coverage",
        "symbol_similarity_edges",
        "symbol_similarity_builds",
    ] {
        let statement = format!(
            r#"SELECT COUNT(*)::bigint FROM "{schema}"."{table}"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)"#,
        );
        let count = query(AssertSqlSafe(statement))
            .bind(project.as_str())
            .bind(generation.as_str())
            .fetch_one(pool)
            .await
            .and_then(|row| row.try_get::<i64, _>(0))
            .unwrap_or_else(|error| panic!("could not count {table}: {error}"));
        assert_eq!(count, expected, "unexpected current {table} rows");
    }
}

async fn assert_deterministic_retrieval(
    database: &CartographDatabase,
    project: &ProjectId,
    generation_id: &GenerationId,
) {
    let generation = database.current_generation_record(project).await;
    assert_matches!(
        generation,
        Ok(Some(generation)) if generation.generation_id() == generation_id
    );
    let path = NormalizedPath::parse("src/json_decoder.rs")
        .unwrap_or_else(|error| panic!("retrieval path fixture is invalid: {error}"));
    assert_current_file_retrieval(database, project, generation_id, &path).await;
    assert_current_symbol_retrieval(database, project, generation_id, &path).await;
    assert_current_numerical_retrieval(database, project, generation_id).await;
}

async fn assert_current_numerical_retrieval(
    database: &CartographDatabase,
    project: &ProjectId,
    generation_id: &GenerationId,
) {
    let query = NumericalSiteQuery::new(10)
        .and_then(|query| query.with_path_prefix(Some("src")))
        .map(|query| query.with_owner_symbol_id(Some(parse_symbol_id(RETRIEVAL_CALLER))))
        .and_then(|query| query.with_hazard(Some("absolute_only_tolerance")))
        .and_then(|query| query.with_evidence_level(Some("heuristic")))
        .map_or_else(
            |error| panic!("numerical query fixture was invalid: {error}"),
            |query| query.with_hazards_only(true),
        );
    let page = database
        .current_numerical_sites(CurrentGenerationLookup::new(project, generation_id), &query)
        .await
        .unwrap_or_else(|error| panic!("current numerical-site query failed: {error}"));
    assert_eq!(page.generation_id(), generation_id);
    assert_eq!(page.total(), 1);
    assert!(!page.truncated());
    assert_matches!(
        page.sites(),
        [site]
            if site.numerical_site_id().as_str() == RETRIEVAL_NUMERICAL_SITE
                && site.owner_symbol_id().map(SymbolId::as_str) == Some(RETRIEVAL_CALLER)
                && site.path().as_str() == "src/json_decoder.rs"
                && site.operation() == "tolerance_comparison"
                && site.hazard() == "absolute_only_tolerance"
                && site.precision() == "f64"
                && site.evidence_level() == "heuristic"
                && site.unknowns() == ["relative_scale", "input_range"]
    );

    let stats = database
        .current_numerical_site_stats(CurrentGenerationLookup::new(project, generation_id))
        .await
        .unwrap_or_else(|error| panic!("current numerical-site stats failed: {error}"));
    assert_eq!(stats.generation_id(), generation_id);
    assert_eq!(stats.total_sites(), 1);
    assert_eq!(stats.hazard_sites(), 1);
    assert_eq!(stats.supported_files(), 1);
    assert_eq!(stats.analyzed_files(), 1);
    assert_eq!(stats.unanalyzed_files(), 0);
    assert_eq!(stats.files_with_sites(), 1);
}

async fn assert_current_file_retrieval(
    database: &CartographDatabase,
    project: &ProjectId,
    generation_id: &GenerationId,
    path: &NormalizedPath,
) {
    let file = database
        .exact_current_file_by_path(CurrentFileLookup::new(project, generation_id, path))
        .await;
    assert_matches!(
        file,
        Ok(Some(file)) if file.file_id().as_str() == RETRIEVAL_FILE
    );

    let directory = NormalizedPath::parse("src")
        .unwrap_or_else(|error| panic!("retrieval directory fixture is invalid: {error}"));
    let files = database
        .current_files(
            CurrentFilesLookup::new(project, generation_id, SEARCH_LIMIT)
                .within_directory(&directory)
                .with_language(cartograph_domain::SourceLanguage::Rust),
        )
        .await;
    assert_matches!(
        files,
        Ok(files)
            if files.len() == 1
                && files[0].path().as_str() == "src/json_decoder.rs"
    );
}

async fn assert_current_symbol_retrieval(
    database: &CartographDatabase,
    project: &ProjectId,
    generation_id: &GenerationId,
    path: &NormalizedPath,
) {
    let symbols_at_range = database
        .current_symbols_at_range(CurrentSourceRangeLookup::new(
            CurrentGenerationLookup::new(project, generation_id),
            SourceLineRange::new(path, 2, 2),
            SEARCH_LIMIT,
        ))
        .await;
    assert_matches!(
        symbols_at_range,
        Ok(symbols)
            if symbols.len() == 1
                && symbols[0].symbol_id().as_str() == RETRIEVAL_CALLER
                && symbols[0].visibility() == Some(Visibility::Private)
                && symbols[0].async_symbol()
                && !symbols[0].exported()
    );

    let named = database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(project, generation_id),
            "decodeJSONPayload",
            SEARCH_LIMIT,
        ))
        .await;
    assert_matches!(
        named,
        Ok(symbols)
            if symbols.len() == 1
                && symbols[0].symbol_id().as_str() == RETRIEVAL_TARGET
                && symbols[0].visibility() == Some(Visibility::Public)
                && symbols[0].exported()
                && !symbols[0].default_export()
                && !symbols[0].static_member()
                && !symbols[0].declaration_only()
    );

    let references = database
        .exact_current_references_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(project, generation_id),
            "decodeJSONPayload",
            SEARCH_LIMIT,
        ))
        .await;
    assert_matches!(
        references,
        Ok(references)
            if references.len() == 1
                && references[0].target_symbol_id().map(SymbolId::as_str)
                    == Some(RETRIEVAL_TARGET)
    );

    let target_id = parse_symbol_id(RETRIEVAL_TARGET);
    let caller_id = parse_symbol_id(RETRIEVAL_CALLER);
    let incoming = database
        .current_graph_edges(
            CurrentGraphLookup::new(
                CurrentGenerationLookup::new(project, generation_id),
                std::slice::from_ref(&target_id),
                GraphDirection::Incoming,
            )
            .with_limit(SEARCH_LIMIT),
        )
        .await;
    assert_matches!(
        incoming,
        Ok(edges)
            if edges.len() == 1
                && edges[0].source_symbol_id() == &caller_id
                && edges[0].target_symbol_id() == &target_id
    );
    let hydrated = database
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            project,
            generation_id,
            &[caller_id, target_id],
        ))
        .await;
    assert_matches!(hydrated, Ok(symbols) if symbols.len() == 2);
}

async fn assert_interactive_reads_timeout_and_pool_recovers(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation_id: &GenerationId,
) {
    let mut blocker = match pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("could not begin interactive-read blocker: {error}"),
    };
    let lock = format!(r#"LOCK TABLE "{schema}"."projects" IN ACCESS EXCLUSIVE MODE"#);
    if let Err(error) = query(AssertSqlSafe(lock)).execute(&mut *blocker).await {
        panic!("could not lock interactive-read fixture: {error}");
    }

    let generation = database
        .current_generation_record_bounded(project, INTERACTIVE_STALL_TIMEOUT)
        .await;
    assert_matches!(
        generation,
        Err(StorageError::DatabaseOperation {
            operation: "current-generation-read"
        })
    );
    let search = database
        .search_current_code_bounded(
            SearchQuery::new(
                CurrentGenerationLookup::new(project, generation_id),
                "json payload",
                SEARCH_LIMIT,
            ),
            INTERACTIVE_STALL_TIMEOUT,
        )
        .await;
    assert_matches!(
        search,
        Err(StorageError::DatabaseOperation {
            operation: "expected-generation-read"
        })
    );
    if let Err(error) = blocker.rollback().await {
        panic!("could not release interactive-read blocker: {error}");
    }

    assert_matches!(
        database.current_generation_record(project).await,
        Ok(Some(_))
    );
    assert_matches!(
        database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(project, generation_id),
                "json payload",
                SEARCH_LIMIT,
            ))
            .await,
        Ok(hits) if hits.len() == 1
    );
}

async fn reject_stale_publication(database: &CartographDatabase, ready: ReadyGeneration) {
    let ready = match publish_fenced(database, ready).await {
        Err(error) => {
            assert_matches!(error.error(), StorageError::StaleGeneration { .. });
            error.into_parts().0
        }
        Ok(_) => panic!("older ready generation replaced a newer current generation"),
    };
    assert!(
        fail_fenced(database, RecoverableGeneration::Ready(ready))
            .await
            .is_ok()
    );
}

async fn assert_restart_recovery(database: &CartographDatabase, project: &ProjectId) {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_FOUR,
            workers: RECOVERY_WORKERS,
        },
    )
    .await;
    let generation_id = staged.generation_id().clone();
    drop(staged);
    let recovered = match database
        .recover_generation(GenerationRecoveryRequest::new(project, &generation_id))
        .await
    {
        Ok(Some(RecoverableGeneration::Staged(generation))) => generation,
        Ok(_) => panic!("staging generation was not recoverable after token loss"),
        Err(error) => panic!("generation recovery failed: {error}"),
    };
    assert!(
        fail_fenced(database, RecoverableGeneration::Staged(recovered))
            .await
            .is_ok()
    );
    assert_state(
        database,
        StateExpectation::new(project, &generation_id, GenerationState::Failed),
    )
    .await;
}

async fn assert_validation_token_return(database: &CartographDatabase, project: &ProjectId) {
    let staged = begin(
        database,
        GenerationFixture {
            project,
            revision: REVISION_FIVE,
            workers: RECOVERY_WORKERS,
        },
    )
    .await;
    let mut invalid = document(DocumentFixture {
        id: DOCUMENT_FOUR,
        path: "src/empty.rs",
        qualified_name: "",
        code: "",
    });
    invalid.natural_text.clear();
    let invalid = validate_for_test(GenerationFacts {
        documents: vec![invalid],
        ..GenerationFacts::default()
    });
    assert_matches!(
        invalid.err(),
        Some(GenerationValidationError::Storage(
            StorageError::InvalidInput {
                field: "searchable_text"
            }
        ))
    );
    assert!(
        fail_fenced(database, RecoverableGeneration::Staged(staged))
            .await
            .is_ok()
    );
}

async fn assert_state(database: &CartographDatabase, expected: StateExpectation<'_>) {
    let actual = database
        .generation_state(expected.project, expected.generation)
        .await;
    assert_eq!(actual.ok().flatten(), Some(expected.state));
}

async fn assert_search(database: &CartographDatabase, expected: SearchExpectation<'_>) {
    let current = database
        .current_generation_record(expected.project)
        .await
        .unwrap_or_else(|error| panic!("current generation lookup failed: {error}"))
        .unwrap_or_else(|| panic!("search fixture has no current generation"));
    let hits = database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(expected.project, current.generation_id()),
            expected.query,
            SEARCH_LIMIT,
        ))
        .await;
    let hits = match hits {
        Ok(hits) => hits,
        Err(error) => panic!("BM25 search failed: {error}"),
    };
    match (expected.document_id, expected.generation_id) {
        (None, None) => assert_eq!(hits, []),
        (Some(document_id), Some(generation_id)) => {
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].document_id().as_str(), document_id);
            assert_eq!(hits[0].generation_id(), generation_id);
            assert!(hits[0].score().is_finite() && hits[0].score().is_sign_positive());
            assert_ne!(hits[0].components(), []);
        }
        _ => panic!("search expectation must contain both document and generation IDs"),
    }
}

async fn begin(
    database: &CartographDatabase,
    fixture: GenerationFixture<'_>,
) -> cartograph_db::StagedGeneration {
    match database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            fixture.revision,
            fixture.workers,
        ))
        .await
    {
        Ok(generation) => generation,
        Err(error) => panic!("generation did not begin: {error}"),
    }
}

async fn prepare(database: &CartographDatabase, fixture: PrepareFixture) -> ReadyGeneration {
    match prepare_fenced(
        database,
        fixture.staged,
        GenerationFacts {
            documents: vec![fixture.document],
            ..GenerationFacts::default()
        },
    )
    .await
    {
        Ok(generation) => generation,
        Err(error) => panic!("generation did not become ready: {error}"),
    }
}

async fn prepare_fenced(
    database: &CartographDatabase,
    staged: cartograph_db::StagedGeneration,
    facts: GenerationFacts,
) -> Result<ReadyGeneration, PrepareGenerationError> {
    let lease =
        acquire_generation_lease(database, staged.project_id(), staged.generation_id()).await;
    let fence = lease.fence();
    let result = database
        .prepare_generation(GenerationContents::new(staged, canonical(facts)), &fence)
        .await;
    assert!(database.release_lease(&lease).await.is_ok());
    result
}

fn canonical(facts: GenerationFacts) -> CanonicalGenerationFacts {
    validate_for_test(facts)
        .unwrap_or_else(|error| panic!("generation fixture was invalid: {error}"))
}

fn validate_for_test(
    facts: GenerationFacts,
) -> Result<CanonicalGenerationFacts, GenerationValidationError> {
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )?;
    validate_generation_facts(facts, limits, || false).map(|(facts, _)| facts)
}

async fn publish_fenced(
    database: &CartographDatabase,
    ready: ReadyGeneration,
) -> Result<CurrentGeneration, PublishGenerationError> {
    let lease = acquire_generation_lease(database, ready.project_id(), ready.generation_id()).await;
    let fence = lease.fence();
    let result = database.publish_generation(ready, &fence).await;
    if result.is_err() {
        assert!(database.release_lease(&lease).await.is_ok());
    }
    result
}

async fn fail_fenced(
    database: &CartographDatabase,
    generation: RecoverableGeneration,
) -> Result<FailedGeneration, FailGenerationError> {
    let (project_id, generation_id) = match &generation {
        RecoverableGeneration::Staged(generation) => {
            (generation.project_id(), generation.generation_id())
        }
        RecoverableGeneration::Ready(generation) => {
            (generation.project_id(), generation.generation_id())
        }
    };
    let lease = acquire_generation_lease(database, project_id, generation_id).await;
    let result = database.fail_generation(generation, &lease.fence()).await;
    assert!(database.release_lease(&lease).await.is_ok());
    result
}

async fn acquire_generation_lease(
    database: &CartographDatabase,
    project_id: &ProjectId,
    generation_id: &GenerationId,
) -> ProjectLease {
    let target = LeaseTarget::new(
        project_id.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    match database
        .acquire_lease(LeaseRequest::new(
            target,
            LeaseOwner::new(process::id(), format!("generation-test-{generation_id}")),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("generation test lease acquisition failed: {error}"),
    }
}

async fn expire_generation_lease(pool: &sqlx_postgres::PgPool, schema: &str, target: &LeaseTarget) {
    let statement = format!(
        r#"UPDATE "{schema}"."project_operation_leases"
            SET acquired_at = clock_timestamp() - interval '3 seconds',
                heartbeat_at = clock_timestamp() - interval '2 seconds',
                expires_at = clock_timestamp() - interval '1 second'
            WHERE project_id = CAST($1 AS uuid) AND operation = $2"#
    );
    if let Err(error) = query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .execute(pool)
        .await
    {
        panic!("could not expire generation fence fixture: {error}");
    }
}

async fn assert_ledger_tampering_is_refused(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) {
    tamper_migration_checksum(pool, schema).await;
    assert_matches!(
        database.migrate().await,
        Err(MigrationError::LedgerConflict {
            version: INITIAL_MIGRATION_VERSION
        })
    );
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

impl AsRef<str> for GuardedSchema {
    fn as_ref(&self) -> &str {
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
        "cartograph_it_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let settings = DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
        .and_then(|settings| settings.with_schema(&schema));
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => panic!("test database settings failed validation: {error}"),
    };
    let pool = match cartograph_db::connect(&settings).await {
        Ok(pool) => pool,
        Err(error) => panic!("test database connection failed: {error}"),
    };
    let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
    let cleanup = TestSchemaGuard::new(database_url, schema.clone())
        .unwrap_or_else(|error| panic!("generation schema guard failed: {error}"));
    (
        database,
        pool,
        GuardedSchema {
            name: schema,
            _cleanup: cleanup,
        },
    )
}

async fn assert_exact_lookup_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 10"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify exact-lookup migration: {error}"));
    assert_eq!(checksum, EXACT_LOOKUP_MIGRATION_CHECKSUM);

    let indexes = query(
        r"SELECT indexname
            FROM pg_indexes
            WHERE schemaname = $1
              AND indexname = ANY($2::text[])
            ORDER BY indexname",
    )
    .bind(schema)
    .bind(vec![
        "references_exact_name_site_idx",
        "symbols_simple_name_idx",
    ])
    .fetch_all(pool)
    .await
    .unwrap_or_else(|error| panic!("could not inspect exact-lookup indexes: {error}"));
    assert_eq!(indexes.len(), 2);

    let latest_ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 25"#
    );
    let latest_checksum = query(AssertSqlSafe(latest_ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify directory-import migration: {error}"));
    assert_eq!(
        latest_checksum,
        DIRECTORY_IMPORT_SIMPLE_NAME_MIGRATION_CHECKSUM
    );

    let expression = query(
        r"SELECT pg_get_expr(attributes.adbin, attributes.adrelid) AS expression
            FROM pg_catalog.pg_attribute AS columns
            JOIN pg_catalog.pg_class AS relations
              ON relations.oid = columns.attrelid
            JOIN pg_catalog.pg_namespace AS namespaces
              ON namespaces.oid = relations.relnamespace
            JOIN pg_catalog.pg_attrdef AS attributes
              ON attributes.adrelid = columns.attrelid
             AND attributes.adnum = columns.attnum
            WHERE namespaces.nspname = $1
              AND relations.relname = 'symbols'
              AND columns.attname = 'simple_name'",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<String, _>("expression"))
    .unwrap_or_else(|error| panic!("could not inspect simple-name expression: {error}"));
    assert!(expression.contains("COALESCE"), "{expression}");
    assert!(expression.contains("NULLIF"), "{expression}");
}

async fn assert_generation_search_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 11"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify generation-search migration: {error}"));
    assert_eq!(checksum, GENERATION_SEARCH_RELATIONS_MIGRATION_CHECKSUM);
    let catalog = query(
        r"SELECT to_regclass($1) IS NOT NULL AS catalog_present,
                  to_regclass($2) IS NULL AS global_bm25_absent",
    )
    .bind(format!("{schema}.generation_search_relations"))
    .bind(format!("{schema}.search_documents_bm25_idx"))
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("could not inspect generation-search catalog: {error}"));
    assert!(
        catalog
            .try_get::<bool, _>("catalog_present")
            .unwrap_or(false)
    );
    assert!(
        catalog
            .try_get::<bool, _>("global_bm25_absent")
            .unwrap_or(false)
    );
    let global_identity = query(
        r"SELECT indexes.indisunique
            FROM pg_catalog.pg_namespace AS namespaces
            INNER JOIN pg_catalog.pg_class AS relations
              ON relations.relnamespace = namespaces.oid
             AND relations.relname = 'index_generations_global_identity_idx'
            INNER JOIN pg_catalog.pg_index AS indexes
              ON indexes.indexrelid = relations.oid
            WHERE namespaces.nspname = $1",
    )
    .bind(schema)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|error| panic!("could not inspect global generation identity: {error}"));
    assert!(global_identity.is_some_and(|row| row.try_get::<bool, _>(0).unwrap_or(false)));
}

async fn assert_typed_symbol_semantics_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 12"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify typed-symbol migration: {error}"));
    assert_eq!(checksum, TYPED_SYMBOL_SEMANTICS_MIGRATION_CHECKSUM);

    let typed_columns = query(
        r"SELECT count(*) AS column_count
            FROM information_schema.columns
            WHERE table_schema = $1
              AND table_name = 'symbols'
              AND column_name = ANY($2::text[])",
    )
    .bind(schema)
    .bind(vec![
        "visibility",
        "exported",
        "default_export",
        "async_symbol",
        "static_member",
        "declaration_only",
    ])
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>("column_count"))
    .unwrap_or_else(|error| panic!("could not inspect typed-symbol columns: {error}"));
    assert_eq!(typed_columns, 6);
}

async fn assert_agent_evidence_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 13"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify agent-evidence migration: {error}"));
    assert_eq!(checksum, AGENT_EVIDENCE_MIGRATION_CHECKSUM);

    let catalogs = query(
        r"SELECT COUNT(*) AS table_count
            FROM information_schema.tables
            WHERE table_schema = $1
              AND table_name = ANY($2::text[])",
    )
    .bind(schema)
    .bind(vec![
        "coverage_sources",
        "symbol_coverage",
        "file_history",
        "file_cochanges",
        "agent_artifacts",
    ])
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>("table_count"))
    .unwrap_or_else(|error| panic!("could not inspect agent-evidence tables: {error}"));
    assert_eq!(catalogs, 5);

    let bm25 = query(r"SELECT to_regclass($1) IS NOT NULL AS present")
        .bind(format!("{schema}.agent_artifacts_bm25_idx"))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<bool, _>("present"))
        .unwrap_or_else(|error| panic!("could not inspect artifact BM25 index: {error}"));
    assert!(bm25);
}

async fn assert_agent_session_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 14"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify agent-session migration: {error}"));
    assert_eq!(checksum, AGENT_SESSION_MIGRATION_CHECKSUM);

    let catalogs = query(
        r"SELECT COUNT(*) AS table_count
            FROM information_schema.tables
            WHERE table_schema = $1
              AND table_name = ANY($2::text[])",
    )
    .bind(schema)
    .bind(vec!["mcp_sessions", "mcp_tool_calls", "mcp_macros"])
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<i64, _>("table_count"))
    .unwrap_or_else(|error| panic!("could not inspect agent-session tables: {error}"));
    assert_eq!(catalogs, 3);
}

async fn assert_deterministic_cochange_order_migration(pool: &sqlx_postgres::PgPool, schema: &str) {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = 21"#
    );
    let checksum = query(AssertSqlSafe(ledger))
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify cochange-order migration: {error}"));
    assert_eq!(checksum, DETERMINISTIC_COCHANGE_ORDER_MIGRATION_CHECKSUM);

    assert_eq!(
        schema_migration_checksum(pool, schema, 43).await,
        "d16123b4cba07e6249e72552c3853cdda61ff1993fbb08d7f767c3cedf6bf956"
    );

    assert_eq!(
        schema_migration_checksum(pool, schema, 44).await,
        "01c25d7fe0efa96c8dc80680b6584609a7e283cf81dd755f0a19a95798f9536d"
    );

    assert_eq!(
        schema_migration_checksum(pool, schema, 45).await,
        "99746f46572ebbe0238a725e1156e0410c4f5016bb2708a6ab1ff963bf70bdfd"
    );

    assert_eq!(
        schema_migration_checksum(pool, schema, 46).await,
        "66741be677ed1d875aa33683023a840190a842e99fd1b57ad1a567f863f5d4b7"
    );

    let definition = query(
        r"SELECT pg_get_constraintdef(constraints.oid) AS definition
            FROM pg_catalog.pg_constraint AS constraints
            JOIN pg_catalog.pg_class AS relations
              ON relations.oid = constraints.conrelid
            JOIN pg_catalog.pg_namespace AS namespaces
              ON namespaces.oid = relations.relnamespace
            WHERE namespaces.nspname = $1
              AND relations.relname = 'file_cochanges'
              AND constraints.conname = 'file_cochanges_check'",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<String, _>("definition"))
    .unwrap_or_else(|error| panic!("could not inspect cochange-order constraint: {error}"));
    assert!(definition.contains("COLLATE \"C\""), "{definition}");
}

async fn assert_native_index_digest_migrations(pool: &sqlx_postgres::PgPool, schema: &str) {
    assert_eq!(
        schema_migration_checksum(pool, schema, 22).await,
        NATIVE_INDEX_DIGEST_V5_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 24).await,
        RUST_WORKSPACE_RESOLUTION_DIGEST_V6_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 26).await,
        NUMERICAL_EVIDENCE_DIGEST_V7_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 27).await,
        STRUCTURAL_DIAGNOSTICS_DIGEST_V8_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 28).await,
        BIOMARKER_PRECISION_DIGEST_V9_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 29).await,
        DETECTOR_PRECISION_DIGEST_V10_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 30).await,
        RUST_CLOSURE_CALL_TARGET_DIGEST_V11_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 31).await,
        GO_CALL_TARGET_DIGEST_V12_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 36).await,
        JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 38).await,
        SHADER_AND_DISPATCH_DIGEST_V14_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 39).await,
        GENERATION_SOURCE_ADMISSION_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 40).await,
        ADA_VHDL_NUMERICAL_DIGEST_V15_MIGRATION_CHECKSUM
    );
    assert_eq!(
        schema_migration_checksum(pool, schema, 41).await,
        RUST_SELF_RECEIVER_DIGEST_V16_MIGRATION_CHECKSUM
    );

    let definition = query(
        r"SELECT pg_get_constraintdef(constraints.oid) AS definition
            FROM pg_catalog.pg_constraint AS constraints
            JOIN pg_catalog.pg_class AS relations
              ON relations.oid = constraints.conrelid
            JOIN pg_catalog.pg_namespace AS namespaces
              ON namespaces.oid = relations.relnamespace
            WHERE namespaces.nspname = $1
              AND relations.relname = 'index_generations'
              AND constraints.conname = 'index_generations_digest_version_check'",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .and_then(|row| row.try_get::<String, _>("definition"))
    .unwrap_or_else(|error| panic!("could not inspect digest-v19 constraint: {error}"));
    assert!(
        definition
            .contains("ARRAY[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19]"),
        "{definition}"
    );
}

async fn schema_migration_checksum(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    version: i64,
) -> String {
    let ledger = format!(
        r#"SELECT checksum
            FROM "{schema}"."schema_migrations"
            WHERE version = $1"#
    );
    query(AssertSqlSafe(ledger))
        .bind(version)
        .fetch_one(pool)
        .await
        .and_then(|row| row.try_get::<String, _>("checksum"))
        .unwrap_or_else(|error| panic!("could not verify migration {version}: {error}"))
}

async fn seed_exact_lookup_scale(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
    rows: i32,
) {
    let file = parse_file_id(RETRIEVAL_FILE);
    let file_insert = format!(
        r#"INSERT INTO "{schema}"."files" (
                project_id, generation_id, file_id, normalized_path, language,
                content_hash, byte_size, parse_status
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid),
                'scale/exact.rs', 'rust', $4, 1000000, 'parsed'
            )"#
    );
    query(AssertSqlSafe(file_insert))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(file.as_str())
        .bind(DIGEST_ONE)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed exact-lookup file: {error}"));

    let symbol_insert = format!(
        r#"INSERT INTO "{schema}"."symbols" (
                project_id, generation_id, symbol_id, file_id, symbol_kind,
                qualified_name, signature, start_byte, end_byte,
                start_line, end_line, structural_digest
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid),
                   md5('symbol-' || series::text)::uuid, CAST($3 AS uuid), 'function',
                   CASE WHEN series = 1 THEN 'Scale.tagscanary'
                        ELSE 'Scale.symbol_' || series::text END,
                   '', series::bigint * 2, series::bigint * 2 + 1,
                   series, series, $4
            FROM generate_series(1, $5) AS series"#
    );
    query(AssertSqlSafe(symbol_insert))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(file.as_str())
        .bind(DIGEST_ONE)
        .bind(rows)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed exact-lookup symbols: {error}"));

    let reference_insert = format!(
        r#"INSERT INTO "{schema}"."references" (
                project_id, generation_id, file_id, target_symbol_id,
                reference_kind, start_byte, end_byte, confidence,
                owner_symbol_id, reference_name, resolution_provenance,
                site_count, span_precision
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid), CAST($3 AS uuid), NULL,
                   'calls', series::bigint * 2, series::bigint * 2 + 1, 0.0,
                   NULL,
                   CASE WHEN series = 1 THEN 'tagscanary'
                        ELSE 'other_' || series::text END,
                   'scale-unresolved', 1, 'exact'
            FROM generate_series(1, $4) AS series"#
    );
    query(AssertSqlSafe(reference_insert))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(file.as_str())
        .bind(rows)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed exact-lookup references: {error}"));

    for table in ["symbols", "references"] {
        let analyze = format!(r#"ANALYZE "{schema}"."{table}""#);
        query(AssertSqlSafe(analyze))
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("could not analyze exact-lookup {table}: {error}"));
    }
}

async fn assert_exact_lookup_plans_use_indexes(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
    generation: &GenerationId,
) {
    let symbol_plan = format!(
        r#"EXPLAIN (FORMAT TEXT, COSTS OFF)
            SELECT symbol_id
            FROM "{schema}"."symbols"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND simple_name = $3
            ORDER BY file_id, start_line, symbol_id
            LIMIT 10"#
    );
    let symbols = explain_lookup(
        pool,
        symbol_plan,
        project,
        generation,
        "symbols_simple_name_idx",
        "symbols",
    )
    .await;
    assert!(symbols.contains("Index"));

    let reference_plan = format!(
        r#"EXPLAIN (FORMAT TEXT, COSTS OFF)
            SELECT reference_id
            FROM "{schema}"."references"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND reference_name = $3
            ORDER BY file_id, start_byte, reference_id
            LIMIT 10"#
    );
    let references = explain_lookup(
        pool,
        reference_plan,
        project,
        generation,
        "references_exact_name_site_idx",
        "references",
    )
    .await;
    assert!(references.contains("Index"));
}

async fn explain_lookup(
    pool: &sqlx_postgres::PgPool,
    statement: String,
    project: &ProjectId,
    generation: &GenerationId,
    expected_index: &str,
    table: &str,
) -> String {
    let rows = query(AssertSqlSafe(statement))
        .bind(project.as_str())
        .bind(generation.as_str())
        .bind(EXACT_LOOKUP_TARGET_NAME)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|error| panic!("could not explain {table} exact lookup: {error}"));
    let plan = rows
        .iter()
        .map(|row| {
            row.try_get::<String, _>(0)
                .unwrap_or_else(|error| panic!("could not decode {table} plan: {error}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains(expected_index), "{table} plan: {plan}");
    assert!(!plan.contains(&format!("Seq Scan on {table}")), "{plan}");
    plan
}

fn document(fixture: DocumentFixture<'_>) -> SearchDocumentInput {
    SearchDocumentInput {
        document_id: parse_document_id(fixture.id),
        file_id: None,
        symbol_id: None,
        path: fixture.path.to_owned(),
        language: "rust".to_owned(),
        kind: DocumentKind::Symbol,
        qualified_name: fixture.qualified_name.to_owned(),
        code: fixture.code.to_owned(),
        natural_text: "Parser implementation evidence".to_owned(),
        metadata: serde_json::json!({"role": "parser"}),
    }
}

fn parse_document_id(raw: &str) -> DocumentId {
    match DocumentId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("test document UUID is invalid: {error}"),
    }
}

fn parse_generation_id(raw: &str) -> GenerationId {
    GenerationId::parse(raw)
        .unwrap_or_else(|error| panic!("generation ID fixture is invalid: {error}"))
}

fn parse_file_id(raw: &str) -> FileId {
    match FileId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("test file UUID is invalid: {error}"),
    }
}

fn parse_symbol_id(raw: &str) -> SymbolId {
    match SymbolId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("test symbol UUID is invalid: {error}"),
    }
}

fn digest(raw: &str) -> ContentDigest {
    match ContentDigest::parse(raw) {
        Ok(digest) => digest,
        Err(error) => panic!("test digest is invalid: {error}"),
    }
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    // The schema is generated exclusively from the decimal process id and an
    // atomic counter above; it contains no caller-controlled SQL fragment.
    if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
        panic!("failed to drop isolated test schema: {error}");
    }
}

async fn tamper_migration_checksum(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement =
        format!("UPDATE \"{schema}\".\"schema_migrations\" SET checksum = $1 WHERE version = 1");
    let result = query(AssertSqlSafe(statement))
        .bind("0".repeat(64))
        .execute(pool)
        .await;
    if let Err(error) = result {
        panic!("failed to prepare ledger-integrity test: {error}");
    }
}

/// Documents in the paging fixture: two full embedding carry pages of 4,096
/// documents plus a partial third page.
const CARRY_DOCUMENTS: usize = 2 * 4_096 + 7;
/// Re-indexed documents whose source changes: the last document of the first
/// carry page, the first of the second, and one in the partial final page.
const CARRY_CHANGED: [usize; 3] = [4_095, 4_096, 2 * 4_096 + 3];
const CARRY_SOURCE_DIGEST: &str =
    "8888888888888888888888888888888888888888888888888888888888888888";
const CARRY_MODEL_FINGERPRINT: &str =
    "9999999999999999999999999999999999999999999999999999999999999999";

/// A re-index that carries embeddings over several keyset pages keeps every
/// unchanged document's vector and drops exactly the changed documents, even
/// where they straddle a page boundary.
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn embedding_carry_spans_pages_and_skips_changed_documents() {
    let (database, pool, schema) = open_isolated_database().await;
    assert_migration_ledger(&database).await;
    let project = register_project(&database).await;
    let initial = publish_carry_generation(
        &database,
        CarryFixture {
            project: &project,
            revision: REVISION_ONE,
            changed: &[],
        },
    )
    .await;
    let initial_rows = GenerationRows {
        pool: &pool,
        schema: &schema,
        project: &project,
        generation: initial.generation_id(),
    };
    seed_carry_embeddings(&initial_rows).await;
    assert_eq!(
        embedded_documents(&initial_rows).await.len(),
        CARRY_DOCUMENTS
    );

    let reindexed = publish_carry_generation(
        &database,
        CarryFixture {
            project: &project,
            revision: REVISION_TWO,
            changed: &CARRY_CHANGED,
        },
    )
    .await;
    let carried = embedded_documents(&GenerationRows {
        pool: &pool,
        schema: &schema,
        project: &project,
        generation: reindexed.generation_id(),
    })
    .await;
    let expected = (0..CARRY_DOCUMENTS)
        .filter(|index| !CARRY_CHANGED.contains(index))
        .map(carry_document_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(carried, expected);

    drop(database);
    drop_schema(&pool, &schema).await;
    pool.close().await;
}

/// One paging-fixture generation: which documents differ from the base.
struct CarryFixture<'a> {
    project: &'a ProjectId,
    revision: &'a str,
    changed: &'a [usize],
}

/// Rows of one published generation in an isolated test schema.
struct GenerationRows<'a> {
    pool: &'a sqlx_postgres::PgPool,
    schema: &'a str,
    project: &'a ProjectId,
    generation: &'a GenerationId,
}

/// Document IDs sort in fixture order, so carry pages follow the index.
fn carry_document_id(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012x}")
}

async fn publish_carry_generation(
    database: &CartographDatabase,
    fixture: CarryFixture<'_>,
) -> CurrentGeneration {
    let staged = begin(
        database,
        GenerationFixture {
            project: fixture.project,
            revision: fixture.revision,
            workers: INITIAL_WORKERS,
        },
    )
    .await;
    let documents = (0..CARRY_DOCUMENTS)
        .map(|index| {
            let id = carry_document_id(index);
            let name = format!("carry::item_{index}");
            let code = if fixture.changed.contains(&index) {
                format!("fn item_{index}() {{ changed(); }}")
            } else {
                format!("fn item_{index}() {{}}")
            };
            document(DocumentFixture {
                id: &id,
                path: "src/carry.rs",
                qualified_name: &name,
                code: &code,
            })
        })
        .collect();
    let facts = GenerationFacts {
        documents,
        ..GenerationFacts::default()
    };
    let ready = prepare_fenced(database, staged, facts)
        .await
        .unwrap_or_else(|error| panic!("paging fixture generation failed: {error}"));
    publish_fenced(database, ready)
        .await
        .unwrap_or_else(|error| panic!("paging fixture generation did not publish: {error}"))
}

/// Give every document of the generation a vector under an active model.
async fn seed_carry_embeddings(rows: &GenerationRows<'_>) {
    let schema = rows.schema;
    let model = format!(
        r#"INSERT INTO "{schema}"."embedding_models" (
                model_id, fingerprint, provider, model_name, dimension, normalization
            ) VALUES (
                CAST($1 AS uuid), $2, 'integration-test', 'tiny-vector', 3, 'none'
            )"#,
    );
    query(AssertSqlSafe(model))
        .bind(EVIDENCE_MODEL)
        .bind(CARRY_MODEL_FINGERPRINT)
        .execute(rows.pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed carry model: {error}"));
    let embeddings = format!(
        r#"INSERT INTO "{schema}"."document_embeddings" (
                project_id, generation_id, document_id, model_id, source_digest, embedding
            )
            SELECT project_id, generation_id, document_id, CAST($3 AS uuid), $4,
                   CAST('[1,2,3]' AS vector)
            FROM "{schema}"."search_documents"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#,
    );
    query(AssertSqlSafe(embeddings))
        .bind(rows.project.as_str())
        .bind(rows.generation.as_str())
        .bind(EVIDENCE_MODEL)
        .bind(CARRY_SOURCE_DIGEST)
        .execute(rows.pool)
        .await
        .unwrap_or_else(|error| panic!("could not seed carry embeddings: {error}"));
}

async fn embedded_documents(rows: &GenerationRows<'_>) -> BTreeSet<String> {
    let schema = rows.schema;
    let statement = format!(
        r#"SELECT document_id::text FROM "{schema}"."document_embeddings"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#,
    );
    query(AssertSqlSafe(statement))
        .bind(rows.project.as_str())
        .bind(rows.generation.as_str())
        .fetch_all(rows.pool)
        .await
        .unwrap_or_else(|error| panic!("could not read carried embeddings: {error}"))
        .iter()
        .map(|row| {
            row.try_get::<String, _>(0)
                .unwrap_or_else(|error| panic!("could not decode document ID: {error}"))
        })
        .collect()
}
