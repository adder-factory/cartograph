//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;

use std::assert_matches;
use std::{
    env, process,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CachedRowPayload, CanonicalGenerationFacts, CartographDatabase, EdgeInput, FactBatchInput,
    FileInput, GenerationContents, GenerationFacts, GenerationRecoveryRequest,
    GenerationRetentionPolicy, GenerationRetentionRequest, GenerationValidationLimits, LeaseOwner,
    LeaseRequest, LeaseTarget, NativeGenerationExtractedCursor, NativeGenerationSpill,
    NativeGenerationSpillCachedRow, NativeGenerationSpillCanonicalProgress,
    NativeGenerationSpillExtractedBatch, NativeGenerationSpillFactBatch,
    NativeGenerationSpillFactCounts, NativeGenerationSpillPolicy, NativeGenerationSpillRelation,
    NativeGenerationSpillRequest, NativeGenerationSpillRow, NativeGenerationSpillState,
    NativeGenerationSpillWrite, NativeParseCacheBatchWrite, NativeParseCacheEntry,
    NativeParseCacheKey, NativeParseCacheKeyInput, NewGeneration, NewProject, NumericalSiteInput,
    PrepareGenerationMetrics, ProjectLease, ReadyGeneration, RecoverableGeneration, ReferenceInput,
    SearchDocumentInput, SpilledGenerationContents, StagedGeneration, StorageError, SymbolInput,
    TerminalGenerationMutation, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus,
    GenerationDigestVersion, GenerationId, NormalizedPath, NumericalSiteId, ProjectId,
    ProjectOperation, SourceLanguage, SymbolId,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const PROJECT_FINGERPRINT: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REVISION: &str = "1111111111111111111111111111111111111111";
const FILE_ONE: &str = "11111111-1111-4111-8111-111111111111";
const FILE_TWO: &str = "22222222-2222-4222-8222-222222222222";
const FILE_REJECTED: &str = "33333333-3333-4333-8333-333333333333";
const SYMBOL_ONE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SYMBOL_TWO: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const NUMERICAL_SITE_ONE: &str = "99999999-9999-4999-8999-999999999999";
const DOCUMENT_ONE: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const DOCUMENT_TWO: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const DOCUMENT_REJECTED: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
const DOCUMENT_CHUNK_ONE: &str = "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const DOCUMENT_CHUNK_TWO: &str = "22222222-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const DOCUMENT_OVERSIZED_ROW: &str = "33333333-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CORRUPT_SOURCE_SYMBOL: &str = "44444444-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CORRUPT_TARGET_SYMBOL: &str = "55555555-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CONTENT_HASH_ONE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const CONTENT_HASH_TWO: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const CONTENT_HASH_REJECTED: &str =
    "3333333333333333333333333333333333333333333333333333333333333333";
const STRUCTURAL_HASH_ONE: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const STRUCTURAL_HASH_TWO: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
// V20 changes the digest domain; the fixture facts and independent row assertions are unchanged.
const EXPECTED_LOGICAL_DIGEST: &str =
    "d36c3e69fa377c205008c0a377c737b84ce7af6848e3c18175a5d23874a9e5d4";
const SINGLE_WORKER: u16 = 1;
const TEST_VALIDATION_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const TEST_VALIDATION_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const PARALLEL_WORKERS: u16 = 16;
const SYMBOL_TWO_START_BYTE: u64 = 8;
const SYMBOL_TWO_END_BYTE: u64 = 80;
const SYMBOL_TWO_START_LINE: u32 = 2;
const SYMBOL_TWO_END_LINE: u32 = 5;
const REFERENCE_START_BYTE: u64 = 32;
const REFERENCE_END_BYTE: u64 = 51;
const REFERENCE_CONFIDENCE: f32 = 0.875;
const CUMULATIVE_COPY_CODE_BYTES: usize = 600 * 1_024;
const INDIVIDUAL_COPY_CODE_BYTES: usize = 1_024 * 1_024 + 128;
const CHUNK_DOCUMENT_COUNT: i64 = 3;
const CUMULATIVE_COPY_ROW_COUNT: i64 = 2;
const INDIVIDUAL_COPY_ROW_COUNT: i64 = 1;
const TEST_LEASE_DURATION: Duration = Duration::from_secs(30);
const DENSE_SPILL_FILES: u64 = 20;
const DENSE_SPILL_SYMBOLS_PER_FILE: u64 = 200;
const DENSE_SPILL_SYMBOL_BYTES: u64 = 16;
const CANONICAL_TRANSACTION_BOUND: usize = 64;
const BLOCKED_STATEMENT_TIMEOUT: Duration = Duration::from_secs(1);

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn native_generation_spill_is_fenced_idempotent_and_partition_reduced() {
    let fixture = open_fixture().await;
    let mut run = begin_spill_test(&fixture).await;
    exercise_spill_parse_resume(&fixture, &mut run).await;
    let canonical_expected = exercise_spill_fact_resume(&fixture, &mut run).await;
    complete_spill_and_prepare(&fixture, run, &canonical_expected).await;
    assert_spill_partition_rejects_missing_edge_target(&fixture).await;
    assert_spill_partition_rejects_conflicting_document(&fixture).await;
    assert_spill_quota_and_fence(&fixture).await;
    assert_parse_cache_batch_round_trip(&fixture).await;
    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn native_generation_spill_rechecks_lease_immediately_before_commit() {
    let append_fixture = open_fixture().await;
    assert_expired_lease_rolls_back_spill_append(&append_fixture).await;
    drop(append_fixture.database);
    drop_schema(&append_fixture.pool, &append_fixture.schema).await;
    append_fixture.pool.close().await;

    let cursor_fixture = open_fixture().await;
    assert_expired_lease_rolls_back_canonical_cursor(&cursor_fixture).await;
    drop(cursor_fixture.database);
    drop_schema(&cursor_fixture.pool, &cursor_fixture.schema).await;
    cursor_fixture.pool.close().await;
}

/// A full rebuild reduces this generation's spilled rows and validates its
/// edges, references, numerical sites, and documents against the files and
/// symbols it loaded moments earlier. Every one of those partition queries must
/// be planned from statistics that include this generation. A never-analyzed
/// table, or one whose sample saw only earlier or failed generations, makes
/// PostgreSQL estimate the generation at about one row; the queries then probe
/// an index matching only the generation prefix and walk the whole generation
/// once per row. On a real TypeScript project that outlived the spill statement
/// timeout on the first edge partition of a fresh rebuild, and on the first
/// document partition of a retry next to a failed generation's spill.
/// Autovacuum is disabled here so nothing but the reduce itself can refresh the
/// statistics.
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn native_generation_spill_plans_partitions_from_this_generations_statistics() {
    let fixture = open_fixture().await;
    disable_planner_autovacuum(&fixture).await;
    let run = begin_spill_test(&fixture).await;
    stage_dense_symbol_spill(&run).await;
    let generation_id = run.staged.generation_id().clone();
    canonicalize_until_relation(&fixture, &run, "edges").await;
    let symbols = DENSE_SPILL_FILES * DENSE_SPILL_SYMBOLS_PER_FILE;
    assert_planned_rows(&fixture, &generation_id, ("files", DENSE_SPILL_FILES)).await;
    assert_planned_rows(&fixture, &generation_id, ("symbols", symbols)).await;
    let first_edges = run
        .spill
        .canonicalize_next()
        .await
        .unwrap_or_else(|error| panic!("first dense edge partition failed: {error}"));
    assert_eq!(
        (first_edges.relation, first_edges.partition),
        (NativeGenerationSpillRelation::Edges, 0)
    );
    let edges = symbols - DENSE_SPILL_FILES;
    assert_planned_rows(
        &fixture,
        &generation_id,
        ("native_generation_spill_edges", edges),
    )
    .await;
    let mut complete = false;
    for _ in 0..CANONICAL_TRANSACTION_BOUND {
        let progress = run
            .spill
            .canonicalize_next()
            .await
            .unwrap_or_else(|error| panic!("dense spill did not canonicalize: {error}"));
        if progress.complete {
            complete = true;
            break;
        }
    }
    assert!(complete, "dense spill never finished canonicalization");
    fail_spill_test_run(&fixture, run).await;
    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

/// A failed reduce must clean up its own staging generation however large its
/// spill is. Deleting the spill run cascades through every spilled row, which
/// for a full rebuild cannot finish inside the short failure-cleanup deadline,
/// so the cleanup failed too and left the generation staging and its lease
/// held. Another session's lock on the spilled rows stands in for that size:
/// any delete of them waits past the deadline. The generation must still fail
/// and release its lease, and the rows stay with it until retention drains
/// them in bounded batches. A spill that does fit the deadline is reclaimed at
/// once, which the supervisor's SCIP spill fault test covers.
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn failed_generation_cleanup_defers_an_oversized_spill_to_retention() {
    let fixture = open_fixture().await;
    let run = begin_spill_test(&fixture).await;
    run.spill
        .append_extracted_batch(extracted_spill_batch())
        .await
        .unwrap_or_else(|error| panic!("cleanup fixture parse batch failed: {error}"));
    let generation_id = run.staged.generation_id().clone();
    let mut blocker = fixture
        .pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("could not open the spill lock holder: {error}"));
    query(AssertSqlSafe(format!(
        r#"LOCK TABLE "{}".native_generation_spill_rows IN SHARE MODE"#,
        fixture.schema
    )))
    .execute(&mut *blocker)
    .await
    .unwrap_or_else(|error| panic!("could not lock the spilled rows: {error}"));
    let fence = run.lease.fence();
    let cleanup = fixture
        .database
        .fail_generation_and_release_bounded(TerminalGenerationMutation::new(
            &fence,
            BLOCKED_STATEMENT_TIMEOUT,
        ))
        .await;
    blocker
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("could not release the spill lock: {error}"));
    assert_eq!(cleanup, Ok(()));
    assert_eq!(generation_state(&fixture, &generation_id).await, "failed");
    assert!(
        !fixture
            .database
            .has_live_lease(&fixture.project, TEST_LEASE_DURATION)
            .await
            .unwrap_or_else(|error| panic!("lease state was unavailable: {error}"))
    );
    assert_eq!(spill_opaque_counts(&fixture, &generation_id).await, (1, 1));
    let migration = fixture
        .database
        .acquire_lease(LeaseRequest::new(
            LeaseTarget::new(fixture.project.clone(), ProjectOperation::Migration, None),
            LeaseOwner::new(process::id(), "ingest-test-retention".to_owned()),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("retention lease acquisition failed: {error}"));
    let retention = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            GenerationRetentionPolicy::new(0, 8)
                .unwrap_or_else(|error| panic!("retention policy failed: {error}")),
            &migration.fence(),
            TEST_LEASE_DURATION,
        ))
        .await
        .unwrap_or_else(|error| panic!("retention did not drain the failed spill: {error}"));
    assert_eq!(retention.failed_removed, 1);
    assert_eq!(spill_opaque_counts(&fixture, &generation_id).await, (0, 0));
    assert!(fixture.database.release_lease(&migration).await.is_ok());
    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

/// A canonical partition that outlives the spill statement timeout must say so,
/// because the reduce stage reports that as its deadline. Every failed
/// canonical insert used to be reported as a fact conflict, and the pipeline
/// then surfaced a reason-less `reduce_failed`. Another session's locks hold
/// the first partition past the deadline twice: a `VACUUM`-strength lock on the
/// raw spill files makes the planner-statistics refresh wait, and a share lock
/// on the canonical files makes the insert wait. Neither commits a partial
/// group, so the retry still starts at the first partition.
#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn spill_canonicalization_reports_statement_timeouts_as_timeouts() {
    let fixture = open_fixture().await;
    let run = begin_spill_test(&fixture).await;
    stage_dense_symbol_spill(&run).await;
    let bounded = NativeGenerationSpill::new(
        fixture.database.clone(),
        &run.staged,
        NativeGenerationSpillRequest {
            fence: run.lease.fence(),
            policy: run.policy,
            statement_timeout: BLOCKED_STATEMENT_TIMEOUT,
        },
    )
    .unwrap_or_else(|error| panic!("bounded spill authority was invalid: {error}"));
    for (lock, operation) in [
        (
            "native_generation_spill_files IN SHARE UPDATE EXCLUSIVE MODE",
            "spill-analyze-planner-statistics",
        ),
        ("files IN SHARE MODE", "spill-insert-canonical-partition"),
    ] {
        let timed_out = canonicalize_while_locked(&fixture, &bounded, lock).await;
        assert_eq!(timed_out, Err(StorageError::StatementTimeout { operation }));
    }
    let progress = bounded
        .canonicalize_next()
        .await
        .unwrap_or_else(|error| panic!("timed-out partition did not retry: {error}"));
    assert_eq!(progress.partition, 0);
    fail_spill_test_run(&fixture, run).await;
    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

/// Reduce one partition group while another session holds `lock`, given as a
/// schema-relative `LOCK TABLE` target and mode.
async fn canonicalize_while_locked(
    fixture: &DatabaseFixture,
    spill: &NativeGenerationSpill,
    lock: &str,
) -> Result<NativeGenerationSpillCanonicalProgress, StorageError> {
    let mut blocker = fixture
        .pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("could not open the lock holder: {error}"));
    query(AssertSqlSafe(format!(
        r#"LOCK TABLE "{}".{lock}"#,
        fixture.schema
    )))
    .execute(&mut *blocker)
    .await
    .unwrap_or_else(|error| panic!("could not take {lock}: {error}"));
    let result = spill.canonicalize_next().await;
    blocker
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("could not release {lock}: {error}"));
    result
}

