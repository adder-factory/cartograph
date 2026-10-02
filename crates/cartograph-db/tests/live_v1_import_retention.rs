//! Live PostgreSQL integration coverage for Cartograph storage contracts.

mod dependency_ownership;

use std::assert_matches;
use std::{
    env, fs, process,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use cartograph_config::{DatabaseSchema, DatabaseSettings};
use cartograph_db::{
    CartographDatabase, GenerationContents, GenerationFacts, GenerationRetentionPolicy,
    GenerationRetentionRequest, GenerationValidationLimits, LeaseOwner, LeaseRequest, LeaseTarget,
    NewGeneration, RecoverableGeneration, SearchDocumentInput, V1PostgresImportCheckpoint,
    V1PostgresImportError, V1PostgresImportExecution, V1PostgresImportLimits,
    V1PostgresImportRequest, V1PostgresSource, V1PostgresSourceRevision, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, GenerationId, NormalizedPath, ProjectId,
    ProjectOperation, SourceManifestDigestBuilder, project_root_identity,
};
use cartograph_test_support::TestSchemaGuard;
use sha2::{Digest, Sha256};
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const PROJECT_FINGERPRINT: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const LEASE_DURATION: Duration = Duration::from_mins(1);
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const VALIDATION_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const VALIDATION_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const STALE_IMPORT_RETENTION_BATCH: u32 = 10;

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn v1_import_is_resumable_rollback_safe_and_compatible_with_bounded_retention() {
    let fixture = open_fixture().await;
    let primary_root = create_checkout();
    let primary_source = source_schema(&fixture.destination_schema, "v1a");
    create_v1_source(&fixture.pool, &primary_source, primary_root.path()).await;
    let primary_request = import_request(&primary_source, primary_root.path(), "primary-import");
    let imported = assert_resumable_primary_import(
        &fixture,
        &primary_source,
        &primary_request,
        primary_root.path(),
    )
    .await;
    let rollback_source = assert_rollback_safe_import(&fixture, &primary_source).await;
    assert_bounded_import_retention(&fixture, &imported, primary_request).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &primary_source).await;
    drop_schema(&fixture.pool, &rollback_source).await;
    drop_schema(&fixture.pool, &fixture.destination_schema).await;
    fixture.pool.close().await;
}

async fn assert_resumable_primary_import(
    fixture: &Fixture,
    primary_source: &str,
    primary_request: &V1PostgresImportRequest,
    primary_root: &std::path::Path,
) -> cartograph_db::V1PostgresImportReport {
    assert_unsupported_language_fails_preflight(fixture, primary_source, primary_request).await;

    assert_destination_absent(&fixture.pool, &fixture.destination_schema).await;
    let dry_run = match fixture
        .database
        .dry_run_v1_postgres_import(primary_request)
        .await
    {
        Ok(report) => report,
        Err(error) => panic!("v1 dry run failed: {error}"),
    };
    assert_destination_absent(&fixture.pool, &fixture.destination_schema).await;
    assert_eq!(dry_run.counts.files, 1);
    assert_eq!(dry_run.counts.symbols, 2);
    assert_eq!(dry_run.counts.edges, 1);
    assert_eq!(dry_run.counts.edge_sites, 1);
    assert_eq!(dry_run.counts.references, 2);
    assert_eq!(dry_run.counts.reference_sites, 2);
    assert_eq!(dry_run.counts.documents, 3);

    let staged = fixture
        .database
        .import_v1_postgres_with_observer(primary_request.clone(), |checkpoint| {
            checkpoint == V1PostgresImportCheckpoint::Staged
        })
        .await;
    assert_eq!(staged, Err(V1PostgresImportError::Interrupted));
    assert_import_state(fixture, "staging", "staged", 0).await;

    let ready = fixture
        .database
        .import_v1_postgres_with_observer(primary_request.clone(), |checkpoint| {
            checkpoint == V1PostgresImportCheckpoint::Bm25Rebuilt
        })
        .await;
    assert_eq!(ready, Err(V1PostgresImportError::Interrupted));
    assert_import_state(fixture, "ready", "bm25_rebuilt", 1).await;

    install_complete_checkpoint_rejection(fixture).await;
    let rejected_complete = fixture
        .database
        .import_v1_postgres(primary_request.clone())
        .await;
    assert_matches!(
        rejected_complete,
        Err(V1PostgresImportError::DatabaseOperation {
            operation: "append-checkpoint"
        })
    );
    assert_post_publish_checkpoint_failure(fixture).await;
    remove_complete_checkpoint_rejection(fixture).await;
    let imported = match fixture
        .database
        .import_v1_postgres(primary_request.clone())
        .await
    {
        Ok(report) => report,
        Err(error) => panic!("resumed v1 import failed: {error}"),
    };
    assert!(imported.resumed);
    assert_eq!(imported.checkpoint, V1PostgresImportCheckpoint::Complete);
    assert_uuid_v8(imported.generation_id.as_str());
    assert_published_import(fixture, &imported.generation_id).await;
    assert_runtime_identity_resolves_import(
        fixture,
        &imported.project_id,
        &imported.generation_id,
        primary_root,
    )
    .await;
    assert_no_migration_lease(fixture).await;
    assert_source_unchanged(fixture, primary_request, &dry_run).await;

    let repeated = match fixture
        .database
        .import_v1_postgres(primary_request.clone())
        .await
    {
        Ok(report) => report,
        Err(error) => panic!("idempotent v1 import failed: {error}"),
    };
    assert_eq!(repeated.generation_id, imported.generation_id);
    assert!(repeated.resumed);
    imported
}

async fn assert_rollback_safe_import(fixture: &Fixture, primary_source: &str) -> String {
    let rollback_root = create_checkout();
    let rollback_source = source_schema(&fixture.destination_schema, "v1b");
    create_v1_source(&fixture.pool, &rollback_source, rollback_root.path()).await;
    replace_v1_schema_history(&fixture.pool, &rollback_source, &[74, 75]).await;
    let rollback_request =
        import_request(&rollback_source, rollback_root.path(), "rollback-import");
    let rollback_dry_run = match fixture
        .database
        .dry_run_v1_postgres_import(&rollback_request)
        .await
    {
        Ok(report) => report,
        Err(error) => panic!("rollback dry run failed: {error}"),
    };
    let staged = fixture
        .database
        .import_v1_postgres_with_observer(rollback_request.clone(), |checkpoint| {
            checkpoint == V1PostgresImportCheckpoint::Staged
        })
        .await;
    assert_eq!(staged, Err(V1PostgresImportError::Interrupted));
    install_copy_rejection(fixture).await;
    let rejected = fixture
        .database
        .import_v1_postgres(rollback_request.clone())
        .await;
    assert_matches!(
        rejected,
        Err(V1PostgresImportError::DatabaseOperation {
            operation: "prepare-generation"
        })
    );
    assert_rollback_preserved_staging(fixture, &rollback_source).await;
    remove_copy_rejection(fixture).await;
    if let Err(error) = fixture
        .database
        .import_v1_postgres(rollback_request.clone())
        .await
    {
        panic!("rollback retry did not complete: {error}");
    }
    assert_source_unchanged(fixture, &rollback_request, &rollback_dry_run).await;
    assert_stable_legacy_symbol_id(fixture, primary_source, &rollback_source).await;
    rollback_source
}

