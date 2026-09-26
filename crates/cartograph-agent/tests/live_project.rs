//! Integration coverage for Cartograph project-runtime and agent evidence contracts.

mod dependency_ownership;
#[path = "live_project/retention.rs"]
mod retention;
#[path = "live_project/rust_receivers.rs"]
mod rust_receivers;
#[path = "live_project/source_batches.rs"]
mod source_batches;

#[path = "live_project/scip_spill.rs"]
mod scip_spill;

#[path = "live_project/architecture_workload.rs"]
mod architecture_workload;

use std::{
    env,
    fmt::Write as _,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process, thread,
    time::{Duration, SystemTime},
};

use cartograph_agent::{
    DeadCodeJudgeOptions, DeadCodeJudgeRequest, DeadCodeVerdict, DiffReviewInput,
    DiffReviewOptions, EmbeddingClientRequest, EmbeddingOptions, FileDriftOptions,
    FileSourceOptions, FileSourceRequest, GenerationRetentionStatus, HistoryIndexOptions,
    ImportAuditError, ImportAuditOptions, ImportAuditRequest, ImportAuditSource, ImportAuditTarget,
    IndexOptions, IndexReport, LcovLoadOptions, NativeGenerationStorageMetrics,
    PipelineFailureReason, ProjectCancellation, ProjectError, ProjectRuntime, RenamePlanError,
    RenamePlanOptions, RenamePlanRequest, RetrievalClientRequest, RetrievalOptions,
    RetrievalRequest, ReviewOptions, ScipExportRequest, ScipImportLimits, ScipImportRequest,
    SourceContextOptions, SourceContextRequest, SourceSearchOptions, TestEvidenceOptions,
    WorkingTreeOverlayRequest, judge_dead_code_candidates,
};
use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CurrentGenerationLookup, DeadCodeQuery, ExactTextLookup, FileCochangeQuery,
    FileDependencyDirection, FileDependencyQuery, FileHistoryQuery, FileSurfaceQuery,
    FileTestImpactQuery, GroupedPathInput, GroupedSymbolQuery, InterchangeSnapshotRequest,
    IssueAttributionKind, IssueCommitSymbolPeerQuery, LeaseOwner, LeaseRequest, LeaseTarget,
    NativeParseCacheKey, NativeParseCacheKeyInput, NewGeneration, SearchQuery,
    SemanticStorageError, StructuralFindingGroupQuery, StructuralFindingQuery,
    StructuralFindingRefresh, StructuralFindingSeverity, StructuralHotspotCategory,
    StructuralHotspotQuery, StructuralHotspotSort, SymbolCoverageQuery, SymbolIssuePeerQuery,
    SymbolIssueQuery, latest_schema_version,
};
use cartograph_domain::{
    ContentDigest, EdgeKind, GenerationDigestVersion, GenerationState, ModelId, NormalizedPath,
    ProjectOperation, SourceLanguage, SymbolId,
};
use cartograph_extract::native_extractor_contract_digest;
use cartograph_llm::{ChatSettings, EmbeddingSettings, OpenAiChatClient, OpenAiEmbeddingClient};
use cartograph_scip::{decode_scip_index, encode_scip_index};
use cartograph_search::{
    DeterministicRetriever, EntryPointBucket, EntryPointsQuery, GraphPathRequest,
    GraphPathRequestInput, IndexFreshness, RetrievalConfidence, RetrievalError, RetrievalExecution,
    ReviewAbstention, SearchMode, SemanticReadiness, SimilarRequest, TraversalBudget,
    WorkingTreeOverlayStatus,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const SCORE_TOLERANCE: f64 = 1.0e-9;
const HISTORY_ANCHOR_PATH: &str = ".github/workflows/check.ts";
const HISTORY_PARTNER_PATH: &str = "ACKNOWLEDGEMENTS.ts";
const HISTORY_THIRD_PATH: &str = "src/c.ts";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn blocked_latest_migration_reports_exact_older_schema_versions() {
    let (schema, settings, project) = live_project_fixture("4");
    let runtime = ProjectRuntime::connect(project.path(), &settings)
        .await
        .unwrap_or_else(|error| panic!("initial schema migration failed: {error}"));
    runtime.close().await;

    let pool = cartograph_db::connect(&settings)
        .await
        .unwrap_or_else(|error| panic!("older-schema fixture connection failed: {error}"));
    let revert_latest_shape = format!(
        r#"ALTER TABLE "{schema}"."index_generations"
                DROP CONSTRAINT index_generations_run_excludes_check,
                DROP COLUMN run_excludes"#
    );
    query(AssertSqlSafe(revert_latest_shape))
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("could not restore the previous schema revision: {error}"));
    query(AssertSqlSafe(format!(
        r#"DELETE FROM "{schema}"."schema_migrations" WHERE version = $1"#
    )))
    .bind(latest_schema_version())
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("could not restore the previous migration ledger: {error}"));

    let mut lock_transaction = pool
        .begin()
        .await
        .unwrap_or_else(|error| panic!("could not begin migration blocker: {error}"));
    query(AssertSqlSafe(format!(
        r#"LOCK TABLE "{schema}"."index_generations" IN ACCESS EXCLUSIVE MODE"#
    )))
    .execute(&mut *lock_transaction)
    .await
    .unwrap_or_else(|error| panic!("could not lock the previous schema revision: {error}"));

    let blocked_settings = settings
        .clone()
        .with_query_timeout_ms(Some("250"))
        .unwrap_or_else(|error| panic!("could not bound the blocked migration: {error}"));
    let connection_result = ProjectRuntime::connect(project.path(), &blocked_settings).await;
    let expected_required = latest_schema_version();
    let expected_database = expected_required - 1;
    assert!(matches!(
        connection_result,
        Err(ProjectError::SchemaMigrationBlocked {
            database_schema_version,
            required_schema_version,
            pending_migration_version,
        }) if database_schema_version == expected_database
            && required_schema_version == expected_required
            && pending_migration_version == expected_required
    ));

    lock_transaction
        .rollback()
        .await
        .unwrap_or_else(|error| panic!("could not release migration blocker: {error}"));
    pool.close().await;
    drop_schema(&settings, &schema).await;
}

fn scores_match(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= SCORE_TOLERANCE
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn forced_postgres_generation_storage_publishes_and_reports_spill() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::create_dir_all(project.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("spill config directory failed: {error}"));
    std::fs::write(
        project.path().join(".cartograph/config.json"),
        r#"{"generationStorage":"postgres","maxSpillBytes":67108864,"maxSpillRows":1000000,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("spill config failed: {error}"));
    std::fs::write(
        project.path().join("service.ts"),
        "export function spillTarget(value: number): number { return value + 1; }\nexport function spillCaller(): number { return spillTarget(4); }\n",
    )
    .unwrap_or_else(|error| panic!("spill source fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("spill runtime connect failed: {error}"));
        let report = runtime
            .index(IndexOptions::default().with_history_refresh(false))
            .await
            .unwrap_or_else(|error| panic!("forced spill index failed: {error}"));
        assert!(report.published);
        let native = report
            .native
            .as_ref()
            .unwrap_or_else(|| panic!("forced spill metrics were missing"));
        assert_eq!(
            native.generation_storage,
            NativeGenerationStorageMetrics::Postgres
        );
        let spill = native
            .spill
            .unwrap_or_else(|| panic!("forced spill accounting was missing"));
        assert_eq!(spill.extracted_files, 1);
        assert!(spill.raw_rows > spill.extracted_files);
        assert!(spill.logical_bytes > 0);
        let exact = runtime
            .database()
            .exact_current_symbols_by_name(ExactTextLookup::new(
                CurrentGenerationLookup::new(&report.project_id, &report.generation_id),
                "spillTarget",
                10,
            ))
            .await
            .unwrap_or_else(|error| panic!("forced spill lookup failed: {error}"));
        assert_eq!(exact.len(), 1);
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn auto_storage_streams_many_crate_workspaces_before_memory_resolve_capacity() {
    const CARGO_MANIFESTS: usize = 64;
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::create_dir_all(project.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("auto-spill config directory failed: {error}"));
    std::fs::write(
        project.path().join(".cartograph/config.json"),
        r#"{"generationStorage":"auto","maxSpillBytes":67108864,"maxSpillRows":1000000,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("auto-spill config failed: {error}"));
    for index in 0..CARGO_MANIFESTS {
        let crate_root = project.path().join(format!("crates/member_{index}"));
        std::fs::create_dir_all(crate_root.join("src"))
            .unwrap_or_else(|error| panic!("member directory failed: {error}"));
        std::fs::write(
            crate_root.join("Cargo.toml"),
            format!("[package]\nname = \"member_{index}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap_or_else(|error| panic!("member manifest failed: {error}"));
        std::fs::write(
            crate_root.join("src/lib.rs"),
            format!("pub fn member_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap_or_else(|error| panic!("member source failed: {error}"));
    }

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("auto-spill runtime connect failed: {error}"));
        let report = runtime
            .index(IndexOptions::automatic())
            .await
            .unwrap_or_else(|error| panic!("auto-spill index failed: {error}"));
        let native = report
            .native
            .as_ref()
            .unwrap_or_else(|| panic!("auto-spill metrics were missing"));
        assert_eq!(
            native.generation_storage,
            NativeGenerationStorageMetrics::Postgres
        );
        assert_eq!(
            native
                .spill
                .unwrap_or_else(|| panic!("auto-spill accounting was missing"))
                .extracted_files,
            u64::try_from(CARGO_MANIFESTS * 2)
                .unwrap_or_else(|_| panic!("fixture file count overflowed"))
        );
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn automatic_capacity_failure_cleans_its_terminal_generation() {
    let (schema, settings, project) = live_project_fixture("9");
    std::fs::create_dir_all(project.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("capacity config directory failed: {error}"));
    std::fs::write(
        project.path().join(".cartograph/config.json"),
        r#"{"generationStorage":"postgres","maxGenerationBytes":1,"maxSpillBytes":67108864,"maxSpillRows":1000000,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("capacity config failed: {error}"));
    std::fs::write(
        project.path().join("dense.rs"),
        "pub fn capacity_fixture(value: usize) -> usize { value + 1 }\n",
    )
    .unwrap_or_else(|error| panic!("capacity source failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("capacity runtime connect failed: {error}"));
        let indexed = runtime.index(IndexOptions::automatic()).await;
        assert!(matches!(
            indexed,
            Err(ProjectError::IndexStageFailedWithReason {
                reason: PipelineFailureReason::GenerationCapacityExceeded,
                ..
            })
        ));
        let status = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("capacity status failed: {error}"));
        let storage = status.snapshot.as_ref().map_or_else(
            || panic!("capacity project snapshot was missing"),
            |snapshot| snapshot.generation_storage,
        );
        assert_eq!(storage.staging, 0);
        assert_eq!(storage.failed, 0);
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn run_excludes_define_freshness_drift_and_reconciliation_until_explicitly_replaced() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::write(
        project.path().join("service.rs"),
        "pub fn admitted_source() -> bool { true }\n",
    )
    .unwrap_or_else(|error| panic!("admitted source fixture failed: {error}"));
    std::fs::write(
        project.path().join("generated.rs"),
        "pub fn generated_source() -> usize { 1 }\n",
    )
    .unwrap_or_else(|error| panic!("excluded source fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("admission runtime connect failed: {error}"));
        let first = runtime
            .index(
                IndexOptions::default()
                    .with_history_refresh(false)
                    .with_additional_excludes(vec!["generated.rs".to_owned()]),
            )
            .await
            .unwrap_or_else(|error| panic!("admission index failed: {error}"));
        assert!(first.published);

        let current = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("admission status failed: {error}"));
        assert!(current.fresh);
        let snapshot = current
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.current.as_ref())
            .unwrap_or_else(|| panic!("admission generation was missing"));
        assert_eq!(snapshot.source_admission.run_exclude_patterns(), 1);
        let public = serde_json::to_string(snapshot)
            .unwrap_or_else(|error| panic!("admission serialization failed: {error}"));
        assert!(!public.contains("generated.rs"));

        std::fs::write(
            project.path().join("generated.rs"),
            "pub fn generated_source() -> usize { 2 }\n",
        )
        .unwrap_or_else(|error| panic!("excluded source rewrite failed: {error}"));
        assert!(
            runtime
                .status()
                .await
                .unwrap_or_else(|error| panic!("post-rewrite status failed: {error}"))
                .fresh
        );
        let drift = runtime
            .file_drift(FileDriftOptions::default(), ProjectCancellation::new())
            .await
            .unwrap_or_else(|error| panic!("admission drift failed: {error}"));
        let drift = serde_json::to_value(drift)
            .unwrap_or_else(|error| panic!("admission drift serialization failed: {error}"));
        assert_eq!(drift["addedCount"], 0);
        assert_eq!(drift["modifiedCount"], 0);
        assert_eq!(drift["deletedCount"], 0);

        let reconciled = runtime
            .index(IndexOptions::automatic())
            .await
            .unwrap_or_else(|error| panic!("admission reconciliation failed: {error}"));
        assert!(!reconciled.published);
        assert_eq!(reconciled.generation_id, first.generation_id);

        let replaced = runtime
            .index(IndexOptions::default().with_history_refresh(false))
            .await
            .unwrap_or_else(|error| panic!("admission replacement failed: {error}"));
        assert!(replaced.published);
        assert_ne!(replaced.generation_id, first.generation_id);
        let status = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("replacement status failed: {error}"));
        assert!(status.fresh);
        assert_eq!(
            status
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.current.as_ref())
                .map(|current| current.source_admission.run_exclude_patterns()),
            Some(0)
        );
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn empty_graph_memory_and_forced_postgres_paths_publish_identical_facts() {
    let (schema, settings, project) = live_project_fixture("8");
    let config_directory = project.path().join(".cartograph");
    std::fs::create_dir_all(&config_directory)
        .unwrap_or_else(|error| panic!("empty graph config directory failed: {error}"));
    let config_path = config_directory.join("config.json");
    std::fs::write(
        &config_path,
        r#"{"generationStorage":"memory","enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("empty memory config failed: {error}"));

    let memory = {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("empty memory runtime connect failed: {error}"));
        let report = runtime
            .index(IndexOptions::default().with_history_refresh(false))
            .await
            .unwrap_or_else(|error| panic!("empty memory index failed: {error}"));
        runtime.close().await;
        report
    };
    let memory_native = memory
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("empty memory metrics were missing"));
    assert_eq!(
        memory_native.generation_storage,
        NativeGenerationStorageMetrics::Memory
    );
    assert_eq!(memory_native.spill, None);

    std::fs::write(
        &config_path,
        r#"{"generationStorage":"postgres","maxSpillBytes":67108864,"maxSpillRows":1000000,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("empty spill config failed: {error}"));
    let spilled = {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("empty spill runtime connect failed: {error}"));
        let report = runtime
            .index(
                IndexOptions::default()
                    .with_force(true)
                    .with_history_refresh(false),
            )
            .await
            .unwrap_or_else(|error| panic!("empty forced spill index failed: {error}"));
        runtime.close().await;
        report
    };
    let spilled_native = spilled
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("empty spill metrics were missing"));
    assert_eq!(
        spilled_native.generation_storage,
        NativeGenerationStorageMetrics::Postgres
    );
    let spill = spilled_native
        .spill
        .unwrap_or_else(|| panic!("empty spill accounting was missing"));
    assert_eq!(spill.extracted_files, 0);
    assert_eq!(spill.raw_rows, 0);
    assert_eq!(spill.logical_bytes, 0);

    let logical_projection = |native: &cartograph_agent::NativeIndexMetrics| {
        (
            native.files,
            native.source_bytes,
            native.symbols,
            native.numerical_sites,
            native.resolved_references,
            native.unresolved_references,
            native.diagnostics,
        )
    };
    assert_eq!(logical_projection(memory_native), (0, 0, 0, 0, 0, 0, 0));
    assert_eq!(
        logical_projection(memory_native),
        logical_projection(spilled_native)
    );
    assert_eq!(memory.source_revision, spilled.source_revision);
    assert_eq!(memory.content_digest, spilled.content_digest);

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn independent_runtimes_terminalize_pre_lease_losers_and_bound_retention() {
    let (schema, settings, project) = live_project_fixture("12");
    std::fs::write(
        project.path().join("service.ts"),
        "export function boundedWatcher(): number { return 73; }\n",
    )
    .unwrap_or_else(|error| panic!("multi-runtime source fixture failed: {error}"));

    {
        let coordinator = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("coordinator runtime connect failed: {error}"));
        let project_id = coordinator
            .register_agent_state_project()
            .await
            .unwrap_or_else(|error| panic!("multi-runtime project registration failed: {error}"));
        let blocker = coordinator
            .database()
            .acquire_lease(LeaseRequest::new(
                LeaseTarget::new(project_id.clone(), ProjectOperation::Migration, None),
                LeaseOwner::new(process::id(), "multi-runtime-index-blocker"),
                Duration::from_mins(1),
            ))
            .await
            .unwrap_or_else(|error| panic!("multi-runtime blocker lease failed: {error}"));

        let options = IndexOptions::default()
            .with_force(true)
            .with_history_refresh(false);
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let runtime = ProjectRuntime::connect(project.path(), &settings)
                .await
                .unwrap_or_else(|error| panic!("contending runtime connect failed: {error}"));
            let options = options.clone();
            tasks.push(tokio::spawn(async move {
                let indexed = runtime.index(options).await;
                (runtime, indexed)
            }));
        }
        let mut contenders = Vec::new();
        for task in tasks {
            let (runtime, indexed) = task
                .await
                .unwrap_or_else(|error| panic!("contending runtime task failed: {error}"));
            assert_eq!(indexed, Err(ProjectError::IndexLeaseBusy));
            contenders.push(runtime);
        }
        let counts_sql = format!(
            r#"SELECT
                    count(*) FILTER (WHERE state = 'staging')::bigint AS staging,
                    count(*) FILTER (WHERE state = 'failed')::bigint AS failed
                FROM "{schema}"."index_generations"
                WHERE project_id = CAST($1 AS uuid)"#
        );
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("multi-runtime count connection failed: {error}"));
        let counts = query(AssertSqlSafe(counts_sql.clone()))
            .bind(project_id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("multi-runtime generation count failed: {error}"));
        assert_eq!(counts.try_get::<i64, _>("staging").ok(), Some(0));
        assert_eq!(counts.try_get::<i64, _>("failed").ok(), Some(4));

        coordinator
            .database()
            .release_lease(&blocker)
            .await
            .unwrap_or_else(|error| panic!("multi-runtime blocker lease did not release: {error}"));
        let published = coordinator
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("multi-runtime recovery index failed: {error}"));
        assert!(published.published);
        assert!(matches!(
            published.retention,
            GenerationRetentionStatus::Completed { report, .. }
                if report.failed_removed == 4 && report.staging_remaining == 0
        ));
        let status = coordinator
            .status()
            .await
            .unwrap_or_else(|error| panic!("multi-runtime status failed: {error}"));
        let storage = status.snapshot.as_ref().map_or_else(
            || panic!("multi-runtime storage summary was missing"),
            |snapshot| snapshot.generation_storage,
        );
        assert_eq!(storage.staging, 0);
        assert_eq!(storage.failed, 0);
        assert_eq!(storage.current, 1);
        assert!(storage.estimated_retained_bytes > 0);

        for runtime in contenders {
            runtime.close().await;
        }
        coordinator.close().await;
        pool.close().await;
    }

    drop_schema(&settings, &schema).await;
}

fn write_graph_policy_fixture(root: &Path) -> std::path::PathBuf {
    std::fs::create_dir(root.join(".cartograph"))
        .unwrap_or_else(|error| panic!("graph policy config directory failed: {error}"));
    let config = root.join(".cartograph/config.json");
    std::fs::write(
        &config,
        r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("disabled graph policy failed: {error}"));
    std::fs::write(
        root.join("policy.rs"),
        "/// Sensitive implementation notes.\npub fn target() {}\npub fn caller() { target(); }\n",
    )
    .unwrap_or_else(|error| panic!("graph policy fixture failed: {error}"));
    config
}

async fn assert_disabled_graph_policy(runtime: &ProjectRuntime) -> IndexReport {
    let report = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("disabled graph policy index failed: {error}"));
    assert!(report.published);
    let caller =
        exact_symbol_id(runtime, &report.project_id, &report.generation_id, "caller").await;
    let target =
        exact_symbol_id(runtime, &report.project_id, &report.generation_id, "target").await;
    let symbol_ids = [caller.clone(), target.clone()];
    let scores = runtime
        .database()
        .current_symbol_pagerank(&report.project_id, &report.generation_id, &symbol_ids)
        .await
        .unwrap_or_else(|error| panic!("disabled PageRank read failed: {error}"));
    assert!(scores.iter().all(|score| score.score.is_none()));
    let bridge_scores = runtime
        .database()
        .current_symbol_betweenness(&report.project_id, &report.generation_id, &symbol_ids)
        .await
        .unwrap_or_else(|error| panic!("disabled betweenness read failed: {error}"));
    assert!(bridge_scores.iter().all(|score| score.score.is_none()));
    assert_graph_policy_evidence(runtime, &report, true).await;
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let path = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: report.project_id.clone(),
                start: caller,
                target,
                budget: TraversalBudget::new(2, 10)
                    .unwrap_or_else(|error| panic!("graph policy budget failed: {error}")),
            })
            .with_edge_kind(EdgeKind::Calls),
        )
        .await
        .unwrap_or_else(|error| panic!("disabled-policy traversal failed: {error}"));
    assert_eq!(
        path.path().map(<[cartograph_search::GraphPathStep]>::len),
        Some(2)
    );
    report
}

async fn assert_enabled_graph_policy(runtime: &ProjectRuntime, disabled: &IndexReport) {
    let enabled = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("enabled graph policy index failed: {error}"));
    assert!(enabled.published);
    assert_ne!(enabled.generation_id, disabled.generation_id);
    let caller = exact_symbol_id(
        runtime,
        &enabled.project_id,
        &enabled.generation_id,
        "caller",
    )
    .await;
    let target = exact_symbol_id(
        runtime,
        &enabled.project_id,
        &enabled.generation_id,
        "target",
    )
    .await;
    assert!(
        runtime
            .database()
            .current_symbol_pagerank(
                &enabled.project_id,
                &enabled.generation_id,
                &[caller, target],
            )
            .await
            .unwrap_or_else(|error| panic!("enabled PageRank read failed: {error}"))
            .iter()
            .all(|score| score.score.is_some())
    );
    assert_graph_policy_evidence(runtime, &enabled, false).await;
}

