//! Stable project runtime for the Rust/PostgreSQL Cartograph product.
//!
//! This crate owns project identity, freshness, bounded worker selection, and
//! the complete source-to-publication operation. CLI and MCP adapters consume
//! this public service instead of reaching into database or indexer internals.

#[cfg(test)]
use cartograph_test_support as _;
#[cfg(test)]
use sha2 as _;
#[cfg(test)]
use sqlx_core as _;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cartograph_config::DatabaseSettings;
use cartograph_config::{
    ProjectConfigError, ProjectGenerationStorage, ProjectSourceSettings,
    load_project_source_settings,
};
use cartograph_db::NativeGenerationSpillReport;
use cartograph_db::{
    CartographDatabase, GenerationContents, GenerationRecoveryRequest, GenerationRetentionAttempt,
    GenerationRetentionPolicy, GenerationRetentionReport, GenerationRetentionRequest,
    HistoryRefreshReport, IssueHistoryRefreshReport, LeaseError, LeaseRequest, LeaseTarget,
    MigrationError, NativeGenerationSpillPolicy, NativeParseCacheRetentionPolicy,
    NativeParseCacheRetentionReport, NativeParseCacheRetentionRequest, NewGeneration, NewProject,
    PostRetentionMaintenancePolicy, ProjectSnapshot, SpilledGenerationContents, StagedGeneration,
};
use cartograph_domain::{
    ContentDigest, GenerationDigestVersion, NormalizedPath, ProjectId, ProjectOperation,
    SourceLanguage, SourceManifestDigestBuilder, project_root_identity,
};
use cartograph_extract::{
    DiscoveryLimits, DiscoveryPolicy, NestedRepositoryPolicy, SourceDiscoveryOptions, SourceLimits,
    SourceReadError, SourceReadOptions, SourceRoot,
};
use cartograph_indexer::{
    IndexerSupervisor, NativeGenerationBuild, NativeGenerationStorage, NativeParseCache,
    NativePipelineConfig, NativePipelineDeadlines, NativePipelineLimits, NativePipelineParallelism,
    NativePipelineReport, NativeRetainedLimits, PipelineFailure, PipelineStageTiming,
    ScipOverlayInput, StageCapacity, SupervisorConfig, SupervisorContext, SupervisorRequest,
    build_native_generation_spilled, build_native_generation_with_scip_and_cache,
    native_parse_cache_contract_digest,
};
pub use cartograph_indexer::{
    PipelineFailureReason, PipelineFileFailure, PipelineStage, SupervisorStatus,
};
use cartograph_scip::ScipOverlayReport;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::task::JoinHandle;

use index_failure::{
    StagingPreflight, SupervisorFailureContext, recover_abandoned_staging, supervisor_index_failure,
};

mod compare;
mod coverage;
mod dead_code;
mod decision_batch;
mod decision_rename;
mod decision_rerank;
mod decision_roles;
mod dependencies;
mod diff_review;
mod drift;
mod embeddings;
mod git_intelligence;
mod history;
mod imports;
mod index_admission;
mod index_failure;
mod issue_history;
mod layering;
mod navigation;
mod rename;
mod retrieval;
mod review;
mod scip_interchange;
mod source_context;
mod source_search;
mod structural_summaries;
mod test_intelligence;
mod verification;
mod working_tree;

pub use structural_summaries::{
    STRUCTURAL_SUMMARY_MODEL, STRUCTURAL_SUMMARY_PAGE_SIZE, StructuralSummaryReport,
    cap_structural_summary, run_structural_summary_sweep,
};

pub use compare::{
    SourceCompareError, SourceCompareOptions, SourceCompareReport, SourceEdgeDelta,
    SourceFileDelta, SourceFinding, SourceFindingsDelta, SourceSymbolDelta,
    discover_source_comparison,
};
pub use coverage::{CoverageError, LcovLoadOptions, LcovLoadReport};
pub use dead_code::{
    DeadCodeJudgeError, DeadCodeJudgeOptions, DeadCodeJudgeReport, DeadCodeJudgeRequest,
    DeadCodeJudgement, DeadCodeVerdict, judge_dead_code_candidates,
};
pub use decision_rename::{RenameTriageEvidence, RenameTriageState};
pub use decision_roles::{JEV_ROLE_MODEL, JudgedRole, RoleCandidate, RoleJudgement, RoleVerdict};
pub use dependencies::{
    DependencyAuditError, DependencyAuditReport, DependencyUseEvidence, UndeclaredDependency,
};
pub use diff_review::{
    DiffCochangeWarning, DiffFileKind, DiffHunkContext, DiffReviewError, DiffReviewInput,
    DiffReviewOptions, DiffReviewReport, DiffSymbolContext, UnifiedDiffComparison, UnifiedDiffFile,
    UnifiedDiffHunk, parse_unified_diff,
};
pub use drift::{
    FileDriftBasis, FileDriftError, FileDriftOptions, FileDriftReport, MAXIMUM_UNIX_MILLISECONDS,
};
pub use embeddings::{
    EmbeddingClientRequest, EmbeddingOptions, EmbeddingStatusReport, EmbeddingSweepReport,
};
pub use git_intelligence::{
    GitBlameLine, GitCommitPathSet, GitHistoryCommit, GitLineHistory, GitLineHistoryRequest,
    GitLineRange, GitPathHistory, GitRenameEvidence, TraceCulprit, TraceCulpritReport,
    discover_git_blame, discover_git_commit_paths, discover_git_history, discover_git_line_history,
    discover_git_rename_evidence, trace_git_culprits,
};
pub use history::{HistoryIndexError, HistoryIndexOptions};
pub use imports::{
    ImportAuditError, ImportAuditOptions, ImportAuditReport, ImportAuditRequest, ImportAuditSource,
    ImportAuditTarget, ImportHit, ImportOrigin,
};
pub use index_failure::IndexFailure;
pub use issue_history::{
    IssueHistoryIndexError, IssueHistoryIndexOptions, IssueHistoryIndexRequest,
};
pub use layering::{LayerAnalysisError, LayerAnalysisReport, LayerViolation};
pub use navigation::{NavigationPolicy, NavigationReport, NavigationRequest};
pub use rename::{
    RenamePlan, RenamePlanError, RenamePlanOptions, RenamePlanRequest, RenameReferenceEvidence,
    RenameTextualMention,
};
pub use retrieval::{
    PreparedRetrieval, RetrievalClientRequest, RetrievalOptions, RetrievalRequest,
    semantic_readiness_from_database,
};
pub use review::{
    GitChangeKind, GitChangedFile, GitComparison, ReviewError, ReviewOptions, ReviewReport,
    discover_git_comparison,
};
pub use scip_interchange::{
    ScipExportReport, ScipExportRequest, ScipImportLimits, ScipImportReport, ScipImportRequest,
};
pub use source_context::{
    FileSourceContext, FileSourceExcerpt, FileSourceOptions, FileSourceRequest,
    SourceContextOptions, SourceContextRequest, SourceExcerpt, SymbolSourceContext,
};
pub use source_search::{
    SourceSearchError, SourceSearchHit, SourceSearchOptions, SourceSearchReport,
};
pub use test_intelligence::{
    TestCaseEvidence, TestEvidenceError, TestEvidenceOptions, TestEvidenceReport, TestFileEvidence,
};
pub use verification::{VerificationCommand, VerificationPlan};
pub use working_tree::WorkingTreeOverlayRequest;

const PROJECT_IDENTITY_DOMAIN: &[u8] = b"cartograph-v2-project-root-v1";
const PROCESS_OWNER_DOMAIN: &[u8] = b"cartograph-v2-process-owner-v1";
const SOURCE_SCIP_OVERLAY_DOMAIN: &[u8] = b"cartograph-v2-source-scip-overlay-v1";
const SOURCE_INDEX_POLICY_DOMAIN: &[u8] = b"cartograph-v2-source-index-policy-v3";
const SCIP_OVERLAY_RELATIVE_PATH: &str = ".cartograph/scip/overlay.scip";
const MAXIMUM_SCIP_OVERLAY_BYTES: usize = 256 * 1024 * 1024;
const MAXIMUM_SCIP_OVERLAY_ROWS: usize = 10_000_000;
const SCIP_READ_BUFFER_BYTES: usize = 64 * 1024;

const DEFAULT_MAX_FILES: usize = 250_000;
const DEFAULT_MAX_PATH_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_GENERATION_BYTES: u64 = 1024 * 1024 * 1024;
const AUTO_SPILL_MINIMUM_FILES: usize = 10_000;
const AUTO_SPILL_MINIMUM_CARGO_MANIFESTS: usize = 64;
const AUTO_SPILL_MINIMUM_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const AUTO_SPILL_EXPANSION_FACTOR: u64 = 16;
const DEFAULT_MAX_SUPERVISOR_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const DEFAULT_MAX_SUPERVISOR_TASKS: usize = 128;
const MAX_CONFIGURED_WORKERS: u16 = 16;
const MAX_CONCURRENT_SOURCE_SCANS: usize = 1;
const SINGLE_WORKER_MAX_FILES: usize = 8;
const TWO_WORKER_MAX_FILES: usize = 16;
const FOUR_WORKER_MAX_FILES: usize = 32;
const EIGHT_WORKER_MAX_FILES: usize = 128;
const SINGLE_WORKER_MAX_SOURCE_BYTES: u64 = 128 * 1_024;
const TWO_WORKER_MAX_SOURCE_BYTES: u64 = 512 * 1_024;
const FOUR_WORKER_MAX_SOURCE_BYTES: u64 = 1_024 * 1_024;
const EIGHT_WORKER_MAX_SOURCE_BYTES: u64 = 4 * 1_024 * 1_024;
const ONE_WORKER: u16 = 1;
const TWO_WORKERS: u16 = 2;
const FOUR_WORKERS: u16 = 4;
const EIGHT_WORKERS: u16 = 8;
const DEFAULT_ITEM_TIMEOUT: Duration = Duration::from_mins(5);
const DEFAULT_STAGE_TIMEOUT: Duration = Duration::from_mins(90);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_hours(2);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PROGRESS_TIMEOUT: Duration = Duration::from_mins(10);
const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(10);
const DEFAULT_COPY_TIMEOUT: Duration = Duration::from_mins(3);
const DEFAULT_LEASE_DURATION: Duration = Duration::from_mins(5);
const DEFAULT_STAGING_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_SOURCE_RECONCILIATION_ATTEMPTS: usize = 2;
const AUTOMATIC_RETENTION_KEEP_SUPERSEDED: u32 = 2;
const AUTOMATIC_RETENTION_MAXIMUM_DELETIONS: u32 = 32;
const AUTOMATIC_RETENTION_LEASE_DURATION: Duration = Duration::from_mins(2);
const AUTOMATIC_RETENTION_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);
const AUTOMATIC_RETENTION_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const AUTOMATIC_RETENTION_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const AUTOMATIC_RETENTION_MAINTENANCE: PostRetentionMaintenancePolicy =
    PostRetentionMaintenancePolicy::DelegateToAutovacuum;
const OVERSIZED_SOURCE_DIGEST_DOMAIN: &[u8] = b"cartograph-v2-oversized-source-v1";

pub(crate) fn trim_ascii_bytes(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

/// User-controlled bounds for one full source index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOptions {
    max_workers: u16,
    force: bool,
    max_source_bytes: Option<usize>,
    profile: bool,
    refresh_history: bool,
    failure_retention: IndexFailureRetention,
    additional_excludes: Vec<String>,
    admission_reconciliation: IndexAdmissionReconciliation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexFailureRetention {
    SuccessfulRequestsOnly,
    AutomaticFailures,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexAdmissionReconciliation {
    Replace,
    PreserveCurrent,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            max_workers: MAX_CONFIGURED_WORKERS,
            force: false,
            max_source_bytes: None,
            profile: false,
            refresh_history: true,
            failure_retention: IndexFailureRetention::SuccessfulRequestsOnly,
            additional_excludes: Vec::new(),
            admission_reconciliation: IndexAdmissionReconciliation::Replace,
        }
    }
}

impl IndexOptions {
    /// Low-resource policy for automatic background reconciliation.
    ///
    /// The structural graph remains complete, while the independent Git
    /// history passes are left for an explicit index and native extraction is
    /// capped at four workers to avoid competing with the editor or coding
    /// agent that caused the change.
    #[must_use]
    pub fn automatic() -> Self {
        Self {
            max_workers: FOUR_WORKERS,
            refresh_history: false,
            failure_retention: IndexFailureRetention::AutomaticFailures,
            admission_reconciliation: IndexAdmissionReconciliation::PreserveCurrent,
            ..Self::default()
        }
    }

    /// Policy for explicit reconciliation workflows such as upgrade and
    /// sync-if-dirty. When no new run exclusions are supplied, the current
    /// generation's admission policy is inherited.
    #[must_use]
    pub fn reconciliation() -> Self {
        Self {
            admission_reconciliation: IndexAdmissionReconciliation::PreserveCurrent,
            ..Self::default()
        }
    }

    /// Add run-scoped exclusion globs on top of the project's configured
    /// `exclude` list. A `!` prefix re-includes a path an exclusion matched.
    ///
    /// The exact list is persisted on a published generation so freshness and
    /// drift use the same admission policy. A later explicit index without
    /// run exclusions clears it; automatic reconciliation inherits it.
    #[must_use]
    pub fn with_additional_excludes(mut self, patterns: Vec<String>) -> Self {
        self.additional_excludes = patterns;
        self
    }

    /// Run-scoped exclusion globs layered over the configured policy.
    #[must_use]
    pub fn additional_excludes(&self) -> &[String] {
        &self.additional_excludes
    }

    /// Cap the corpus-aware worker selector at a validated value in 1..=16.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `value` is zero or exceeds
    /// the maximum supported indexing worker count.
    pub fn with_max_workers(mut self, value: u16) -> Result<Self, ProjectError> {
        if value == 0 || value > MAX_CONFIGURED_WORKERS {
            return Err(ProjectError::InvalidOptions);
        }
        self.max_workers = value;
        Ok(self)
    }

    /// Rebuild and publish even when the exact source manifest is already current.
    #[must_use]
    pub const fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Restrict the maximum admitted bytes for any one source file.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `value` is zero or exceeds
    /// the hard per-file source byte ceiling.
    pub fn with_max_source_bytes(mut self, value: usize) -> Result<Self, ProjectError> {
        if value == 0 || value > DEFAULT_MAX_SOURCE_BYTES {
            return Err(ProjectError::InvalidOptions);
        }
        self.max_source_bytes = Some(value);
        Ok(self)
    }

    /// Retain exact monotonic preparation, stage, history, and wall timings.
    #[must_use]
    pub const fn with_profile(mut self, profile: bool) -> Self {
        self.profile = profile;
        self
    }

    /// Refresh auxiliary Git churn/co-change evidence alongside the structural index.
    ///
    /// Semantic-only maintenance can disable this independent pass when it only needs
    /// to reuse or rebuild the complete current graph before generating vectors.
    #[must_use]
    pub const fn with_history_refresh(mut self, refresh_history: bool) -> Self {
        self.refresh_history = refresh_history;
        self
    }
}

/// Fixed-size evidence from the native extraction pipeline.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeIndexMetrics {
    /// Supported files in the indexed source manifest.
    pub files: u64,
    /// Paths a configured exclusion glob or `.cartographignore` pattern skipped.
    ///
    /// A partial index must never look complete, so an exclusion is reported
    /// even when it is exactly what the project asked for.
    pub excluded_paths: u64,
    /// Directory subtrees skipped without being descended into. A pruned tree is
    /// deliberately not expanded into a file count.
    pub excluded_trees: u64,
    /// Supported files deliberately skipped because they exceeded maxFileSize.
    pub skipped_oversized_files: u64,
    /// Exact bytes read and hashed.
    pub source_bytes: u64,
    /// Extracted code symbols.
    pub symbols: u64,
    /// Generation-scoped static numerical source sites.
    pub numerical_sites: u64,
    /// References resolved to one exact symbol.
    pub resolved_references: u64,
    /// Explicit unresolved evidence retained without guessing.
    pub unresolved_references: u64,
    /// Recoverable parser diagnostics.
    pub diagnostics: u64,
    /// Bounded exact paths retained as partial after a recoverable extractor limit.
    pub degraded_files: Vec<NativeDegradedFileMetrics>,
    /// Additional degraded paths omitted after the report bound.
    pub degraded_files_truncated: u64,
    /// Conservative retained canonical-generation bytes.
    pub modeled_generation_bytes: u64,
    /// Conservative cumulative allocation charge observed while resolving facts.
    pub resolve_high_water_bytes: u64,
    /// Conservative cumulative allocation charge observed while canonicalizing the payload.
    pub validation_high_water_bytes: u64,
    /// Persistent SCIP replacement accounting, absent when no overlay is configured.
    pub scip_overlay: Option<ScipOverlayMetrics>,
    /// PostgreSQL incremental parse-cache evidence.
    pub parse_cache: NativeParseCacheMetrics,
    /// Working-set strategy selected for native construction.
    pub generation_storage: NativeGenerationStorageMetrics,
    /// Fixed-size durable spill accounting, absent for memory construction.
    pub spill: Option<NativeGenerationSpillMetrics>,
}

/// One source file retained as partial instead of aborting its generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeDegradedFileMetrics {
    /// Exact normalized project-relative source path.
    pub path: String,
    /// Stable source-safe recovery category.
    pub reason: &'static str,
}

/// Public native working-set strategy used by one completed build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeGenerationStorageMetrics {
    /// Complete generation canonicalization stayed in Rust memory.
    #[default]
    Memory,
    /// Bulky extraction and resolved facts were reduced in PostgreSQL.
    Postgres,
}

