use cartograph_domain::{NormalizedPath, ProjectId};
use serde::{Deserialize, Serialize};
pub(crate) mod native;

const MAX_HISTORY_FILES: usize = 500_000;
const MAX_HISTORY_PAIRS: usize = 2_000_000;
const MAX_HISTORY_QUERY_LIMIT: u16 = 500;

/// Aggregated Git history for one project-relative path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistoryFact {
    path: NormalizedPath,
    commit_count: u64,
    author_count: u64,
    insertions: u64,
    deletions: u64,
    last_touched_at: Option<u64>,
}

/// Validated churn counters for one file-history fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHistoryMetrics {
    /// Number of commit entries.
    pub commit_count: u64,
    /// Number of author entries.
    pub author_count: u64,
    /// Number of insertions.
    pub insertions: u64,
    /// Number of deletions.
    pub deletions: u64,
    /// Unix timestamp of the most recent change.
    pub last_touched_at: Option<u64>,
}

impl FileHistoryFact {
    /// Creates a validated file history fact.
    ///
    /// # Errors
    ///
    /// Returns an error if authors exceed commits or commit/timestamp presence
    /// is inconsistent.
    pub fn new(path: NormalizedPath, metrics: FileHistoryMetrics) -> Result<Self, StorageError> {
        if metrics.author_count > metrics.commit_count
            || (metrics.commit_count == 0) != metrics.last_touched_at.is_none()
        {
            return Err(StorageError::InvalidInput {
                field: "file_history",
            });
        }
        Ok(Self {
            path,
            commit_count: metrics.commit_count,
            author_count: metrics.author_count,
            insertions: metrics.insertions,
            deletions: metrics.deletions,
            last_touched_at: metrics.last_touched_at,
        })
    }
}

/// Aggregated same-commit relationship between two canonical paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileCochangeFact {
    path_a: NormalizedPath,
    path_b: NormalizedPath,
    commit_count: u64,
    confidence: f32,
}

/// Validated shared-commit metrics for one co-change pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileCochangeMetrics {
    /// Number of commit entries.
    pub commit_count: u64,
    /// Confidence for this record.
    pub confidence: f32,
}

impl FileCochangeFact {
    /// Creates a validated file cochange fact.
    ///
    /// # Errors
    ///
    /// Returns an error if paths are not strictly ordered/distinct, commits are
    /// zero, or confidence is non-finite or outside zero to one.
    pub fn new(
        path_a: NormalizedPath,
        path_b: NormalizedPath,
        metrics: FileCochangeMetrics,
    ) -> Result<Self, StorageError> {
        if path_a >= path_b
            || metrics.commit_count == 0
            || !metrics.confidence.is_finite()
            || !(0.0..=1.0).contains(&metrics.confidence)
        {
            return Err(StorageError::InvalidInput {
                field: "file_cochange",
            });
        }
        Ok(Self {
            path_a,
            path_b,
            commit_count: metrics.commit_count,
            confidence: metrics.confidence,
        })
    }
}

/// Atomic replacement of one repository-head history snapshot.
pub struct HistoryRefreshRequest {
    project_id: ProjectId,
    head_commit: String,
    shallow_history: bool,
    commits_scanned: u64,
    truncated: bool,
    oversized_commits_skipped: u64,
    files: Vec<FileHistoryFact>,
    cochanges: Vec<FileCochangeFact>,
    parameters: Option<HistoryRefreshParameters>,
}

/// Inputs that, with HEAD and shallowness, fully determine a churn and
/// co-change refresh. A stored refresh with identical inputs is reusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRefreshParameters {
    /// Maximum commits scanned from HEAD.
    pub max_commits: u64,
    /// Commits reachable from HEAD within `max_commits + 1`; deepening a
    /// shallow clone at the same HEAD changes it and forces a rescan.
    pub commits_available: u64,
    /// Version of the mining semantics; bumping it invalidates stored refreshes.
    pub algorithm_version: u32,
    /// Whether churn rows were retained.
    pub churn: bool,
    /// Whether co-change rows were retained.
    pub co_change: bool,
}