async fn assert_graph_policy_evidence(
    runtime: &ProjectRuntime,
    report: &IndexReport,
    expected_empty: bool,
) {
    let references = runtime
        .database()
        .exact_current_references_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&report.project_id, &report.generation_id),
            "target",
            10,
        ))
        .await
        .unwrap_or_else(|error| panic!("graph policy call-site read failed: {error}"));
    assert_eq!(references.is_empty(), expected_empty);
    let docstrings = runtime
        .database()
        .search_current_intent(SearchQuery::new(
            CurrentGenerationLookup::new(&report.project_id, &report.generation_id),
            "sensitive implementation notes",
            10,
        ))
        .await
        .unwrap_or_else(|error| panic!("graph policy docstring search failed: {error}"));
    assert_eq!(docstrings.is_empty(), expected_empty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn incremental_sync_reparses_only_changed_or_corrupt_files_and_keeps_complete_graph() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = write_incremental_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("incremental runtime connect failed: {error}"));
        let options = IndexOptions::default()
            .with_max_workers(4)
            .unwrap_or_else(|error| panic!("incremental worker options failed: {error}"))
            .with_history_refresh(false);
        let first = initial_incremental_index(&runtime, options.clone()).await;
        let changed_contract = assert_incremental_cache_contract(&runtime, &first, &source).await;

        assert_incremental_contract_upgrade(
            &runtime,
            &settings,
            &schema,
            &first,
            &changed_contract,
            options.clone(),
        )
        .await;

        let recovered = assert_incremental_corruption_recovery(
            &runtime,
            &settings,
            &schema,
            &source,
            options.clone(),
        )
        .await;
        assert_forced_incremental_rebuild(&runtime, options, &recovered).await;
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn incremental_embedding_reuses_unchanged_generation_documents() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = project.path().join("src");
    std::fs::create_dir(&source)
        .unwrap_or_else(|error| panic!("embedding reuse source directory failed: {error}"));
    for (path, body) in [
        (
            "service.ts",
            "/** Adds one to the input. */\nexport function calculate(value: number): number { return value + 1; }\n",
        ),
        (
            "caller.ts",
            "import { calculate } from './service.js';\nexport function checkout(): number { return calculate(2); }\n",
        ),
        (
            "stable.ts",
            "export function stable(): boolean { return true; }\n",
        ),
    ] {
        std::fs::write(source.join(path), body)
            .unwrap_or_else(|error| panic!("embedding reuse fixture failed: {error}"));
    }

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("embedding reuse runtime connect failed: {error}"));
        let options = IndexOptions::default().with_history_refresh(false);
        let first = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("embedding reuse initial index failed: {error}"));
        assert!(first.published);

        let (endpoint, server) = embedding_fixture_server(4);
        let embedding_settings = EmbeddingSettings::new(&endpoint, "fixture-reuse-model", None)
            .unwrap_or_else(|error| panic!("embedding reuse settings failed: {error}"));
        let first_sweep = runtime
            .embed_current_with_client(EmbeddingClientRequest::new(
                OpenAiEmbeddingClient::new(embedding_settings.clone())
                    .unwrap_or_else(|error| panic!("embedding reuse client failed: {error}")),
                EmbeddingOptions::default(),
                ProjectCancellation::new(),
            ))
            .await
            .unwrap_or_else(|error| panic!("embedding reuse initial sweep failed: {error}"));
        assert_eq!(first_sweep.reused_documents(), 0);
        assert_eq!(
            first_sweep.endpoint_documents(),
            first_sweep.corpus_documents()
        );

        std::fs::write(
            source.join("service.ts"),
            "/** Adds two to the input. */\nexport function calculate(value: number): number { return value + 2; }\n",
        )
        .unwrap_or_else(|error| panic!("embedding reuse source edit failed: {error}"));
        let second = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("embedding reuse delta index failed: {error}"));
        assert!(second.published);
        assert_ne!(second.generation_id, first.generation_id);

        let second_sweep = runtime
            .embed_current_with_client(EmbeddingClientRequest::new(
                OpenAiEmbeddingClient::new(embedding_settings)
                    .unwrap_or_else(|error| panic!("embedding reuse client failed: {error}")),
                EmbeddingOptions::default(),
                ProjectCancellation::new(),
            ))
            .await
            .unwrap_or_else(|error| panic!("embedding reuse delta sweep failed: {error}"));
        assert!(second_sweep.reused_documents() > 0);
        assert!(second_sweep.endpoint_documents() > 0);
        assert!(second_sweep.endpoint_documents() < second_sweep.corpus_documents());
        assert_eq!(
            second_sweep
                .reused_documents()
                .saturating_add(second_sweep.endpoint_documents()),
            second_sweep.corpus_documents()
        );
        assert!(second_sweep.readiness().ready());
        server
            .join()
            .unwrap_or_else(|_| panic!("embedding reuse fixture server panicked"));
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn unchanged_index_terminalizes_all_abandoned_staging_generations() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::write(
        project.path().join("service.ts"),
        "export function stableSource(): boolean { return true; }\n",
    )
    .unwrap_or_else(|error| panic!("staging recovery fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("staging recovery runtime failed: {error}"));
        let options = IndexOptions::default().with_history_refresh(false);
        let first = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("staging recovery initial index failed: {error}"));
        let abandoned = runtime
            .database()
            .begin_generation(NewGeneration::new(
                first.project_id.clone(),
                first.source_revision.as_str(),
                1,
            ))
            .await
            .unwrap_or_else(|error| panic!("abandoned staging fixture failed: {error}"));
        let unchanged = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("staging recovery no-op index failed: {error}"));
        assert!(!unchanged.published);
        assert_eq!(unchanged.generation_id, first.generation_id);
        let recovered_state = runtime
            .database()
            .generation_state(&first.project_id, abandoned.generation_id())
            .await
            .unwrap_or_else(|error| panic!("staging recovery state failed: {error}"));
        assert!(
            matches!(recovered_state, None | Some(GenerationState::Failed)),
            "abandoned generation remained nonterminal: {recovered_state:?}"
        );
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

fn write_incremental_fixture(root: &Path) -> std::path::PathBuf {
    let source = root.join("src");
    std::fs::create_dir(&source)
        .unwrap_or_else(|error| panic!("incremental source directory failed: {error}"));
    std::fs::write(
        source.join("service.ts"),
        "export function calculateTotal(value: number): number { return value + 1; }\n",
    )
    .unwrap_or_else(|error| panic!("incremental service fixture failed: {error}"));
    std::fs::write(
        source.join("caller.ts"),
        "import { calculateTotal } from './service.js';\nexport function checkout(): number { return calculateTotal(2); }\n",
    )
    .unwrap_or_else(|error| panic!("incremental caller fixture failed: {error}"));
    std::fs::write(
        source.join("extra.ts"),
        "export function untouched(): boolean { return true; }\n",
    )
    .unwrap_or_else(|error| panic!("incremental extra fixture failed: {error}"));
    source
}

async fn initial_incremental_index(runtime: &ProjectRuntime, options: IndexOptions) -> IndexReport {
    let first = runtime
        .index(options)
        .await
        .unwrap_or_else(|error| panic!("initial incremental index failed: {error}"));
    let metrics = first
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("initial native metrics were missing"));
    assert_eq!(metrics.parse_cache.hits, 0);
    assert_eq!(metrics.parse_cache.misses, 3);
    assert_eq!(metrics.parse_cache.parsed_files, 3);
    assert_eq!(metrics.parse_cache.writes, 3);
    assert_complete_checkout_call(runtime, &first.project_id).await;
    first
}

async fn assert_incremental_cache_contract(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    source: &Path,
) -> ContentDigest {
    let service_bytes = std::fs::read(source.join("service.ts"))
        .unwrap_or_else(|error| panic!("service cache fixture read failed: {error}"));
    let service_path = NormalizedPath::parse("src/service.ts")
        .unwrap_or_else(|error| panic!("service cache path failed: {error}"));
    let service_hash = ContentDigest::from_bytes(*blake3::hash(&service_bytes).as_bytes());
    let key_input = |extractor_contract_digest, path, content_hash| NativeParseCacheKeyInput {
        project_id: first.project_id.clone(),
        extractor_contract_digest,
        path,
        language: SourceLanguage::TypeScript,
        content_hash,
        source_bytes: u64::try_from(service_bytes.len()).unwrap_or(u64::MAX),
    };
    let exact_key = NativeParseCacheKey::new(key_input(
        parse_cache_policy_digest(&native_extractor_contract_digest()),
        service_path.clone(),
        service_hash.clone(),
    ));
    assert!(
        runtime
            .database()
            .load_native_parse_cache(&exact_key)
            .await
            .unwrap_or_else(|error| panic!("exact cache lookup failed: {error}"))
            .is_some()
    );
    let changed_contract =
        ContentDigest::from_bytes(*blake3::hash(b"changed-extractor-contract").as_bytes());
    let changed_key = NativeParseCacheKey::new(key_input(
        changed_contract.clone(),
        service_path,
        service_hash,
    ));
    assert!(
        runtime
            .database()
            .load_native_parse_cache(&changed_key)
            .await
            .unwrap_or_else(|error| panic!("changed contract lookup failed: {error}"))
            .is_none()
    );
    changed_contract
}

fn parse_cache_policy_digest(extractor_contract: &ContentDigest) -> ContentDigest {
    let mut hasher =
        blake3::Hasher::new_derive_key("cartograph.v2.native-parse-cache-policy.2026-08-13");
    hasher.update(extractor_contract.as_str().as_bytes());
    hasher.update(
        &u64::try_from(cartograph_extract::DEFAULT_MAXIMUM_AST_DEPTH)
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

async fn assert_incremental_contract_upgrade(
    runtime: &ProjectRuntime,
    settings: &DatabaseSettings,
    schema: &str,
    first: &IndexReport,
    changed_contract: &ContentDigest,
    options: IndexOptions,
) {
    let unchanged = runtime
        .index(options.clone())
        .await
        .unwrap_or_else(|error| panic!("incremental no-op failed: {error}"));
    assert!(!unchanged.published);
    assert!(unchanged.native.is_none());

    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("contract-upgrade connection failed: {error}"));
    let downgrade_generation = format!(
        r#"UPDATE "{schema}"."index_generations"
           SET content_digest_version = $1
           WHERE project_id = CAST($2 AS uuid) AND state = 'current'"#
    );
    let downgraded = query(AssertSqlSafe(downgrade_generation))
        .bind(GenerationDigestVersion::V17.database_value())
        .bind(first.project_id.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|_| panic!("generation contract downgrade fixture failed"));
    assert_eq!(downgraded.rows_affected(), 1);
    let downgrade_parse_cache = format!(
        r#"UPDATE "{schema}"."native_parse_cache"
           SET extractor_contract_digest = $1
           WHERE project_id = CAST($2 AS uuid)"#
    );
    let downgraded_cache = query(AssertSqlSafe(downgrade_parse_cache))
        .bind(changed_contract.as_str())
        .bind(first.project_id.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|_| panic!("parse-cache contract downgrade fixture failed"));
    assert_eq!(downgraded_cache.rows_affected(), 3);
    pool.close().await;

    let stale = runtime
        .status()
        .await
        .unwrap_or_else(|error| panic!("stale-contract status failed: {error}"));
    assert!(!stale.fresh);
    let upgraded = runtime
        .index(options.clone())
        .await
        .unwrap_or_else(|error| panic!("contract-upgrade index failed: {error}"));
    assert!(upgraded.published);
    let metrics = upgraded
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("contract-upgrade native metrics were missing"));
    assert_eq!(metrics.parse_cache.hits, 0);
    assert_eq!(metrics.parse_cache.misses, 3);
    assert_eq!(metrics.parse_cache.parsed_files, 3);
    assert_eq!(metrics.parse_cache.writes, 3);
    let status = runtime
        .status()
        .await
        .unwrap_or_else(|error| panic!("upgraded-contract status failed: {error}"));
    assert!(status.fresh);
    assert!(matches!(
        status
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.current.as_ref()),
        Some(current) if current.digest_version == GenerationDigestVersion::CURRENT
    ));
    assert_eq!(first.source_revision, upgraded.source_revision);
    assert_ne!(first.generation_id, upgraded.generation_id);
    let unchanged = runtime
        .index(options)
        .await
        .unwrap_or_else(|error| panic!("upgraded-contract no-op failed: {error}"));
    assert!(!unchanged.published);
    assert_eq!(unchanged.generation_id, upgraded.generation_id);
}

async fn assert_incremental_corruption_recovery(
    runtime: &ProjectRuntime,
    settings: &DatabaseSettings,
    schema: &str,
    source: &Path,
    options: IndexOptions,
) -> IndexReport {
    std::fs::write(
        source.join("service.ts"),
        "export function calculateTotal(value: number): number { return value + 2; }\nexport function taxRate(): number { return 13; }\n",
    )
    .unwrap_or_else(|error| panic!("incremental service edit failed: {error}"));
    let changed = runtime
        .index(options.clone())
        .await
        .unwrap_or_else(|error| panic!("one-file incremental index failed: {error}"));
    let metrics = changed
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("one-file native metrics were missing"));
    assert_eq!(metrics.parse_cache.hits, 2);
    assert_eq!(metrics.parse_cache.misses, 1);
    assert_eq!(metrics.parse_cache.parsed_files, 1);
    assert_eq!(metrics.parse_cache.writes, 1);
    assert_complete_checkout_call(runtime, &changed.project_id).await;
    corrupt_incremental_cache(settings, schema, &changed).await;
    std::fs::write(
        source.join("extra.ts"),
        "export function untouched(): boolean { return false; }\n",
    )
    .unwrap_or_else(|error| panic!("incremental extra edit failed: {error}"));
    let recovered = runtime
        .index(options)
        .await
        .unwrap_or_else(|error| panic!("corrupt-cache recovery index failed: {error}"));
    let metrics = recovered
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("recovery native metrics were missing"));
    assert_eq!(metrics.parse_cache.hits, 1);
    assert_eq!(metrics.parse_cache.misses, 2);
    assert_eq!(metrics.parse_cache.parsed_files, 2);
    assert_eq!(metrics.parse_cache.corruptions, 1);
    assert_eq!(metrics.parse_cache.writes, 2);
    assert_eq!(metrics.parse_cache.read_errors, 0);
    assert_eq!(metrics.parse_cache.write_errors, 0);
    assert_complete_checkout_call(runtime, &recovered.project_id).await;
    recovered
}

async fn corrupt_incremental_cache(
    settings: &DatabaseSettings,
    schema: &str,
    changed: &IndexReport,
) {
    let payload = b"{";
    let digest = ContentDigest::from_bytes(*blake3::hash(payload).as_bytes());
    let statement = format!(
        r#"UPDATE "{schema}"."native_parse_cache"
           SET payload = $1, payload_digest = $2
           WHERE project_id = $3::uuid AND normalized_path = 'src/caller.ts'
             AND extractor_contract_digest = $4"#
    );
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("cache corruption connection failed: {error}"));
    let cache_contract = parse_cache_policy_digest(&native_extractor_contract_digest());
    let corrupted = query(AssertSqlSafe(statement))
        .bind(payload.as_slice())
        .bind(digest.as_str())
        .bind(changed.project_id.as_str())
        .bind(cache_contract.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|_| panic!("cache corruption fixture failed"));
    assert_eq!(corrupted.rows_affected(), 1);
    pool.close().await;
}

async fn assert_forced_incremental_rebuild(
    runtime: &ProjectRuntime,
    options: IndexOptions,
    recovered: &IndexReport,
) {
    let forced = runtime
        .index(options.with_force(true))
        .await
        .unwrap_or_else(|error| panic!("forced deterministic rebuild failed: {error}"));
    let metrics = forced
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("forced native metrics were missing"));
    assert_eq!(metrics.parse_cache.hits, 0);
    assert_eq!(metrics.parse_cache.misses, 0);
    assert_eq!(metrics.parse_cache.bypassed, 3);
    assert_eq!(metrics.parse_cache.parsed_files, 3);
    assert_eq!(forced.content_digest, recovered.content_digest);
}

