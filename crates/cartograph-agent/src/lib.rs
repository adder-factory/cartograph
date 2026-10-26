//! Stable project runtime for the native `RocksDB` Cartograph product.
//!
//! This crate owns project identity, freshness, bounded worker selection, and
//! the complete source-to-publication operation. CLI and MCP adapters consume
//! this public service instead of reaching into database or indexer internals.

#[cfg(test)]
use sha2 as _;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use cartograph_config::{ProjectConfigError, ProjectSourceSettings, load_project_source_settings};
use cartograph_db::{
    CartographDatabase, HistoryRefreshReport, IssueHistoryRefreshReport, ProjectSnapshot,
};
use cartograph_domain::{
    ContentDigest, GenerationDigestVersion, NormalizedPath, ProjectId, SourceManifestDigestBuilder,
    project_root_identity,
};
use cartograph_extract::{
    DiscoveryLimits, DiscoveryPolicy, NestedRepositoryPolicy, SourceDiscoveryOptions, SourceLimits,
    SourceRoot,
};
use cartograph_indexer::{
    NativeBuildProgress, NativeGenerationStorage, NativePipelineConfig, NativePipelineDeadlines,
    NativePipelineLimits, NativePipelineParallelism, NativePipelineReport, NativeRetainedLimits,
    PipelineStageTiming, ScipOverlayInput, StageCapacity,
};
pub use cartograph_indexer::{
    PipelineFailureReason, PipelineFileFailure, PipelineStage, SupervisorState, SupervisorStatus,
};
use cartograph_scip::ScipOverlayReport;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Semaphore, watch};

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
mod issue_history;
mod layering;
mod navigation;
mod rename;
mod retrieval;
mod review;
mod rocks;
pub use rocks::{
    RocksIndexOptions, RocksIndexReport, RocksMaintenanceReport, RocksProjectError,
    RocksProjectRuntime, RocksQuery, RocksQueryResult, RocksStatus,
};
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
    PreparedRetrieval, RetrievalClientRequest, RetrievalFailureCause, RetrievalFailureStage,
    RetrievalOptions, RetrievalRequest, semantic_readiness_from_database,
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
const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(10);
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
    source_drift: SourceDriftPolicy,
}

/// How an index request reacts when the live checkout changed after it published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceDriftPolicy {
    /// Rebuild up to the bounded reconciliation attempts, then fail.
    Rebuild,
    /// Return the first published generation flagged as changed after publication.
    ReportPublished,
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
            source_drift: SourceDriftPolicy::Rebuild,
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

    /// Return the first generation this request publishes even when the live
    /// checkout changes again before the request finishes, reporting
    /// [`IndexLiveSource::ChangedAfterPublication`] instead of rebuilding.
    ///
    /// By default a changed checkout is rebuilt a bounded number of times and
    /// then fails. Upgrade reconciliation publishes once so a continuously
    /// edited checkout cannot keep it rebuilding until its deadline.
    #[must_use]
    pub const fn with_single_publication(mut self, single_publication: bool) -> Self {
        self.source_drift = if single_publication {
            SourceDriftPolicy::ReportPublished
        } else {
            SourceDriftPolicy::Rebuild
        };
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

struct NativeAuxiliaryMetrics {
    scip_overlay: Option<ScipOverlayMetrics>,
    parse_cache: NativeParseCacheMetrics,
    generation_storage: NativeGenerationStorageMetrics,
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
        }
    }
}

impl From<NativeGenerationStorage> for NativeGenerationStorageMetrics {
    fn from(storage: NativeGenerationStorage) -> Self {
        match storage {
            NativeGenerationStorage::Memory => Self::Memory,
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
        }
    }
}

/// Terminal result for an index request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    /// Complete file-local replacements that reused proven resolution inputs.
    /// Zero identifies a full build or an unchanged reconciliation.
    pub incremental_files: u16,
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
    /// Whether the live checkout still matched `source_revision` at this
    /// request's final source observation.
    pub live_source: IndexLiveSource,
}

/// Agreement between a returned generation and the live checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexLiveSource {
    /// The final observation matched; an unchanged no-op compares at preparation.
    Matched,
    /// This request published a complete generation, but the checkout changed
    /// before the request finished, so that current generation is not fresh.
    /// Only requests built with [`IndexOptions::with_single_publication`]
    /// report it; others rebuild or fail instead.
    ChangedAfterPublication,
    /// This request published a complete generation, but cancellation or a
    /// scan failure prevented the post-publication source check; a later
    /// status decides freshness. Only single-publication requests report it.
    Unverified,
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

/// Independent bounded native retention outcomes attached to every successful request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GenerationRetentionStatus {
    /// Each maintenance phase reports its own committed progress or failure.
    Attempted {
        /// Graph, extraction-cache and auxiliary cleanup outcomes.
        report: RocksMaintenanceReport,
    },
}
impl GenerationRetentionStatus {
    /// Whether background maintenance should retry after backoff.
    #[must_use]
    pub fn maintenance_failed(&self) -> bool {
        match self {
            Self::Attempted { report } => report.maintenance_failed(),
        }
    }
}

