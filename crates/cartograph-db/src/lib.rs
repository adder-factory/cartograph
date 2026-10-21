//! Per-project `RocksDB` persistence and typed graph services for Cartograph.

mod artifacts;
mod centrality;
mod coverage;
mod database;
mod file_intelligence;
mod history;
mod ingest;
mod insights;
mod interchange;
mod issue_history;
mod native_database;
mod numerical;
pub use native_database::NativeDatabase;
pub use native_database::dataset::NativeAuxiliaryPruneReport;
pub use native_database::retention::NativeDerivedPruneReport;
mod parse_cache;
mod project;
mod qualified;
mod retrieval;
pub mod rocks;
mod search;
mod semantic;
mod sessions;
mod structural_findings;
mod summary_priority;

pub use artifacts::{
    AgentArtifactContent, AgentArtifactKind, AgentArtifactQuery, AgentArtifactRecord,
    AgentArtifactScope, AgentArtifactState, AgentRoleCount, CurrentModuleSummary,
    CurrentModuleSummaryPage, FileSummarySaveRequest, ModuleSummaryRollupItem,
    ModuleSummarySaveRequest, NeighborSummarySaveInput, NeighborSummarySaveRequest,
    NeighborSummarySource, NewAgentArtifact, PendingFileSummary, PendingModelSummaryQuery,
    PendingModuleSummary, PendingNeighborSummary, PendingNeighborSummaryQuery, PendingRoleSymbol,
    PendingStructuralSummary, PendingStructuralSummaryQuery, PendingSummaryRollupQuery,
    PendingSummarySymbol, RoleSweepModel, StructuralRoleCursor, StructuralRolePage,
    StructuralSummaryEdge, StructuralSymbolSummarySaveInput, SummaryCandidatePolicy,
    SummaryCoverageStats, SummaryRollupItem, SummarySaveInput, SymbolRoleSaveInput,
    SymbolSummarySaveInput,
};
pub use centrality::{
    BetweennessError, BetweennessReport, PageRankError, PageRankReport, SymbolBetweennessScore,
    SymbolPageRankScore, apply_page_rank, apply_sampled_betweenness,
};
pub use coverage::{
    CoverageCount, CoverageLoadInput, CoverageLoadReport, CoverageLoadRequest,
    CoverageSourceRecord, CoverageStats, CoverageTarget, SymbolCoverageFact, SymbolCoverageQuery,
    SymbolCoverageRecord,
};
pub use file_intelligence::{
    FileAggregateResult, FileDependencyDirection, FileDependencyQuery, FileDependencyResult,
    FileDependencyRow, FileDirectoryAggregate, FileLanguageAggregate, FileSurfaceQuery,
    FileSurfaceResult, FileSurfaceRow,
};
pub use history::{
    FileCochangeFact, FileCochangeMetrics, FileCochangeQuery, FileCochangeRecord, FileHistoryFact,
    FileHistoryMetrics, FileHistoryQuery, FileHistoryRecord, HistoryRefreshInput,
    HistoryRefreshMetadata, HistoryRefreshParameters, HistoryRefreshReport, HistoryRefreshRequest,
    HistoryReuseQuery,
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
pub use numerical::{
    NumericalSitePage, NumericalSiteQuery, NumericalSiteRecord, NumericalSiteStats,
};
pub use parse_cache::{
    MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES, NativeParseCacheBatchWrite,
    NativeParseCacheBudgetPressure, NativeParseCacheEntry, NativeParseCacheError,
    NativeParseCacheKey, NativeParseCacheKeyInput, NativeParseCacheRecord,
    NativeParseCacheRetentionCapacity, NativeParseCacheRetentionPolicy,
    NativeParseCacheRetentionPolicyInput, NativeParseCacheRetentionReport, NativeParseCacheStats,
    NativeParseCacheWrite,
};
pub use project::{
    GenerationCounts, GenerationSourceAdmission, GenerationStorageSummary,
    ProjectCurrentGeneration, ProjectSnapshot,
};
pub use qualified::{
    QualifiedCentralityComparator, QualifiedSymbolHit, QualifiedSymbolPage, QualifiedSymbolQuery,
    QualifiedSymbolSort,
};
pub use retrieval::{
    CurrentEntryPointPage, CurrentEntryPointsLookup, CurrentFileLookup, CurrentFileRecord,
    CurrentFileSymbolsLookup, CurrentFilesLookup, CurrentGenerationLookup, CurrentGenerationRecord,
    CurrentGraphEdge, CurrentGraphLookup, CurrentReferenceRecord, CurrentSourceRangeLookup,
    CurrentSymbolRecord, CurrentSymbolSetLookup, EntryPointBucket, ExactTextLookup, GraphDirection,
    SourceLineRange,
};
pub use search::{SearchComponent, SearchHit, SearchQuery};
pub use semantic::{
    EmbeddingBatchUpsertInput, EmbeddingBatchUpsertReport, EmbeddingBatchUpsertRequest,
    EmbeddingIndexStatus, EmbeddingModelRegistration, EmbeddingModelRegistrationInput,
    EmbeddingModelSelector, EmbeddingModelState, EmbeddingNormalization, EmbeddingPageCursor,
    EmbeddingStorageAudit, EmbeddingUpsertRow, PendingEmbeddingDocument, PendingEmbeddingPage,
    PendingEmbeddingPageInput, PendingEmbeddingPageRequest, RegisteredEmbeddingModel,
    RerankDocument, RerankDocumentsRequest, RetireEmbeddingModelRequest,
    RetiredEmbeddingCleanupReport, SemanticReadinessReport, SemanticReadinessRequest,
    SemanticReadinessState, SemanticStorageError, SimilarSymbolHit, SimilarSymbolsInput,
    SimilarSymbolsRequest, SimilarSymbolsResult, SimilarityMaterializationPolicy,
    SimilarityMaterializationReport, VectorSearchHit, VectorSearchInput, VectorSearchRequest,
};
pub use sessions::{
    McpMacroRecord, McpMacroStep, McpSessionCallsQuery, McpSessionKind, McpSessionLookup,
    McpSessionRecord, McpToolCallData, McpToolCallInput, McpToolCallRecord, McpToolCallWrite,
    McpToolUsage, McpTraceUsage, NewMcpMacro, NewMcpSession,
};
pub use structural_findings::StructuralFindingRefresh;
pub use summary_priority::{
    SummaryPriorityEnqueueReport, SummaryPriorityFailure, SummaryPriorityQueueStats,
};

pub use database::StorageError;
pub use native_database::NativeDatabase as CartographDatabase;