async fn assert_complete_checkout_call(
    runtime: &ProjectRuntime,
    project_id: &cartograph_domain::ProjectId,
) {
    let snapshot = runtime
        .database()
        .current_interchange_snapshot(InterchangeSnapshotRequest {
            project_id,
            maximum_rows: 10_000,
            statement_timeout: Duration::from_secs(30),
        })
        .await
        .unwrap_or_else(|error| panic!("incremental graph snapshot failed: {error}"));
    let source_symbol = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "checkout")
        .unwrap_or_else(|| panic!("checkout symbol was missing"));
    let target_symbol = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "calculateTotal")
        .unwrap_or_else(|| panic!("calculateTotal symbol was missing"));
    assert!(snapshot.edges.iter().any(|edge| {
        edge.source_symbol_id == source_symbol.symbol_id
            && edge.target_symbol_id == target_symbol.symbol_id
            && edge.edge_kind == "calls"
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn fuzzy_name_and_parallel_source_search_use_live_paradedb_generation() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::write(
        project.path().join("auth.ts"),
        r#"/** Verify an incoming JWT signature before authorization. */
export function verifyJwtSignature(token: string): boolean {
  const issuer = process.env.JWT_ISSUER;
  const query = "SELECT id FROM auth_sessions WHERE issuer = $1";
  return issuer !== undefined && query.length > 0 && token.length > 0;
}
"#,
    )
    .unwrap_or_else(|error| panic!("find fixture write failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("find runtime connect failed: {error}"));
        let indexed = runtime
            .index(
                IndexOptions::default()
                    .with_max_workers(4)
                    .unwrap_or_else(|error| panic!("find worker options failed: {error}")),
            )
            .await
            .unwrap_or_else(|error| panic!("find index failed: {error}"));
        let fuzzy = runtime
            .database()
            .search_current_names_fuzzy(
                SearchQuery::new(
                    CurrentGenerationLookup::new(&indexed.project_id, &indexed.generation_id),
                    "verfyJwtSignatre",
                    10,
                ),
                2,
            )
            .await
            .unwrap_or_else(|error| panic!("ParadeDB fuzzy name search failed: {error}"));
        assert!(
            fuzzy
                .iter()
                .any(|hit| hit.qualified_name() == "verifyJwtSignature")
        );
        assert!(
            fuzzy
                .iter()
                .all(|hit| hit.components() == [cartograph_db::SearchComponent::QualifiedName])
        );
        let source = runtime
            .search_source(
                &indexed.project_id,
                SourceSearchOptions::new(r"process\.env\.JWT_ISSUER|FROM\s+auth_sessions", 10)
                    .unwrap_or_else(|error| panic!("source search options failed: {error}")),
                ProjectCancellation::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("parallel source search failed: {error}"));
        assert_eq!(source.hits().len(), 2);
        let source_json = serde_json::to_value(&source)
            .unwrap_or_else(|error| panic!("source search serialization failed: {error}"));
        assert_eq!(source_json["regexEngine"], "rust_regex_linear_time");
        assert!(source_json["hits"].as_array().is_some_and(|hits| {
            hits.iter()
                .all(|hit| hit["enclosingSymbol"]["qualifiedName"] == "verifyJwtSignature")
        }));
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn scip_export_and_persistent_partial_import_preserve_exact_graph_and_uncovered_files() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::create_dir(project.path().join("src"))
        .unwrap_or_else(|error| panic!("source directory failed: {error}"));
    std::fs::write(
        project.path().join("src/main.rs"),
        scip_spill::large_overlay_source(),
    )
    .unwrap_or_else(|error| panic!("main source failed: {error}"));
    std::fs::write(
        project.path().join("src/keep.rs"),
        "pub fn uncovered_file_symbol() {}\n",
    )
    .unwrap_or_else(|error| panic!("keep source failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("SCIP runtime connect failed: {error}"));
        let first = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("SCIP initial index failed: {error}"));
        let exported = runtime
            .export_scip_with_cancellation(
                ScipExportRequest::new("full.scip", 10_000)
                    .unwrap_or_else(|error| panic!("SCIP export request failed: {error}")),
                ProjectCancellation::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("SCIP export failed: {error}"));
        assert_eq!(exported.generation_id, first.generation_id);
        assert!(exported.stats.exact_typed_edges() > 0);
        let full_bytes = std::fs::read(project.path().join("full.scip"))
            .unwrap_or_else(|error| panic!("SCIP artifact read failed: {error}"));
        let mut partial = decode_scip_index(&full_bytes)
            .unwrap_or_else(|error| panic!("SCIP artifact decode failed: {error}"));
        partial
            .documents
            .retain(|document| document.relative_path == "src/main.rs");
        assert_eq!(partial.documents.len(), 1);
        let partial_bytes = encode_scip_index(&partial)
            .unwrap_or_else(|error| panic!("partial SCIP encode failed: {error}"));
        std::fs::write(project.path().join("partial.scip"), partial_bytes)
            .unwrap_or_else(|error| panic!("partial SCIP write failed: {error}"));

        let imported = runtime
            .import_scip_with_cancellation(
                ScipImportRequest::new(
                    "partial.scip",
                    ScipImportLimits::new(1024 * 1024, 10_000, 4)
                        .unwrap_or_else(|error| panic!("SCIP import limits failed: {error}")),
                )
                .unwrap_or_else(|error| panic!("SCIP import request failed: {error}")),
                ProjectCancellation::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("SCIP import failed: {error}"));
        assert!(imported.index.published);
        assert_ne!(imported.index.generation_id, first.generation_id);
        let overlay = imported
            .index
            .native
            .as_ref()
            .and_then(|native| native.scip_overlay)
            .unwrap_or_else(|| panic!("SCIP overlay metrics were missing"));
        assert_eq!(overlay.covered_documents, 1);
        assert!(overlay.exact_typed_edges > 0);
        let snapshot = runtime
            .database()
            .current_interchange_snapshot(InterchangeSnapshotRequest {
                project_id: &imported.index.project_id,
                maximum_rows: 10_000,
                statement_timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_or_else(|error| panic!("SCIP current snapshot failed: {error}"));
        assert_eq!(snapshot.files.len(), 2);
        assert!(
            snapshot
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "uncovered_file_symbol")
        );
        assert!(
            snapshot
                .edges
                .iter()
                .any(|edge| { edge.edge_kind == "calls" && edge.site_count == 1 })
        );
        let status = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("SCIP status failed: {error}"));
        assert!(status.fresh);
        scip_spill::assert_overlay_storage_parity(&runtime, project.path()).await;
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

fn write_auth_checkout_fixture(root: &Path) -> std::path::PathBuf {
    let source = root.join("auth.ts");
    std::fs::write(
        &source,
        "export function parseToken(token: string): boolean { return token.length > 0; }\n\
         export function verifyJwtSignature(token: string): boolean { return parseToken(token); }\n\
         export function authorizeRequest(token: string): boolean { return verifyJwtSignature(token); }\n",
    )
    .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "review@example.invalid"]);
    git(root, &["config", "user.name", "Review Fixture"]);
    git(root, &["add", "auth.ts"]);
    git(root, &["commit", "-m", "base"]);
    source
}

async fn initial_auth_index(runtime: &ProjectRuntime) -> IndexReport {
    let report = runtime
        .index(
            IndexOptions::default()
                .with_max_workers(4)
                .unwrap_or_else(|error| panic!("worker options failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("first index failed: {error}"));
    assert!(report.published);
    assert_eq!(report.native.as_ref().map(|native| native.files), Some(1));
    let status = runtime
        .status()
        .await
        .unwrap_or_else(|error| panic!("status failed: {error}"));
    assert!(status.fresh);
    assert_eq!(
        status
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.current.as_ref())
            .map(|current| current.generation_id.clone()),
        Some(report.generation_id.clone())
    );
    report
}

async fn assert_fresh_source_queries(runtime: &ProjectRuntime, first: &IndexReport) -> SymbolId {
    let hits = runtime
        .database()
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(&first.project_id, &first.generation_id),
            "verify jwt signature",
            5,
        ))
        .await
        .unwrap_or_else(|error| panic!("BM25 search failed: {error}"));
    assert_eq!(
        hits.first().map(cartograph_db::SearchHit::path),
        Some("auth.ts")
    );
    let symbol_id = hits
        .first()
        .and_then(|hit| hit.symbol_id())
        .cloned()
        .unwrap_or_else(|| panic!("BM25 fixture hit did not retain its symbol identity"));
    let context = runtime
        .source_context(SourceContextRequest::new(
            symbol_id.clone(),
            SourceContextOptions::default(),
        ))
        .await
        .unwrap_or_else(|error| panic!("fresh source context failed: {error}"));
    assert!(context.fresh());
    assert!(
        context
            .excerpt()
            .is_some_and(|excerpt| excerpt.text().contains("verifyJwtSignature"))
    );
    let file = runtime
        .file_source_with_cancellation(
            FileSourceRequest::new(
                NormalizedPath::parse("auth.ts")
                    .unwrap_or_else(|error| panic!("file source path failed: {error}")),
                FileSourceOptions::new(1, 1)
                    .unwrap_or_else(|error| panic!("file source options failed: {error}")),
            ),
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("fresh file source failed: {error}"));
    let file = serde_json::to_value(file)
        .unwrap_or_else(|error| panic!("file source serialization failed: {error}"));
    assert_eq!(file["fresh"], true);
    assert_eq!(file["excerpt"]["startLine"], 2);
    assert!(
        file["excerpt"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("verifyJwtSignature"))
    );
    symbol_id
}

async fn assert_coverage_evidence(runtime: &ProjectRuntime, first: &IndexReport, root: &Path) {
    let directory = root.join("coverage");
    std::fs::create_dir(&directory)
        .unwrap_or_else(|error| panic!("coverage directory failed: {error}"));
    std::fs::write(
        directory.join("lcov.info"),
        "SF:auth.ts\nDA:1,1\nDA:2,1\nDA:3,0\nend_of_record\n",
    )
    .unwrap_or_else(|error| panic!("coverage fixture write failed: {error}"));
    let report = runtime
        .load_lcov(
            LcovLoadOptions::new(vec![Path::new("coverage/lcov.info").to_path_buf()], "unit")
                .unwrap_or_else(|error| panic!("coverage options failed: {error}")),
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("LCOV ingest failed: {error}"));
    let report = serde_json::to_value(&report)
        .unwrap_or_else(|error| panic!("coverage report serialization failed: {error}"));
    assert_eq!(report["reports"], 1);
    assert_eq!(report["reportFiles"], 1);
    assert_eq!(report["matchedSymbols"], 3);
    let query = SymbolCoverageQuery::new(10)
        .and_then(|query| query.with_source(Some("unit")))
        .unwrap_or_else(|error| panic!("coverage query failed: {error}"));
    let rows = runtime
        .database()
        .current_symbol_coverage(&first.project_id, &query)
        .await
        .unwrap_or_else(|error| panic!("coverage ranking failed: {error}"));
    assert_eq!(rows.len(), 3);
    let rows = serde_json::to_value(&rows)
        .unwrap_or_else(|error| panic!("coverage rows serialization failed: {error}"));
    assert_eq!(rows[0]["coverageFraction"], 0.0);
    assert_eq!(rows[0]["qualifiedName"], "authorizeRequest");
    assert!(rows[0]["degreeCentrality"].is_number());
    let stats = runtime
        .database()
        .current_coverage_stats(&first.project_id, Some("unit"))
        .await
        .unwrap_or_else(|error| panic!("coverage stats failed: {error}"));
    let stats = serde_json::to_value(stats)
        .unwrap_or_else(|error| panic!("coverage stats serialization failed: {error}"));
    assert_eq!(stats["symbols"], 3);
    assert_eq!(stats["linesFound"], 3);
    assert_eq!(stats["linesHit"], 2);
    let sources = runtime
        .database()
        .coverage_sources(&first.project_id)
        .await
        .unwrap_or_else(|error| panic!("coverage sources failed: {error}"));
    assert_eq!(sources.len(), 1);
    std::fs::remove_dir_all(&directory)
        .unwrap_or_else(|error| panic!("coverage fixture cleanup failed: {error}"));
}

struct AuthSymbols {
    authorize: SymbolId,
    parse: SymbolId,
    verify: SymbolId,
}

async fn auth_symbols(runtime: &ProjectRuntime, first: &IndexReport) -> AuthSymbols {
    let authorize = exact_symbol_id(
        runtime,
        &first.project_id,
        &first.generation_id,
        "authorizeRequest",
    )
    .await;
    let parse = exact_symbol_id(
        runtime,
        &first.project_id,
        &first.generation_id,
        "parseToken",
    )
    .await;
    let verify = exact_symbol_id(
        runtime,
        &first.project_id,
        &first.generation_id,
        "verifyJwtSignature",
    )
    .await;
    AuthSymbols {
        authorize,
        parse,
        verify,
    }
}

async fn assert_auth_graph_scores(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    symbols: &AuthSymbols,
) {
    let symbol_ids = [
        symbols.authorize.clone(),
        symbols.verify.clone(),
        symbols.parse.clone(),
    ];
    let page_rank_scores = runtime
        .database()
        .current_symbol_pagerank(&first.project_id, &first.generation_id, &symbol_ids)
        .await
        .unwrap_or_else(|error| panic!("PageRank read failed: {error}"));
    let page_rank = |symbol| {
        page_rank_scores
            .iter()
            .find(|score| &score.symbol_id == symbol)
            .and_then(|score| score.score)
            .unwrap_or_else(|| panic!("symbol did not retain a PageRank score"))
    };
    assert!(page_rank(&symbols.parse) > page_rank(&symbols.verify));
    assert!(page_rank(&symbols.verify) > page_rank(&symbols.authorize));
    let bridge_scores = runtime
        .database()
        .current_symbol_betweenness(&first.project_id, &first.generation_id, &symbol_ids)
        .await
        .unwrap_or_else(|error| panic!("betweenness read failed: {error}"));
    let verify_score = bridge_scores
        .iter()
        .find(|score| score.symbol_id == symbols.verify)
        .and_then(|score| score.score)
        .unwrap_or_else(|| panic!("bridge symbol did not retain a score"));
    assert!(verify_score > 0.0);
    assert!(
        bridge_scores
            .iter()
            .filter(|score| score.symbol_id != symbols.verify)
            .all(|score| score.score == Some(0.0))
    );
}

async fn assert_auth_entry_points(runtime: &ProjectRuntime, first: &IndexReport) {
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let entry_points = retriever
        .entry_points(
            &first.project_id,
            EntryPointsQuery::new(20)
                .unwrap_or_else(|error| panic!("entry-point query failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("entry-point discovery failed: {error}"));
    assert_eq!(entry_points.generation_id(), Some(&first.generation_id));
    assert_eq!(entry_points.buckets().len(), EntryPointBucket::ALL.len());
    let public = entry_points
        .buckets()
        .iter()
        .find(|page| page.bucket() == EntryPointBucket::PublicExports)
        .unwrap_or_else(|| panic!("public-export entry-point page was missing"));
    assert_eq!(public.total(), 1);
    assert!(!public.truncated());
    assert_eq!(
        public
            .symbols()
            .first()
            .map(cartograph_db::CurrentSymbolRecord::qualified_name),
        Some("authorizeRequest")
    );
    assert!(
        entry_points.buckets()[..4]
            .iter()
            .all(|page| page.total() == 0)
    );
}

async fn assert_auth_call_path(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    symbols: &AuthSymbols,
) {
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let response = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: first.project_id.clone(),
                start: symbols.authorize.clone(),
                target: symbols.parse.clone(),
                budget: TraversalBudget::new(4, 20)
                    .unwrap_or_else(|error| panic!("graph path budget failed: {error}")),
            })
            .with_edge_kind(EdgeKind::Calls),
        )
        .await
        .unwrap_or_else(|error| panic!("calls path failed: {error}"));
    let steps = response
        .path()
        .unwrap_or_else(|| panic!("calls path did not find the fixture chain"));
    assert_eq!(steps.len(), 3);
    assert_eq!(
        steps
            .iter()
            .map(|step| step.symbol().qualified_name())
            .collect::<Vec<_>>(),
        vec!["authorizeRequest", "verifyJwtSignature", "parseToken"]
    );
    assert!(steps[0].via().is_none());
    assert!(
        steps[1..]
            .iter()
            .all(|step| step.via().is_some_and(|hop| hop.edge_kind() == "calls"))
    );
    assert!(!response.truncated());
}

async fn assert_auth_path_limits(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    symbols: &AuthSymbols,
) {
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let filtered = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: first.project_id.clone(),
                start: symbols.authorize.clone(),
                target: symbols.parse.clone(),
                budget: TraversalBudget::new(4, 20)
                    .unwrap_or_else(|error| panic!("filtered path budget failed: {error}")),
            })
            .with_edge_kind(EdgeKind::Imports),
        )
        .await
        .unwrap_or_else(|error| panic!("filtered path failed: {error}"));
    assert!(filtered.path().is_none());
    assert!(!filtered.truncated());
    let depth_limited = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: first.project_id.clone(),
                start: symbols.authorize.clone(),
                target: symbols.parse.clone(),
                budget: TraversalBudget::new(1, 20)
                    .unwrap_or_else(|error| panic!("depth path budget failed: {error}")),
            })
            .with_edge_kind(EdgeKind::Calls),
        )
        .await
        .unwrap_or_else(|error| panic!("depth-limited path failed: {error}"));
    assert!(depth_limited.path().is_none());
    assert!(depth_limited.truncated());
    let node_limited = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: first.project_id.clone(),
                start: symbols.authorize.clone(),
                target: symbols.parse.clone(),
                budget: TraversalBudget::new(4, 1)
                    .unwrap_or_else(|error| panic!("node path budget failed: {error}")),
            })
            .with_edge_kind(EdgeKind::Calls),
        )
        .await
        .unwrap_or_else(|error| panic!("node-limited path failed: {error}"));
    assert!(node_limited.path().is_none());
    assert!(node_limited.truncated());
}