/// Leave planner statistics for the tables the dense reduce reads and fills to
/// the code under test.
async fn disable_planner_autovacuum(fixture: &DatabaseFixture) {
    for table in ["files", "symbols", "native_generation_spill_edges"] {
        query(AssertSqlSafe(format!(
            r#"ALTER TABLE "{}"."{table}" SET (autovacuum_enabled = false)"#,
            fixture.schema
        )))
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not pin {table} statistics: {error}"));
    }
}

/// Assert the planner expects at least half of this generation's `rows` in `table`.
async fn assert_planned_rows(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
    (table, rows): (&str, u64),
) {
    let estimated = planner_generation_rows(fixture, table, generation_id).await;
    assert!(
        estimated.saturating_mul(2) >= rows,
        "{table} was planned for {estimated} of {rows} rows of this generation"
    );
}

/// Stage files whose symbols each call the next symbol in the same file.
async fn stage_dense_symbol_spill(run: &SpillTestRun) {
    run.spill
        .append_extracted_batch(extracted_spill_batch())
        .await
        .unwrap_or_else(|error| panic!("dense spill parse batch failed: {error}"));
    run.spill
        .finish_parsing(1)
        .await
        .unwrap_or_else(|error| panic!("dense spill parse phase did not seal: {error}"));
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("dense spill limits were invalid: {error}"));
    let batch = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 0,
            facts: dense_symbol_facts(),
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("dense spill facts were invalid: {error}"));
    let counts = batch.counts();
    run.spill
        .append_fact_batch(batch)
        .await
        .unwrap_or_else(|error| panic!("dense spill facts did not stage: {error}"));
    run.spill
        .seal_resolution(counts)
        .await
        .unwrap_or_else(|error| panic!("dense spill facts did not seal: {error}"));
}

fn dense_symbol_facts() -> GenerationFacts {
    let mut facts = GenerationFacts::default();
    for file in 0..DENSE_SPILL_FILES {
        let file_id = FileId::from_uuid_v8(fixture_uuid(&format!("dense-file-{file}")));
        facts.files.push(FileInput {
            file_id: file_id.clone(),
            normalized_path: format!("src/dense_{file}.rs"),
            language: "rust".to_owned(),
            content_hash: fixture_digest(&format!("dense-content-{file}")),
            byte_size: DENSE_SPILL_SYMBOLS_PER_FILE * DENSE_SPILL_SYMBOL_BYTES,
            parse_status: FileParseStatus::Parsed,
        });
        let mut caller: Option<SymbolId> = None;
        for index in 0..DENSE_SPILL_SYMBOLS_PER_FILE {
            let symbol = dense_symbol(&file_id, file, index);
            if let Some(source_symbol_id) = caller.replace(symbol.symbol_id.clone()) {
                facts.edges.push(EdgeInput {
                    source_symbol_id,
                    target_symbol_id: symbol.symbol_id.clone(),
                    kind: EdgeKind::Calls,
                    confidence: 1.0,
                    provenance: "tree-sitter".to_owned(),
                    site_count: 1,
                });
            }
            facts.symbols.push(symbol);
        }
    }
    facts
}

fn dense_symbol(file_id: &FileId, file: u64, index: u64) -> SymbolInput {
    let line = u32::try_from(index + 1)
        .unwrap_or_else(|error| panic!("dense symbol line overflowed: {error}"));
    SymbolInput {
        symbol_id: SymbolId::from_uuid_v8(fixture_uuid(&format!("dense-symbol-{file}-{index}"))),
        file_id: file_id.clone(),
        qualified_name: format!("dense_{file}::symbol_{index}"),
        start_byte: index * DENSE_SPILL_SYMBOL_BYTES,
        end_byte: (index + 1) * DENSE_SPILL_SYMBOL_BYTES,
        start_line: line,
        end_line: line,
        structural_digest: fixture_digest(&format!("dense-structure-{file}-{index}")),
        ..symbol_one()
    }
}

fn fixture_uuid(seed: &str) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&blake3::hash(seed.as_bytes()).as_bytes()[..16]);
    bytes
}

fn fixture_digest(seed: &str) -> ContentDigest {
    ContentDigest::from_bytes(*blake3::hash(seed.as_bytes()).as_bytes())
}

/// Reduce partition groups until the durable cursor reaches `relation`, so no
/// partition of that relation has been validated yet.
async fn canonicalize_until_relation(
    fixture: &DatabaseFixture,
    run: &SpillTestRun,
    relation: &str,
) {
    let generation_id = run.staged.generation_id();
    for _ in 0..CANONICAL_TRANSACTION_BOUND {
        let (_, current, ..) = spill_cursor_snapshot(fixture, generation_id).await;
        if current.as_deref() == Some(relation) {
            return;
        }
        run.spill
            .canonicalize_next()
            .await
            .unwrap_or_else(|error| panic!("dense spill partition failed: {error}"));
    }
    panic!("dense spill never reached the {relation} relation");
}

/// Rows the planner expects one generation of `table` to hold.
async fn planner_generation_rows(
    fixture: &DatabaseFixture,
    table: &str,
    generation_id: &GenerationId,
) -> u64 {
    let statement = format!(
        r#"EXPLAIN SELECT 1 FROM "{schema}"."{table}"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        schema = fixture.schema,
    );
    let plan = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not plan the {table} generation scan: {error}"))
        .try_get::<String, _>(0)
        .unwrap_or_else(|error| panic!("{table} plan was not text: {error}"));
    plan.split_once(" rows=")
        .and_then(|(_, rest)| rest.split_once(' '))
        .and_then(|(rows, _)| rows.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("{table} plan carried no row estimate: {plan}"))
}

async fn generation_state(fixture: &DatabaseFixture, generation_id: &GenerationId) -> String {
    query(AssertSqlSafe(format!(
        r#"SELECT state FROM "{}".index_generations
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#,
        fixture.schema
    )))
    .bind(fixture.project.as_str())
    .bind(generation_id.as_str())
    .fetch_one(&fixture.pool)
    .await
    .unwrap_or_else(|error| panic!("generation state was unavailable: {error}"))
    .try_get::<String, _>(0)
    .unwrap_or_else(|error| panic!("generation state was invalid: {error}"))
}

async fn assert_expired_lease_rolls_back_spill_append(fixture: &DatabaseFixture) {
    let run = begin_spill_test(fixture).await;
    install_spill_lease_expiry_trigger(fixture).await;
    assert_eq!(
        run.spill
            .append_extracted_batch(extracted_spill_batch())
            .await,
        Err(StorageError::LeaseFenceLost)
    );
    remove_spill_lease_expiry_trigger(fixture).await;
    let report = run
        .spill
        .report()
        .await
        .unwrap_or_else(|error| panic!("rolled-back spill report was unavailable: {error}"));
    assert_eq!(report.logical_bytes, 0);
    assert_eq!(report.raw_rows, 0);
    assert_eq!(report.extracted_files, 0);
    assert_eq!(
        spill_opaque_counts(fixture, run.staged.generation_id()).await,
        (0, 0)
    );
    fail_spill_test_run(fixture, run).await;
}