/// Git-scan provenance kept separate from the durable relation batches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryRefreshMetadata {
    /// Whether this value is shallow history.
    pub shallow_history: bool,
    /// Number of commits scanned.
    pub commits_scanned: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
    /// Number of oversized commits skipped.
    pub oversized_commits_skipped: u64,
}

/// Bounded history and co-change rows supplied by one Git scan.
pub struct HistoryRefreshInput {
    /// Metadata for this record.
    pub metadata: HistoryRefreshMetadata,
    /// Bounded files included in this result.
    pub files: Vec<FileHistoryFact>,
    /// Bounded cochanges included in this result.
    pub cochanges: Vec<FileCochangeFact>,
}

impl HistoryRefreshRequest {
    /// Creates a validated history refresh request.
    ///
    /// # Errors
    ///
    /// Returns an error if the head commit is invalid or file/co-change payloads
    /// exceed their bounded refresh maxima.
    pub fn new(
        project_id: ProjectId,
        head_commit: impl Into<String>,
        input: HistoryRefreshInput,
    ) -> Result<Self, StorageError> {
        let head_commit = head_commit.into();
        if !valid_commit(&head_commit)
            || input.files.len() > MAX_HISTORY_FILES
            || input.cochanges.len() > MAX_HISTORY_PAIRS
        {
            return Err(StorageError::InvalidInput {
                field: "history_refresh",
            });
        }
        Ok(Self {
            project_id,
            head_commit,
            shallow_history: input.metadata.shallow_history,
            commits_scanned: input.metadata.commits_scanned,
            truncated: input.metadata.truncated,
            oversized_commits_skipped: input.metadata.oversized_commits_skipped,
            files: input.files,
            cochanges: input.cochanges,
            parameters: None,
        })
    }

    /// Record the inputs that produced this refresh so an unchanged HEAD can
    /// reuse it. Without them, any stored reuse record is removed.
    #[must_use]
    pub const fn with_parameters(mut self, parameters: HistoryRefreshParameters) -> Self {
        self.parameters = Some(parameters);
        self
    }
}

/// Exact checkout state whose stored history refresh may be reused.
#[derive(Clone, Copy, Debug)]
pub struct HistoryReuseQuery<'query> {
    /// Stable root identity of the registered project checkout.
    pub root_identity: &'query str,
    /// Current HEAD commit of the checkout.
    pub head_commit: &'query str,
    /// Whether the checkout's history is shallow.
    pub shallow_history: bool,
    /// Inputs the stored refresh must have been produced with.
    pub parameters: HistoryRefreshParameters,
}

/// Bounded current-file history query.
pub struct FileHistoryQuery<'query> {
    project_id: &'query ProjectId,
    path: Option<&'query NormalizedPath>,
    minimum_commits: u32,
    limit: u16,
}

impl<'query> FileHistoryQuery<'query> {
    #[must_use]
    /// Creates a validated file history query.
    pub const fn new(project_id: &'query ProjectId, limit: u16) -> Self {
        Self {
            project_id,
            path: None,
            minimum_commits: 0,
            limit,
        }
    }

    #[must_use]
    /// Returns the for path.
    pub const fn for_path(mut self, path: &'query NormalizedPath) -> Self {
        self.path = Some(path);
        self
    }

    #[must_use]
    /// Sets the minimum commits and returns the updated value.
    pub const fn with_minimum_commits(mut self, minimum_commits: u32) -> Self {
        self.minimum_commits = minimum_commits;
        self
    }
}

/// Bounded current-file co-change query.
pub struct FileCochangeQuery<'query> {
    project_id: &'query ProjectId,
    path: &'query NormalizedPath,
    minimum_commits: u32,
    limit: u16,
}