async fn assert_bounded_import_retention(
    fixture: &Fixture,
    imported: &cartograph_db::V1PostgresImportReport,
    primary_request: V1PostgresImportRequest,
) {
    for sequence in 0_u32..5 {
        publish_document_generation(&fixture.database, &imported.project_id, sequence).await;
    }
    create_failed_generation(&fixture.database, &imported.project_id).await;
    let retention_lease = acquire_lease(
        &fixture.database,
        LeaseTarget::new(
            imported.project_id.clone(),
            ProjectOperation::Migration,
            None,
        ),
        "retention",
    )
    .await;
    let policy = match GenerationRetentionPolicy::new(2, 2) {
        Ok(policy) => policy,
        Err(error) => panic!("retention policy failed validation: {error}"),
    };
    let mut removed = 0_u64;
    loop {
        let report = match fixture
            .database
            .cleanup_generations(GenerationRetentionRequest::new(
                policy,
                &retention_lease.fence(),
                STATEMENT_TIMEOUT,
            ))
            .await
        {
            Ok(report) => report,
            Err(error) => panic!("retention cleanup failed: {error}"),
        };
        assert!(report.removed() <= 2);
        removed = removed.saturating_add(report.removed());
        if report.removed() == 0 {
            assert_eq!(report.current_preserved, 1);
            assert_eq!(report.superseded_preserved, 2);
            assert_eq!(report.failed_remaining, 0);
            break;
        }
    }
    assert_eq!(removed, 5);
    if let Err(error) = fixture.database.release_lease(&retention_lease).await {
        panic!("retention lease release failed: {error}");
    }
    for _ in 0..65 {
        create_failed_generation(&fixture.database, &imported.project_id).await;
    }
    let backlog_lease = acquire_lease(
        &fixture.database,
        LeaseTarget::new(
            imported.project_id.clone(),
            ProjectOperation::Migration,
            None,
        ),
        "retention-backlog",
    )
    .await;
    let backlog_policy = GenerationRetentionPolicy::new(2, 65)
        .unwrap_or_else(|error| panic!("backlog retention policy failed: {error}"));
    let backlog = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            backlog_policy,
            &backlog_lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("backlog retention cleanup failed: {error}"));
    assert_eq!(backlog.failed_removed, 65);
    assert_eq!(backlog.failed_remaining, 0);
    if let Err(error) = fixture.database.release_lease(&backlog_lease).await {
        panic!("backlog retention lease release failed: {error}");
    }
    assert_retained_storage_bound(fixture, &imported.project_id, &imported.generation_id).await;
    assert_expired_retention_fence_rolls_back(fixture, &imported.project_id).await;

    let retained_rerun = match fixture.database.import_v1_postgres(primary_request).await {
        Ok(report) => report,
        Err(error) => panic!("completed import did not survive historical retention: {error}"),
    };
    assert_eq!(retained_rerun.generation_id, imported.generation_id);
    assert!(retained_rerun.resumed);
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn v1_dry_run_accepts_bom_hash_provenance_and_tree_sitter_byte_columns() {
    let fixture = open_fixture().await;
    let root = create_checkout();
    let source = source_schema(&fixture.destination_schema, "bom");
    create_v1_source(&fixture.pool, &source, root.path()).await;
    install_bom_source(&fixture.pool, &source, root.path()).await;
    let report = fixture
        .database
        .dry_run_v1_postgres_import(&import_request(&source, root.path(), "bom-import"))
        .await
        .unwrap_or_else(|error| panic!("BOM v1 dry run failed: {error}"));
    assert_eq!(report.counts.files, 1);
    assert_eq!(report.counts.symbols, 2);
    assert_eq!(report.counts.documents, 3);

    drop(fixture.database);
    drop_schema(&fixture.pool, &source).await;
    drop_schema(&fixture.pool, &fixture.destination_schema).await;
    fixture.pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn v1_import_rejects_checkout_file_omission_before_destination_mutation() {
    let fixture = open_fixture().await;
    let root = create_checkout();
    let source = source_schema(&fixture.destination_schema, "omitted_file");
    create_v1_source(&fixture.pool, &source, root.path()).await;
    fs::write(root.path().join("extra.rs"), "pub fn extra() {}\n")
        .unwrap_or_else(|error| panic!("could not write omitted source fixture: {error}"));
    let expected_manifest = checkout_manifest_for_paths(root.path(), &["extra.rs", "sample.rs"]);
    let request = import_request_with_execution_and_manifest(
        &source,
        root.path(),
        "omitted-file",
        LEASE_DURATION,
        STATEMENT_TIMEOUT,
        expected_manifest,
    );

    assert_destination_absent(&fixture.pool, &fixture.destination_schema).await;
    assert_eq!(
        fixture.database.dry_run_v1_postgres_import(&request).await,
        Err(V1PostgresImportError::SourceManifestMismatch)
    );
    assert_destination_absent(&fixture.pool, &fixture.destination_schema).await;
    assert_eq!(
        fixture.database.import_v1_postgres(request).await,
        Err(V1PostgresImportError::SourceManifestMismatch)
    );
    assert_destination_absent(&fixture.pool, &fixture.destination_schema).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &source).await;
    drop_schema(&fixture.pool, &fixture.destination_schema).await;
    fixture.pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn expired_bm25_rebuild_fence_rolls_back_ddl_and_checkpoint() {
    let fixture = open_fixture().await;
    let root = create_checkout();
    let source = source_schema(&fixture.destination_schema, "bm25_expiry");
    create_v1_source(&fixture.pool, &source, root.path()).await;
    let request = import_request(&source, root.path(), "bm25-expiry-ready");
    let ready = fixture
        .database
        .import_v1_postgres_with_observer(request, |checkpoint| {
            checkpoint == V1PostgresImportCheckpoint::Ready
        })
        .await;
    assert_eq!(ready, Err(V1PostgresImportError::Interrupted));
    assert_import_state(&fixture, "ready", "ready", 1).await;
    let original_index = bm25_index_state(&fixture).await;

    let event_trigger = install_bm25_rebuild_delay(&fixture).await;
    let expiring_request = import_request_with_execution(
        &source,
        root.path(),
        "bm25-expiry-rebuild",
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    let result = fixture.database.import_v1_postgres(expiring_request).await;
    remove_bm25_rebuild_delay(&fixture, &event_trigger).await;

    assert_eq!(result, Err(V1PostgresImportError::LeaseFenceLost));
    assert_bm25_expiry_rolled_back(&fixture).await;
    let restored_index = bm25_index_state(&fixture).await;
    assert_eq!(restored_index, original_index);
    assert!(restored_index.1);

    let publish_request = import_request_with_execution(
        &source,
        root.path(),
        "publish-expiry",
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    let publish_result = fixture
        .database
        .import_v1_postgres_with_observer(publish_request, |checkpoint| {
            if checkpoint == V1PostgresImportCheckpoint::Bm25Rebuilt {
                std::thread::sleep(Duration::from_millis(1_250));
            }
            false
        })
        .await;
    assert_eq!(publish_result, Err(V1PostgresImportError::LeaseFenceLost));
    assert_import_state(&fixture, "ready", "bm25_rebuilt", 1).await;
    assert_generation_is_not_current(&fixture).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &source).await;
    drop_schema(&fixture.pool, &fixture.destination_schema).await;
    fixture.pool.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn import_recovers_after_an_intervening_project_publication() {
    let fixture = open_fixture().await;
    let root = create_checkout();
    let source = source_schema(&fixture.destination_schema, "stale_publish");
    create_v1_source(&fixture.pool, &source, root.path()).await;
    let request = import_request(&source, root.path(), "stale-publication");
    let staged = fixture
        .database
        .import_v1_postgres_with_observer(request.clone(), |checkpoint| {
            checkpoint == V1PostgresImportCheckpoint::Staged
        })
        .await;
    assert_eq!(staged, Err(V1PostgresImportError::Interrupted));

    let project_id = only_project_id(&fixture).await;
    publish_document_generation(&fixture.database, &project_id, 901).await;
    let stale = fixture.database.import_v1_postgres(request.clone()).await;
    assert_eq!(stale, Err(V1PostgresImportError::ConcurrentPublication));
    assert_import_state(&fixture, "failed", "bm25_rebuilt", 1).await;
    assert_no_migration_lease(&fixture).await;

    let retention_lease = acquire_lease(
        &fixture.database,
        LeaseTarget::new(project_id.clone(), ProjectOperation::Migration, None),
        "stale-import-retention",
    )
    .await;
    let retention_policy = GenerationRetentionPolicy::new(0, STALE_IMPORT_RETENTION_BATCH)
        .unwrap_or_else(|error| panic!("stale-import retention policy is invalid: {error}"));
    let retained = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            retention_policy,
            &retention_lease.fence(),
            STATEMENT_TIMEOUT,
        ))
        .await
        .unwrap_or_else(|error| panic!("stale-import retention failed: {error}"));
    assert_eq!(retained.removed(), 0);
    assert_eq!(retained.failed_remaining, 1);
    fixture
        .database
        .release_lease(&retention_lease)
        .await
        .unwrap_or_else(|error| panic!("stale-import retention lease did not release: {error}"));
    assert_import_state(&fixture, "failed", "bm25_rebuilt", 1).await;

    let recovered = fixture
        .database
        .import_v1_postgres(request)
        .await
        .unwrap_or_else(|error| panic!("stale import retry did not recover: {error}"));
    assert_eq!(recovered.checkpoint, V1PostgresImportCheckpoint::Complete);
    assert_published_import(&fixture, &recovered.generation_id).await;

    drop(fixture.database);
    drop_schema(&fixture.pool, &source).await;
    drop_schema(&fixture.pool, &fixture.destination_schema).await;
    fixture.pool.close().await;
}

struct Fixture {
    database: CartographDatabase,
    pool: sqlx_postgres::PgPool,
    destination_schema: String,
    _schema_guard: TestSchemaGuard,
}

async fn open_fixture() -> Fixture {
    let database_url = env::var(TEST_DATABASE_URL_ENV).unwrap_or_else(|error| {
        panic!("{TEST_DATABASE_URL_ENV} must be set for the ignored integration test: {error}")
    });
    let destination_schema = format!(
        "cartograph_cutover_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let settings = match DatabaseSettings::parse(&database_url, Some("4"), Some("10000"))
        .and_then(|settings| settings.with_schema(&destination_schema))
    {
        Ok(settings) => settings,
        Err(error) => panic!("test database settings failed validation: {error}"),
    };
    let pool = match cartograph_db::connect(&settings).await {
        Ok(pool) => pool,
        Err(error) => panic!("test database connection failed: {error}"),
    };
    Fixture {
        database: CartographDatabase::new(pool.clone(), settings.schema().clone()),
        pool,
        destination_schema,
        _schema_guard: TestSchemaGuard::new(database_url, settings.schema().as_str())
            .unwrap_or_else(|error| panic!("import schema guard failed: {error}")),
    }
}

fn source_schema(destination: &str, suffix: &str) -> String {
    format!("{destination}_{suffix}")
}

fn create_checkout() -> tempfile::TempDir {
    let directory = match tempfile::tempdir() {
        Ok(directory) => directory,
        Err(error) => panic!("could not create source checkout fixture: {error}"),
    };
    let source = "fn alpha() {}\nfn beta() { alpha(); }\n";
    if let Err(error) = fs::write(directory.path().join("sample.rs"), source) {
        panic!("could not write source checkout fixture: {error}");
    }
    directory
}

async fn create_v1_source(pool: &sqlx_postgres::PgPool, schema: &str, root: &std::path::Path) {
    let quoted = format!("\"{schema}\"");
    create_v1_tables(pool, &quoted).await;
    let source = match fs::read_to_string(root.join("sample.rs")) {
        Ok(source) => source,
        Err(error) => panic!("could not read source fixture: {error}"),
    };
    seed_v1_source_rows(pool, &quoted, &source).await;
}

async fn create_v1_tables(pool: &sqlx_postgres::PgPool, quoted: &str) {
    let statements = [
        format!("CREATE SCHEMA {quoted}"),
        format!(
            "CREATE TABLE {quoted}.schema_versions (version integer PRIMARY KEY, applied_at double precision NOT NULL, description text)"
        ),
        format!(
            "CREATE TABLE {quoted}.files (path text PRIMARY KEY, content_hash text NOT NULL, language text NOT NULL, size integer NOT NULL, errors text, node_count integer, is_test integer NOT NULL, needs_reextract integer NOT NULL)"
        ),
        format!(
            "CREATE TABLE {quoted}.nodes (id text PRIMARY KEY, name text NOT NULL, kind text NOT NULL, qualified_name text NOT NULL, file_path text NOT NULL, language text NOT NULL, start_line integer NOT NULL, end_line integer NOT NULL, start_column integer NOT NULL, end_column integer NOT NULL, docstring text, signature text, body_hash text NOT NULL, visibility text, is_exported integer, is_default_export integer, is_async integer, is_static integer)"
        ),
        format!(
            "CREATE TABLE {quoted}.edges (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, source text NOT NULL, target text NOT NULL, kind text NOT NULL, confidence text, line integer, col integer, metadata text)"
        ),
        format!(
            "CREATE TABLE {quoted}.unresolved_refs (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, from_node_id text NOT NULL, reference_name text NOT NULL, reference_kind text NOT NULL, line integer NOT NULL, col integer NOT NULL, file_path text NOT NULL, language text NOT NULL, candidates text, site_count integer NOT NULL, extra_lines text)"
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
            panic!("could not create v1 source schema: {error}");
        }
    }
    let version =
        format!("INSERT INTO {quoted}.schema_versions (version, applied_at) VALUES (75, 75.0)");
    if let Err(error) = query(AssertSqlSafe(version)).execute(pool).await {
        panic!("could not seed v1 schema history: {error}");
    }
}

async fn seed_v1_source_rows(pool: &sqlx_postgres::PgPool, quoted: &str, source: &str) {
    let file_hash = sha256_hex(source.as_bytes());
    let file_insert = format!(
        "INSERT INTO {quoted}.files (path, content_hash, language, size, errors, node_count, is_test, needs_reextract) VALUES ($1, $2, 'rust', $3, NULL, 2, 0, 0)"
    );
    if let Err(error) = query(AssertSqlSafe(file_insert))
        .bind("sample.rs")
        .bind(file_hash)
        .bind(i32_len(source.len(), "source byte length"))
        .execute(pool)
        .await
    {
        panic!("could not seed v1 file: {error}");
    }
    let mut lines = source.lines();
    let Some(alpha) = lines.next() else {
        panic!("source fixture lost alpha line");
    };
    let Some(beta) = lines.next() else {
        panic!("source fixture lost beta line");
    };
    insert_v1_node(
        pool,
        quoted,
        V1NodeFixture {
            id: "function:legacy-alpha",
            qualified_name: "alpha",
            line: 1,
            end_column: i32_len(alpha.len(), "alpha line length"),
            signature: "fn alpha()",
            body_hash: &v1_body_hash("fn alpha()", alpha),
        },
    )
    .await;
    insert_v1_node(
        pool,
        quoted,
        V1NodeFixture {
            id: "function:legacy-beta",
            qualified_name: "beta",
            line: 2,
            end_column: i32_len(beta.len(), "beta line length"),
            signature: "fn beta()",
            body_hash: &v1_body_hash("fn beta()", beta),
        },
    )
    .await;
    let reference_column = match beta.find("alpha") {
        Some(column) => i32_len(column, "reference column"),
        None => panic!("source fixture lost reference site"),
    };
    let edge_insert = format!(
        "INSERT INTO {quoted}.edges (source, target, kind, confidence, line, col, metadata) VALUES ($1, $2, 'calls', 'EXTRACTED', 2, $3, $4)"
    );
    if let Err(error) = query(AssertSqlSafe(edge_insert))
        .bind("function:legacy-beta")
        .bind("function:legacy-alpha")
        .bind(reference_column)
        .bind(r#"{"resolvedBy":"qualified-name","confidence":0.95}"#)
        .execute(pool)
        .await
    {
        panic!("could not seed v1 edge: {error}");
    }
    let reference_insert = format!(
        "INSERT INTO {quoted}.unresolved_refs (from_node_id, reference_name, reference_kind, line, col, file_path, language, candidates, site_count, extra_lines) VALUES ($1, 'alpha', 'call', 2, $2, 'sample.rs', 'rust', '[\"function:legacy-alpha\"]', 1, NULL)"
    );
    if let Err(error) = query(AssertSqlSafe(reference_insert))
        .bind("function:legacy-beta")
        .bind(reference_column)
        .execute(pool)
        .await
    {
        panic!("could not seed v1 reference: {error}");
    }
}

async fn replace_v1_schema_history(pool: &sqlx_postgres::PgPool, schema: &str, versions: &[i32]) {
    let quoted = format!("\"{schema}\"");
    let clear = format!("DELETE FROM {quoted}.schema_versions");
    if let Err(error) = query(AssertSqlSafe(clear)).execute(pool).await {
        panic!("could not clear v1 schema history: {error}");
    }
    let insert = format!(
        "INSERT INTO {quoted}.schema_versions (version, applied_at) VALUES ($1, $1::double precision)"
    );
    for version in versions {
        if let Err(error) = query(AssertSqlSafe(insert.clone()))
            .bind(*version)
            .execute(pool)
            .await
        {
            panic!("could not seed sparse v1 schema history: {error}");
        }
    }
}

async fn install_bom_source(pool: &sqlx_postgres::PgPool, schema: &str, root: &std::path::Path) {
    let alpha = "\u{feff}fn alpha() {}";
    let beta = "fn beta() { alpha(); }";
    let source = format!("{alpha}\n{beta}\n");
    fs::write(root.join("sample.rs"), &source)
        .unwrap_or_else(|error| panic!("could not write BOM checkout fixture: {error}"));
    let quoted = format!("\"{schema}\"");
    let update_file = format!(
        r"UPDATE {quoted}.files SET content_hash = $1, size = $2 WHERE path = 'sample.rs'",
    );
    query(AssertSqlSafe(update_file))
        .bind(sha256_hex(source.as_bytes()))
        .bind(i32_len(source.len(), "BOM source byte length"))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("could not update BOM file fixture: {error}"));
    let update_node = format!(
        r"UPDATE {quoted}.nodes
            SET start_column = $2, end_column = $3, body_hash = $4
            WHERE id = $1",
    );
    for (id, start_column, end_column, signature, body) in [
        (
            "function:legacy-alpha",
            3,
            i32_len(alpha.len(), "BOM alpha byte length"),
            "fn alpha()",
            alpha,
        ),
        (
            "function:legacy-beta",
            0,
            i32_len(beta.len(), "BOM beta byte length"),
            "fn beta()",
            beta,
        ),
    ] {
        query(AssertSqlSafe(update_node.clone()))
            .bind(id)
            .bind(start_column)
            .bind(end_column)
            .bind(v1_body_hash(signature, body))
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("could not update BOM node fixture: {error}"));
    }
}

struct V1NodeFixture<'a> {
    id: &'a str,
    qualified_name: &'a str,
    line: i32,
    end_column: i32,
    signature: &'a str,
    body_hash: &'a str,
}