async fn assert_expired_lease_rolls_back_canonical_cursor(fixture: &DatabaseFixture) {
    let run = begin_spill_test(fixture).await;
    run.spill
        .append_extracted_batch(extracted_spill_batch())
        .await
        .unwrap_or_else(|error| panic!("cursor fixture parse batch failed: {error}"));
    run.spill
        .finish_parsing(1)
        .await
        .unwrap_or_else(|error| panic!("cursor fixture parse phase did not seal: {error}"));
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("cursor fixture limits were invalid: {error}"));
    let batch = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 0,
            facts: GenerationFacts {
                files: vec![file_one()],
                symbols: vec![symbol_one()],
                documents: vec![document_one(false)],
                ..GenerationFacts::default()
            },
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("cursor fixture fact batch was invalid: {error}"));
    let counts = batch.counts();
    run.spill
        .append_fact_batch(batch)
        .await
        .unwrap_or_else(|error| panic!("cursor fixture facts did not stage: {error}"));
    run.spill
        .seal_resolution(counts)
        .await
        .unwrap_or_else(|error| panic!("cursor fixture facts did not seal: {error}"));
    let before = spill_cursor_snapshot(fixture, run.staged.generation_id()).await;
    install_spill_lease_expiry_trigger(fixture).await;
    assert_eq!(
        run.spill.canonicalize_next().await,
        Err(StorageError::LeaseFenceLost)
    );
    remove_spill_lease_expiry_trigger(fixture).await;
    let after = spill_cursor_snapshot(fixture, run.staged.generation_id()).await;
    assert_eq!(after, before);
    fail_spill_test_run(fixture, run).await;
}

async fn install_spill_lease_expiry_trigger(fixture: &DatabaseFixture) {
    let statements = [
        format!(
            r#"CREATE FUNCTION "{schema}".expire_spill_lease_before_commit()
                RETURNS trigger LANGUAGE plpgsql AS $body$
                BEGIN
                    UPDATE "{schema}".project_operation_leases
                    SET acquired_at = clock_timestamp() - interval '3 seconds',
                        heartbeat_at = clock_timestamp() - interval '2 seconds',
                        expires_at = clock_timestamp() - interval '1 second'
                    WHERE project_id = NEW.project_id
                      AND generation_id = NEW.generation_id
                      AND operation = 'index';
                    PERFORM pg_sleep(0.05);
                    RETURN NEW;
                END
                $body$"#,
            schema = fixture.schema,
        ),
        format!(
            r#"CREATE TRIGGER expire_spill_lease_before_commit
                BEFORE UPDATE ON "{schema}".native_generation_spills
                FOR EACH ROW EXECUTE FUNCTION
                    "{schema}".expire_spill_lease_before_commit()"#,
            schema = fixture.schema,
        ),
    ];
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not install spill lease expiry: {error}"));
    }
}

async fn remove_spill_lease_expiry_trigger(fixture: &DatabaseFixture) {
    let statements = [
        format!(
            r#"DROP TRIGGER expire_spill_lease_before_commit
                ON "{}".native_generation_spills"#,
            fixture.schema,
        ),
        format!(
            r#"DROP FUNCTION "{}".expire_spill_lease_before_commit()"#,
            fixture.schema,
        ),
    ];
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not remove spill lease expiry: {error}"));
    }
}

async fn spill_opaque_counts(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
) -> (i64, i64) {
    let statement = format!(
        r#"SELECT
              (SELECT count(*) FROM "{schema}".native_generation_spill_batches
               WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
              (SELECT count(*) FROM "{schema}".native_generation_spill_rows
               WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid))"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("spill rollback counts were unavailable: {error}"));
    (
        row.try_get::<i64, _>(0)
            .unwrap_or_else(|error| panic!("spill batch count was invalid: {error}")),
        row.try_get::<i64, _>(1)
            .unwrap_or_else(|error| panic!("spill row count was invalid: {error}")),
    )
}

async fn spill_cursor_snapshot(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
) -> (String, Option<String>, i32, i64, i64, i64) {
    let statement = format!(
        r#"SELECT spill.phase, spill.canonical_relation, spill.canonical_partition,
                  spill.canonical_rows,
                  (SELECT count(*) FROM "{schema}".native_generation_spill_files AS raw
                   WHERE raw.project_id = spill.project_id
                     AND raw.generation_id = spill.generation_id),
                  (SELECT count(*) FROM "{schema}".files AS canonical
                   WHERE canonical.project_id = spill.project_id
                     AND canonical.generation_id = spill.generation_id)
            FROM "{schema}".native_generation_spills AS spill
            WHERE spill.project_id = CAST($1 AS uuid)
              AND spill.generation_id = CAST($2 AS uuid)"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("spill cursor snapshot was unavailable: {error}"));
    (
        row.try_get::<String, _>(0)
            .unwrap_or_else(|error| panic!("spill phase was invalid: {error}")),
        row.try_get::<Option<String>, _>(1)
            .unwrap_or_else(|error| panic!("spill relation was invalid: {error}")),
        row.try_get::<i32, _>(2)
            .unwrap_or_else(|error| panic!("spill partition was invalid: {error}")),
        row.try_get::<i64, _>(3)
            .unwrap_or_else(|error| panic!("spill canonical count was invalid: {error}")),
        row.try_get::<i64, _>(4)
            .unwrap_or_else(|error| panic!("spill raw count was invalid: {error}")),
        row.try_get::<i64, _>(5)
            .unwrap_or_else(|error| panic!("spill published count was invalid: {error}")),
    )
}

async fn fail_spill_test_run(fixture: &DatabaseFixture, run: SpillTestRun) {
    fixture
        .database
        .fail_generation(
            RecoverableGeneration::Staged(run.staged),
            &run.lease.fence(),
        )
        .await
        .unwrap_or_else(|error| panic!("spill rollback generation did not fail: {error}"));
    assert!(fixture.database.release_lease(&run.lease).await.is_ok());
}

async fn assert_spill_partition_rejects_missing_edge_target(fixture: &DatabaseFixture) {
    let run = begin_spill_test(fixture).await;
    run.spill
        .append_extracted_batch(extracted_spill_batch())
        .await
        .unwrap_or_else(|error| panic!("invalid-relation parse batch failed early: {error}"));
    run.spill
        .finish_parsing(1)
        .await
        .unwrap_or_else(|error| panic!("invalid-relation parse phase did not seal: {error}"));
    let mut missing_target = edge();
    missing_target.source_symbol_id = symbol_id(SYMBOL_ONE);
    missing_target.target_symbol_id = symbol_id(SYMBOL_TWO);
    let facts = GenerationFacts {
        files: vec![file_one()],
        symbols: vec![symbol_one()],
        edges: vec![missing_target],
        ..GenerationFacts::default()
    };
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("invalid-relation limits were invalid: {error}"));
    let batch = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 0,
            facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("cross-batch relation was rejected too early: {error}"));
    let counts = batch.counts();
    run.spill
        .append_fact_batch(batch)
        .await
        .unwrap_or_else(|error| panic!("invalid-relation fact batch did not stage: {error}"));
    run.spill
        .seal_resolution(counts)
        .await
        .unwrap_or_else(|error| panic!("invalid-relation facts did not seal: {error}"));
    let mut rejected = false;
    for _ in 0..64 {
        match run.spill.canonicalize_next().await {
            Err(StorageError::GenerationSpillConflict) => {
                rejected = true;
                break;
            }
            Err(error) => panic!("invalid relation failed with the wrong error: {error}"),
            Ok(progress) if progress.complete => {
                panic!("missing edge target reached canonical completion")
            }
            Ok(_) => {}
        }
    }
    assert!(
        rejected,
        "missing edge target was not rejected by its partition"
    );
    assert!(
        fixture
            .database
            .fail_generation(
                RecoverableGeneration::Staged(run.staged),
                &run.lease.fence(),
            )
            .await
            .is_ok()
    );
    assert!(fixture.database.release_lease(&run.lease).await.is_ok());
}

async fn assert_spill_partition_rejects_conflicting_document(fixture: &DatabaseFixture) {
    let run = begin_spill_test(fixture).await;
    run.spill
        .append_extracted_batch(extracted_spill_batch())
        .await
        .unwrap_or_else(|error| panic!("document-conflict parse batch failed early: {error}"));
    run.spill
        .finish_parsing(1)
        .await
        .unwrap_or_else(|error| panic!("document-conflict parse phase did not seal: {error}"));
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("document-conflict limits were invalid: {error}"));
    let first = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 0,
            facts: GenerationFacts {
                files: vec![file_one()],
                symbols: vec![symbol_one()],
                documents: vec![document_one(false)],
                ..GenerationFacts::default()
            },
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("first document-conflict batch was invalid: {error}"));
    let mut conflicting_document = document_one(false);
    conflicting_document.natural_text.push_str(" conflicting");
    let second = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 1,
            facts: GenerationFacts {
                documents: vec![conflicting_document],
                ..GenerationFacts::default()
            },
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("second document-conflict batch was invalid: {error}"));
    run.spill
        .append_fact_batch(first)
        .await
        .unwrap_or_else(|error| panic!("first document-conflict batch did not stage: {error}"));
    run.spill
        .append_fact_batch(second)
        .await
        .unwrap_or_else(|error| panic!("second document-conflict batch did not stage: {error}"));
    run.spill
        .seal_resolution(NativeGenerationSpillFactCounts {
            files: 1,
            symbols: 1,
            documents: 2,
            ..NativeGenerationSpillFactCounts::default()
        })
        .await
        .unwrap_or_else(|error| panic!("document-conflict facts did not seal: {error}"));
    let mut rejected = false;
    for _ in 0..96 {
        match run.spill.canonicalize_next().await {
            Err(StorageError::GenerationSpillConflict) => {
                rejected = true;
                break;
            }
            Err(error) => panic!("document conflict failed with the wrong error: {error}"),
            Ok(progress) if progress.complete => {
                panic!("conflicting document reached canonical completion")
            }
            Ok(_) => {}
        }
    }
    assert!(
        rejected,
        "conflicting document was not rejected by its partition"
    );
    assert!(
        fixture
            .database
            .fail_generation(
                RecoverableGeneration::Staged(run.staged),
                &run.lease.fence(),
            )
            .await
            .is_ok()
    );
    assert!(fixture.database.release_lease(&run.lease).await.is_ok());
}

async fn assert_parse_cache_batch_round_trip(fixture: &DatabaseFixture) {
    let contract = digest(CONTENT_HASH_REJECTED);
    let first_key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cache-first.rs",
        CONTENT_HASH_ONE,
        5,
    );
    let second_key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cache-second.rs",
        CONTENT_HASH_TWO,
        6,
    );
    let entries = || {
        vec![
            NativeParseCacheEntry::new(first_key.clone(), b"first".to_vec()),
            NativeParseCacheEntry::new(second_key.clone(), b"second".to_vec()),
        ]
    };
    assert_eq!(
        fixture
            .database
            .store_native_parse_cache_batch(entries())
            .await
            .unwrap_or_else(|error| panic!("parse-cache batch did not commit: {error}")),
        NativeParseCacheBatchWrite {
            inserted: 2,
            already_present: 0,
        }
    );
    assert_eq!(
        fixture
            .database
            .store_native_parse_cache_batch(entries())
            .await
            .unwrap_or_else(|error| panic!("parse-cache retry failed: {error}")),
        NativeParseCacheBatchWrite {
            inserted: 0,
            already_present: 2,
        }
    );
    let loaded = fixture
        .database
        .load_native_parse_cache_batch(&[second_key, first_key])
        .await
        .unwrap_or_else(|error| panic!("parse-cache batch was unavailable: {error}"));
    assert_eq!(
        loaded
            .iter()
            .map(|record| {
                record
                    .as_ref()
                    .map(cartograph_db::NativeParseCacheRecord::payload)
            })
            .collect::<Vec<_>>(),
        vec![Some(b"second".as_slice()), Some(b"first".as_slice())]
    );
}