async fn assert_primary_semantic_retrieval(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    symbol_id: &SymbolId,
) -> ModelId {
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let (endpoint, server) = embedding_fixture_server(3);
    let settings = EmbeddingSettings::new(&endpoint, "fixture-code-model", None)
        .unwrap_or_else(|error| panic!("embedding settings failed: {error}"));
    let client = OpenAiEmbeddingClient::new(settings)
        .unwrap_or_else(|error| panic!("embedding client failed: {error}"));
    let query_client = client.clone();
    let embedded = runtime
        .embed_current_with_client(EmbeddingClientRequest::new(
            client,
            EmbeddingOptions::default()
                .with_max_workers(4)
                .unwrap_or_else(|error| panic!("embedding options failed: {error}")),
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("embedding sweep failed: {error}"));
    assert!(embedded.readiness().ready());
    assert_eq!(embedded.readiness().documents(), 7);
    assert_eq!(embedded.readiness().embedded(), 7);
    let similar = retriever
        .similar(
            &SimilarRequest::new(first.project_id.clone(), symbol_id.clone(), 2)
                .and_then(|request| request.with_minimum_score(0.9))
                .unwrap_or_else(|error| panic!("similar request failed: {error}"))
                .with_same_language(true),
        )
        .await
        .unwrap_or_else(|error| panic!("stored-vector similar query failed: {error}"));
    assert_eq!(similar.source_symbol_id(), symbol_id);
    assert_eq!(similar.hits().len(), 2);
    assert!(
        similar
            .hits()
            .iter()
            .all(|hit| scores_match(hit.score(), 1.0) && hit.symbol().language() == "typescript")
    );
    assert!(
        similar
            .hits()
            .iter()
            .all(|hit| hit.symbol().symbol_id() != Some(symbol_id))
    );
    assert!(similar.truncated());
    let model_id = similar.model().model_id().clone();
    let hybrid = runtime
        .search_with_client(RetrievalClientRequest::new(
            RetrievalRequest::new(
                first.project_id.clone(),
                "verify jwt signature",
                RetrievalOptions::new(SearchMode::Auto, 5)
                    .unwrap_or_else(|error| panic!("retrieval options failed: {error}")),
            ),
            query_client,
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("hybrid search failed: {error}"));
    assert_eq!(hybrid.semantic_readiness(), SemanticReadiness::Ready);
    assert_eq!(hybrid.execution(), RetrievalExecution::Hybrid);
    assert!(!hybrid.items().is_empty());
    assert!(
        hybrid
            .items()
            .iter()
            .any(|item| item.contributions().len() == 2)
    );
    server
        .join()
        .unwrap_or_else(|_| panic!("embedding fixture server panicked"));
    model_id
}

async fn assert_semantic_model_selection(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    symbol_id: &SymbolId,
    primary_model_id: ModelId,
) {
    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let (endpoint, server) = embedding_fixture_server(2);
    let settings = EmbeddingSettings::new(&endpoint, "fixture-code-model-2", None)
        .unwrap_or_else(|error| panic!("second embedding settings failed: {error}"));
    let client = OpenAiEmbeddingClient::new(settings)
        .unwrap_or_else(|error| panic!("second embedding client failed: {error}"));
    let embedded = runtime
        .embed_current_with_client(EmbeddingClientRequest::new(
            client,
            EmbeddingOptions::default(),
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("second embedding sweep failed: {error}"));
    assert!(embedded.readiness().ready());
    server
        .join()
        .unwrap_or_else(|_| panic!("second embedding fixture server panicked"));
    let ambiguous = retriever
        .similar(
            &SimilarRequest::new(first.project_id.clone(), symbol_id.clone(), 2)
                .unwrap_or_else(|error| panic!("ambiguous similar request failed: {error}")),
        )
        .await;
    assert!(matches!(
        ambiguous,
        Err(RetrievalError::Semantic(
            SemanticStorageError::AmbiguousActiveModels
        ))
    ));
    let selected = retriever
        .similar(
            &SimilarRequest::new(first.project_id.clone(), symbol_id.clone(), 2)
                .unwrap_or_else(|error| panic!("selected similar request failed: {error}"))
                .with_model_id(primary_model_id),
        )
        .await
        .unwrap_or_else(|error| panic!("model-selected similar query failed: {error}"));
    assert_eq!(selected.hits().len(), 2);
}

async fn assert_auth_index_noop(runtime: &ProjectRuntime, first: &IndexReport) {
    let unchanged = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("unchanged index failed: {error}"));
    assert!(!unchanged.published);
    assert_eq!(unchanged.generation_id, first.generation_id);
}

async fn assert_stale_source_behavior(
    runtime: &ProjectRuntime,
    source: &Path,
    symbol_id: &SymbolId,
) {
    std::fs::write(
        source,
        "export function rejectExpiredJwt(token: string): boolean { return token.length === 0; }\n",
    )
    .unwrap_or_else(|error| panic!("fixture update failed: {error}"));
    let status = runtime
        .status()
        .await
        .unwrap_or_else(|error| panic!("stale status failed: {error}"));
    assert!(!status.fresh);
    let stale = runtime
        .source_context(SourceContextRequest::new(
            symbol_id.clone(),
            SourceContextOptions::default(),
        ))
        .await
        .unwrap_or_else(|error| panic!("stale source context failed: {error}"));
    assert!(!stale.fresh());
    assert!(stale.excerpt().is_none());
    let live = runtime
        .source_context(SourceContextRequest::new(
            symbol_id.clone(),
            SourceContextOptions::default().with_stale_live_source(true),
        ))
        .await
        .unwrap_or_else(|error| panic!("explicit live source context failed: {error}"));
    assert!(!live.fresh());
    assert!(live.live_source());
    assert!(
        live.excerpt()
            .is_some_and(|excerpt| excerpt.text().contains("rejectExpiredJwt"))
    );
    let overlay = runtime
        .working_tree_overlay(WorkingTreeOverlayRequest::new(
            "diagnose rejectExpiredJwt behavior",
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("working-tree overlay failed: {error}"));
    assert_eq!(overlay.status(), WorkingTreeOverlayStatus::Used);
    assert_eq!(
        overlay.files().first().map(|file| file.path().as_str()),
        Some("auth.ts")
    );
}

async fn assert_stale_review_behavior(runtime: &ProjectRuntime) -> ReviewOptions {
    let options =
        ReviewOptions::new("HEAD").unwrap_or_else(|error| panic!("review options failed: {error}"));
    let review = runtime
        .review(&options)
        .await
        .unwrap_or_else(|error| panic!("stale review failed: {error}"));
    assert!(review.comparison().worktree_dirty());
    assert_eq!(review.comparison().files().len(), 1);
    assert_eq!(review.packet().freshness(), IndexFreshness::Stale);
    assert_eq!(
        review.packet().abstention(),
        Some(ReviewAbstention::StaleIndex)
    );
    let supplied_diff = "diff --git a/auth.ts b/auth.ts\n--- a/auth.ts\n+++ b/auth.ts\n@@ -1,3 +1 @@\n-export function parseToken(token: string): boolean { return token.length > 0; }\n-export function verifyJwtSignature(token: string): boolean { return parseToken(token); }\n-export function authorizeRequest(token: string): boolean { return verifyJwtSignature(token); }\n+export function rejectExpiredJwt(token: string): boolean { return token.length === 0; }\n";
    let diff_review = runtime
        .review_diff_with_cancellation(DiffReviewInput::new(
            supplied_diff,
            DiffReviewOptions::default(),
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("supplied-diff review failed: {error}"));
    let diff_review = serde_json::to_value(diff_review)
        .unwrap_or_else(|error| panic!("supplied-diff review serialization failed: {error}"));
    assert_eq!(diff_review["comparison"]["files"][0]["path"], "auth.ts");
    assert_eq!(diff_review["comparison"]["addedLines"], 1);
    assert_eq!(diff_review["comparison"]["removedLines"], 3);
    assert_eq!(diff_review["packet"]["freshness"], "stale");
    assert!(
        diff_review["hunks"][0]["symbolIds"]
            .as_array()
            .is_some_and(|symbols| !symbols.is_empty())
    );
    assert!(
        diff_review["symbols"]
            .as_array()
            .is_some_and(|symbols| !symbols.is_empty())
    );
    options
}

async fn assert_auth_refresh(
    runtime: &ProjectRuntime,
    first: &IndexReport,
    review_options: &ReviewOptions,
) {
    let refreshed = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("refresh index failed: {error}"));
    assert!(refreshed.published);
    assert_ne!(refreshed.generation_id, first.generation_id);
    assert_ne!(refreshed.content_digest, first.content_digest);
    let status = runtime
        .status()
        .await
        .unwrap_or_else(|error| panic!("fresh status failed: {error}"));
    assert!(status.fresh);
    let review = runtime
        .review(review_options)
        .await
        .unwrap_or_else(|error| panic!("current review failed: {error}"));
    assert_eq!(review.packet().freshness(), IndexFreshness::Current);
    assert_eq!(review.packet().confidence(), RetrievalConfidence::High);
    assert_eq!(review.packet().abstention(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn fresh_checkout_indexes_searches_noops_and_atomically_refreshes() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = write_auth_checkout_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("runtime connect failed: {error}"));
        let first = initial_auth_index(&runtime).await;
        let symbol_id = assert_fresh_source_queries(&runtime, &first).await;
        assert_coverage_evidence(&runtime, &first, project.path()).await;
        let symbols = auth_symbols(&runtime, &first).await;
        assert_auth_graph_scores(&runtime, &first, &symbols).await;
        assert_auth_entry_points(&runtime, &first).await;
        assert_auth_call_path(&runtime, &first, &symbols).await;
        assert_auth_path_limits(&runtime, &first, &symbols).await;
        let primary_model = assert_primary_semantic_retrieval(&runtime, &first, &symbol_id).await;
        assert_semantic_model_selection(&runtime, &first, &symbol_id, primary_model).await;
        assert_auth_index_noop(&runtime, &first).await;
        assert_stale_source_behavior(&runtime, &source, &symbol_id).await;
        let review_options = assert_stale_review_behavior(&runtime).await;
        assert_auth_refresh(&runtime, &first, &review_options).await;
        runtime.close().await;
    }

    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn graph_analysis_and_evidence_flags_reindex_without_weakening_traversal() {
    let (schema, settings, project) = live_project_fixture("8");
    let config = write_graph_policy_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("graph policy runtime failed: {error}"));
        let disabled = assert_disabled_graph_policy(&runtime).await;

        std::fs::write(
            &config,
            r#"{"enableCentrality":true,"enableBetweenness":true,"extractDocstrings":true,"trackCallSites":true,"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
        )
        .unwrap_or_else(|error| panic!("enabled graph policy failed: {error}"));
        assert_enabled_graph_policy(&runtime, &disabled).await;
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

fn write_dependency_workspace_fixture(root: &Path) {
    for directory in [
        "packages/app",
        "packages/forbidden",
        "generated",
        ".cartograph",
        "node_modules/typescript",
    ] {
        std::fs::create_dir_all(root.join(directory))
            .unwrap_or_else(|error| panic!("dependency fixture directory failed: {error}"));
    }
    std::fs::write(
        root.join("package.json"),
        r#"{
          "name": "fixture-root",
          "private": true,
          "workspaces": ["packages/*"],
          "dependencies": {"zod": "1", "unused-runtime": "1"},
          "devDependencies": {"typescript": "1", "@types/node": "1", "unused-dev": "1"},
          "optionalDependencies": {"optional-pkg": "1"},
          "peerDependencies": {"react": "1"},
          "scripts": {"build": "tsc --noEmit", "test": "bun test"}
        }"#,
    )
    .unwrap_or_else(|error| panic!("root manifest write failed: {error}"));
    std::fs::write(
        root.join("packages/app/package.json"),
        r#"{"name":"@fixture/app","dependencies":{"left-pad":"1"}}"#,
    )
    .unwrap_or_else(|error| panic!("workspace manifest write failed: {error}"));
    std::fs::write(
        root.join(".cartograph/config.json"),
        r#"{
          "duplicateCodeAllowlist": ["generated/**"],
          "layers": [
            {"name":"app","paths":["packages/app/**"],"cannotImport":["forbidden"]},
            {"name":"forbidden","paths":["packages/forbidden/**"]}
          ]
        }"#,
    )
    .unwrap_or_else(|error| panic!("layer config write failed: {error}"));
    std::fs::write(
        root.join("packages/forbidden/secret.ts"),
        "export const forbidden = 'internal';\n",
    )
    .unwrap_or_else(|error| panic!("forbidden source write failed: {error}"));
    std::fs::write(
        root.join("node_modules/typescript/package.json"),
        r#"{"name":"typescript","bin":{"tsc":"bin/tsc"}}"#,
    )
    .unwrap_or_else(|error| panic!("bin manifest write failed: {error}"));
}

fn write_dependency_source_fixture(root: &Path) {
    std::fs::write(
        root.join("packages/app/index.ts"),
        "import { z } from 'zod';\n\
         import leftPad from 'left-pad';\n\
         import { describe } from 'vitest';\n\
         import missing from 'undeclared-pkg';\n\
         import { forbidden } from '../forbidden/secret';\n\
         export async function run(input: string) {\n\
           const optional = await import('optional-pkg');\n\
           describe(input, () => undefined);\n\
           return [z.string().parse(input), leftPad(input, 2), missing, optional, forbidden];\n\
         }\n\
         export function risky(a: string, b: string, c: string, d: string, e: string) {\n\
           const first = eval(a);\n\
           const second = b + c;\n\
           const third = d + e;\n\
           return first + second + third;\n\
         }\n\
         /** Default retry count is 3. */\n\
         export const RETRY_COUNT = 5;\n\
         export function authenticate(apiKey: string, password: string) {\n\
           const endpoint = 'https://api.example.com/v1';\n\
           const weights = [3, 4, 5, 6, 8];\n\
           const signingKey = process.env.SECRET_KEY;\n\
           return sign(endpoint, signingKey, apiKey, password, weights);\n\
         }\n\
         export function firstClone(input: number) {\n\
           const offset = input + 2;\n\
           const doubled = offset * 3;\n\
           const bounded = Math.max(doubled, 1);\n\
           const normalized = bounded / 2;\n\
           if (normalized > 9) {\n\
             const adjusted = normalized - 4;\n\
             return adjusted * 2;\n\
           }\n\
           const fallback = normalized + 5;\n\
           return fallback * 3;\n\
         }\n\
         export function secondClone(value: number) {\n\
           const delta = value + 6;\n\
           const scaled = delta * 7;\n\
           const clamped = Math.max(scaled, 5);\n\
           const ratio = clamped / 4;\n\
           if (ratio > 21) {\n\
             const reduced = ratio - 8;\n\
             return reduced * 6;\n\
           }\n\
           const alternative = ratio + 11;\n\
           return alternative * 7;\n\
         }\n\
         function abandonedTask() { return 'do-not-send-secret'; }\n",
    )
    .unwrap_or_else(|error| panic!("dependency source write failed: {error}"));
}

fn write_dependency_clone_fixtures(root: &Path) {
    std::fs::write(
        root.join("partial.ts"),
        r"export function partialAlpha(input: number, limit: number): number {
  log(input);
  const total = input + 1;
  if (total > limit) {
    save(total);
  }





  return total;
}
export function partialBeta(input: number, limit: number): number {
  const total = input + 1;
  if (total > limit) {
    save(total);
  }
  log(input);





  return total;
}
",
    )
    .unwrap_or_else(|error| panic!("partial clone source write failed: {error}"));
    let deliberate_copy = r"export function deliberateCopy(value: number): number {
  const first = value + 1;
  const second = first * 2;
  const third = second - 3;
  const fourth = third / 4;
  return fourth + 5;
}
";
    std::fs::write(root.join("deliberate.ts"), deliberate_copy)
        .unwrap_or_else(|error| panic!("deliberate clone source write failed: {error}"));
    std::fs::write(root.join("generated/copy.ts"), deliberate_copy)
        .unwrap_or_else(|error| panic!("allowlisted clone source write failed: {error}"));

    std::fs::write(
        root.join("presentation.tsx"),
        r#"export function SetupHelp() {
  return <>
    <input placeholder="https://portal.example.test" />
    <a href="https://docs.example.test/setup">Setup guide</a>
    <script src="https://static.example.test/widget.js" async />
  </>;
}
"#,
    )
    .unwrap_or_else(|error| panic!("presentation URL fixture write failed: {error}"));
    std::fs::write(
        root.join("serial-loops.ts"),
        r#"export async function parallelCandidate(items: Item[]) {
  for (const item of items) {
    await consume(item);
  }
}

export async function replay(events: Event[]) {
  let revision = await currentRevision();
  for (const event of events) {
    revision = await applyEvent(event, revision);
  }
  return revision;
}

export async function drain(records: Record[]) {
  for (const record of records) {
    const outcome = await deliver(record);
    if (outcome === "stop") break;
    await acknowledge(record);
  }
}
"#,
    )
    .unwrap_or_else(|error| panic!("serial loop fixture write failed: {error}"));
    write_facade_fixture(root);
    write_biomarker_precision_fixtures(root);
    write_cross_domain_clone_fixtures(root);
}

fn write_facade_fixture(root: &Path) {
    let mut facade = String::from("export function recordsRepository(db: unknown) {\n");
    for index in 0..30 {
        writeln!(
            facade,
            "  const operation{index} = (input: unknown) => focused{index}(db, input);"
        )
        .unwrap_or_else(|error| panic!("facade delegate formatting failed: {error}"));
    }
    facade.push_str("  return {\n");
    for index in 0..30 {
        writeln!(facade, "    operation{index},")
            .unwrap_or_else(|error| panic!("facade return formatting failed: {error}"));
    }
    facade.push_str("  };\n}\n\nexport function mixedOrchestrator(input: unknown) {\n");
    for index in 0..30 {
        writeln!(facade, "  focused{index}(input);")
            .unwrap_or_else(|error| panic!("orchestrator formatting failed: {error}"));
    }
    facade.push_str("}\n");
    for index in 0..30 {
        writeln!(
            facade,
            "function focused{index}(..._values: unknown[]) {{ return {index}; }}"
        )
        .unwrap_or_else(|error| panic!("focused operation formatting failed: {error}"));
    }
    std::fs::write(root.join("facade.ts"), facade)
        .unwrap_or_else(|error| panic!("facade fixture write failed: {error}"));
}

fn write_biomarker_precision_fixtures(root: &Path) {
    write_biomarker_precision_typescript_fixture(root);
    write_biomarker_graph_and_framework_fixtures(root);
    write_biomarker_python_fixture(root);
    write_biomarker_import_fixtures(root);
}

fn write_biomarker_precision_typescript_fixture(root: &Path) {
    let mut typescript = String::from(
        "function consume(_value: string): void {}\n\
         export function StaticMarkup({ name }: { name: string }) {\n\
           return <section>\n",
    );
    for _ in 0..150 {
        typescript.push_str("    <span>Static presentation</span>\n");
    }
    typescript.push_str("    <span>{name.trim()}</span>\n  </section>;\n}\n");
    typescript.push_str("export function ImperativeLong(value: string) {\n");
    for _ in 0..110 {
        typescript.push_str("  consume(value);\n");
    }
    typescript.push_str(
        "}\n\
         export function DeleteLabel({ name }: { name: string }) {\n\
           const label = `Delete ${name}`;\n\
           return <button aria-label={label}>Delete</button>;\n\
         }\n\
         export function presentationUpdate(name: string) {\n\
           const label = `Update ${name}`;\n\
           return <span>{label}</span>;\n\
         }\n\
         export function dynamicSelect(id: string) {\n\
           const query = `SELECT value FROM records WHERE id = ${id}`;\n\
           consume(id);\n\
           return query;\n\
         }\n\
         export function dynamicInsert(id: string) {\n\
           const query = `INSERT INTO records (id) VALUES (${id})`;\n\
           consume(id);\n\
           return query;\n\
         }\n\
         export function dynamicUpdate(id: string) {\n\
           const query = \"UPDATE records SET active = true WHERE id = \" + id;\n\
           consume(id);\n\
           return query;\n\
         }\n\
         export function dynamicDelete(id: string) {\n\
           const query = \"DELETE FROM records WHERE id = \" + id;\n\
           consume(id);\n\
           return query;\n\
         }\n\
         /** A gap at 0 is actionable; the example shows 11 pm-7 am. */\n\
         export const ILLUSTRATIVE_COLUMNS = 2;\n\
         /** The default retry limit is 3. */\n\
         export const EXPLICIT_RETRY_LIMIT = 5;\n\
         export function safeSign(payload: string, secretKey: string) {\n\
           return sign(payload, secretKey);\n\
         }\n\
         export function exposeSecret(secretKey: string) {\n\
           console.log(secretKey);\n\
         }\n\
         export function ordinaryFive(a: string, b: string, c: string, d: string, e: string) {\n\
           return [a, b, c, d, e].join(':');\n\
         }\n\
         export function explicitlyUnused(): string {\n\
           return 'unused';\n\
         }\n",
    );
    std::fs::write(root.join("precision.tsx"), typescript)
        .unwrap_or_else(|error| panic!("precision TypeScript fixture write failed: {error}"));
}