async fn insert_v1_node(pool: &sqlx_postgres::PgPool, schema: &str, node: V1NodeFixture<'_>) {
    let statement = format!(
        "INSERT INTO {schema}.nodes (id, name, kind, qualified_name, file_path, language, start_line, end_line, start_column, end_column, docstring, signature, body_hash, visibility, is_exported, is_default_export, is_async, is_static) VALUES ($1, $2, 'function', $2, 'sample.rs', 'rust', $3, $3, 0, $4, $5, $6, $7, 'public', 1, 0, 0, 0)"
    );
    if let Err(error) = query(AssertSqlSafe(statement))
        .bind(node.id)
        .bind(node.qualified_name)
        .bind(node.line)
        .bind(node.end_column)
        .bind(format!("{} documentation", node.qualified_name))
        .bind(node.signature)
        .bind(node.body_hash)
        .execute(pool)
        .await
    {
        panic!("could not seed v1 node: {error}");
    }
}

fn import_request(
    source_schema: &str,
    project_root: &std::path::Path,
    owner: &str,
) -> V1PostgresImportRequest {
    import_request_with_execution(
        source_schema,
        project_root,
        owner,
        LEASE_DURATION,
        STATEMENT_TIMEOUT,
    )
}

fn import_request_with_execution(
    source_schema: &str,
    project_root: &std::path::Path,
    owner: &str,
    lease_duration: Duration,
    statement_timeout: Duration,
) -> V1PostgresImportRequest {
    import_request_with_execution_and_manifest(
        source_schema,
        project_root,
        owner,
        lease_duration,
        statement_timeout,
        checkout_manifest(project_root),
    )
}