fn parse_cache_key(
    project_id: &ProjectId,
    contract: &ContentDigest,
    path: &str,
    content_hash: &str,
    source_bytes: u64,
) -> NativeParseCacheKey {
    NativeParseCacheKey::new(NativeParseCacheKeyInput {
        project_id: project_id.clone(),
        extractor_contract_digest: contract.clone(),
        path: NormalizedPath::parse(path)
            .unwrap_or_else(|error| panic!("parse-cache path was invalid: {error}")),
        language: SourceLanguage::Rust,
        content_hash: digest(content_hash),
        source_bytes,
    })
}

struct SpillTestRun {
    staged: StagedGeneration,
    lease: ProjectLease,
    policy: NativeGenerationSpillPolicy,
    spill: NativeGenerationSpill,
}

async fn begin_spill_test(fixture: &DatabaseFixture) -> SpillTestRun {
    let staged = fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            REVISION,
            PARALLEL_WORKERS,
        ))
        .await
        .unwrap_or_else(|error| panic!("spill generation did not begin: {error}"));
    let lease = acquire_generation_lease(fixture, staged.generation_id()).await;
    let policy = NativeGenerationSpillPolicy::new(64 * 1024 * 1024, 1_000_000)
        .unwrap_or_else(|error| panic!("spill policy was invalid: {error}"));
    let spill = NativeGenerationSpill::new(
        fixture.database.clone(),
        &staged,
        NativeGenerationSpillRequest {
            fence: lease.fence(),
            policy,
            statement_timeout: TEST_LEASE_DURATION,
        },
    )
    .unwrap_or_else(|error| panic!("spill authority was invalid: {error}"));
    let report = spill
        .initialize()
        .await
        .unwrap_or_else(|error| panic!("spill did not initialize: {error}"));
    assert_eq!(report.raw_rows, 0);
    SpillTestRun {
        staged,
        lease,
        policy,
        spill,
    }
}

fn extracted_spill_batch() -> NativeGenerationSpillExtractedBatch {
    let first = NativeGenerationSpillRow::new(vec![1], br#"{"file":"first"}"#.to_vec())
        .unwrap_or_else(|error| panic!("extracted row was invalid: {error}"));
    NativeGenerationSpillExtractedBatch::new(0, vec![first.into()])
        .unwrap_or_else(|error| panic!("extracted batch was invalid: {error}"))
}

async fn cached_extracted_spill_batch(
    fixture: &DatabaseFixture,
) -> NativeGenerationSpillExtractedBatch {
    let payload = br#"{"file":"second"}"#.to_vec();
    let contract = digest(CONTENT_HASH_REJECTED);
    let key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cached-spill.rs",
        CONTENT_HASH_TWO,
        6,
    );
    fixture
        .database
        .store_native_parse_cache(&key, &payload)
        .await
        .unwrap_or_else(|error| panic!("cached spill payload did not commit: {error}"));
    let payload_digest = ContentDigest::from_bytes(*blake3::hash(&payload).as_bytes());
    let row = NativeGenerationSpillCachedRow::new(
        vec![2],
        CachedRowPayload {
            key,
            payload_digest,
            payload_bytes: u64::try_from(payload.len()).unwrap_or(u64::MAX),
        },
    )
    .unwrap_or_else(|error| panic!("cached extracted row was invalid: {error}"));
    NativeGenerationSpillExtractedBatch::new(1, vec![row.into()])
        .unwrap_or_else(|error| panic!("cached extracted batch was invalid: {error}"))
}

async fn assert_spill_referenced_cache_survives_new_content(fixture: &DatabaseFixture) {
    let contract = digest(CONTENT_HASH_REJECTED);
    let referenced_key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cached-spill.rs",
        CONTENT_HASH_TWO,
        6,
    );
    let replacement_payload = b"replacement".to_vec();
    let replacement_key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cached-spill.rs",
        CONTENT_HASH_ONE,
        u64::try_from(replacement_payload.len()).unwrap_or(u64::MAX),
    );
    fixture
        .database
        .store_native_parse_cache(&replacement_key, &replacement_payload)
        .await
        .unwrap_or_else(|error| panic!("referenced cache single replacement failed: {error}"));

    let batch_payload = b"batch-replacement".to_vec();
    let batch_key = parse_cache_key(
        &fixture.project,
        &contract,
        "src/cached-spill.rs",
        CONTENT_HASH_REJECTED,
        u64::try_from(batch_payload.len()).unwrap_or(u64::MAX),
    );
    assert_eq!(
        fixture
            .database
            .store_native_parse_cache_batch(vec![NativeParseCacheEntry::new(
                batch_key.clone(),
                batch_payload,
            )])
            .await
            .unwrap_or_else(|error| panic!("referenced cache batch replacement failed: {error}")),
        NativeParseCacheBatchWrite {
            inserted: 1,
            already_present: 0,
        }
    );
    assert!(
        fixture
            .database
            .load_native_parse_cache(&referenced_key)
            .await
            .unwrap_or_else(|error| panic!("referenced cache lookup failed: {error}"))
            .is_some()
    );
    assert!(
        fixture
            .database
            .load_native_parse_cache(&replacement_key)
            .await
            .unwrap_or_else(|error| panic!("obsolete replacement lookup failed: {error}"))
            .is_none()
    );
    assert!(
        fixture
            .database
            .load_native_parse_cache(&batch_key)
            .await
            .unwrap_or_else(|error| panic!("batch replacement lookup failed: {error}"))
            .is_some()
    );
    assert!(
        !fixture
            .database
            .evict_native_parse_cache(&referenced_key)
            .await
            .unwrap_or_else(|error| panic!("referenced cache eviction failed: {error}"))
    );
}

async fn representation_independent_batches(
    fixture: &DatabaseFixture,
    sequence: u64,
    sort_key: u8,
    path: &str,
    content_hash: &str,
    payload: &[u8],
) -> (
    NativeGenerationSpillExtractedBatch,
    NativeGenerationSpillExtractedBatch,
) {
    let contract = digest(CONTENT_HASH_REJECTED);
    let key = parse_cache_key(
        &fixture.project,
        &contract,
        path,
        content_hash,
        u64::try_from(payload.len()).unwrap_or(u64::MAX),
    );
    fixture
        .database
        .store_native_parse_cache(&key, payload)
        .await
        .unwrap_or_else(|error| panic!("replay cache payload did not commit: {error}"));
    let inline = NativeGenerationSpillRow::new(vec![sort_key], payload.to_vec())
        .unwrap_or_else(|error| panic!("inline replay row was invalid: {error}"));
    let cached = NativeGenerationSpillCachedRow::new(
        vec![sort_key],
        CachedRowPayload {
            key,
            payload_digest: ContentDigest::from_bytes(*blake3::hash(payload).as_bytes()),
            payload_bytes: u64::try_from(payload.len()).unwrap_or(u64::MAX),
        },
    )
    .unwrap_or_else(|error| panic!("cached replay row was invalid: {error}"));
    (
        NativeGenerationSpillExtractedBatch::new(sequence, vec![inline.into()])
            .unwrap_or_else(|error| panic!("inline replay batch was invalid: {error}")),
        NativeGenerationSpillExtractedBatch::new(sequence, vec![cached.into()])
            .unwrap_or_else(|error| panic!("cached replay batch was invalid: {error}")),
    )
}

async fn assert_representation_independent_replay(
    fixture: &DatabaseFixture,
    spill: &NativeGenerationSpill,
) {
    let (inline_first, cached_retry) = representation_independent_batches(
        fixture,
        2,
        4,
        "src/inline-first.rs",
        CONTENT_HASH_ONE,
        br#"{"file":"inline-first"}"#,
    )
    .await;
    assert_eq!(
        spill
            .append_extracted_batch(inline_first)
            .await
            .unwrap_or_else(|error| panic!("inline-first replay did not commit: {error}")),
        NativeGenerationSpillWrite::Inserted
    );
    assert_eq!(
        spill
            .append_extracted_batch(cached_retry)
            .await
            .unwrap_or_else(|error| panic!("inline-to-cached replay failed: {error}")),
        NativeGenerationSpillWrite::AlreadyPresent
    );
    let (inline_retry, cached_first) = representation_independent_batches(
        fixture,
        3,
        5,
        "src/cached-first.rs",
        CONTENT_HASH_REJECTED,
        br#"{"file":"cached-first"}"#,
    )
    .await;
    assert_eq!(
        spill
            .append_extracted_batch(cached_first)
            .await
            .unwrap_or_else(|error| panic!("cached-first replay did not commit: {error}")),
        NativeGenerationSpillWrite::Inserted
    );
    assert_eq!(
        spill
            .append_extracted_batch(inline_retry)
            .await
            .unwrap_or_else(|error| panic!("cached-to-inline replay failed: {error}")),
        NativeGenerationSpillWrite::AlreadyPresent
    );
}