fn write_biomarker_graph_and_framework_fixtures(root: &Path) {
    let mut contained = String::from("export function containedCoordinator(value: string) {\n");
    for index in 0..30 {
        writeln!(
            contained,
            "  function localStep{index}() {{ return value; }}"
        )
        .unwrap_or_else(|error| panic!("contained helper formatting failed: {error}"));
    }
    for index in 0..30 {
        writeln!(contained, "  localStep{index}();")
            .unwrap_or_else(|error| panic!("contained call formatting failed: {error}"));
    }
    contained.push_str("  return value;\n}\n");
    std::fs::write(root.join("contained.ts"), contained)
        .unwrap_or_else(|error| panic!("contained fan-out fixture write failed: {error}"));

    let route = root.join("app/api/precision/route.ts");
    std::fs::create_dir_all(
        route
            .parent()
            .unwrap_or_else(|| panic!("precision route fixture had no parent")),
    )
    .unwrap_or_else(|error| panic!("precision route parent failed: {error}"));
    std::fs::write(
        route,
        "export async function GET(request: Request, context: unknown, params: unknown, signal: AbortSignal, state: unknown) {\n  return new Response(String(Boolean(request && context && params && signal && state)));\n}\n",
    )
    .unwrap_or_else(|error| panic!("precision route fixture write failed: {error}"));
}

fn write_biomarker_python_fixture(root: &Path) {
    let mut python = String::from(
        "def normalize_values(values):\n    output = []\n    for index, value in enumerate(values):\n",
    );
    for index in 0..8 {
        writeln!(
            python,
            "        normalized_{index} = str(value).strip()\n        output.append(normalized_{index})"
        )
        .unwrap_or_else(|error| panic!("Python intrinsic fixture formatting failed: {error}"));
    }
    python.push_str(
        "    if len(output) == 0:\n        raise RuntimeError('empty')\n    return output\n\n\
         def missing_project_pressure(value):\n",
    );
    for index in 0..16 {
        writeln!(python, "    missing_project_{index}(value)")
            .unwrap_or_else(|error| panic!("Python unresolved fixture formatting failed: {error}"));
    }
    python.push_str(
        "    return value\n\n\
         def implicit_public():\n    return 'module-visible-without-explicit-export-intent'\n",
    );
    std::fs::write(root.join("precision.py"), python)
        .unwrap_or_else(|error| panic!("precision Python fixture write failed: {error}"));
}

fn write_biomarker_import_fixtures(root: &Path) {
    for (path, source) in [
        (
            "lazy-panel.tsx",
            "export default function LazyPanel() { return <section />; }\n",
        ),
        (
            "lazy-consumer.tsx",
            "export const LoadedPanel = React.lazy(() => import('./lazy-panel'));\n",
        ),
        (
            "runtime-config.ts",
            "export const RuntimeConfig = { mode: 'safe' } as const;\n",
        ),
        (
            "runtime-config-consumer.ts",
            "import type { RuntimeConfig } from './runtime-config';\nexport type RuntimeConfigShape = typeof RuntimeConfig;\n",
        ),
    ] {
        std::fs::write(root.join(path), source)
            .unwrap_or_else(|error| panic!("precision import fixture {path} failed: {error}"));
    }
}

fn write_cross_domain_clone_fixtures(root: &Path) {
    for (path, source) in [
        (
            "sessions/revoke.ts",
            r"export async function revokeSession(id: string) {
  const result = await sessions.revoke(id);
  const mapped = mapSessionResult(result);
  if (mapped.ok) {
    recordSessionAudit(mapped);
  }




  return mapped;
}
",
        ),
        (
            "billing/cancel.ts",
            r"export async function cancelInvoice(number: InvoiceId) {
  const outcome = await invoices.cancel(number);
  const response = mapInvoiceResult(outcome);
  if (response.accepted) {
    recordBillingLedger(response);
  }




  return response;
}
",
        ),
    ] {
        let target = root.join(path);
        std::fs::create_dir_all(
            target
                .parent()
                .unwrap_or_else(|| panic!("cross-domain clone fixture had no parent")),
        )
        .unwrap_or_else(|error| panic!("cross-domain clone parent failed: {error}"));
        std::fs::write(target, source)
            .unwrap_or_else(|error| panic!("cross-domain clone fixture write failed: {error}"));
    }
}

async fn insert_misleading_health_document(
    settings: &DatabaseSettings,
    schema: &str,
    indexed: &IndexReport,
) {
    let statement = format!(
        r#"INSERT INTO "{schema}"."search_documents" (
                id, project_id, generation_id, document_id, file_id, symbol_id,
                path, language, document_kind, qualified_name, code, natural_text, metadata
            ) OVERRIDING SYSTEM VALUE
            SELECT -1, symbols.project_id, symbols.generation_id,
                   '00000000-0000-4000-8000-000000000001'::uuid,
                   symbols.file_id, symbols.symbol_id, files.normalized_path,
                   files.language, 'file', 'misleading-file-document', '',
                   'non-symbol metadata must not override symbol health',
                   '{{"health":{{"dynamic_eval":0}},"duplicate_detection_enabled":false}}'::jsonb
            FROM "{schema}"."symbols" AS symbols
            JOIN "{schema}"."files" AS files
              ON files.project_id = symbols.project_id
             AND files.generation_id = symbols.generation_id
             AND files.file_id = symbols.file_id
            WHERE symbols.project_id = CAST($1 AS uuid)
              AND symbols.generation_id = CAST($2 AS uuid)
              AND symbols.qualified_name = 'risky'"#,
    );
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("metadata fixture connection failed: {error}"));
    let inserted = query(AssertSqlSafe(statement))
        .bind(indexed.project_id.as_str())
        .bind(indexed.generation_id.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("misleading metadata fixture failed: {error}"));
    assert_eq!(inserted.rows_affected(), 1);
    pool.close().await;
}

async fn assert_dependency_audit(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let report = runtime
        .audit_javascript_dependencies(&indexed.project_id, ProjectCancellation::new())
        .await
        .unwrap_or_else(|error| panic!("dependency audit failed: {error}"));
    let report = serde_json::to_value(report)
        .unwrap_or_else(|error| panic!("dependency audit serialization failed: {error}"));
    assert_eq!(report["rootManifest"], "package.json");
    assert_eq!(
        report["workspaceManifests"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(json_strings(&report["unusedRuntimeCandidates"]).contains(&"unused-runtime"));
    assert!(json_strings(&report["unusedDevelopmentCandidates"]).contains(&"unused-dev"));
    assert!(json_strings(&report["unusedPeerCandidates"]).contains(&"react"));
    assert!(!json_strings(&report["unusedOptionalCandidates"]).contains(&"optional-pkg"));
    let used = report["used"]
        .as_array()
        .unwrap_or_else(|| panic!("used dependency evidence was not an array"));
    for package in [
        "zod",
        "left-pad",
        "optional-pkg",
        "typescript",
        "@types/node",
    ] {
        assert!(
            used.iter().any(|entry| entry["package"] == package),
            "expected used package {package} in {used:?}"
        );
    }
    let undeclared = report["undeclared"]
        .as_array()
        .unwrap_or_else(|| panic!("undeclared dependency evidence was not an array"));
    assert!(
        undeclared
            .iter()
            .any(|entry| entry["package"] == "undeclared-pkg")
    );
    assert!(!undeclared.iter().any(|entry| entry["package"] == "vitest"));
    assert_eq!(report["evidenceComplete"], true);
}

async fn assert_layer_analysis(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let layers = runtime
        .analyze_layers(&indexed.project_id, ProjectCancellation::new())
        .await
        .unwrap_or_else(|error| panic!("layer analysis failed: {error}"));
    let layers = serde_json::to_value(layers)
        .unwrap_or_else(|error| panic!("layer analysis serialization failed: {error}"));
    assert_eq!(layers["configured"], true);
    assert_eq!(layers["importsEvaluated"], 8);
    assert_eq!(
        layers["violations"].as_array().map(Vec::len),
        Some(1),
        "{layers}"
    );
    assert_eq!(layers["violations"][0]["finding"], "illegal_import");
    assert_eq!(layers["violations"][0]["detail"]["fromLayer"], "app");
    assert_eq!(layers["violations"][0]["detail"]["toLayer"], "forbidden");
    assert_eq!(
        layers["violations"][0]["detail"]["confidenceBasis"],
        "fresh_source_resolved_current_generation_import"
    );
}

fn app_typescript_surface_query() -> FileSurfaceQuery {
    let directory = NormalizedPath::parse("packages/app")
        .unwrap_or_else(|error| panic!("surface directory failed: {error}"));
    FileSurfaceQuery::new(10)
        .map(|query| query.within_directory(directory))
        .and_then(|query| query.with_path_regex(Some("^packages/app/.*\\.ts$")))
        .unwrap_or_else(|error| panic!("file surface options failed: {error}"))
}

async fn assert_file_analysis(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let file_surface = runtime
        .database()
        .current_file_surface(&indexed.project_id, &app_typescript_surface_query())
        .await
        .unwrap_or_else(|error| panic!("file surface query failed: {error}"));
    assert_eq!(file_surface.total_files(), 1);
    assert_eq!(file_surface.files()[0].path(), "packages/app/index.ts");
    assert!(file_surface.total_symbols() >= 1);
    let aggregates = runtime
        .database()
        .current_file_aggregates(&indexed.project_id, &app_typescript_surface_query(), 10)
        .await
        .unwrap_or_else(|error| panic!("file aggregate query failed: {error}"));
    let aggregates = serde_json::to_value(aggregates)
        .unwrap_or_else(|error| panic!("file aggregate serialization failed: {error}"));
    assert!(aggregates["languages"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["language"] == "typescript" && row["files"] == 1)
    }));
    assert!(aggregates["directories"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["path"] == "packages/app" && row["files"] == 1)
    }));
    let dependencies = runtime
        .database()
        .current_file_dependencies(
            &indexed.project_id,
            &FileDependencyQuery::new(
                NormalizedPath::parse("packages/app/index.ts")
                    .unwrap_or_else(|error| panic!("dependency path failed: {error}")),
                10,
            )
            .map_or_else(
                |error| panic!("file dependency options failed: {error}"),
                |query| query.with_direction(FileDependencyDirection::Dependencies),
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("file dependency query failed: {error}"));
    let dependencies = serde_json::to_value(dependencies)
        .unwrap_or_else(|error| panic!("file dependency serialization failed: {error}"));
    assert!(dependencies["rows"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row["path"] == "packages/forbidden/secret.ts" && row["direction"] == "dependencies"
        })
    }));
}

async fn assert_dead_code_judgement(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let candidates = runtime
        .database()
        .query_current_dead_code(
            &indexed.project_id,
            &DeadCodeQuery::new(10)
                .unwrap_or_else(|error| panic!("dead-code options failed: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("dead-code candidate query failed: {error}"));
    let abandoned = candidates
        .iter()
        .find(|candidate| candidate.qualified_name().ends_with("abandonedTask"))
        .cloned()
        .unwrap_or_else(|| panic!("private orphan was not selected: {candidates:?}"));
    assert!(!abandoned.safe_code().contains("do-not-send-secret"));
    let (endpoint, server) = dead_code_chat_fixture_server();
    let chat = OpenAiChatClient::new(
        ChatSettings::new(&endpoint, "fixture-judge", None)
            .unwrap_or_else(|error| panic!("chat settings failed: {error}")),
    )
    .unwrap_or_else(|error| panic!("chat client failed: {error}"));
    let judged = judge_dead_code_candidates(DeadCodeJudgeRequest {
        client: &chat,
        candidates: vec![abandoned],
        options: DeadCodeJudgeOptions::new(1)
            .unwrap_or_else(|error| panic!("judge options failed: {error}")),
        cancellation: ProjectCancellation::new(),
    })
    .await
    .unwrap_or_else(|error| panic!("dead-code judge failed: {error}"));
    assert_eq!(judged.results().len(), 1);
    assert_eq!(judged.results()[0].verdict(), DeadCodeVerdict::Dead);
    assert!(scores_match(judged.results()[0].confidence(), 0.8));
    server
        .join()
        .unwrap_or_else(|_| panic!("chat fixture server panicked"));
}

async fn structural_findings_json(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
) -> serde_json::Value {
    let findings = runtime
        .database()
        .current_structural_findings(&indexed.project_id, 100)
        .await
        .unwrap_or_else(|error| panic!("expanded biomarker query failed: {error}"));
    serde_json::to_value(findings)
        .unwrap_or_else(|error| panic!("biomarker serialization failed: {error}"))
}

fn assert_structural_finding_inventory(value: &serde_json::Value) {
    let findings = value
        .as_array()
        .unwrap_or_else(|| panic!("biomarker result was not an array"));
    for expected in ["long_parameter_list", "dynamic_eval"] {
        assert!(
            findings.iter().any(|finding| {
                finding["qualifiedName"] == "risky" && finding["finding"] == expected
            }),
            "missing {expected} finding in {findings:?}"
        );
    }
    for expected in ["magic_number", "hardcoded_url", "secrets_handling"] {
        assert!(
            findings.iter().any(|finding| {
                finding["qualifiedName"] == "authenticate" && finding["finding"] == expected
            }),
            "missing {expected} finding in {findings:?}"
        );
    }
    assert_contextual_detector_findings(findings);
    assert!(findings.iter().any(|finding| {
        finding["qualifiedName"] == "RETRY_COUNT" && finding["finding"] == "stale_doc"
    }));
    assert_biomarker_precision_inventory(findings);
    assert_clone_class_findings(findings);
}

fn assert_biomarker_precision_inventory(findings: &[serde_json::Value]) {
    for (detector, path, name) in [
        ("large_method", "precision.tsx", "StaticMarkup"),
        ("sql_string_concat", "precision.tsx", "DeleteLabel"),
        ("sql_string_concat", "precision.tsx", "presentationUpdate"),
        ("high_fan_out", "contained.ts", "containedCoordinator"),
        ("long_parameter_list", "app/api/precision/route.ts", "GET"),
        (
            "unresolved_reference_pressure",
            "precision.py",
            "normalize_values",
        ),
        ("unused_export", "precision.py", "implicit_public"),
        ("unused_export", "lazy-panel.tsx", "LazyPanel"),
        ("unused_export", "runtime-config.ts", "RuntimeConfig"),
        ("stale_doc", "precision.tsx", "ILLUSTRATIVE_COLUMNS"),
        ("secrets_handling", "precision.tsx", "safeSign"),
    ] {
        assert!(
            !has_json_finding(findings, detector, path, name),
            "precision negative control was actionable: {detector} {path}::{name}; findings={findings:?}"
        );
    }
    for (detector, path, name) in [
        ("large_method", "precision.tsx", "ImperativeLong"),
        ("sql_string_concat", "precision.tsx", "dynamicSelect"),
        ("sql_string_concat", "precision.tsx", "dynamicInsert"),
        ("sql_string_concat", "precision.tsx", "dynamicUpdate"),
        ("sql_string_concat", "precision.tsx", "dynamicDelete"),
        ("high_fan_out", "facade.ts", "mixedOrchestrator"),
        ("long_parameter_list", "precision.tsx", "ordinaryFive"),
        (
            "unresolved_reference_pressure",
            "precision.py",
            "missing_project_pressure",
        ),
        ("unused_export", "precision.tsx", "explicitlyUnused"),
        ("stale_doc", "precision.tsx", "EXPLICIT_RETRY_LIMIT"),
        ("secrets_handling", "precision.tsx", "exposeSecret"),
    ] {
        assert!(
            has_json_finding(findings, detector, path, name),
            "precision positive control was hidden: {detector} {path}::{name}; findings={findings:?}"
        );
    }
}

fn has_json_finding(
    findings: &[serde_json::Value],
    detector: &str,
    path: &str,
    name: &str,
) -> bool {
    findings.iter().any(|finding| {
        finding["finding"] == detector
            && finding["path"] == path
            && finding["qualifiedName"] == name
    })
}

fn assert_contextual_detector_findings(findings: &[serde_json::Value]) {
    let endpoint = findings
        .iter()
        .find(|finding| {
            finding["qualifiedName"] == "authenticate" && finding["finding"] == "hardcoded_url"
        })
        .unwrap_or_else(|| panic!("hardcoded endpoint detail was missing"));
    assert_eq!(endpoint["detail"]["category"], "service_configuration");
    assert_eq!(endpoint["detail"]["serviceConfiguration"], 1);
    assert!(!findings.iter().any(|finding| {
        finding["qualifiedName"] == "SetupHelp" && finding["finding"] == "hardcoded_url"
    }));

    let parallel = findings
        .iter()
        .find(|finding| {
            finding["qualifiedName"] == "parallelCandidate" && finding["finding"] == "forof_await"
        })
        .unwrap_or_else(|| panic!("parallelizable for-of finding was missing"));
    assert_eq!(parallel["detail"]["awaitedCall"], "owned_await_expression");
    for serial in ["replay", "drain"] {
        assert!(!findings.iter().any(|finding| {
            finding["qualifiedName"] == serial && finding["finding"] == "forof_await"
        }));
    }

    assert!(!findings.iter().any(|finding| {
        finding["qualifiedName"] == "recordsRepository" && finding["finding"] == "high_fan_out"
    }));
    let mixed = findings
        .iter()
        .find(|finding| {
            finding["qualifiedName"] == "mixedOrchestrator" && finding["finding"] == "high_fan_out"
        })
        .unwrap_or_else(|| panic!("mixed orchestration fan-out finding was missing"));
    assert_eq!(mixed["detail"]["role"], "mixed_orchestration");
    assert!(
        mixed["detail"]["dependencyGroups"]["calls"]
            .as_u64()
            .is_some_and(|calls| calls >= 25)
    );
}

fn assert_clone_class_findings(findings: &[serde_json::Value]) {
    let near_classes = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding["qualifiedName"].as_str(),
                Some("firstClone" | "secondClone")
            ) && finding["finding"] == "duplicate_code"
                && finding["metricName"] == "normalized_shape_copies"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        near_classes.len(),
        1,
        "near-clone class was not represented exactly once: {findings:?}"
    );
    assert_eq!(near_classes[0]["detail"]["recordScope"], "clone_class");
    assert_eq!(
        near_classes[0]["detail"]["symmetricMemberRowsSuppressed"],
        true
    );

    let partial_classes = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding["qualifiedName"].as_str(),
                Some("partialAlpha" | "partialBeta")
            ) && finding["finding"] == "duplicate_code"
                && finding["metricName"] == "partial_clone_peers"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        partial_classes.len(),
        1,
        "partial-clone class was not represented exactly once: {findings:?}"
    );
    let partial = partial_classes[0];
    assert_eq!(partial["detail"]["cloneType"], "partial");
    assert_eq!(partial["detail"]["classSize"], 2);
    assert_eq!(
        partial["detail"]["members"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(
        partial["detail"]["maximumOverlap"]
            .as_f64()
            .is_some_and(|overlap| (0.95..=1.0).contains(&overlap))
    );
    assert!(!findings.iter().any(|finding| {
        finding["qualifiedName"] == "deliberateCopy" && finding["finding"] == "duplicate_code"
    }));
    for unrelated in ["revokeSession", "cancelInvoice"] {
        assert!(!findings.iter().any(|finding| {
            finding["qualifiedName"] == unrelated && finding["finding"] == "duplicate_code"
        }));
    }
}

/// The readiness probe must never evaluate the detector cascade, and the stored
/// relation must agree exactly with a live evaluation, recompute when any input
/// fingerprint moves, and stay put when nothing changed.
/// Coverage feeds `low_coverage` and the centrality percentile per symbol, so an
/// import must move the input fingerprint. A re-import can redistribute hit
/// lines between symbols while leaving project-wide totals identical, so an
/// aggregate alone cannot fence this input.
async fn assert_coverage_moves_the_finding_fingerprint(
    database: &cartograph_db::CartographDatabase,
    indexed: &IndexReport,
    settings: &DatabaseSettings,
    schema: &str,
) {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("coverage fixture connection failed: {error}"));
    let coverage_rows = query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}"."coverage_sources" (
                project_id, label, report_format, report_digest
            )
            VALUES (CAST($1 AS uuid), 'fingerprint-fixture', 'lcov', repeat('a', 64))"#
    )))
    .bind(indexed.project_id.as_str())
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("coverage source insert failed: {error}"))
    .rows_affected();
    assert_eq!(coverage_rows, 1);
    assert_eq!(
        database
            .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("coverage-change refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed,
        "an imported coverage report must move the input fingerprint"
    );
    let rewritten = query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."coverage_sources"
            SET report_digest = repeat('b', 64)
            WHERE project_id = CAST($1 AS uuid)
              AND label = 'fingerprint-fixture'"#
    )))
    .bind(indexed.project_id.as_str())
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("coverage digest rewrite failed: {error}"))
    .rows_affected();
    pool.close().await;
    assert_eq!(rewritten, 1);
    assert_eq!(
        database
            .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("re-import refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed,
        "a coverage re-import must move the fingerprint even when totals are unchanged"
    );
}