fn import_request_with_execution_and_manifest(
    source_schema: &str,
    project_root: &std::path::Path,
    owner: &str,
    lease_duration: Duration,
    statement_timeout: Duration,
    source_manifest: ContentDigest,
) -> V1PostgresImportRequest {
    let source_schema = match DatabaseSchema::parse(source_schema) {
        Ok(schema) => schema,
        Err(error) => panic!("source schema fixture is invalid: {error}"),
    };
    let limits =
        match GenerationValidationLimits::new(VALIDATION_OUTPUT_BYTES, VALIDATION_WORKING_BYTES) {
            Ok(limits) => limits,
            Err(error) => panic!("validation limits are invalid: {error}"),
        };
    let limits = match V1PostgresImportLimits::new(10_000, 1024 * 1024, limits) {
        Ok(limits) => limits,
        Err(error) => panic!("import limits are invalid: {error}"),
    };
    V1PostgresImportRequest::new(
        V1PostgresSource::new(
            source_schema,
            project_root,
            V1PostgresSourceRevision::new(digest(PROJECT_FINGERPRINT), source_manifest),
        ),
        V1PostgresImportExecution::new(
            LeaseOwner::new(process::id(), owner),
            lease_duration,
            statement_timeout,
        ),
        limits,
    )
}

fn checkout_manifest(project_root: &std::path::Path) -> ContentDigest {
    checkout_manifest_for_paths(project_root, &["sample.rs"])
}

