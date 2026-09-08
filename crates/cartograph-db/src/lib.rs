//! PostgreSQL-only persistence and capability checks for Cartograph v2.

#[cfg(test)]
use cartograph_test_support as _;

mod adhoc;
mod artifacts;
mod capabilities;
mod centrality;
mod compaction;
mod coverage;
mod database;
mod derived_prune;
mod file_intelligence;
mod generation;
mod history;
mod ingest;
mod insights;
mod interchange;
mod issue_history;
mod leases;
mod managed;
mod migrations;
mod numerical;
mod parse_cache;
mod project;
mod qualified;
mod retention;
mod retrieval;
mod search;
mod search_relation;
mod semantic;
mod sessions;
mod spill;
mod storage;
mod structural_findings;
mod summary_priority;
mod v1_import;

use std::str::FromStr;

pub use adhoc::{
    ReadOnlySqlError, ReadOnlySqlRelation, ReadOnlySqlRequest, ReadOnlySqlResult, ReadOnlySqlRow,
    read_only_sql_schema,
};
pub use artifacts::{
    AgentArtifactContent, AgentArtifactKind, AgentArtifactQuery, AgentArtifactRecord,
    AgentArtifactScope, AgentArtifactState, AgentRoleCount, CurrentModuleSummary,
    CurrentModuleSummaryPage, FileSummarySaveRequest, ModuleSummaryRollupItem,
    ModuleSummarySaveRequest, NeighborSummarySaveInput, NeighborSummarySaveRequest,
    NeighborSummarySource, NewAgentArtifact, PendingFileSummary, PendingModelSummaryQuery,
    PendingModuleSummary, PendingNeighborSummary, PendingNeighborSummaryQuery, PendingRoleSymbol,
    PendingStructuralSummary, PendingStructuralSummaryQuery, PendingSummaryRollupQuery,
    PendingSummarySymbol, StructuralSummaryEdge, StructuralSymbolSummarySaveInput,
    SummaryCandidatePolicy, SummaryCoverageStats, SummaryRollupItem, SummarySaveInput,
    SymbolRoleSaveInput, SymbolSummarySaveInput,
};
pub use capabilities::{
    CapabilityCheck, CapabilityReport, CheckStatus, probe_capabilities, probe_capabilities_bounded,
};
use cartograph_config::DatabaseSettings;
pub use centrality::{
    BetweennessError, BetweennessReport, PageRankError, PageRankReport, SymbolBetweennessScore,
    SymbolPageRankScore, apply_page_rank, apply_sampled_betweenness,
};
pub use compaction::{
    HeapCompactionCandidate, HeapCompactionPlan, HeapCompactionPolicy, HeapCompactionPolicyInput,
    HeapCompactionReport, HeapCompactionResult, InvalidIndexArtifact, StorageCompactionCandidate,
    StorageCompactionError, StorageCompactionPlan, StorageCompactionPolicy,
    StorageCompactionPolicyInput, StorageCompactionReport, StorageCompactionStopReason,
};
pub use coverage::{
    CoverageCount, CoverageLoadInput, CoverageLoadReport, CoverageLoadRequest,
    CoverageSourceRecord, CoverageStats, CoverageTarget, SymbolCoverageFact, SymbolCoverageQuery,
    SymbolCoverageRecord,
};
pub use database::{CartographDatabase, StorageError};
pub use derived_prune::{
    DerivedStorePruneError, DerivedStorePrunePolicy, DerivedStorePruneReport,
    DerivedStorePruneRequest,
};
pub use file_intelligence::{
    FileAggregateResult, FileDependencyDirection, FileDependencyQuery, FileDependencyResult,
    FileDependencyRow, FileDirectoryAggregate, FileLanguageAggregate, FileSurfaceQuery,
    FileSurfaceResult, FileSurfaceRow,
};
pub use generation::{
    CurrentGeneration, FailGenerationError, FailedGeneration, GenerationContents,
    GenerationRecoveryRequest, NewGeneration, NewProject, ObservedGeneration, ObservedLease,
    OperationReconciliation, PrepareGenerationError, PrepareGenerationMetrics,
    PrepareGenerationMetricsSnapshot, PrepareGenerationMutation, PrepareGenerationProgress,
    PublishGenerationError, ReadyGeneration, RecoverableGeneration, SpilledGenerationContents,
    StagedGeneration, TerminalGenerationMutation,
};
pub use history::{
    FileCochangeFact, FileCochangeMetrics, FileCochangeQuery, FileCochangeRecord, FileHistoryFact,
    FileHistoryMetrics, FileHistoryQuery, FileHistoryRecord, HistoryRefreshInput,
    HistoryRefreshMetadata, HistoryRefreshReport, HistoryRefreshRequest,
};
pub use ingest::{
    CanonicalGenerationFacts, CanonicalSearchDocument, EdgeInput, FileInput, GenerationFacts,
    GenerationMemoryMeasurement, GenerationMemoryModelError, GenerationValidationError,
    GenerationValidationLimits, GenerationValidationReport, NumericalSiteInput, ReferenceInput,
    ReferenceSpanPrecision, SearchDocumentInput, SymbolInput, validate_generation_facts,
};
pub use insights::{
    DEFAULT_INSIGHT_TIMEOUT, DeadCodeCandidate, DeadCodeQuery, DependencyCoverageRow,
    ExternalImportRecord, FileTestImpact, FileTestImpactQuery, FileTestImpactResult,
    GroupedPathInput, GroupedSymbolPeer, GroupedSymbolPeers, GroupedSymbolQuery, ImportInsight,
    IndexedFileFingerprint, RenameReferenceSite, StructuralCoverageRow, StructuralFinding,
    StructuralFindingGroup, StructuralFindingGroupQuery, StructuralFindingQuery,
    StructuralFindingSeverity, StructuralFindingStats, StructuralHotspot,
    StructuralHotspotCategory, StructuralHotspotQuery, StructuralHotspotSort,
};
pub use interchange::{
    InterchangeEdge, InterchangeFile, InterchangeReference, InterchangeSnapshot,
    InterchangeSnapshotError, InterchangeSnapshotRequest, InterchangeSymbol,
};
pub use issue_history::{
    IssueAttributionKind, IssueCommitSymbolPeerQuery, IssueHistoryError, IssueHistoryRefreshInput,
    IssueHistoryRefreshMetadata, IssueHistoryRefreshMetadataInput, IssueHistoryRefreshReport,
    IssueHistoryRefreshRequest, SymbolIssueAttribution, SymbolIssueAttributionInput,
    SymbolIssueCommitPeer, SymbolIssueCommitPeers, SymbolIssuePeer, SymbolIssuePeerQuery,
    SymbolIssueQuery, SymbolIssueRecord,
};
pub use leases::{
    LeaseAcquisitionAttempt, LeaseAcquisitionProbe, LeaseError, LeaseFence, LeaseOwner,
    LeaseRequest, LeaseStatus, LeaseTarget, ProjectLease,
};
pub use managed::{
    DEFAULT_MANAGED_DATABASE_PORT, MANAGED_DATABASE_IMAGE, MANAGED_DATABASE_MEMORY_LIMIT_BYTES,
    MANAGED_DATABASE_MEMORY_RESERVATION_BYTES, MANAGED_DATABASE_NANO_CPUS,
    MANAGED_DATABASE_PIDS_LIMIT, MANAGED_DATABASE_SHARED_MEMORY_BYTES, ManagedBackupReport,
    ManagedContainerState, ManagedDatabase, ManagedDatabaseArchives, ManagedDatabaseError,
    ManagedDatabaseLifecycle, ManagedDatabaseMaintenance, ManagedDatabaseStatus,
    ManagedDerivedIndexAvailability, ManagedDerivedIndexHealth, ManagedDestructiveConfirmation,
    ManagedDestructiveOperation, ManagedRemoveReport, ManagedRestoreReport, ManagedStartReport,
    ManagedUpgradeReport,
};
pub use migrations::{MigrationError, MigrationReport, latest_schema_version};
pub use numerical::{
    NumericalSitePage, NumericalSiteQuery, NumericalSiteRecord, NumericalSiteStats,
};
pub use parse_cache::{
    MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES, NativeParseCacheBatchWrite,
    NativeParseCacheBudgetPressure, NativeParseCacheEntry, NativeParseCacheError,
    NativeParseCacheKey, NativeParseCacheKeyInput, NativeParseCacheRecord,
    NativeParseCacheRetentionCapacity, NativeParseCacheRetentionPolicy,
    NativeParseCacheRetentionPolicyInput, NativeParseCacheRetentionReport,
    NativeParseCacheRetentionRequest, NativeParseCacheStats, NativeParseCacheWrite,
};
pub use project::{
    GenerationCounts, GenerationSourceAdmission, GenerationStorageSummary,
    ProjectCurrentGeneration, ProjectPurgeError, ProjectPurgeReport, ProjectPurgeRequest,
    ProjectSnapshot,
};
pub use qualified::{
    QualifiedCentralityComparator, QualifiedSymbolHit, QualifiedSymbolPage, QualifiedSymbolQuery,
    QualifiedSymbolSort,
};
pub use retention::{
    GenerationRetentionAttempt, GenerationRetentionError, GenerationRetentionPolicy,
    GenerationRetentionReport, GenerationRetentionRequest, PostRetentionMaintenance,
    PostRetentionMaintenancePolicy,
};
pub use retrieval::{
    CurrentEntryPointPage, CurrentEntryPointsLookup, CurrentFileLookup, CurrentFileRecord,
    CurrentFileSymbolsLookup, CurrentFilesLookup, CurrentGenerationLookup, CurrentGenerationRecord,
    CurrentGraphEdge, CurrentGraphLookup, CurrentReferenceRecord, CurrentSourceRangeLookup,
    CurrentSymbolRecord, CurrentSymbolSetLookup, EntryPointBucket, ExactTextLookup, GraphDirection,
    SourceLineRange,
};
pub use search::{SearchComponent, SearchHit, SearchQuery};
pub use search_relation::SearchRelationMaintenanceReport;
use secrecy::ExposeSecret;
pub use semantic::{
    EmbeddingBatchUpsertInput, EmbeddingBatchUpsertReport, EmbeddingBatchUpsertRequest,
    EmbeddingHnswStatus, EmbeddingModelRegistration, EmbeddingModelRegistrationInput,
    EmbeddingModelSelector, EmbeddingModelState, EmbeddingNormalization, EmbeddingPageCursor,
    EmbeddingStorageAudit, EmbeddingUpsertRow, PendingEmbeddingDocument, PendingEmbeddingPage,
    PendingEmbeddingPageInput, PendingEmbeddingPageRequest, RegisteredEmbeddingModel,
    RetireEmbeddingModelRequest, RetiredEmbeddingCleanupReport, SemanticReadinessReport,
    SemanticReadinessRequest, SemanticReadinessState, SemanticStorageError, SimilarSymbolHit,
    SimilarSymbolsInput, SimilarSymbolsRequest, SimilarSymbolsResult,
    SimilarityMaterializationPolicy, SimilarityMaterializationReport, VectorSearchHit,
    VectorSearchInput, VectorSearchRequest,
};
pub use sessions::{
    McpMacroRecord, McpMacroStep, McpSessionCallsQuery, McpSessionKind, McpSessionLookup,
    McpSessionRecord, McpToolCallData, McpToolCallInput, McpToolCallRecord, McpToolCallWrite,
    McpToolUsage, McpTraceUsage, NewMcpMacro, NewMcpSession,
};
pub use spill::{
    CachedRowPayload, FactBatchInput, NativeGenerationExtractedCursor,
    NativeGenerationExtractedPage, NativeGenerationSpill, NativeGenerationSpillCachedRow,
    NativeGenerationSpillCanonicalProgress, NativeGenerationSpillCentralityScore,
    NativeGenerationSpillDigestReport, NativeGenerationSpillExtractedBatch,
    NativeGenerationSpillExtractedRow, NativeGenerationSpillFactBatch,
    NativeGenerationSpillFactCounts, NativeGenerationSpillPolicy, NativeGenerationSpillRelation,
    NativeGenerationSpillReport, NativeGenerationSpillRequest, NativeGenerationSpillRow,
    NativeGenerationSpillState, NativeGenerationSpillWrite,
};
use sqlx_core::{pool::PoolOptions, query::query};
use sqlx_postgres::{PgConnectOptions, PgPool, PgSslMode, Postgres};
pub use storage::{
    GenerationDeduplicationAssessment, GenerationRetentionSnapshot, IndexStorageUsage,
    ParseCacheStorageUsage, StorageStatisticsObservation, StorageTotalsReport, StorageUsageOffsets,
    StorageUsagePage, StorageUsageReport, StorageWarning, TableStorageUsage,
};
pub use structural_findings::StructuralFindingRefresh;
pub use summary_priority::{
    SummaryPriorityEnqueueReport, SummaryPriorityFailure, SummaryPriorityQueueStats,
};
use thiserror::Error;
pub use v1_import::{
    V1PostgresDryRunReport, V1PostgresImportCheckpoint, V1PostgresImportCounts,
    V1PostgresImportError, V1PostgresImportExecution, V1PostgresImportLimits,
    V1PostgresImportReport, V1PostgresImportRequest, V1PostgresSource, V1PostgresSourceRevision,
};