/// Fixed-size quota and admitted-row evidence for PostgreSQL generation spill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeGenerationSpillMetrics {
    /// Sort-key plus serialized payload bytes admitted before canonical reduction.
    pub logical_bytes: u64,
    /// Raw extraction and fact rows admitted.
    pub raw_rows: u64,
    /// File-local extraction payloads durably retained.
    pub extracted_files: u64,
    /// Configured logical spill byte quota.
    pub maximum_bytes: u64,
    /// Configured raw spill row quota.
    pub maximum_rows: u64,
}

/// Exact cache activity proving how much source was reparsed without weakening global resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeParseCacheMetrics {
    /// Number of hits.
    pub hits: u64,
    /// Number of misses.
    pub misses: u64,
    /// Number of bypassed.
    pub bypassed: u64,
    /// Number of parsed files.
    pub parsed_files: u64,
    /// Number of writes.
    pub writes: u64,
    /// Number of corruptions.
    pub corruptions: u64,
    /// Number of read errors.
    pub read_errors: u64,
    /// Number of write errors.
    pub write_errors: u64,
}

/// Public accounting for a persistent SCIP per-file replacement overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScipOverlayMetrics {
    /// Project files covered by valid SCIP documents.
    pub covered_documents: u64,
    /// Invalid, stale, duplicate, or non-project SCIP documents ignored.
    pub skipped_documents: u64,
    /// Native non-file symbols replaced in covered files.
    pub replaced_native_symbols: u64,
    /// SCIP symbol facts admitted.
    pub imported_symbols: u64,
    /// Resolved typed graph edges admitted.
    pub imported_edges: u64,
    /// Exact source occurrences admitted.
    pub imported_references: u64,
    /// Exact typed edges recovered from Cartograph's SCIP extension.
    pub exact_typed_edges: u64,
    /// Foreign targets that could not be resolved without guessing.
    pub unresolved_links: u64,
}

impl From<ScipOverlayReport> for ScipOverlayMetrics {
    fn from(overlay: ScipOverlayReport) -> Self {
        Self {
            covered_documents: overlay.covered_documents(),
            skipped_documents: overlay.skipped_documents(),
            replaced_native_symbols: overlay.replaced_native_symbols(),
            imported_symbols: overlay.imported_symbols(),
            imported_edges: overlay.imported_edges(),
            imported_references: overlay.imported_references(),
            exact_typed_edges: overlay.exact_typed_edges(),
            unresolved_links: overlay.unresolved_links(),
        }
    }
}

impl From<NativeGenerationSpillReport> for NativeGenerationSpillMetrics {
    fn from(spill: NativeGenerationSpillReport) -> Self {
        Self {
            logical_bytes: spill.logical_bytes,
            raw_rows: spill.raw_rows,
            extracted_files: spill.extracted_files,
            maximum_bytes: spill.maximum_bytes,
            maximum_rows: spill.maximum_rows,
        }
    }
}

struct NativeAuxiliaryMetrics {
    scip_overlay: Option<ScipOverlayMetrics>,
    parse_cache: NativeParseCacheMetrics,
    generation_storage: NativeGenerationStorageMetrics,
    spill: Option<NativeGenerationSpillMetrics>,
}

impl From<&NativePipelineReport> for NativeAuxiliaryMetrics {
    fn from(report: &NativePipelineReport) -> Self {
        let cache = report.parse_cache();
        Self {
            scip_overlay: report.scip_overlay().map(ScipOverlayMetrics::from),
            parse_cache: NativeParseCacheMetrics {
                hits: cache.hits(),
                misses: cache.misses(),
                bypassed: cache.bypassed(),
                parsed_files: cache.parsed_files(),
                writes: cache.writes(),
                corruptions: cache.corruptions(),
                read_errors: cache.read_errors(),
                write_errors: cache.write_errors(),
            },
            generation_storage: report.storage().into(),
            spill: report.spill().map(NativeGenerationSpillMetrics::from),
        }
    }
}

impl From<NativeGenerationStorage> for NativeGenerationStorageMetrics {
    fn from(storage: NativeGenerationStorage) -> Self {
        match storage {
            NativeGenerationStorage::Memory => Self::Memory,
            NativeGenerationStorage::PostgreSql => Self::Postgres,
        }
    }
}

impl From<NativePipelineReport> for NativeIndexMetrics {
    fn from(report: NativePipelineReport) -> Self {
        let auxiliary = NativeAuxiliaryMetrics::from(&report);
        Self {
            files: report.discovered_files(),
            excluded_paths: report.excluded_paths(),
            excluded_trees: report.excluded_trees(),
            skipped_oversized_files: report.skipped_oversized_files(),
            source_bytes: report.source_bytes(),
            symbols: report.symbols(),
            numerical_sites: report.numerical_sites(),
            resolved_references: report.resolved_references(),
            unresolved_references: report.unresolved_references(),
            diagnostics: report.diagnostics(),
            degraded_files: report
                .degraded_files()
                .iter()
                .map(|file| NativeDegradedFileMetrics {
                    path: file.path().to_owned(),
                    reason: file.reason(),
                })
                .collect(),
            degraded_files_truncated: report.degraded_files_truncated(),
            modeled_generation_bytes: report.modeled_generation_bytes(),
            resolve_high_water_bytes: report.resolve_high_water_bytes(),
            validation_high_water_bytes: report.validation_high_water_bytes(),
            scip_overlay: auxiliary.scip_overlay,
            parse_cache: auxiliary.parse_cache,
            generation_storage: auxiliary.generation_storage,
            spill: auxiliary.spill,
        }
    }
}

/// Terminal result for an index request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    /// Stable project identity.
    pub project_id: ProjectId,
    /// Visible immutable generation identity.
    pub generation_id: cartograph_domain::GenerationId,
    /// Source-manifest digest used for freshness checks.
    pub source_revision: ContentDigest,
    /// Exact parsing-policy identity protected by automatic cache maintenance.
    pub parse_cache_contract_digest: ContentDigest,
    /// Complete logical generation digest.
    pub content_digest: ContentDigest,
    /// Number of workers selected from the bounded corpus policy.
    pub workers: u16,
    /// False when an identical source revision under the current digest contract made this a no-op.
    pub published: bool,
    /// Explicit publication outcome, including why a no-op reused the visible current generation.
    pub publication: IndexPublication,
    /// Native pipeline metrics, absent for a no-op publication.
    pub native: Option<NativeIndexMetrics>,
    /// Auxiliary Git churn/co-change indexing outcome. Git absence never invalidates code indexing.
    pub history: HistoryIndexStatus,
    /// Generation-fenced issue-tagged symbol evidence. Git absence never invalidates code indexing.
    pub issue_history: IssueHistoryIndexStatus,
    /// Exact native timing evidence, present only when explicitly requested.
    pub profile: Option<IndexProfile>,
    /// Bounded post-index generation retention outcome.
    pub retention: GenerationRetentionStatus,
}

/// Unambiguous publication outcome for an index or sync request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IndexPublication {
    /// This request atomically published a new immutable generation.
    Published,
    /// Publication was deliberately skipped because the returned generation was already current.
    Skipped {
        /// Stable machine-readable no-op reason.
        reason: &'static str,
        /// The report generation remains the visible current generation.
        generation_remains_current: bool,
    },
}

/// Automatic bounded generation-retention outcome attached to every successful index request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GenerationRetentionStatus {
    /// A bounded cleanup transaction committed under an exact migration lease.
    Completed {
        /// Generation rows removed or preserved by the retention transaction.
        report: GenerationRetentionReport,
        /// Parse-cache rows removed or preserved under the same lease.
        parse_cache: NativeParseCacheRetentionReport,
    },
    /// Cleanup committed, but releasing its bounded lease could not be confirmed.
    CompletedWithWarning {
        /// Generation rows removed or preserved by the committed transaction.
        report: GenerationRetentionReport,
        /// Parse-cache retention report, absent when that phase did not complete.
        parse_cache: Option<NativeParseCacheRetentionReport>,
        /// Stable warning describing the unconfirmed lease-release outcome.
        warning: &'static str,
    },
    /// Cache eviction committed independently while generation cleanup failed.
    CacheOnly {
        /// Committed cache progress; generation rows were not removed.
        parse_cache: NativeParseCacheRetentionReport,
        /// Stable generation-cleanup failure label.
        reason: &'static str,
        /// Whether the migration lease release was confirmed.
        lease_released: bool,
    },
    /// Cleanup was safely deferred because another writer won or storage was unavailable.
    Deferred {
        /// Stable reason the bounded cleanup could not safely start.
        reason: &'static str,
        /// Whether a later bounded index/prune attempt may safely retry.
        retryable: bool,
        /// Whether `admin unlock` can change this historical report.
        unlock_applicable: bool,
        /// Stable operator action for this outcome.
        next_action: &'static str,
    },
}

/// Monotonic phase timing evidence for one index/no-op request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProfile {
    /// Source manifest comparison plus generation staging.
    pub preparation_millis: u64,
    /// Supervisor-owned extraction, COPY, derived-index, and publication time.
    pub pipeline_millis: u64,
    /// Per-stage timings in exact native pipeline order.
    pub pipeline_stages: Vec<PipelineStageTiming>,
    /// Concurrent Git history preparation/persistence duration.
    pub history_millis: u64,
    /// Concurrent issue-tagged symbol-history preparation/persistence duration.
    pub issue_history_millis: u64,
    /// Whole request wall time; history and structural work may overlap.
    pub total_millis: u64,
}

/// Honest auxiliary Git-history outcome attached to every index request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HistoryIndexStatus {
    /// Durable history/co-change relations were refreshed for current HEAD.
    Indexed {
        /// Exact commit/file history counts written by the refresh.
        report: HistoryRefreshReport,
    },
    /// Code indexing succeeded but Git history was unavailable within its bounds.
    Unavailable {
        /// Stable source-safe reason history indexing could not run.
        reason: &'static str,
    },
}

/// Honest issue-tagged symbol-history outcome attached to every index request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IssueHistoryIndexStatus {
    /// Durable symbol/issue attributions were refreshed for the exact current generation.
    Indexed {
        /// Exact issue-attribution counts written by the refresh.
        report: IssueHistoryRefreshReport,
    },
    /// Code indexing succeeded but issue-tagged history was disabled or unavailable.
    Unavailable {
        /// Stable source-safe reason issue-history indexing could not run.
        reason: &'static str,
    },
}

/// Read-only project state with an honest live-source freshness decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectStatus {
    /// Durable project/generation state, absent before first registration.
    pub snapshot: Option<ProjectSnapshot>,
    /// Current supported-source manifest digest.
    pub live_source_revision: ContentDigest,
    /// True only when the durable generation recorded this exact manifest and digest contract.
    pub fresh: bool,
}

/// Stable checkout identity and complete supported-source revision used by migration tooling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectSourceIdentity {
    /// Privacy-preserving repository identity derived from the canonical checkout root.
    pub repository_fingerprint: ContentDigest,
    /// Complete digest of every supported source path and content hash.
    pub source_revision: ContentDigest,
    /// Exact v1.1.33-compatible path/content manifest, excluding additive v2
    /// modes while retaining the checkout's bounded source policy.
    pub v1_source_manifest: ContentDigest,
    /// Supported source files included in the revision.
    pub files: usize,
    /// Exact indexed source bytes contributing to worker selection.
    pub source_bytes: u64,
}

/// Cloneable cooperative cancellation signal for project scans and long operations.
#[derive(Clone)]
pub struct ProjectCancellation {
    sender: watch::Sender<bool>,
    index_supervisor: Arc<RwLock<Option<IndexerSupervisor>>>,
}

impl std::fmt::Debug for ProjectCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectCancellation")
            .field("cancelled", &self.is_cancelled())
            .field(
                "index_progress_attached",
                &self
                    .index_supervisor
                    .read()
                    .is_ok_and(|supervisor| supervisor.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl ProjectCancellation {
    /// Create one active request signal.
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self {
            sender,
            index_supervisor: Arc::new(RwLock::new(None)),
        }
    }

    /// Request cancellation. Repeated calls are idempotent.
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    /// Point-in-time cooperative cancellation probe.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Current native index progress when this cancellation belongs to an index job.
    pub async fn index_progress_status(&self) -> Option<SupervisorStatus> {
        let supervisor = self
            .index_supervisor
            .read()
            .ok()
            .and_then(|supervisor| supervisor.clone())?;
        Some(supervisor.status().await)
    }

    fn attach_index_supervisor(&self, supervisor: IndexerSupervisor) {
        if let Ok(mut attached) = self.index_supervisor.write() {
            *attached = Some(supervisor);
        }
    }
}

impl Default for ProjectCancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// PostgreSQL-backed service for one canonical project root.
pub struct ProjectRuntime {
    root: PathBuf,
    root_identity: String,
    repository_fingerprint: ContentDigest,
    database: CartographDatabase,
    source_scan_permits: Arc<Semaphore>,
    source_scan_observations: Arc<AtomicU64>,
    /// Set once this runtime attempted to backfill legacy generation counts.
    fact_count_backfill_attempted: Arc<AtomicBool>,
}

struct AbortTaskOnDrop {
    handle: Option<JoinHandle<()>>,
}

impl AbortTaskOnDrop {
    fn new(handle: JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn abort_and_reap(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _reaped = handle.await;
        }
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[derive(Clone, Copy)]
struct IndexEnrichmentPolicy {
    refresh_history: bool,
    history_enabled: bool,
    history_channels: HistoryEnrichmentChannels,
    issue_history_enabled: bool,
}

#[derive(Clone, Copy)]
struct HistoryEnrichmentChannels {
    churn: bool,
    co_change: bool,
}

struct TimedHistoryPreparation {
    result: Result<history::PreparedHistoryIndex, HistoryIndexError>,
    elapsed_millis: u64,
}

struct TimedIssueHistoryPreparation {
    result: Result<issue_history::PreparedIssueHistory, IssueHistoryIndexError>,
    elapsed_millis: u64,
}

struct IndexCompletion {
    report: IndexReport,
    history: Option<TimedHistoryPreparation>,
    issue_history: Option<TimedIssueHistoryPreparation>,
    policy: IndexEnrichmentPolicy,
    operation_started: Instant,
}

struct IndexSourceReconciliation<'report> {
    options: &'report IndexOptions,
    report: &'report IndexReport,
    cancellation: ProjectCancellation,
}

enum IndexAttemptOutcome {
    Complete(IndexReport),
    Published(IndexReport),
}

fn index_enrichment_policy(
    options: &IndexOptions,
    source_settings: &ProjectSourceSettings,
) -> IndexEnrichmentPolicy {
    let churn_enabled = source_settings.enable_churn();
    let co_change_enabled = source_settings.enable_co_change();
    IndexEnrichmentPolicy {
        refresh_history: options.refresh_history,
        history_enabled: options.refresh_history && (churn_enabled || co_change_enabled),
        history_channels: HistoryEnrichmentChannels {
            churn: churn_enabled,
            co_change: co_change_enabled,
        },
        issue_history_enabled: options.refresh_history && source_settings.enable_issue_history(),
    }
}

async fn run_core_index(
    runtime: &ProjectRuntime,
    options: IndexOptions,
    cancellation: ProjectCancellation,
) -> Result<IndexReport, IndexFailure> {
    for attempt in 0..=MAXIMUM_SOURCE_RECONCILIATION_ATTEMPTS {
        let mut report =
            match run_core_index_attempt(runtime, &options, cancellation.clone()).await? {
                IndexAttemptOutcome::Complete(report) => return Ok(report),
                IndexAttemptOutcome::Published(report) => report,
            };
        if index_report_matches_live_source(
            runtime,
            IndexSourceReconciliation {
                options: &options,
                report: &report,
                cancellation: cancellation.clone(),
            },
        )
        .await?
        {
            report.retention = runtime
                .maintain_generation_retention(
                    &report.project_id,
                    &report.parse_cache_contract_digest,
                )
                .await;
            return Ok(report);
        }
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled.into());
        }
        if attempt == MAXIMUM_SOURCE_RECONCILIATION_ATTEMPTS {
            return Err(ProjectError::SourceChangedDuringIndex.into());
        }
    }
    Err(ProjectError::SourceChangedDuringIndex.into())
}

async fn run_core_index_attempt(
    runtime: &ProjectRuntime,
    options: &IndexOptions,
    cancellation: ProjectCancellation,
) -> Result<IndexAttemptOutcome, IndexFailure> {
    let preparation_started = Instant::now();
    let preparation = runtime
        .prepare_index(options.clone(), cancellation.clone())
        .await?;
    let preparation_millis = monotonic_millis(preparation_started.elapsed());
    let unchanged = matches!(&preparation, IndexPreparation::Unchanged(_));
    let mut report = match preparation {
        IndexPreparation::Unchanged(report) => *report,
        IndexPreparation::Pending(pending) => {
            let cache_contract =
                native_parse_cache_contract_digest(pending.index_policy.maximum_ast_depth);
            match runtime
                .publish_index(*pending, cancellation, options.profile)
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    if options.failure_retention == IndexFailureRetention::AutomaticFailures {
                        maintain_failed_generation_retention(runtime, &cache_contract).await;
                    }
                    return Err(error);
                }
            }
        }
    };
    if let Some(profile) = report.profile.as_mut() {
        profile.preparation_millis = preparation_millis;
    }
    if unchanged {
        // Generations published before counts were persisted gain them once
        // per process; failure only leaves status on its counting fallback.
        if !runtime
            .fact_count_backfill_attempted
            .swap(true, Ordering::Relaxed)
        {
            let _backfilled = runtime
                .database
                .backfill_generation_fact_counts(&report.project_id, &report.generation_id)
                .await;
        }
        report.retention = runtime
            .maintain_generation_retention(&report.project_id, &report.parse_cache_contract_digest)
            .await;
        return Ok(IndexAttemptOutcome::Complete(report));
    }
    Ok(IndexAttemptOutcome::Published(report))
}