fn checkout_manifest_for_paths(project_root: &std::path::Path, paths: &[&str]) -> ContentDigest {
    let mut manifest = SourceManifestDigestBuilder::new(paths.len())
        .unwrap_or_else(|error| panic!("could not begin manifest fixture: {error}"));
    for raw_path in paths {
        let path = NormalizedPath::parse(raw_path)
            .unwrap_or_else(|error| panic!("manifest path was invalid: {error}"));
        let source = fs::read(project_root.join(path.as_str()))
            .unwrap_or_else(|error| panic!("could not read manifest fixture: {error}"));
        let content_hash = ContentDigest::from_bytes(*blake3::hash(&source).as_bytes());
        manifest
            .push(&path, &content_hash)
            .unwrap_or_else(|error| panic!("could not add manifest fixture: {error}"));
    }
    manifest
        .finish()
        .unwrap_or_else(|error| panic!("could not finish manifest fixture: {error}"))
}

async fn assert_destination_absent(pool: &sqlx_postgres::PgPool, schema: &str) {
    let row = match query("SELECT to_regnamespace($1) IS NULL")
        .bind(schema)
        .fetch_one(pool)
        .await
    {
        Ok(row) => row,
        Err(error) => panic!("could not inspect destination schema: {error}"),
    };
    assert!(read_bool(&row, 0, "destination absence"));
}

async fn only_project_id(fixture: &Fixture) -> ProjectId {
    let statement = format!(
        r#"SELECT project_id::text FROM "{}".projects"#,
        fixture.destination_schema
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not load importer project: {error}"));
    ProjectId::parse(&read_string(&row, 0, "project id"))
        .unwrap_or_else(|error| panic!("importer project id was invalid: {error}"))
}

async fn assert_unsupported_language_fails_preflight(
    fixture: &Fixture,
    source_schema: &str,
    request: &V1PostgresImportRequest,
) {
    let update = format!(r#"UPDATE "{source_schema}".files SET language = $1"#);
    query(AssertSqlSafe(update.clone()))
        .bind("unsupported-test-language")
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not install unsupported language: {error}"));
    let result = fixture.database.dry_run_v1_postgres_import(request).await;
    assert_eq!(
        result,
        Err(V1PostgresImportError::InvalidSourceData {
            field: "source_language"
        })
    );
    query(AssertSqlSafe(update.clone()))
        .bind("python")
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not install mismatched language: {error}"));
    let result = fixture.database.dry_run_v1_postgres_import(request).await;
    assert_eq!(
        result,
        Err(V1PostgresImportError::InvalidSourceData {
            field: "file_language"
        })
    );
    query(AssertSqlSafe(update))
        .bind("rust")
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not restore supported language: {error}"));
}

async fn assert_import_state(fixture: &Fixture, state: &str, checkpoint: &str, files: i64) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state, runs.checkpoint,
                  (SELECT count(*)::bigint FROM "{schema}".files) AS files
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id"#
    );
    let row = match query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
    {
        Ok(row) => row,
        Err(error) => panic!("could not inspect import state: {error}"),
    };
    assert_eq!(read_string(&row, 0, "generation state"), state);
    assert_eq!(read_string(&row, 1, "checkpoint"), checkpoint);
    assert_eq!(read_i64(&row, 2, "file count"), files);
}

async fn bm25_index_state(fixture: &Fixture) -> (i64, bool) {
    let statement = format!(
        r#"SELECT index_relations.oid::bigint,
                  indexes.indisvalid AND indexes.indisready
                  AND methods.amname IN ('paradedb', 'bm25')
            FROM "{}"."v1_import_runs" AS runs
            JOIN pg_namespace AS namespaces ON namespaces.nspname = $1
            JOIN pg_class AS tables
              ON tables.relnamespace = namespaces.oid
             AND tables.relname = 'search_g_' || replace(runs.generation_id::text, '-', '')
            JOIN pg_class AS index_relations
              ON index_relations.relnamespace = namespaces.oid
             AND index_relations.relname = tables.relname || '_bm25'
            JOIN pg_index AS indexes
              ON indexes.indexrelid = index_relations.oid
             AND indexes.indrelid = tables.oid
            JOIN pg_am AS methods ON methods.oid = index_relations.relam"#,
        fixture.destination_schema
    );
    let row = query(AssertSqlSafe(statement))
        .bind(&fixture.destination_schema)
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect BM25 index state: {error}"));
    (
        read_i64(&row, 0, "BM25 index oid"),
        read_bool(&row, 1, "BM25 index health"),
    )
}