async fn assert_structural_finding_cache(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    settings: &DatabaseSettings,
    schema: &str,
) {
    let database = runtime.database();
    assert_uncomputed_structural_finding_reads(runtime, indexed).await;

    assert_eq!(
        database
            .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("first finding refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed
    );
    assert_eq!(
        database
            .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("idempotent finding refresh failed: {error}")),
        StructuralFindingRefresh::Current,
        "an unchanged fingerprint must not recompute the relation"
    );

    let cached = database
        .cached_current_structural_finding_stats(&indexed.project_id)
        .await
        .unwrap_or_else(|error| panic!("computed cache read failed: {error}"))
        .unwrap_or_else(|| panic!("computed relation must be readable"));
    let evaluated = database
        .current_structural_finding_stats_bounded(&indexed.project_id, Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("live finding evaluation failed: {error}"));
    assert_eq!(
        serde_json::to_value(&cached).ok(),
        serde_json::to_value(&evaluated).ok(),
        "stored findings must equal a live evaluation of the same generation"
    );
    assert!(
        cached.total_findings() > 0,
        "the fixture corpus must produce findings"
    );

    assert_coverage_moves_the_finding_fingerprint(database, indexed, settings, schema).await;

    // A detector-contract change must invalidate a warm relation; otherwise a
    // shipped rule change keeps serving findings the rules no longer produce.
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("cache fixture connection failed: {error}"));
    let updated = query(AssertSqlSafe(format!(
        r#"UPDATE "{schema}"."structural_finding_runs"
            SET inputs_digest = repeat('0', 64)
            WHERE project_id = CAST($1 AS uuid)"#
    )))
    .bind(indexed.project_id.as_str())
    .execute(&pool)
    .await
    .unwrap_or_else(|error| panic!("cache fingerprint rewrite failed: {error}"))
    .rows_affected();
    pool.close().await;
    assert_eq!(updated, 1);
    assert_eq!(
        database
            .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("stale fingerprint refresh failed: {error}")),
        StructuralFindingRefresh::Recomputed,
        "a moved input fingerprint must force a recomputation"
    );
}

async fn assert_uncomputed_structural_finding_reads(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
) {
    let database = runtime.database();
    assert!(
        database
            .cached_current_structural_finding_stats(&indexed.project_id)
            .await
            .unwrap_or_else(|error| panic!("uncomputed cache read failed: {error}"))
            .is_none(),
        "readiness must report pending before the relation is computed"
    );

    let uncomputed = database
        .current_structural_finding_stats(&indexed.project_id)
        .await
        .unwrap_or_else(|error| panic!("uncomputed stats read failed: {error}"));
    assert_eq!(
        serde_json::to_value(&uncomputed)
            .unwrap_or_else(|error| panic!("uncomputed stats serialization failed: {error}"))["state"],
        "not_computed",
        "a biomarker read must not start a full-generation detector refresh"
    );
    let uncomputed_query = StructuralFindingQuery::new(10)
        .unwrap_or_else(|error| panic!("uncomputed finding query failed: {error}"));
    assert!(
        database
            .query_current_structural_findings(&indexed.project_id, &uncomputed_query)
            .await
            .unwrap_or_else(|error| panic!("uncomputed findings read failed: {error}"))
            .is_empty(),
        "an uncomputed finding relation must degrade to an empty bounded result"
    );
    assert_eq!(
        database
            .count_current_structural_findings(&indexed.project_id, &uncomputed_query)
            .await
            .unwrap_or_else(|error| panic!("uncomputed finding count failed: {error}")),
        0
    );
    assert!(
        database
            .cached_current_structural_finding_stats(&indexed.project_id)
            .await
            .unwrap_or_else(|error| panic!("post-read cache check failed: {error}"))
            .is_none(),
        "read-only biomarker queries must leave not_computed state unchanged"
    );
}

async fn assert_structural_finding_queries(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let query = StructuralFindingQuery::new(10)
        .and_then(|query| query.with_finding(Some("dynamic_eval")))
        .map(|query| query.with_minimum_severity(StructuralFindingSeverity::Warning))
        .and_then(|query| query.with_metric_bounds(Some(1.0), Some(1.0)))
        .and_then(|query| query.with_minimum_centrality(Some(0.0)))
        .and_then(|query| query.with_excluded_path_prefix(Some("packages/legacy/")))
        .unwrap_or_else(|error| panic!("filtered biomarker options failed: {error}"));
    let filtered = runtime
        .database()
        .query_current_structural_findings(&indexed.project_id, &query)
        .await
        .unwrap_or_else(|error| panic!("filtered biomarker query failed: {error}"));
    assert_eq!(filtered.len(), 1);
    assert!((0.0..=1.0).contains(&filtered[0].degree_centrality()));
    let count = runtime
        .database()
        .count_current_structural_findings(&indexed.project_id, &query)
        .await
        .unwrap_or_else(|error| panic!("filtered biomarker count failed: {error}"));
    assert_eq!(count, 1);
    let grouped_query = StructuralFindingGroupQuery::new(
        vec!["dynamic_eval".to_owned(), "magic_number".to_owned()],
        1,
    )
    .map(|query| query.with_minimum_severity(StructuralFindingSeverity::Info))
    .map_or_else(
        |error| panic!("grouped biomarker options failed: {error}"),
        |query| query.with_exclude_fixtures(true),
    );
    let grouped = runtime
        .database()
        .query_current_structural_findings_per_detector(&indexed.project_id, &grouped_query)
        .await
        .unwrap_or_else(|error| panic!("grouped biomarker query failed: {error}"));
    assert_eq!(grouped.counts().get("dynamic_eval"), Some(&1));
    assert!(
        grouped
            .counts()
            .get("magic_number")
            .is_some_and(|count| *count >= 1)
    );
    assert_eq!(grouped.into_findings().len(), 2);
    let stats = runtime
        .database()
        .current_structural_finding_stats(&indexed.project_id)
        .await
        .unwrap_or_else(|error| panic!("expanded biomarker stats failed: {error}"));
    let stats = serde_json::to_value(stats)
        .unwrap_or_else(|error| panic!("biomarker stats serialization failed: {error}"));
    assert!(stats["totalFindings"].as_u64().unwrap_or_default() >= 2);
    assert_eq!(stats["byFinding"]["dynamic_eval"], 1);
}

async fn assert_biomarker_precision_stats(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let stats = runtime
        .database()
        .current_structural_finding_stats(&indexed.project_id)
        .await
        .unwrap_or_else(|error| panic!("precision biomarker stats failed: {error}"));
    let stats = serde_json::to_value(stats)
        .unwrap_or_else(|error| panic!("precision biomarker stats serialization failed: {error}"));
    for detector in [
        "large_method",
        "sql_string_concat",
        "high_fan_out",
        "long_parameter_list",
        "unresolved_reference_pressure",
        "unused_export",
        "stale_doc",
        "secrets_handling",
    ] {
        let query = StructuralFindingQuery::new(100)
            .and_then(|query| query.with_finding(Some(detector)))
            .map_or_else(
                |error| panic!("precision biomarker query was invalid: {error}"),
                |query| query.with_minimum_severity(StructuralFindingSeverity::Info),
            );
        let ranked_count = runtime
            .database()
            .count_current_structural_findings(&indexed.project_id, &query)
            .await
            .unwrap_or_else(|error| panic!("precision biomarker count failed: {error}"));
        assert_eq!(
            stats["byFinding"][detector].as_u64().unwrap_or_default(),
            ranked_count,
            "ranked and stats precision policy diverged for {detector}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn workspace_dependency_audit_combines_manifests_graph_scripts_and_dynamic_imports() {
    let (schema, settings, project) = live_project_fixture("8");
    write_dependency_workspace_fixture(project.path());
    write_dependency_source_fixture(project.path());
    write_dependency_clone_fixtures(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("dependency runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("dependency fixture index failed: {error}"));
        insert_misleading_health_document(&settings, &schema, &indexed).await;
        assert_dependency_audit(&runtime, &indexed).await;
        assert_layer_analysis(&runtime, &indexed).await;
        assert_file_analysis(&runtime, &indexed).await;
        assert_dead_code_judgement(&runtime, &indexed).await;
        assert_structural_finding_cache(&runtime, &indexed, &settings, &schema).await;
        let findings = structural_findings_json(&runtime, &indexed).await;
        assert_structural_finding_inventory(&findings);
        assert_structural_finding_queries(&runtime, &indexed).await;
        assert_biomarker_precision_stats(&runtime, &indexed).await;
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

fn write_git_history_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("history source directory failed: {error}"));
    std::fs::create_dir_all(root.join(".github/workflows"))
        .unwrap_or_else(|error| panic!("history workflow directory failed: {error}"));
    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "history@example.invalid"]);
    git(root, &["config", "user.name", "History Fixture"]);
    std::fs::write(
        root.join(HISTORY_ANCHOR_PATH),
        "export function workflowCheck() { return 1; }\n",
    )
    .unwrap_or_else(|error| panic!("history a write failed: {error}"));
    std::fs::write(
        root.join(HISTORY_PARTNER_PATH),
        "export function acknowledgements() { return 1; }\n",
    )
    .unwrap_or_else(|error| panic!("history b write failed: {error}"));
    git(root, &["add", HISTORY_ANCHOR_PATH, HISTORY_PARTNER_PATH]);
    git(root, &["commit", "-m", "add a and b"]);
    std::fs::write(
        root.join(HISTORY_ANCHOR_PATH),
        "export function workflowCheck() { return 2; }\n",
    )
    .unwrap_or_else(|error| panic!("history a update failed: {error}"));
    std::fs::write(
        root.join(HISTORY_PARTNER_PATH),
        "export function acknowledgements() { return 2; }\n",
    )
    .unwrap_or_else(|error| panic!("history b update failed: {error}"));
    git(root, &["add", HISTORY_ANCHOR_PATH, HISTORY_PARTNER_PATH]);
    git(root, &["commit", "-m", "change a and b"]);
    std::fs::write(
        root.join(HISTORY_ANCHOR_PATH),
        "export function workflowCheck() { return 3; }\n",
    )
    .unwrap_or_else(|error| panic!("history a second update failed: {error}"));
    std::fs::write(
        root.join(HISTORY_THIRD_PATH),
        "export function c() { return 1; }\n",
    )
    .unwrap_or_else(|error| panic!("history c write failed: {error}"));
    git(root, &["add", HISTORY_ANCHOR_PATH, HISTORY_THIRD_PATH]);
    git(root, &["commit", "-m", "change a and add c"]);
}

async fn refresh_history(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let indexed_json = serde_json::to_value(indexed)
        .unwrap_or_else(|error| panic!("index history serialization failed: {error}"));
    assert_eq!(indexed_json["history"]["state"], "indexed");
    assert_eq!(indexed_json["history"]["report"]["commitsScanned"], 3);
    let options = HistoryIndexOptions::default()
        .with_max_commits(100)
        .unwrap_or_else(|error| panic!("history options failed: {error}"));
    let report = runtime
        .refresh_git_history(
            indexed.project_id.clone(),
            options,
            ProjectCancellation::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("history refresh failed: {error}"));
    let report = serde_json::to_value(report)
        .unwrap_or_else(|error| panic!("history report serialization failed: {error}"));
    assert_eq!(report["commitsScanned"], 3);
    assert_eq!(report["filesWritten"], 3);
    assert_eq!(report["cochangesWritten"], 2);
    assert_eq!(report["shallowHistory"], false);
    assert_eq!(report["truncated"], false);
}

async fn assert_history_rows(runtime: &ProjectRuntime, indexed: &IndexReport) -> NormalizedPath {
    let anchor = NormalizedPath::parse(HISTORY_ANCHOR_PATH)
        .unwrap_or_else(|error| panic!("history anchor path failed: {error}"));
    let history = runtime
        .database()
        .current_file_history(FileHistoryQuery::new(&indexed.project_id, 10).for_path(&anchor))
        .await
        .unwrap_or_else(|error| panic!("history rows failed: {error}"));
    let history = serde_json::to_value(history)
        .unwrap_or_else(|error| panic!("history rows serialization failed: {error}"));
    assert_eq!(history[0]["commitCount"], 3);
    assert_eq!(history[0]["authorCount"], 1);
    let partners = runtime
        .database()
        .current_file_cochanges(
            FileCochangeQuery::new(&indexed.project_id, &anchor, 10).with_minimum_commits(2),
        )
        .await
        .unwrap_or_else(|error| panic!("cochange rows failed: {error}"));
    let partners = serde_json::to_value(partners)
        .unwrap_or_else(|error| panic!("cochange serialization failed: {error}"));
    assert_eq!(partners.as_array().map(Vec::len), Some(1));
    assert_eq!(partners[0]["path"], HISTORY_PARTNER_PATH);
    assert_eq!(partners[0]["sharedCommits"], 2);
    assert_eq!(partners[0]["anchorCommits"], 3);
    assert_eq!(partners[0]["partnerCommits"], 2);
    assert!((partners[0]["jaccard"].as_f64().unwrap_or_default() - (2.0 / 3.0)).abs() < 0.001);
    anchor
}

async fn assert_history_hotspots(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let hotspots = runtime
        .database()
        .current_structural_hotspots(&indexed.project_id, 10)
        .await
        .unwrap_or_else(|error| panic!("history-composed hotspots failed: {error}"));
    let hotspots = serde_json::to_value(hotspots)
        .unwrap_or_else(|error| panic!("hotspot serialization failed: {error}"));
    let anchor = hotspots
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["path"] == HISTORY_ANCHOR_PATH)
        .unwrap_or_else(|| panic!("history-composed hotspot for a was missing"));
    assert_eq!(anchor["historyAvailable"], true);
    assert_eq!(anchor["commitCount"], 3);
    assert!(anchor["compositeRisk"].as_u64().unwrap_or_default() > 0);
    assert!(anchor["centrality"].as_f64().is_some());
    assert!(anchor["churnScore"].as_f64().is_some());
    let maintenance = runtime
        .database()
        .query_structural_hotspots(
            &indexed.project_id,
            StructuralHotspotQuery::new(10)
                .and_then(|query| query.with_minimum_commits(2))
                .unwrap_or_else(|error| panic!("maintenance hotspot query failed: {error}"))
                .with_category(StructuralHotspotCategory::Maintenance)
                .with_sort(StructuralHotspotSort::Churn),
        )
        .await
        .unwrap_or_else(|error| panic!("maintenance hotspots failed: {error}"));
    assert_eq!(maintenance.len(), 2);
    assert_eq!(maintenance[0].path(), HISTORY_ANCHOR_PATH);
    assert!(maintenance.iter().all(|row| row.commit_count() >= 2));
}

async fn assert_grouped_history_peers(runtime: &ProjectRuntime, indexed: &IndexReport) {
    let wide_paths = [
        HISTORY_ANCHOR_PATH,
        HISTORY_PARTNER_PATH,
        HISTORY_THIRD_PATH,
    ]
    .into_iter()
    .map(|path| {
        NormalizedPath::parse(path).unwrap_or_else(|error| panic!("grouped path failed: {error}"))
    })
    .collect();
    let wide = GroupedPathInput::new("commit-wide", wide_paths)
        .unwrap_or_else(|error| panic!("wide peer group failed: {error}"));
    let partner = NormalizedPath::parse(HISTORY_PARTNER_PATH)
        .unwrap_or_else(|error| panic!("small grouped path failed: {error}"));
    let small = GroupedPathInput::new("commit-small", vec![partner])
        .unwrap_or_else(|error| panic!("small peer group failed: {error}"));
    let query = GroupedSymbolQuery::new(vec![wide, small], 1)
        .unwrap_or_else(|error| panic!("grouped peer query failed: {error}"));
    let peers = runtime
        .database()
        .current_grouped_path_symbols(&indexed.project_id, query)
        .await
        .unwrap_or_else(|error| panic!("grouped peer lookup failed: {error}"));
    let peers = serde_json::to_value(peers)
        .unwrap_or_else(|error| panic!("grouped peer serialization failed: {error}"));
    assert_eq!(peers[0]["key"], "commit-wide");
    assert_eq!(peers[0]["total"], 3);
    assert_eq!(peers[0]["peers"].as_array().map(Vec::len), Some(1));
    assert_eq!(peers[0]["truncated"], true);
    assert_eq!(peers[1]["key"], "commit-small");
    assert_eq!(peers[1]["total"], 1);
    assert_eq!(peers[1]["truncated"], false);
}