impl<'query> FileCochangeQuery<'query> {
    #[must_use]
    /// Creates a validated file cochange query.
    pub const fn new(
        project_id: &'query ProjectId,
        path: &'query NormalizedPath,
        limit: u16,
    ) -> Self {
        Self {
            project_id,
            path,
            minimum_commits: 0,
            limit,
        }
    }

    #[must_use]
    /// Sets the minimum commits and returns the updated value.
    pub const fn with_minimum_commits(mut self, minimum_commits: u32) -> Self {
        self.minimum_commits = minimum_commits;
        self
    }
}

/// Durable refresh counts and exact Git provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRefreshReport {
    head_commit: String,
    shallow_history: bool,
    commits_scanned: u64,
    truncated: bool,
    oversized_commits_skipped: u64,
    files_written: u64,
    cochanges_written: u64,
    /// Whether the stored refresh for the same HEAD and inputs was reused.
    reused: bool,
}

impl HistoryRefreshReport {
    /// Whether stored evidence for the same HEAD and inputs was reused.
    #[must_use]
    pub const fn reused(&self) -> bool {
        self.reused
    }
}

/// One persisted path-level churn row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryRecord {
    path: String,
    head_commit: String,
    commit_count: u64,
    author_count: u64,
    insertions: u64,
    deletions: u64,
    last_touched_at: Option<String>,
    shallow_history: bool,
}

/// One co-changing partner with symmetric and anchor-relative confidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCochangeRecord {
    path: String,
    shared_commits: u64,
    jaccard: f32,
    anchor_ratio: f32,
    partner_ratio: f32,
    anchor_commits: u64,
    partner_commits: u64,
}

impl FileCochangeRecord {
    /// Current-generation partner path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Number of retained commits in which the pair changed together.
    #[must_use]
    pub const fn shared_commits(&self) -> u64 {
        self.shared_commits
    }

    /// Pairwise Jaccard similarity across retained commits.
    #[must_use]
    pub const fn jaccard(&self) -> f32 {
        self.jaccard
    }

    /// Fraction of the anchor file's retained commits shared with this partner.
    #[must_use]
    pub const fn anchor_ratio(&self) -> f32 {
        self.anchor_ratio
    }
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_query_limit(limit: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > MAX_HISTORY_QUERY_LIMIT {
        Err(StorageError::InvalidInput { field: "limit" })
    } else {
        Ok(())
    }
}

use crate::StorageError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cochanges_require_canonical_order_nonzero_count_and_probability() {
        let metrics = |commit_count, confidence| FileCochangeMetrics {
            commit_count,
            confidence,
        };
        let a = NormalizedPath::parse("src/a.rs")
            .unwrap_or_else(|error| panic!("path a failed: {error}"));
        let b = NormalizedPath::parse("src/b.rs")
            .unwrap_or_else(|error| panic!("path b failed: {error}"));
        assert!(FileCochangeFact::new(a.clone(), b.clone(), metrics(2, 0.5)).is_ok());
        assert!(FileCochangeFact::new(b, a.clone(), metrics(2, 0.5)).is_err());
        assert!(FileCochangeFact::new(a.clone(), a.clone(), metrics(2, 0.5)).is_err());
        assert!(FileCochangeFact::new(a.clone(), a, metrics(0, 1.5)).is_err());

        let punctuation = NormalizedPath::parse(".github/workflows/check.ts")
            .unwrap_or_else(|error| panic!("punctuation path failed: {error}"));
        let uppercase = NormalizedPath::parse("ACKNOWLEDGEMENTS.ts")
            .unwrap_or_else(|error| panic!("uppercase path failed: {error}"));
        assert!(
            FileCochangeFact::new(punctuation.clone(), uppercase.clone(), metrics(1, 1.0)).is_ok()
        );
        assert!(FileCochangeFact::new(uppercase, punctuation, metrics(1, 1.0)).is_err());
    }
}