/// Connect to the configured PostgreSQL database with a bounded pool.
/// # Errors
///
/// Returns an error if the redacted connection URL is invalid, the bounded
/// pool cannot connect, or per-connection timeout/session setup fails.
pub async fn connect(settings: &DatabaseSettings) -> Result<PgPool, DatabaseError> {
    let mut options = PgConnectOptions::from_str(settings.url().expose_secret())
        .map_err(|_| DatabaseError::InvalidConnectionOptions)?
        .application_name("cartograph-v2");
    if settings.require_ssl() {
        options = options.ssl_mode(PgSslMode::Require);
    }
    let query_timeout_ms = settings.query_timeout().as_millis().to_string();

    PoolOptions::<Postgres>::new()
        .max_connections(settings.max_connections().get())
        .acquire_timeout(settings.acquire_timeout())
        .after_connect(move |connection, _metadata| {
            let query_timeout_ms = query_timeout_ms.clone();
            Box::pin(async move {
                query("SELECT set_config('statement_timeout', $1, false)")
                    .bind(format!("{query_timeout_ms}ms"))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .map_err(|_| DatabaseError::ConnectionFailed)
}

/// Database failures whose public rendering cannot expose credentials.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// The validated URL could not be converted into driver options.
    #[error("PostgreSQL connection options are invalid")]
    InvalidConnectionOptions,
    /// Connection or authentication failed.
    #[error("could not connect to PostgreSQL; verify the server, credentials, and TLS settings")]
    ConnectionFailed,
    /// A capability probe query failed unexpectedly.
    #[error("PostgreSQL capability probe failed during {check}")]
    CapabilityProbe {
        /// Stable name of the failed probe.
        check: &'static str,
    },
}