async fn assert_history_can_be_disabled(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    anchor: &NormalizedPath,
    root: &Path,
) {
    std::fs::create_dir_all(root.join(".cartograph"))
        .unwrap_or_else(|error| panic!("history config directory failed: {error}"));
    std::fs::write(
        root.join(".cartograph/config.json"),
        r#"{"enableChurn":false,"enableCoChange":false,"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("history disable config failed: {error}"));
    let disabled = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("history disable index failed: {error}"));
    assert!(!disabled.published);
    let report = serde_json::to_value(&disabled)
        .unwrap_or_else(|error| panic!("history disable report failed: {error}"));
    assert_eq!(report["history"]["reason"], "disabled_by_project_config");
    let history = runtime
        .database()
        .current_file_history(FileHistoryQuery::new(&indexed.project_id, 10))
        .await
        .unwrap_or_else(|error| panic!("disabled history query failed: {error}"));
    assert!(history.is_empty());
    let cochanges = runtime
        .database()
        .current_file_cochanges(
            FileCochangeQuery::new(&indexed.project_id, anchor, 10).with_minimum_commits(1),
        )
        .await
        .unwrap_or_else(|error| panic!("disabled cochange query failed: {error}"));
    assert!(cochanges.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn git_history_refresh_persists_churn_and_symmetric_cochange_confidence() {
    let (schema, settings, project) = live_project_fixture("8");
    write_git_history_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("history runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("history fixture index failed: {error}"));
        refresh_history(&runtime, &indexed).await;
        let anchor = assert_history_rows(&runtime, &indexed).await;
        assert_history_hotspots(&runtime, &indexed).await;
        assert_grouped_history_peers(&runtime, &indexed).await;
        assert_history_can_be_disabled(&runtime, &indexed, &anchor, project.path()).await;
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

fn write_issue_history_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("issue-history source directory failed: {error}"));
    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "issues@example.invalid"]);
    git(root, &["config", "user.name", "Issue Fixture"]);
    write_issue_functions(root, 1, 1);
    git(root, &["add", "src/a.ts", "src/b.ts"]);
    git(root, &["commit", "-m", "initial symbols"]);
    write_issue_functions(root, 2, 2);
    git(root, &["add", "src/a.ts", "src/b.ts"]);
    git(
        root,
        &["commit", "-m", "change pair\n\nFixes #12, closes #13"],
    );
    write_issue_functions(root, 3, 3);
    git(root, &["add", "src/a.ts", "src/b.ts"]);
    git(root, &["commit", "-m", "change pair again\n\nResolves #14"]);
    std::fs::write(
        root.join("src/new.ts"),
        "export function brandNew(): number { return 15; }\n",
    )
    .unwrap_or_else(|error| panic!("new issue symbol write failed: {error}"));
    git(root, &["add", "src/new.ts"]);
    git(root, &["commit", "-m", "add symbol\n\nFixes #15"]);
}

fn assert_issue_history_index_report(indexed: &IndexReport) {
    let report = serde_json::to_value(indexed)
        .unwrap_or_else(|error| panic!("issue-history report serialization failed: {error}"));
    assert_eq!(report["issue_history"]["state"], "indexed", "{report}");
    assert_eq!(report["issue_history"]["report"]["taggedCommits"], 3);
    assert_eq!(
        report["issue_history"]["report"]["oversizedCommitsSkipped"],
        0
    );
    assert_eq!(
        report["issue_history"]["report"]["comparisonFailuresSkipped"],
        0
    );
}

struct IssueSymbols {
    alpha: SymbolId,
    beta: SymbolId,
    brand_new: SymbolId,
}

async fn issue_symbols(runtime: &ProjectRuntime, indexed: &IndexReport) -> IssueSymbols {
    let alpha = exact_symbol_id(
        runtime,
        &indexed.project_id,
        &indexed.generation_id,
        "alpha",
    )
    .await;
    let beta = exact_symbol_id(runtime, &indexed.project_id, &indexed.generation_id, "beta").await;
    let brand_new = exact_symbol_id(
        runtime,
        &indexed.project_id,
        &indexed.generation_id,
        "brandNew",
    )
    .await;
    IssueSymbols {
        alpha,
        beta,
        brand_new,
    }
}

async fn assert_issue_attributions(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    symbols: &IssueSymbols,
) {
    let alpha_issues = runtime
        .database()
        .current_symbol_issues(SymbolIssueQuery {
            project_id: &indexed.project_id,
            symbol_id: &symbols.alpha,
            limit: 50,
        })
        .await
        .unwrap_or_else(|error| panic!("alpha issue read failed: {error}"));
    assert_eq!(
        alpha_issues
            .iter()
            .map(|issue| issue.issue_number)
            .collect::<std::collections::BTreeSet<_>>(),
        [12, 13, 14].into_iter().collect()
    );
    assert!(
        alpha_issues
            .iter()
            .all(|issue| issue.kind == IssueAttributionKind::Modified)
    );
    let added = runtime
        .database()
        .current_symbol_issues(SymbolIssueQuery {
            project_id: &indexed.project_id,
            symbol_id: &symbols.brand_new,
            limit: 50,
        })
        .await
        .unwrap_or_else(|error| panic!("added issue read failed: {error}"));
    assert!(
        added
            .iter()
            .any(|issue| { issue.issue_number == 15 && issue.kind == IssueAttributionKind::Added })
    );
}

async fn assert_issue_peers(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    symbols: &IssueSymbols,
) {
    let peers = runtime
        .database()
        .current_symbol_issue_peers(SymbolIssuePeerQuery {
            project_id: &indexed.project_id,
            symbol_id: &symbols.alpha,
            minimum_shared: 2,
            limit: 10,
        })
        .await
        .unwrap_or_else(|error| panic!("issue peer read failed: {error}"));
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].symbol_id, symbols.beta);
    assert_eq!(peers[0].co_occurrences, 2);
    assert_eq!(peers[0].shared_commits.len(), 2);
    let groups = runtime
        .database()
        .current_issue_commit_symbol_peers(IssueCommitSymbolPeerQuery {
            project_id: &indexed.project_id,
            excluded_symbol_id: &symbols.alpha,
            commits: &peers[0].shared_commits,
            per_commit_limit: 10,
        })
        .await
        .unwrap_or_else(|error| panic!("issue commit peer read failed: {error}"));
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|group| {
        group.total == 1
            && !group.truncated
            && group
                .peers
                .iter()
                .any(|peer| peer.symbol_id == symbols.beta)
    }));
}

async fn assert_issue_generation_fence(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    symbols: &IssueSymbols,
    root: &Path,
) -> IndexReport {
    std::fs::remove_file(root.join("src/new.ts"))
        .unwrap_or_else(|error| panic!("removed symbol fixture failed: {error}"));
    std::fs::write(
        root.join("src/a.ts"),
        "export function alpha(): number { return 4; }\n",
    )
    .unwrap_or_else(|error| panic!("generation-fence alpha write failed: {error}"));
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "remove and change\n\nFixes #16"]);
    let refreshed = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("issue-history reindex failed: {error}"));
    assert_ne!(refreshed.generation_id, indexed.generation_id);
    let alpha_issues = runtime
        .database()
        .current_symbol_issues(SymbolIssueQuery {
            project_id: &refreshed.project_id,
            symbol_id: &symbols.alpha,
            limit: 50,
        })
        .await
        .unwrap_or_else(|error| panic!("refreshed alpha issue read failed: {error}"));
    assert!(alpha_issues.iter().any(|issue| issue.issue_number == 16));
    let removed_issues = runtime
        .database()
        .current_symbol_issues(SymbolIssueQuery {
            project_id: &refreshed.project_id,
            symbol_id: &symbols.brand_new,
            limit: 50,
        })
        .await
        .unwrap_or_else(|error| panic!("removed symbol issue read failed: {error}"));
    assert!(removed_issues.is_empty());
    refreshed
}