async fn index_report_matches_live_source(
    runtime: &ProjectRuntime,
    reconciliation: IndexSourceReconciliation<'_>,
) -> Result<bool, ProjectError> {
    let IndexSourceReconciliation {
        options,
        report,
        cancellation,
    } = reconciliation;
    let source_policy =
        project_source_policy_with_excludes(&runtime.root, options.additional_excludes())?;
    let max_source_bytes = options
        .max_source_bytes
        .or(source_policy.maximum_file_bytes)
        .unwrap_or(DEFAULT_MAX_SOURCE_BYTES);
    let source = runtime
        .scan_source_for_index(IndexSourceScan {
            cancellation,
            max_source_bytes,
            discovery_policy: source_policy.discovery,
            index_policy: source_policy.index,
        })
        .await?;
    Ok(source.digest == report.source_revision)
}

async fn prepare_optional_history(
    runtime: &ProjectRuntime,
    policy: IndexEnrichmentPolicy,
    cancellation: ProjectCancellation,
) -> Option<TimedHistoryPreparation> {
    if !policy.history_enabled {
        return None;
    }
    let started = Instant::now();
    let result = runtime
        .prepare_git_history_reusing(
            HistoryIndexOptions::default(),
            (
                policy.history_channels.churn,
                policy.history_channels.co_change,
            ),
            cancellation,
        )
        .await;
    Some(TimedHistoryPreparation {
        result,
        elapsed_millis: monotonic_millis(started.elapsed()),
    })
}

async fn prepare_optional_issue_history(
    runtime: &ProjectRuntime,
    policy: IndexEnrichmentPolicy,
    cancellation: ProjectCancellation,
) -> Option<TimedIssueHistoryPreparation> {
    if !policy.issue_history_enabled {
        return None;
    }
    let started = Instant::now();
    let result = runtime
        .prepare_issue_history(IssueHistoryIndexOptions::default(), cancellation)
        .await;
    Some(TimedIssueHistoryPreparation {
        result,
        elapsed_millis: monotonic_millis(started.elapsed()),
    })
}

async fn finalize_index_completion(
    runtime: &ProjectRuntime,
    completion: IndexCompletion,
) -> IndexReport {
    let mut report = completion.report;
    if let Some(history) = completion.history {
        report = runtime.attach_history(report, history.result).await;
        if let Some(profile) = report.profile.as_mut() {
            profile.history_millis = history.elapsed_millis;
        }
    } else if completion.policy.refresh_history && !completion.policy.history_enabled {
        report.history = match runtime
            .database
            .clear_file_history(&report.project_id)
            .await
        {
            Ok(()) => HistoryIndexStatus::Unavailable {
                reason: "disabled_by_project_config",
            },
            Err(_) => HistoryIndexStatus::Unavailable {
                reason: "storage_unavailable",
            },
        };
    }
    if let Some(issue_history) = completion.issue_history {
        report = runtime
            .attach_issue_history(report, issue_history.result)
            .await;
        if let Some(profile) = report.profile.as_mut() {
            profile.issue_history_millis = issue_history.elapsed_millis;
        }
    } else if completion.policy.refresh_history {
        report.issue_history = match runtime
            .database
            .clear_issue_history(&report.project_id, &report.generation_id)
            .await
        {
            Ok(()) => IssueHistoryIndexStatus::Unavailable {
                reason: "disabled_by_project_config",
            },
            Err(_) => IssueHistoryIndexStatus::Unavailable {
                reason: "storage_unavailable",
            },
        };
    }
    if let Some(profile) = report.profile.as_mut() {
        profile.total_millis = monotonic_millis(completion.operation_started.elapsed());
    }
    report
}

async fn cancellable_project_read<T>(
    cancellation: &ProjectCancellation,
    operation: impl Future<Output = Result<T, ProjectError>>,
) -> Result<T, ProjectError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ProjectError::RequestCancelled),
        result = operation => result,
    }
}

async fn project_status_with_cancellation(
    runtime: &ProjectRuntime,
    cancellation: ProjectCancellation,
) -> Result<ProjectStatus, ProjectError> {
    let before = cancellable_project_read(&cancellation, async {
        runtime
            .database
            .project_snapshot_by_root(&runtime.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)
    })
    .await?;
    let run_excludes = before
        .as_ref()
        .and_then(|project| project.current.as_ref())
        .map_or(&[][..], |current| current.source_admission.run_excludes());
    let mut source = runtime
        .scan_source_with_excludes(None, run_excludes, cancellation.clone())
        .await?;
    if cancellation.is_cancelled() {
        return Err(ProjectError::RequestCancelled);
    }
    let mut snapshot = cancellable_project_read(&cancellation, async {
        runtime
            .database
            .project_snapshot_by_root(&runtime.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)
    })
    .await?;
    let stable_basis = same_status_source_basis(before.as_ref(), snapshot.as_ref());
    if !stable_basis {
        let run_excludes = snapshot
            .as_ref()
            .and_then(|project| project.current.as_ref())
            .map_or(&[][..], |current| current.source_admission.run_excludes());
        source = runtime
            .scan_source_with_excludes(None, run_excludes, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let observed = cancellable_project_read(&cancellation, async {
            runtime
                .database
                .project_snapshot_by_root(&runtime.root_identity)
                .await
                .map_err(|_| ProjectError::StatusFailed)
        })
        .await?;
        if !same_status_source_basis(snapshot.as_ref(), observed.as_ref()) {
            return Ok(ProjectStatus {
                snapshot: observed,
                live_source_revision: source.digest,
                fresh: false,
            });
        }
        snapshot = observed;
    }
    let fresh = snapshot
        .as_ref()
        .and_then(|project| project.current.as_ref())
        .is_some_and(|current| {
            current.source_revision == source.digest.as_str()
                && current.digest_version == GenerationDigestVersion::CURRENT
        });
    Ok(ProjectStatus {
        snapshot,
        live_source_revision: source.digest,
        fresh,
    })
}

fn same_status_source_basis(
    left: Option<&ProjectSnapshot>,
    right: Option<&ProjectSnapshot>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) if left.project_id == right.project_id => {
            match (left.current.as_ref(), right.current.as_ref()) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    left.generation_id == right.generation_id
                        && left.source_admission.run_excludes()
                            == right.source_admission.run_excludes()
                }
                (None, Some(_)) | (Some(_), None) => false,
            }
        }
        _ => false,
    }
}

async fn index_project_with_cancellation(
    runtime: &ProjectRuntime,
    options: IndexOptions,
    cancellation: ProjectCancellation,
) -> Result<IndexReport, IndexFailure> {
    let operation_started = Instant::now();
    let source_settings =
        load_project_source_settings(&runtime.root).map_err(ProjectError::from)?;
    let policy = index_enrichment_policy(&options, &source_settings);
    let index = run_core_index(runtime, options, cancellation.clone());
    let history = prepare_optional_history(runtime, policy, cancellation.clone());
    let issue_history = prepare_optional_issue_history(runtime, policy, cancellation);
    let (report, history, issue_history) = tokio::join!(index, history, issue_history);
    Ok(finalize_index_completion(
        runtime,
        IndexCompletion {
            report: report?,
            history,
            issue_history,
            policy,
            operation_started,
        },
    )
    .await)
}

async fn attach_history_result(
    runtime: &ProjectRuntime,
    mut report: IndexReport,
    history: Result<history::PreparedHistoryIndex, HistoryIndexError>,
) -> IndexReport {
    report.history = match history {
        Ok(history) => match runtime
            .persist_git_history(report.project_id.clone(), history)
            .await
        {
            Ok(history) => HistoryIndexStatus::Indexed { report: history },
            Err(error) => HistoryIndexStatus::Unavailable {
                reason: history_unavailable_reason(error),
            },
        },
        Err(error) => HistoryIndexStatus::Unavailable {
            reason: history_unavailable_reason(error),
        },
    };
    report
}

async fn attach_issue_history_result(
    runtime: &ProjectRuntime,
    mut report: IndexReport,
    issue_history: Result<issue_history::PreparedIssueHistory, IssueHistoryIndexError>,
) -> IndexReport {
    report.issue_history = match issue_history {
        Ok(issue_history) => match runtime
            .persist_issue_history(
                report.project_id.clone(),
                report.generation_id.clone(),
                issue_history,
            )
            .await
        {
            Ok(issue_history) => IssueHistoryIndexStatus::Indexed {
                report: issue_history,
            },
            Err(error) => IssueHistoryIndexStatus::Unavailable {
                reason: issue_history_unavailable_reason(error),
            },
        },
        Err(error) => IssueHistoryIndexStatus::Unavailable {
            reason: issue_history_unavailable_reason(error),
        },
    };
    report
}

async fn maintain_generation_retention(
    runtime: &ProjectRuntime,
    project_id: &ProjectId,
    contract: &ContentDigest,
) -> GenerationRetentionStatus {
    let Ok(policy) = GenerationRetentionPolicy::new(
        AUTOMATIC_RETENTION_KEEP_SUPERSEDED,
        AUTOMATIC_RETENTION_MAXIMUM_DELETIONS,
    ) else {
        return GenerationRetentionStatus::Deferred {
            reason: "invalid_policy",
            retryable: false,
            unlock_applicable: false,
            next_action: "upgrade_or_reconfigure_cartograph",
        };
    };
    let target = LeaseTarget::new(project_id.clone(), ProjectOperation::Migration, None);
    let lease = match runtime
        .database
        .acquire_lease_bounded(
            LeaseRequest::new(target, process_owner(), AUTOMATIC_RETENTION_LEASE_DURATION),
            AUTOMATIC_RETENTION_ACQUIRE_TIMEOUT,
        )
        .await
    {
        Ok(lease) => lease,
        Err(LeaseError::Busy) => {
            return GenerationRetentionStatus::Deferred {
                reason: "project_busy",
                retryable: true,
                unlock_applicable: false,
                next_action: "retry_after_the_active_writer_completes",
            };
        }
        Err(_) => {
            return GenerationRetentionStatus::Deferred {
                reason: "lease_unavailable",
                retryable: true,
                unlock_applicable: true,
                next_action: "inspect_leases_then_retry",
            };
        }
    };
    let fence = lease.fence();
    let report = runtime
        .database
        .cleanup_generations(
            GenerationRetentionRequest::new(policy, &fence, AUTOMATIC_RETENTION_STATEMENT_TIMEOUT)
                .with_post_retention_maintenance(AUTOMATIC_RETENTION_MAINTENANCE),
        )
        .await;
    // This is an independent transaction with its own exact-fence check. A
    // generation timeout must not suppress eviction of old unpinned contracts.
    let parse_cache = tokio::time::timeout(
        AUTOMATIC_RETENTION_STATEMENT_TIMEOUT,
        runtime
            .database
            .cleanup_native_parse_cache(NativeParseCacheRetentionRequest {
                project_id,
                protected_contract_digest: contract,
                policy: NativeParseCacheRetentionPolicy::automatic(),
                fence: &fence,
                statement_timeout: AUTOMATIC_RETENTION_STATEMENT_TIMEOUT,
            }),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let recorded = matches!(
        tokio::time::timeout(
            AUTOMATIC_RETENTION_RELEASE_TIMEOUT,
            runtime.database.record_generation_retention_attempt(
                &fence,
                GenerationRetentionAttempt::new(&report, parse_cache),
                AUTOMATIC_RETENTION_RELEASE_TIMEOUT,
            )
        )
        .await,
        Ok(Ok(()))
    );
    let released = runtime
        .database
        .release_lease_bounded(&lease, AUTOMATIC_RETENTION_RELEASE_TIMEOUT)
        .await;
    generation_retention_outcome(
        report,
        parse_cache,
        RetentionFinalization {
            lease_released: released.is_ok(),
            recorded,
        },
    )
}

#[derive(Clone, Copy)]
struct RetentionFinalization {
    lease_released: bool,
    recorded: bool,
}

fn generation_retention_outcome(
    report: Result<GenerationRetentionReport, cartograph_db::GenerationRetentionError>,
    parse_cache: Option<NativeParseCacheRetentionReport>,
    finalization: RetentionFinalization,
) -> GenerationRetentionStatus {
    let RetentionFinalization {
        lease_released: released,
        recorded,
    } = finalization;
    match (report, parse_cache, released) {
        (Ok(report), Some(parse_cache), true) if recorded => GenerationRetentionStatus::Completed {
            report,
            parse_cache,
        },
        (Ok(report), Some(parse_cache), true) => GenerationRetentionStatus::CompletedWithWarning {
            report,
            parse_cache: Some(parse_cache),
            warning: "maintenance_report_unavailable",
        },
        (Ok(report), None, true) => GenerationRetentionStatus::CompletedWithWarning {
            report,
            parse_cache: None,
            warning: "parse_cache_cleanup_unavailable",
        },
        (Ok(report), parse_cache, false) => GenerationRetentionStatus::CompletedWithWarning {
            report,
            parse_cache,
            warning: "lease_release_unavailable",
        },
        (Err(_), Some(parse_cache), released) => GenerationRetentionStatus::CacheOnly {
            parse_cache,
            reason: "generation_cleanup_unavailable",
            lease_released: released,
        },
        (Err(_), None, _) => GenerationRetentionStatus::Deferred {
            reason: "cleanup_unavailable",
            retryable: true,
            unlock_applicable: false,
            next_action: "rerun_index_or_bounded_prune",
        },
    }
}

/// Whether a bounded retention attempt committed any generation cleanup.
const fn retention_made_progress(status: &GenerationRetentionStatus) -> bool {
    match status {
        GenerationRetentionStatus::Completed { report, .. }
        | GenerationRetentionStatus::CompletedWithWarning { report, .. } => {
            report.cascade_rows_removed > 0
                || report.search_relations_removed > 0
                || report.removed() > 0
        }
        GenerationRetentionStatus::CacheOnly { .. }
        | GenerationRetentionStatus::Deferred { .. } => false,
    }
}

/// A prior project's terminal-generation backlog, counted and drained through
/// its runtime for [`index_admission`].
struct RuntimeTerminalBacklog<'runtime> {
    runtime: &'runtime ProjectRuntime,
    project_id: &'runtime ProjectId,
    /// Bound whose parse-cache contract a drain protects; digested per drain.
    maximum_ast_depth: usize,
}

impl index_admission::TerminalBacklog for RuntimeTerminalBacklog<'_> {
    async fn count(&self) -> Result<u64, ProjectError> {
        self.runtime
            .database
            .terminal_generation_backlog(self.project_id)
            .await
            .map_err(|_| ProjectError::StatusFailed)
    }

    async fn drain(&self) -> bool {
        let contract = native_parse_cache_contract_digest(self.maximum_ast_depth);
        let retention =
            maintain_generation_retention(self.runtime, self.project_id, &contract).await;
        retention_made_progress(&retention)
    }
}

async fn maintain_failed_generation_retention(runtime: &ProjectRuntime, contract: &ContentDigest) {
    let Ok(Some(snapshot)) = runtime
        .database
        .project_snapshot_by_root(&runtime.root_identity)
        .await
    else {
        return;
    };
    let _status = maintain_generation_retention(runtime, &snapshot.project_id, contract).await;
}