async fn exercise_spill_parse_resume(fixture: &DatabaseFixture, run: &mut SpillTestRun) {
    assert_eq!(
        run.spill
            .append_extracted_batch(extracted_spill_batch())
            .await
            .unwrap_or_else(|error| panic!("extracted batch did not commit: {error}")),
        NativeGenerationSpillWrite::Inserted
    );
    assert_eq!(
        run.spill
            .append_extracted_batch(cached_extracted_spill_batch(fixture).await)
            .await
            .unwrap_or_else(|error| panic!("cached extracted batch did not commit: {error}")),
        NativeGenerationSpillWrite::Inserted
    );
    assert_spill_referenced_cache_survives_new_content(fixture).await;
    run.spill = NativeGenerationSpill::new(
        fixture.database.clone(),
        &run.staged,
        NativeGenerationSpillRequest {
            fence: run.lease.fence(),
            policy: run.policy,
            statement_timeout: TEST_LEASE_DURATION,
        },
    )
    .unwrap_or_else(|error| panic!("resumed parse authority was invalid: {error}"));
    let resumed = run
        .spill
        .initialize()
        .await
        .unwrap_or_else(|error| panic!("parse spill did not resume: {error}"));
    assert_eq!(resumed.state, NativeGenerationSpillState::Parsing);
    assert_eq!(
        run.spill
            .append_extracted_batch(extracted_spill_batch())
            .await
            .unwrap_or_else(|error| panic!("extracted retry did not commit: {error}")),
        NativeGenerationSpillWrite::AlreadyPresent
    );
    assert_representation_independent_replay(fixture, &run.spill).await;
    assert_eq!(
        run.spill
            .append_extracted_batch(cached_extracted_spill_batch(fixture).await)
            .await
            .unwrap_or_else(|error| panic!("cached extracted retry failed: {error}")),
        NativeGenerationSpillWrite::AlreadyPresent
    );
    let overlapping = NativeGenerationSpillExtractedBatch::new(
        1,
        vec![
            NativeGenerationSpillRow::new(vec![3], br#"{"file":"overlap"}"#.to_vec())
                .unwrap_or_else(|error| panic!("overlap row was invalid: {error}"))
                .into(),
        ],
    )
    .unwrap_or_else(|error| panic!("overlap batch was invalid: {error}"));
    assert_eq!(
        run.spill.append_extracted_batch(overlapping).await,
        Err(StorageError::GenerationSpillConflict)
    );
    run.spill
        .finish_parsing(4)
        .await
        .unwrap_or_else(|error| panic!("parse spill did not seal: {error}"));
    let first_page = run
        .spill
        .load_extracted_page(NativeGenerationExtractedCursor::default(), 1)
        .await
        .unwrap_or_else(|error| panic!("batched extraction page was unavailable: {error}"));
    assert_eq!(
        first_page
            .rows()
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        vec![0]
    );
    let second_page = run
        .spill
        .load_extracted_page(first_page.next(), 1)
        .await
        .unwrap_or_else(|error| panic!("second extraction page was unavailable: {error}"));
    assert_eq!(
        second_page
            .rows()
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(second_page.rows()[0].1.payload(), br#"{"file":"second"}"#);
}

async fn exercise_spill_fact_resume(
    fixture: &DatabaseFixture,
    run: &mut SpillTestRun,
) -> CanonicalGenerationFacts {
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("spill validation limits were invalid: {error}"));
    let expected = canonical(generation_facts(false));
    let (first_facts, second_facts) = spill_fact_partitions();
    let first = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 1,
            facts: first_facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("fact spill batch was invalid: {error}"));
    let first_counts = first.counts();
    let second = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 2,
            facts: second_facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("second fact spill batch was invalid: {error}"));
    let counts = combined_fact_counts(first_counts, second.counts());
    assert_eq!(
        run.spill
            .append_fact_batches(vec![first, second])
            .await
            .unwrap_or_else(|error| panic!("fact spill batches did not commit: {error}")),
        vec![
            NativeGenerationSpillWrite::Inserted,
            NativeGenerationSpillWrite::Inserted
        ]
    );
    let (first_retry_facts, second_retry_facts) = spill_fact_partitions();
    let first_retry = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 1,
            facts: first_retry_facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("retry fact spill batch was invalid: {error}"));
    let second_retry = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 2,
            facts: second_retry_facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("second retry fact spill batch was invalid: {error}"));
    assert_eq!(
        run.spill
            .append_fact_batches(vec![first_retry, second_retry])
            .await
            .unwrap_or_else(|error| panic!("fact spill retry failed: {error}")),
        vec![
            NativeGenerationSpillWrite::AlreadyPresent,
            NativeGenerationSpillWrite::AlreadyPresent
        ]
    );
    run.spill
        .seal_resolution(counts)
        .await
        .unwrap_or_else(|error| panic!("resolver spill did not seal: {error}"));
    for _ in 0..8 {
        let progress = run
            .spill
            .canonicalize_next()
            .await
            .unwrap_or_else(|error| panic!("initial spill partition failed: {error}"));
        assert!(!progress.complete);
    }
    run.spill = NativeGenerationSpill::new(
        fixture.database.clone(),
        &run.staged,
        NativeGenerationSpillRequest {
            fence: run.lease.fence(),
            policy: run.policy,
            statement_timeout: TEST_LEASE_DURATION,
        },
    )
    .unwrap_or_else(|error| panic!("resumed reduction authority was invalid: {error}"));
    assert_spill_resume_replay(&run.spill, limits).await;
    expected
}

async fn assert_spill_resume_replay(
    spill: &NativeGenerationSpill,
    limits: GenerationValidationLimits,
) {
    let resumed = spill
        .initialize()
        .await
        .unwrap_or_else(|error| panic!("canonical spill did not resume: {error}"));
    assert_eq!(resumed.state, NativeGenerationSpillState::Canonicalizing);
    spill
        .finish_parsing(4)
        .await
        .unwrap_or_else(|error| panic!("resumed parse seal was not idempotent: {error}"));
    let (retry_facts, _) = spill_fact_partitions();
    let retry = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence: 1,
            facts: retry_facts,
            limits,
        },
        || false,
    )
    .unwrap_or_else(|error| panic!("sealed fact retry was invalid: {error}"));
    assert_eq!(
        spill
            .append_fact_batch(retry)
            .await
            .unwrap_or_else(|error| panic!("sealed fact retry failed: {error}")),
        NativeGenerationSpillWrite::AlreadyPresent
    );
}

async fn complete_spill_and_prepare(
    fixture: &DatabaseFixture,
    run: SpillTestRun,
    expected: &CanonicalGenerationFacts,
) {
    let mut complete = false;
    for _ in 0..400 {
        let progress = run
            .spill
            .canonicalize_next()
            .await
            .unwrap_or_else(|error| panic!("spill canonicalization failed: {error}"));
        if progress.complete {
            complete = true;
            break;
        }
    }
    assert!(complete, "spill canonicalization did not complete");
    assert_spill_canonical_counts(
        fixture,
        run.staged.generation_id(),
        canonical_fact_counts(expected),
    )
    .await;
    assert_eq!(
        run.spill
            .compute_digest(|| false, |_| async { false })
            .await,
        Err(StorageError::GenerationSpillCancelled)
    );
    let digest = run
        .spill
        .compute_digest(|| false, |_| async { true })
        .await
        .unwrap_or_else(|error| panic!("spill digest failed: {error}"));
    assert_eq!(digest.digest(), expected.digest());
    assert_spill_digest_counts(digest.counts(), expected);
    let ready = fixture
        .database
        .prepare_spilled_generation(
            SpilledGenerationContents::new(run.staged, digest),
            &run.lease.fence(),
        )
        .await
        .unwrap_or_else(|error| panic!("spilled generation did not become ready: {error}"));
    assert_eq!(ready.content_digest(), expected.digest());
    assert!(fixture.database.release_lease(&run.lease).await.is_ok());
}

fn assert_spill_digest_counts(
    counts: NativeGenerationSpillFactCounts,
    expected: &CanonicalGenerationFacts,
) {
    assert_eq!(counts.files, expected.files().len() as u64);
    assert_eq!(counts.symbols, expected.symbols().len() as u64);
    assert_eq!(counts.edges, expected.edges().len() as u64);
    assert_eq!(counts.references, expected.references().len() as u64);
    assert_eq!(
        counts.numerical_sites,
        expected.numerical_sites().len() as u64
    );
    assert_eq!(counts.documents, expected.documents().len() as u64);
}

fn combined_fact_counts(
    first: NativeGenerationSpillFactCounts,
    second: NativeGenerationSpillFactCounts,
) -> NativeGenerationSpillFactCounts {
    NativeGenerationSpillFactCounts {
        files: first.files + second.files,
        symbols: first.symbols + second.symbols,
        edges: first.edges + second.edges,
        references: first.references + second.references,
        numerical_sites: first.numerical_sites + second.numerical_sites,
        documents: first.documents + second.documents,
    }
}

fn canonical_fact_counts(expected: &CanonicalGenerationFacts) -> NativeGenerationSpillFactCounts {
    NativeGenerationSpillFactCounts {
        files: expected.files().len() as u64,
        symbols: expected.symbols().len() as u64,
        edges: expected.edges().len() as u64,
        references: expected.references().len() as u64,
        numerical_sites: expected.numerical_sites().len() as u64,
        documents: expected.documents().len() as u64,
    }
}

async fn assert_spill_quota_and_fence(fixture: &DatabaseFixture) {
    let staged = fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            "2222222222222222222222222222222222222222",
            SINGLE_WORKER,
        ))
        .await
        .unwrap_or_else(|error| panic!("quota generation did not begin: {error}"));
    let lease = acquire_generation_lease(fixture, staged.generation_id()).await;
    let spill = NativeGenerationSpill::new(
        fixture.database.clone(),
        &staged,
        NativeGenerationSpillRequest {
            fence: lease.fence(),
            policy: NativeGenerationSpillPolicy::new(1, 1)
                .unwrap_or_else(|error| panic!("quota policy was invalid: {error}")),
            statement_timeout: TEST_LEASE_DURATION,
        },
    )
    .unwrap_or_else(|error| panic!("quota authority was invalid: {error}"));
    spill
        .initialize()
        .await
        .unwrap_or_else(|error| panic!("quota spill did not initialize: {error}"));
    let oversized = || {
        NativeGenerationSpillExtractedBatch::new(
            0,
            vec![
                NativeGenerationSpillRow::new(vec![1], vec![2])
                    .unwrap_or_else(|error| panic!("quota row was invalid: {error}"))
                    .into(),
            ],
        )
        .unwrap_or_else(|error| panic!("quota batch was invalid: {error}"))
    };
    assert_matches!(
        spill.append_extracted_batch(oversized()).await,
        Err(StorageError::GenerationSpillLimitReached { resource: "bytes" })
    );
    assert_eq!(
        spill
            .report()
            .await
            .unwrap_or_else(|error| panic!("quota report was unavailable: {error}"))
            .raw_rows,
        0
    );
    assert!(fixture.database.release_lease(&lease).await.is_ok());
    assert_eq!(
        spill.append_extracted_batch(oversized()).await,
        Err(StorageError::LeaseFenceLost)
    );
}