async fn install_bm25_rebuild_delay(fixture: &Fixture) -> String {
    let schema = &fixture.destination_schema;
    let trigger = format!("{schema}_bm25_delay");
    let statements = [
        format!(
            r#"CREATE FUNCTION "{schema}".delay_bm25_rebuild()
                RETURNS event_trigger LANGUAGE plpgsql AS $body$
                BEGIN
                    IF TG_TAG = 'DROP TABLE'
                       AND strpos(current_query(), '{schema}') > 0
                       AND strpos(current_query(), 'search_g_') > 0 THEN
                        PERFORM pg_sleep(1.25);
                    END IF;
                END
                $body$"#,
        ),
        format!(
            r#"CREATE EVENT TRIGGER "{trigger}" ON ddl_command_start
                WHEN TAG IN ('DROP TABLE', 'CREATE INDEX')
                EXECUTE FUNCTION "{schema}".delay_bm25_rebuild()"#,
        ),
    ];
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not install BM25 rebuild delay: {error}"));
    }
    trigger
}

async fn remove_bm25_rebuild_delay(fixture: &Fixture, trigger: &str) {
    let schema = &fixture.destination_schema;
    let statements = [
        format!(r#"DROP EVENT TRIGGER "{trigger}""#),
        format!(r#"DROP FUNCTION "{schema}".delay_bm25_rebuild()"#),
    ];
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not remove BM25 rebuild delay: {error}"));
    }
}

async fn assert_bm25_expiry_rolled_back(fixture: &Fixture) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state, runs.checkpoint,
                  projects.current_generation_id IS NULL AS is_not_current,
                  (SELECT count(*)::bigint FROM "{schema}".v1_import_checkpoints
                    WHERE import_id = runs.import_id) AS checkpoint_count,
                  (SELECT count(*)::bigint FROM "{schema}".v1_import_checkpoints
                    WHERE import_id = runs.import_id
                      AND checkpoint IN ('bm25_rebuilt', 'complete')) AS terminal_checkpoints,
                  (SELECT count(*)::bigint FROM "{schema}".project_operation_leases
                    WHERE operation = 'migration'
                      AND expires_at <= clock_timestamp()) AS expired_migration_leases
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id
            JOIN "{schema}".projects AS projects
              ON projects.project_id = generations.project_id"#,
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect expired BM25 rebuild: {error}"));
    assert_eq!(
        read_string(&row, 0, "BM25 expiry generation state"),
        "ready"
    );
    assert_eq!(read_string(&row, 1, "BM25 expiry checkpoint"), "ready");
    assert!(read_bool(&row, 2, "BM25 expiry current generation"));
    assert_eq!(read_i64(&row, 3, "BM25 expiry checkpoint count"), 2);
    assert_eq!(read_i64(&row, 4, "BM25 expiry terminal checkpoints"), 0);
    assert_eq!(read_i64(&row, 5, "BM25 expiry migration leases"), 1);
}

async fn assert_generation_is_not_current(fixture: &Fixture) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state, projects.current_generation_id IS NULL,
                  (SELECT count(*)::bigint FROM "{schema}".v1_import_checkpoints
                    WHERE import_id = runs.import_id AND checkpoint = 'complete')
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id
            JOIN "{schema}".projects AS projects
              ON projects.project_id = generations.project_id"#,
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect pre-publish expiry: {error}"));
    assert_eq!(read_string(&row, 0, "pre-publish state"), "ready");
    assert!(read_bool(&row, 1, "pre-publish current generation"));
    assert_eq!(read_i64(&row, 2, "pre-publish complete checkpoints"), 0);
}

async fn assert_published_import(fixture: &Fixture, generation_id: &GenerationId) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state, runs.checkpoint,
                  (SELECT count(*)::bigint FROM "{schema}".v1_import_checkpoints
                    WHERE import_id = runs.import_id),
                  indexes.indisvalid AND indexes.indisready
                  AND methods.amname IN ('paradedb', 'bm25')
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id
            JOIN pg_namespace AS namespaces
              ON namespaces.nspname = $2
            JOIN pg_class AS tables
              ON tables.relnamespace = namespaces.oid
             AND tables.relname = 'search_g_'
                 || replace(runs.generation_id::text, '-', '')
            JOIN pg_class AS index_relations
              ON index_relations.relnamespace = namespaces.oid
             AND index_relations.relname = tables.relname || '_bm25'
            JOIN pg_index AS indexes
              ON indexes.indexrelid = index_relations.oid
             AND indexes.indrelid = tables.oid
            JOIN pg_am AS methods ON methods.oid = index_relations.relam
            WHERE runs.generation_id = CAST($1 AS uuid)"#
    );
    let row = match query(AssertSqlSafe(statement))
        .bind(generation_id.as_str())
        .bind(schema)
        .fetch_one(&fixture.pool)
        .await
    {
        Ok(row) => row,
        Err(error) => panic!("could not inspect completed import: {error}"),
    };
    assert_eq!(read_string(&row, 0, "generation state"), "current");
    assert_eq!(read_string(&row, 1, "checkpoint"), "complete");
    assert_eq!(read_i64(&row, 2, "checkpoint count"), 4);
    assert!(read_bool(&row, 3, "bm25 health"));
}

async fn assert_runtime_identity_resolves_import(
    fixture: &Fixture,
    project_id: &ProjectId,
    generation_id: &GenerationId,
    checkout: &std::path::Path,
) {
    let identity = project_root_identity(&digest(PROJECT_FINGERPRINT));
    let snapshot = fixture
        .database
        .project_snapshot_by_root(&identity)
        .await
        .unwrap_or_else(|error| panic!("runtime project lookup failed: {error}"))
        .unwrap_or_else(|| panic!("runtime project lookup did not resolve the import"));
    assert_eq!(&snapshot.project_id, project_id);
    assert_eq!(
        snapshot
            .current
            .as_ref()
            .map(|current| &current.generation_id),
        Some(generation_id)
    );
    assert_eq!(
        snapshot
            .current
            .as_ref()
            .map(|current| current.source_revision.as_str()),
        Some(checkout_manifest(checkout).as_str())
    );
    let statement = format!(
        r#"SELECT root_identity FROM "{}".projects WHERE project_id = CAST($1 AS uuid)"#,
        fixture.destination_schema
    );
    let row = query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect private project identity: {error}"));
    let stored = read_string(&row, 0, "root identity");
    assert_eq!(stored, identity);
    assert!(!stored.contains(&checkout.to_string_lossy().into_owned()));
}

async fn install_complete_checkpoint_rejection(fixture: &Fixture) {
    let schema = &fixture.destination_schema;
    let statements = [
        format!(
            r#"CREATE FUNCTION "{schema}".reject_complete_checkpoint()
                RETURNS trigger LANGUAGE plpgsql AS $body$
                BEGIN
                    IF NEW.checkpoint = 'complete' THEN
                        RAISE EXCEPTION 'injected complete checkpoint rejection';
                    END IF;
                    RETURN NEW;
                END
                $body$"#,
        ),
        format!(
            r#"CREATE TRIGGER reject_complete_checkpoint
                BEFORE INSERT ON "{schema}".v1_import_checkpoints
                FOR EACH ROW EXECUTE FUNCTION "{schema}".reject_complete_checkpoint()"#,
        ),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install complete-checkpoint rejection: {error}");
        }
    }
}