/// Monotonic phase timing evidence for one index/no-op request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProfile {
    /// Source manifest comparison plus generation staging.
    pub preparation_millis: u64,
    /// Complete byte-hashed manifest was reused by the full native pipeline.
    /// Discovery time is included in preparation instead of a second scan.
    pub source_manifest_reused: bool,
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
    index_supervisor: Arc<RwLock<Option<NativeBuildProgress>>>,
    reserved_generation: Arc<RwLock<Option<ReservedIndexGeneration>>>,
}

/// A staging generation that index work under one [`ProjectCancellation`]
/// reserved for itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedIndexGeneration {
    /// Project that owns the generation.
    pub project_id: ProjectId,
    /// The reserved generation; its lease, while held, names it.
    pub generation_id: cartograph_domain::GenerationId,
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
            reserved_generation: Arc::new(RwLock::new(None)),
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

    fn attach_index_supervisor(&self, supervisor: NativeBuildProgress) {
        if let Ok(mut attached) = self.index_supervisor.write() {
            *attached = Some(supervisor);
        }
    }

    /// The generation that index work under this signal reserved most
    /// recently, if any. After cancelling, a caller can check exactly that
    /// generation's cleanup instead of every writer's state on the project;
    /// a new reservation always carries a new generation identifier.
    #[must_use]
    pub fn reserved_index_generation(&self) -> Option<ReservedIndexGeneration> {
        self.reserved_generation
            .read()
            .ok()
            .and_then(|reserved| reserved.clone())
    }
}

impl Default for ProjectCancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// Saturating counters shared by every source scan of one runtime.
#[derive(Default)]
struct SourceScanCounters {
    /// Admitted full source-manifest observations.
    observations: AtomicU64,
    /// Cancellation checkpoints passed while discovering and hashing files.
    checkpoints: AtomicU64,
}

fn saturating_increment(counter: &AtomicU64) {
    counter.update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.saturating_add(1)
    });
}

struct SourceScanRequest {
    root: PathBuf,
    permits: Arc<Semaphore>,
    counters: Arc<SourceScanCounters>,
    capture_paths: BTreeSet<NormalizedPath>,
    retain_scip_overlay: bool,
    max_source_bytes: usize,
    discovery_policy: DiscoveryPolicy,
    index_policy: SourceIndexPolicy,
    cancellation: ProjectCancellation,
}

async fn scan_source_path(input: SourceScanRequest) -> Result<SourceRevision, ProjectError> {
    let SourceScanRequest {
        root,
        permits,
        counters,
        capture_paths,
        retain_scip_overlay,
        max_source_bytes,
        discovery_policy,
        index_policy,
        cancellation,
    } = input;
    let native_control = CartographDatabase::request_control(Duration::from_secs(30))
        .map_err(|_| ProjectError::RequestCancelled)?;
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
        _reason = native_control.stopped() => return Err(ProjectError::RequestCancelled),
        result = permits.acquire_owned() => result.map_err(|_| ProjectError::SourceScanFailed)?,
    };
    if cancellation.is_cancelled() {
        return Err(ProjectError::RequestCancelled);
    }
    let worker_cancellation = cancellation.clone();
    let worker_control = native_control.clone();
    let result = run_source_worker(move || {
        // Keep the per-project permit inside the blocking worker. Tokio cannot
        // abort `spawn_blocking` work once it has started, so retaining the
        // permit in the async caller would allow a dropped request to start a
        // second scan while the first worker was still unwinding.
        let _permit = permit;
        source_scan::retry_complete_scan(|| {
            if worker_cancellation.is_cancelled() || worker_control.check().is_err() {
                return Err(ProjectError::RequestCancelled);
            }
            saturating_increment(&counters.observations);
            source_revision_with_options(
                SourceRevisionRequest {
                    root: &root,
                    capture_paths: &capture_paths,
                    retain_scip_overlay,
                    max_source_bytes,
                    discovery_policy: discovery_policy.clone(),
                    index_policy,
                },
                || {
                    saturating_increment(&counters.checkpoints);
                    worker_cancellation.is_cancelled() || worker_control.check().is_err()
                },
            )
        })
    })
    .await;
    match result {
        _ if cancellation.is_cancelled() || native_control.check().is_err() => {
            Err(ProjectError::RequestCancelled)
        }
        Ok(source) => Ok(source),
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
    admitted_source_digest: ContentDigest,
    admitted_manifest: Vec<(NormalizedPath, ContentDigest)>,
    v1_source_manifest: ContentDigest,
    files: usize,
    source_bytes: u64,
    captures: BTreeMap<NormalizedPath, CapturedSource>,
    scip_overlay: Option<ScipOverlayInput>,
    native_manifest: Option<cartograph_indexer::NativeObservedManifest>,
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
    index: SourceIndexPolicy,
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct SourceIndexPolicy {
    centrality: SourceCentralityPolicy,
    retention: SourceRetentionPolicy,
    partial_clones: bool,
    duplicate_code_allowlist_digest: [u8; 32],
    maximum_ast_depth: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SourceCentralityPolicy {
    page_rank: bool,
    betweenness: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
            index: SourceIndexPolicy::from_settings(settings),
        })
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
    Cancel: FnMut() -> bool + Send,
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
    Cancel: FnMut() -> bool + Send,
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
    let (files, exclusions) = source_root
        .discover_with_evidence(SourceDiscoveryOptions::new(discovery, &mut cancelled))
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
    let mut admitted_manifest_entries = Vec::new();
    admitted_manifest_entries
        .try_reserve_exact(files.len())
        .map_err(|_| ProjectError::SourceScanFailed)?;
    let mut source_bytes = 0_u64;
    let mut native_manifest =
        retain_scip_overlay.then(|| cartograph_indexer::NativeObservedManifest::new(exclusions));
    source_scan::read_sources(
        source_scan::HashScan {
            root: &source_root,
            files: &files,
            limits: source_limits,
            maximum_bytes: max_source_bytes,
            retain_native: retain_scip_overlay,
        },
        &mut cancelled,
        capture_paths,
        &mut captures,
        |observed| {
            if let Some(file) = observed.native {
                if native_manifest
                    .as_mut()
                    .is_some_and(|manifest| !manifest.push(file))
                {
                    native_manifest = None;
                }
            } else if !observed.admitted
                && let Some(manifest) = &mut native_manifest
            {
                manifest.record_oversized();
            }
            source_bytes = source_bytes
                .checked_add(observed.bytes)
                .ok_or(ProjectError::SourceScanFailed)?;
            if observed.v1 {
                v1_manifest_entries.push((observed.path.clone(), observed.hash.clone()));
            }
            if observed.admitted {
                admitted_manifest_entries.push((observed.path.clone(), observed.hash.clone()));
            }
            manifest_entries.push((observed.path, observed.hash));
            Ok(())
        },
    )?;
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
        admitted_source_digest: finish_source_manifest(&admitted_manifest_entries)?,
        admitted_manifest: admitted_manifest_entries,
        v1_source_manifest,
        files: manifest_entries.len(),
        source_bytes,
        captures: captures.entries,
        scip_overlay: overlay.input,
        native_manifest,
    })
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