async fn assert_spill_canonical_counts(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
    expected: cartograph_db::NativeGenerationSpillFactCounts,
) {
    let statement = format!(
        r#"SELECT
            (SELECT count(*) FROM "{schema}"."files"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
            (SELECT count(*) FROM "{schema}"."symbols"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
            (SELECT count(*) FROM "{schema}"."edges"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
            (SELECT count(*) FROM "{schema}"."references"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
            (SELECT count(*) FROM "{schema}"."numerical_sites"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)),
            (SELECT count(*) FROM "{schema}"."search_documents"
              WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid))"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("canonical spill counts were unavailable: {error}"));
    let actual = [
        row.try_get::<i64, _>(0).ok(),
        row.try_get::<i64, _>(1).ok(),
        row.try_get::<i64, _>(2).ok(),
        row.try_get::<i64, _>(3).ok(),
        row.try_get::<i64, _>(4).ok(),
        row.try_get::<i64, _>(5).ok(),
    ];
    assert_eq!(actual[0], i64::try_from(expected.files).ok());
    assert_eq!(actual[1], i64::try_from(expected.symbols).ok());
    assert_eq!(actual[2], i64::try_from(expected.edges).ok());
    assert_eq!(actual[3], i64::try_from(expected.references).ok());
    assert_eq!(actual[4], i64::try_from(expected.numerical_sites).ok());
    assert_eq!(actual[5], i64::try_from(expected.documents).ok());
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn copy_ingestion_is_atomic_and_logically_deterministic() {
    let fixture = open_fixture().await;

    let reused_metrics = PrepareGenerationMetrics::new();
    let first = prepare_generation_with_metrics(
        &fixture,
        ObservedGenerationInput {
            workers: SINGLE_WORKER,
            facts: generation_facts(false),
            metrics: reused_metrics.clone(),
        },
    )
    .await;
    let second = prepare_generation(&fixture, PARALLEL_WORKERS, generation_facts(true)).await;
    assert_eq!(first.content_digest(), second.content_digest());
    assert_eq!(first.digest_version(), GenerationDigestVersion::CURRENT);
    assert_eq!(second.digest_version(), GenerationDigestVersion::CURRENT);
    assert_eq!(first.content_digest().as_str(), EXPECTED_LOGICAL_DIGEST);
    assert_persisted_generation(&fixture, &first).await;
    assert_persisted_generation(&fixture, &second).await;
    assert_copy_text_round_trip(&fixture, &second).await;
    assert_reference_evidence_round_trip(&fixture, &second).await;
    assert_numerical_evidence_round_trip(&fixture, &second).await;
    assert_ready_digest_recovery(&fixture, &first).await;
    assert_copy_chunk_and_null_boundaries(&fixture).await;

    install_relation_corruption_trigger(&fixture).await;
    assert_relation_validation_rolls_back(&fixture).await;
    remove_relation_corruption_trigger(&fixture).await;

    install_rejecting_trigger(&fixture).await;
    assert_copy_failure_rolls_back(&fixture, reused_metrics).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &fixture.schema).await;
    fixture.pool.close().await;
}

async fn assert_numerical_evidence_round_trip(fixture: &DatabaseFixture, ready: &ReadyGeneration) {
    let statement = format!(
        r#"SELECT numerical_site_id::text, owner_symbol_id::text, operation, hazard,
                  precision, confidence_ppm, provenance, evidence_level, unknowns
            FROM "{}"."numerical_sites"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(ready.generation_id().as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect numerical evidence: {error}"));
    assert_eq!(
        row.try_get::<String, _>(0).ok().as_deref(),
        Some(NUMERICAL_SITE_ONE)
    );
    assert_eq!(
        row.try_get::<String, _>(1).ok().as_deref(),
        Some(SYMBOL_ONE)
    );
    assert_eq!(
        row.try_get::<String, _>(2).ok().as_deref(),
        Some("tolerance_comparison")
    );
    assert_eq!(
        row.try_get::<String, _>(3).ok().as_deref(),
        Some("absolute_only_tolerance")
    );
    assert_eq!(row.try_get::<String, _>(4).ok().as_deref(), Some("f32"));
    assert_matches!(row.try_get::<i32, _>(5), Ok(900_000));
    assert_eq!(
        row.try_get::<String, _>(6).ok().as_deref(),
        Some("rust_ast_v1")
    );
    assert_eq!(
        row.try_get::<String, _>(7).ok().as_deref(),
        Some("heuristic")
    );
    assert_eq!(
        row.try_get::<String, _>(8).ok().as_deref(),
        Some("relative_scale,input_range")
    );
}

async fn assert_reference_evidence_round_trip(fixture: &DatabaseFixture, ready: &ReadyGeneration) {
    let statement = format!(
        r#"SELECT owner_symbol_id::text, target_symbol_id::text, reference_name,
                  resolution_provenance
            FROM "{}"."references"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(ready.generation_id().as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect reference evidence: {error}"));
    assert_matches!(
        row.try_get::<String, _>(0),
        Ok(value) if value == SYMBOL_TWO
    );
    assert_matches!(
        row.try_get::<String, _>(1),
        Ok(value) if value == SYMBOL_ONE
    );
    assert_matches!(
        row.try_get::<String, _>(2),
        Ok(value) if value == "fixture_call"
    );
    assert_matches!(
        row.try_get::<String, _>(3),
        Ok(value) if value == "test-exact"
    );
}

async fn assert_copy_chunk_and_null_boundaries(fixture: &DatabaseFixture) {
    let ready = prepare_generation(
        fixture,
        PARALLEL_WORKERS,
        GenerationFacts {
            documents: vec![
                chunk_document(
                    DOCUMENT_CHUNK_ONE,
                    "docs/chunk-one.md",
                    CUMULATIVE_COPY_CODE_BYTES,
                ),
                chunk_document(
                    DOCUMENT_CHUNK_TWO,
                    "docs/chunk-two.md",
                    CUMULATIVE_COPY_CODE_BYTES,
                ),
                chunk_document(
                    DOCUMENT_OVERSIZED_ROW,
                    "docs/oversized-row.md",
                    INDIVIDUAL_COPY_CODE_BYTES,
                ),
            ],
            ..GenerationFacts::default()
        },
    )
    .await;
    let statement = format!(
        r#"SELECT
                count(*) AS documents,
                count(*) FILTER (WHERE file_id IS NULL) AS null_file_ids,
                count(*) FILTER (WHERE symbol_id IS NULL) AS null_symbol_ids,
                count(*) FILTER (
                    WHERE octet_length(code) = {cumulative_bytes}
                ) AS cumulative_rows,
                count(*) FILTER (
                    WHERE octet_length(code) = {individual_bytes}
                ) AS individual_rows
            FROM "{schema}"."search_documents"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#,
        schema = fixture.schema,
        cumulative_bytes = CUMULATIVE_COPY_CODE_BYTES,
        individual_bytes = INDIVIDUAL_COPY_CODE_BYTES,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(ready.generation_id().as_str())
        .fetch_one(&fixture.pool)
        .await;
    let row = match row {
        Ok(row) => row,
        Err(error) => panic!("could not inspect COPY chunk boundaries: {error}"),
    };
    assert_matches!(row.try_get::<i64, _>("documents"), Ok(CHUNK_DOCUMENT_COUNT));
    assert_matches!(
        row.try_get::<i64, _>("null_file_ids"),
        Ok(CHUNK_DOCUMENT_COUNT)
    );
    assert_matches!(
        row.try_get::<i64, _>("null_symbol_ids"),
        Ok(CHUNK_DOCUMENT_COUNT)
    );
    assert_matches!(
        row.try_get::<i64, _>("cumulative_rows"),
        Ok(CUMULATIVE_COPY_ROW_COUNT)
    );
    assert_matches!(
        row.try_get::<i64, _>("individual_rows"),
        Ok(INDIVIDUAL_COPY_ROW_COUNT)
    );
}

fn chunk_document(id: &str, path: &str, code_bytes: usize) -> SearchDocumentInput {
    SearchDocumentInput {
        document_id: document_id(id),
        file_id: None,
        symbol_id: None,
        path: path.to_owned(),
        language: "markdown".to_owned(),
        kind: DocumentKind::Documentation,
        qualified_name: String::new(),
        code: "x".repeat(code_bytes),
        natural_text: String::new(),
        metadata: serde_json::json!({}),
    }
}

async fn assert_ready_digest_recovery(fixture: &DatabaseFixture, ready: &ReadyGeneration) {
    let recovered = fixture
        .database
        .recover_generation(GenerationRecoveryRequest::new(
            &fixture.project,
            ready.generation_id(),
        ))
        .await;
    let recovered = match recovered {
        Ok(Some(RecoverableGeneration::Ready(recovered))) => recovered,
        Ok(_) => panic!("ready COPY generation was not recoverable"),
        Err(error) => panic!("ready COPY generation recovery failed: {error}"),
    };
    assert_eq!(recovered.content_digest(), ready.content_digest());
}

struct DatabaseFixture {
    database: CartographDatabase,
    pool: sqlx_postgres::PgPool,
    schema: String,
    project: ProjectId,
    _schema_guard: TestSchemaGuard,
}

async fn open_fixture() -> DatabaseFixture {
    let database_url = env::var(TEST_DATABASE_URL_ENV).unwrap_or_else(|error| {
        panic!("{TEST_DATABASE_URL_ENV} must be set for the ignored integration test: {error}")
    });
    let schema = format!(
        "cartograph_ingest_it_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let settings = DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
        .and_then(|settings| settings.with_schema(&schema));
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => panic!("ingest test database settings failed validation: {error}"),
    };
    let pool = match cartograph_db::connect(&settings).await {
        Ok(pool) => pool,
        Err(error) => panic!("ingest test database connection failed: {error}"),
    };
    let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
    if let Err(error) = database.migrate().await {
        panic!("ingest test migration failed: {error}");
    }
    let project = register_project(&database).await;
    DatabaseFixture {
        database,
        pool,
        schema,
        project,
        _schema_guard: TestSchemaGuard::new(database_url, settings.schema().as_str())
            .unwrap_or_else(|error| panic!("ingest schema guard failed: {error}")),
    }
}

async fn register_project(database: &CartographDatabase) -> ProjectId {
    match database
        .register_project(NewProject::new(
            "workspace/copy-ingest",
            digest(PROJECT_FINGERPRINT),
        ))
        .await
    {
        Ok(project) => project,
        Err(error) => panic!("ingest test project registration failed: {error}"),
    }
}

async fn prepare_generation(
    fixture: &DatabaseFixture,
    workers: u16,
    facts: GenerationFacts,
) -> ReadyGeneration {
    prepare_generation_with_metrics(
        fixture,
        ObservedGenerationInput {
            workers,
            facts,
            metrics: PrepareGenerationMetrics::new(),
        },
    )
    .await
}

struct ObservedGenerationInput {
    workers: u16,
    facts: GenerationFacts,
    metrics: PrepareGenerationMetrics,
}

async fn prepare_generation_with_metrics(
    fixture: &DatabaseFixture,
    input: ObservedGenerationInput,
) -> ReadyGeneration {
    let ObservedGenerationInput {
        workers,
        facts,
        metrics,
    } = input;
    let staged = match fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            REVISION,
            workers,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("ingest test generation did not begin: {error}"),
    };
    let lease = acquire_generation_lease(fixture, staged.generation_id()).await;
    let result = fixture
        .database
        .prepare_generation(
            GenerationContents::new(staged, canonical(facts)).with_metrics(metrics.clone()),
            &lease.fence(),
        )
        .await;
    assert!(fixture.database.release_lease(&lease).await.is_ok());
    match result {
        Ok(ready) => {
            let snapshot = metrics.snapshot();
            assert!(!snapshot.copy_duration().is_zero());
            let table_durations = [
                snapshot.files_copy_duration(),
                snapshot.symbols_copy_duration(),
                snapshot.edges_copy_duration(),
                snapshot.references_copy_duration(),
                snapshot.numerical_sites_copy_duration(),
                snapshot.documents_copy_duration(),
            ];
            assert!(table_durations.iter().all(|duration| !duration.is_zero()));
            assert!(table_durations.into_iter().sum::<Duration>() <= snapshot.copy_duration());
            assert!(!snapshot.relation_validation_duration().is_zero());
            ready
        }
        Err(error) => panic!("COPY generation preparation failed: {error}"),
    }
}

async fn install_relation_corruption_trigger(fixture: &DatabaseFixture) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}".inject_invalid_edge_fixture() RETURNS trigger
            LANGUAGE plpgsql AS $$
            BEGIN
                INSERT INTO "{schema}"."edges" (
                    project_id, generation_id, source_symbol_id, target_symbol_id,
                    edge_kind, confidence, provenance
                ) VALUES (
                    NEW.project_id, NEW.generation_id,
                    '{source_symbol}'::uuid, '{target_symbol}'::uuid,
                    'calls', 1.0, 'relation-validation-fixture'
                ) ON CONFLICT DO NOTHING;
                RETURN NEW;
            END
            $$"#,
        schema = fixture.schema,
        source_symbol = CORRUPT_SOURCE_SYMBOL,
        target_symbol = CORRUPT_TARGET_SYMBOL,
    );
    let trigger = format!(
        r#"CREATE TRIGGER inject_invalid_edge_fixture
            AFTER INSERT ON "{schema}"."search_documents"
            FOR EACH ROW EXECUTE FUNCTION "{schema}".inject_invalid_edge_fixture()"#,
        schema = fixture.schema,
    );
    if let Err(error) = query(AssertSqlSafe(function)).execute(&fixture.pool).await {
        panic!("could not create relation-corruption function: {error}");
    }
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(&fixture.pool).await {
        panic!("could not create relation-corruption trigger: {error}");
    }
}

async fn remove_relation_corruption_trigger(fixture: &DatabaseFixture) {
    let trigger = format!(
        r#"DROP TRIGGER inject_invalid_edge_fixture ON "{schema}"."search_documents""#,
        schema = fixture.schema,
    );
    let function = format!(
        r#"DROP FUNCTION "{schema}".inject_invalid_edge_fixture()"#,
        schema = fixture.schema,
    );
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(&fixture.pool).await {
        panic!("could not remove relation-corruption trigger: {error}");
    }
    if let Err(error) = query(AssertSqlSafe(function)).execute(&fixture.pool).await {
        panic!("could not remove relation-corruption trigger: {error}");
    }
}

async fn assert_relation_validation_rolls_back(fixture: &DatabaseFixture) {
    let staged = match fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            REVISION,
            PARALLEL_WORKERS,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("relation fixture generation did not begin: {error}"),
    };
    let generation_id = staged.generation_id().clone();
    let lease = acquire_generation_lease(fixture, &generation_id).await;
    let fence = lease.fence();
    let failed = fixture
        .database
        .prepare_generation(
            GenerationContents::new(staged, canonical(generation_facts(false))),
            &fence,
        )
        .await;
    let staged = match failed {
        Err(error) => {
            assert_matches!(
                error.error(),
                StorageError::DatabaseOperation {
                    operation: "verify-copied-relations"
                }
            );
            error.into_parts().0
        }
        Ok(_) => panic!("relation-corrupted COPY unexpectedly became ready"),
    };
    assert_generation_is_empty(fixture, &generation_id).await;
    assert!(
        fixture
            .database
            .fail_generation(RecoverableGeneration::Staged(staged), &fence)
            .await
            .is_ok()
    );
    assert!(fixture.database.release_lease(&lease).await.is_ok());
}

fn generation_facts(reversed: bool) -> GenerationFacts {
    let mut files = vec![file_one(), file_two()];
    let mut symbols = vec![symbol_one(), symbol_two()];
    let edges = vec![edge()];
    let mut references = vec![reference()];
    let mut numerical_sites = vec![numerical_site()];
    let mut documents = vec![document_one(reversed), document_two()];
    if reversed {
        files.reverse();
        symbols.reverse();
        documents.reverse();
        references.push(reference());
        numerical_sites.push(numerical_site());
        documents.push(document_two());
    }
    GenerationFacts {
        files,
        symbols,
        edges,
        references,
        numerical_sites,
        documents,
    }
}

fn spill_fact_partitions() -> (GenerationFacts, GenerationFacts) {
    let mut facts = generation_facts(false);
    let first = GenerationFacts {
        files: vec![facts.files.remove(0)],
        symbols: vec![facts.symbols.remove(0)],
        edges: Vec::new(),
        references: Vec::new(),
        numerical_sites: vec![facts.numerical_sites.remove(0)],
        documents: vec![facts.documents.remove(0)],
    };
    (first, facts)
}

fn file_one() -> FileInput {
    FileInput {
        file_id: file_id(FILE_ONE),
        normalized_path: "src/parser.rs".to_owned(),
        language: "rust".to_owned(),
        content_hash: digest(CONTENT_HASH_ONE),
        byte_size: 128,
        parse_status: FileParseStatus::Parsed,
    }
}

fn file_two() -> FileInput {
    FileInput {
        file_id: file_id(FILE_TWO),
        normalized_path: "tests/parser_test.rs".to_owned(),
        language: "rust".to_owned(),
        content_hash: digest(CONTENT_HASH_TWO),
        byte_size: 256,
        parse_status: FileParseStatus::Partial,
    }
}

fn symbol_one() -> SymbolInput {
    SymbolInput {
        symbol_id: symbol_id(SYMBOL_ONE),
        file_id: file_id(FILE_ONE),
        symbol_kind: "function".to_owned(),
        qualified_name: "parser::parseHTTPResponse".to_owned(),
        signature: "fn parse_http_response(input:\t&str)\n -> Result<Response, Error>\\path"
            .to_owned(),
        start_byte: 0,
        end_byte: 96,
        start_line: 1,
        end_line: 4,
        structural_digest: digest(STRUCTURAL_HASH_ONE),
        visibility: None,
        export: cartograph_domain::SymbolExportFlags::named(true),
        execution: cartograph_domain::SymbolExecutionFlags::default(),
        declaration_only: false,
        betweenness_ppb: None,
        pagerank_ppb: None,
    }
}

fn symbol_two() -> SymbolInput {
    SymbolInput {
        symbol_id: symbol_id(SYMBOL_TWO),
        file_id: file_id(FILE_TWO),
        symbol_kind: "test".to_owned(),
        qualified_name: "parser_test::parses_http_response".to_owned(),
        signature: "fn parses_http_response()".to_owned(),
        start_byte: SYMBOL_TWO_START_BYTE,
        end_byte: SYMBOL_TWO_END_BYTE,
        start_line: SYMBOL_TWO_START_LINE,
        end_line: SYMBOL_TWO_END_LINE,
        structural_digest: digest(STRUCTURAL_HASH_TWO),
        visibility: None,
        export: cartograph_domain::SymbolExportFlags::default(),
        execution: cartograph_domain::SymbolExecutionFlags::default(),
        declaration_only: false,
        betweenness_ppb: None,
        pagerank_ppb: None,
    }
}

fn edge() -> EdgeInput {
    EdgeInput {
        source_symbol_id: symbol_id(SYMBOL_TWO),
        target_symbol_id: symbol_id(SYMBOL_ONE),
        kind: EdgeKind::Tests,
        confidence: 1.0,
        provenance: "tree-sitter\ttest-resolver\\v2".to_owned(),
        site_count: 1,
    }
}

fn reference() -> ReferenceInput {
    ReferenceInput {
        file_id: file_id(FILE_TWO),
        owner_symbol_id: Some(symbol_id(SYMBOL_TWO)),
        target_symbol_id: Some(symbol_id(SYMBOL_ONE)),
        reference_name: "fixture_call".to_owned(),
        reference_kind: "call".to_owned(),
        start_byte: REFERENCE_START_BYTE,
        end_byte: REFERENCE_END_BYTE,
        confidence: REFERENCE_CONFIDENCE,
        resolution_provenance: "test-exact".to_owned(),
        site_count: 1,
        span_precision: cartograph_db::ReferenceSpanPrecision::Exact,
    }
}

fn numerical_site() -> NumericalSiteInput {
    NumericalSiteInput {
        site_id: NumericalSiteId::parse(NUMERICAL_SITE_ONE)
            .unwrap_or_else(|error| panic!("fixture numerical site UUID is invalid: {error}")),
        file_id: file_id(FILE_ONE),
        owner_symbol_id: Some(symbol_id(SYMBOL_ONE)),
        start_byte: 20,
        end_byte: 32,
        start_line: 2,
        end_line: 2,
        operation: "tolerance_comparison".to_owned(),
        hazard: "absolute_only_tolerance".to_owned(),
        precision: "f32".to_owned(),
        expression_digest: digest(CONTENT_HASH_ONE),
        confidence_ppm: 900_000,
        provenance: "rust_ast_v1".to_owned(),
        evidence_level: "heuristic".to_owned(),
        unknowns: "relative_scale,input_range".to_owned(),
    }
}

fn canonical(facts: GenerationFacts) -> CanonicalGenerationFacts {
    let limits = GenerationValidationLimits::new(
        TEST_VALIDATION_OUTPUT_BYTES,
        TEST_VALIDATION_WORKING_BYTES,
    )
    .unwrap_or_else(|error| panic!("ingest validation limits were invalid: {error}"));
    validate_generation_facts(facts, limits, || false).map_or_else(
        |error| panic!("ingest fixture was invalid: {error}"),
        |(facts, _)| facts,
    )
}

fn document_one(reordered_metadata: bool) -> SearchDocumentInput {
    let metadata = if reordered_metadata {
        serde_json::json!({
            "zeta": "last",
            "sentinel": "\\N\tline\nnext",
            "alpha": "first"
        })
    } else {
        serde_json::json!({
            "alpha": "first",
            "sentinel": "\\N\tline\nnext",
            "zeta": "last"
        })
    };
    SearchDocumentInput {
        document_id: document_id(DOCUMENT_ONE),
        file_id: Some(file_id(FILE_ONE)),
        symbol_id: Some(symbol_id(SYMBOL_ONE)),
        path: "src/parser.rs".to_owned(),
        language: "rust".to_owned(),
        kind: DocumentKind::Symbol,
        qualified_name: "parser::parseHTTPResponse".to_owned(),
        code: "fn parse_http_response() {\n\tlet marker = \\\"\\\\N\\\";\n}".to_owned(),
        natural_text: "Parse an HTTP response\nwithout client-wall-clock assumptions".to_owned(),
        metadata,
    }
}

fn document_two() -> SearchDocumentInput {
    SearchDocumentInput {
        document_id: document_id(DOCUMENT_TWO),
        file_id: Some(file_id(FILE_TWO)),
        symbol_id: Some(symbol_id(SYMBOL_TWO)),
        path: "tests/parser_test.rs".to_owned(),
        language: "rust".to_owned(),
        kind: DocumentKind::Test,
        qualified_name: "parser_test::parses_http_response".to_owned(),
        code: "#[test]\nfn parses_http_response() {}".to_owned(),
        natural_text: "Regression for HTTP response parsing".to_owned(),
        metadata: serde_json::json!({"role": "regression"}),
    }
}

async fn assert_persisted_generation(fixture: &DatabaseFixture, ready: &ReadyGeneration) {
    let statement = format!(
        r#"SELECT
                (SELECT count(*) FROM "{schema}"."files"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS files,
                (SELECT count(*) FROM "{schema}"."symbols"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS symbols,
                (SELECT count(*) FROM "{schema}"."edges"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS edges,
                (SELECT count(*) FROM "{schema}"."references"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS refs,
                (SELECT count(*) FROM "{schema}"."numerical_sites"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS numerical_sites,
                (SELECT count(*) FROM "{schema}"."search_documents"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS documents,
                (SELECT content_digest FROM "{schema}"."index_generations"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS digest,
                (SELECT content_digest_version FROM "{schema}"."index_generations"
                    WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS digest_version"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(ready.generation_id().as_str())
        .fetch_one(&fixture.pool)
        .await;
    let row = match row {
        Ok(row) => row,
        Err(error) => panic!("could not inspect copied generation: {error}"),
    };
    assert_matches!(row.try_get::<i64, _>("files"), Ok(2));
    assert_matches!(row.try_get::<i64, _>("symbols"), Ok(2));
    assert_matches!(row.try_get::<i64, _>("edges"), Ok(1));
    assert_matches!(row.try_get::<i64, _>("refs"), Ok(1));
    assert_matches!(row.try_get::<i64, _>("numerical_sites"), Ok(1));
    assert_matches!(row.try_get::<i64, _>("documents"), Ok(2));
    assert_eq!(
        row.try_get::<String, _>("digest").ok().as_deref(),
        Some(ready.content_digest().as_str())
    );
    assert_matches!(
        row.try_get::<i16, _>("digest_version"),
        Ok(value) if value == GenerationDigestVersion::CURRENT.database_value()
    );
}

async fn assert_copy_text_round_trip(fixture: &DatabaseFixture, ready: &ReadyGeneration) {
    let statement = format!(
        r#"SELECT symbols.signature, edges.provenance, documents.code,
                   documents.metadata ->> 'sentinel' AS sentinel
            FROM "{schema}"."symbols" AS symbols
            INNER JOIN "{schema}"."edges" AS edges
              ON edges.project_id = symbols.project_id
             AND edges.generation_id = symbols.generation_id
             AND edges.target_symbol_id = symbols.symbol_id
            INNER JOIN "{schema}"."search_documents" AS documents
              ON documents.project_id = symbols.project_id
             AND documents.generation_id = symbols.generation_id
             AND documents.symbol_id = symbols.symbol_id
            WHERE symbols.project_id = CAST($1 AS uuid)
              AND symbols.generation_id = CAST($2 AS uuid)
              AND symbols.symbol_id = CAST($3 AS uuid)"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(ready.generation_id().as_str())
        .bind(SYMBOL_ONE)
        .fetch_one(&fixture.pool)
        .await;
    let row = match row {
        Ok(row) => row,
        Err(error) => panic!("could not inspect COPY text round trip: {error}"),
    };
    assert_eq!(
        row.try_get::<String, _>("signature").ok().as_deref(),
        Some("fn parse_http_response(input:\t&str)\n -> Result<Response, Error>\\path")
    );
    assert_eq!(
        row.try_get::<String, _>("provenance").ok().as_deref(),
        Some("tree-sitter\ttest-resolver\\v2")
    );
    assert_eq!(
        row.try_get::<String, _>("code").ok().as_deref(),
        Some("fn parse_http_response() {\n\tlet marker = \\\"\\\\N\\\";\n}")
    );
    assert_eq!(
        row.try_get::<String, _>("sentinel").ok().as_deref(),
        Some("\\N\tline\nnext")
    );
}

async fn install_rejecting_trigger(fixture: &DatabaseFixture) {
    let function = format!(
        r#"CREATE FUNCTION "{schema}".reject_copy_fixture() RETURNS trigger
            LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.path = 'src/reject.rs' THEN
                    RAISE EXCEPTION 'intentional COPY rollback fixture';
                END IF;
                RETURN NEW;
            END
            $$"#,
        schema = fixture.schema,
    );
    let trigger = format!(
        r#"CREATE TRIGGER reject_copy_fixture
            BEFORE INSERT ON "{schema}"."search_documents"
            FOR EACH ROW EXECUTE FUNCTION "{schema}".reject_copy_fixture()"#,
        schema = fixture.schema,
    );
    if let Err(error) = query(AssertSqlSafe(function)).execute(&fixture.pool).await {
        panic!("could not create COPY rollback function: {error}");
    }
    if let Err(error) = query(AssertSqlSafe(trigger)).execute(&fixture.pool).await {
        panic!("could not create COPY rollback trigger: {error}");
    }
}

async fn assert_copy_failure_rolls_back(
    fixture: &DatabaseFixture,
    metrics: PrepareGenerationMetrics,
) {
    let staged = match fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            REVISION,
            PARALLEL_WORKERS,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("rollback fixture generation did not begin: {error}"),
    };
    let generation_id = staged.generation_id().clone();
    let lease = acquire_generation_lease(fixture, &generation_id).await;
    let fence = lease.fence();
    let failed = fixture
        .database
        .prepare_generation(
            GenerationContents::new(staged, canonical(rejected_facts()))
                .with_metrics(metrics.clone()),
            &fence,
        )
        .await;
    let staged = match failed {
        Err(error) => {
            assert_matches!(
                error.error(),
                StorageError::DatabaseOperation {
                    operation: "copy-search-documents"
                }
            );
            error.into_parts().0
        }
        Ok(_) => panic!("trigger-rejected COPY unexpectedly became ready"),
    };
    let snapshot = metrics.snapshot();
    assert!(!snapshot.copy_duration().is_zero());
    let attempted_tables = [
        snapshot.files_copy_duration(),
        snapshot.symbols_copy_duration(),
        snapshot.edges_copy_duration(),
        snapshot.references_copy_duration(),
        snapshot.numerical_sites_copy_duration(),
        snapshot.documents_copy_duration(),
    ];
    assert!(attempted_tables.iter().all(|duration| !duration.is_zero()));
    assert!(attempted_tables.into_iter().sum::<Duration>() <= snapshot.copy_duration());
    assert!(snapshot.relation_validation_duration().is_zero());
    assert_generation_is_empty(fixture, &generation_id).await;
    assert!(
        fixture
            .database
            .fail_generation(RecoverableGeneration::Staged(staged), &fence)
            .await
            .is_ok()
    );
    assert!(fixture.database.release_lease(&lease).await.is_ok());
}

async fn acquire_generation_lease(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
) -> ProjectLease {
    let target = LeaseTarget::new(
        fixture.project.clone(),
        ProjectOperation::Index,
        Some(generation_id.clone()),
    );
    match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target,
            LeaseOwner::new(process::id(), format!("ingest-test-{generation_id}")),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("ingest test lease acquisition failed: {error}"),
    }
}

fn rejected_facts() -> GenerationFacts {
    GenerationFacts {
        files: vec![FileInput {
            file_id: file_id(FILE_REJECTED),
            normalized_path: "src/reject.rs".to_owned(),
            language: "rust".to_owned(),
            content_hash: digest(CONTENT_HASH_REJECTED),
            byte_size: 16,
            parse_status: FileParseStatus::Parsed,
        }],
        symbols: Vec::new(),
        edges: Vec::new(),
        references: Vec::new(),
        numerical_sites: Vec::new(),
        documents: vec![SearchDocumentInput {
            document_id: document_id(DOCUMENT_REJECTED),
            file_id: Some(file_id(FILE_REJECTED)),
            symbol_id: None,
            path: "src/reject.rs".to_owned(),
            language: "rust".to_owned(),
            kind: DocumentKind::File,
            qualified_name: String::new(),
            code: "fn rejected() {}".to_owned(),
            natural_text: String::new(),
            metadata: serde_json::json!({}),
        }],
    }
}

async fn assert_generation_is_empty(fixture: &DatabaseFixture, generation: &GenerationId) {
    let statement = format!(
        r#"SELECT
                (SELECT count(*) FROM "{schema}"."files" WHERE generation_id = CAST($1 AS uuid))
              + (SELECT count(*) FROM "{schema}"."symbols" WHERE generation_id = CAST($1 AS uuid))
              + (SELECT count(*) FROM "{schema}"."edges" WHERE generation_id = CAST($1 AS uuid))
              + (SELECT count(*) FROM "{schema}"."references" WHERE generation_id = CAST($1 AS uuid))
              + (SELECT count(*) FROM "{schema}"."numerical_sites" WHERE generation_id = CAST($1 AS uuid))
              + (SELECT count(*) FROM "{schema}"."search_documents" WHERE generation_id = CAST($1 AS uuid))
                AS fact_count"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(generation.as_str())
        .fetch_one(&fixture.pool)
        .await;
    assert_matches!(
        row.and_then(|row| row.try_get::<i64, _>("fact_count")),
        Ok(0)
    );
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
        panic!("failed to drop isolated ingest-test schema: {error}");
    }
}

fn file_id(raw: &str) -> FileId {
    match FileId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("fixture file UUID is invalid: {error}"),
    }
}

fn symbol_id(raw: &str) -> SymbolId {
    match SymbolId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("fixture symbol UUID is invalid: {error}"),
    }
}

fn document_id(raw: &str) -> DocumentId {
    match DocumentId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("fixture document UUID is invalid: {error}"),
    }
}

fn digest(raw: &str) -> ContentDigest {
    match ContentDigest::parse(raw) {
        Ok(digest) => digest,
        Err(error) => panic!("fixture digest is invalid: {error}"),
    }
}