async fn remove_complete_checkpoint_rejection(fixture: &Fixture) {
    let schema = &fixture.destination_schema;
    let statements = [
        format!(r#"DROP TRIGGER reject_complete_checkpoint ON "{schema}".v1_import_checkpoints"#),
        format!(r#"DROP FUNCTION "{schema}".reject_complete_checkpoint()"#),
    ];
    for statement in statements {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not remove complete-checkpoint rejection: {error}");
        }
    }
}

async fn assert_post_publish_checkpoint_failure(fixture: &Fixture) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state, runs.checkpoint,
                  (SELECT count(*)::bigint FROM "{schema}".project_operation_leases
                    WHERE operation = 'migration') AS migration_leases
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id"#,
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect post-publish failure: {error}"));
    assert_eq!(read_string(&row, 0, "post-publish state"), "current");
    assert_eq!(
        read_string(&row, 1, "post-publish checkpoint"),
        "bm25_rebuilt"
    );
    assert_eq!(read_i64(&row, 2, "post-publish migration leases"), 0);
}

async fn assert_no_migration_lease(fixture: &Fixture) {
    let statement = format!(
        r#"SELECT count(*)::bigint FROM "{}".project_operation_leases
            WHERE operation = 'migration'"#,
        fixture.destination_schema
    );
    let row = query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect migration leases: {error}"));
    assert_eq!(read_i64(&row, 0, "migration lease count"), 0);
}

async fn assert_source_unchanged(
    fixture: &Fixture,
    request: &V1PostgresImportRequest,
    expected: &cartograph_db::V1PostgresDryRunReport,
) {
    let actual = match fixture.database.dry_run_v1_postgres_import(request).await {
        Ok(report) => report,
        Err(error) => panic!("source no longer validates after import attempt: {error}"),
    };
    assert_eq!(&actual, expected);
}

async fn install_copy_rejection(fixture: &Fixture) {
    let statement = format!(
        r#"ALTER TABLE "{}".files
            ADD CONSTRAINT import_copy_rejection CHECK (false) NOT VALID"#,
        fixture.destination_schema
    );
    if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
        panic!("could not install COPY rejection constraint: {error}");
    }
}

async fn remove_copy_rejection(fixture: &Fixture) {
    let statement = format!(
        r#"ALTER TABLE "{}".files DROP CONSTRAINT import_copy_rejection"#,
        fixture.destination_schema
    );
    if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
        panic!("could not remove COPY rejection constraint: {error}");
    }
}

async fn assert_rollback_preserved_staging(fixture: &Fixture, source_schema: &str) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT generations.state,
                  (SELECT count(*)::bigint FROM "{schema}".files AS files
                    WHERE files.generation_id = runs.generation_id)
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".index_generations AS generations
              ON generations.generation_id = runs.generation_id
            WHERE runs.source_schema = $1"#
    );
    let row = match query(AssertSqlSafe(statement))
        .bind(source_schema)
        .fetch_one(&fixture.pool)
        .await
    {
        Ok(row) => row,
        Err(error) => panic!("could not inspect rollback result: {error}"),
    };
    assert_eq!(read_string(&row, 0, "rollback state"), "staging");
    assert_eq!(read_i64(&row, 1, "rollback file count"), 0);
}

async fn assert_stable_legacy_symbol_id(fixture: &Fixture, first: &str, second: &str) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT runs.source_schema, min(symbols.symbol_id::text)
            FROM "{schema}".v1_import_runs AS runs
            JOIN "{schema}".symbols AS symbols
              ON symbols.generation_id = runs.generation_id
            WHERE runs.source_schema IN ($1, $2)
            GROUP BY runs.source_schema ORDER BY runs.source_schema"#
    );
    let rows = match query(AssertSqlSafe(statement))
        .bind(first)
        .bind(second)
        .fetch_all(&fixture.pool)
        .await
    {
        Ok(rows) => rows,
        Err(error) => panic!("could not inspect imported identities: {error}"),
    };
    assert_eq!(rows.len(), 2);
    let first_id = read_string(&rows[0], 1, "first symbol id");
    let second_id = read_string(&rows[1], 1, "second symbol id");
    assert_eq!(first_id, second_id);
    assert_uuid_v8(&first_id);
}