fn monotonic_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    /// Another PostgreSQL session held a lock that the pending schema migration
    /// needed for the migration's whole lock-contention budget. Nothing was
    /// applied, and retrying once that session ends is safe.
    #[error(
        "Cartograph PostgreSQL schema migration could not acquire its locks within its bounded lock-contention wait; another Cartograph process, often a long-running MCP server started from an older binary, is using the schema. Restart or stop that process, or retry"
    )]
    SchemaBusy,
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
    /// Context failure preserving its source-free stage and cause across adapters.
    #[error("Cartograph context failed during {stage:?}: {cause:?}")]
    RetrievalFailed {
        /// Stage that could not complete.
        stage: RetrievalFailureStage,
        /// Source-free failure category.
        cause: RetrievalFailureCause,
    },
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

mod runtime;
pub use runtime::ProjectRuntime;
mod index_failure;
mod source_runtime;
mod source_scan;

async fn cancellable_project_read<T>(
    cancellation: &ProjectCancellation,
    operation: impl std::future::Future<Output = Result<T, ProjectError>>,
) -> Result<T, ProjectError> {
    cancellable_native_read(cancellation, operation, || ProjectError::RequestCancelled).await
}

fn cancellable_native_read<T, E>(
    cancellation: &ProjectCancellation,
    operation: impl std::future::Future<Output = Result<T, E>>,
    cancelled: impl Fn() -> E,
) -> impl std::future::Future<Output = Result<T, E>> {
    let operation = Box::pin(operation);
    async move {
        let request = CartographDatabase::request_control(Duration::from_secs(30))
            .map_err(|_| cancelled())?;
        let operation = CartographDatabase::with_request(request.clone(), operation);
        tokio::pin!(operation);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                request.cancel();
                let _completed = operation.await;
                Err(cancelled())
            },
            result = &mut operation => result,
        }
    }
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
        IssueHistoryIndexError::DisabledByProjectConfig => "disabled_by_project_config",
        IssueHistoryIndexError::InvalidOptions => "invalid_options",
        IssueHistoryIndexError::NotGitRepository => "not_git_repository",
        IssueHistoryIndexError::GitUnavailable => "git_unavailable",
        IssueHistoryIndexError::ComparisonFailed => "comparison_failed",
        IssueHistoryIndexError::RelationLimit => "relation_bound",
        IssueHistoryIndexError::Cancelled => "cancelled",
        IssueHistoryIndexError::StorageUnavailable => "storage_unavailable",
    }
}

#[cfg(test)]
mod lib_tests;