impl ProjectRuntime {
    /// Inspect a checkout without connecting to PostgreSQL.
    /// # Errors
    ///
    /// Returns an error when the checkout root cannot be canonicalized as a
    /// directory or its bounded source/config/SCIP manifest cannot be read safely.
    pub fn inspect_source_identity(
        project_root: impl AsRef<Path>,
    ) -> Result<ProjectSourceIdentity, ProjectError> {
        let root = std::fs::canonicalize(project_root.as_ref())
            .map_err(|_| ProjectError::ProjectRootUnavailable)?;
        if !root.is_dir() {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        let source = source_revision(&root)?;
        Ok(ProjectSourceIdentity {
            repository_fingerprint: project_identity_digest(&root),
            source_revision: source.digest,
            v1_source_manifest: source.v1_source_manifest,
            files: source.files,
            source_bytes: source.source_bytes,
        })
    }

    /// Open a project and apply the append-only v2 schema after capability proof.
    /// # Errors
    ///
    /// Returns an error when the checkout root is unavailable, PostgreSQL
    /// cannot be connected, or append-only schema migration fails.
    pub async fn connect(
        project_root: impl AsRef<Path>,
        settings: &DatabaseSettings,
    ) -> Result<Self, ProjectError> {
        let root = std::fs::canonicalize(project_root.as_ref())
            .map_err(|_| ProjectError::ProjectRootUnavailable)?;
        if !root.is_dir() {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        let repository_fingerprint = project_identity_digest(&root);
        let root_identity = project_root_identity(&repository_fingerprint);
        let pool = cartograph_db::connect(settings)
            .await
            .map_err(|_| ProjectError::DatabaseUnavailable)?;
        let database = CartographDatabase::new(pool, settings.schema().clone());
        match database.migrate().await {
            Ok(_) => {}
            Err(MigrationError::SchemaVersionAhead { version }) => {
                return Err(ProjectError::SchemaVersionAhead {
                    binary_version: env!("CARGO_PKG_VERSION"),
                    database_schema_version: version,
                    supported_schema_version: cartograph_db::latest_schema_version(),
                });
            }
            Err(_) => {
                let error = match database.verify_current_schema().await {
                    Err(MigrationError::SchemaVersionBehind {
                        version,
                        required_version,
                    }) => ProjectError::SchemaMigrationBlocked {
                        database_schema_version: version,
                        required_schema_version: required_version,
                        pending_migration_version: version.saturating_add(1),
                    },
                    Err(MigrationError::SchemaVersionAhead { version }) => {
                        ProjectError::SchemaVersionAhead {
                            binary_version: env!("CARGO_PKG_VERSION"),
                            database_schema_version: version,
                            supported_schema_version: cartograph_db::latest_schema_version(),
                        }
                    }
                    _ => ProjectError::MigrationFailed,
                };
                return Err(error);
            }
        }
        Ok(Self {
            root,
            root_identity,
            repository_fingerprint,
            database,
            source_scan_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS)),
            source_scan_observations: Arc::new(AtomicU64::new(0)),
            fact_count_backfill_attempted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Open a project for diagnostics without applying schema migrations.
    ///
    /// Doctor and other read-only health checks use this boundary after an
    /// explicit capability probe. Normal runtime startup must use [`Self::connect`]
    /// so append-only migrations are applied before serving queries.
    /// # Errors
    ///
    /// Returns an error when the checkout root is unavailable or PostgreSQL
    /// cannot be connected with the supplied settings.
    pub async fn connect_read_only(
        project_root: impl AsRef<Path>,
        settings: &DatabaseSettings,
    ) -> Result<Self, ProjectError> {
        let root = std::fs::canonicalize(project_root.as_ref())
            .map_err(|_| ProjectError::ProjectRootUnavailable)?;
        if !root.is_dir() {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        let repository_fingerprint = project_identity_digest(&root);
        let root_identity = project_root_identity(&repository_fingerprint);
        let pool = cartograph_db::connect(settings)
            .await
            .map_err(|_| ProjectError::DatabaseUnavailable)?;
        Ok(Self {
            root,
            root_identity,
            repository_fingerprint,
            database: CartographDatabase::new(pool, settings.schema().clone()),
            source_scan_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS)),
            source_scan_observations: Arc::new(AtomicU64::new(0)),
            fact_count_backfill_attempted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Bind another checkout to an already migrated PostgreSQL data plane.
    ///
    /// Long-lived MCP hosts use this for cross-project calls when an explicit
    /// shared `CARTOGRAPH_DATABASE_URL` is configured. The pool and schema are
    /// cloned cheaply; project identity and all source access remain rooted in
    /// the newly canonicalized checkout.
    /// # Errors
    ///
    /// Returns [`ProjectError::ProjectRootUnavailable`] when the checkout root
    /// cannot be canonicalized as an existing directory.
    pub fn with_database(
        project_root: impl AsRef<Path>,
        database: CartographDatabase,
    ) -> Result<Self, ProjectError> {
        let root = std::fs::canonicalize(project_root.as_ref())
            .map_err(|_| ProjectError::ProjectRootUnavailable)?;
        if !root.is_dir() {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        let repository_fingerprint = project_identity_digest(&root);
        let root_identity = project_root_identity(&repository_fingerprint);
        Ok(Self {
            root,
            root_identity,
            repository_fingerprint,
            database,
            source_scan_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS)),
            source_scan_observations: Arc::new(AtomicU64::new(0)),
            fact_count_backfill_attempted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Public database service for graph/search adapters; SQL internals stay private.
    #[must_use]
    pub const fn database(&self) -> &CartographDatabase {
        &self.database
    }

    /// Privacy-preserving stable identity stored instead of the absolute checkout path.
    #[must_use]
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Canonical checkout root for explicit host/install operations.
    /// Callers must not copy it into errors, logs, or unrelated tool results.
    #[must_use]
    pub fn project_root_for_host_operations(&self) -> &Path {
        &self.root
    }

    /// Normalize a project-relative path or an existing absolute path contained by this checkout.
    /// # Errors
    ///
    /// Returns an error when the input is empty or contains NUL, a relative
    /// path is invalid, or an absolute path is missing, outside the checkout,
    /// not a file, or not representable as a normalized project path.
    pub fn normalized_project_path(&self, input: &str) -> Result<NormalizedPath, ProjectError> {
        if input.is_empty() || input.contains('\0') {
            return Err(ProjectError::InvalidOptions);
        }
        let path = Path::new(input);
        if !path.is_absolute() {
            return NormalizedPath::parse(input).map_err(|_| ProjectError::InvalidOptions);
        }
        let canonical = std::fs::canonicalize(path).map_err(|_| ProjectError::FileNotFound)?;
        if !canonical.starts_with(&self.root) || !canonical.is_file() {
            return Err(ProjectError::InvalidOptions);
        }
        let relative = canonical
            .strip_prefix(&self.root)
            .map_err(|_| ProjectError::InvalidOptions)?;
        let relative = relative.to_str().ok_or(ProjectError::InvalidOptions)?;
        NormalizedPath::parse(relative).map_err(|_| ProjectError::InvalidOptions)
    }

    /// Ensure durable project-scoped agent state can be written before the first index.
    /// This registers only stable privacy-preserving identities; it does not create a generation.
    /// # Errors
    ///
    /// Returns [`ProjectError::RegisterFailed`] when the stable project identity
    /// cannot be registered in PostgreSQL.
    pub async fn register_agent_state_project(&self) -> Result<ProjectId, ProjectError> {
        self.database
            .register_project(NewProject::new(
                self.root_identity.clone(),
                self.repository_fingerprint.clone(),
            ))
            .await
            .map_err(|_| ProjectError::RegisterFailed)
    }

    /// Inspect durable state and compare it with a complete live source manifest.
    /// # Errors
    ///
    /// Returns an error when durable project status or the bounded live source
    /// manifest cannot be read, or the source scan cannot be scheduled.
    pub async fn status(&self) -> Result<ProjectStatus, ProjectError> {
        self.status_with_cancellation(ProjectCancellation::new())
            .await
    }

    /// Inspect durable state with a cancellable, blocking-pool source scan.
    /// # Errors
    ///
    /// Returns an error when durable project status or the bounded live source
    /// manifest cannot be read, the source scan cannot be scheduled, or
    /// `cancellation` wins.
    pub async fn status_with_cancellation(
        &self,
        cancellation: ProjectCancellation,
    ) -> Result<ProjectStatus, ProjectError> {
        project_status_with_cancellation(self, cancellation).await
    }

    /// Build and atomically publish one complete generation, or return an exact no-op.
    /// # Errors
    ///
    /// Returns an error when options or source policy are invalid, source
    /// discovery fails, a generation/extraction/COPY/publication stage fails,
    /// or failed pre-publication work cannot be reconciled safely.
    pub async fn index(&self, options: IndexOptions) -> Result<IndexReport, ProjectError> {
        Box::pin(self.index_with_cancellation(options, ProjectCancellation::new())).await
    }

    /// Build and publish one generation while cooperatively reconciling cancellation.
    /// # Errors
    ///
    /// Returns an error when options or source policy are invalid, source
    /// discovery fails, a generation/extraction/COPY/publication stage fails,
    /// cancellation wins, or failed pre-publication work cannot be reconciled safely.
    pub async fn index_with_cancellation(
        &self,
        options: IndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<IndexReport, ProjectError> {
        Box::pin(index_project_with_cancellation(self, options, cancellation))
            .await
            .map_err(IndexFailure::into_error)
    }

    /// Build and publish one generation, keeping a secondary cleanup failure.
    ///
    /// The returned [`IndexFailure`] carries the same primary error as
    /// [`Self::index`] plus whether bounded cleanup of the attempt's own
    /// staging generation also failed, for surfaces that report both.
    /// # Errors
    ///
    /// Returns the primary failure under the same conditions as [`Self::index`].
    pub async fn index_with_failure_detail(
        &self,
        options: IndexOptions,
    ) -> Result<IndexReport, IndexFailure> {
        Box::pin(index_project_with_cancellation(
            self,
            options,
            ProjectCancellation::new(),
        ))
        .await
    }

    async fn attach_history(
        &self,
        report: IndexReport,
        history: Result<history::PreparedHistoryIndex, HistoryIndexError>,
    ) -> IndexReport {
        attach_history_result(self, report, history).await
    }

    async fn attach_issue_history(
        &self,
        report: IndexReport,
        issue_history: Result<issue_history::PreparedIssueHistory, IssueHistoryIndexError>,
    ) -> IndexReport {
        attach_issue_history_result(self, report, issue_history).await
    }

    async fn prepare_index(
        &self,
        options: IndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<IndexPreparation, ProjectError> {
        let mut source = self
            .prepare_index_source(&options, cancellation.clone())
            .await?;
        if let Some(unchanged) = unchanged_index_preparation(UnchangedIndexInput {
            prior: source.prior.as_ref(),
            source: &source.source,
            options: &options,
            effective_run_excludes: &source.effective_run_excludes,
            maximum_ast_depth: source.index_policy.maximum_ast_depth,
        }) {
            return Ok(unchanged);
        }
        // An unchanged checkout needs no lease; a changed one must not reserve
        // a generation that another live writer's lease would reject.
        if source.staging == StagingPreflight::ProjectBusy {
            return Err(ProjectError::IndexLeaseBusy);
        }
        self.admit_automatic_generation(&source, &options).await?;
        let reservation = self.reserve_index_generation(&mut source, &options).await?;
        Ok(IndexPreparation::Pending(Box::new(PendingIndex {
            project_id: reservation.project_id,
            generation_id: reservation.generation_id,
            source_revision: source.source.digest,
            scip_overlay: source.source.scip_overlay,
            workers: reservation.workers,
            max_source_bytes: source.max_source_bytes,
            max_generation_bytes: source.max_generation_bytes,
            parse_cache_reads: !options.force,
            discovery_policy: source.discovery_policy,
            index_policy: source.index_policy,
            generation_storage: reservation.generation_storage,
            staged: reservation.staged,
        })))
    }

    /// Apply the terminal-backlog admission policy in [`index_admission`]
    /// before an attempt reserves a generation, counting and draining the prior
    /// project's backlog through this runtime. Only automatic attempts are ever
    /// deferred, and only while bounded cleanup is still making progress.
    async fn admit_automatic_generation(
        &self,
        source: &PreparedIndexSource,
        options: &IndexOptions,
    ) -> Result<(), ProjectError> {
        let backlog = source.prior.as_ref().map(|prior| RuntimeTerminalBacklog {
            runtime: self,
            project_id: &prior.project_id,
            maximum_ast_depth: source.index_policy.maximum_ast_depth,
        });
        index_admission::admit_index_attempt(options.failure_retention, backlog.as_ref())
            .await?
            .into_result()
    }

    async fn prepare_index_source(
        &self,
        options: &IndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<PreparedIndexSource, ProjectError> {
        let prior = self
            .database
            .project_snapshot_by_root(&self.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)?;
        let effective_run_excludes = effective_run_excludes(options, prior.as_ref());
        let source_policy =
            project_source_policy_with_excludes(&self.root, &effective_run_excludes)?;
        let max_source_bytes = options
            .max_source_bytes
            .or(source_policy.maximum_file_bytes)
            .unwrap_or(DEFAULT_MAX_SOURCE_BYTES);
        let max_generation_bytes = source_policy
            .maximum_generation_bytes
            .unwrap_or(DEFAULT_MAX_GENERATION_BYTES);
        let source = self
            .scan_source_for_index(IndexSourceScan {
                cancellation: cancellation.clone(),
                max_source_bytes,
                discovery_policy: source_policy.discovery.clone(),
                index_policy: source_policy.index,
            })
            .await?;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let staging = match prior.as_ref() {
            Some(prior) => {
                recover_abandoned_staging(
                    &self.database,
                    &prior.project_id,
                    DEFAULT_STAGING_CLEANUP_TIMEOUT,
                )
                .await?
            }
            None => StagingPreflight::Recovered,
        };
        Ok(PreparedIndexSource {
            prior,
            staging,
            source,
            effective_run_excludes,
            max_source_bytes,
            max_generation_bytes,
            generation_storage_policy: source_policy.generation_storage,
            discovery_policy: source_policy.discovery,
            index_policy: source_policy.index,
        })
    }

    async fn reserve_index_generation(
        &self,
        source: &mut PreparedIndexSource,
        options: &IndexOptions,
    ) -> Result<IndexGenerationReservation, ProjectError> {
        let workers = select_worker_count(
            source.source.files,
            source.source.source_bytes,
            options.max_workers,
        );
        let generation_storage = select_generation_storage(
            source.generation_storage_policy,
            GenerationStorageSignals {
                files: source.source.files,
                source_bytes: source.source.source_bytes,
                cargo_manifests: source.source.cargo_manifests,
                maximum_generation_bytes: source.max_generation_bytes,
            },
        );
        let project_id = self
            .database
            .register_project(NewProject::new(
                self.root_identity.clone(),
                self.repository_fingerprint.clone(),
            ))
            .await
            .map_err(|_| ProjectError::RegisterFailed)?;
        if source.prior.is_none()
            && recover_abandoned_staging(
                &self.database,
                &project_id,
                DEFAULT_STAGING_CLEANUP_TIMEOUT,
            )
            .await?
                == StagingPreflight::ProjectBusy
        {
            return Err(ProjectError::IndexLeaseBusy);
        }
        let staged = self
            .database
            .begin_generation(
                NewGeneration::new(project_id.clone(), source.source.digest.as_str(), workers)
                    .with_run_excludes(std::mem::take(&mut source.effective_run_excludes)),
            )
            .await
            .map_err(|_| ProjectError::BeginGenerationFailed)?;
        let generation_id = staged.generation_id().clone();
        Ok(IndexGenerationReservation {
            project_id,
            generation_id,
            workers,
            generation_storage,
            staged,
        })
    }

    async fn publish_index(
        &self,
        pending: PendingIndex,
        cancellation: ProjectCancellation,
        profile_requested: bool,
    ) -> Result<IndexReport, IndexFailure> {
        let project_id = pending.project_id.clone();
        let generation_id = pending.generation_id.clone();
        let failure = match self
            .publish_index_inner(pending, cancellation, profile_requested)
            .await
        {
            Ok(report) => return Ok(report),
            Err(failure) => failure,
        };
        // The primary failure stays authoritative: an unfinished cleanup only
        // leaves this attempt's staging row for the next bounded preflight.
        let cleanup = self
            .database
            .fail_unleased_staging_generation_bounded(
                GenerationRecoveryRequest::new(&project_id, &generation_id),
                DEFAULT_STAGING_CLEANUP_TIMEOUT,
            )
            .await;
        Err(failure.after_staging_cleanup(&cleanup))
    }

    async fn publish_index_inner(
        &self,
        pending: PendingIndex,
        cancellation: ProjectCancellation,
        profile_requested: bool,
    ) -> Result<IndexReport, IndexFailure> {
        self.prepare_index_publication(pending, cancellation)?
            .execute(profile_requested)
            .await
    }

    fn prepare_index_publication(
        &self,
        pending: PendingIndex,
        cancellation: ProjectCancellation,
    ) -> Result<PreparedIndexPublication, ProjectError> {
        let PendingIndex {
            project_id,
            generation_id,
            source_revision,
            scip_overlay,
            workers,
            max_source_bytes,
            max_generation_bytes,
            parse_cache_reads,
            discovery_policy,
            index_policy,
            generation_storage,
            staged,
        } = pending;
        let target = cartograph_db::LeaseTarget::new(
            project_id.clone(),
            ProjectOperation::Index,
            Some(generation_id.clone()),
        );
        let source_root = SourceRoot::open_with_policy(&self.root, discovery_policy)
            .map_err(|_| ProjectError::ProjectRootUnavailable)?;
        let pipeline = pipeline_config(
            PipelineBounds {
                workers,
                max_source_bytes,
                max_generation_bytes,
            },
            index_policy,
        )?;
        let maximum_stage_reservation = pipeline
            .maximum_stage_reservation_bytes()
            .map_err(|_| ProjectError::InvalidOptions)?;
        let supervisor = IndexerSupervisor::new(
            self.database.clone(),
            supervisor_config(maximum_stage_reservation.max(DEFAULT_MAX_SUPERVISOR_BYTES)),
        );
        cancellation.attach_index_supervisor(supervisor.clone());
        let cancellation_supervisor = supervisor.clone();
        let cancellation_signal = cancellation.clone();
        let cancellation_task = AbortTaskOnDrop::new(tokio::spawn(async move {
            cancellation_signal.cancelled().await;
            let _cancellation_was_new = cancellation_supervisor.cancel();
        }));
        let request = SupervisorRequest::new(target, process_owner(), DEFAULT_LEASE_DURATION);
        let parse_cache = NativeParseCache::new(self.database.clone(), project_id.clone())
            .with_reads(parse_cache_reads);
        let (report_sender, report_receiver) = oneshot::channel();
        let mut build =
            NativeGenerationBuild::new(source_root, pipeline).with_parse_cache(parse_cache);
        if let Some(overlay) = scip_overlay {
            build = build.with_scip_overlay(overlay);
        }
        let work = GenerationBuildWork {
            build,
            generation_storage,
            staged,
            report_sender,
        };
        Ok(PreparedIndexPublication {
            parse_cache_contract_digest: native_parse_cache_contract_digest(
                index_policy.maximum_ast_depth,
            ),
            project_id,
            generation_id,
            source_revision,
            workers,
            supervisor,
            cancellation,
            cancellation_task,
            request,
            work,
            report_receiver,
        })
    }

    async fn maintain_generation_retention(
        &self,
        project_id: &ProjectId,
        contract: &ContentDigest,
    ) -> GenerationRetentionStatus {
        maintain_generation_retention(self, project_id, contract).await
    }

    /// Number of admitted full source-manifest observations by this runtime.
    ///
    /// Includes failed scans; queued requests cancelled before admission do not
    /// increment it. The counter saturates and contains no source or query data.
    #[must_use]
    pub fn source_scan_observations(&self) -> u64 {
        self.source_scan_observations.load(Ordering::Relaxed)
    }

    /// Close all PostgreSQL connections owned by this project runtime.
    pub async fn close(self) {
        self.database.close().await;
    }

    async fn scan_source_with_excludes(
        &self,
        capture_path: Option<NormalizedPath>,
        additional_excludes: &[String],
        cancellation: ProjectCancellation,
    ) -> Result<SourceRevision, ProjectError> {
        self.scan_source_batch(
            capture_path.into_iter().collect(),
            additional_excludes,
            cancellation,
        )
        .await
    }

    async fn scan_source_batch(
        &self,
        capture_paths: BTreeSet<NormalizedPath>,
        additional_excludes: &[String],
        cancellation: ProjectCancellation,
    ) -> Result<SourceRevision, ProjectError> {
        if capture_paths.len() > source_context::MAXIMUM_CONTEXT_BATCH {
            return Err(ProjectError::InvalidOptions);
        }
        let source_policy = project_source_policy_with_excludes(&self.root, additional_excludes)?;
        let max_source_bytes = source_policy
            .maximum_file_bytes
            .unwrap_or(DEFAULT_MAX_SOURCE_BYTES);
        scan_source_path(SourceScanRequest {
            root: self.root.clone(),
            permits: self.source_scan_permits.clone(),
            observations: self.source_scan_observations.clone(),
            capture_paths,
            retain_scip_overlay: false,
            max_source_bytes,
            discovery_policy: source_policy.discovery,
            index_policy: source_policy.index,
            cancellation,
        })
        .await
    }

    async fn scan_source(
        &self,
        capture_path: Option<NormalizedPath>,
        cancellation: ProjectCancellation,
    ) -> Result<SourceRevision, ProjectError> {
        let snapshot = self
            .database
            .project_snapshot_by_root(&self.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)?;
        let run_excludes = snapshot
            .as_ref()
            .and_then(|project| project.current.as_ref())
            .map_or(&[][..], |current| current.source_admission.run_excludes());
        self.scan_source_with_excludes(capture_path, run_excludes, cancellation)
            .await
    }

    async fn scan_source_for_index(
        &self,
        input: IndexSourceScan,
    ) -> Result<SourceRevision, ProjectError> {
        let IndexSourceScan {
            cancellation,
            max_source_bytes,
            discovery_policy,
            index_policy,
        } = input;
        scan_source_path(SourceScanRequest {
            root: self.root.clone(),
            permits: self.source_scan_permits.clone(),
            observations: self.source_scan_observations.clone(),
            capture_paths: BTreeSet::new(),
            retain_scip_overlay: true,
            max_source_bytes,
            discovery_policy,
            index_policy,
            cancellation,
        })
        .await
    }
}

enum IndexPreparation {
    Unchanged(Box<IndexReport>),
    Pending(Box<PendingIndex>),
}

struct PreparedIndexSource {
    prior: Option<ProjectSnapshot>,
    /// Whether staging recovery ran or found another live project writer.
    staging: StagingPreflight,
    source: SourceRevision,
    effective_run_excludes: Vec<String>,
    max_source_bytes: usize,
    max_generation_bytes: u64,
    generation_storage_policy: GenerationStoragePolicy,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
}

struct IndexGenerationReservation {
    project_id: ProjectId,
    generation_id: cartograph_domain::GenerationId,
    workers: u16,
    generation_storage: GenerationStorageSelection,
    staged: StagedGeneration,
}

fn effective_run_excludes(options: &IndexOptions, prior: Option<&ProjectSnapshot>) -> Vec<String> {
    if !options.additional_excludes().is_empty() {
        return options.additional_excludes().to_vec();
    }
    if options.admission_reconciliation == IndexAdmissionReconciliation::PreserveCurrent {
        return prior
            .and_then(|snapshot| snapshot.current.as_ref())
            .map_or_else(Vec::new, |current| {
                current.source_admission.run_excludes().to_vec()
            });
    }
    Vec::new()
}

#[derive(Clone, Copy)]
struct UnchangedIndexInput<'input> {
    prior: Option<&'input ProjectSnapshot>,
    source: &'input SourceRevision,
    options: &'input IndexOptions,
    effective_run_excludes: &'input [String],
    maximum_ast_depth: usize,
}

fn unchanged_index_preparation(input: UnchangedIndexInput<'_>) -> Option<IndexPreparation> {
    let prior = input.prior?;
    let current = prior.current.as_ref()?;
    if input.options.force
        || current.source_revision != input.source.digest.as_str()
        || current.digest_version != GenerationDigestVersion::CURRENT
        || current.source_admission.run_excludes() != input.effective_run_excludes
    {
        return None;
    }
    Some(IndexPreparation::Unchanged(Box::new(IndexReport {
        project_id: prior.project_id.clone(),
        generation_id: current.generation_id.clone(),
        source_revision: input.source.digest.clone(),
        parse_cache_contract_digest: native_parse_cache_contract_digest(input.maximum_ast_depth),
        content_digest: current.content_digest.clone(),
        workers: select_worker_count(
            input.source.files,
            input.source.source_bytes,
            input.options.max_workers,
        ),
        published: false,
        publication: IndexPublication::Skipped {
            reason: "unchanged_current_generation",
            generation_remains_current: true,
        },
        native: None,
        history: HistoryIndexStatus::Unavailable {
            reason: "not_attempted",
        },
        issue_history: IssueHistoryIndexStatus::Unavailable {
            reason: "not_attempted",
        },
        profile: input.options.profile.then(IndexProfile::default),
        retention: GenerationRetentionStatus::Deferred {
            reason: "not_attempted",
            retryable: false,
            unlock_applicable: false,
            next_action: "none",
        },
    })))
}

struct PendingIndex {
    project_id: ProjectId,
    generation_id: cartograph_domain::GenerationId,
    source_revision: ContentDigest,
    scip_overlay: Option<ScipOverlayInput>,
    workers: u16,
    max_source_bytes: usize,
    max_generation_bytes: u64,
    parse_cache_reads: bool,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
    generation_storage: GenerationStorageSelection,
    staged: StagedGeneration,
}

struct PreparedIndexPublication {
    parse_cache_contract_digest: ContentDigest,
    project_id: ProjectId,
    generation_id: cartograph_domain::GenerationId,
    source_revision: ContentDigest,
    workers: u16,
    supervisor: IndexerSupervisor,
    cancellation: ProjectCancellation,
    cancellation_task: AbortTaskOnDrop,
    request: SupervisorRequest,
    work: GenerationBuildWork,
    report_receiver: oneshot::Receiver<NativePipelineReport>,
}

impl PreparedIndexPublication {
    async fn execute(self, profile_requested: bool) -> Result<IndexReport, IndexFailure> {
        let current_result = self
            .supervisor
            .run(self.request, move |context| {
                prepare_generation(context, self.work)
            })
            .await;
        self.cancellation_task.abort_and_reap().await;
        let supervisor_status = self.supervisor.status().await;
        let current = current_result.map_err(|error| {
            supervisor_index_failure(
                error,
                &SupervisorFailureContext {
                    cancelled: self.cancellation.is_cancelled(),
                    stage: supervisor_status.stage(),
                },
            )
        })?;
        let native = self
            .report_receiver
            .await
            .map_err(|_| ProjectError::IndexFailed)?;
        Ok(IndexReport {
            project_id: self.project_id,
            generation_id: self.generation_id,
            source_revision: self.source_revision,
            parse_cache_contract_digest: self.parse_cache_contract_digest,
            content_digest: current.content_digest().clone(),
            workers: self.workers,
            published: true,
            publication: IndexPublication::Published,
            native: Some(native.into()),
            history: HistoryIndexStatus::Unavailable {
                reason: "not_attempted",
            },
            issue_history: IssueHistoryIndexStatus::Unavailable {
                reason: "not_attempted",
            },
            profile: profile_requested.then(|| IndexProfile {
                pipeline_millis: supervisor_status.total_elapsed_millis(),
                pipeline_stages: supervisor_status.stage_timings().to_vec(),
                ..IndexProfile::default()
            }),
            retention: GenerationRetentionStatus::Deferred {
                reason: "not_attempted",
                retryable: false,
                unlock_applicable: false,
                next_action: "none",
            },
        })
    }
}

struct GenerationBuildWork {
    build: NativeGenerationBuild,
    generation_storage: GenerationStorageSelection,
    staged: StagedGeneration,
    report_sender: oneshot::Sender<NativePipelineReport>,
}

async fn prepare_generation(
    context: SupervisorContext,
    work: GenerationBuildWork,
) -> Result<cartograph_db::ReadyGeneration, PipelineFailure> {
    match work.generation_storage {
        GenerationStorageSelection::Memory => {
            let native = build_native_generation_with_scip_and_cache(&context.stages(), work.build)
                .await
                .map_err(|error| native_pipeline_failure(&error))?;
            let report = native.report();
            let (facts, _) = native.into_parts();
            work.report_sender
                .send(report)
                .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
            context
                .progress()
                .begin_stage(PipelineStage::Copy)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
            context
                .prepare_generation(GenerationContents::new(work.staged, facts))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        }
        GenerationStorageSelection::Postgres(policy) => {
            let spill = context
                .generation_spill(&work.staged, policy)
                .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
            let native = build_native_generation_spilled(&context.stages(), work.build, spill)
                .await
                .map_err(|error| native_pipeline_failure(&error))?;
            let report = native.report();
            let (digest, _) = native.into_parts();
            work.report_sender
                .send(report)
                .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
            context
                .progress()
                .begin_stage(PipelineStage::Copy)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
            context
                .prepare_spilled_generation(SpilledGenerationContents::new(work.staged, digest))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        }
    }
}

fn native_pipeline_failure(error: &cartograph_indexer::NativePipelineError) -> PipelineFailure {
    if let Some(failure) = error.file_failure() {
        return PipelineFailure::with_file_failure(error.stage(), failure.clone());
    }
    match error.reason() {
        Some(reason) => PipelineFailure::with_reason(error.stage(), reason),
        None => PipelineFailure::new(error.stage()),
    }
}

struct SourceScanRequest {
    root: PathBuf,
    permits: Arc<Semaphore>,
    observations: Arc<AtomicU64>,
    capture_paths: BTreeSet<NormalizedPath>,
    retain_scip_overlay: bool,
    max_source_bytes: usize,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
    cancellation: ProjectCancellation,
}

struct IndexSourceScan {
    cancellation: ProjectCancellation,
    max_source_bytes: usize,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
}

async fn scan_source_path(input: SourceScanRequest) -> Result<SourceRevision, ProjectError> {
    let SourceScanRequest {
        root,
        permits,
        observations,
        capture_paths,
        retain_scip_overlay,
        max_source_bytes,
        discovery_policy,
        index_policy,
        cancellation,
    } = input;
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
        result = permits.acquire_owned() => result.map_err(|_| ProjectError::SourceScanFailed)?,
    };
    if cancellation.is_cancelled() {
        return Err(ProjectError::RequestCancelled);
    }
    let worker_cancellation = cancellation.clone();
    let result = run_source_worker(move || {
        // Keep the per-project permit inside the blocking worker. Tokio cannot
        // abort `spawn_blocking` work once it has started, so retaining the
        // permit in the async caller would allow a dropped request to start a
        // second scan while the first worker was still unwinding.
        let _permit = permit;
        observations.update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.saturating_add(1)
        });
        source_revision_with_options(
            SourceRevisionRequest {
                root: &root,
                capture_paths: &capture_paths,
                retain_scip_overlay,
                max_source_bytes,
                discovery_policy,
                index_policy,
            },
            || worker_cancellation.is_cancelled(),
        )
    })
    .await;
    match result {
        Ok(source) => Ok(source),
        Err(_) if cancellation.is_cancelled() => Err(ProjectError::RequestCancelled),
        Err(error) => Err(error),
    }
}

async fn run_source_worker<T: Send + 'static>(
    worker: impl FnOnce() -> Result<T, ProjectError> + Send + 'static,
) -> Result<T, ProjectError> {
    tokio::task::spawn_blocking(worker)
        .await
        .map_err(|_| ProjectError::SourceScanFailed)?
}

struct SourceRevision {
    digest: ContentDigest,
    v1_source_manifest: ContentDigest,
    files: usize,
    source_bytes: u64,
    cargo_manifests: usize,
    captures: BTreeMap<NormalizedPath, CapturedSource>,
    scip_overlay: Option<ScipOverlayInput>,
}

struct CapturedSource {
    source: Box<str>,
    content_hash: ContentDigest,
}

#[derive(Default)]
struct SourceCaptures {
    entries: BTreeMap<NormalizedPath, CapturedSource>,
    bytes: usize,
}

impl SourceCaptures {
    fn insert(
        &mut self,
        path: &NormalizedPath,
        snapshot: &cartograph_extract::SourceSnapshot,
    ) -> Result<(), ProjectError> {
        self.bytes = self
            .bytes
            .checked_add(snapshot.source().len())
            .filter(|bytes| *bytes <= source_context::MAXIMUM_CAPTURE_BATCH_BYTES)
            .ok_or(ProjectError::SourceScanFailed)?;
        let mut source = String::new();
        source
            .try_reserve_exact(snapshot.source().len())
            .map_err(|_| ProjectError::SourceScanFailed)?;
        source.push_str(snapshot.source());
        self.entries.insert(
            path.clone(),
            CapturedSource {
                source: source.into_boxed_str(),
                content_hash: snapshot.content_hash().clone(),
            },
        );
        Ok(())
    }
}

struct ProjectSourcePolicy {
    discovery: DiscoveryPolicy,
    maximum_file_bytes: Option<usize>,
    maximum_generation_bytes: Option<u64>,
    generation_storage: GenerationStoragePolicy,
    index: SourceIndexPolicy,
}

#[derive(Clone, Copy)]
struct GenerationStoragePolicy {
    preference: ProjectGenerationStorage,
    spill: NativeGenerationSpillPolicy,
}

#[derive(Clone, Copy)]
enum GenerationStorageSelection {
    Memory,
    Postgres(NativeGenerationSpillPolicy),
}

/// Cheap, immutable source-policy snapshot used by filesystem watchers before
/// they enqueue a full status reconciliation.
#[derive(Clone, Debug)]
pub struct ProjectWatchFilter {
    discovery: DiscoveryPolicy,
}

impl ProjectWatchFilter {
    /// Load the same project include, exclude, and language policy used by a
    /// source scan. Callers should replace this snapshot after config changes.
    /// # Errors
    ///
    /// Returns an error when `.cartograph/config.json` source policy is
    /// malformed, unsafe, oversized, or cannot be read from `project_root`.
    pub fn load(project_root: &Path) -> Result<Self, ProjectError> {
        let source_policy = project_source_policy(project_root)?;
        Ok(Self {
            discovery: source_policy.discovery,
        })
    }

    /// Decide whether a relative filesystem path could change the indexed
    /// source manifest. Invalid or non-Unicode paths fail open so periodic
    /// reconciliation remains the final correctness boundary.
    #[must_use]
    pub fn relevant_relative_path(&self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return true;
        }
        let Some(raw) = path.to_str() else {
            return true;
        };
        let Ok(normalized) = NormalizedPath::parse(raw) else {
            return true;
        };
        if self.discovery.excludes_path_or_descendants(&normalized) {
            return false;
        }
        self.discovery.supports_normalized_path(&normalized) || path.extension().is_none()
    }
}

#[derive(Clone, Copy)]
struct SourceIndexPolicy {
    centrality: SourceCentralityPolicy,
    retention: SourceRetentionPolicy,
    partial_clones: bool,
    duplicate_code_allowlist_digest: [u8; 32],
    maximum_ast_depth: usize,
}

#[derive(Clone, Copy)]
struct SourceCentralityPolicy {
    page_rank: bool,
    betweenness: bool,
}

#[derive(Clone, Copy)]
struct SourceRetentionPolicy {
    docstrings: bool,
    call_sites: bool,
}

impl SourceIndexPolicy {
    fn from_settings(settings: &ProjectSourceSettings) -> Self {
        Self {
            centrality: SourceCentralityPolicy {
                page_rank: settings.enable_centrality(),
                betweenness: settings.enable_betweenness(),
            },
            retention: SourceRetentionPolicy {
                docstrings: settings.extract_docstrings(),
                call_sites: settings.track_call_sites(),
            },
            partial_clones: settings.duplicate_code_partial_clones(),
            duplicate_code_allowlist_digest: duplicate_code_allowlist_digest(
                settings.duplicate_code_allowlist(),
            ),
            maximum_ast_depth: settings.maximum_ast_depth(),
        }
    }

    #[cfg(test)]
    fn full(duplicate_code_allowlist_digest: [u8; 32]) -> Self {
        Self {
            centrality: SourceCentralityPolicy {
                page_rank: true,
                betweenness: true,
            },
            retention: SourceRetentionPolicy {
                docstrings: true,
                call_sites: true,
            },
            partial_clones: false,
            duplicate_code_allowlist_digest,
            maximum_ast_depth: cartograph_extract::DEFAULT_MAXIMUM_AST_DEPTH,
        }
    }
}

impl ProjectSourcePolicy {
    fn from_settings(settings: &ProjectSourceSettings) -> Result<Self, ProjectError> {
        Self::from_settings_with_excludes(settings, settings.excludes())
    }

    fn from_settings_with_excludes(
        settings: &ProjectSourceSettings,
        excludes: &[String],
    ) -> Result<Self, ProjectError> {
        let discovery = DiscoveryPolicy::new_with_languages(
            excludes,
            NestedRepositoryPolicy::new(
                settings.index_submodules(),
                settings.index_embedded_repositories(),
            ),
            settings.languages(),
        )
        .and_then(|policy| policy.with_includes(settings.includes()))
        .and_then(|policy| {
            policy.with_duplicate_code_allowlist(settings.duplicate_code_allowlist())
        })
        .map_err(|_| ProjectError::InvalidOptions)?;
        Ok(Self {
            discovery,
            maximum_file_bytes: settings.maximum_file_bytes(),
            maximum_generation_bytes: settings.maximum_generation_bytes(),
            generation_storage: GenerationStoragePolicy {
                preference: settings.generation_storage(),
                spill: NativeGenerationSpillPolicy::new(
                    settings
                        .maximum_spill_bytes()
                        .unwrap_or_else(|| NativeGenerationSpillPolicy::default().maximum_bytes()),
                    settings
                        .maximum_spill_rows()
                        .unwrap_or_else(|| NativeGenerationSpillPolicy::default().maximum_rows()),
                )
                .map_err(|_| ProjectError::InvalidOptions)?,
            },
            index: SourceIndexPolicy::from_settings(settings),
        })
    }
}

/// Observed corpus size and overlay state one storage selection weighs.
#[derive(Clone, Copy)]
struct GenerationStorageSignals {
    files: usize,
    source_bytes: u64,
    cargo_manifests: usize,
    maximum_generation_bytes: u64,
}

fn select_generation_storage(
    policy: GenerationStoragePolicy,
    signals: GenerationStorageSignals,
) -> GenerationStorageSelection {
    let GenerationStorageSignals {
        files,
        source_bytes,
        cargo_manifests,
        maximum_generation_bytes,
    } = signals;
    match policy.preference {
        ProjectGenerationStorage::Memory => GenerationStorageSelection::Memory,
        ProjectGenerationStorage::Postgres => GenerationStorageSelection::Postgres(policy.spill),
        ProjectGenerationStorage::Auto => {
            let estimated_generation_bytes =
                source_bytes.saturating_mul(AUTO_SPILL_EXPANSION_FACTOR);
            if files >= AUTO_SPILL_MINIMUM_FILES
                || source_bytes >= AUTO_SPILL_MINIMUM_SOURCE_BYTES
                || estimated_generation_bytes >= maximum_generation_bytes
                || cargo_manifests >= AUTO_SPILL_MINIMUM_CARGO_MANIFESTS
            {
                GenerationStorageSelection::Postgres(policy.spill)
            } else {
                GenerationStorageSelection::Memory
            }
        }
    }
}

fn project_source_policy(root: &Path) -> Result<ProjectSourcePolicy, ProjectError> {
    project_source_policy_with_excludes(root, &[])
}

/// Resolve the source policy, layering run-scoped exclusion globs over the
/// project's configured `exclude` list.
fn project_source_policy_with_excludes(
    root: &Path,
    additional: &[String],
) -> Result<ProjectSourcePolicy, ProjectError> {
    let settings = load_project_source_settings(root)?;
    if additional.is_empty() {
        return ProjectSourcePolicy::from_settings(&settings);
    }
    let mut merged = settings.excludes().to_vec();
    merged.extend_from_slice(additional);
    ProjectSourcePolicy::from_settings_with_excludes(&settings, &merged)
}

fn source_revision(root: &Path) -> Result<SourceRevision, ProjectError> {
    source_revision_with_capture(root, None, || false)
}

fn source_revision_with_capture<Cancel>(
    root: &Path,
    capture_path: Option<&NormalizedPath>,
    cancelled: Cancel,
) -> Result<SourceRevision, ProjectError>
where
    Cancel: FnMut() -> bool,
{
    let source_policy = project_source_policy(root)?;
    source_revision_with_options(
        SourceRevisionRequest {
            root,
            capture_paths: &capture_path.into_iter().cloned().collect(),
            retain_scip_overlay: false,
            max_source_bytes: source_policy
                .maximum_file_bytes
                .unwrap_or(DEFAULT_MAX_SOURCE_BYTES),
            discovery_policy: source_policy.discovery,
            index_policy: source_policy.index,
        },
        cancelled,
    )
}

struct SourceRevisionRequest<'path> {
    root: &'path Path,
    capture_paths: &'path BTreeSet<NormalizedPath>,
    retain_scip_overlay: bool,
    max_source_bytes: usize,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
}

fn source_revision_with_options<Cancel>(
    request: SourceRevisionRequest<'_>,
    mut cancelled: Cancel,
) -> Result<SourceRevision, ProjectError>
where
    Cancel: FnMut() -> bool,
{
    let SourceRevisionRequest {
        root,
        capture_paths,
        retain_scip_overlay,
        max_source_bytes,
        discovery_policy,
        index_policy,
    } = request;
    let source_root = SourceRoot::open_with_policy(root, discovery_policy)
        .map_err(|_| ProjectError::ProjectRootUnavailable)?;
    let discovery = discovery_limits()?;
    let source_limits = source_limits_with_max(max_source_bytes)?;
    let files = source_root
        .discover_with_cancellation(SourceDiscoveryOptions::new(discovery, &mut cancelled))
        .map_err(|_| ProjectError::SourceScanFailed)?;
    let mut manifest_entries = Vec::new();
    manifest_entries
        .try_reserve_exact(files.len())
        .map_err(|_| ProjectError::SourceScanFailed)?;
    let mut v1_manifest_entries = Vec::new();
    v1_manifest_entries
        .try_reserve_exact(files.len())
        .map_err(|_| ProjectError::SourceScanFailed)?;
    let mut captures = SourceCaptures::default();
    let mut source_bytes = 0_u64;
    let mut cargo_manifests = 0_usize;
    for file in &files {
        if is_cargo_manifest(file.path()) {
            cargo_manifests = cargo_manifests
                .checked_add(1)
                .ok_or(ProjectError::SourceScanFailed)?;
        }
        if file.byte_size() > u64::try_from(max_source_bytes).unwrap_or(u64::MAX) {
            let marker = oversized_source_digest(file);
            if SourceLanguage::is_v1_candidate_path(file.path().as_str()) {
                v1_manifest_entries.push((file.path().clone(), marker.clone()));
            }
            manifest_entries.push((file.path().clone(), marker));
            continue;
        }
        let snapshot = match source_root.read_with_cancellation(
            file.path(),
            SourceReadOptions::new(source_limits, &mut cancelled),
        ) {
            Ok(snapshot) => snapshot,
            Err(SourceReadError::UnsupportedLanguage) => continue,
            Err(_) => return Err(ProjectError::SourceScanFailed),
        };
        let snapshot_bytes =
            u64::try_from(snapshot.source().len()).map_err(|_| ProjectError::SourceScanFailed)?;
        source_bytes = source_bytes
            .checked_add(snapshot_bytes)
            .ok_or(ProjectError::SourceScanFailed)?;
        let content_hash = snapshot.content_hash().clone();
        if SourceLanguage::for_v1_normalized_path_with_source(
            file.path().as_str(),
            snapshot.source(),
        )
        .is_some()
        {
            v1_manifest_entries.push((file.path().clone(), content_hash.clone()));
        }
        manifest_entries.push((file.path().clone(), content_hash));
        if capture_paths.contains(file.path()) {
            captures.insert(file.path(), &snapshot)?;
        }
    }
    let source_digest = finish_source_manifest(&manifest_entries)?;
    let v1_source_manifest = finish_source_manifest(&v1_manifest_entries)?;
    let overlay = read_scip_overlay(root, retain_scip_overlay, &mut cancelled)?;
    let source_and_overlay_digest = overlay.digest.as_ref().map_or_else(
        || source_digest.clone(),
        |overlay_digest| source_overlay_digest(&source_digest, overlay_digest),
    );
    let digest = source_index_policy_digest(&source_and_overlay_digest, index_policy);
    Ok(SourceRevision {
        digest,
        v1_source_manifest,
        files: manifest_entries.len(),
        source_bytes,
        cargo_manifests,
        captures: captures.entries,
        scip_overlay: overlay.input,
    })
}

fn is_cargo_manifest(path: &NormalizedPath) -> bool {
    let path = path.as_str();
    path == "Cargo.toml" || path.ends_with("/Cargo.toml")
}

fn finish_source_manifest(
    entries: &[(NormalizedPath, ContentDigest)],
) -> Result<ContentDigest, ProjectError> {
    let mut digest = SourceManifestDigestBuilder::new(entries.len())
        .map_err(|_| ProjectError::SourceScanFailed)?;
    for (path, content_hash) in entries {
        digest
            .push(path, content_hash)
            .map_err(|_| ProjectError::SourceScanFailed)?;
    }
    digest.finish().map_err(|_| ProjectError::SourceScanFailed)
}

fn oversized_source_digest(file: &cartograph_extract::DiscoveredSource) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(OVERSIZED_SOURCE_DIGEST_DOMAIN);
    hasher.update(file.path().as_str().as_bytes());
    hasher.update(&file.byte_size().to_le_bytes());
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

struct ScipOverlayRead {
    digest: Option<ContentDigest>,
    input: Option<ScipOverlayInput>,
}

struct OpenScipOverlay {
    file: File,
    expected_bytes: usize,
}

fn open_scip_overlay(root: &Path) -> Result<Option<OpenScipOverlay>, ProjectError> {
    let path = root.join(SCIP_OVERLAY_RELATIVE_PATH);
    let initial = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectError::ScipOverlayInvalid),
    };
    if initial.file_type().is_symlink() || !initial.is_file() {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let metadata = file
        .metadata()
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let expected_bytes = usize::try_from(metadata.len())
        .ok()
        .filter(|length| (1..=MAXIMUM_SCIP_OVERLAY_BYTES).contains(length))
        .ok_or(ProjectError::ScipOverlayInvalid)?;
    if !metadata.is_file() {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    Ok(Some(OpenScipOverlay {
        file,
        expected_bytes,
    }))
}

fn read_open_scip_overlay(
    mut opened: OpenScipOverlay,
    retain_bytes: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<ScipOverlayRead, ProjectError> {
    let mut retained = if retain_bytes {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(opened.expected_bytes)
            .map_err(|_| ProjectError::ScipOverlayInvalid)?;
        Some(bytes)
    } else {
        None
    };
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_usize;
    let mut buffer = vec![0_u8; SCIP_READ_BUFFER_BYTES];
    loop {
        if cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let count = match opened.file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ProjectError::ScipOverlayInvalid),
        };
        total = total
            .checked_add(count)
            .filter(|total| *total <= MAXIMUM_SCIP_OVERLAY_BYTES)
            .ok_or(ProjectError::ScipOverlayInvalid)?;
        hasher.update(&buffer[..count]);
        if let Some(bytes) = retained.as_mut() {
            bytes.extend_from_slice(&buffer[..count]);
        }
    }
    if total != opened.expected_bytes {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    let digest = ContentDigest::from_bytes(*hasher.finalize().as_bytes());
    let input = retained
        .map(|bytes| ScipOverlayInput::new(bytes, MAXIMUM_SCIP_OVERLAY_ROWS))
        .transpose()
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    Ok(ScipOverlayRead {
        digest: Some(digest),
        input,
    })
}

fn read_scip_overlay(
    root: &Path,
    retain_bytes: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<ScipOverlayRead, ProjectError> {
    let Some(opened) = open_scip_overlay(root)? else {
        return Ok(ScipOverlayRead {
            digest: None,
            input: None,
        });
    };
    read_open_scip_overlay(opened, retain_bytes, cancelled)
}

fn source_overlay_digest(source: &ContentDigest, overlay: &ContentDigest) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SOURCE_SCIP_OVERLAY_DOMAIN);
    hasher.update(source.as_str().as_bytes());
    hasher.update(overlay.as_str().as_bytes());
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn source_index_policy_digest(source: &ContentDigest, policy: SourceIndexPolicy) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SOURCE_INDEX_POLICY_DOMAIN);
    hasher.update(source.as_str().as_bytes());
    hasher.update(&[
        u8::from(policy.centrality.page_rank),
        u8::from(policy.centrality.betweenness),
        u8::from(policy.retention.docstrings),
        u8::from(policy.retention.call_sites),
        u8::from(policy.partial_clones),
    ]);
    hasher.update(&policy.duplicate_code_allowlist_digest);
    hasher.update(
        &u64::try_from(policy.maximum_ast_depth)
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn duplicate_code_allowlist_digest(patterns: &[String]) -> [u8; 32] {
    let mut hasher =
        blake3::Hasher::new_derive_key("cartograph.v2.duplicate-code-allowlist-policy.2026-07-24");
    let unique = patterns.iter().map(String::as_str).collect::<BTreeSet<_>>();
    hasher.update(
        &u64::try_from(unique.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for pattern in unique {
        hasher.update(
            &u64::try_from(pattern.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(pattern.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn project_identity_digest(root: &Path) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PROJECT_IDENTITY_DOMAIN);
    hasher.update(root.as_os_str().as_encoded_bytes());
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn process_owner() -> cartograph_db::LeaseOwner {
    static MARKER: OnceLock<String> = OnceLock::new();
    let marker = MARKER.get_or_init(|| {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let mut hasher = blake3::Hasher::new();
        hasher.update(PROCESS_OWNER_DOMAIN);
        hasher.update(&process::id().to_le_bytes());
        hasher.update(&started.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    });
    cartograph_db::LeaseOwner::new(process::id(), marker.clone())
}

fn select_worker_count(files: usize, source_bytes: u64, maximum: u16) -> u16 {
    let file_workers = if files <= SINGLE_WORKER_MAX_FILES {
        ONE_WORKER
    } else if files <= TWO_WORKER_MAX_FILES {
        TWO_WORKERS
    } else if files <= FOUR_WORKER_MAX_FILES {
        FOUR_WORKERS
    } else if files <= EIGHT_WORKER_MAX_FILES {
        EIGHT_WORKERS
    } else {
        MAX_CONFIGURED_WORKERS
    };
    let byte_workers = if source_bytes <= SINGLE_WORKER_MAX_SOURCE_BYTES {
        ONE_WORKER
    } else if source_bytes <= TWO_WORKER_MAX_SOURCE_BYTES {
        TWO_WORKERS
    } else if source_bytes <= FOUR_WORKER_MAX_SOURCE_BYTES {
        FOUR_WORKERS
    } else if source_bytes <= EIGHT_WORKER_MAX_SOURCE_BYTES {
        EIGHT_WORKERS
    } else {
        MAX_CONFIGURED_WORKERS
    };
    let corpus = file_workers.max(byte_workers);
    let hardware = std::thread::available_parallelism()
        .ok()
        .and_then(|value| u16::try_from(value.get()).ok())
        .unwrap_or(ONE_WORKER);
    corpus.min(maximum).min(hardware.max(ONE_WORKER))
}

const fn history_unavailable_reason(error: HistoryIndexError) -> &'static str {
    match error {
        HistoryIndexError::InvalidOptions => "invalid_options",
        HistoryIndexError::NotGitRepository => "not_git_repository",
        HistoryIndexError::GitUnavailable => "git_unavailable",
        HistoryIndexError::GitOutputInvalid => "git_output_bound",
        HistoryIndexError::RelationLimit => "relation_bound",
        HistoryIndexError::Cancelled => "cancelled",
        HistoryIndexError::StorageUnavailable => "storage_unavailable",
        HistoryIndexError::DisabledByProjectConfig => "disabled_by_project_config",
    }
}

const fn issue_history_unavailable_reason(error: IssueHistoryIndexError) -> &'static str {
    match error {
        IssueHistoryIndexError::InvalidOptions => "invalid_options",
        IssueHistoryIndexError::NotGitRepository => "not_git_repository",
        IssueHistoryIndexError::GitUnavailable => "git_unavailable",
        IssueHistoryIndexError::ComparisonFailed => "comparison_failed",
        IssueHistoryIndexError::RelationLimit => "relation_bound",
        IssueHistoryIndexError::Cancelled => "cancelled",
        IssueHistoryIndexError::StorageUnavailable => "storage_unavailable",
    }
}

fn discovery_limits() -> Result<DiscoveryLimits, ProjectError> {
    DiscoveryLimits::new(DEFAULT_MAX_FILES, DEFAULT_MAX_PATH_BYTES)
        .map_err(|_| ProjectError::InvalidOptions)
}

fn source_limits() -> Result<SourceLimits, ProjectError> {
    source_limits_with_max(DEFAULT_MAX_SOURCE_BYTES)
}

fn source_limits_with_max(max_source_bytes: usize) -> Result<SourceLimits, ProjectError> {
    SourceLimits::new(max_source_bytes).map_err(|_| ProjectError::InvalidOptions)
}

/// Worker count and byte ceilings admitted for one native pipeline run.
#[derive(Clone, Copy)]
struct PipelineBounds {
    workers: u16,
    max_source_bytes: usize,
    max_generation_bytes: u64,
}

fn pipeline_config(
    bounds: PipelineBounds,
    policy: SourceIndexPolicy,
) -> Result<NativePipelineConfig, ProjectError> {
    let PipelineBounds {
        workers,
        max_source_bytes,
        max_generation_bytes,
    } = bounds;
    let retained = NativeRetainedLimits::new(DEFAULT_MAX_MANIFEST_BYTES, max_generation_bytes)
        .map_err(|_| ProjectError::InvalidOptions)?;
    let queue = usize::from(workers)
        .checked_mul(2)
        .ok_or(ProjectError::InvalidOptions)?;
    let capacity = StageCapacity::new(usize::from(workers), queue);
    let parallelism = NativePipelineParallelism::new(capacity, capacity)
        .map_err(|_| ProjectError::InvalidOptions)?;
    let deadlines = NativePipelineDeadlines::new(
        DEFAULT_ITEM_TIMEOUT,
        DEFAULT_STAGE_TIMEOUT,
        DEFAULT_CANCELLATION_GRACE,
    )
    .map_err(|_| ProjectError::InvalidOptions)?;
    NativePipelineConfig::new(
        NativePipelineLimits::new(
            discovery_limits()?,
            source_limits_with_max(max_source_bytes)?,
            retained,
        ),
        parallelism,
        deadlines,
    )
    .with_page_rank(policy.centrality.page_rank)
    .with_betweenness(policy.centrality.betweenness)
    .with_docstrings(policy.retention.docstrings)
    .with_call_sites(policy.retention.call_sites)
    .with_partial_clones(policy.partial_clones)
    .with_maximum_ast_depth(policy.maximum_ast_depth)
    .map_err(|_| ProjectError::InvalidOptions)
}

fn supervisor_config(max_worker_bytes: u64) -> SupervisorConfig {
    SupervisorConfig::new(DEFAULT_OPERATION_TIMEOUT)
        .with_heartbeat_interval(DEFAULT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(DEFAULT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(DEFAULT_PROGRESS_TIMEOUT)
        .with_cancellation_grace(DEFAULT_CANCELLATION_GRACE)
        .with_copy_timeout(DEFAULT_COPY_TIMEOUT)
        .with_max_worker_tasks(DEFAULT_MAX_SUPERVISOR_TASKS)
        .with_max_worker_bytes(max_worker_bytes)
}

fn monotonic_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn utf8_boundary(value: &str, maximum: usize) -> usize {
    let mut boundary = maximum.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    boundary
}

/// Credential-safe project service failures.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProjectError {
    /// Project root is missing, inaccessible, or not a directory.
    #[error("Cartograph project root is unavailable")]
    ProjectRootUnavailable,
    /// Database URL/settings did not yield a live PostgreSQL connection.
    #[error("Cartograph PostgreSQL database is unavailable")]
    DatabaseUnavailable,
    /// Required capabilities or append-only migrations failed.
    #[error("Cartograph PostgreSQL migration failed")]
    MigrationFailed,
    /// An older otherwise valid schema could not apply its next append-only migration.
    #[error(
        "Cartograph PostgreSQL schema version {database_schema_version} is below required version {required_schema_version}; pending migration {pending_migration_version} could not be applied"
    )]
    SchemaMigrationBlocked {
        /// Highest migration version still recorded by PostgreSQL after rollback.
        database_schema_version: i64,
        /// Exact migration version required by this binary.
        required_schema_version: i64,
        /// First append-only migration that remains unapplied.
        pending_migration_version: i64,
    },
    /// The database was migrated by a newer Cartograph binary.
    #[error(
        "Cartograph {binary_version} supports schema version {supported_schema_version}, but the database is at newer schema version {database_schema_version}; upgrade the Cartograph binary and repin the MCP registration before retrying"
    )]
    SchemaVersionAhead {
        /// Version of the binary that detected the mismatch.
        binary_version: &'static str,
        /// Newer schema version recorded by PostgreSQL.
        database_schema_version: i64,
        /// Highest schema version this binary can safely use.
        supported_schema_version: i64,
    },
    /// A project identity could not be registered safely.
    #[error("Cartograph project registration failed")]
    RegisterFailed,
    /// A generation could not be opened for staging.
    #[error("Cartograph index generation could not start")]
    BeginGenerationFailed,
    /// Live source discovery or hashing failed closed.
    #[error("Cartograph could not build a complete supported-source manifest")]
    SourceScanFailed,
    /// Every bounded post-build reconciliation observed another source change.
    #[error("Cartograph source changed during every bounded index reconciliation attempt")]
    SourceChangedDuringIndex,
    /// Durable project status could not be read.
    #[error("Cartograph project status is unavailable")]
    StatusFailed,
    /// A bounded extraction/COPY/publication stage failed.
    #[error("Cartograph index operation failed; the previous generation remains visible")]
    IndexFailed,
    /// One exact native pipeline stage failed before publication.
    #[error(
        "Cartograph index operation failed during the {stage} stage; the previous generation remains visible"
    )]
    IndexStageFailed {
        /// Stable credential-safe stage identifier.
        stage: PipelineStage,
    },
    /// One exact native pipeline stage failed for an allowlisted actionable reason.
    #[error(
        "Cartograph index operation failed during {stage}/{reason}; the previous generation remains visible"
    )]
    IndexStageFailedWithReason {
        /// Stable credential-safe stage identifier.
        stage: PipelineStage,
        /// Stable allowlisted reason with source and driver text discarded.
        reason: PipelineFailureReason,
    },
    /// One exact native pipeline input failed with a bounded relative-path diagnostic.
    #[error(
        "Cartograph index operation failed during {stage}/{failure}; the previous generation remains visible"
    )]
    IndexStageFileFailed {
        /// Stable credential-safe stage identifier.
        stage: PipelineStage,
        /// Validated project-relative path plus an allowlisted failure reason.
        failure: PipelineFileFailure,
    },
    /// Another live operation owns the project lease, or another writer still
    /// holds the project lock. Contention detected before reservation reserves
    /// no generation; a writer that wins after that check is still rejected at
    /// lease acquisition. Retry after it finishes.
    #[error(
        "Cartograph index is waiting for another project operation; the previous generation remains visible"
    )]
    IndexLeaseBusy,
    /// Automatic indexing is deferred until earlier failed generations drain.
    #[error(
        "Cartograph deferred automatic indexing until failed generations are cleaned up; the previous generation remains visible"
    )]
    IndexRetentionBacklog,
    /// The index operation could not acquire or retain its exact lease.
    #[error(
        "Cartograph index operation failed during the lease stage; the previous generation remains visible"
    )]
    IndexLeaseFailed,
    /// Durable preparation or atomic publication could not be reconciled safely.
    #[error(
        "Cartograph index operation failed during the publication stage; the previous generation remains visible"
    )]
    IndexPublicationFailed,
    /// Bounded recovery of abandoned staging generations failed before this
    /// attempt reserved one. A cleanup that fails after another failure is
    /// reported through [`IndexFailure::cleanup_failed`] instead.
    #[error("Cartograph index cleanup failed; inspect generation retention before retrying")]
    IndexCleanupFailed,
    /// Persistent SCIP bytes were missing, unsafe, changed while read, or exceeded bounds.
    #[error("Cartograph SCIP overlay is invalid or unsafe")]
    ScipOverlayInvalid,
    /// A caller supplied a zero, overflowing, or unsupported bound.
    #[error("Cartograph index options are invalid")]
    InvalidOptions,
    /// Project configuration could not be parsed or violated a named bound.
    #[error(transparent)]
    ProjectConfiguration(#[from] ProjectConfigError),
    /// No current-generation symbol matches the supplied exact identity.
    #[error("Cartograph symbol was not found in the current generation")]
    SymbolNotFound,
    /// No current-generation file matches the supplied exact path.
    #[error("Cartograph file was not found in the current generation")]
    FileNotFound,
    /// Current source context could not be read or bounded safely.
    #[error("Cartograph source context is unavailable")]
    SourceContextUnavailable,
    /// Required endpoint/model embedding configuration is absent or invalid.
    #[error("Cartograph embedding configuration is unavailable")]
    EmbeddingConfigurationUnavailable,
    /// A bounded embedding endpoint, model, or PostgreSQL semantic operation failed.
    #[error("Cartograph embedding operation failed")]
    EmbeddingOperationFailed,
    /// PostgreSQL could not allocate the shared-memory segment for HNSW creation.
    #[error(
        "Cartograph HNSW creation could not allocate shared memory; vectors remain resumable after the managed database upgrade"
    )]
    HnswCreateSharedMemoryUnavailable,
    /// Natural-language retrieval could not validate or assemble its bounded channels.
    #[error("Cartograph retrieval operation failed")]
    RetrievalOperationFailed,
    /// Optional enrichment could not read its generation-fenced candidates.
    #[error("Cartograph enrichment candidate read failed")]
    EnrichmentReadFailed,
    /// Optional enrichment could not persist one generation-fenced artifact.
    #[error("Cartograph enrichment artifact write failed")]
    EnrichmentWriteFailed,
    /// Optional enrichment encountered malformed bounded stored evidence.
    #[error("Cartograph enrichment candidate data was invalid")]
    EnrichmentDataInvalid,
    /// A caller cancelled a bounded source scan or long project operation.
    #[error("Cartograph project operation was cancelled")]
    RequestCancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_ahead_error_names_binary_and_both_schema_versions() {
        let error = ProjectError::SchemaVersionAhead {
            binary_version: "2.1.4",
            database_schema_version: 27,
            supported_schema_version: 26,
        };
        let message = error.to_string();
        assert!(message.contains("Cartograph 2.1.4"));
        assert!(message.contains("schema version 26"));
        assert!(message.contains("schema version 27"));
        assert!(message.contains("repin the MCP registration"));
    }

    #[test]
    fn blocked_schema_migration_names_the_exact_pending_version() {
        let error = ProjectError::SchemaMigrationBlocked {
            database_schema_version: 38,
            required_schema_version: 39,
            pending_migration_version: 39,
        };
        let message = error.to_string();
        assert!(message.contains("schema version 38"));
        assert!(message.contains("required version 39"));
        assert!(message.contains("pending migration 39"));
    }

    #[test]
    fn native_pipeline_failures_preserve_stage_and_allowlisted_reason_without_source_text() {
        let parse = native_pipeline_failure(&cartograph_indexer::NativePipelineError::Stage(
            cartograph_indexer::StageRunError::StageDeadline {
                stage: PipelineStage::Parse,
            },
        ));
        assert_eq!(parse.stage(), PipelineStage::Parse);
        assert_eq!(
            parse.reason(),
            Some(PipelineFailureReason::DeadlineExceeded)
        );

        let bounded =
            native_pipeline_failure(&cartograph_indexer::NativePipelineError::Validation {
                reason: Some(PipelineFailureReason::ReferenceNameTooLong),
            });
        assert_eq!(bounded.stage(), PipelineStage::Reduce);
        assert_eq!(
            bounded.reason(),
            Some(PipelineFailureReason::ReferenceNameTooLong)
        );
        let capacity =
            native_pipeline_failure(&cartograph_indexer::NativePipelineError::StageWithReason {
                stage: PipelineStage::Resolve,
                reason: PipelineFailureReason::GenerationCapacityExceeded,
            });
        assert_eq!(capacity.stage(), PipelineStage::Resolve);
        assert_eq!(
            capacity.reason(),
            Some(PipelineFailureReason::GenerationCapacityExceeded)
        );
        let Some(capacity_reason) = capacity.reason() else {
            panic!("generation-capacity reason was discarded");
        };
        assert_eq!(
            ProjectError::IndexStageFailedWithReason {
                stage: capacity.stage(),
                reason: capacity_reason,
            }
            .to_string(),
            "Cartograph index operation failed during resolve/generation_capacity_exceeded; the previous generation remains visible"
        );
        let Some(reason) = bounded.reason() else {
            panic!("allowlisted pipeline reason was discarded");
        };
        let public = ProjectError::IndexStageFailedWithReason {
            stage: bounded.stage(),
            reason,
        };
        assert_eq!(
            public.to_string(),
            "Cartograph index operation failed during reduce/reference_name_too_long; the previous generation remains visible"
        );

        let path = NormalizedPath::parse("src/broken.rs")
            .unwrap_or_else(|error| panic!("fixture path was invalid: {error}"));
        let input = native_pipeline_failure(
            &cartograph_indexer::NativePipelineError::StageWithFileFailure {
                stage: PipelineStage::Parse,
                failure: PipelineFileFailure::new(
                    path,
                    PipelineFailureReason::ExtractionParserStopped,
                ),
            },
        );
        let input_failure = input
            .file_failure()
            .unwrap_or_else(|| panic!("native parser failure lost its input path"));
        assert_eq!(input_failure.path().as_str(), "src/broken.rs");
        assert_eq!(
            input_failure.reason(),
            PipelineFailureReason::ExtractionParserStopped
        );
        let public = ProjectError::IndexStageFileFailed {
            stage: input.stage(),
            failure: input_failure.clone(),
        };
        assert_eq!(
            public.to_string(),
            "Cartograph index operation failed during parse/extraction_parser_stopped at project-relative path \"src/broken.rs\"; the previous generation remains visible"
        );
    }

    #[test]
    fn retention_release_warning_preserves_the_committed_cleanup_report() {
        let status = GenerationRetentionStatus::CompletedWithWarning {
            report: GenerationRetentionReport {
                staging_removed: 1,
                ready_removed: 0,
                superseded_removed: 2,
                failed_removed: 3,
                embeddings_removed: 4,
                current_preserved: 1,
                superseded_preserved: 2,
                failed_remaining: 0,
                staging_remaining: 0,
                ready_remaining: 0,
                retiring_remaining: 0,
                batches_committed: 1,
                deferred_reason: None,
                cascade_rows_removed: 20,
                search_relations_removed: 2,
                search_relation_bytes_removed: 1_024,
                maintenance: cartograph_db::PostRetentionMaintenance::NotNeeded,
            },
            parse_cache: Some(NativeParseCacheRetentionReport::default()),
            warning: "lease_release_unavailable",
        };
        let value = serde_json::to_value(status)
            .unwrap_or_else(|error| panic!("retention status serialization failed: {error}"));

        assert_eq!(value["state"], "completed_with_warning");
        assert_eq!(value["warning"], "lease_release_unavailable");
        assert_eq!(value["report"]["failed_removed"], 3);
        assert_eq!(value["report"]["search_relation_bytes_removed"], 1_024);
    }

    #[test]
    fn worker_selector_is_corpus_aware_bounded_and_monotonic() {
        const OVERSIZED_CORPUS: usize = EIGHT_WORKER_MAX_FILES * EIGHT_WORKER_MAX_FILES;
        const CALLER_CAP: u16 = 3;

        let hardware = std::thread::available_parallelism()
            .ok()
            .and_then(|value| u16::try_from(value.get()).ok())
            .unwrap_or(ONE_WORKER)
            .max(ONE_WORKER);
        let cases = [
            (0, ONE_WORKER),
            (SINGLE_WORKER_MAX_FILES, ONE_WORKER),
            (SINGLE_WORKER_MAX_FILES + 1, TWO_WORKERS),
            (TWO_WORKER_MAX_FILES + 1, FOUR_WORKERS),
            (FOUR_WORKER_MAX_FILES + 1, EIGHT_WORKERS),
            (EIGHT_WORKER_MAX_FILES + 1, MAX_CONFIGURED_WORKERS),
        ];
        let mut prior = 0;
        for (files, requested) in cases {
            let selected = select_worker_count(files, 0, MAX_CONFIGURED_WORKERS);
            assert_eq!(selected, requested.min(hardware));
            assert!(selected >= prior);
            prior = selected;
        }
        let byte_cases = [
            (0, ONE_WORKER),
            (SINGLE_WORKER_MAX_SOURCE_BYTES, ONE_WORKER),
            (SINGLE_WORKER_MAX_SOURCE_BYTES + 1, TWO_WORKERS),
            (TWO_WORKER_MAX_SOURCE_BYTES + 1, FOUR_WORKERS),
            (FOUR_WORKER_MAX_SOURCE_BYTES + 1, EIGHT_WORKERS),
            (EIGHT_WORKER_MAX_SOURCE_BYTES + 1, MAX_CONFIGURED_WORKERS),
        ];
        let mut prior = 0;
        for (source_bytes, requested) in byte_cases {
            let selected = select_worker_count(1, source_bytes, MAX_CONFIGURED_WORKERS);
            assert_eq!(selected, requested.min(hardware));
            assert!(selected >= prior);
            prior = selected;
        }
        assert_eq!(
            select_worker_count(34, 1_052_564, MAX_CONFIGURED_WORKERS),
            EIGHT_WORKERS.min(hardware)
        );
        assert_eq!(
            select_worker_count(256, 6_145_536, MAX_CONFIGURED_WORKERS),
            MAX_CONFIGURED_WORKERS.min(hardware)
        );
        assert_eq!(
            select_worker_count(OVERSIZED_CORPUS, u64::MAX, CALLER_CAP),
            CALLER_CAP.min(hardware)
        );
    }

    #[test]
    fn generation_storage_selection_is_forced_or_manifest_aware() {
        let policy = |preference| GenerationStoragePolicy {
            preference,
            spill: NativeGenerationSpillPolicy::default(),
        };
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Memory),
                GenerationStorageSignals {
                    files: AUTO_SPILL_MINIMUM_FILES,
                    source_bytes: AUTO_SPILL_MINIMUM_SOURCE_BYTES,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Memory
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Postgres),
                GenerationStorageSignals {
                    files: 1,
                    source_bytes: 1,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: AUTO_SPILL_MINIMUM_FILES - 1,
                    source_bytes: (DEFAULT_MAX_GENERATION_BYTES / AUTO_SPILL_EXPANSION_FACTOR) - 1,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Memory
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: AUTO_SPILL_MINIMUM_FILES,
                    source_bytes: 1,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: 1,
                    source_bytes: 1,
                    cargo_manifests: AUTO_SPILL_MINIMUM_CARGO_MANIFESTS,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(is_cargo_manifest(
            &NormalizedPath::parse("crates/service/Cargo.toml")
                .unwrap_or_else(|error| panic!("fixture path failed: {error}"))
        ));
        assert!(!is_cargo_manifest(
            &NormalizedPath::parse("docs/Cargo.toml.md")
                .unwrap_or_else(|error| panic!("fixture path failed: {error}"))
        ));
    }

    #[test]
    fn generation_storage_selection_honours_forced_preferences() {
        let policy = |preference| GenerationStoragePolicy {
            preference,
            spill: NativeGenerationSpillPolicy::default(),
        };
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: 1,
                    source_bytes: AUTO_SPILL_MINIMUM_SOURCE_BYTES,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: 1,
                    source_bytes: DEFAULT_MAX_GENERATION_BYTES / AUTO_SPILL_EXPANSION_FACTOR,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Auto),
                GenerationStorageSignals {
                    files: AUTO_SPILL_MINIMUM_FILES,
                    source_bytes: AUTO_SPILL_MINIMUM_SOURCE_BYTES,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
        assert!(matches!(
            select_generation_storage(
                policy(ProjectGenerationStorage::Postgres),
                GenerationStorageSignals {
                    files: 1,
                    source_bytes: 1,
                    cargo_manifests: 0,
                    maximum_generation_bytes: DEFAULT_MAX_GENERATION_BYTES,
                },
            ),
            GenerationStorageSelection::Postgres(_)
        ));
    }

    #[test]
    fn project_identity_is_stable_distinct_and_does_not_expose_the_path() {
        let first = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let second = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let first_path = std::fs::canonicalize(first.path())
            .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));
        let second_path = std::fs::canonicalize(second.path())
            .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));
        let identity = project_identity_digest(&first_path);
        assert_eq!(identity, project_identity_digest(&first_path));
        assert_ne!(identity, project_identity_digest(&second_path));
        assert!(
            !identity
                .as_str()
                .contains(&first_path.to_string_lossy().to_string())
        );
    }

    #[test]
    fn source_revision_counts_only_exact_cargo_manifests_for_storage_selection() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let nested = directory.path().join("crates/service");
        std::fs::create_dir_all(&nested)
            .unwrap_or_else(|error| panic!("fixture directory failed: {error}"));
        std::fs::write(directory.path().join("Cargo.toml"), "[workspace]\n")
            .unwrap_or_else(|error| panic!("root manifest write failed: {error}"));
        std::fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname = \"service\"\nversion = \"0.1.0\"\n",
        )
        .unwrap_or_else(|error| panic!("nested manifest write failed: {error}"));
        std::fs::write(directory.path().join("Cargo.toml.md"), "not a manifest\n")
            .unwrap_or_else(|error| panic!("decoy write failed: {error}"));

        let revision = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));
        assert_eq!(revision.cargo_manifests, 2);
    }

    #[test]
    fn source_revision_changes_with_supported_content_and_ignores_local_state() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let source = directory.path().join("service.ts");
        std::fs::write(&source, "export const value = 1;\n")
            .unwrap_or_else(|error| panic!("source write failed: {error}"));
        let first = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));
        std::fs::write(directory.path().join(".editorconfig"), "root = true\n")
            .unwrap_or_else(|error| panic!("unrelated dotfile write failed: {error}"));
        let unrelated_dotfile = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("dotfile source revision failed: {error}"));
        assert_eq!(first.digest, unrelated_dotfile.digest);
        std::fs::create_dir(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("state dir failed: {error}"));
        std::fs::write(directory.path().join(".cartograph/cache"), "private state")
            .unwrap_or_else(|error| panic!("state write failed: {error}"));
        let ignored = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));
        assert_eq!(first.digest, ignored.digest);
        std::fs::write(&source, "export const value = 2;\n")
            .unwrap_or_else(|error| panic!("source write failed: {error}"));
        let changed = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));
        assert_ne!(first.digest, changed.digest);
        assert_eq!(changed.files, 1);
        std::fs::create_dir_all(directory.path().join(".cartograph/scip"))
            .unwrap_or_else(|error| panic!("SCIP state dir failed: {error}"));
        let overlay_bytes = [0x0a_u8, 0x00];
        std::fs::write(
            directory.path().join(SCIP_OVERLAY_RELATIVE_PATH),
            overlay_bytes,
        )
        .unwrap_or_else(|error| panic!("SCIP overlay write failed: {error}"));
        let overlaid = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("overlaid source revision failed: {error}"));
        assert_ne!(changed.digest, overlaid.digest);
        assert_eq!(changed.files, overlaid.files);
        let retained = source_revision_with_options(
            SourceRevisionRequest {
                root: directory.path(),
                capture_paths: &BTreeSet::new(),
                retain_scip_overlay: true,
                max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
                discovery_policy: DiscoveryPolicy::v1_defaults()
                    .unwrap_or_else(|error| panic!("default discovery policy failed: {error}")),
                index_policy: SourceIndexPolicy::full(duplicate_code_allowlist_digest(&[])),
            },
            || false,
        )
        .unwrap_or_else(|error| panic!("retained overlay scan failed: {error}"));
        assert_eq!(retained.digest, overlaid.digest);
        assert_eq!(
            retained
                .scip_overlay
                .as_ref()
                .map(|overlay| overlay.content_digest().clone()),
            Some(ContentDigest::from_bytes(
                *blake3::hash(&overlay_bytes).as_bytes()
            ))
        );
    }

    #[test]
    fn source_revision_honors_live_project_excludes_and_ignores_excluded_edits() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::create_dir_all(directory.path().join("private"))
            .unwrap_or_else(|error| panic!("private directory failed: {error}"));
        std::fs::create_dir_all(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        std::fs::write(
            directory.path().join("public.ts"),
            "export const visible = true;\n",
        )
        .unwrap_or_else(|error| panic!("public source failed: {error}"));
        std::fs::write(
            directory.path().join("private/hidden.ts"),
            "export const hidden = 1;\n",
        )
        .unwrap_or_else(|error| panic!("private source failed: {error}"));
        std::fs::write(
            directory.path().join(".cartograph/config.json"),
            r#"{"exclude":["private/**"]}"#,
        )
        .unwrap_or_else(|error| panic!("project config failed: {error}"));
        let excluded = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("excluded revision failed: {error}"));
        assert_eq!(excluded.files, 1);
        std::fs::write(
            directory.path().join("private/hidden.ts"),
            "export const hidden = 2;\n",
        )
        .unwrap_or_else(|error| panic!("private edit failed: {error}"));
        let excluded_edit = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("excluded edit revision failed: {error}"));
        assert_eq!(excluded.digest, excluded_edit.digest);
        std::fs::write(directory.path().join(".cartograph/config.json"), "{}")
            .unwrap_or_else(|error| panic!("project config reset failed: {error}"));
        let included = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("included revision failed: {error}"));
        assert_eq!(included.files, 2);
        assert_ne!(included.digest, excluded.digest);
    }

    #[test]
    fn source_revision_fences_graph_analysis_policy_changes() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
            .unwrap_or_else(|error| panic!("source write failed: {error}"));
        let baseline = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("baseline revision failed: {error}"));
        std::fs::create_dir(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let config = directory.path().join(".cartograph/config.json");
        std::fs::write(&config, r#"{"enableCentrality":false}"#)
            .unwrap_or_else(|error| panic!("centrality config failed: {error}"));
        let without_page_rank = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("centrality revision failed: {error}"));
        assert_ne!(baseline.digest, without_page_rank.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false}"#,
        )
        .unwrap_or_else(|error| panic!("betweenness config failed: {error}"));
        let without_graph_scores = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("betweenness revision failed: {error}"));
        assert_ne!(without_page_rank.digest, without_graph_scores.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"enableIssueHistory":false}"#,
        )
        .unwrap_or_else(|error| panic!("history config failed: {error}"));
        let auxiliary_change = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("auxiliary revision failed: {error}"));
        assert_eq!(without_graph_scores.digest, auxiliary_change.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false}"#,
        )
        .unwrap_or_else(|error| panic!("docstring config failed: {error}"));
        let without_docstrings = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("docstring revision failed: {error}"));
        assert_ne!(without_graph_scores.digest, without_docstrings.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false}"#,
        )
        .unwrap_or_else(|error| panic!("call-site config failed: {error}"));
        let without_call_sites = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("call-site revision failed: {error}"));
        assert_ne!(without_docstrings.digest, without_call_sites.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true}"#,
        )
        .unwrap_or_else(|error| panic!("partial-clone config failed: {error}"));
        let wider_clone_band = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("partial-clone revision failed: {error}"));
        assert_ne!(without_call_sites.digest, wider_clone_band.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true,"duplicateCodeAllowlist":["generated/**"]}"#,
        )
        .unwrap_or_else(|error| panic!("clone allowlist config failed: {error}"));
        let clone_allowlist = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("clone allowlist revision failed: {error}"));
        assert_ne!(wider_clone_band.digest, clone_allowlist.digest);

        std::fs::write(
            &config,
            r#"{"enableCentrality":false,"enableBetweenness":false,"extractDocstrings":false,"trackCallSites":false,"duplicateCodePartialClones":true,"duplicateCodeAllowlist":["generated/**"],"maxAstDepth":512}"#,
        )
        .unwrap_or_else(|error| panic!("AST depth config failed: {error}"));
        let deeper_ast = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("AST depth revision failed: {error}"));
        assert_ne!(clone_allowlist.digest, deeper_ast.digest);
    }

    #[test]
    fn source_revision_honors_the_configured_language_allow_list() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
            .unwrap_or_else(|error| panic!("Rust fixture failed: {error}"));
        let typescript = directory.path().join("ignored.ts");
        std::fs::write(&typescript, "export const ignored = 1;\n")
            .unwrap_or_else(|error| panic!("TypeScript fixture failed: {error}"));
        std::fs::create_dir(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let config = directory.path().join(".cartograph/config.json");
        std::fs::write(&config, r#"{"languages":["rust"]}"#)
            .unwrap_or_else(|error| panic!("language config failed: {error}"));
        let rust_only = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("Rust-only revision failed: {error}"));
        assert_eq!(rust_only.files, 1);
        std::fs::write(&typescript, "export const ignored = 2;\n")
            .unwrap_or_else(|error| panic!("ignored edit failed: {error}"));
        let ignored_edit = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("ignored revision failed: {error}"));
        assert_eq!(rust_only.digest, ignored_edit.digest);

        std::fs::write(&config, r#"{"languages":[]}"#)
            .unwrap_or_else(|error| panic!("all-language config failed: {error}"));
        let all_languages = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("all-language revision failed: {error}"));
        assert_eq!(all_languages.files, 2);
        assert_ne!(rust_only.digest, all_languages.digest);
    }

    #[test]
    fn source_revision_honors_include_globs_and_migrates_v1_additions() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::create_dir_all(directory.path().join("src"))
            .unwrap_or_else(|error| panic!("source directory failed: {error}"));
        std::fs::create_dir_all(directory.path().join("tests"))
            .unwrap_or_else(|error| panic!("test directory failed: {error}"));
        std::fs::create_dir(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        std::fs::write(directory.path().join("src/lib.rs"), "pub fn ready() {}\n")
            .unwrap_or_else(|error| panic!("source fixture failed: {error}"));
        let excluded = directory.path().join("tests/lib.rs");
        std::fs::write(&excluded, "#[test] fn ready() {}\n")
            .unwrap_or_else(|error| panic!("test fixture failed: {error}"));
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\n",
        )
        .unwrap_or_else(|error| panic!("TOML fixture failed: {error}"));
        let config = directory.path().join(".cartograph/config.json");
        std::fs::write(&config, r#"{"version":2,"include":["src/**/*.rs"]}"#)
            .unwrap_or_else(|error| panic!("include config failed: {error}"));
        let included = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("included revision failed: {error}"));
        assert_eq!(included.files, 1);
        std::fs::write(&excluded, "#[test] fn changed() {}\n")
            .unwrap_or_else(|error| panic!("excluded edit failed: {error}"));
        let excluded_edit = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("excluded edit revision failed: {error}"));
        assert_eq!(included.digest, excluded_edit.digest);

        std::fs::write(&config, r#"{"version":1,"include":["src/**/*.rs"]}"#)
            .unwrap_or_else(|error| panic!("v1 include config failed: {error}"));
        let migrated = source_revision(directory.path())
            .unwrap_or_else(|error| panic!("migrated include revision failed: {error}"));
        assert_eq!(migrated.files, 2);
        assert_ne!(included.digest, migrated.digest);
    }

    #[cfg(unix)]
    #[test]
    fn source_revision_rejects_a_symlinked_scip_overlay() {
        use std::os::unix::fs::symlink;

        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
            .unwrap_or_else(|error| panic!("source write failed: {error}"));
        std::fs::create_dir_all(directory.path().join(".cartograph/scip"))
            .unwrap_or_else(|error| panic!("SCIP state dir failed: {error}"));
        let external = directory.path().join("foreign.scip");
        std::fs::write(&external, [0x0a_u8, 0x00])
            .unwrap_or_else(|error| panic!("external write failed: {error}"));
        symlink(&external, directory.path().join(SCIP_OVERLAY_RELATIVE_PATH))
            .unwrap_or_else(|error| panic!("symlink failed: {error}"));
        assert_eq!(
            source_revision(directory.path()).map(|source| source.digest),
            Err(ProjectError::ScipOverlayInvalid)
        );
    }

    #[test]
    fn public_source_identity_matches_internal_revision_without_leaking_root_text() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(
            directory.path().join("service.ts"),
            "export const ready = true;\n",
        )
        .unwrap_or_else(|error| panic!("source write failed: {error}"));
        std::fs::write(
            directory.path().join("service.pyi"),
            "def ready() -> bool: ...\n",
        )
        .unwrap_or_else(|error| panic!("additive Python source write failed: {error}"));
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap_or_else(|error| panic!("additive TOML source write failed: {error}"));
        let canonical = std::fs::canonicalize(directory.path())
            .unwrap_or_else(|error| panic!("canonicalize failed: {error}"));

        let identity = ProjectRuntime::inspect_source_identity(directory.path())
            .unwrap_or_else(|error| panic!("source identity failed: {error}"));
        let internal = source_revision(&canonical)
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));

        assert_eq!(identity.source_revision, internal.digest);
        assert_eq!(identity.v1_source_manifest, internal.v1_source_manifest);
        let service_path = NormalizedPath::parse("service.ts")
            .unwrap_or_else(|error| panic!("service path was invalid: {error}"));
        let service_hash =
            ContentDigest::from_bytes(*blake3::hash(b"export const ready = true;\n").as_bytes());
        assert_eq!(
            identity.v1_source_manifest,
            finish_source_manifest(&[(service_path, service_hash)])
                .unwrap_or_else(|error| panic!("v1 manifest failed: {error}"))
        );
        assert_eq!(identity.files, 3);
        assert_eq!(
            identity.repository_fingerprint,
            project_identity_digest(&canonical)
        );
        assert!(!format!("{identity:?}").contains(&canonical.to_string_lossy().to_string()));
    }

    #[test]
    fn invalid_worker_caps_fail_before_any_database_or_source_work() {
        assert_eq!(
            IndexOptions::default().with_max_workers(0),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            IndexOptions::default().with_max_workers(17),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            IndexOptions::default().with_max_source_bytes(0),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            IndexOptions::default().with_max_source_bytes(DEFAULT_MAX_SOURCE_BYTES + 1),
            Err(ProjectError::InvalidOptions)
        );
        assert!(
            IndexOptions::default()
                .with_max_source_bytes(10 * 1024 * 1024)
                .is_ok()
        );
    }

    #[test]
    fn source_identity_preserves_named_project_configuration_range_errors() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::create_dir(directory.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        std::fs::write(
            directory.path().join(".cartograph/config.json"),
            r#"{"maxGenerationBytes":8589934593}"#,
        )
        .unwrap_or_else(|error| panic!("config fixture failed: {error}"));
        let error = ProjectRuntime::inspect_source_identity(directory.path())
            .err()
            .unwrap_or_else(|| panic!("out-of-range source identity unexpectedly succeeded"));
        assert_eq!(
            error,
            ProjectError::ProjectConfiguration(ProjectConfigError::NumericFieldOutOfRange {
                field: "maxGenerationBytes",
                minimum: 1,
                maximum: 8_589_934_592,
            })
        );
        assert!(error.to_string().contains("`maxGenerationBytes`"));
    }

    #[test]
    fn automatic_indexing_is_history_free_and_worker_bounded() {
        let automatic = IndexOptions::automatic();

        assert_eq!(automatic.max_workers, FOUR_WORKERS);
        assert!(!automatic.refresh_history);
        assert_eq!(
            automatic.failure_retention,
            IndexFailureRetention::AutomaticFailures
        );
        assert!(!automatic.force);
        assert_eq!(automatic.max_source_bytes, None);
        assert!(!automatic.profile);
        assert_eq!(automatic.additional_excludes, [] as [String; 0]);
        assert_eq!(
            automatic.admission_reconciliation,
            IndexAdmissionReconciliation::PreserveCurrent
        );
        assert_eq!(
            AUTOMATIC_RETENTION_MAINTENANCE,
            PostRetentionMaintenancePolicy::DelegateToAutovacuum
        );
    }

    #[test]
    fn semantic_maintenance_can_skip_only_the_independent_history_refresh() {
        let defaults = IndexOptions::default();
        assert!(defaults.refresh_history);
        assert_eq!(
            defaults.admission_reconciliation,
            IndexAdmissionReconciliation::Replace
        );
        assert_eq!(
            IndexOptions::reconciliation().admission_reconciliation,
            IndexAdmissionReconciliation::PreserveCurrent
        );
        let semantic = defaults.with_history_refresh(false);
        assert!(!semantic.refresh_history);
        assert!(!semantic.force);
        assert_eq!(semantic.max_source_bytes, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_source_scans_cancel_without_starving_runtime_timers() {
        const REQUESTS: usize = 32;
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(directory.path().join("service.rs"), "pub fn ready() {}\n")
            .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
        let root = directory
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("fixture canonicalize failed: {error}"));
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS));
        let held = permits
            .clone()
            .acquire_owned()
            .await
            .unwrap_or_else(|error| panic!("fixture permit failed: {error}"));
        let mut signals = Vec::new();
        let mut scans = Vec::new();
        for _ in 0..REQUESTS {
            let cancellation = ProjectCancellation::new();
            signals.push(cancellation.clone());
            scans.push(tokio::spawn(scan_source_path(SourceScanRequest {
                root: root.clone(),
                permits: permits.clone(),
                observations: Arc::new(AtomicU64::new(0)),
                capture_paths: BTreeSet::new(),
                retain_scip_overlay: false,
                max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
                discovery_policy: DiscoveryPolicy::v1_defaults()
                    .unwrap_or_else(|error| panic!("default discovery policy failed: {error}")),
                index_policy: SourceIndexPolicy::full(duplicate_code_allowlist_digest(&[])),
                cancellation,
            })));
        }
        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap_or_else(|_| panic!("source scans starved the runtime timer"));
        for signal in &signals {
            signal.cancel();
        }
        let reaped = tokio::time::timeout(Duration::from_secs(1), async {
            for scan in scans {
                let result = scan
                    .await
                    .unwrap_or_else(|error| panic!("source scan task failed: {error}"));
                assert_eq!(
                    result.map(|source| source.files),
                    Err(ProjectError::RequestCancelled)
                );
            }
        });
        reaped
            .await
            .unwrap_or_else(|_| panic!("cancelled source scans were not reaped"));
        drop(held);
    }
}