async fn publish_document_generation(
    database: &CartographDatabase,
    project_id: &ProjectId,
    sequence: u32,
) {
    let staged = match database
        .begin_generation(NewGeneration::new(
            project_id.clone(),
            format!("retention-{sequence}"),
            1,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("could not begin retention generation: {error}"),
    };
    let lease = acquire_lease(
        database,
        LeaseTarget::new(
            project_id.clone(),
            ProjectOperation::Index,
            Some(staged.generation_id().clone()),
        ),
        "retention-publication",
    )
    .await;
    let document_id = parse_document_id(&format!("00000000-0000-4000-8000-{sequence:012x}"));
    let limits = validation_limits();
    let facts = GenerationFacts {
        documents: vec![SearchDocumentInput {
            document_id,
            file_id: None,
            symbol_id: None,
            path: format!("src/revision_{sequence}.rs"),
            language: "rust".to_owned(),
            kind: DocumentKind::File,
            qualified_name: String::new(),
            code: format!("fn revision_{sequence}() {{}}"),
            natural_text: String::new(),
            metadata: serde_json::json!({"sequence": sequence}),
        }],
        ..GenerationFacts::default()
    };
    let canonical = match validate_generation_facts(facts, limits, || false) {
        Ok((facts, _)) => facts,
        Err(error) => panic!("retention generation facts are invalid: {error}"),
    };
    let ready = match database
        .prepare_generation(GenerationContents::new(staged, canonical), &lease.fence())
        .await
    {
        Ok(ready) => ready,
        Err(error) => panic!("could not prepare retention generation: {error}"),
    };
    if let Err(error) = database.publish_generation(ready, &lease.fence()).await {
        panic!("could not publish retention generation: {error}");
    }
}

async fn create_failed_generation(database: &CartographDatabase, project_id: &ProjectId) {
    let staged = match database
        .begin_generation(NewGeneration::new(
            project_id.clone(),
            "retention-failed",
            1,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("could not begin failed generation: {error}"),
    };
    let lease = acquire_lease(
        database,
        LeaseTarget::new(
            project_id.clone(),
            ProjectOperation::Index,
            Some(staged.generation_id().clone()),
        ),
        "retention-failure",
    )
    .await;
    if let Err(error) = database
        .fail_generation(RecoverableGeneration::Staged(staged), &lease.fence())
        .await
    {
        panic!("could not fail retention generation: {error}");
    }
    if let Err(error) = database.release_lease(&lease).await {
        panic!("could not release failed generation lease: {error}");
    }
}

async fn acquire_lease(
    database: &CartographDatabase,
    target: LeaseTarget,
    owner: &str,
) -> cartograph_db::ProjectLease {
    match database
        .acquire_lease(LeaseRequest::new(
            target,
            LeaseOwner::new(process::id(), owner),
            LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("could not acquire test lease: {error}"),
    }
}

async fn assert_retained_storage_bound(
    fixture: &Fixture,
    project_id: &ProjectId,
    imported_generation_id: &GenerationId,
) {
    let schema = &fixture.destination_schema;
    let statement = format!(
        r#"SELECT
            count(*)::bigint,
            count(*) FILTER (WHERE state = 'current')::bigint,
            count(*) FILTER (WHERE state = 'superseded')::bigint,
            count(*) FILTER (WHERE state = 'failed')::bigint,
            (SELECT count(*)::bigint FROM "{schema}".search_documents AS documents
                WHERE documents.project_id = CAST($1 AS uuid)),
            count(*) FILTER (WHERE generation_id = CAST($2 AS uuid))::bigint
        FROM "{schema}".index_generations
        WHERE project_id = CAST($1 AS uuid)"#
    );
    let row = match query(AssertSqlSafe(statement))
        .bind(project_id.as_str())
        .bind(imported_generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
    {
        Ok(row) => row,
        Err(error) => panic!("could not inspect retained storage bound: {error}"),
    };
    assert_eq!(read_i64(&row, 0, "generation total"), 3);
    assert_eq!(read_i64(&row, 1, "current count"), 1);
    assert_eq!(read_i64(&row, 2, "superseded count"), 2);
    assert_eq!(read_i64(&row, 3, "failed count"), 0);
    assert_eq!(read_i64(&row, 4, "document count"), 3);
    assert_eq!(read_i64(&row, 5, "import generation count"), 0);
}

async fn assert_expired_retention_fence_rolls_back(fixture: &Fixture, project_id: &ProjectId) {
    create_failed_generation(&fixture.database, project_id).await;
    let lease = acquire_lease(
        &fixture.database,
        LeaseTarget::new(project_id.clone(), ProjectOperation::Migration, None),
        "retention-expiry",
    )
    .await;
    let schema = &fixture.destination_schema;
    let statements = [
        format!(
            r#"CREATE FUNCTION "{schema}".delay_generation_delete()
                RETURNS trigger LANGUAGE plpgsql AS $body$
                BEGIN
                    PERFORM pg_sleep(0.25);
                    RETURN OLD;
                END
                $body$"#,
        ),
        format!(
            r#"CREATE TRIGGER delay_generation_delete
                BEFORE DELETE ON "{schema}".index_generations
                FOR EACH ROW EXECUTE FUNCTION "{schema}".delay_generation_delete()"#,
        ),
    ];
    for statement in statements {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not install retention delay: {error}"));
    }
    let expire = format!(
        r#"UPDATE "{schema}".project_operation_leases
            SET expires_at = clock_timestamp() + interval '100 milliseconds'
            WHERE project_id = CAST($1 AS uuid)
              AND operation = 'migration'
              AND lease_id = CAST($2 AS uuid)"#,
    );
    query(AssertSqlSafe(expire))
        .bind(project_id.as_str())
        .bind(lease.lease_id().as_str())
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not shorten retention lease: {error}"));
    let policy = GenerationRetentionPolicy::new(0, 1)
        .unwrap_or_else(|error| panic!("retention expiry policy was invalid: {error}"));
    let result = fixture
        .database
        .cleanup_generations(GenerationRetentionRequest::new(
            policy,
            &lease.fence(),
            Duration::from_secs(2),
        ))
        .await;
    assert_eq!(
        result,
        Err(cartograph_db::GenerationRetentionError::LeaseFenceLost)
    );
    let cleanup = [
        format!(r#"DROP TRIGGER delay_generation_delete ON "{schema}".index_generations"#),
        format!(r#"DROP FUNCTION "{schema}".delay_generation_delete()"#),
    ];
    for statement in cleanup {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("could not remove retention delay: {error}"));
    }
    let count = format!(
        r#"SELECT count(*)::bigint FROM "{schema}".index_generations
            WHERE project_id = CAST($1 AS uuid) AND state = 'failed'"#,
    );
    let row = query(AssertSqlSafe(count))
        .bind(project_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect rolled-back retention: {error}"));
    assert_eq!(read_i64(&row, 0, "failed generation after expiry"), 1);
}

fn validation_limits() -> GenerationValidationLimits {
    match GenerationValidationLimits::new(VALIDATION_OUTPUT_BYTES, VALIDATION_WORKING_BYTES) {
        Ok(limits) => limits,
        Err(error) => panic!("test validation limits are invalid: {error}"),
    }
}

fn digest(raw: &str) -> ContentDigest {
    match ContentDigest::parse(raw) {
        Ok(digest) => digest,
        Err(error) => panic!("test digest is invalid: {error}"),
    }
}

fn parse_document_id(raw: &str) -> DocumentId {
    match DocumentId::parse(raw) {
        Ok(id) => id,
        Err(error) => panic!("test document id is invalid: {error}"),
    }
}

fn assert_uuid_v8(raw: &str) {
    assert_eq!(raw.as_bytes().get(14), Some(&b'8'));
    assert_matches!(raw.as_bytes().get(19), Some(b'8' | b'9' | b'a' | b'b'));
}

fn v1_body_hash(signature: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(signature.as_bytes());
    hasher.update([0]);
    hasher.update(body.as_bytes());
    sha256_digest_hex(&hasher.finalize())[..32].to_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha256_digest_hex(&Sha256::digest(bytes))
}

fn sha256_digest_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn i32_len(value: usize, field: &str) -> i32 {
    i32::try_from(value)
        .unwrap_or_else(|error| panic!("{field} does not fit PostgreSQL integer: {error}"))
}

fn read_string(row: &sqlx_postgres::PgRow, index: usize, field: &str) -> String {
    match row.try_get::<String, _>(index) {
        Ok(value) => value,
        Err(error) => panic!("could not decode {field}: {error}"),
    }
}

fn read_i64(row: &sqlx_postgres::PgRow, index: usize, field: &str) -> i64 {
    match row.try_get::<i64, _>(index) {
        Ok(value) => value,
        Err(error) => panic!("could not decode {field}: {error}"),
    }
}

fn read_bool(row: &sqlx_postgres::PgRow, index: usize, field: &str) -> bool {
    match row.try_get::<bool, _>(index) {
        Ok(value) => value,
        Err(error) => panic!("could not decode {field}: {error}"),
    }
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
        panic!("failed to drop isolated schema: {error}");
    }
}