async fn assert_issue_history_can_be_disabled(
    runtime: &ProjectRuntime,
    refreshed: &IndexReport,
    root: &Path,
) {
    std::fs::create_dir_all(root.join(".cartograph"))
        .unwrap_or_else(|error| panic!("disabled issue config directory failed: {error}"));
    std::fs::write(
        root.join(".cartograph/config.json"),
        r#"{"enableIssueHistory":false}"#,
    )
    .unwrap_or_else(|error| panic!("disabled issue config write failed: {error}"));
    let disabled = runtime
        .index(IndexOptions::default())
        .await
        .unwrap_or_else(|error| panic!("disabled issue-history index failed: {error}"));
    let report = serde_json::to_value(disabled)
        .unwrap_or_else(|error| panic!("disabled issue report serialization failed: {error}"));
    assert_eq!(report["issue_history"]["state"], "unavailable");
    assert_eq!(
        report["issue_history"]["reason"],
        "disabled_by_project_config"
    );
    let count = runtime
        .database()
        .current_issue_history_attribution_count(&refreshed.project_id)
        .await
        .unwrap_or_else(|error| panic!("disabled issue count failed: {error}"));
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn issue_history_is_structural_generation_fenced_coupled_and_disableable() {
    let (schema, settings, project) = live_project_fixture("8");
    write_issue_history_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("issue-history runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("issue-history index failed: {error}"));
        assert_issue_history_index_report(&indexed);
        let symbols = issue_symbols(&runtime, &indexed).await;
        assert_issue_attributions(&runtime, &indexed, &symbols).await;
        assert_issue_peers(&runtime, &indexed, &symbols).await;
        let refreshed =
            assert_issue_generation_fence(&runtime, &indexed, &symbols, project.path()).await;
        assert_issue_history_can_be_disabled(&runtime, &refreshed, project.path()).await;
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

fn write_issue_functions(root: &Path, alpha: u32, beta: u32) {
    std::fs::write(
        root.join("src/a.ts"),
        format!("export function alpha(): number {{ return {alpha}; }}\n"),
    )
    .unwrap_or_else(|error| panic!("alpha issue fixture failed: {error}"));
    std::fs::write(
        root.join("src/b.ts"),
        format!("export function beta(): number {{ return {beta}; }}\n"),
    )
    .unwrap_or_else(|error| panic!("beta issue fixture failed: {error}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn import_audit_classifies_and_filters_complete_fresh_evidence() {
    let (schema, settings, project) = live_project_fixture("8");
    for directory in ["src/dir", "src/fixtures"] {
        std::fs::create_dir_all(project.path().join(directory))
            .unwrap_or_else(|error| panic!("import fixture directory failed: {error}"));
    }
    std::fs::write(
        project.path().join("tsconfig.json"),
        r#"{
          "compilerOptions": {
            "baseUrl": ".",
            "paths": {"@/*": ["src/*"]}
          }
        }"#,
    )
    .unwrap_or_else(|error| panic!("tsconfig fixture failed: {error}"));
    for path in [
        "src/direct.ts",
        "src/dir/index.ts",
        "src/alias.ts",
        "src/lazy.ts",
    ] {
        std::fs::write(project.path().join(path), "export default 1;\n")
            .unwrap_or_else(|error| panic!("import target fixture failed: {error}"));
    }
    std::fs::write(
        project.path().join("src/main.ts"),
        "import direct from './direct';\n\
         import directory from './dir';\n\
         import alias from '@/alias';\n\
         import React from 'react';\n\
         export async function load() {\n\
           const lazy = await import ('./lazy');\n\
           const missing = require('./missing');\n\
           return [direct, directory, alias, React, lazy, missing];\n\
         }\n\
         export const generated = `import embedded from './literal-only';`;\n",
    )
    .unwrap_or_else(|error| panic!("import source fixture failed: {error}"));
    std::fs::write(
        project.path().join("src/fixtures/example.ts"),
        "import missing from './fixture-missing';\nexport { missing };\n",
    )
    .unwrap_or_else(|error| panic!("fixture import source failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("import runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("import fixture index failed: {error}"));
        let report = runtime
            .audit_imports(ImportAuditRequest::new(
                indexed.project_id.clone(),
                ImportAuditOptions::default().with_source(ImportAuditSource::All),
                ProjectCancellation::new(),
            ))
            .await
            .unwrap_or_else(|error| panic!("import audit failed: {error}"));
        let report = serde_json::to_value(report)
            .unwrap_or_else(|error| panic!("import audit serialization failed: {error}"));
        assert_eq!(report["fixtureHitsExcluded"], 1);
        assert_eq!(report["total"], 8, "unexpected import report: {report:?}");
        assert_eq!(report["matched"], 7, "unexpected import report: {report:?}");
        let hits = report["hits"]
            .as_array()
            .unwrap_or_else(|| panic!("import hits were not an array"));
        for (specifier, target) in [
            ("./direct", "file"),
            ("./dir", "directory"),
            ("@/alias", "file"),
            ("react", "bare"),
            ("./missing", "unresolvable"),
            ("./literal-only", "literal"),
        ] {
            assert!(
                hits.iter()
                    .any(|hit| { hit["specifier"] == specifier && hit["target"] == target }),
                "missing {specifier} => {target} in {hits:?}"
            );
        }
        assert!(
            hits.iter()
                .any(|hit| { hit["specifier"] == "./lazy" && hit["dynamic"] == true })
        );
        assert!(hits.iter().any(|hit| {
            hit["specifier"] == "./literal-only"
                && hit["origin"] == "literal"
                && hit["language"] == "typescript"
        }));

        let filtered_options = ImportAuditOptions::new(10)
            .unwrap_or_else(|error| panic!("import options failed: {error}"))
            .with_source(ImportAuditSource::Static)
            .with_target(Some(ImportAuditTarget::File))
            .with_extension_missing(Some(true))
            .with_dynamic(Some(false))
            .with_path_filter(Some("src/"))
            .and_then(|options| options.with_language(Some("TypeScript")))
            .unwrap_or_else(|error| panic!("import filters failed: {error}"));
        let filtered = runtime
            .audit_imports(ImportAuditRequest::new(
                indexed.project_id.clone(),
                filtered_options,
                ProjectCancellation::new(),
            ))
            .await
            .unwrap_or_else(|error| panic!("filtered import audit failed: {error}"));
        let filtered = serde_json::to_value(filtered)
            .unwrap_or_else(|error| panic!("filtered import serialization failed: {error}"));
        assert_eq!(filtered["matched"], 1);
        assert_eq!(filtered["truncated"], false);

        std::fs::write(project.path().join("src/direct.ts"), "export default 2;\n")
            .unwrap_or_else(|error| panic!("stale import fixture failed: {error}"));
        let stale = runtime
            .audit_imports(ImportAuditRequest::new(
                indexed.project_id,
                ImportAuditOptions::default(),
                ProjectCancellation::new(),
            ))
            .await;
        assert_eq!(stale, Err(ImportAuditError::SourceChanged));
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn rename_plan_combines_exact_references_and_attributed_textual_mentions() {
    let (schema, settings, project) = live_project_fixture("8");
    let source = write_rename_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("rename runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("rename fixture index failed: {error}"));
        let symbol_id = exact_symbol_id(
            &runtime,
            &indexed.project_id,
            &indexed.generation_id,
            "parseToken",
        )
        .await;
        let mut definitions = runtime
            .database()
            .current_symbols_by_ids(cartograph_db::CurrentSymbolSetLookup::new(
                &indexed.project_id,
                &indexed.generation_id,
                std::slice::from_ref(&symbol_id),
            ))
            .await
            .unwrap_or_else(|error| panic!("rename definition lookup failed: {error}"));
        let definition = definitions
            .pop()
            .unwrap_or_else(|| panic!("rename definition was missing"));
        let plan = runtime
            .plan_rename(RenamePlanRequest {
                project_id: indexed.project_id.clone(),
                definition,
                options: RenamePlanOptions::new(500, 30)
                    .unwrap_or_else(|error| panic!("rename options failed: {error}")),
                cancellation: ProjectCancellation::new(),
            })
            .await
            .unwrap_or_else(|error| panic!("rename plan failed: {error}"));
        let plan = serde_json::to_value(plan)
            .unwrap_or_else(|error| panic!("rename plan serialization failed: {error}"));
        assert_eq!(plan["definition"]["qualified_name"], "parseToken");
        assert!(plan["exactReferenceCount"].as_u64().unwrap_or_default() >= 1);
        assert!(
            plan["exactReferences"]
                .as_array()
                .is_some_and(|references| {
                    references.iter().any(|reference| {
                        reference["path"] == "service.ts"
                            && reference["line"] == 2
                            && reference["representedSite"] == "parseToken"
                    })
                })
        );
        assert_eq!(plan["textualMentionCount"], 2);
        let mentions = plan["textualMentions"]
            .as_array()
            .unwrap_or_else(|| panic!("textual mentions were not an array"));
        assert!(mentions.iter().all(|mention| {
            mention["enclosingQualifiedName"] == "documentation"
                && mention["confidence"] == "textual_review_required"
        }));
        assert!(!mentions.iter().any(|mention| {
            mention["text"]
                .as_str()
                .is_some_and(|text| text.contains("parseTokenValue"))
        }));
        assert_eq!(plan["editsApplied"], false);

        std::fs::write(
            &source,
            "export function parseToken(): boolean { return false; }\n",
        )
        .unwrap_or_else(|error| panic!("stale rename fixture failed: {error}"));
        let stale_definition = runtime
            .database()
            .current_symbols_by_ids(cartograph_db::CurrentSymbolSetLookup::new(
                &indexed.project_id,
                &indexed.generation_id,
                std::slice::from_ref(&symbol_id),
            ))
            .await
            .unwrap_or_else(|error| panic!("stale definition lookup failed: {error}"))
            .pop()
            .unwrap_or_else(|| panic!("stale definition was missing"));
        let stale = runtime
            .plan_rename(RenamePlanRequest {
                project_id: indexed.project_id,
                definition: stale_definition,
                options: RenamePlanOptions::new(500, 30)
                    .unwrap_or_else(|error| panic!("stale rename options failed: {error}")),
                cancellation: ProjectCancellation::new(),
            })
            .await;
        assert_eq!(stale, Err(RenamePlanError::SourceChanged));
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

async fn assert_file_test_impact_node_budget(
    runtime: &ProjectRuntime,
    indexed: &IndexReport,
    leaf: &NormalizedPath,
) {
    let capped = runtime
        .database()
        .current_file_test_impact(FileTestImpactQuery {
            project_id: &indexed.project_id,
            paths: std::slice::from_ref(leaf),
            max_depth: 5,
            max_nodes: 1,
            limit: 40,
            test_path_regex: None,
        })
        .await
        .unwrap_or_else(|error| panic!("capped test-impact query failed: {error}"))
        .unwrap_or_else(|| panic!("capped test-impact generation was missing"));
    assert!(capped.nodes_truncated());
    let capped_json = serde_json::to_value(capped)
        .unwrap_or_else(|error| panic!("capped test-impact serialization failed: {error}"));
    assert_eq!(capped_json["nodesTruncated"], true);
    assert_eq!(capped_json["testsTruncated"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn changed_file_test_impact_traverses_named_imports_and_reports_barrels() {
    let (schema, settings, project) = live_project_fixture("8");
    write_test_impact_fixture(project.path());

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("test-impact runtime connect failed: {error}"));
        let indexed = runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("test-impact fixture index failed: {error}"));
        let title_hits = runtime
            .database()
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&indexed.project_id, &indexed.generation_id),
                "returns leaf value",
                10,
            ))
            .await
            .unwrap_or_else(|error| panic!("test-title BM25 query failed: {error}"));
        assert!(title_hits.iter().any(|hit| {
            hit.path() == "src/leaf.test.ts"
                && hit.document_kind() == "test"
                && hit
                    .components()
                    .contains(&cartograph_db::SearchComponent::NaturalText)
        }));
        let leaf = NormalizedPath::parse("src/leaf.ts")
            .unwrap_or_else(|error| panic!("test-impact path failed: {error}"));
        let missing = NormalizedPath::parse("src/missing.ts")
            .unwrap_or_else(|error| panic!("missing path fixture failed: {error}"));
        let impact = runtime
            .database()
            .current_file_test_impact(FileTestImpactQuery {
                project_id: &indexed.project_id,
                paths: &[leaf.clone(), missing],
                max_depth: 5,
                max_nodes: 40,
                limit: 40,
                test_path_regex: None,
            })
            .await
            .unwrap_or_else(|error| panic!("test-impact query failed: {error}"))
            .unwrap_or_else(|| panic!("test-impact generation was missing"));
        assert_eq!(impact.generation_id(), &indexed.generation_id);
        assert_eq!(impact.matched_inputs(), ["src/leaf.ts"]);
        assert!(impact.dependent_file_count() >= 2);
        assert_eq!(impact.affected_test_file_count(), 1);
        assert!(!impact.nodes_truncated());
        assert_eq!(impact.tests().len(), 1);
        assert_eq!(impact.tests()[0].path(), "src/leaf.test.ts");
        assert!(impact.tests()[0].distance() >= 1);
        assert_eq!(impact.reached_barrel_count(), 1);
        assert_eq!(impact.reached_barrels(), ["src/index.ts"]);
        assert_file_test_impact_node_budget(&runtime, &indexed, &leaf).await;
        let names = runtime
            .test_evidence(
                indexed.project_id.clone(),
                indexed.generation_id.clone(),
                vec![
                    NormalizedPath::parse("src/leaf.test.ts")
                        .unwrap_or_else(|error| panic!("test evidence path failed: {error}")),
                ],
                TestEvidenceOptions::new(20)
                    .unwrap_or_else(|error| panic!("test evidence options failed: {error}")),
                ProjectCancellation::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("test evidence query failed: {error}"));
        let names = serde_json::to_value(names)
            .unwrap_or_else(|error| panic!("test evidence serialization failed: {error}"));
        let cases = names["files"][0]["cases"]
            .as_array()
            .unwrap_or_else(|| panic!("test cases were not an array"));
        assert!(cases.iter().any(|case| case["name"] == "leaf API"));
        assert!(
            cases
                .iter()
                .any(|case| case["name"] == "returns the leaf value")
        );

        let filtered = runtime
            .database()
            .current_file_test_impact(FileTestImpactQuery {
                project_id: &indexed.project_id,
                paths: &[leaf],
                max_depth: 5,
                max_nodes: 40,
                limit: 40,
                test_path_regex: Some(".*\\.spec\\.ts"),
            })
            .await
            .unwrap_or_else(|error| panic!("filtered test-impact query failed: {error}"))
            .unwrap_or_else(|| panic!("filtered test-impact generation was missing"));
        assert_eq!(filtered.affected_test_file_count(), 0);
        assert!(filtered.tests().is_empty());
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn file_drift_distinguishes_content_hash_from_mtime_threshold_semantics() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::create_dir_all(project.path().join(".cartograph"))
        .unwrap_or_else(|error| panic!("file-drift policy directory failed: {error}"));
    std::fs::create_dir_all(project.path().join(".generated"))
        .unwrap_or_else(|error| panic!("file-drift excluded fixture directory failed: {error}"));
    std::fs::write(
        project.path().join(".cartograph/config.json"),
        r#"{"exclude":["**/.generated/**"]}"#,
    )
    .unwrap_or_else(|error| panic!("file-drift source policy failed: {error}"));
    std::fs::write(project.path().join("a.ts"), "export const a = 1;\n")
        .unwrap_or_else(|error| panic!("file-drift a fixture failed: {error}"));
    std::fs::write(project.path().join("b.ts"), "export const b = 2;\n")
        .unwrap_or_else(|error| panic!("file-drift b fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("file-drift runtime connect failed: {error}"));
        runtime
            .index(IndexOptions::default())
            .await
            .unwrap_or_else(|error| panic!("file-drift fixture index failed: {error}"));
        std::fs::write(project.path().join("a.ts"), "export const a = 9;\n")
            .unwrap_or_else(|error| panic!("file-drift modification failed: {error}"));
        std::fs::remove_file(project.path().join("b.ts"))
            .unwrap_or_else(|error| panic!("file-drift deletion failed: {error}"));
        std::fs::write(project.path().join("c.ts"), "export const c = 3;\n")
            .unwrap_or_else(|error| panic!("file-drift addition failed: {error}"));
        std::fs::write(
            project.path().join(".generated/types.ts"),
            "export interface Generated {}\n",
        )
        .unwrap_or_else(|error| panic!("file-drift excluded addition failed: {error}"));
        let report = runtime
            .file_drift(FileDriftOptions::default(), ProjectCancellation::new())
            .await
            .unwrap_or_else(|error| panic!("content file-drift failed: {error}"));
        let report = serde_json::to_value(report)
            .unwrap_or_else(|error| panic!("content file-drift serialization failed: {error}"));
        assert_eq!(report["basis"], "indexed_content_hash");
        assert_eq!(report["addedCount"], 1);
        assert_eq!(report["modifiedCount"], 1);
        assert_eq!(report["deletedCount"], 1);
        assert_eq!(report["added"], serde_json::json!(["c.ts"]));
        assert_eq!(report["modified"], serde_json::json!(["a.ts"]));
        assert_eq!(report["deleted"], serde_json::json!(["b.ts"]));

        let future = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("clock failed: {error}"))
            .as_millis()
            .saturating_add(86_400_000);
        let future = u64::try_from(future)
            .unwrap_or_else(|error| panic!("future threshold failed: {error}"));
        let threshold = runtime
            .file_drift(
                FileDriftOptions::since(future)
                    .unwrap_or_else(|error| panic!("threshold options failed: {error}")),
                ProjectCancellation::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("threshold file-drift failed: {error}"));
        let threshold = serde_json::to_value(threshold)
            .unwrap_or_else(|error| panic!("threshold serialization failed: {error}"));
        assert_eq!(threshold["basis"], "modification_time");
        assert_eq!(threshold["modifiedCount"], 0);
        assert_eq!(threshold["addedCount"], 1);
        assert_eq!(threshold["deletedCount"], 1);
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn oversized_names_shorten_and_deep_files_publish_a_partial_generation() {
    let (schema, settings, project) = live_project_fixture("8");
    let source_path = project.path().join("repro.rs");
    // A single ordinary construct used to synthesize a name past the canonical
    // storage bound and discard the whole generation (issues #118 and #119).
    let long_target = "reference_target_".repeat(300);
    let oversized_source = format!("pub fn trigger() {{ {long_target}(); }}\n");
    std::fs::write(&source_path, &oversized_source)
        .unwrap_or_else(|error| panic!("reference-bound fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("reference-bound runtime connect failed: {error}"));
        let options = IndexOptions::default()
            .with_force(true)
            .with_history_refresh(false);
        let shortened = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("oversized reference name must still publish: {error}"));
        assert!(
            shortened.published,
            "one over-long synthesized name must never discard the generation"
        );

        // The stored name is bounded, deterministic, and marked as shortened.
        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("shortened-name connection failed: {error}"));
        let widest: i64 = query(AssertSqlSafe(format!(
            r#"SELECT COALESCE(MAX(octet_length(reference_name)), 0)::bigint
                FROM "{schema}"."references"
                WHERE project_id = CAST($1 AS uuid)"#
        )))
        .bind(shortened.project_id.as_str())
        .fetch_one(&pool)
        .await
        .and_then(|row| row.try_get::<i64, _>(0))
        .unwrap_or_else(|error| panic!("shortened-name probe failed: {error}"));
        pool.close().await;
        assert!(
            widest > 0 && widest <= 4_096,
            "stored reference names must fit the canonical bound, saw {widest}"
        );

        let published_generation = shortened.generation_id.clone();

        // Defensive syntax depth is file-local and recoverable: the exact path
        // is reported while the rest of the generation still publishes.
        let deep = format!(
            "pub fn nested() {{{}{}}}\n",
            "{ ".repeat(400),
            "} ".repeat(400)
        );
        std::fs::write(&source_path, deep)
            .unwrap_or_else(|error| panic!("nesting fixture failed: {error}"));
        let recovered = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("deep source must publish as partial: {error}"));
        assert!(recovered.published);
        let native = recovered
            .native
            .as_ref()
            .unwrap_or_else(|| panic!("deep-source native metrics were missing"));
        assert_eq!(native.degraded_files_truncated, 0);
        assert_eq!(native.degraded_files.len(), 1);
        assert_eq!(native.degraded_files[0].path, "repro.rs");
        assert_eq!(native.degraded_files[0].reason, "nesting_limit_exceeded");
        let rendered = format!("{recovered:?}");
        assert!(
            !rendered.contains(&long_target)
                && !rendered.contains(&project.path().to_string_lossy().to_string()),
            "a partial report must never render source text or a checkout path"
        );
        let retained = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("retained-generation status failed: {error}"));
        assert_eq!(
            retained
                .snapshot
                .and_then(|snapshot| snapshot.current)
                .map(|current| current.generation_id),
            Some(recovered.generation_id.clone()),
            "the recoverable generation must become current"
        );
        assert_ne!(published_generation, recovered.generation_id);
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
async fn profiled_index_skips_oversized_sources_without_weakening_the_published_graph() {
    let (schema, settings, project) = live_project_fixture("8");
    std::fs::write(
        project.path().join("graph.rs"),
        "pub fn caller() { callee(); }\npub fn callee() {}\n",
    )
    .unwrap_or_else(|error| panic!("profile graph fixture failed: {error}"));
    std::fs::write(
        project.path().join("oversized.rs"),
        "pub fn deliberately_skipped() {}\n".repeat(64),
    )
    .unwrap_or_else(|error| panic!("profile oversized fixture failed: {error}"));

    {
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("profile runtime connect failed: {error}"));
        let options = IndexOptions::default()
            .with_max_source_bytes(256)
            .unwrap_or_else(|error| panic!("profile source bound failed: {error}"))
            .with_profile(true)
            .with_history_refresh(false);
        let first = runtime
            .index(options.clone())
            .await
            .unwrap_or_else(|error| panic!("profile index failed: {error}"));
        let first_json = serde_json::to_value(&first)
            .unwrap_or_else(|error| panic!("profile report serialization failed: {error}"));
        assert!(first.published);
        assert_eq!(first_json["native"]["files"], 1);
        assert_eq!(first_json["native"]["skipped_oversized_files"], 1);
        assert_eq!(first_json["history"]["state"], "unavailable");
        assert_eq!(first_json["history"]["reason"], "not_attempted");
        assert_eq!(first_json["profile"]["historyMillis"], 0);
        let stages = first_json["profile"]["pipelineStages"]
            .as_array()
            .unwrap_or_else(|| panic!("profile stages were missing"));
        for expected in [
            "discover", "read", "parse", "resolve", "reduce", "copy", "publish",
        ] {
            assert!(
                stages.iter().any(|stage| stage["stage"] == expected),
                "profile omitted {expected}: {first_json}"
            );
        }
        let graph = runtime
            .database()
            .current_interchange_snapshot(InterchangeSnapshotRequest {
                project_id: &first.project_id,
                maximum_rows: 100,
                statement_timeout: Duration::from_secs(30),
            })
            .await
            .unwrap_or_else(|error| panic!("profile graph snapshot failed: {error}"));
        assert_eq!(graph.files.len(), 1);
        assert!(
            graph
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "caller")
        );
        assert!(
            graph
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "callee")
        );
        assert!(graph.edges.iter().any(|edge| edge.edge_kind == "calls"));

        let unchanged = runtime
            .index(options)
            .await
            .unwrap_or_else(|error| panic!("profile no-op index failed: {error}"));
        let unchanged_json = serde_json::to_value(&unchanged)
            .unwrap_or_else(|error| panic!("profile no-op serialization failed: {error}"));
        assert!(!unchanged.published);
        assert_eq!(unchanged.generation_id, first.generation_id);
        assert_eq!(
            unchanged_json["profile"]["pipelineStages"],
            serde_json::json!([])
        );
        assert_eq!(unchanged_json["profile"]["historyMillis"], 0);
        runtime.close().await;
    }
    drop_schema(&settings, &schema).await;
}

fn write_rename_fixture(root: &Path) -> std::path::PathBuf {
    let source = root.join("service.ts");
    std::fs::write(
        &source,
        "export function parseToken(value: string): boolean { return value.length > 0; }\n\
         export function caller(value: string): boolean { return parseToken(value); }\n\
         export function documentation(): string {\n\
           // parseToken is part of the public authentication example.\n\
           return 'Call parseToken before authorizing a request';\n\
         }\n\
         export const parseTokenValue = 1;\n",
    )
    .unwrap_or_else(|error| panic!("rename fixture write failed: {error}"));
    source
}

fn write_test_impact_fixture(root: &Path) {
    let source = root.join("src");
    std::fs::create_dir(&source)
        .unwrap_or_else(|error| panic!("test-impact source directory failed: {error}"));
    std::fs::write(
        source.join("leaf.ts"),
        "export function leaf(): number { return 1; }\n",
    )
    .unwrap_or_else(|error| panic!("test-impact leaf fixture failed: {error}"));
    std::fs::write(
        source.join("index.ts"),
        "export { leaf } from './leaf.js';\n",
    )
    .unwrap_or_else(|error| panic!("test-impact barrel fixture failed: {error}"));
    std::fs::write(
        source.join("leaf.test.ts"),
        "import { describe, it } from 'vitest';\n\
         import { leaf } from './index.js';\n\
         describe('leaf API', () => {\n\
           it('returns the leaf value', () => { leaf(); });\n\
         });\n",
    )
    .unwrap_or_else(|error| panic!("test-impact test fixture failed: {error}"));
}

async fn exact_symbol_id(
    runtime: &ProjectRuntime,
    project_id: &cartograph_domain::ProjectId,
    generation_id: &cartograph_domain::GenerationId,
    qualified_name: &str,
) -> cartograph_domain::SymbolId {
    let matches = runtime
        .database()
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(project_id, generation_id),
            qualified_name,
            10,
        ))
        .await
        .unwrap_or_else(|error| panic!("exact graph symbol lookup failed: {error}"));
    let exact = matches
        .iter()
        .filter(|symbol| symbol.qualified_name() == qualified_name)
        .collect::<Vec<_>>();
    assert_eq!(
        exact.len(),
        1,
        "graph fixture symbol was not uniquely indexed: {qualified_name}"
    );
    exact[0].symbol_id().clone()
}

fn embedding_fixture_server(requests: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("embedding fixture bind failed: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("embedding fixture address failed: {error}"));
    let server = thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("embedding fixture accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap_or_else(|error| panic!("embedding fixture timeout failed: {error}"));
            let request = read_http_request(&mut stream);
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map_or_else(
                    || panic!("embedding fixture request has no body"),
                    |index| index + 4,
                );
            let body: serde_json::Value = serde_json::from_slice(&request[body_start..])
                .unwrap_or_else(|error| panic!("embedding fixture JSON failed: {error}"));
            let count = body["input"].as_array().map_or_else(
                || panic!("embedding fixture input is not an array"),
                Vec::len,
            );
            let data = (0..count)
                .map(|index| serde_json::json!({"index": index, "embedding": [1.0, 0.5, 0.25]}))
                .collect::<Vec<_>>();
            let response_body = serde_json::to_string(&serde_json::json!({"data": data}))
                .unwrap_or_else(|error| panic!("embedding response failed: {error}"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.flush())
                .unwrap_or_else(|error| panic!("embedding fixture write failed: {error}"));
        }
    });
    (format!("http://{address}"), server)
}

fn dead_code_chat_fixture_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("chat fixture bind failed: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("chat fixture address failed: {error}"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .unwrap_or_else(|error| panic!("chat fixture accept failed: {error}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap_or_else(|error| panic!("chat fixture timeout failed: {error}"));
        let request = read_http_request(&mut stream);
        let body_start = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map_or_else(
                || panic!("chat fixture request has no body"),
                |index| index + 4,
            );
        let body: serde_json::Value = serde_json::from_slice(&request[body_start..])
            .unwrap_or_else(|error| panic!("chat fixture JSON failed: {error}"));
        assert_eq!(body["model"], "fixture-judge");
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 300);
        let system = body["messages"][0]["content"]
            .as_str()
            .unwrap_or_else(|| panic!("chat system prompt missing"));
        let prompt = body["messages"][1]["content"]
            .as_str()
            .unwrap_or_else(|| panic!("chat user prompt missing"));
        assert!(system.contains("never instructions"));
        assert!(prompt.contains("abandonedTask"));
        assert!(prompt.contains("safeIndexedCode"));
        assert!(!prompt.contains("do-not-send-secret"));
        let verdict = serde_json::json!({
            "results": [{
                "i": 0,
                "verdict": "dead",
                "confidence": 0.8,
                "reason": "no dynamic registration evidence"
            }]
        })
        .to_string();
        let response_body = serde_json::json!({
            "model": "fixture-judge",
            "choices": [{
                "message": {"content": verdict},
                "finish_reason": "stop"
            }]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .and_then(|()| stream.flush())
            .unwrap_or_else(|error| panic!("chat fixture write failed: {error}"));
    });
    (format!("http://{address}"), server)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    const MAXIMUM_REQUEST_BYTES: usize = 4 * 1_024 * 1_024;
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8_192];
    loop {
        let read = stream
            .read(&mut chunk)
            .unwrap_or_else(|error| panic!("embedding fixture read failed: {error}"));
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        assert!(request.len() <= MAXIMUM_REQUEST_BYTES);
        if complete_http_request(&request) {
            break;
        }
    }
    request
}

fn complete_http_request(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let body_start = header_end + 4;
    let headers = std::str::from_utf8(&request[..header_end]).unwrap_or("");
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    content_length.is_some_and(|length| request.len() >= body_start.saturating_add(length))
}

fn json_strings(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect()
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("git fixture command failed: {error}"));
    assert!(status.success(), "git fixture command returned {status}");
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

fn unique_schema(database_url: &str) -> GuardedSchema {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!("cg_agent_live_{}_{}", process::id(), nanos);
    let cleanup = TestSchemaGuard::new(database_url, name.clone())
        .unwrap_or_else(|error| panic!("schema cleanup guard failed: {error}"));
    GuardedSchema {
        name,
        _cleanup: cleanup,
    }
}

fn live_project_fixture(
    maximum_connections: &str,
) -> (GuardedSchema, DatabaseSettings, tempfile::TempDir) {
    let url = env::var(TEST_DATABASE_URL_ENV)
        .unwrap_or_else(|_| panic!("live project test database is not configured"));
    let schema = unique_schema(&url);
    let settings = DatabaseSettings::parse(&url, Some(maximum_connections), Some("10000"))
        .and_then(|value| value.with_schema(&schema))
        .unwrap_or_else(|error| panic!("live project database settings failed: {error}"));
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    (schema, settings, project)
}

async fn drop_schema(settings: &DatabaseSettings, schema: &str) {
    let pool = cartograph_db::connect(settings)
        .await
        .unwrap_or_else(|error| panic!("cleanup connection failed: {error}"));
    query(AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap_or_else(|_| panic!("cleanup schema failed"));
    pool.close().await;
}
